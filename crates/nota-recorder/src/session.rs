//! Recording a session's tracks into journals that rotate at every segment
//! boundary.
//!
//! The [`SessionWriter`] owns one [`JournalWriter`] per track at a time and
//! gives every new journal the session's next [`JournalId`]. A track's
//! journal ends, and the next one starts lazily with the next sample, when:
//! - the track reaches a segment window boundary ([`SegmentLength`]),
//!   whether or not earlier segments have been published, so no journal
//!   ever holds more than one window of audio;
//! - the track starts a new epoch, so a journal holds one epoch;
//! - the journal breaks (a failed write or fsync).
//!
//! Ended journals are handed out by [`SessionWriter::take_finished`], for
//! [`publish_journals`](crate::segment::publish_journals). A broken journal
//! is held back until its replacement ends too, so they're published
//! together and the newer one wins where they overlap.
//!
//! # Journal breaks
//!
//! Each track keeps the samples written since its journal's last fsync: at
//! most [`SYNC_INTERVAL`](crate::journal::SYNC_INTERVAL)'s worth of audio,
//! by the journal's sync rule. When a journal breaks, a replacement starts
//! at the durable position and those samples are written to it again, so a
//! failed write or fsync loses nothing. If
//! the broken journal's unsynced tail survives a crash as well, the two
//! journals overlap, and the replacement's higher id wins in salvage. If
//! the replacement fails too, the error is returned and the samples from
//! the durable position to the end of the failed call are a gap; the next
//! call starts a new journal.
//!
//! # A full disk
//!
//! A journal that can't start for want of space (`ENOSPC`, `EDQUOT`), new
//! or a replacement, gets one more try. Through a
//! [`WatchedFs`](crate::disk::WatchedFs) the first such failure has freed
//! the ballast by then, so a full disk costs no audio: the write that met
//! it breaks its journal, and the replacement takes over its samples in
//! the ballast's room (see [`disk`](crate::disk)).
//!
//! # Fsyncs on other threads
//!
//! With [`Syncing::Threads`] each track's fsyncs run on a thread of its
//! own, so the writer never waits on one and a slow fsync holds back only
//! its own track:
//! - A sync started covers what the journal held when it started; the
//!   durable position moves once its result is taken, by the next call.
//! - A journal whose sync budget is full while its fsync runs takes no
//!   more audio: the rest waits in memory, in the track's
//!   [`SessionWriter::next_sample`] but not yet written, until the fsync
//!   completes. No journal ever holds more than its budget unsynced, as
//!   salvage expects.
//! - A journal that ends (at a window boundary or a new epoch) has its
//!   last fsync started, and is handed out by
//!   [`SessionWriter::take_finished`] once that completes, in order. The
//!   next journal starts meanwhile. (Broken journals whose replacement
//!   couldn't start are handed out at once, a gap, even ahead of older
//!   ones still waiting; journals never overlap, so publishing doesn't
//!   depend on the order.) Until then the track's
//!   [`SessionWriter::durable`] is the ended journal's.
//! - A failure found late is handled as one found at once: the broken
//!   journal is replaced from its durable position, an ended one by a
//!   journal that is ended in its place.
//!
//! Inline (the default), each fsync runs as it starts, as before.
//!
//! [`Syncing::Auto`], as recording runs, is inline while one track
//! records. When a second track starts, every track's fsyncs move to
//! threads; syncs the first track ran inline and the writer hasn't taken
//! yet come back first, so nothing about its durable position changes.
//!
//! # Resuming a session
//!
//! A writer may open on a session that has recorded before (after salvage,
//! or with journals still there). It never reuses what that recording used:
//! - **Journal ids** continue after every id the session has used, kept in
//!   its marks file (see `marks`) and in the names still on disk. Ids are
//!   reserved durably before use, so publishing a stale
//!   [`FinishedJournal`] can't delete a newer journal.
//! - **Sample numbers:** [`SessionWriter::start_track`] refuses a first
//!   sample before the end of what the track already holds, in journals or
//!   published segments ([`SessionWriter::first_free_sample`]). New audio at
//!   covered sample numbers would plan no segment, and its journal would be
//!   deleted.
//! - **Epoch ids:** each track's epochs must be higher than any it has
//!   journaled ([`SessionWriter::highest_epoch`]), so two recordings never
//!   share one.
//! - **Session time:** a track's first epoch must start no earlier than
//!   its earlier audio ends ([`SessionWriter::earlier_end`]), so the new
//!   recording comes after the old one. Start the session's clock at
//!   [`SessionWriter::resume_from`], and each track's timeline from
//!   [`SessionWriter::resumed_timeline`], which carries on from the newest
//!   epoch the marks keep.
//!
//! # Epoch anchors
//!
//! Every journal's header holds its epoch's anchor, and the marks hold each
//! track's newest, both written before the epoch's first sample. Salvage
//! times any journal it finds by its header; a resumed session times its
//! tracks on from the marks.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use nota_core::{
    Clock, Epoch, EpochAnchor, EpochError, EpochId, SampleCount, SampleIndex, SampleRate,
    SessionId, SessionTime, TrackId, TrackTimeline,
};

use crate::fs::{Fs, FsFile};
use crate::journal::format::frames_after;
use crate::journal::{
    DurablePosition, JournalError, JournalHeader, JournalId, JournalWriter, SyncDone, read_journal,
};
use crate::segment::SegmentLength;

mod handle;
mod marks;
mod syncs;

use handle::InUse;
pub(crate) use handle::Rows;
pub use handle::{SessionDir, SessionLock, SessionStore, Use};
pub(crate) use marks::is_temp as is_marks_temp;
pub use marks::{BadMarks, FILE_NAME as MARKS_FILE_NAME};
use marks::{MarkedEpoch, Marks};
pub use syncs::Syncing;
use syncs::TrackSyncs;

/// How many journal ids one write of the marks reserves, so the marks
/// aren't rewritten at every rotation. Ids skipped by a restart are never
/// used; that's harmless.
const ID_BLOCK: u64 = 64;

