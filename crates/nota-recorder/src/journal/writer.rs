//! Writing the journal: frames appended as audio arrives, fsync'd every
//! [`SYNC_INTERVAL`].

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nota_core::{Clock, SampleCount, SampleIndex, SampleRate, SessionTime, TrackId};

use super::JournalId;
use super::format::{JournalHeader, MAX_FRAME_SAMPLES, encode_frame, encode_header};
use crate::fs::{FileSyncer, Fs, FsFile, Synced};

/// The next [`JournalWriter`]'s token.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0);

/// How often the journal is fsync'd while audio arrives: at most this long,
/// or this much audio, goes unsynced.
///
/// Durable may trail the audio the stream has delivered by this, plus the
/// stream's buffering (about 40 ms), plus however long the fsync takes.
/// 850 ms keeps that within the 1.1 s bounded-loss rule for fsyncs up to
/// about 200 ms; at a full second, one ~105 ms fsync was enough to break it.
pub const SYNC_INTERVAL: Duration = Duration::from_millis(850);

/// The most audio a journal at `rate` may hold unsynced, in samples:
/// [`SYNC_INTERVAL`]'s worth, and at least one, so even a very low rate
/// makes progress (a zero budget would sync forever without writing).
/// Salvage judges torn tails by it too.
pub(crate) fn sync_budget(rate: SampleRate) -> NonZeroU64 {
    let samples = SampleCount::started_within(SYNC_INTERVAL, rate).map_or(0, SampleCount::get);
    NonZeroU64::new(samples).unwrap_or(NonZeroU64::MIN)
}

/// How far one journal has been fsync'd: every sample it holds for its
/// track, up to `end`, is on disk. Only a completed fsync of that journal
/// makes one.
///
/// It names its journal, and equality includes it: a broken journal and its
/// replacement carry on the same track's sample numbers, so without the id
/// their positions would compare equal. There's deliberately no ordering;
/// compare `end`s only after checking the journals match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurablePosition {
    journal: JournalId,
    track: TrackId,
    end: SampleIndex,
}

impl DurablePosition {
    /// The position `end` of `header`'s journal, made durable by the fsync
    /// `_proof` stands for. Private: [`JournalWriter`] is the only caller.
    const fn after(_proof: &Synced, header: JournalHeader, end: SampleIndex) -> Self {
        Self {
            journal: header.id(),
            track: header.track(),
            end,
        }
    }

    /// The journal it covers.
    #[must_use]
    pub const fn journal(self) -> JournalId {
        self.journal
    }

    /// The track.
    #[must_use]
    pub const fn track(self) -> TrackId {
        self.track
    }

    /// The first sample not yet known to be on disk.
    #[must_use]
    pub const fn end(self) -> SampleIndex {
        self.end
    }
}

/// Why the journal couldn't be written.
#[derive(Debug)]
pub enum JournalError {
    /// A filesystem operation failed. The journal is now broken.
    Io(io::Error),
    /// The track's sample index would overflow.
    SampleOverflow,
    /// An earlier write or fsync failed. What's in the file up to the last
    /// durable position is safe; nothing more can be added. After a failed
    /// fsync the kernel may have dropped the unsynced data, so retrying
    /// would only hide the loss. Start a new journal.
    Broken,
    /// A sync [`JournalWriter::begin_sync`] started hasn't completed, so
    /// one run here couldn't say what's durable. Complete it first.
    SyncPending,
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "journal write failed: {e}"),
            Self::SampleOverflow => f.write_str("the track ran out of sample numbers"),
            Self::Broken => f.write_str("the journal broke after an earlier failure"),
            Self::SyncPending => f.write_str("an earlier sync of the journal hasn't completed"),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Appends one track's audio to a journal file and fsyncs it every
