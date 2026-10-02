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

use nota_core::SampleRate;
use nota_store::SegmentRow;

use super::flac::{self, FlacError};
use super::plan::{self, JournalSummary, PlannedSegment};
use super::publish::{Committed, DeletableJournal, TempSegment, delete_journals};
use super::store::SegmentStore;
use super::{SegmentLength, is_temp_segment, segment_file_name};
use crate::fs::Fs;
use crate::journal::format::{FRAME_HEADER_LEN, HEADER_LEN, MAX_FRAME_SAMPLES};
use crate::journal::{JournalHeader, JournalId, read_journal};
use crate::session::FinishedJournal;

/// What a publish run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Published {
    segments: Vec<SegmentRow>,
    deleted: Vec<JournalId>,
    quarantined: Vec<PathBuf>,
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
    /// unreadable bytes after their valid frames than a crash can leave
    /// (corruption; what did read was published).
    #[must_use]
    pub fn quarantined(&self) -> &[PathBuf] {
        &self.quarantined
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
        }
    }
}

impl Error for PublishError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Store(e) => Some(e.as_ref()),
            Self::Flac(e) => Some(e),
            Self::Changed(_) => None,
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

/// Recovers a session after a crash: publishes every journal left in `dir`
/// as segments, adds the rows missing from `store`, and deletes the
/// journals once their rows are committed. Leftover segment temp files are
/// removed first; they're never the only copy of anything.
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
    fs: &S,
    store: &mut T,
    dir: &Path,
    length: SegmentLength,
) -> Result<Published, PublishError> {
    let mut journals = Vec::new();
    for path in fs.list(dir)? {
        if is_temp_segment(&path) {
            match fs.remove(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        } else if let Some(id) = path.file_name().and_then(JournalId::from_file_name) {
            // At startup nothing is writing: every journal is finished.
            journals.push(FinishedJournal::new(id));
        }
    }
    publish_journals(fs, store, dir, length, &journals)
}

/// Whether `dir` holds journals: a session that was still recording when
/// nota stopped, and needs [`salvage`].
///
/// # Errors
///
/// Any I/O error listing `dir`.
pub fn needs_salvage<S: Fs>(fs: &S, dir: &Path) -> io::Result<bool> {
    Ok(fs
        .list(dir)?
        .iter()
        .any(|p| p.file_name().and_then(JournalId::from_file_name).is_some()))
}

/// Publishes the audio in `journals` in `dir` as segments: plans
/// the segments that `store` doesn't hold yet (see the module docs for the
/// rules), publishes each in the fixed order, and deletes each journal once
/// every sample it holds is in a committed row. A journal that isn't there
/// any more is skipped.
///
/// It takes [`FinishedJournal`]s, from
/// [`SessionWriter::take_finished`](crate::session::SessionWriter::take_finished),
/// because it deletes what it publishes: a journal still being written
/// can't be passed by mistake.
///
/// # Errors
///
/// [`PublishError`]. What was done before the error is consistent: rows
/// only for durable files, journals deleted only after their rows.
pub fn publish_journals<S: Fs, T: SegmentStore>(
    fs: &S,
    store: &mut T,
    dir: &Path,
    length: SegmentLength,
    journals: &[FinishedJournal],
) -> Result<Published, PublishError> {
    let ids: BTreeSet<JournalId> = journals.iter().map(|j| j.id()).collect();
    // A row claims its samples only if its file is here: the store should
    // hold this session's rows alone, and a row of another session (or one
    // whose file went missing) must never let a journal be deleted.
    let present: BTreeSet<PathBuf> = fs.list(dir)?.into_iter().collect();
    let rows: Vec<SegmentRow> = store
        .rows()
        .map_err(store_error)?
        .into_iter()
        .filter(|r| present.contains(&dir.join(segment_file_name(r.track(), r.range()))))
        .collect();

    // Pass 1: what each journal holds, without keeping its samples.
    let mut summaries = Vec::new();
    let mut unreadable = Vec::new();
    // Journals to delete once their segments are committed.
    let mut waiting: BTreeMap<JournalId, PathBuf> = BTreeMap::new();
    for &id in &ids {
        let path = dir.join(id.file_name());
        let Some(bytes) = read_if_present(fs, &path)? else {
            continue;
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
                if bytes.len() - read.valid_len() > max_torn_tail(header.rate()) {
                    // More unreadable bytes after the valid frames than a
                    // crash can leave: corruption, with audio after it that
                    // can't be read. Publish what reads, but keep the file.
                    unreadable.push(path);
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
            _ => unreadable.push(path),
        }
    }
    let plan = plan::plan(&rows, &summaries, length);

    let mut published = Published::default();
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

    quarantine(fs, dir, &unreadable, &mut published)?;
    Ok(published)
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
/// unsynced tail, at most a second of audio (the writer's sync rule), even
/// if every sample were a frame of its own, plus one more largest frame.
fn max_torn_tail(rate: SampleRate) -> usize {
    let per_sample = FRAME_HEADER_LEN + 2;
    let largest = FRAME_HEADER_LEN + 2 * usize::try_from(MAX_FRAME_SAMPLES).unwrap_or(usize::MAX);
    usize::try_from(rate.hz())
        .unwrap_or(usize::MAX)
        .saturating_mul(per_sample)
        .saturating_add(largest)
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
