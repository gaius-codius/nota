//! What publishing found wrong with committed rows, kept in the session's
//! directory so the app can show it.
//!
//! A committed row claims its samples only if its file is in the session's
//! directory, can be read, and matches it. A row that doesn't is a [`Finding`]: nothing is
//! published over its samples, its file is left alone, and the journals
//! holding those samples are kept, until it's resolved (not in this
//! version: every finding is [`Status::Unresolved`]).
//!
//! The findings live in one file, `salvage-findings`, written like a
//! segment: temp file, fsync, rename, directory fsync. So a crash leaves the
//! old findings or the new ones, never a torn file. Each publish run merges
//! what it found into what's there: a finding is never dropped, and one
//! found again isn't added twice. The file is rewritten only when that
//! changes it.
//!
//! # Format, version 1
//!
//! All integers little-endian.
//!
//! | Bytes | Field |
//! |---|---|
//! | 8 | magic `NOTAFIND` |
//! | 2 | version, 1 |
//! | 1 | the last run's verification: 0 done, 1 unavailable |
//! | 4 | the number of findings, `n` |
//! | `n` × 58 | each finding: track (4), epoch (4), first sample (8), end sample (8), the row's SHA-256 (32), problem (1, below), status (1: 0 unresolved) |
//! | 4 | CRC-32 of everything before it |
//!
//! The problem codes: 0 missing, 1 hash mismatch, 2 length mismatch, and
//! for a file that couldn't be read, 3 permission denied, 4 a directory, 5
//! any other I/O error.
//!
//! The file comes from disk, so it's parsed into typed values and anything
//! else is refused: a wrong length, CRC, magic, version or count, an empty
//! range, or an unknown problem or status.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use nota_core::{EpochId, SampleIndex, SampleRange, TrackId};
use nota_store::{SegmentRow, Sha256Digest};

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
const VERSION: u16 = 1;
/// Magic, version, verification and count.
const HEADER_LEN: usize = 8 + 2 + 1 + 4;
const ENTRY_LEN: usize = 4 + 4 + 8 + 8 + 32 + 1 + 1;
const CRC_LEN: usize = 4;

/// What's wrong with a committed row's file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Problem {
    /// There's no file under the row's name in the session directory.
    Missing,
    /// The file's SHA-256 isn't the row's.
    HashMismatch,
    /// The file's SHA-256 is the row's, but its FLAC header doesn't declare
    /// the row's number of samples.
    LengthMismatch,
    /// There's something under the row's name, but reading it failed. It may
    /// be transient (a later run that reads it and finds it matching lets
    /// the row claim its samples again), but it's recorded either way.
    Unreadable(ReadFailure),
}

/// Why a row's file couldn't be read: the error's kind, as far as the
/// findings file keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReadFailure {
    /// `EACCES` or `EPERM`.
    PermissionDenied,
    /// A directory is under the row's name.
    IsADirectory,
    /// Any other error (`EIO`, for one).
    Other,
}

impl ReadFailure {
    /// The failure an error of `kind` is.
    #[must_use]
    pub fn of(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::IsADirectory => Self::IsADirectory,
            _ => Self::Other,
        }
    }
}

impl Problem {
    const fn code(self) -> u8 {
        match self {
            Self::Missing => 0,
            Self::HashMismatch => 1,
            Self::LengthMismatch => 2,
            Self::Unreadable(ReadFailure::PermissionDenied) => 3,
            Self::Unreadable(ReadFailure::IsADirectory) => 4,
            Self::Unreadable(ReadFailure::Other) => 5,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Missing),
            1 => Some(Self::HashMismatch),
            2 => Some(Self::LengthMismatch),
            3 => Some(Self::Unreadable(ReadFailure::PermissionDenied)),
            4 => Some(Self::Unreadable(ReadFailure::IsADirectory)),
            5 => Some(Self::Unreadable(ReadFailure::Other)),
            _ => None,
        }
    }
}

/// Whether a finding has been dealt with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    /// Not yet: the row claims nothing, and its samples stay in journals.
    Unresolved,
}

/// A committed row that claims nothing, and why.
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

    /// The row, as committed: track, epoch, range and expected SHA-256.
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

    /// The order findings are kept in: by track, then sample range.
    fn key(
        &self,
    ) -> (
        TrackId,
        SampleIndex,
        SampleIndex,
        EpochId,
        [u8; 32],
        Problem,
    ) {
        let r = &self.row;
        (
            r.track(),
            r.range().start(),
            r.range().end(),
            r.epoch(),
            *r.sha256().as_bytes(),
            self.problem,
        )
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

/// Every finding recorded for a session, and how the last run went.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Findings {
    /// Sorted by [`Finding::key`], without duplicates.
    found: Vec<Finding>,
    verification: Verification,
}