/// [`SYNC_INTERVAL`].
///
/// It keeps two positions: **captured**, the end of what was written, and
/// **durable**, the end of what an fsync has confirmed. Never more than
/// [`SYNC_INTERVAL`]'s worth of audio is captured and not durable (its
/// sync budget, which salvage relies on).
///
/// It can fsync in two ways:
/// - **Inline:** each [`Self::append`] syncs when [`SYNC_INTERVAL`] has
///   passed since the last sync or that much audio is unsynced, so durable
///   stays within [`SYNC_INTERVAL`] of captured. Call [`Self::sync_if_due`]
///   on a timer too, so audio that stops arriving still gets synced. An
///   fsync can take a second or more on a busy disk, so the writer then
///   belongs on its own thread, fed by a channel, never on the audio
///   callback.
/// - **On another thread:** [`Self::append_within`] writes only what fits
///   in the budget and never syncs. When [`Self::sync_due`],
///   [`Self::begin_sync`] starts one: its [`PendingSync`] runs anywhere,
///   while appends go on, and its [`SyncDone`] comes back to
///   [`Self::complete_sync`]. The durable position it proves is where the
///   journal stood when the fsync was started, never later.
///
/// After any failed write or fsync the writer is broken: every later call
/// returns [`JournalError::Broken`].
#[derive(Debug)]
pub struct JournalWriter<F: FsFile> {
    file: F,
    syncer: Arc<F::Syncer>,
    path: PathBuf,
    clock: Arc<dyn Clock>,
    header: JournalHeader,
    next_seq: u64,
    start: SampleIndex,
    captured: SampleIndex,
    durable: DurablePosition,
    /// When the last sync was started.
    last_sync: SessionTime,
    /// Where the last sync started covers to: anything captured past it
    /// still needs one.
    requested: SampleIndex,
    /// The numbers of the syncs started and not yet completed, oldest
    /// first.
    in_flight: VecDeque<u64>,
    /// Durable positions proved by syncs that completed while an older
    /// one hadn't: they count only once every older one has succeeded.
    early: Vec<(u64, DurablePosition)>,
    /// The number the next sync started gets.
    next_sync: u64,
    /// Unique to this writer in the process, so a result from another
    /// writer's sync, even of a journal with the same id in another
    /// session, is never taken for one of its own.
    token: u64,
    broken: bool,
    buf: Vec<u8>,
}

/// An fsync of a journal, started by [`JournalWriter::begin_sync`] and not
/// yet run. Run it on any thread, while the journal is still appended to,
/// and hand what it returns to [`JournalWriter::complete_sync`].
#[derive(Debug)]
pub struct PendingSync<Y> {
    syncer: Arc<Y>,
    header: JournalHeader,
    /// Captured when it started: everything the fsync will cover.
    end: SampleIndex,
    seq: u64,
    token: u64,
}

impl<Y: FileSyncer> PendingSync<Y> {
    /// The journal it syncs.
    #[must_use]
    pub const fn journal(&self) -> JournalId {
        self.header.id()
    }

    /// Fsyncs the journal: a durable position up to where it stood when
    /// the sync was started, or why it failed.
    #[must_use]
    pub fn run(self) -> SyncDone {
        let result = self
            .syncer
            .sync()
            .map(|proof| DurablePosition::after(&proof, self.header, self.end));
        SyncDone {
            journal: self.header.id(),
            seq: self.seq,
            token: self.token,
            result,
        }
    }

    /// What to complete it with if it never runs (its thread has gone):
    /// a failure, which breaks the journal. Made before it's handed over,
    /// since running it consumes it.
    #[must_use]
    pub fn lost(&self) -> SyncDone {
        SyncDone {
            journal: self.header.id(),
            seq: self.seq,
            token: self.token,
            result: Err(io::Error::other("the journal's sync thread stopped")),
        }
    }
}

/// What a [`PendingSync`] did: for [`JournalWriter::complete_sync`].
#[derive(Debug)]
pub struct SyncDone {
    journal: JournalId,
    seq: u64,
    token: u64,
    result: Result<DurablePosition, io::Error>,
}

impl SyncDone {
    /// The journal it synced.
    #[must_use]
    pub const fn journal(&self) -> JournalId {
        self.journal
    }
}

