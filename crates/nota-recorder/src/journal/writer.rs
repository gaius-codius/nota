//! Writing the journal: frames appended as audio arrives, fsync'd about
//! every second.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nota_core::{Clock, SampleCount, SampleIndex, SampleRate, SessionTime, TrackId};

use super::format::{MAX_FRAME_SAMPLES, encode_frame, encode_header};
use crate::fs::{Fs, FsFile, Synced};

/// How often the journal is fsync'd while audio arrives: at most this long,
/// or this much audio on any track, goes unsynced.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(1);

/// How far a track has been fsync'd: every sample the journal holds for it,
/// from the track's start up to `end`, is on disk. Only a completed fsync of
/// the journal makes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurablePosition {
    track: TrackId,
    end: SampleIndex,
}

impl DurablePosition {
    /// The position `end` of `track`, made durable by the fsync `_proof`
    /// stands for. Private: [`JournalWriter::sync`] is the only caller.
    const fn after(_proof: &Synced, track: TrackId, end: SampleIndex) -> Self {
        Self { track, end }
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
    /// Audio for a track that wasn't started.
    UnknownTrack(TrackId),
    /// A track started twice.
    TrackExists(TrackId),
    /// The track's sample index would overflow.
    SampleOverflow(TrackId),
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
            Self::UnknownTrack(t) => write!(f, "track {} wasn't started", t.get()),
            Self::TrackExists(t) => write!(f, "track {} was already started", t.get()),
            Self::SampleOverflow(t) => write!(f, "track {} ran out of sample numbers", t.get()),
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

#[derive(Debug, Clone, Copy)]
struct Track {
    start: SampleIndex,
    captured: SampleIndex,
    durable: Option<DurablePosition>,
}

impl Track {
    /// Samples captured but not yet fsync'd.
    fn unsynced(&self) -> SampleCount {
        let from = self.durable.map_or(self.start, DurablePosition::end);
        self.captured
            .checked_count_since(from)
            .unwrap_or(SampleCount::ZERO)
    }
}

/// Appends audio to a journal file and fsyncs it about every second.
///
/// Per track it keeps two positions: **captured**, the end of what was
/// written, and **durable**, the end of what an fsync has confirmed. Each
/// [`Self::append`] syncs when [`SYNC_INTERVAL`] has passed since the last
/// sync or a track has that much audio unsynced, so durable stays within
/// about a second of captured. Call [`Self::sync_if_due`] on a timer too,
/// so audio that stops arriving still gets synced.
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
    clock: Arc<dyn Clock>,
    rate: SampleRate,
    next_seq: u64,
    tracks: BTreeMap<TrackId, Track>,
    last_sync: SessionTime,
    /// Written since the last sync.
    dirty: bool,
    broken: bool,
    buf: Vec<u8>,
}

impl<F: FsFile> JournalWriter<F> {
    /// Creates a journal at `path` for audio at `rate`, and makes the file
    /// and its header durable (fsync, then a directory fsync) before
    /// returning.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] if any step fails, including when the file
    /// already exists. If a step after creating the file fails, the file
    /// is removed again (best effort).
    pub fn create<S>(
        fs: &S,
        path: &Path,
        rate: SampleRate,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, JournalError>
    where
        S: Fs<File = F>,
    {
        let dir = path
            .parent()
            .ok_or_else(|| JournalError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?;
        let mut file = fs.create(path).map_err(JournalError::Io)?;
        let made_durable = file
            .write_all(&encode_header(rate))
            .and_then(|()| file.sync())
            .and_then(|_| fs.sync_dir(dir));
        if let Err(e) = made_durable {
            // Don't leave a half-made journal to block the next attempt.
            // Best effort: the original error is the one to report.
            let _ = fs.remove(path);
            return Err(JournalError::Io(e));
        }
        let last_sync = clock.now();
        Ok(Self {
            file,
            clock,
            rate,
            next_seq: 0,
            tracks: BTreeMap::new(),
            last_sync,
            dirty: false,
            broken: false,
            buf: Vec::new(),
        })
    }

    /// The sampling rate of every track.
    #[must_use]
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// Starts `track` at sample `at`: its first audio will be sample `at`.
    ///
    /// # Errors
    ///
    /// [`JournalError::TrackExists`] if it was started already;
    /// [`JournalError::Broken`].
    pub fn start_track(&mut self, track: TrackId, at: SampleIndex) -> Result<(), JournalError> {
        if self.broken {
            return Err(JournalError::Broken);
        }
        if self.tracks.contains_key(&track) {
            return Err(JournalError::TrackExists(track));
        }
        self.tracks.insert(
            track,
            Track {
                start: at,
                captured: at,
                durable: None,
            },
        );
        Ok(())
    }

    /// The end of what's been written for `track`.
    #[must_use]
    pub fn captured(&self, track: TrackId) -> Option<SampleIndex> {
        self.tracks.get(&track).map(|t| t.captured)
    }

    /// The end of what an fsync has confirmed for `track`; `None` before the
    /// track's first sync.
    #[must_use]
    pub fn durable(&self, track: TrackId) -> Option<DurablePosition> {
        self.tracks.get(&track).and_then(|t| t.durable)
    }

    /// Appends `samples` to `track`, continuing where it left off, syncing
    /// whenever one is due, so the track never has more than a second of
    /// audio unsynced.
    ///
    /// # Errors
    ///
    /// [`JournalError::UnknownTrack`] or [`JournalError::SampleOverflow`],
    /// with nothing written; [`JournalError::Io`] if a write or fsync fails,
    /// which breaks the journal; [`JournalError::Broken`].
    pub fn append(&mut self, track: TrackId, samples: &[i16]) -> Result<(), JournalError> {
        if self.broken {
            return Err(JournalError::Broken);
        }
        let mut first = self
            .tracks
            .get(&track)
            .ok_or(JournalError::UnknownTrack(track))?
            .captured;
        let total =
            u64::try_from(samples.len()).map_err(|_| JournalError::SampleOverflow(track))?;
        first
            .checked_add(SampleCount::new(total))
            .ok_or(JournalError::SampleOverflow(track))?;

        // The max is a u32, so it fits in usize on every platform nota builds for.
        let max = usize::try_from(MAX_FRAME_SAMPLES).unwrap_or(usize::MAX);
        let second = u64::from(self.rate.hz());
        let mut rest = samples;
        while !rest.is_empty() {
            // Never more than a second unsynced on a track, even within one
            // long append: a frame never runs past the budget, and the sync
            // check runs after every frame.
            let unsynced = self.tracks.get(&track).map_or(0, |t| t.unsynced().get());
            let room = second.saturating_sub(unsynced);
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
            encode_frame(&mut self.buf, self.next_seq, track, first, chunk);
            self.dirty = true;
            if let Err(e) = self.file.write_all(&self.buf) {
                self.broken = true;
                return Err(JournalError::Io(e));
            }
            self.next_seq += 1;
            // Checked above for the whole run, so each chunk fits.
            first = first
                .checked_add(SampleCount::new(chunk.len() as u64))
                .ok_or(JournalError::SampleOverflow(track))?;
            if let Some(t) = self.tracks.get_mut(&track) {
                t.captured = first;
            }
            rest = tail;
            self.sync_if_due()?;
        }
        Ok(())
    }

    /// Syncs if [`SYNC_INTERVAL`] has passed since the last sync, or a track
    /// has that much audio unsynced. Returns whether it synced.
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
        let second_of_audio = SampleCount::new(u64::from(self.rate.hz()));
        let due = waited >= SYNC_INTERVAL
            || self
                .tracks
                .values()
                .any(|t| t.unsynced() >= second_of_audio);
        if due {
            self.sync()?;
        }
        Ok(due)
    }

    /// Ends the journal cleanly: a last fsync, so everything captured is
    /// durable. Call it when recording stops; dropping the writer doesn't
    /// sync (an fsync in `drop` would block unseen and lose its error).
    ///
    /// # Errors
    ///
    /// As [`Self::sync`].
    pub fn finish(mut self) -> Result<(), JournalError> {
        if self.dirty {
            self.sync()
        } else if self.broken {
            Err(JournalError::Broken)
        } else {
            Ok(())
        }
    }

    /// Fsyncs the journal now, moving every track's durable position up to
    /// its captured one.
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
        for (&id, track) in &mut self.tracks {
            track.durable = Some(DurablePosition::after(&proof, id, track.captured));
        }
        self.dirty = false;
        self.last_sync = self.clock.now();
        Ok(())
    }
}
