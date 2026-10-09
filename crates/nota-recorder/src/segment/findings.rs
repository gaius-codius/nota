//! What publishing found wrong with committed rows, kept in the session's
//! directory so the app can show it.
//!
//! A committed row claims its samples only if its file is in the session's
//! directory, can be read, and proves it (see `verify`). A row that doesn't
//! is a [`Finding`]: while it fails that check, nothing is published over
//! its samples, its file is left alone, and the journals holding those
//! samples are kept, unless publishing can repair it (the parent module's
//! docs). A row that doesn't parse at all stops its session's publishing,
//! and is a finding too, by where it is ([`UnparsableRow`]).
//!
//! A finding is never dropped. Its [`Status`] says what became of it:
//! [`Status::Unresolved`] while it stands, [`Status::SinceVerified`] once a
//! later run finds the row proven by its file (or parsing) with nothing
//! changed by nota, [`Status::Repaired`] once publishing rebuilt the row's
//! file from its journals. A row found failing again is unresolved again.
//!
//! The findings live in one file, `salvage-findings`, written like a
//! segment: temp file, fsync, rename, directory fsync. So a crash leaves the
//! old findings or the new ones, never a torn file. Each publish run merges
//! what it found into what's there, and the file is rewritten only when
//! that changes it. The library database keeps an index of it
//! ([`nota_store::Store::index_findings`]); the file is the record.
//!
//! # Format, version 2
//!
//! All integers little-endian.
//!
//! | Bytes | Field |
//! |---|---|
//! | 8 | magic `NOTAFIND` |
//! | 2 | version, 2 |
//! | 1 | the last run's verification: 0 done, 1 unavailable |
//! | 4 | the number of row findings, `n` |
//! | `n` × 91 | each: track (4), epoch (4), first sample (8), end sample (8), the row's SHA-256 (32), whether it has an audio digest (1: 0 or 1), the digest (32, zeros if not), problem (1, below), status (1, below) |
//! | 4 | the number of unparsable rows, `m` |
//! | `m` × 17 | each: track (8, signed), first sample (8, signed), status (1) |
//! | 4 | CRC-32 of everything before it |
//!
//! The problem codes: 0 missing, 1 hash mismatch, 2 length mismatch, and
//! for a file that couldn't be read, 3 permission denied, 4 a directory, 5
//! any other I/O error. The status codes: 0 unresolved, 1 since verified, 2
//! repaired.
//!
//! Version 1 is read too: no audio digest (58-byte entries, without its
//! two fields), every status 0, and no unparsable rows. It's rewritten as
//! version 2 the next time it changes.
//!
//! The file comes from disk, so it's parsed into typed values and anything
//! else is refused: a wrong length, CRC, magic, version or count, an empty
//! range, or an unknown flag, problem or status.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use nota_core::{EpochId, SampleIndex, SampleRange, TrackId};
use nota_store::{
    AudioDigest, IndexedFinding, Problem, ReadFailure, RowKey, SegmentRow, Sha256Digest, Status,
};

use crate::fs::{Fs, FsFile};
use crate::session::SessionDir;

/// The findings file's name in the session directory.
pub const FILE_NAME: &str = "salvage-findings";
/// The name it's written under before its rename.
const TEMP_NAME: &str = "salvage-findings.tmp";
/// Where a findings file that doesn't parse is set aside, keeping its bytes
/// (with `.1`, `.2`, … after it if that's taken).
const ASIDE_NAME: &str = "salvage-findings.unreadable";

const MAGIC: &[u8; 8] = b"NOTAFIND";
const VERSION: u16 = 2;
/// Magic, version, verification and count.
const HEADER_LEN: usize = 8 + 2 + 1 + 4;
const V1_ENTRY_LEN: usize = 4 + 4 + 8 + 8 + 32 + 1 + 1;
const ENTRY_LEN: usize = V1_ENTRY_LEN + 1 + 32;
const UNPARSABLE_LEN: usize = 8 + 8 + 1;
const COUNT_LEN: usize = 4;
const CRC_LEN: usize = 4;

