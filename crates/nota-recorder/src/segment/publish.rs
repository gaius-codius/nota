//! The segment publish steps, as a typestate: each step consumes the value
//! the step before it made, so they can only run in order.
//!
//! [`TempSegment`] (written) → [`SyncedSegment`] (fsync'd) →
//! [`RenamedSegment`] → [`DurableSegment`] (directory fsync'd) →
//! [`CommittedSegment`] (row in the store) → [`DeletableJournal`] once
//! every segment a journal holds is committed.
//!
//! Every type has private fields and one way in, so a row can't be
//! committed for a file that isn't durable, and a journal can't be deleted
//! for a segment that isn't committed.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use nota_core::{EpochId, SampleRange, TrackId};
use nota_store::{SegmentRow, Sha256Digest};
use sha2::{Digest, Sha256};

use super::plan::SegmentKey;
use super::store::SegmentStore;
use super::{segment_file_name, temp_path};
use crate::fs::{Fs, FsFile, Synced};
use crate::journal::JournalId;
use crate::session::Rows;

/// What every step carries: where the segment goes, and its row.
#[derive(Debug)]
struct Segment {
    dir: PathBuf,
    temp: PathBuf,
    path: PathBuf,
    row: SegmentRow,
}

/// Step 1: the encoded FLAC written to the temp file, not yet durable.
#[derive(Debug)]
pub(super) struct TempSegment<F> {
    segment: Segment,
    file: F,
}

impl<F: FsFile> TempSegment<F> {
    /// Writes `flac`, the encoded segment holding `range` of `track` from
    /// `epoch`, to its temp file in `dir`. A temp file left by an earlier
    /// attempt is removed first: it's never the only copy of anything.
    ///
    /// # Errors
    ///
    /// Any I/O error. `None`-like failures (an empty range) are an
    /// `InvalidInput` error.
    pub(super) fn write<S: Fs<File = F>>(
        fs: &S,
        dir: &Path,
        track: TrackId,
        epoch: EpochId,
        range: SampleRange,
        flac: &[u8],
    ) -> io::Result<Self> {
        let sha256 = Sha256Digest::new(Sha256::digest(flac).into());
        let row = SegmentRow::new(track, epoch, range, sha256).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "a segment needs samples")
        })?;
        let temp = temp_path(dir, track, range);
        match fs.remove(&temp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut file = fs.create(&temp)?;
        file.write_all(flac)?;
        Ok(Self {
            segment: Segment {
                dir: dir.to_path_buf(),
                path: dir.join(segment_file_name(track, range)),
                temp,
                row,
            },
            file,
        })
    }

    /// Step 2: fsyncs the temp file.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub(super) fn sync(mut self) -> io::Result<SyncedSegment> {
        let synced = self.file.sync()?;
        Ok(SyncedSegment {
            segment: self.segment,
            _synced: synced,
        })
    }
}

/// Step 2 done: the temp file's data is on disk.
#[derive(Debug)]
pub(super) struct SyncedSegment {
    segment: Segment,
    _synced: Synced,
}

impl SyncedSegment {
    /// Step 3: renames the temp file to the segment's name, replacing any
    /// file there (an earlier attempt's, with the same audio).
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub(super) fn rename<S: Fs>(self, fs: &S) -> io::Result<RenamedSegment> {
        fs.rename(&self.segment.temp, &self.segment.path)?;
        Ok(RenamedSegment {
            segment: self.segment,
        })
    }
}

/// Step 3 done: the segment has its name, not yet durably.
#[derive(Debug)]
pub(super) struct RenamedSegment {
    segment: Segment,
}

impl RenamedSegment {
    /// Step 4: fsyncs the directory, making the name durable.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    pub(super) fn sync_dir<S: Fs>(self, fs: &S) -> io::Result<DurableSegment> {
        fs.sync_dir(&self.segment.dir)?;
        Ok(DurableSegment {
            segment: self.segment,
        })
    }
}

/// A segment file that is durable under its final name: the only thing a
/// [`SegmentStore`] can commit a row for.
#[derive(Debug)]
pub struct DurableSegment {
    segment: Segment,
}

impl DurableSegment {
    /// The row to commit.
    #[must_use]
    pub const fn row(&self) -> &SegmentRow {
        &self.segment.row
    }

    /// Where the file is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.segment.path
    }

    /// Step 5: commits the row. Once it returns, the row is durable.
    ///
    /// # Errors
    ///
    /// The store's error.
    pub(super) fn commit<T: SegmentStore>(
        self,
        store: &mut Rows<'_, T>,
    ) -> Result<CommittedSegment, T::Error> {
        store.insert(&self)?;
        Ok(CommittedSegment {
            row: self.segment.row,
        })
    }
}

/// Step 5 done: the segment's row is in the store, after its file.
#[derive(Debug)]
pub(super) struct CommittedSegment {
    row: SegmentRow,
}

impl CommittedSegment {
    pub(super) const fn row(&self) -> &SegmentRow {
        &self.row
    }
}

/// The segments committed so far in one publish run: only
/// [`CommittedSegment`]s go in, so a journal is released only for segments
/// whose rows are durable.
#[derive(Debug, Default)]
pub(super) struct Committed(BTreeSet<SegmentKey>);

impl Committed {
    pub(super) fn add(&mut self, segment: &CommittedSegment) {
        self.0
            .insert((segment.row.track(), segment.row.range().start()));
    }

    /// The journal at `path`, released for deletion if every segment it
    /// `needs` is committed. A journal that needs none holds only samples
    /// already in rows committed before this run (or none at all).
    pub(super) fn release(
        &self,
        id: JournalId,
        path: &Path,
        needs: &BTreeSet<SegmentKey>,
    ) -> Option<DeletableJournal> {
        needs.is_subset(&self.0).then(|| DeletableJournal {
            id,
            path: path.to_path_buf(),
        })
    }
}

/// A journal whose every sample is in a committed row. Only
/// [`Committed::release`] makes one.
#[derive(Debug)]
pub(super) struct DeletableJournal {
    id: JournalId,
    path: PathBuf,
}

impl DeletableJournal {
    pub(super) const fn id(&self) -> JournalId {
        self.id
    }
}

/// Step 6: deletes `journals` and fsyncs the directory. A journal already
/// gone is fine.
///
/// # Errors
///
/// Any I/O error.
pub(super) fn delete_journals<S: Fs>(
    fs: &S,
    dir: &Path,
    journals: &[DeletableJournal],
) -> io::Result<()> {
    if journals.is_empty() {
        return Ok(());
    }
    for journal in journals {
        match fs.remove(&journal.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    fs.sync_dir(dir)
}