/// A journal no writer will append to again: ended by a [`SessionWriter`],
/// or found on disk by salvage at startup, when nothing is recording.
/// Publishing deletes the journals it publishes, so it takes only these: a
/// journal still being written can't be handed to it by mistake. Journal
/// ids are numbered per session, so it names its session too, and
/// publishing refuses another session's.
///
/// It's neither `Copy` nor `Clone`: each is handed out once. Ids never
/// repeat within a session, so even one kept for a retry names only the
/// journal it was made for.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FinishedJournal {
    session: SessionId,
    id: JournalId,
}

impl FinishedJournal {
    /// Only the session writer, and salvage, say a journal is finished.
    pub(crate) const fn new(session: SessionId, id: JournalId) -> Self {
        Self { session, id }
    }

    /// The session the journal belongs to.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// The journal's id.
    #[must_use]
    pub const fn id(&self) -> JournalId {
        self.id
    }
}

/// Why the session writer couldn't do what was asked.
#[derive(Debug)]
pub enum SessionError {
    /// The journal failed, and so did its replacement. The samples since
    /// the last durable position were dropped.
    Journal(JournalError),
    /// Reading the session directory failed.
    Io(std::io::Error),
    /// Writing the session's marks failed, so a journal couldn't start: like
    /// [`Self::Journal`], the samples it would have held are a gap, and the
    /// next call tries again.
    Marks(std::io::Error),
    /// The session's owner is already using it: salvaging it, or recording
    /// it with another writer.
    InUse(Use),
    /// Audio or an epoch for a track that wasn't started.
    UnknownTrack(TrackId),
    /// A track started twice.
    TrackExists(TrackId),
    /// A track started, or moved to an epoch, no higher than one it has
    /// already used.
    EpochUsed {
        /// The track.
        track: TrackId,
        /// The epoch asked for.
        epoch: EpochId,
    },
    /// A track started, or moved to an epoch, that doesn't fit it: at
    /// another rate than the writer's, or, for a new epoch, not starting at
    /// the track's next sample.
    EpochMisplaced {
        /// The track.
        track: TrackId,
        /// The epoch asked for.
        epoch: EpochId,
    },
    /// A track started in an epoch that starts before its earlier
    /// recordings' audio ends: the session's clock wasn't resumed.
    TimeWentBack {
        /// The track.
        track: TrackId,
        /// When its earlier audio ends.
        earlier_end: SessionTime,
    },
    /// A track started before the end of what it already holds.
    Covered {
        /// The track.
        track: TrackId,
        /// The first sample it may start at.
        first_free: SampleIndex,
    },
    /// The session ran out of journal ids or a track out of sample numbers.
    Overflow,
    /// A file in the session directory is named for the last journal id
    /// (`journal-18446744073709551615`, or that set aside), so no journal
    /// can come after it. Nothing was done; moving the file out lets the
    /// session open.
    LastJournalId(PathBuf),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Journal(e) => write!(f, "recording to the journal failed: {e}"),
            Self::Io(e) => write!(f, "reading the session directory failed: {e}"),
            Self::InUse(Use::Salvaging) => f.write_str("the session is being salvaged"),
            Self::InUse(Use::Publishing) => f.write_str("the session is being published"),
            Self::InUse(Use::Recording) => f.write_str("the session is already being recorded"),
            Self::Marks(e) => write!(f, "reserving the next journal's ids failed: {e}"),
            Self::UnknownTrack(t) => write!(f, "track {} wasn't started", t.get()),
            Self::TrackExists(t) => write!(f, "track {} was already started", t.get()),
            Self::EpochUsed { track, epoch } => write!(
                f,
                "track {} has already used epoch {} or a later one",
                track.get(),
                epoch.get()
            ),
            Self::EpochMisplaced { track, epoch } => write!(
                f,
                "epoch {} doesn't fit track {}: another rate, or not at its next sample",
                epoch.get(),
                track.get()
            ),
            Self::TimeWentBack { track, earlier_end } => write!(
                f,
                "track {} would start before its earlier audio ends at {} ns",
                track.get(),
                earlier_end.as_nanos()
            ),
            Self::Covered { track, first_free } => write!(
                f,
                "track {} already holds audio before sample {}",
                track.get(),
                first_free.get()
            ),
            Self::Overflow => f.write_str("ran out of journal ids or sample numbers"),
            Self::LastJournalId(path) => write!(
                f,
                "{} takes the last journal id, so no journal can follow it",
                path.display()
            ),
        }
    }
}

impl SessionError {
    /// Whether it failed for want of space (`ENOSPC` or `EDQUOT`; see
    /// [`is_disk_full`](crate::fs::is_disk_full)).
    #[must_use]
    pub fn is_disk_full(&self) -> bool {
        match self {
            Self::Journal(JournalError::Io(e)) | Self::Io(e) | Self::Marks(e) => {
                crate::fs::is_disk_full(e)
            }
            _ => false,
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Journal(e) => Some(e),
            Self::Io(e) | Self::Marks(e) => Some(e),
            _ => None,
        }
    }
}

/// Why [`SessionWriter::finish`] failed, with the journals that did finish,
/// so they can still be published.
#[derive(Debug)]
pub struct FinishError {
    error: SessionError,
    finished: Vec<FinishedJournal>,
}

impl FinishError {
    /// The first error.
    #[must_use]
    pub const fn error(&self) -> &SessionError {
        &self.error
    }

    /// Every finished journal not yet taken.
    #[must_use]
    pub fn into_finished(self) -> Vec<FinishedJournal> {
        self.finished
    }

    /// The first error, and every finished journal not yet taken.
    #[must_use]
    pub fn into_parts(self) -> (SessionError, Vec<FinishedJournal>) {
        (self.error, self.finished)
    }
}

impl fmt::Display for FinishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for FinishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

#[derive(Debug)]
struct Track<F: FsFile> {
    /// The epoch the track records in.
    epoch: Epoch,
    /// The next sample to record.
    next: SampleIndex,
    /// The journal being written.
    journal: Option<Open<F>>,
    /// Journals ended and waiting for their last fsync, oldest first.
    ending: VecDeque<Open<F>>,
    /// Samples taken and not yet written, up to `next`: the journal was
    /// full while its fsync ran.
    waiting: Vec<i16>,
    /// Where the track's fsyncs run.
    syncs: TrackSyncs<F::Syncer>,
}

impl<F: FsFile> Track<F> {
    /// The first waiting sample's number: where the next write goes.
    fn waiting_from(&self) -> SampleIndex {
        SampleIndex::new(self.next.get().saturating_sub(self.waiting.len() as u64))
    }
}