/// The failure an error of `kind` is.
pub(super) fn read_failure(kind: io::ErrorKind) -> ReadFailure {
    match kind {
        io::ErrorKind::PermissionDenied => ReadFailure::PermissionDenied,
        io::ErrorKind::IsADirectory => ReadFailure::IsADirectory,
        _ => ReadFailure::Other,
    }
}

const fn problem_code(problem: Problem) -> u8 {
    match problem {
        Problem::Missing => 0,
        Problem::HashMismatch => 1,
        Problem::LengthMismatch => 2,
        Problem::Unreadable(ReadFailure::PermissionDenied) => 3,
        Problem::Unreadable(ReadFailure::IsADirectory) => 4,
        Problem::Unreadable(ReadFailure::Other) => 5,
    }
}

const fn problem_from_code(code: u8) -> Option<Problem> {
    match code {
        0 => Some(Problem::Missing),
        1 => Some(Problem::HashMismatch),
        2 => Some(Problem::LengthMismatch),
        3 => Some(Problem::Unreadable(ReadFailure::PermissionDenied)),
        4 => Some(Problem::Unreadable(ReadFailure::IsADirectory)),
        5 => Some(Problem::Unreadable(ReadFailure::Other)),
        _ => None,
    }
}

const fn status_code(status: Status) -> u8 {
    match status {
        Status::Unresolved => 0,
        Status::SinceVerified => 1,
        Status::Repaired => 2,
    }
}

const fn status_from_code(code: u8) -> Option<Status> {
    match code {
        0 => Some(Status::Unresolved),
        1 => Some(Status::SinceVerified),
        2 => Some(Status::Repaired),
        _ => None,
    }
}

/// A committed row that claimed nothing when a run checked it, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finding {
    row: SegmentRow,
    problem: Problem,
    status: Status,
}

impl Finding {
    /// An unresolved finding: `row`'s file has `problem`.
    pub(super) const fn new(row: SegmentRow, problem: Problem) -> Self {
        Self {
            row,
            problem,
            status: Status::Unresolved,
        }
    }

    /// The row, as committed: track, epoch, range, expected SHA-256 and
    /// audio digest.
    #[must_use]
    pub const fn row(&self) -> &SegmentRow {
        &self.row
    }

    /// What was found under the row's name.
    #[must_use]
    pub const fn problem(&self) -> Problem {
        self.problem
    }

    /// Whether it's been dealt with.
    #[must_use]
    pub const fn status(&self) -> Status {
        self.status
    }

    /// What identifies it: its row and problem. The order findings are kept
    /// in: by track, then sample range.
    fn key(&self) -> FindingKey {
        let r = &self.row;
        (
            r.track(),
            r.range().start(),
            r.range().end(),
            r.epoch(),
            *r.sha256().as_bytes(),
            r.audio().map(|a| *a.as_bytes()),
            self.problem,
        )
    }
}

type FindingKey = (
    TrackId,
    SampleIndex,
    SampleIndex,
    EpochId,
    [u8; 32],
    Option<[u8; 32]>,
    Problem,
);

/// A committed row that doesn't parse, which stopped its session's
/// publishing, by where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnparsableRow {
    key: RowKey,
    status: Status,
}

impl UnparsableRow {
    /// Its track and first sample, as stored.
    #[must_use]
    pub const fn key(&self) -> RowKey {
        self.key
    }

    /// Whether it's been dealt with: [`Status::SinceVerified`] once the
    /// session's rows parse again.
    #[must_use]
    pub const fn status(&self) -> Status {
        self.status
    }
}

/// Whether the last publish run could check the rows against their files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verification {
    /// It read the store and checked every row it needed to.
    #[default]
    Done,
    /// It couldn't read the store, so nothing was checked or published, and
    /// no journal was deleted. Earlier findings still stand.
    Unavailable,
}

/// What one run learned, to merge into the findings file.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Run<'a> {
    /// Rows whose file didn't prove them.
    pub(super) found: &'a [Finding],
    /// Rows whose file proved them, with nothing changed.
    pub(super) verified: &'a [SegmentRow],
    /// Rows whose file publishing rebuilt from their journals.
    pub(super) repaired: &'a [SegmentRow],
    /// A row that doesn't parse, if reading the rows failed on one.
    pub(super) unparsable: Option<RowKey>,
    /// Whether it read the store.
    pub(super) verification: Verification,
}

