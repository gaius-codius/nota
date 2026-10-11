//! Playing a moment of a session: about five seconds of a track's audio
//! around a session time, on the default output.
//!
//! [`play_moment`] decodes the published FLAC segments that hold the moment
//! and hands the samples to an [`Output`]; [`CpalOutput`] plays them through
//! cpal on the default output device. The correction card calls it so the
//! user can hear what was said before choosing a word.
//!
//! # What plays
//!
//! The window is [`REACH`] (2.5 s) either side of the session time, clamped
//! to the recording. The track's epoch anchors turn session times into
//! samples (see `plan`), so a moment that crosses a segment boundary plays
//! on without a seam, and one that crosses a gap between epochs plays the
//! gap as silence of its length: what is heard matches the timeline. Every
//! segment read is checked against its row, as the final pass checks it
//! (see [`read_segment`]). A moment is played from a track: the caller says
//! which, as the microphone and the system's audio are not mixed here.
//!
//! # Never while a recording runs
//!
//! Playing takes the session's [`SessionLock`] and keeps it until the
//! [`Playback`] is stopped or dropped. A recording holds that lock, so
//! playing is refused with [`PlaybackError::Busy`] while one runs, in this
//! process or another; and a recording or a repair can't start under a
//! moment that is playing.
//!
//! # What isn't covered
//!
//! Audio that isn't published yet (a journal still waiting for salvage)
//! isn't played: a moment whose samples aren't all in a published segment is
//! refused with [`PlaybackError::Missing`], not played with a hole in it.

use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use nota_core::{
    EpochError, SampleCount, SampleIndex, SampleRange, SampleRate, SessionTime, TrackId,
    TrackTimeline,
};
use nota_store::{SegmentRow, Store, StoreError};

use crate::fs::Fs;
use crate::segment::{ReadSegmentError, read_segment};
use crate::session::{SessionDir, SessionLock};

#[cfg(target_os = "linux")]
mod cpal_output;
mod plan;
#[cfg(test)]
mod tests;

#[cfg(target_os = "linux")]
pub use cpal_output::{CpalOutput, CpalStream};
pub use plan::REACH;

use plan::{Piece, Plan, silent_samples};

/// Why a moment couldn't be played. The messages are for the card to show.
#[derive(Debug)]
pub enum PlaybackError {
    /// The session is in use: it is being recorded, or repaired, or another
    /// moment of it is playing. Nothing was read or played.
    Busy,
    /// The session's directory couldn't be locked.
    Session(io::Error),
    /// The library couldn't give the session's epochs or segments.
    Store(StoreError),
    /// The stored epochs don't make a timeline.
    Timeline(EpochError),
    /// The track has no published audio.
    NoAudio(TrackId),
    /// The track's audio has no epoch timing it (a session recorded before
    /// epochs were kept), so a session time can't be turned into a sample.
    NotTimed,
    /// The session time is before the track's audio or after its end.
    OutsideRecording {
        /// The time asked for.
        at: SessionTime,
    },
    /// The moment has audio that no published segment holds.
    Missing {
        /// The first sample no segment holds.
        from: SampleIndex,
    },
    /// The moment spans epochs recorded at different rates.
    RateChanged,
    /// A time or a sample count doesn't fit its type.
    TimeOverflow,
    /// A segment couldn't be read back (see [`read_segment`]).
    Segment(ReadSegmentError),
    /// The output refused the audio.
    Output(OutputError),
}