/// Records a session's tracks into rotating journals in one directory.
///
/// Its fsyncs run as its [`Syncing`] says: inline by default, where, like
/// [`JournalWriter`], it may fsync on any call, so it belongs on its own
/// thread; or, with [`Self::with_syncing`], on a thread for each track,
/// so one track's fsync never holds up another's audio. Call
/// [`Self::sync_if_due`] on a timer.
///
/// It keeps its session owned, and marked as recording, until it's
/// finished or dropped.
#[derive(Debug)]
pub struct SessionWriter<S: Fs> {
    session: SessionId,
    fs: S,
    dir: PathBuf,
    rate: SampleRate,
    length: SegmentLength,
    clock: Arc<dyn Clock>,
    syncing: Syncing,
    next_id: Option<JournalId>,
    tracks: BTreeMap<TrackId, Track<S::File>>,
    finished: Vec<FinishedJournal>,
    /// The marks as they are on disk.
    marks: Marks,
    /// What each track already holds from earlier recordings.
    earlier: BTreeMap<TrackId, Earlier>,
    _recording: InUse<S::Lock>,
}

/// What a track already holds when a writer opens: no new audio or epoch
/// may land on it.
#[derive(Debug, Clone, Copy, Default)]
struct Earlier {
    /// The first sample after every journal and published segment.
    end: Option<SampleIndex>,
    /// The highest epoch journaled, with its anchor if one was kept.
    epoch: Option<MarkedEpoch>,
}