/// Every finding recorded for a session, and how the last run went.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Findings {
    /// Sorted by [`Finding::key`], one per key.
    found: Vec<Finding>,
    /// Sorted by key, one per key.
    unparsable: Vec<UnparsableRow>,
    verification: Verification,
}

impl Findings {
    /// Every row finding, by track then sample range.
    #[must_use]
    pub fn found(&self) -> &[Finding] {
        &self.found
    }

    /// Every row found not to parse, by track then first sample.
    #[must_use]
    pub fn unparsable(&self) -> &[UnparsableRow] {
        &self.unparsable
    }

    /// How many findings, of either kind, are [`Status::Unresolved`].
    #[must_use]
    pub fn unresolved(&self) -> usize {
        let open = |s: Status| usize::from(s == Status::Unresolved);
        self.found.iter().map(|f| open(f.status)).sum::<usize>()
            + self
                .unparsable
                .iter()
                .map(|u| open(u.status))
                .sum::<usize>()
    }

    /// Whether the last run could check the rows.
    #[must_use]
    pub const fn verification(&self) -> Verification {
        self.verification
    }

    /// Every finding, as the library database indexes them.
    #[must_use]
    pub fn indexed(&self) -> Vec<IndexedFinding> {
        let rows = self.found.iter().map(|f| IndexedFinding::Row {
            row: f.row,
            problem: f.problem,
            status: f.status,
        });
        let unparsable = self.unparsable.iter().map(|u| IndexedFinding::Unparsable {
            key: u.key,
            status: u.status,
        });
        rows.chain(unparsable).collect()
    }

    /// These findings with `run` merged in: its findings added, or made
    /// unresolved again if they're here; findings of rows it verified or
    /// repaired (and didn't find failing) marked so, if unresolved; its unparsable row added or made
    /// unresolved again, and, if it read the rows, every other unparsable
    /// row marked since verified; and its verification as the last run's.
    fn merged(&self, run: &Run<'_>) -> Self {
        let mut found = self.found.clone();
        for new in run.found {
            match found.iter_mut().find(|f| f.key() == new.key()) {
                Some(f) => f.status = Status::Unresolved,
                None => found.push(*new),
            }
        }
        for (rows, status) in [
            (run.verified, Status::SinceVerified),
            (run.repaired, Status::Repaired),
        ] {
            for f in &mut found {
                // A row found failing in this run wasn't verified in it.
                let failing = run.found.iter().any(|n| n.row == f.row);
                if f.status == Status::Unresolved && rows.contains(&f.row) && !failing {
                    f.status = status;
                }
            }
        }
        let mut unparsable = self.unparsable.clone();
        if run.verification == Verification::Done {
            for u in &mut unparsable {
                if u.status == Status::Unresolved {
                    u.status = Status::SinceVerified;
                }
            }
        }
        if let Some(key) = run.unparsable {
            match unparsable.iter_mut().find(|u| u.key == key) {
                Some(u) => u.status = Status::Unresolved,
                None => unparsable.push(UnparsableRow {
                    key,
                    status: Status::Unresolved,
                }),
            }
        }
        Self::from_parts(found, unparsable, run.verification)
    }

    fn from_parts(
        mut found: Vec<Finding>,
        mut unparsable: Vec<UnparsableRow>,
        verification: Verification,
    ) -> Self {
        found.sort_by_key(Finding::key);
        found.dedup_by_key(|f| f.key());
        unparsable.sort_by_key(|u| u.key);
        unparsable.dedup_by_key(|u| u.key);
        Self {
            found,
            unparsable,
            verification,
        }
    }

