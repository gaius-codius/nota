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
use super::publish::{
    Committed, DeletableJournal, DurableSegment, StepError, TempSegment, delete_journals,
};
use super::store::SegmentStore;
use super::{SegmentLength, is_temp_segment, segment_file_name};
use crate::fs::Fs;
use crate::journal::format::{FRAME_HEADER_LEN, HEADER_LEN, MAX_FRAME_SAMPLES, frames_after};
use crate::journal::{JournalHeader, JournalId, JournalRead, read_journal, sync_budget};
use crate::session::{FinishedJournal, SessionDir, SessionStore, Use};

/// What a publish run did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Published {
    segments: Vec<SegmentRow>,
    deleted: Vec<JournalId>,
    quarantined: Vec<PathBuf>,
    not_set_aside: Vec<(PathBuf, io::ErrorKind)>,
    not_deleted: Vec<(JournalId, io::ErrorKind)>,
    blocked: Vec<(PathBuf, io::ErrorKind)>,
    temps_kept: Vec<(PathBuf, io::ErrorKind)>,
    unread: Vec<(FinishedJournal, io::ErrorKind)>,
    findings: Vec<Finding>,
    findings_unsaved: Option<io::ErrorKind>,
    set_aside_unsynced: Option<io::ErrorKind>,
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
    /// its name until that window publishes, and one whose aside name is
    /// taken stays under its name too (see [`Self::not_set_aside`]).
    #[must_use]
    pub fn quarantined(&self) -> &[PathBuf] {
        &self.quarantined
    }

    /// Journal files that were to be set aside (as [`Self::quarantined`]
    /// says) but couldn't be, with the error's kind: something is already
    /// under the aside name (`AlreadyExists`; it's never replaced), or the
    /// rename failed. Each is left under its own name, so every run publishes
    /// what reads of it again (finding it already in rows) and reports it
    /// here, until the name is free.
    #[must_use]
    pub fn not_set_aside(&self) -> &[(PathBuf, io::ErrorKind)] {
        &self.not_set_aside
    }

    /// Journals whose audio is all in committed rows but that couldn't be
    /// deleted (an immutable file's `EPERM`, `EIO`), with the error's kind.
    /// Each is left as it is; nothing in it is lost, and the next run tries
    /// again.
    #[must_use]
    pub fn not_deleted(&self) -> &[(JournalId, io::ErrorKind)] {
        &self.not_deleted
    }

    /// Names a planned segment needed but couldn't use, with the error's
    /// kind: its temp name (something there can't be removed, or the file
    /// can't be created) or its own (the rename onto it failed, as onto a
    /// directory). That segment wasn't published and its journals are kept;
    /// every other segment was. A run reports each again until the name is
    /// free.
    #[must_use]
    pub fn blocked(&self) -> &[(PathBuf, io::ErrorKind)] {
        &self.blocked
    }

    /// Segment temp files salvage found but couldn't remove, with the
    /// error's kind (a directory under the name, an immutable file). Each is
    /// left as it is; it's never the only copy of anything. Only
    /// [`salvage`] removes temp files, so only it reports these.
    #[must_use]
    pub fn temps_kept(&self) -> &[(PathBuf, io::ErrorKind)] {
        &self.temps_kept
    }

    /// Why the directory couldn't be synced after journals were set aside,
    /// if it couldn't. They're under their aside names
    /// ([`Self::quarantined`]) all the same, but not durably: after a crash
    /// one may be back under its own name, and the next run sets it aside
    /// again.
    #[must_use]
    pub const fn set_aside_unsynced(&self) -> Option<io::ErrorKind> {
        self.set_aside_unsynced
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
    /// Refused, with nothing done: the session's owner is already using it
    /// in a way this can't run alongside. Salvage needs the session to
    /// itself, since a writer's journals aren't finished; publishing runs
    /// one at a time, and never during salvage.
    InUse(Use),
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
            Self::InUse(Use::Salvaging) => f.write_str("the session is being salvaged"),
            Self::InUse(Use::Publishing) => f.write_str("the session is being published"),
            Self::InUse(Use::Recording) => {
                f.write_str("the session is being recorded; it can't be salvaged")
            }
        }
    }
}