impl<S: Fs> SessionWriter<S> {
    /// A writer recording into the owned `session`'s directory, at `rate`,
    /// rotating every `length`. It marks the session as recording until
    /// it's finished or dropped. Salvage the session first.
    ///
    /// What the session recorded before is read from its directory: its
    /// marks, the journals still there (and those salvage set aside), and
    /// each track's newest published segment. New journal ids continue
    /// after every one used, so they stay in order and never repeat across
    /// a restart, and tracks can't start inside what they hold (see the
    /// module docs).
    ///
    /// It takes the session's lock, not its [`SessionStore`]: recording
    /// never depends on the store.
    ///
    /// # Errors
    ///
    /// [`SessionError::InUse`] if the owner is salvaging the session,
    /// already recording it, or publishing it (it may open once that run
    /// ends, and then record alongside publishing); [`SessionError::Io`] if the directory, a
    /// journal still to publish or the marks can't be read (damaged marks
    /// are [`std::io::ErrorKind::InvalidData`]);
    /// [`SessionError::LastJournalId`], naming the file, if one takes the
    /// last journal id; [`SessionError::Overflow`] if the marks have used
    /// them all.
    pub fn open(
        session: &SessionLock<S>,
        rate: SampleRate,
        length: SegmentLength,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, SessionError>
    where
        S: Clone,
    {
        let recording = session.begin(Use::Recording).map_err(SessionError::InUse)?;
        // What the directory holds must hold still while it's read: no
        // publishing run may delete a journal or add a segment meanwhile.
        let _still = session
            .begin(Use::Publishing)
            .map_err(SessionError::InUse)?;
        let id = session.session().id();
        let fs = session.session().fs().clone();
        let dir = session.session().dir().to_path_buf();
        let paths = fs.list(&dir).map_err(SessionError::Io)?;
        let marks = if paths.contains(&dir.join(MARKS_FILE_NAME)) {
            Marks::read(&fs, &dir).map_err(SessionError::Io)?
        } else {
            Marks::default()
        };
        let mut earlier: BTreeMap<TrackId, Earlier> = BTreeMap::new();
        for (&track, &epoch) in &marks.epochs {
            earlier.entry(track).or_default().raise_epoch(epoch);
        }
        for (track, end) in crate::segment::published_ends(&fs, &dir, &paths, length) {
            earlier.entry(track).or_default().raise_end(end);
        }
        let mut next_id = marks.journals_below;
        for path in &paths {
            let Some(journal) = path.file_name().and_then(journal_id_in_name) else {
                continue;
            };
            let after = journal
                .next()
                .ok_or_else(|| SessionError::LastJournalId(path.clone()))?;
            next_id = next_id.max(after);
            let bytes = match fs.read(path) {
                Ok(bytes) => bytes,
                // One salvage set aside is never published again: its name
                // is enough. A journal still to publish must be read.
                Err(_) if is_set_aside(path) => continue,
                Err(e) => return Err(SessionError::Io(e)),
            };
            let read = read_journal(&bytes);
            // A journal whose header can't be read holds nothing salvage
            // can publish, so nothing new can collide with it.
            if let Some(header) = read.header() {
                let held = earlier.entry(header.track()).or_default();
                held.raise_epoch(
                    header
                        .anchor()
                        .map_or(MarkedEpoch::Untimed(header.epoch()), MarkedEpoch::Timed),
                );
                if let Some(range) = read.range() {
                    held.raise_end(range.end());
                }
                // Frames past damage that salvage kept: unreadable now,
                // but still that track's samples.
                for range in frames_after(&bytes, read.valid_len(), header.track()) {
                    held.raise_end(range.end());
                }
            }
        }
        Ok(Self {
            session: id,
            fs,
            dir,
            rate,
            length,
            clock,
            syncing: Syncing::Inline,
            next_id: Some(next_id),
            tracks: BTreeMap::new(),
            finished: Vec::new(),
            marks,
            earlier,
            _recording: recording,
        })
    }

    /// Runs the fsyncs of tracks started from now on as `syncing` says
    /// (inline until this is called).
    #[must_use]
    pub const fn with_syncing(mut self, syncing: Syncing) -> Self {
        self.syncing = syncing;
        self
    }

    /// The first sample `track` may start at: after everything it already
    /// holds from earlier recordings of the session, as its directory shows
    /// it: its journals (those set aside too, frames past their damage
    /// included) and its newest published segment. Recording never reads
    /// the store, so a committed row whose file is gone isn't seen; new
    /// audio over it stays in its journals, since nothing is published over
    /// a row that claims nothing (see the `segment` module).
    #[must_use]
    pub fn first_free_sample(&self, track: TrackId) -> SampleIndex {
        self.earlier
            .get(&track)
            .and_then(|e| e.end)
            .unwrap_or(SampleIndex::ZERO)
    }

    /// The highest epoch `track` used in earlier recordings of the session,
    /// if any: it must start in a higher one.
    #[must_use]
    pub fn highest_epoch(&self, track: TrackId) -> Option<EpochId> {
        self.earlier
            .get(&track)
            .and_then(|e| e.epoch)
            .map(MarkedEpoch::id)
    }

    /// The session time `track`'s earlier recordings end at: where the
    /// audio before [`Self::first_free_sample`] ends, timed by the newest
    /// epoch's anchor, or that epoch's start if no audio in it was kept.
    /// `None` if the track has no earlier epoch, or the newest wasn't timed
    /// (marked by an older nota).
    #[must_use]
    pub fn earlier_end(&self, track: TrackId) -> Option<SessionTime> {
        let anchor = self.earlier.get(&track)?.epoch?.anchor()?;
        let end = TrackTimeline::rebuild(track, [anchor])
            .ok()?
            .time_of(self.first_free_sample(track));
        Some(end.map_or(anchor.start, |end| end.max(anchor.start)))
    }

    /// The session time a resumed session's clock starts at: the latest
    /// [`Self::earlier_end`] of any track, or zero for a new session.
    #[must_use]
    pub fn resume_from(&self) -> SessionTime {
        self.earlier
            .keys()
            .filter_map(|&track| self.earlier_end(track))
            .max()
            .unwrap_or(SessionTime::ZERO)
    }

    /// A timeline for `track` to open its first epoch on: one that carries
    /// on from its newest earlier epoch, so the first is numbered above it
    /// and starts no earlier than its audio ends. If that epoch's anchor
    /// wasn't kept, one numbered above it with no earlier epochs; for a
    /// track with none, a new one.
    ///
    /// # Errors
    ///
    /// [`EpochError::TooManyEpochs`] if the newest earlier epoch is the
    /// last [`EpochId`].
    pub fn resumed_timeline(&self, track: TrackId) -> Result<TrackTimeline, EpochError> {
        match self.earlier.get(&track).and_then(|e| e.epoch) {
            Some(MarkedEpoch::Timed(anchor)) => TrackTimeline::rebuild(track, [anchor]),
            Some(MarkedEpoch::Untimed(id)) => TrackTimeline::starting_after(track, id),
            None => Ok(TrackTimeline::new(track)),
        }
    }

    /// `track`'s timeline as [`Self::resumed_timeline`] gives it, with its
    /// first epoch in this writer opened `at`, from
    /// [`Self::first_free_sample`], and that epoch: what
    /// [`Self::start_track`] takes.
    ///
    /// # Errors
    ///
    /// As [`Self::resumed_timeline`], and as
    /// [`TrackTimeline::open_epoch`] refuses the epoch: one starting well
    /// before the track's earlier audio ends.
    pub fn open_first_epoch(
        &self,
        track: TrackId,
        at: SessionTime,
    ) -> Result<(TrackTimeline, Epoch), EpochError> {
        let mut timeline = self.resumed_timeline(track)?;
        timeline.open_epoch(at, self.first_free_sample(track), self.rate)?;
        // Just opened, so there's a current epoch.
        let epoch = timeline
            .current()
            .copied()
            .ok_or(EpochError::TooManyEpochs)?;
        Ok((timeline, epoch))
    }

    /// Starts `track` in `epoch`, from its first sample. With
    /// [`Syncing::Auto`], a second track moves every track's fsyncs to
    /// threads.
    ///
    /// # Errors
    ///
    /// [`SessionError::TrackExists`]; [`SessionError::EpochUsed`] unless
    /// `epoch` is above [`Self::highest_epoch`];
    /// [`SessionError::EpochMisplaced`] unless it's at the writer's rate;
    /// [`SessionError::Covered`] if it starts before
    /// [`Self::first_free_sample`]; [`SessionError::TimeWentBack`] if it
    /// starts before [`Self::earlier_end`]; [`SessionError::Io`] if a sync
    /// thread can't start (the track isn't started, and tracks already
    /// moved to threads stay there).
    pub fn start_track(&mut self, track: TrackId, epoch: &Epoch) -> Result<(), SessionError> {
        if self.tracks.contains_key(&track) {
            return Err(SessionError::TrackExists(track));
        }
        self.check_first_epoch(track, epoch)?;
        let syncing = match self.syncing {
            Syncing::Auto if !self.tracks.is_empty() => {
                // A second track: every track's fsyncs move to threads.
                for (&started, state) in &mut self.tracks {
                    state
                        .syncs
                        .move_to_thread(started)
                        .map_err(SessionError::Io)?;
                }
                Syncing::Threads
            }
            syncing => syncing,
        };
        let syncs = TrackSyncs::new(syncing, track).map_err(SessionError::Io)?;
        self.tracks.insert(
            track,
            Track {
                epoch: *epoch,
                next: epoch.first_sample(),
                journal: None,
                ending: VecDeque::new(),
                waiting: Vec::new(),
                syncs,
            },
        );
        Ok(())
    }

    /// Whether `epoch` may be `track`'s first in this writer: above the
    /// epochs it used, at the writer's rate, and after its earlier audio in
    /// samples and in session time.
    fn check_first_epoch(&self, track: TrackId, epoch: &Epoch) -> Result<(), SessionError> {
        let id = epoch.id();
        if self.highest_epoch(track).is_some_and(|used| id <= used) {
            return Err(SessionError::EpochUsed { track, epoch: id });
        }
        if epoch.rate() != self.rate {
            return Err(SessionError::EpochMisplaced { track, epoch: id });
        }
        let first_free = self.first_free_sample(track);
        if epoch.first_sample() < first_free {
            return Err(SessionError::Covered { track, first_free });
        }
        if let Some(earlier_end) = self.earlier_end(track)
            && epoch.start() < earlier_end
        {
            return Err(SessionError::TimeWentBack { track, earlier_end });
        }
        Ok(())
    }

    /// Where `track`'s fsyncs run now: [`Syncing::Inline`] or
    /// [`Syncing::Threads`], never [`Syncing::Auto`]. `None` if it isn't
    /// started.
    #[must_use]
    pub fn syncing(&self, track: TrackId) -> Option<Syncing> {
        self.tracks.get(&track).map(|state| state.syncs.syncing())
    }

    /// Moves `track` to `epoch` (its stream reopened), which starts at the
    /// track's next sample: its journal ends, and the next sample starts a
    /// new one. Audio still waiting for an fsync is written first, in the
    /// old epoch: a journal full while its fsync runs is ended, and the
    /// rest goes to another journal, so this never waits on an fsync.
    ///
    /// # Errors
    ///
    /// With nothing changed: [`SessionError::UnknownTrack`];
    /// [`SessionError::EpochUsed`] unless `epoch` is above the track's
    /// current one; [`SessionError::EpochMisplaced`] unless it starts at
    /// [`Self::next_sample`], at the writer's rate. Otherwise
    /// [`SessionError::Journal`] if the ending journal's last fsync failed
    /// and its replacement failed too.
    pub fn new_epoch(&mut self, track: TrackId, epoch: &Epoch) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let id = epoch.id();
        if id <= state.epoch.id() {
            return Err(SessionError::EpochUsed { track, epoch: id });
        }
        if epoch.first_sample() != state.next || epoch.rate() != self.rate {
            return Err(SessionError::EpochMisplaced { track, epoch: id });
        }
        let drained = self.write_out(track);
        let ended = self.end_journal(track);
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        state.epoch = *epoch;
        drained.and(ended)
    }