    fn encode(&self) -> Vec<u8> {
        // A count past u32 can't happen (each finding is a row of the
        // session), but if it did, write only what the count says.
        let count = u32::try_from(self.found.len()).unwrap_or(u32::MAX);
        let unparsable = u32::try_from(self.unparsable.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(
            HEADER_LEN
                + self.found.len() * ENTRY_LEN
                + COUNT_LEN
                + self.unparsable.len() * UNPARSABLE_LEN
                + CRC_LEN,
        );
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.push(match self.verification {
            Verification::Done => 0,
            Verification::Unavailable => 1,
        });
        out.extend_from_slice(&count.to_le_bytes());
        for f in self.found.iter().take(count as usize) {
            out.extend_from_slice(&f.row.track().get().to_le_bytes());
            out.extend_from_slice(&f.row.epoch().get().to_le_bytes());
            out.extend_from_slice(&f.row.range().start().get().to_le_bytes());
            out.extend_from_slice(&f.row.range().end().get().to_le_bytes());
            out.extend_from_slice(f.row.sha256().as_bytes());
            if let Some(audio) = f.row.audio() {
                out.push(1);
                out.extend_from_slice(audio.as_bytes());
            } else {
                out.push(0);
                out.extend_from_slice(&[0; 32]);
            }
            out.push(problem_code(f.problem));
            out.push(status_code(f.status));
        }
        out.extend_from_slice(&unparsable.to_le_bytes());
        for u in self.unparsable.iter().take(unparsable as usize) {
            out.extend_from_slice(&u.key.track.to_le_bytes());
            out.extend_from_slice(&u.key.start.to_le_bytes());
            out.push(status_code(u.status));
        }
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Parses a findings file; `None` for anything that isn't a whole,
    /// valid version 1 or 2 file.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let body_len = bytes.len().checked_sub(CRC_LEN)?;
        let (body, crc) = bytes.split_at(body_len);
        if crc32fast::hash(body).to_le_bytes() != crc {
            return None;
        }
        let mut r = Reader(body);
        if r.take::<8>()? != *MAGIC {
            return None;
        }
        let version = u16::from_le_bytes(r.take()?);
        if version != 1 && version != VERSION {
            return None;
        }
        let verification = match r.take::<1>()? {
            [0] => Verification::Done,
            [1] => Verification::Unavailable,
            _ => return None,
        };
        let count = usize::try_from(u32::from_le_bytes(r.take()?)).ok()?;
        let entry_len = if version == 1 {
            V1_ENTRY_LEN
        } else {
            ENTRY_LEN
        };
        if r.0.len() < count.checked_mul(entry_len)? {
            return None;
        }
        let mut found = Vec::with_capacity(count);
        for _ in 0..count {
            let track = TrackId::new(u32::from_le_bytes(r.take()?));
            let epoch = EpochId::new(u32::from_le_bytes(r.take()?));
            let start = SampleIndex::new(u64::from_le_bytes(r.take()?));
            let end = SampleIndex::new(u64::from_le_bytes(r.take()?));
            let sha256 = Sha256Digest::new(r.take()?);
            let audio = if version == 1 {
                None
            } else {
                let [flag] = r.take()?;
                let audio: [u8; 32] = r.take()?;
                match flag {
                    0 if audio == [0; 32] => None,
                    1 => Some(AudioDigest::new(audio)),
                    _ => return None,
                }
            };
            let [problem] = r.take()?;
            let [status] = r.take()?;
            let mut row = SegmentRow::new(track, epoch, SampleRange::new(start, end)?, sha256)?;
            if let Some(audio) = audio {
                row = row.with_audio(audio);
            }
            let status = status_from_code(status)?;
            if version == 1 && status != Status::Unresolved {
                return None;
            }
            found.push(Finding {
                row,
                problem: problem_from_code(problem)?,
                status,
            });
        }
        let mut unparsable = Vec::new();
        if version != 1 {
            let m = usize::try_from(u32::from_le_bytes(r.take()?)).ok()?;
            if r.0.len() != m.checked_mul(UNPARSABLE_LEN)? {
                return None;
            }
            for _ in 0..m {
                let track = i64::from_le_bytes(r.take()?);
                let start = i64::from_le_bytes(r.take()?);
                let [status] = r.take()?;
                unparsable.push(UnparsableRow {
                    key: RowKey { track, start },
                    status: status_from_code(status)?,
                });
            }
        }
        if !r.0.is_empty() {
            return None;
        }
        // One entry per key, as the writer keeps them.
        let parsed = Self::from_parts(found.clone(), unparsable.clone(), verification);
        (parsed.found.len() == found.len() && parsed.unparsable.len() == unparsable.len())
            .then_some(parsed)
    }
}

/// Reads fixed-size fields off the front of a byte slice.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
}

