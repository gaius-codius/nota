//! Publishing journals as segments, live or at startup (salvage).
//!
//! Both read the journals back from disk, so recording and recovery share
//! one path. Memory stays bounded: the first pass keeps only each journal's
//! header and range, and the second reads just the journals of one segment
//! at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use nota_core::{SampleCount, SampleIndex, SampleRange, SampleRate};
use nota_store::SegmentRow;
use sha2::{Digest, Sha256};

use super::findings::{self, Finding, Problem, ReadFailure, Verification};
use super::flac::{self, FlacError};
use super::plan::{self, JournalSummary, PlannedSegment};
use super::publish::{Committed, DeletableJournal, TempSegment, delete_journals};
use super::store::SegmentStore;
use super::{SegmentLength, is_temp_segment, segment_file_name};
use crate::fs::Fs;
use crate::journal::format::{FRAME_HEADER_LEN, HEADER_LEN, MAX_FRAME_SAMPLES, frames_after};
use crate::journal::{JournalHeader, JournalId, JournalRead, read_journal, sync_budget};
use crate::session::{FinishedJournal, SessionDir, SessionStore};

/// What a publish run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Published {
    segments: Vec<SegmentRow>,
    deleted: Vec<JournalId>,
    quarantined: Vec<PathBuf>,
    unread: Vec<(FinishedJournal, io::ErrorKind)>,
    findings: Vec<Finding>,
    findings_unsaved: Option<io::ErrorKind>,
}

impl Published {
    /// The rows committed, in order.
    #[must_use]
    pub fn segments(&self) -> &[SegmentRow] {
        &self.segments
    }

    /// The journals deleted, their audio all in committed rows.
    #[must_use]
    pub fn deleted(&self) -> &[JournalId] {
        &self.deleted
    }

    /// Journal files set aside: renamed with `.unreadable` appended, so
    /// they're kept but not salvaged again. Those whose header couldn't be
    /// read or named another id than the file's name, and those with more
    /// after their valid frames than a crash can leave: too many unreadable
    /// bytes, or a frame of fsync'd audio past the damage (corruption; what
    /// did read was published). One whose window has a bad row stays under
    /// its name until that window publishes.
    #[must_use]
    pub fn quarantined(&self) -> &[PathBuf] {
        &self.quarantined
    }

    /// The journals that are there but couldn't be read (`EIO`, `EACCES`, a
    /// directory under a journal's name), with the error's kind. Each is
    /// left as it is, not renamed or deleted, and nothing in it is
    /// published or planned around: the failure may pass, and what it holds
    /// isn't known. Every other segment is published; where this journal
    /// holds samples of a window, the window's other samples may be
    /// published without them, and a later run that reads it publishes the
    /// rest of the window as further segments. A committed row that only
    /// they overlap isn't checked, so it isn't a finding this run. Pass them
    /// again to retry; salvage finds them again by name. Not to be confused
    /// with [`Self::quarantined`]: journals that did read, but whose contents
    /// couldn't be.
    #[must_use]
    pub fn unread(&self) -> &[(FinishedJournal, io::ErrorKind)] {
        &self.unread
    }

    /// The committed rows this run checked that claim nothing: their file
    /// is missing from the session directory, can't be read, or doesn't
    /// match them (another SHA-256, or another length). No segment was
    /// published over their samples, so the file, if any, is as it was and
    /// the journals holding those samples are kept. They're also recorded in
    /// the session's findings file (see
    /// [`read_findings`](super::read_findings)), with any found before.
    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// Why the findings file couldn't be updated, if it couldn't. Publishing
    /// carried on: the rows are checked again on every run, so the next one
    /// records them.
    #[must_use]
    pub const fn findings_unsaved(&self) -> Option<io::ErrorKind> {
        self.findings_unsaved
    }
}