impl fmt::Display for PlaybackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("can't play while the session is being recorded or repaired"),
            Self::Session(e) => write!(f, "couldn't open the session to play it: {e}"),
            Self::Store(e) => write!(f, "couldn't read the session from the library: {e}"),
            Self::Timeline(e) => write!(f, "the session's timing is not usable: {e}"),
            Self::NoAudio(track) => write!(f, "track {} has no saved audio", track.get()),
            Self::NotTimed => f.write_str("the session has no timing for this track's audio"),
            Self::OutsideRecording { at } => {
                write!(f, "{} ns is outside the recording", at.as_nanos())
            }
            Self::Missing { from } => {
                write!(f, "the audio from sample {} isn't saved yet", from.get())
            }
            Self::RateChanged => f.write_str("the moment spans audio at different rates"),
            Self::TimeOverflow => f.write_str("the moment's time is out of range"),
            Self::Segment(e) => write!(f, "a saved segment can't be played: {e}"),
            Self::Output(e) => write!(f, "couldn't play it: {e}"),
        }
    }
}

impl std::error::Error for PlaybackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Session(e) => Some(e),
            Self::Store(e) => Some(e),
            Self::Timeline(e) => Some(e),
            Self::Segment(e) => Some(e),
            Self::Output(e) => Some(e),
            _ => None,
        }
    }
}

/// Why an [`Output`] couldn't play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputError {
    /// There is no default output device.
    NoDevice,
    /// The device's format isn't one the output plays (which).
    Unsupported(String),
    /// The audio system refused (why).
    Backend(String),
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevice => f.write_str("there is no output device"),
            Self::Unsupported(format) => {
                write!(f, "the output device wants {format}, not supported")
            }
            Self::Backend(why) => write!(f, "the audio system refused: {why}"),
        }
    }
}

impl std::error::Error for OutputError {}

/// Somewhere to play audio. Real output is [`CpalOutput`]; tests use a fake
/// that records what it was given.
pub trait Output {
    /// What plays one moment. Dropping it stops the sound.
    type Stream: OutputStream;

    /// Starts playing mono 16-bit `samples` at `rate`, once.
    ///
    /// # Errors
    ///
    /// [`OutputError`] if nothing can play them.
    fn start(&self, samples: &[i16], rate: SampleRate) -> Result<Self::Stream, OutputError>;
}

/// One moment playing on an [`Output`]; dropping it stops it.
pub trait OutputStream {
    /// Whether every sample has been handed to the device, or the device
    /// has failed: either way, nothing more will be heard.
    fn finished(&self) -> bool;
}

/// A moment playing. Holds the session's lock until it is stopped or
/// dropped, so nothing records or repairs the session meanwhile.
#[derive(Debug)]
pub struct Playback<S: Fs, T> {
    /// Dropped first, so the sound stops before the lock is let go.
    stream: T,
    length: Duration,
    _session: SessionLock<S>,
}

impl<S: Fs, T: OutputStream> Playback<S, T> {
    /// How long the moment lasts.
    #[must_use]
    pub const fn length(&self) -> Duration {
        self.length
    }

    /// Whether it has played to its end (or the output failed).
    #[must_use]
    pub fn finished(&self) -> bool {
        self.stream.finished()
    }

    /// Stops the sound and lets the session go.
    pub fn stop(self) {
        drop(self);
    }
}

/// Plays the moment at `at` of `track` in `session`, about five seconds
/// around it, on `output`.
///
/// # Errors
///
/// [`PlaybackError::Busy`] while the session is being recorded; otherwise as
/// the variants say: the session or its library can't be read, `at` is
/// outside the track's recording, audio the moment needs isn't saved, or the
/// output refuses.
pub fn play_moment<S, O>(
    session: &SessionDir<S>,
    store: &Store,
    track: TrackId,
    at: SessionTime,
    output: &O,
) -> Result<Playback<S, O::Stream>, PlaybackError>
where
    S: Fs + Clone,
    O: Output,
{
    let lock = session.lock().map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock => PlaybackError::Busy,
        _ => PlaybackError::Session(e),
    })?;
    let rows = rows_of(store, session, track)?;
    let timeline = timeline_of(store, session, track)?;
    let plan = plan::plan(&timeline, recorded(&rows, track)?, at)?;
    let samples = assemble(session.fs(), session.dir(), &rows, &plan)?;
    let stream = output
        .start(&samples, plan.rate)
        .map_err(PlaybackError::Output)?;
    Ok(Playback {
        stream,
        length: SampleCount::new(samples.len() as u64)
            .duration_at(plan.rate)
            .ok_or(PlaybackError::TimeOverflow)?,
        _session: lock,
    })
}