impl Findings {
    /// Every finding, by track then sample range.
    #[must_use]
    pub fn found(&self) -> &[Finding] {
        &self.found
    }

    /// Whether the last run could check the rows.
    #[must_use]
    pub const fn verification(&self) -> Verification {
        self.verification
    }

    /// These findings with `new` added (those not already here) and
    /// `verification` as the last run's.
    fn merged(&self, new: &[Finding], verification: Verification) -> Self {
        let mut found = self.found.clone();
        found.extend_from_slice(new);
        Self::from_parts(found, verification)
    }

    fn from_parts(mut found: Vec<Finding>, verification: Verification) -> Self {
        found.sort_by_key(Finding::key);
        found.dedup();
        Self {
            found,
            verification,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let count = u32::try_from(self.found.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(HEADER_LEN + self.found.len() * ENTRY_LEN + CRC_LEN);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.push(match self.verification {
            Verification::Done => 0,
            Verification::Unavailable => 1,
        });
        out.extend_from_slice(&count.to_le_bytes());
        // A count past u32 can't happen (each finding is a row overlapping
        // a journal), but if it did, write only what the count says.
        for f in self.found.iter().take(count as usize) {
            out.extend_from_slice(&f.row.track().get().to_le_bytes());
            out.extend_from_slice(&f.row.epoch().get().to_le_bytes());
            out.extend_from_slice(&f.row.range().start().get().to_le_bytes());
            out.extend_from_slice(&f.row.range().end().get().to_le_bytes());
            out.extend_from_slice(f.row.sha256().as_bytes());
            out.push(f.problem.code());
            out.push(match f.status {
                Status::Unresolved => 0,
            });
        }
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Parses a findings file; `None` for anything that isn't a whole,
    /// valid version 1 file.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let body_len = bytes.len().checked_sub(CRC_LEN)?;
        let (body, crc) = bytes.split_at(body_len);
        if crc32fast::hash(body).to_le_bytes() != crc {
            return None;
        }
        let mut r = Reader(body);
        if r.take::<8>()? != *MAGIC || u16::from_le_bytes(r.take()?) != VERSION {
            return None;
        }
        let verification = match r.take::<1>()? {
            [0] => Verification::Done,
            [1] => Verification::Unavailable,
            _ => return None,
        };
        let count = usize::try_from(u32::from_le_bytes(r.take()?)).ok()?;
        if r.0.len() != count.checked_mul(ENTRY_LEN)? {
            return None;
        }
        let mut found = Vec::with_capacity(count);
        for _ in 0..count {
            let track = TrackId::new(u32::from_le_bytes(r.take()?));
            let epoch = EpochId::new(u32::from_le_bytes(r.take()?));
            let start = SampleIndex::new(u64::from_le_bytes(r.take()?));
            let end = SampleIndex::new(u64::from_le_bytes(r.take()?));
            let sha256 = Sha256Digest::new(r.take()?);
            let [problem] = r.take()?;
            let [status] = r.take()?;
            let row = SegmentRow::new(track, epoch, SampleRange::new(start, end)?, sha256)?;
            let problem = Problem::from_code(problem)?;
            let status = match status {
                0 => Status::Unresolved,
                _ => return None,
            };
            found.push(Finding {
                row,
                problem,
                status,
            });
        }
        Some(Self::from_parts(found, verification))
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
    let path = session.dir().join(FILE_NAME);
    match session.fs().read(&path) {
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

/// Merges `new` findings and the run's `verification` into the findings
/// file in `dir`, durably, and returns them all. Rewrites the file only if
/// that changes it (and otherwise only syncs the directory, in case an
/// earlier run's rename isn't durable yet). A file that doesn't parse is first renamed aside,
/// keeping its bytes, and the findings start again from `new`.
///
/// # Errors
///
/// Any I/O error. The file is then the old one or the new one, never torn.
pub(super) fn record<S: Fs>(
    fs: &S,
    dir: &Path,
    new: &[Finding],
    verification: Verification,
) -> io::Result<Findings> {
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
    let merged = old.clone().unwrap_or_default().merged(new, verification);
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