/// Why the findings couldn't be read.
#[derive(Debug)]
pub enum FindingsError {
    /// Reading the file failed.
    Io(io::Error),
    /// The file isn't a valid findings file (see the module docs). The next
    /// publish run sets it aside, as `salvage-findings.unreadable` (or
    /// `.unreadable.1`, …), and starts a new one.
    Corrupt,
}

impl fmt::Display for FindingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "reading the salvage findings failed: {e}"),
            Self::Corrupt => f.write_str("the salvage findings file is corrupt"),
        }
    }
}

impl Error for FindingsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Corrupt => None,
        }
    }
}

/// The findings recorded for `session`: none if there's no findings file.
/// Only reads.
///
/// # Errors
///
/// [`FindingsError::Io`] if the file can't be read, and
/// [`FindingsError::Corrupt`] if it doesn't parse.
pub fn read_findings<S: Fs>(session: &SessionDir<S>) -> Result<Findings, FindingsError> {
    read(session.fs(), session.dir())
}

/// The findings in `dir`, as [`read_findings`] reads them.
pub(super) fn read<S: Fs>(fs: &S, dir: &Path) -> Result<Findings, FindingsError> {
    match fs.read(&dir.join(FILE_NAME)) {
        Ok(bytes) => Findings::decode(&bytes).ok_or(FindingsError::Corrupt),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Findings::default()),
        Err(e) => Err(FindingsError::Io(e)),
    }
}

/// Whether `path` is a findings temp file: a write a crash interrupted,
/// never the only copy of anything (the findings it held were merged with
/// the file it would have replaced), so salvage removes it.
pub(super) fn is_temp(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == TEMP_NAME)
}

/// Merges `run` into the findings file in `dir`, durably, and returns them
/// all (see [`Findings::merged`]). Rewrites the file only if
/// that changes it (and otherwise only syncs the directory, in case an
/// earlier run's rename isn't durable yet). A file that doesn't parse is first renamed aside,
/// keeping its bytes, and the findings start again from `run`.
///
/// # Errors
///
/// Any I/O error. The file is then the old one or the new one, never torn.
pub(super) fn record<S: Fs>(fs: &S, dir: &Path, run: &Run<'_>) -> io::Result<Findings> {
    let path = dir.join(FILE_NAME);
    let old = match fs.read(&path) {
        Ok(bytes) => {
            let old = Findings::decode(&bytes);
            if old.is_none() {
                fs.rename(&path, &aside_path(fs, dir)?)?;
                fs.sync_dir(dir)?;
            }
            old
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let merged = old.clone().unwrap_or_default().merged(run);
    match &old {
        // Already says it. The file may be one an earlier run renamed into
        // place before its directory sync failed: sync now, so success
        // here means durable.
        Some(old) if *old == merged => fs.sync_dir(dir)?,
        // No file and nothing to say: leave it that way.
        None if merged == Findings::default() => {}
        _ => write(fs, dir, &path, &merged.encode())?,
    }
    Ok(merged)
}

/// Where to set a findings file that doesn't parse aside: the first of
/// `salvage-findings.unreadable`, `salvage-findings.unreadable.1`, … not
/// taken, so an earlier one is never replaced.
fn aside_path<S: Fs>(fs: &S, dir: &Path) -> io::Result<PathBuf> {
    let taken = fs.list(dir)?;
    let mut name = ASIDE_NAME.to_owned();
    let mut n = 0_u64;
    while taken.contains(&dir.join(&name)) {
        n += 1;
        name = format!("{ASIDE_NAME}.{n}");
    }
    Ok(dir.join(name))
}

/// Writes `bytes` to `path` in `dir` atomically: temp file, fsync, rename,
/// directory fsync.
fn write<S: Fs>(fs: &S, dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = dir.join(TEMP_NAME);
    match fs.remove(&temp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // A temp file a crash left (removed just now, or by salvage) must be
    // durably gone before its name is reused: otherwise a crash could keep
    // the rename below but not the unlink, and move the old, torn temp
    // over the findings.
    fs.sync_dir(dir)?;
    let mut file = fs.create(&temp)?;
    file.write_all(bytes)?;
    file.sync()?;
    drop(file);
    fs.rename(&temp, path)?;
    fs.sync_dir(dir)
}

#[cfg(test)]
mod tests;