    /// The epoch `track` is recording in, or `None` if the track wasn't
    /// started.
    #[must_use]
    pub fn epoch(&self, track: TrackId) -> Option<Epoch> {
        self.tracks.get(&track).map(|t| t.epoch)
    }

    /// The rate stamped on every journal this writer starts.
    #[must_use]
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The next sample `track` will record, or `None` if it wasn't started.
    #[must_use]
    pub fn next_sample(&self, track: TrackId) -> Option<SampleIndex> {
        self.tracks.get(&track).map(|t| t.next)
    }

    /// How far `track`'s audio is known durable: its oldest ended journal
    /// still waiting for its last fsync, or else its current journal; `None`
    /// between journals. Everything the track recorded before it is
    /// durable, or a gap.
    #[must_use]
    pub fn durable(&self, track: TrackId) -> Option<DurablePosition> {
        let state = self.tracks.get(&track)?;
        state
            .ending
            .front()
            .or(state.journal.as_ref())
            .map(|o| o.writer.durable())
    }

    /// The journals that have ended since the last call, in order: ready
    /// to publish.
    pub fn take_finished(&mut self) -> Vec<FinishedJournal> {
        std::mem::take(&mut self.finished)
    }

    /// Records `samples` on `track`, rotating its journal at window
    /// boundaries. The track always moves on by all of `samples`, even after
    /// an error, so its sample numbers keep matching the stream: samples
    /// that couldn't be journaled are a gap.
    ///
    /// With [`Syncing::Threads`], a journal whose sync budget is used up
    /// while its fsync runs takes no more until the fsync completes: the
    /// rest waits in memory, counted in [`Self::next_sample`] but not yet
    /// written, and is written by a later call once it has. Nothing waits
    /// for the fsync, so no other track's audio does either.
    ///
    /// # Errors
    ///
    /// [`SessionError::UnknownTrack`] or [`SessionError::Overflow`], with
    /// nothing recorded; otherwise the first [`SessionError::Journal`] for a
    /// journal that broke and couldn't be replaced (see the module docs).
    /// Later windows of `samples` still try a new journal.
    pub fn append(&mut self, track: TrackId, samples: &[i16]) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let total = u64::try_from(samples.len()).map_err(|_| SessionError::Overflow)?;
        let end = state
            .next
            .checked_add(SampleCount::new(total))
            .ok_or(SessionError::Overflow)?;
        state.waiting.extend_from_slice(samples);
        state.next = end;
        let collected = self.collect(track);
        let flushed = self.flush(track);
        collected.and(flushed)
    }

    /// Takes every fsync that has completed and starts each one due (see
    /// [`JournalWriter::sync_due`]), for every track; writes audio that
    /// was waiting for an fsync to complete. Call it on a timer, so audio
    /// that stops arriving still gets synced and its completions are seen.
    ///
    /// # Errors
    ///
    /// [`SessionError::Journal`] for the first track whose journal broke
    /// and couldn't be replaced; the other tracks are still synced.
    pub fn sync_if_due(&mut self) -> Result<(), SessionError> {
        let tracks: Vec<TrackId> = self.tracks.keys().copied().collect();
        let mut first_error = None;
        for track in tracks {
            let results = [
                self.collect(track),
                self.sync_journal_if_due(track),
                self.flush(track),
            ];
            if let Some(e) = results.into_iter().find_map(Result::err) {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Ends every track's journal (a last fsync each), waits for every
    /// fsync to complete, and returns every journal finished and not yet
    /// taken.
    ///
    /// # Errors
    ///
    /// The first [`SessionError::Journal`], with every finished journal
    /// still, for publishing; the other tracks are still ended.
    pub fn finish(mut self) -> Result<Vec<FinishedJournal>, FinishError> {
        let tracks: Vec<TrackId> = self.tracks.keys().copied().collect();
        let mut first_error = None;
        for track in tracks {
            let results = [
                self.drain(track),
                self.end_journal(track),
                self.settle_ended(track),
            ];
            if let Some(e) = results.into_iter().find_map(Result::err) {
                first_error.get_or_insert(e);
            }
        }
        match first_error {
            Some(error) => Err(FinishError {
                error,
                finished: self.finished,
            }),
            None => Ok(self.finished),
        }
    }

    /// Writes `track`'s waiting audio to its journals, a window at a time,
    /// starting a journal where there's none and ending each at its
    /// window's end. Stops early, leaving the rest waiting, when the
    /// journal's sync budget is used up while its fsync runs.
    fn flush(&mut self, track: TrackId) -> Result<(), SessionError> {
        let mut first_error = None;
        // Whether results were taken since the last write: if the journal
        // is still full then, the rest waits.
        let mut collected = false;
        loop {
            let length = self.length;
            let state = self
                .tracks
                .get_mut(&track)
                .ok_or(SessionError::UnknownTrack(track))?;
            if state.waiting.is_empty() {
                break;
            }
            let from = state.waiting_from();
            let in_window = length
                .window_end(from)
                .and_then(|end| end.checked_count_since(from))
                .map_or(usize::MAX, |n| {
                    usize::try_from(n.get()).unwrap_or(usize::MAX)
                });
            let Some(open) = state.journal.as_mut() else {
                let epoch = state.epoch.anchor();
                // No journal: start one here, which is replaced if writing
                // to it fails. If none can start, this window's waiting
                // samples are a gap.
                if let Err(e) = self.open_journal(track, from, epoch, Vec::new(), Vec::new()) {
                    first_error.get_or_insert(e);
                    self.drop_waiting(track, in_window);
                }
                continue;
            };
            let room = usize::try_from(open.writer.room()).unwrap_or(usize::MAX);
            if room == 0 {
                if open.writer.sync_in_flight() {
                    if collected {
                        // The rest waits for the fsync to complete.
                        break;
                    }
                    collected = true;
                    if let Err(e) = self.collect(track) {
                        first_error.get_or_insert(e);
                    }
                    continue;
                }
                if let Err(e) = self.sync_journal(track) {
                    first_error.get_or_insert(e);
                }
                continue;
            }
            collected = false;
            let n = state.waiting.len().min(in_window).min(room);
            let written = open.writer.append_within(&state.waiting[..n]);
            // Written or not, they're this journal's now: if it broke, its
            // replacement takes them over from its durable position.
            open.unsynced.extend(state.waiting.drain(..n));
            open.trim();
            if written.is_err()
                && let Err(e) = self.replace(track)
            {
                first_error.get_or_insert(e);
                // The rest of this window was the broken journal's too.
                self.drop_waiting(track, in_window.saturating_sub(n));
            }
            let ended = if n == in_window {
                self.end_journal(track)
            } else {
                self.sync_journal_if_due(track)
            };
            if let Err(e) = ended {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Drops up to `n` of `track`'s waiting samples, oldest first: a gap.
    /// Only while the track has no journal, which goes on from its own last
    /// sample: `start_journal` and `replace` fail only before installing
    /// one.
    fn drop_waiting(&mut self, track: TrackId, n: usize) {
        if let Some(state) = self.tracks.get_mut(&track) {
            let n = n.min(state.waiting.len());
            state.waiting.drain(..n);
        }
    }

    /// Writes all of `track`'s waiting audio without waiting on an fsync:
    /// each journal still full while its fsync runs is ended, and the rest
    /// starts another.
    fn write_out(&mut self, track: TrackId) -> Result<(), SessionError> {
        let mut first_error = None;
        loop {
            if let Err(e) = self.flush(track) {
                first_error.get_or_insert(e);
            }
            let state = self
                .tracks
                .get(&track)
                .ok_or(SessionError::UnknownTrack(track))?;
            if state.waiting.is_empty() {
                break;
            }
            if let Err(e) = self.end_journal(track) {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Writes all of `track`'s waiting audio, waiting for its fsyncs to
    /// complete as needed.
    fn drain(&mut self, track: TrackId) -> Result<(), SessionError> {
        let mut first_error = None;
        loop {
            if let Err(e) = self.flush(track) {
                first_error.get_or_insert(e);
            }
            let state = self
                .tracks
                .get_mut(&track)
                .ok_or(SessionError::UnknownTrack(track))?;
            if state.waiting.is_empty() {
                break;
            }
            let Some(done) = state.syncs.next() else {
                // Waiting on no fsync: nothing more can be written.
                break;
            };
            if let Err(e) = self.complete(track, done) {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Waits until each of `track`'s ended journals has had its last fsync
    /// complete, and hands them out.
    fn settle_ended(&mut self, track: TrackId) -> Result<(), SessionError> {
        let mut first_error = None;
        loop {
            self.hand_out_settled(track);
            let state = self
                .tracks
                .get_mut(&track)
                .ok_or(SessionError::UnknownTrack(track))?;
            if state.ending.is_empty() {
                break;
            }
            // Every ended journal not yet settled has a sync outstanding:
            // its last, or one in flight when it ended.
            let Some(done) = state.syncs.next() else {
                break;
            };
            if let Err(e) = self.complete(track, done) {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Takes every fsync of `track`'s journals that has completed, without
    /// waiting.
    fn collect(&mut self, track: TrackId) -> Result<(), SessionError> {
        let mut first_error = None;
        loop {
            let state = self
                .tracks
                .get_mut(&track)
                .ok_or(SessionError::UnknownTrack(track))?;
            let Some(done) = state.syncs.try_next() else {
                break;
            };
            if let Err(e) = self.complete(track, done) {
                first_error.get_or_insert(e);
            }
        }
        self.hand_out_settled(track);
        first_error.map_or(Ok(()), Err)
    }

    /// Takes one completed fsync of one of `track`'s journals: its durable
    /// position moves on, or, if it failed, the journal is replaced. One for
    /// a journal already replaced is ignored.
    fn complete(&mut self, track: TrackId, done: SyncDone) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let id = done.journal();
        if let Some(open) = state
            .journal
            .as_mut()
            .filter(|o| o.writer.header().id() == id)
        {
            let synced = open.writer.complete_sync(done);
            open.trim();
            return match synced {
                Ok(()) => {
                    open.fresh = false;
                    Ok(())
                }
                Err(_) => self.replace(track),
            };
        }
        let Some(at) = state
            .ending
            .iter()
            .position(|o| o.writer.header().id() == id)
        else {
            return Ok(());
        };
        let Some(open) = state.ending.get_mut(at) else {
            return Ok(());
        };
        let synced = open.writer.complete_sync(done);
        open.trim();
        match synced {
            Ok(()) => {
                open.fresh = false;
                Ok(())
            }
            Err(_) => self.replace_ending(track, at),
        }
    }

    /// Hands out `track`'s oldest ended journals while their last fsync
    /// has completed, in order.
    fn hand_out_settled(&mut self, track: TrackId) {
        let Some(state) = self.tracks.get_mut(&track) else {
            return;
        };
        while state.ending.front().is_some_and(|o| o.writer.is_settled()) {
            if let Some(mut open) = state.ending.pop_front() {
                let id = open.writer.header().id();
                hand_out(self.session, &mut open.held, &mut self.finished, Some(id));
            }
        }
    }

    /// Starts an fsync of `track`'s journal if one is due.
    fn sync_journal_if_due(&mut self, track: TrackId) -> Result<(), SessionError> {
        let due = self
            .tracks
            .get(&track)
            .and_then(|t| t.journal.as_ref())
            .is_some_and(|o| o.writer.sync_due());
        if due {
            self.sync_journal(track)
        } else {
            Ok(())
        }
    }

    /// Starts an fsync of `track`'s journal now, and takes what has
    /// completed (inline, it already has).
    fn sync_journal(&mut self, track: TrackId) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let Some(open) = state.journal.as_mut() else {
            return Ok(());
        };
        match open.writer.begin_sync() {
            Ok(job) => state.syncs.start(job),
            Err(_) => return self.replace(track),
        }
        self.collect(track)
    }

    /// Starts a journal for `track` at `at` in `epoch`, writes `replay` to
    /// it (the samples from `at` on: a broken journal's unsynced ones,
    /// within one sync budget), and returns it. If any of that fails, the
    /// samples are a gap, and the journals in `held`, with this one if it
    /// was made, are handed out.
    ///
    /// A start that fails for want of space (see
    /// [`SessionError::is_disk_full`]) is tried once more, with the next
    /// id: the failure has freed the ballast, if the recording keeps one
    /// (see [`disk`](crate::disk)), so the second try finds room. A journal
    /// the first try made and couldn't fill is handed out with the rest.
    fn start_journal(
        &mut self,
        track: TrackId,
        at: SampleIndex,
        epoch: EpochAnchor,
        replay: Vec<i16>,
        mut held: Vec<FinishedJournal>,
    ) -> Result<Open<S::File>, SessionError> {
        let mut tried = false;
        let writer = loop {
            match self.try_start(track, at, epoch, &replay, &mut held) {
                Ok(writer) => break writer,
                Err(e) if !tried && e.is_disk_full() => tried = true,
                Err(e) => {
                    // Give up on these samples.
                    hand_out(self.session, &mut held, &mut self.finished, None);
                    return Err(e);
                }
            }
        };
        let fresh = !held.is_empty();
        let mut open = Open {
            writer,
            epoch,
            unsynced: replay,
            unsynced_from: at,
            held,
            fresh,
        };
        open.trim();
        Ok(open)
    }

    /// One try at [`Self::start_journal`]: reserves the next id, creates the
    /// journal and writes `replay` to it. A journal made but not filled is
    /// added to `held`, to be handed out.
    fn try_start(
        &mut self,
        track: TrackId,
        at: SampleIndex,
        epoch: EpochAnchor,
        replay: &[i16],
        held: &mut Vec<FinishedJournal>,
    ) -> Result<JournalWriter<S::File>, SessionError> {
        let id = self.next_id.ok_or(SessionError::Overflow)?;
        self.mark(id, track, epoch)?;
        self.next_id = id.next();
        let header = JournalHeader::new(id, track, epoch);
        let mut writer =
            JournalWriter::create(&self.fs, &self.dir, header, at, Arc::clone(&self.clock))
                .map_err(SessionError::Journal)?;
        // Within one sync budget, so it all fits; its fsync is started
        // like any other.
        let written = writer.append_within(replay);
        if !matches!(written, Ok(n) if n == replay.len()) {
            held.push(FinishedJournal::new(self.session, id));
            return Err(SessionError::Journal(
                written.err().unwrap_or(JournalError::Broken),
            ));
        }
        Ok(writer)
    }

    /// Starts `track`'s journal at `at` (see [`Self::start_journal`]), as
    /// the one being written.
    fn open_journal(
        &mut self,
        track: TrackId,
        at: SampleIndex,
        epoch: EpochAnchor,
        replay: Vec<i16>,
        held: Vec<FinishedJournal>,
    ) -> Result<(), SessionError> {
        let mut open = self.start_journal(track, at, epoch, replay, held)?;
        if let Some(state) = self.tracks.get_mut(&track) {
            // A full replay's fsync starts now; its result, like any other,
            // is taken by a later collect, so nothing another journal did
            // can come back as this one's failure.
            if open.writer.sync_due()
                && let Ok(job) = open.writer.begin_sync()
            {
                state.syncs.start(job);
            }
            state.journal = Some(open);
        }
        Ok(())
    }

    /// Makes sure the marks on disk cover journal `id` and `track`'s
    /// `epoch`, with its anchor, before a journal uses them, reserving a
    /// block of ids at a time.
    fn mark(
        &mut self,
        id: JournalId,
        track: TrackId,
        epoch: EpochAnchor,
    ) -> Result<(), SessionError> {
        let mut wanted = self.marks.clone();
        if id >= wanted.journals_below {
            // The last id is never used, so every used id is below the mark.
            let after = id.next().ok_or(SessionError::Overflow)?;
            let block = JournalId::new(id.get().saturating_add(ID_BLOCK));
            wanted.journals_below = after.max(block);
        }
        let used = wanted
            .epochs
            .entry(track)
            .or_insert(MarkedEpoch::Timed(epoch));
        if epoch.id > used.id() {
            *used = MarkedEpoch::Timed(epoch);
        }
        if wanted != self.marks {
            wanted
                .write(&self.fs, &self.dir)
                .map_err(SessionError::Marks)?;
            self.marks = wanted;
        }
        Ok(())
    }

    /// Replaces `track`'s broken journal with a new one starting at its
    /// durable position, rewriting the samples since. If that fails, or the
    /// broken journal was itself a replacement that never synced, they are
    /// a gap, and the next samples start a new journal.
    fn replace(&mut self, track: TrackId) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let Some(broken) = state.journal.take() else {
            return Ok(());
        };
        let fresh = broken.fresh;
        let (from, epoch, unsynced, mut held) = broken.retire(self.session);
        if fresh {
            hand_out(self.session, &mut held, &mut self.finished, None);
            return Err(SessionError::Journal(JournalError::Broken));
        }
        self.open_journal(track, from, epoch, unsynced, held)
    }

    /// Replaces the broken journal at `at` among `track`'s ended ones with a
    /// new one starting at its durable position, rewriting the samples
    /// since, and ends that one too, in its place. If that fails, or the
    /// broken journal was itself a replacement that never synced, they are
    /// a gap.
    fn replace_ending(&mut self, track: TrackId, at: usize) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let Some(broken) = state.ending.remove(at) else {
            return Ok(());
        };
        let fresh = broken.fresh;
        let (from, epoch, unsynced, mut held) = broken.retire(self.session);
        if fresh {
            hand_out(self.session, &mut held, &mut self.finished, None);
            return Err(SessionError::Journal(JournalError::Broken));
        }
        let mut open = self.start_journal(track, from, epoch, unsynced, held)?;
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        if open.writer.needs_sync() {
            // Just written, so it isn't broken.
            if let Ok(job) = open.writer.begin_sync() {
                state.syncs.start(job);
            }
        }
        state.ending.insert(at.min(state.ending.len()), open);
        self.collect(track)
    }

    /// Ends `track`'s journal: its last fsync is started, and it's handed
    /// out once that completes. If that fsync fails, a replacement takes
    /// the unsynced samples and is ended in turn.
    fn end_journal(&mut self, track: TrackId) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let Some(mut open) = state.journal.take() else {
            return Ok(());
        };
        // A journal being written is never broken (a break replaces it at
        // once), so its last sync can start.
        if open.writer.needs_sync()
            && let Ok(job) = open.writer.begin_sync()
        {
            state.syncs.start(job);
        }
        state.ending.push_back(open);
        self.collect(track)
    }
}

#[cfg(test)]
impl<S: Fs> SessionWriter<S> {
    /// Runs up to `n` of `track`'s held fsyncs, with [`Syncing::Manual`],
    /// oldest first; returns how many ran. Their results are taken by the
    /// writer's next call.
    pub(crate) fn run_syncs(&mut self, track: TrackId, n: usize) -> usize {
        let Some(state) = self.tracks.get_mut(&track) else {
            return 0;
        };
        (0..n).take_while(|_| state.syncs.run_one()).count()
    }

    /// How many of `track`'s samples wait in memory for an fsync.
    pub(crate) fn waiting(&self, track: TrackId) -> usize {
        self.tracks.get(&track).map_or(0, |t| t.waiting.len())
    }

    /// Epoch `id` of `track` from sample `at`, at the writer's rate,
    /// starting where the track's earlier audio ends (zero if it has
    /// none): one [`Self::start_track`] takes, or, with `at` the track's
    /// next sample, [`Self::new_epoch`].
    pub(crate) fn test_epoch(&self, track: TrackId, id: EpochId, at: SampleIndex) -> Epoch {
        let anchor = EpochAnchor {
            id,
            start: self.earlier_end(track).unwrap_or(SessionTime::ZERO),
            first_sample: at,
            rate: self.rate,
        };
        let timeline = TrackTimeline::rebuild(track, [anchor]).unwrap();
        *timeline.current().unwrap()
    }
}

/// A journal still to hand out: being written, or ended and waiting for
/// its last fsync.
#[derive(Debug)]
struct Open<F: FsFile> {
    writer: JournalWriter<F>,
    /// The epoch its header names: a replacement is in the same one.
    epoch: EpochAnchor,
    /// The samples from its durable position on that were given to it:
    /// what a replacement must write again if it breaks. After a failed
    /// write they run past its captured position.
    unsynced: Vec<i16>,
    /// The first of them.
    unsynced_from: SampleIndex,
    /// Broken journals it replaces: handed out with it, so publishing sees
    /// both and the overlap rule holds.
    held: Vec<FinishedJournal>,
    /// A replacement none of whose fsyncs has succeeded yet: if it breaks
    /// too, its samples are given up as a gap rather than replaced again.
    fresh: bool,
}

impl<F: FsFile> Open<F> {
    /// Drops the samples before the durable position from `unsynced`.
    fn trim(&mut self) {
        let durable = self.writer.durable().end();
        let synced = durable
            .checked_count_since(self.unsynced_from)
            .map_or(0, |n| usize::try_from(n.get()).unwrap_or(usize::MAX));
        self.unsynced.drain(..synced.min(self.unsynced.len()));
        self.unsynced_from = self.unsynced_from.max(durable);
    }

    /// Gives up a broken journal: where its replacement starts, in which
    /// epoch, the samples it must write again, and the broken journals it
    /// must be handed out with, this one last.
    fn retire(
        self,
        session: SessionId,
    ) -> (SampleIndex, EpochAnchor, Vec<i16>, Vec<FinishedJournal>) {
        let header = self.writer.header();
        let mut held = self.held;
        held.push(FinishedJournal::new(session, header.id()));
        (self.unsynced_from, self.epoch, self.unsynced, held)
    }
}

/// Moves a track's held broken journals, then the journal that just
/// `ended`, to the finished list.
fn hand_out(
    session: SessionId,
    held: &mut Vec<FinishedJournal>,
    finished: &mut Vec<FinishedJournal>,
    ended: Option<JournalId>,
) {
    finished.append(held);
    finished.extend(ended.map(|id| FinishedJournal::new(session, id)));
}

/// The id in a journal's file name, or in a journal set aside by salvage
/// (`journal-000042.unreadable`): new ids must not reuse either.
fn journal_id_in_name(name: &std::ffi::OsStr) -> Option<JournalId> {
    let name = name.to_str()?;
    let name = name.strip_suffix(SET_ASIDE).unwrap_or(name);
    JournalId::from_file_name(std::ffi::OsStr::new(name))
}

/// The suffix salvage gives a journal it sets aside.
pub(crate) const SET_ASIDE: &str = ".unreadable";

/// Whether `path` is a journal salvage set aside.
fn is_set_aside(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(SET_ASIDE))
}

impl Earlier {
    /// Takes `end` as the track's end if it's past the one known.
    fn raise_end(&mut self, end: SampleIndex) {
        self.end = self.end.max(Some(end));
    }

    /// Takes `epoch` as the track's highest if it's above the one known,
    /// or the same one with its anchor where that was missing.
    fn raise_epoch(&mut self, epoch: MarkedEpoch) {
        let higher = match self.epoch {
            None => true,
            Some(known) => {
                epoch.id() > known.id() || (epoch.id() == known.id() && known.anchor().is_none())
            }
        };
        if higher {
            self.epoch = Some(epoch);
        }
    }
}

#[cfg(test)]
mod tests;
