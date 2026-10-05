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

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use nota_core::{Clock, EpochId, SampleCount, SampleIndex, SampleRate, SessionId, TrackId};

use crate::fs::Fs;
use crate::journal::format::frames_after;
use crate::journal::{
    DurablePosition, JournalError, JournalHeader, JournalId, JournalWriter, read_journal,
};
use crate::segment::SegmentLength;

mod handle;
mod marks;

use handle::InUse;
pub use handle::{SessionDir, SessionLock, SessionStore, Use};
use marks::Marks;
pub(crate) use marks::is_temp as is_marks_temp;
pub use marks::{BadMarks, FILE_NAME as MARKS_FILE_NAME};

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
    /// A track started before the end of what it already holds.
    Covered {
        /// The track.
        track: TrackId,
        /// The first sample it may start at.
        first_free: SampleIndex,
    },
    /// The session ran out of journal ids or a track out of sample numbers.
    Overflow,
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
            Self::Covered { track, first_free } => write!(
                f,
                "track {} already holds audio before sample {}",
                track.get(),
                first_free.get()
            ),
            Self::Overflow => f.write_str("ran out of journal ids or sample numbers"),
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
struct Track<F> {
    epoch: EpochId,
    /// The next sample to record.
    next: SampleIndex,
    journal: Option<JournalWriter<F>>,
    /// The samples from the journal's durable position up to `next`.
    unsynced: Vec<i16>,
    /// Broken journals whose replacement is still being written: handed
    /// out with it, so publishing sees both and the overlap rule holds.
    held: Vec<FinishedJournal>,
}