impl Error for PublishError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Store(e) => Some(e.as_ref()),
            Self::Flac(e) => Some(e),
            Self::Changed(_) | Self::OtherSession(_) | Self::InUse(_) => None,
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
/// temp files are removed first; they're never the only copy of anything. A
/// segment temp file that can't be removed is left and reported (see
/// [`Published::temps_kept`]).
///
/// Safe to run again after it fails or the machine crashes partway: every
/// step is repeatable, and a second run on a recovered session changes
/// nothing. `length` should be what the session recorded with; another
/// value still loses nothing, but splits segments differently.
///
/// Every journal it finds is taken as finished, so it runs only on a
/// session nothing is recording: the store's [`SessionLock`] keeps out
/// other owners, and within the owner it refuses while a
/// [`SessionWriter`](crate::session::SessionWriter) is open.
///
/// # Errors
///
/// [`PublishError::InUse`], before anything is done, if the owner is
/// recording, publishing or already salvaging the session. Otherwise as
/// [`publish_journals`].
///
/// [`SessionLock`]: crate::session::SessionLock
pub fn salvage<S: Fs, T: SegmentStore>(
    session: &mut SessionStore<S, T>,
    length: SegmentLength,
) -> Result<Published, PublishError> {
    let _salvaging = session
        .lock()
        .begin(Use::Salvaging)
        .map_err(PublishError::InUse)?;
    let (fs, dir) = (session.session().fs(), session.session().dir());
    let mut journals = Vec::new();
    let mut temps_kept = Vec::new();
    for path in fs.list(dir)? {
        if findings::is_temp(&path) || crate::session::is_marks_temp(&path) {
            // Best effort: the next findings write removes it anyway, and
            // the findings must never hold up publishing.
            let _gone = fs.remove(&path);
        } else if is_temp_segment(&path) {
            match fs.remove(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                // Left as it is: the segment it's named for reports its own
                // name blocked if it's planned, and nothing else needs it.
                Err(e) => temps_kept.push((path, e.kind())),
            }
        } else if let Some(id) = path.file_name().and_then(JournalId::from_file_name) {
            // At startup nothing is writing: every journal is finished.
            journals.push(FinishedJournal::new(session.session().id(), id));
        }
    }
    let mut published = publish(session, length, &journals)?;
    published.temps_kept = temps_kept;
    Ok(published)
}

/// Whether the session's directory holds journals, so [`salvage`] may have
/// work to do. A session that was still recording when nota stopped holds
/// some, but so can one already salvaged: salvage keeps a journal it can't
/// read, delete or set aside, whose segment's name is blocked, or whose
/// audio is held up by a row that doesn't match its file (see
/// [`Published`]). Salvaging such a session again changes nothing, so don't
/// loop on this: it stays true until someone clears what holds them.
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
/// other segment is published. So is every segment but one whose temp or
/// own name can't be used ([`Published::blocked`]), and a journal that
/// can't be deleted or set aside is left and reported
/// ([`Published::not_deleted`], [`Published::not_set_aside`]) without
/// stopping the run.
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
/// Within the session's owner, one publishing run goes at a time, and none
/// while salvage runs; it runs alongside recording.
///
/// # Errors
///
/// [`PublishError::InUse`], before anything is done, if the owner is
/// already publishing or salvaging the session.
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
    let _publishing = session
        .lock()
        .begin(Use::Publishing)
        .map_err(PublishError::InUse)?;
    publish(session, length, journals)
}

/// [`publish_journals`], for a caller already using the session to itself.
fn publish<S: Fs, T: SegmentStore>(
    session: &mut SessionStore<S, T>,
    length: SegmentLength,
    journals: &[FinishedJournal],
) -> Result<Published, PublishError> {
    let (session, mut store) = session.parts();
    if let Some(foreign) = journals.iter().find(|j| j.session() != session.id()) {
        return Err(PublishError::OtherSession(FinishedJournal::new(
            foreign.session(),
            foreign.id(),
        )));
    }
    let (fs, dir) = (session.fs(), session.dir());
    let ids: BTreeSet<JournalId> = journals.iter().map(FinishedJournal::id).collect();

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
        let durable = match write_segment(fs, dir, segment)? {
            Ok(durable) => durable,
            Err((path, kind)) => {
                // Nothing durable changed: like a bad window, its samples
                // stay in their journals, which need it.
                published.blocked.push((path, kind));
                continue;
            }
        };
        let done = durable.commit(&mut store).map_err(store_error)?;
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
    quarantine(fs, dir, &unreadable, &mut published);
    Ok(published)
}