/// Why publishing stopped. Whatever was done before it is consistent, and
/// running it again carries on.
#[derive(Debug)]
pub enum PublishError {
    /// A filesystem operation failed.
    Io(io::Error),
    /// The store failed.
    Store(Box<dyn Error + Send + Sync>),
    /// A segment couldn't be encoded.
    Flac(FlacError),
    /// A journal read back differently between the planning pass and the
    /// encoding pass: something else is writing to it.
    Changed(JournalId),
    /// A journal finished in another session than the one being published.
    /// Nothing was done.
    OtherSession(FinishedJournal),
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "publishing a segment failed: {e}"),
            Self::Store(e) => write!(f, "committing a segment row failed: {e}"),
            Self::Flac(e) => write!(f, "encoding a segment failed: {e}"),
            Self::Changed(id) => write!(
                f,
                "journal {} changed while it was being published",
                id.file_name()
            ),
            Self::OtherSession(j) => write!(
                f,
                "journal {} is session {}'s, not this one's",
                j.id().file_name(),
                j.session().get()
            ),
        }
    }
}

impl Error for PublishError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Store(e) => Some(e.as_ref()),
            Self::Flac(e) => Some(e),
            Self::Changed(_) | Self::OtherSession(_) => None,
        }
    }
}

impl From<io::Error> for PublishError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

fn store_error<E: Error + Send + Sync + 'static>(e: E) -> PublishError {
    PublishError::Store(Box::new(e))
}