impl<F: FsFile> JournalWriter<F> {
    /// Creates the journal `header` describes in `dir`, named
    /// [`JournalId::file_name`], whose first sample will be `first`. Makes
    /// the file and its header durable (fsync, then a directory fsync)
    /// before returning, so [`Self::durable`] starts at `first`.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] if any step fails, including when the file
    /// already exists. If a step after creating the file fails, the file
    /// is removed again (best effort).
    pub fn create<S>(
        fs: &S,
        dir: &Path,
        header: JournalHeader,
        first: SampleIndex,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, JournalError>
    where
        S: Fs<File = F>,
    {
        let path = dir.join(header.id().file_name());
        let mut file = fs.create(&path).map_err(JournalError::Io)?;
        let made_durable = file
            .write_all(&encode_header(header))
            .and_then(|()| file.sync())
            .and_then(|proof| fs.sync_dir(dir).map(|()| proof))
            .and_then(|proof| file.syncer().map(|syncer| (proof, syncer)));
        let (proof, syncer) = match made_durable {
            Ok(made) => made,
            Err(e) => {
                // Don't leave a half-made journal to block the next attempt.
                // Best effort: the original error is the one to report.
                let _ = fs.remove(&path);
                return Err(JournalError::Io(e));
            }
        };
        let last_sync = clock.now();
        Ok(Self {
            file,
            syncer: Arc::new(syncer),
            path,
            clock,
            header,
            next_seq: 0,
            start: first,
            captured: first,
            durable: DurablePosition::after(&proof, header, first),
            last_sync,
            requested: first,
            in_flight: VecDeque::new(),
            early: Vec::new(),
            next_sync: 0,
            token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
            broken: false,
            buf: Vec::new(),
        })
    }

    /// The journal's header: its id, track, epoch and rate.
    #[must_use]
    pub const fn header(&self) -> JournalHeader {
        self.header
    }

    /// Where the journal is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The journal's first sample.
    #[must_use]
    pub const fn start(&self) -> SampleIndex {
        self.start
    }

    /// The end of what's been written.
    #[must_use]
    pub const fn captured(&self) -> SampleIndex {
        self.captured
    }

    /// The end of what an fsync has confirmed.
    #[must_use]
    pub const fn durable(&self) -> DurablePosition {
        self.durable
    }

    /// Whether an earlier write or fsync failed.
    #[must_use]
    pub const fn is_broken(&self) -> bool {
        self.broken
    }

    /// Whether a sync was started and hasn't completed.
    #[must_use]
    pub fn sync_in_flight(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Whether everything written is durable, with no sync in flight: the
    /// journal can end without another fsync.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.in_flight.is_empty() && self.durable.end == self.captured
    }

    /// Samples captured but not yet fsync'd.
    fn unsynced(&self) -> SampleCount {
        self.captured
            .checked_count_since(self.durable.end)
            .unwrap_or(SampleCount::ZERO)
    }

    /// How many more samples [`Self::append_within`] takes before a sync
    /// must complete: the sync budget less what's unsynced.
    #[must_use]
    pub fn room(&self) -> u64 {
        sync_budget(self.header.rate())
            .get()
            .saturating_sub(self.unsynced().get())
    }

    /// Appends `samples`, continuing where the track left off, syncing
    /// whenever one is due, so no more than [`SYNC_INTERVAL`]'s worth of
    /// audio is ever unsynced.
    ///
    /// # Errors
    ///
    /// [`JournalError::SampleOverflow`], with nothing written;
    /// [`JournalError::Io`] if a write or fsync fails, which breaks the
    /// journal (frames before the failure may have been written: see
    /// [`Self::captured`]); [`JournalError::Broken`].
    pub fn append(&mut self, samples: &[i16]) -> Result<(), JournalError> {
        let mut rest = samples;
        loop {
            let written = self.append_within(rest)?;
            rest = rest.get(written..).unwrap_or_default();
            // Never more than the budget unsynced, even within one long
            // append: a full budget is synced at once.
            if self.room() == 0 || self.sync_due() {
                self.sync()?;
            }
            if rest.is_empty() {
                return Ok(());
            }
        }
    }

    /// Appends as much of `samples` as the sync budget leaves room for,
    /// continuing where the track left off, and returns how many samples
    /// that was. It never syncs (see [`Self::sync_due`]).
    ///
    /// # Errors
    ///
    /// [`JournalError::SampleOverflow`], with nothing written;
    /// [`JournalError::Io`] if a write fails, which breaks the journal
    /// (frames before the failure may have been written: see
    /// [`Self::captured`]); [`JournalError::Broken`].
    pub fn append_within(&mut self, samples: &[i16]) -> Result<usize, JournalError> {
        if self.broken {
            return Err(JournalError::Broken);
        }
        let total = u64::try_from(samples.len()).map_err(|_| JournalError::SampleOverflow)?;
        self.captured
            .checked_add(SampleCount::new(total))
            .ok_or(JournalError::SampleOverflow)?;

        // The max is a u32, so it fits in usize on every platform nota builds for.
        let max = usize::try_from(MAX_FRAME_SAMPLES).unwrap_or(usize::MAX);
        let room = usize::try_from(self.room()).unwrap_or(usize::MAX);
        let (mut rest, _) = samples.split_at(samples.len().min(room));
        let mut written = 0;
        while !rest.is_empty() {
            let (chunk, tail) = rest.split_at(rest.len().min(max));
            self.buf.clear();
            encode_frame(
                &mut self.buf,
                self.next_seq,
                self.header.track(),
                self.captured,
                chunk,
            );
            if let Err(e) = self.file.write_all(&self.buf) {
                self.broken = true;
                return Err(JournalError::Io(e));
            }
            self.next_seq += 1;
            // Checked above for the whole run, so each chunk fits; if it
            // somehow didn't, the frame is written, so the journal is broken.
            let Some(captured) = self
                .captured
                .checked_add(SampleCount::new(chunk.len() as u64))
            else {
                self.broken = true;
                return Err(JournalError::SampleOverflow);
            };
            self.captured = captured;
            written += chunk.len();
            rest = tail;
        }
        Ok(written)
    }

    /// Whether a sync should start now: something is written that no sync
    /// covers, none is in flight, and [`SYNC_INTERVAL`] has passed since
    /// the last one started or that much audio is unsynced.
    #[must_use]
    pub fn sync_due(&self) -> bool {
        if self.broken || !self.in_flight.is_empty() || self.captured <= self.requested {
            return false;
        }
        let waited = self
            .clock
            .now()
            .checked_duration_since(self.last_sync)
            .unwrap_or(Duration::ZERO);
        waited >= SYNC_INTERVAL || self.room() == 0
    }

    /// Whether something is written that no sync started covers: the
    /// journal needs another before it can end.
    #[must_use]
    pub fn needs_sync(&self) -> bool {
        self.captured > self.requested
    }

    /// Starts a sync of everything written so far, to run with
    /// [`PendingSync::run`], on any thread, and then complete with
    /// [`Self::complete_sync`]. Appends may go on meanwhile; the sync
    /// covers only what was written before it started.
    ///
    /// # Errors
    ///
    /// [`JournalError::Broken`].
    pub fn begin_sync(&mut self) -> Result<PendingSync<F::Syncer>, JournalError> {
        if self.broken {
            return Err(JournalError::Broken);
        }
        let seq = self.next_sync;
        self.next_sync += 1;
        self.in_flight.push_back(seq);
        self.requested = self.captured;
        self.last_sync = self.clock.now();
        Ok(PendingSync {
            syncer: Arc::clone(&self.syncer),
            header: self.header,
            end: self.captured,
            seq,
            token: self.token,
        })
    }

    /// Takes the result of a sync [`Self::begin_sync`] started: the
    /// durable position moves up to where the journal stood when it
    /// started, once every older sync has completed too, and never after
    /// one failed. A result for another writer, or one already taken, is
    /// ignored.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] if the fsync failed, which breaks the journal.
    pub fn complete_sync(&mut self, done: SyncDone) -> Result<(), JournalError> {
        let Some(at) = self
            .in_flight
            .iter()
            .position(|&seq| seq == done.seq)
            .filter(|_| done.token == self.token && done.journal == self.header.id())
        else {
            return Ok(());
        };
        self.in_flight.remove(at);
        match done.result {
            // After a failed fsync nothing unsynced then can be trusted,
            // even if a later fsync succeeds.
            Err(e) => {
                self.broken = true;
                self.in_flight.clear();
                self.early.clear();
                Err(JournalError::Io(e))
            }
            Ok(_) if self.broken => Ok(()),
            Ok(durable) => {
                self.early.push((done.seq, durable));
                let oldest = self.in_flight.front().copied().unwrap_or(u64::MAX);
                let (proved, waiting): (Vec<_>, Vec<_>) =
                    self.early.drain(..).partition(|&(seq, _)| seq < oldest);
                self.early = waiting;
                for (_, durable) in proved {
                    if durable.end > self.durable.end {
                        self.durable = durable;
                    }
                }
                Ok(())
            }
        }
    }

    /// Syncs if [`SYNC_INTERVAL`] has passed since the last sync, or that
    /// much audio is unsynced. Returns whether it synced.
    ///
    /// # Errors
    ///
    /// As [`Self::sync`].
    pub fn sync_if_due(&mut self) -> Result<bool, JournalError> {
        if self.broken {
            return Err(JournalError::Broken);
        }
        let due = self.sync_due();
        if due {
            self.sync()?;
        }
        Ok(due)
    }

    /// Ends the journal cleanly: a last fsync, so everything captured is
    /// durable, and returns how far that is. Call it when recording stops
    /// or the journal rotates; dropping the writer doesn't sync (an fsync
    /// in `drop` would block unseen and lose its error).
    ///
    /// # Errors
    ///
    /// As [`Self::sync`].
    pub fn finish(mut self) -> Result<DurablePosition, JournalError> {
        if self.broken {
            Err(JournalError::Broken)
        } else if self.sync_in_flight() {
            Err(JournalError::SyncPending)
        } else if self.is_settled() {
            Ok(self.durable)
        } else {
            self.sync().map(|()| self.durable)
        }
    }

    /// Fsyncs the journal now, on this thread, moving the durable position
    /// up to the captured one.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] if the fsync fails, which breaks the journal;
    /// [`JournalError::Broken`]; [`JournalError::SyncPending`], with
    /// nothing done, while a sync [`Self::begin_sync`] started hasn't
    /// completed.
    pub fn sync(&mut self) -> Result<(), JournalError> {
        if self.sync_in_flight() {
            return Err(JournalError::SyncPending);
        }
        let done = self.begin_sync()?.run();
        self.complete_sync(done)
    }
}