/// Encodes `segment` and makes its file durable under its name: steps 1 to
/// 4 of the module's order. `Ok(Err(..))` if a name it needs can't be used,
/// with that name and the error's kind; nothing durable changed then.
fn write_segment<S: Fs>(
    fs: &S,
    dir: &Path,
    segment: &PlannedSegment,
) -> Result<Result<DurableSegment, (PathBuf, io::ErrorKind)>, PublishError> {
    let flac = encode(fs, dir, segment)?;
    let renamed = TempSegment::write(fs, dir, segment.track, segment.epoch, segment.range, &flac)
        .and_then(|temp| temp.sync().map_err(StepError::Io))
        .and_then(|synced| synced.rename(fs));
    match renamed {
        Ok(renamed) => Ok(Ok(renamed.sync_dir(fs)?)),
        Err(StepError::Name(path, e)) => Ok(Err((path, e.kind()))),
        Err(StepError::Io(e)) => Err(e.into()),
    }
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
    let from = SampleIndex::new(
        length
            .window_of(start)
            .saturating_mul(length.samples().get()),
    );
    let to = length
        .window_end(start)
        .unwrap_or(SampleIndex::new(u64::MAX));
    findings
        .iter()
        .map(Finding::row)
        .any(|r| r.track() == segment.track && r.range().start() < to && from < r.range().end())
}

const fn overlap(a: SampleRange, b: SampleRange) -> bool {
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
    let kept = delete_journals(fs, dir, &ready)?;
    for journal in ready {
        // One that couldn't be deleted isn't tried again this run: it holds
        // nothing still to publish, and the next run finds it again.
        waiting.remove(&journal.id());
        match kept.iter().find(|(id, _)| *id == journal.id()) {
            Some(&failed) => published.not_deleted.push(failed),
            None => published.deleted.push(journal.id()),
        }
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

/// Renames unreadable journals aside, keeping their bytes. One whose aside
/// name is taken, by anything, keeps its own name: a rename would replace a
/// file there, which may be another journal set aside. It, and one whose
/// rename fails, are reported in [`Published::not_set_aside`]. Nothing here
/// stops the run: what it published stays reported, renames that worked
/// with it, even if the directory can't be synced after them
/// ([`Published::set_aside_unsynced`]).
fn quarantine<S: Fs>(fs: &S, dir: &Path, paths: &[PathBuf], published: &mut Published) {
    if paths.is_empty() {
        return;
    }
    let there = match fs.list(dir) {
        Ok(there) => there,
        Err(e) => {
            let kind = e.kind();
            published
                .not_set_aside
                .extend(paths.iter().map(|p| (p.clone(), kind)));
            return;
        }
    };
    let mut renamed = false;
    for path in paths {
        let aside = set_aside_path(path);
        if there.contains(&aside) {
            published
                .not_set_aside
                .push((path.clone(), io::ErrorKind::AlreadyExists));
            continue;
        }
        match fs.rename(path, &aside) {
            Ok(()) => {
                renamed = true;
                published.quarantined.push(aside);
            }
            Err(e) => published.not_set_aside.push((path.clone(), e.kind())),
        }
    }
    if renamed && let Err(e) = fs.sync_dir(dir) {
        published.set_aside_unsynced = Some(e.kind());
    }
}

/// Where a journal at `path` is set aside: its name with
/// [`SET_ASIDE`](crate::session::SET_ASIDE) appended.
fn set_aside_path(path: &Path) -> PathBuf {
    let mut aside = path.as_os_str().to_os_string();
    aside.push(crate::session::SET_ASIDE);
    PathBuf::from(aside)
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