/// Recovers a session after a crash: publishes every journal left in its
/// directory as segments, adds the rows missing from its store, and deletes
/// the journals once their rows are committed. Leftover segment and findings
/// temp files are removed first; they're never the only copy of anything.
///
/// Safe to run again after it fails or the machine crashes partway: every
/// step is repeatable, and a second run on a recovered session changes
/// nothing. `length` should be what the session recorded with; another
/// value still loses nothing, but splits segments differently.
///
/// # Errors
///
/// As [`publish_journals`].
pub fn salvage<S: Fs, T: SegmentStore>(
    session: &mut SessionStore<S, T>,
    length: SegmentLength,
) -> Result<Published, PublishError> {
    let (fs, dir) = (session.session().fs(), session.session().dir());
    let mut journals = Vec::new();
    for path in fs.list(dir)? {
        if findings::is_temp(&path) {
            // Best effort: the next findings write removes it anyway, and
            // the findings must never hold up publishing.
            let _gone = fs.remove(&path);
        } else if is_temp_segment(&path) {
            match fs.remove(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        } else if let Some(id) = path.file_name().and_then(JournalId::from_file_name) {
            // At startup nothing is writing: every journal is finished.
            journals.push(FinishedJournal::new(session.session().id(), id));
        }
    }
    publish_journals(session, length, &journals)
}

/// Whether the session's directory holds journals: a session that was still
/// recording when nota stopped, and needs [`salvage`].
///
/// # Errors
///
/// Any I/O error listing the directory.
pub fn needs_salvage<S: Fs>(session: &SessionDir<S>) -> io::Result<bool> {
    Ok(session
        .fs()
        .list(session.dir())?
        .iter()
        .any(|p| p.file_name().and_then(JournalId::from_file_name).is_some()))
}

/// Publishes the audio in `journals` in the session's directory as
/// segments: plans the segments that its store doesn't hold yet (see the
/// module docs for the rules), publishes each in the fixed order, and
/// deletes each journal once every sample it holds is in a committed row. A
/// journal that isn't there any more is skipped. One that's there but can't
/// be read is left as it is and reported in [`Published::unread`]; every
/// other segment is published.
///
/// A committed row claims its samples only if its file is in the session's
/// directory, can be read, and matches it: the file's SHA-256 is the row's,
/// and its FLAC header declares the row's number of samples. Only rows that
/// overlap the journals' samples are checked, so the cost is bounded by
/// what's being published. A row whose file is missing, can't be read (for
/// any reason, even one that may pass: the next run checks it again) or
/// doesn't match claims nothing, and is a finding: reported in
/// [`Published::findings`] and recorded in the session's findings file
/// before anything is published. No segment is
/// published over its samples, so its file, if any, is never replaced and
/// the journals holding them are kept; every other segment is published.
///
/// If the store can't be read, the findings file records that verification
/// was unavailable, keeping every earlier finding, and nothing is published
/// or deleted. Failing to write the findings file doesn't stop publishing
/// (see [`Published::findings_unsaved`]).
///
/// It takes [`FinishedJournal`]s, from
/// [`SessionWriter::take_finished`](crate::session::SessionWriter::take_finished),
/// because it deletes what it publishes: a journal still being written
/// can't be passed by mistake. They must be this session's: journal ids are
/// numbered per session, so another session's would name a journal here
/// that may still be recording.
///
/// # Errors
///
/// [`PublishError::OtherSession`], before anything is done, if a journal is
/// another session's. Otherwise [`PublishError`]: what was done before the
/// error is consistent, rows only for durable files, journals deleted only
/// after their rows. A journal that read in the first pass but can't be
/// read again when its segment is encoded (something changed it, or a
/// transient error) stops the run with [`PublishError::Changed`] or
/// [`PublishError::Io`]; the next run reads it again from the start.
pub fn publish_journals<S: Fs, T: SegmentStore>(
    session: &mut SessionStore<S, T>,
    length: SegmentLength,
    journals: &[FinishedJournal],
) -> Result<Published, PublishError> {
    let (session, store) = session.parts();
    if let Some(&foreign) = journals.iter().find(|j| j.session() != session.id()) {
        return Err(PublishError::OtherSession(foreign));
    }
    let (fs, dir) = (session.fs(), session.dir());
    let ids: BTreeSet<JournalId> = journals.iter().map(|j| j.id()).collect();

    // Pass 1: what each journal holds, without keeping its samples.
    let mut summaries = Vec::new();
    let mut unreadable = Vec::new();
    let mut unread = Vec::new();
    // Journals to delete once their segments are committed.
    let mut waiting: BTreeMap<JournalId, PathBuf> = BTreeMap::new();
    for &id in &ids {
        let path = dir.join(id.file_name());
        let bytes = match fs.read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                // Left as it is: the failure may pass, and renaming it aside
                // may fail too. Not in the plan, so it's never deleted.
                unread.push((FinishedJournal::new(session.id(), id), e.kind()));
                continue;
            }
        };
        let read = read_journal(&bytes);
        match read.header() {
            Some(header) if header.id() == id => {
                summaries.push(JournalSummary {
                    id,
                    track: header.track(),
                    epoch: header.epoch(),
                    rate: header.rate(),
                    range: read.range(),
                });
                if bytes.len() - read.valid_len() > max_torn_tail(header.rate())
                    || audio_past_a_crash(&bytes, &read, header)
                {
                    // More after the valid frames than a crash can leave:
                    // corruption, with audio after it that can't be read.
                    // Publish what reads, but keep the file.
                    unreadable.push((id, path));
                } else {
                    waiting.insert(id, path);
                }
            }
            // A crash while the journal was being created: its header is
            // fsync'd before any frame is written, so a file no longer
            // than a header that doesn't parse never held audio.
            None if bytes.len() <= HEADER_LEN => {
                waiting.insert(id, path);
            }
            _ => unreadable.push((id, path)),
        }
    }
    let rows = match store.rows() {
        Ok(rows) => rows,
        Err(e) => {
            // Nothing can be checked: say so, keeping what was found before.
            // The store's error is the one to report.
            let _unsaved = findings::record(fs, dir, &[], Verification::Unavailable);
            return Err(store_error(e));
        }
    };
    let mut published = Published {
        unread,
        ..Published::default()
    };
    let rows = claims(fs, dir, rows, &summaries, &mut published);
    published.findings_unsaved = findings::record(fs, dir, &published.findings, Verification::Done)
        .err()
        .map(|e| e.kind());
    let plan = plan::plan(&rows, &summaries, length);

    let mut committed = Committed::default();
    // Journals with no samples, or all of them in rows already, go first.
    release(
        fs,
        dir,
        &plan.needs,
        &committed,
        &mut waiting,
        &mut published,
    )?;

    // Pass 2: one segment at a time.
    for segment in &plan.segments {
        if in_a_bad_window(&published.findings, segment, length) {
            // Its samples stay in their journals, which need it and so
            // aren't deleted.
            continue;
        }
        let flac = encode(fs, dir, segment)?;
        let durable =
            TempSegment::write(fs, dir, segment.track, segment.epoch, segment.range, &flac)?
                .sync()?
                .rename(fs)?
                .sync_dir(fs)?;
        let done = durable.commit(store).map_err(store_error)?;
        committed.add(&done);
        published.segments.push(*done.row());
        release(
            fs,
            dir,
            &plan.needs,
            &committed,
            &mut waiting,
            &mut published,
        )?;
    }

    let unreadable = settled(unreadable, &plan.needs, &committed);
    quarantine(fs, dir, &unreadable, &mut published)?;
    Ok(published)
}