/// `track`'s published segments, first sample first.
fn rows_of<S: Fs>(
    store: &Store,
    session: &SessionDir<S>,
    track: TrackId,
) -> Result<Vec<SegmentRow>, PlaybackError> {
    let mut rows = store.segments(session.id()).map_err(PlaybackError::Store)?;
    rows.retain(|row| row.track() == track);
    rows.sort_by_key(|row| row.range().start());
    Ok(rows)
}

/// `track`'s epochs, as the library holds them.
fn timeline_of<S: Fs>(
    store: &Store,
    session: &SessionDir<S>,
    track: TrackId,
) -> Result<TrackTimeline, PlaybackError> {
    let anchors = store.epochs(session.id()).map_err(PlaybackError::Store)?;
    TrackTimeline::rebuild(
        track,
        anchors
            .into_iter()
            .filter(|(of, _)| *of == track)
            .map(|(_, anchor)| anchor),
    )
    .map_err(PlaybackError::Timeline)
}

/// From the first sample of `rows` (sorted) to the end of the last.
fn recorded(rows: &[SegmentRow], track: TrackId) -> Result<SampleRange, PlaybackError> {
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return Err(PlaybackError::NoAudio(track));
    };
    SampleRange::new(first.range().start(), last.range().end()).ok_or(PlaybackError::NoAudio(track))
}

/// The samples of `plan`: its audio read from the segments in `dir`, and
/// zeros for its silence.
fn assemble<S: Fs>(
    fs: &S,
    dir: &Path,
    rows: &[SegmentRow],
    plan: &Plan,
) -> Result<Vec<i16>, PlaybackError> {
    let mut samples = Vec::new();
    for piece in &plan.pieces {
        match *piece {
            Piece::Audio(range) => read_range(fs, dir, rows, range, plan.rate, &mut samples)?,
            Piece::Silence(length) => {
                let count = silent_samples(length, plan.rate).ok_or(PlaybackError::TimeOverflow)?;
                samples.resize(samples.len() + count, 0);
            }
        }
    }
    Ok(samples)
}

/// Appends the samples of `range` to `into`, read from the segments among
/// `rows` (sorted, and not overlapping) that cover it.
fn read_range<S: Fs>(
    fs: &S,
    dir: &Path,
    rows: &[SegmentRow],
    range: SampleRange,
    rate: SampleRate,
    into: &mut Vec<i16>,
) -> Result<(), PlaybackError> {
    let mut next = range.start();
    for row in rows
        .iter()
        .filter(|row| row.range().end() > range.start() && row.range().start() < range.end())
    {
        // A segment that starts after the sample wanted leaves a hole.
        if row.range().start() > next {
            return Err(PlaybackError::Missing { from: next });
        }
        let end = row.range().end().min(range.end());
        let segment = read_segment(fs, dir, row, rate).map_err(PlaybackError::Segment)?;
        let part = index(next, row.range().start())?..index(end, row.range().start())?;
        into.extend_from_slice(
            segment
                .get(part)
                .ok_or(PlaybackError::Missing { from: next })?,
        );
        next = end;
    }
    if next < range.end() {
        return Err(PlaybackError::Missing { from: next });
    }
    Ok(())
}

/// Where `sample` is within a segment that starts at `first`.
fn index(sample: SampleIndex, first: SampleIndex) -> Result<usize, PlaybackError> {
    sample
        .checked_count_since(first)
        .and_then(|count| usize::try_from(count.get()).ok())
        .ok_or(PlaybackError::TimeOverflow)
}