/// Records a session's tracks into rotating journals in one directory.
///
/// Like [`JournalWriter`], it may fsync on any call, so it belongs on its
/// own thread. Call [`Self::sync_if_due`] on a timer.
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
    /// The highest epoch journaled.
    epoch: Option<EpochId>,
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
    /// [`SessionError::Overflow`] if the journal ids have run out.
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
            earlier.entry(track).or_default().epoch = Some(epoch);
        }
        for (track, end) in crate::segment::published_ends(&fs, &dir, &paths, length) {
            earlier.entry(track).or_default().raise_end(end);
        }
        let mut next_id = marks.journals_below;
        for path in &paths {
            let Some(journal) = path.file_name().and_then(journal_id_in_name) else {
                continue;
            };
            next_id = next_id.max(journal.next().ok_or(SessionError::Overflow)?);
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
                held.epoch = held.epoch.max(Some(header.epoch()));
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
            next_id: Some(next_id),
            tracks: BTreeMap::new(),
            finished: Vec::new(),
            marks,
            earlier,
            _recording: recording,
        })
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
        self.earlier.get(&track).and_then(|e| e.epoch)
    }

    /// Starts `track` in `epoch`; its first sample will be `at`.
    ///
    /// # Errors
    ///
    /// [`SessionError::TrackExists`]; [`SessionError::EpochUsed`] unless
    /// `epoch` is above [`Self::highest_epoch`]; [`SessionError::Covered`]
    /// if `at` is before [`Self::first_free_sample`].
    pub fn start_track(
        &mut self,
        track: TrackId,
        epoch: EpochId,
        at: SampleIndex,
    ) -> Result<(), SessionError> {
        if self.tracks.contains_key(&track) {
            return Err(SessionError::TrackExists(track));
        }
        if self.highest_epoch(track).is_some_and(|used| epoch <= used) {
            return Err(SessionError::EpochUsed { track, epoch });
        }
        let first_free = self.first_free_sample(track);
        if at < first_free {
            return Err(SessionError::Covered { track, first_free });
        }
        self.tracks.insert(
            track,
            Track {
                epoch,
                next: at,
                journal: None,
                unsynced: Vec::new(),
                held: Vec::new(),
            },
        );
        Ok(())
    }

    /// Moves `track` to `epoch` (its stream reopened): its journal ends, and
    /// the next sample starts a new one.
    ///
    /// # Errors
    ///
    /// [`SessionError::UnknownTrack`]; [`SessionError::EpochUsed`], with
    /// nothing changed, unless `epoch` is above the track's current one;
    /// [`SessionError::Journal`] if the ending journal's last fsync failed
    /// and its replacement failed too.
    pub fn new_epoch(&mut self, track: TrackId, epoch: EpochId) -> Result<(), SessionError> {
        let current = self
            .tracks
            .get(&track)
            .ok_or(SessionError::UnknownTrack(track))?
            .epoch;
        if epoch <= current {
            return Err(SessionError::EpochUsed { track, epoch });
        }
        let ended = self.end_journal(track);
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        state.epoch = epoch;
        ended
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

    /// How far `track`'s current journal is durable, or `None` between
    /// journals.
    #[must_use]
    pub fn durable(&self, track: TrackId) -> Option<DurablePosition> {
        self.tracks
            .get(&track)
            .and_then(|t| t.journal.as_ref())
            .map(JournalWriter::durable)
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
    /// # Errors
    ///
    /// [`SessionError::UnknownTrack`] or [`SessionError::Overflow`], with
    /// nothing recorded; otherwise the first [`SessionError::Journal`] for a
    /// journal that broke and couldn't be replaced (see the module docs).
    /// Later chunks of `samples` still try a new journal.
    pub fn append(&mut self, track: TrackId, samples: &[i16]) -> Result<(), SessionError> {
        let next = self
            .tracks
            .get(&track)
            .ok_or(SessionError::UnknownTrack(track))?
            .next;
        let total = u64::try_from(samples.len()).map_err(|_| SessionError::Overflow)?;
        next.checked_add(SampleCount::new(total))
            .ok_or(SessionError::Overflow)?;
        let mut rest = samples;
        let mut first_error = None;
        while !rest.is_empty() {
            let next = self.tracks.get(&track).map_or(next, |t| t.next);
            let room = self
                .length
                .window_end(next)
                .and_then(|end| end.checked_count_since(next))
                .map_or(usize::MAX, |n| {
                    usize::try_from(n.get()).unwrap_or(usize::MAX)
                });
            let (chunk, tail) = rest.split_at(rest.len().min(room));
            if let Err(e) = self.write(track, chunk) {
                first_error.get_or_insert(e);
            }
            rest = tail;
            if chunk.len() == room
                && let Err(e) = self.end_journal(track)
            {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Fsyncs every journal that's due (see [`JournalWriter::sync_if_due`]).
    ///
    /// # Errors
    ///
    /// [`SessionError::Journal`] for the first track whose journal broke
    /// and couldn't be replaced; the other tracks are still synced.
    pub fn sync_if_due(&mut self) -> Result<(), SessionError> {
        let tracks: Vec<TrackId> = self.tracks.keys().copied().collect();
        let mut first_error = None;
        for track in tracks {
            let Some(state) = self.tracks.get_mut(&track) else {
                continue;
            };
            let Some(journal) = state.journal.as_mut() else {
                continue;
            };
            let synced = journal.sync_if_due();
            let durable = journal.durable();
            trim_unsynced(state, durable);
            if synced.is_err()
                && let Err(e) = self.replace(track)
            {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Ends every track's journal (a last fsync each), and returns every
    /// journal finished and not yet taken.
    ///
    /// # Errors
    ///
    /// The first [`SessionError::Journal`], with every finished journal
    /// still, for publishing; the other tracks are still ended.
    pub fn finish(mut self) -> Result<Vec<FinishedJournal>, FinishError> {
        let tracks: Vec<TrackId> = self.tracks.keys().copied().collect();
        let mut first_error = None;
        for track in tracks {
            if let Err(e) = self.end_journal(track) {
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

    /// Writes `chunk`, which stays within the current window, to the
    /// track's journal, starting one if needed and replacing it if it
    /// breaks.
    fn write(&mut self, track: TrackId, chunk: &[i16]) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let len = SampleCount::new(chunk.len() as u64);
        let end = state.next.checked_add(len).ok_or(SessionError::Overflow)?;
        let written = match state.journal.as_mut() {
            Some(journal) => {
                let result = journal.append(chunk);
                state.unsynced.extend_from_slice(chunk);
                state.next = end;
                let durable = journal.durable();
                trim_unsynced(state, durable);
                result.is_ok()
            }
            None => false,
        };
        if written {
            return Ok(());
        }
        if state.journal.is_none() {
            // No journal yet: start one at `next`, with nothing to replay.
            state.unsynced.clear();
            state.unsynced.extend_from_slice(chunk);
            let at = state.next;
            state.next = end;
            return self.start_journal(track, at, true);
        }
        self.replace(track)
    }

    /// Starts a journal for `track` at `at` and writes the track's
    /// `unsynced` samples to it, which run from `at` to `next`. If writing
    /// them fails and `may_replace`, one replacement takes them over from the
    /// new journal's durable position; otherwise they're dropped: a gap.
    fn start_journal(
        &mut self,
        track: TrackId,
        at: SampleIndex,
        may_replace: bool,
    ) -> Result<(), SessionError> {
        let id = self.next_id.ok_or(SessionError::Overflow)?;
        let epoch = self
            .tracks
            .get(&track)
            .ok_or(SessionError::UnknownTrack(track))?
            .epoch;
        let marked = self.mark(id, track, epoch);
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        if let Err(e) = marked {
            state.unsynced.clear();
            hand_out(self.session, &mut state.held, &mut self.finished, None);
            return Err(e);
        }
        self.next_id = id.next();
        let header = JournalHeader::new(id, track, state.epoch, self.rate);
        let created =
            JournalWriter::create(&self.fs, &self.dir, header, at, Arc::clone(&self.clock));
        let mut journal = match created {
            Ok(journal) => journal,
            Err(e) => {
                state.unsynced.clear();
                hand_out(self.session, &mut state.held, &mut self.finished, None);
                return Err(SessionError::Journal(e));
            }
        };
        let appended = journal.append(&state.unsynced);
        let durable = journal.durable();
        state.journal = Some(journal);
        trim_unsynced(state, durable);
        if let Err(e) = appended {
            if may_replace {
                return self.replace(track);
            }
            // A replacement broke too: give up on these samples.
            let broken = state.journal.take().map(|j| j.header().id());
            hand_out(self.session, &mut state.held, &mut self.finished, broken);
            state.unsynced.clear();
            return Err(SessionError::Journal(e));
        }
        Ok(())
    }

    /// Makes sure the marks on disk cover journal `id` and `track`'s
    /// `epoch` before a journal uses them, reserving a block of ids at a
    /// time.
    fn mark(&mut self, id: JournalId, track: TrackId, epoch: EpochId) -> Result<(), SessionError> {
        let mut wanted = self.marks.clone();
        if id >= wanted.journals_below {
            // The last id is never used, so every used id is below the mark.
            let after = id.next().ok_or(SessionError::Overflow)?;
            let block = JournalId::new(id.get().saturating_add(ID_BLOCK));
            wanted.journals_below = after.max(block);
        }
        let used = wanted.epochs.entry(track).or_insert(epoch);
        *used = (*used).max(epoch);
        if wanted != self.marks {
            wanted
                .write(&self.fs, &self.dir)
                .map_err(SessionError::Marks)?;
            self.marks = wanted;
        }
        Ok(())
    }

    /// Replaces `track`'s broken journal with a new one starting at its
    /// durable position, rewriting the samples since.
    fn replace(&mut self, track: TrackId) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let Some(broken) = state.journal.take() else {
            return Ok(());
        };
        let from = broken.durable().end();
        state
            .held
            .push(FinishedJournal::new(self.session, broken.header().id()));
        self.start_journal(track, from, false)
    }

    /// Ends `track`'s journal with a last fsync. If that fails, a
    /// replacement takes the unsynced samples and is ended in turn.
    fn end_journal(&mut self, track: TrackId) -> Result<(), SessionError> {
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let Some(mut journal) = state.journal.take() else {
            return Ok(());
        };
        let synced = journal.sync();
        let durable = journal.durable();
        trim_unsynced(state, durable);
        if synced.is_ok() {
            let id = journal.header().id();
            hand_out(self.session, &mut state.held, &mut self.finished, Some(id));
            state.unsynced.clear();
            return Ok(());
        }
        state.journal = Some(journal);
        self.replace(track)?;
        // The replacement holds the unsynced samples; end it too.
        let state = self
            .tracks
            .get_mut(&track)
            .ok_or(SessionError::UnknownTrack(track))?;
        let Some(mut journal) = state.journal.take() else {
            return Ok(());
        };
        let synced = journal.sync();
        let id = journal.header().id();
        hand_out(self.session, &mut state.held, &mut self.finished, Some(id));
        state.unsynced.clear();
        synced.map_err(SessionError::Journal)
    }
}

/// Moves a track's held broken journals, then the journal that just
/// `ended`, to the finished list: the track has no journal in progress.
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
const SET_ASIDE: &str = ".unreadable";

/// Whether `path` is a journal salvage set aside.
fn is_set_aside(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(SET_ASIDE))
}

impl Earlier {
    fn raise_end(&mut self, end: SampleIndex) {
        self.end = self.end.max(Some(end));
    }
}

/// Drops the samples before the journal's durable position from the
/// track's `unsynced` buffer, which ends at `next`.
fn trim_unsynced<F>(state: &mut Track<F>, durable: DurablePosition) {
    let behind = state
        .next
        .checked_count_since(durable.end())
        .map_or(0, |n| usize::try_from(n.get()).unwrap_or(usize::MAX));
    let drop = state.unsynced.len().saturating_sub(behind);
    state.unsynced.drain(..drop);
}

#[cfg(test)]
mod tests;