/// Of the committed `rows`, those that claim samples in this run: those
/// overlapping a journal in `summaries` whose file is in `dir`, reads, and
/// matches them. Overlapping rows whose file is missing, unreadable or
/// doesn't match go to `published.findings`.
fn claims<S: Fs>(
    fs: &S,
    dir: &Path,
    rows: Vec<SegmentRow>,
    summaries: &[JournalSummary],
    published: &mut Published,
) -> Vec<SegmentRow> {
    let mut claiming = Vec::new();
    for row in rows {
        let overlaps = summaries
            .iter()
            .any(|j| j.track == row.track() && j.range.is_some_and(|r| overlap(r, row.range())));
        if !overlaps {
            continue;
        }
        let path = dir.join(segment_file_name(row.track(), row.range()));
        let bytes = match fs.read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                let problem = match e.kind() {
                    io::ErrorKind::NotFound => Problem::Missing,
                    kind => Problem::Unreadable(ReadFailure::of(kind)),
                };
                published.findings.push(Finding::new(row, problem));
                continue;
            }
        };
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if &digest != row.sha256().as_bytes() {
            published
                .findings
                .push(Finding::new(row, Problem::HashMismatch));
        } else if flac::stream_len(&bytes) != Some(row.range().len().get()) {
            published
                .findings
                .push(Finding::new(row, Problem::LengthMismatch));
        } else {
            claiming.push(row);
        }
    }
    claiming
}

/// Whether `segment`'s window shares a sample with any of `findings`' rows
/// on its track. The whole window is left alone, not only the samples a row
/// claims: a window can hold several segments (an epoch change, a gap), and
/// none of them is published while a row over that window is unresolved.
/// Only rows this run checked are findings: those overlapping a journal it
/// read (see [`claims`]).
fn in_a_bad_window(findings: &[Finding], segment: &PlannedSegment, length: SegmentLength) -> bool {
    let start = segment.range.start();
    let from = SampleIndex::new(length.window_of(start).saturating_mul(length.samples()));
    let to = length
        .window_end(start)
        .unwrap_or(SampleIndex::new(u64::MAX));
    findings
        .iter()
        .map(Finding::row)
        .any(|r| r.track() == segment.track && r.range().start() < to && from < r.range().end())
}

const fn overlap(a: nota_core::SampleRange, b: nota_core::SampleRange) -> bool {
    a.start().get() < b.end().get() && b.start().get() < a.end().get()
}

/// Deletes every waiting journal whose segments are all committed.
fn release<S: Fs>(
    fs: &S,
    dir: &Path,
    needs: &BTreeMap<JournalId, BTreeSet<plan::SegmentKey>>,
    committed: &Committed,
    waiting: &mut BTreeMap<JournalId, PathBuf>,
    published: &mut Published,
) -> io::Result<()> {
    let empty = BTreeSet::new();
    let ready: Vec<DeletableJournal> = waiting
        .iter()
        .filter_map(|(&id, path)| committed.release(id, path, needs.get(&id).unwrap_or(&empty)))
        .collect();
    delete_journals(fs, dir, &ready)?;
    for journal in ready {
        waiting.remove(&journal.id());
        published.deleted.push(journal.id());
    }
    Ok(())
}

/// Of the `unreadable` journals, those to set aside: those nothing waits
/// on, every segment they're needed for committed. One whose window has a
/// bad row stays under its name, so a later run, after the row is
/// resolved, still finds and publishes it.
fn settled(
    unreadable: Vec<(JournalId, PathBuf)>,
    needs: &BTreeMap<JournalId, BTreeSet<plan::SegmentKey>>,
    committed: &Committed,
) -> Vec<PathBuf> {
    let empty = BTreeSet::new();
    unreadable
        .into_iter()
        .filter(|(id, path)| {
            committed
                .release(*id, path, needs.get(id).unwrap_or(&empty))
                .is_some()
        })
        .map(|(_, path)| path)
        .collect()
}

