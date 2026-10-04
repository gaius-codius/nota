//! Writing the journal: frames appended as audio arrives, fsync'd every
//! [`SYNC_INTERVAL`].

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nota_core::{Clock, SampleCount, SampleIndex, SampleRate, SessionTime, TrackId};

use super::JournalId;
use super::format::{JournalHeader, MAX_FRAME_SAMPLES, encode_frame, encode_header};
use crate::fs::{Fs, FsFile, Synced};

/// How often the journal is fsync'd while audio arrives: at most this long,
/// or this much audio, goes unsynced.
///
/// Durable may trail the audio the stream has delivered by this, plus the
/// stream's buffering (about 40 ms), plus however long the fsync takes.
/// 850 ms keeps that within the 1.1 s bounded-loss rule for fsyncs up to
/// about 200 ms; at a full second, one ~105 ms fsync was enough to break it.
pub const SYNC_INTERVAL: Duration = Duration::from_millis(850);

/// The most audio a journal at `rate` may hold unsynced: [`SYNC_INTERVAL`]'s
/// worth, and at least one sample, so even a very low rate makes progress.
/// Salvage judges torn tails by it too.
pub(crate) fn sync_budget(rate: SampleRate) -> SampleCount {
    SampleCount::started_within(SYNC_INTERVAL, rate)
        .unwrap_or(SampleCount::ZERO)
        .max(SampleCount::new(1))
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
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "journal write failed: {e}"),
            Self::SampleOverflow => f.write_str("the track ran out of sample numbers"),
            Self::Broken => f.write_str("the journal broke after an earlier failure"),
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
/// **durable**, the end of what an fsync has confirmed. Each
/// [`Self::append`] syncs when [`SYNC_INTERVAL`] has passed since the last
/// sync or that much audio is unsynced, so durable stays within
/// [`SYNC_INTERVAL`] of captured. Call [`Self::sync_if_due`] on a timer too, so audio
/// that stops arriving still gets synced.
///
/// After any failed write or fsync the writer is broken: every later call
/// returns [`JournalError::Broken`].
///
/// An fsync can take a second or more on a busy disk, and `append` may run
/// one, so the writer belongs on its own thread, fed by a channel, never on
/// the audio callback.
#[derive(Debug)]
pub struct JournalWriter<F> {
    file: F,
    path: PathBuf,
    clock: Arc<dyn Clock>,
    header: JournalHeader,
    next_seq: u64,
    start: SampleIndex,
    captured: SampleIndex,
    durable: DurablePosition,
    last_sync: SessionTime,
    /// Written since the last sync.
    dirty: bool,
    broken: bool,
    buf: Vec<u8>,
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
            .and_then(|proof| fs.sync_dir(dir).map(|()| proof));
        let proof = match made_durable {
            Ok(proof) => proof,
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
            path,
            clock,
            header,
            next_seq: 0,
            start: first,
            captured: first,
            durable: DurablePosition::after(&proof, header, first),
            last_sync,
            dirty: false,
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

    /// The most audio that may go unsynced, at the journal's rate.
    fn sync_budget(&self) -> SampleCount {
        sync_budget(self.header.rate())
    }

    /// Samples captured but not yet fsync'd.
    fn unsynced(&self) -> SampleCount {
        self.captured
            .checked_count_since(self.durable.end)
            .unwrap_or(SampleCount::ZERO)
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
        if self.broken {
            return Err(JournalError::Broken);
        }
        let total = u64::try_from(samples.len()).map_err(|_| JournalError::SampleOverflow)?;
        self.captured
            .checked_add(SampleCount::new(total))
            .ok_or(JournalError::SampleOverflow)?;

        // The max is a u32, so it fits in usize on every platform nota builds for.
        let max = usize::try_from(MAX_FRAME_SAMPLES).unwrap_or(usize::MAX);
        let budget = self.sync_budget().get();
        let mut rest = samples;
        while !rest.is_empty() {
            // Never more than the budget unsynced, even within one long
            // append: a frame never runs past it, and the sync check runs
            // after every frame.
            let room = budget.saturating_sub(self.unsynced().get());
            if room == 0 {
                self.sync()?;
                continue;
            }
            let len = rest
                .len()
                .min(max)
                .min(usize::try_from(room).unwrap_or(usize::MAX));
            let (chunk, tail) = rest.split_at(len);
            self.buf.clear();
            encode_frame(
                &mut self.buf,
                self.next_seq,
                self.header.track(),
                self.captured,
                chunk,
            );
            self.dirty = true;
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
            rest = tail;
            self.sync_if_due()?;
        }
        Ok(())
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
        if !self.dirty {
            return Ok(false);
        }
        let waited = self
            .clock
            .now()
            .checked_duration_since(self.last_sync)
            .unwrap_or(Duration::ZERO);
        let due = waited >= SYNC_INTERVAL || self.unsynced() >= self.sync_budget();
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
        } else if self.dirty {
            self.sync().map(|()| self.durable)
        } else {
            Ok(self.durable)
        }
    }

    /// Fsyncs the journal now, moving the durable position up to the
    /// captured one.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] if the fsync fails, which breaks the journal;
    /// [`JournalError::Broken`].
    pub fn sync(&mut self) -> Result<(), JournalError> {
        if self.broken {
            return Err(JournalError::Broken);
        }
        let proof = match self.file.sync() {
            Ok(proof) => proof,
            Err(e) => {
                self.broken = true;
                return Err(JournalError::Io(e));
            }
        };
        self.durable = DurablePosition::after(&proof, self.header, self.captured);
        self.dirty = false;
        self.last_sync = self.clock.now();
        Ok(())
    }
}