/// Renames unreadable journals aside, keeping their bytes.
fn quarantine<S: Fs>(
    fs: &S,
    dir: &Path,
    paths: &[PathBuf],
    published: &mut Published,
) -> io::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    for path in paths {
        let mut aside = path.clone().into_os_string();
        aside.push(".unreadable");
        let aside = PathBuf::from(aside);
        fs.rename(path, &aside)?;
        published.quarantined.push(aside);
    }
    fs.sync_dir(dir)
}

/// The most bytes a crash can leave after a journal's last valid frame: the
/// unsynced tail, at most the writer's sync budget of audio
/// ([`SYNC_INTERVAL`](crate::journal::SYNC_INTERVAL)), even if every sample
/// were a frame of its own, plus one more largest frame.
fn max_torn_tail(rate: SampleRate) -> usize {
    let per_sample = FRAME_HEADER_LEN + 2;
    let largest = FRAME_HEADER_LEN + 2 * usize::try_from(MAX_FRAME_SAMPLES).unwrap_or(usize::MAX);
    usize::try_from(sync_budget(rate).get())
        .unwrap_or(usize::MAX)
        .saturating_mul(per_sample)
        .saturating_add(largest)
}

/// Whether a frame after the valid ones, though they stop at damage, holds
/// audio a crash can't have left unsynced. The writer never has more than
/// its sync budget of audio unsynced
/// ([`SYNC_INTERVAL`](crate::journal::SYNC_INTERVAL)), and everything
/// before that is intact after a crash, so a torn tail's frames all start
/// within the budget of where the valid frames end (or, with none valid, of
/// each other). A frame starting later was fsync'd: the damage is
/// corruption, and the journal is kept. Damage in the last interval can't be
/// told from a crash this way.
///
/// A journal written under the earlier one-second rule can have a torn tail
/// longer than the budget; it is then kept as unreadable rather than
/// deleted, which loses nothing.
fn audio_past_a_crash(bytes: &[u8], read: &JournalRead, header: JournalHeader) -> bool {
    let starts: Vec<SampleIndex> = frames_after(bytes, read.valid_len(), header.track())
        .iter()
        .map(|r| r.start())
        .collect();
    let base = read
        .range()
        .map(SampleRange::end)
        .or_else(|| starts.iter().min().copied());
    let budget = SampleCount::new(sync_budget(header.rate()).get());
    base.and_then(|b| b.checked_add(budget))
        .is_some_and(|limit| starts.iter().any(|&s| s >= limit))
}

fn read_if_present<S: Fs>(fs: &S, path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs.read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reads the segment's journals again and encodes its samples, straight
/// from their frames.
fn encode<S: Fs>(fs: &S, dir: &Path, segment: &PlannedSegment) -> Result<Vec<u8>, PublishError> {
    let mut journals = BTreeMap::new();
    for part in &segment.parts {
        if let std::collections::btree_map::Entry::Vacant(slot) = journals.entry(part.journal) {
            let path = dir.join(part.journal.file_name());
            let bytes = read_if_present(fs, &path)?.ok_or(PublishError::Changed(part.journal))?;
            slot.insert(read_journal(&bytes));
        }
    }
    let mut pieces: Vec<&[i16]> = Vec::new();
    for part in &segment.parts {
        let read = journals
            .get(&part.journal)
            .ok_or(PublishError::Changed(part.journal))?;
        if read.header().map(JournalHeader::rate) != Some(segment.rate) {
            return Err(PublishError::Changed(part.journal));
        }
        let mut taken = 0_u64;
        for frame in read.frames() {
            let r = frame.range();
            let from = r.start().max(part.range.start());
            let to = r.end().min(part.range.end());
            if from >= to {
                continue;
            }
            let (Ok(a), Ok(b)) = (
                usize::try_from(from.get() - r.start().get()),
                usize::try_from(to.get() - r.start().get()),
            ) else {
                return Err(PublishError::Changed(part.journal));
            };
            let slice = frame
                .samples()
                .get(a..b)
                .ok_or(PublishError::Changed(part.journal))?;
            taken += slice.len() as u64;
            pieces.push(slice);
        }
        if taken != part.range.len().get() {
            return Err(PublishError::Changed(part.journal));
        }
    }
    flac::encode(segment.rate, &pieces).map_err(PublishError::Flac)
}
