//! Which samples a moment is made of: the window around a session time, cut
//! along the track's epochs.
//!
//! The window runs [`REACH`] either side of the moment, clamped to the
//! recording. Each epoch the window meets gives a run of the track's samples,
//! and the time between two runs (a gap between epochs, or the window's
//! edge where no epoch reaches) gives silence, so what is heard has the
//! timeline's shape. Nothing here reads a file: it only decides.

use std::time::Duration;

use nota_core::{Epoch, SampleCount, SampleRange, SampleRate, SessionTime, TrackTimeline};

use super::PlaybackError;

/// How far either side of a moment the window reaches.
pub const REACH: Duration = Duration::from_millis(2_500);

/// One stretch of a moment, in the order it is heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Piece {
    /// These samples of the track, recorded in one epoch.
    Audio(SampleRange),
    /// No audio: the time between two epochs, or between the window's edge
    /// and the nearest audio.
    Silence(Duration),
}

/// A moment's pieces and the rate its audio is at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Plan {
    /// The rate of every audio piece, and of the silence between them.
    pub(super) rate: SampleRate,
    /// The pieces, in order.
    pub(super) pieces: Vec<Piece>,
}

/// The plan for the moment at `at` in the track `timeline` times, whose
/// published audio is `recorded`.
///
/// # Errors
///
/// [`PlaybackError::NotTimed`] if the timeline has no epochs;
/// [`PlaybackError::OutsideRecording`] if `at` isn't within `recorded`;
/// [`PlaybackError::RateChanged`] if the window meets epochs at different
/// rates; [`PlaybackError::TimeOverflow`] if a time doesn't fit.
pub(super) fn plan(
    timeline: &TrackTimeline,
    recorded: SampleRange,
    at: SessionTime,
) -> Result<Plan, PlaybackError> {
    let first = timeline.epochs().first().ok_or(PlaybackError::NotTimed)?;
    let (begins, ends) = timeline
        .span_of(recorded)
        .ok_or(PlaybackError::TimeOverflow)?;
    if at < begins || at > ends {
        return Err(PlaybackError::OutsideRecording { at });
    }
    let (from, to) = window(at, (begins, ends));

    let mut cursor = from;
    let mut pieces = Vec::new();
    let mut rate = None;
    for (epoch, run) in runs(timeline, recorded) {
        let Some(range) = clip(epoch, run, (from, to)) else {
            continue;
        };
        let starts = epoch
            .time_of(range.start())
            .ok_or(PlaybackError::TimeOverflow)?;
        pieces.extend(silence(cursor, starts));
        pieces.push(Piece::Audio(range));
        cursor = epoch
            .time_of(range.end())
            .ok_or(PlaybackError::TimeOverflow)?;
        if *rate.get_or_insert(epoch.rate()) != epoch.rate() {
            return Err(PlaybackError::RateChanged);
        }
    }
    pieces.extend(silence(cursor, to));
    Ok(Plan {
        rate: rate.unwrap_or_else(|| first.rate()),
        pieces,
    })
}

/// The window around `at`, clamped to the recording that lasts from `begins`
/// to `ends`.
fn window(
    at: SessionTime,
    (begins, ends): (SessionTime, SessionTime),
) -> (SessionTime, SessionTime) {
    let from = at.as_nanos().saturating_sub(nanos(REACH));
    let to = at.checked_add(REACH).map_or(ends, |end| end.min(ends));
    (SessionTime::from_nanos(from.max(begins.as_nanos())), to)
}

/// How many samples of silence last `duration` at `rate`.
pub(super) fn silent_samples(duration: Duration, rate: SampleRate) -> Option<usize> {
    let count = SampleCount::started_within(duration, rate)?;
    usize::try_from(count.get()).ok()
}

/// `duration` in whole nanoseconds, stopping at the largest the session
/// clock holds.
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// The silence from `from` to `to`, if there is any.
fn silence(from: SessionTime, to: SessionTime) -> Option<Piece> {
    to.checked_duration_since(from)
        .filter(|gap| !gap.is_zero())
        .map(Piece::Silence)
}

/// Each epoch with the run of `recorded` samples it timed: from its first
/// sample (or the recording's first) to the next epoch's first (or the
/// recording's end). An epoch that timed no recorded sample is left out.
fn runs(
    timeline: &TrackTimeline,
    recorded: SampleRange,
) -> impl Iterator<Item = (&Epoch, SampleRange)> {
    let epochs = timeline.epochs();
    epochs.iter().enumerate().filter_map(move |(i, epoch)| {
        let begins = epoch.first_sample().max(recorded.start());
        let ends = epochs.get(i + 1).map_or(recorded.end(), |next| {
            next.first_sample().min(recorded.end())
        });
        Some((epoch, SampleRange::new(begins, ends)?)).filter(|(_, run)| !run.is_empty())
    })
}

/// The samples of `run` that play within `window`: from the one playing at
/// its start to the last that starts before its end. `None` if the window
/// doesn't meet `run`.
fn clip(
    epoch: &Epoch,
    run: SampleRange,
    window: (SessionTime, SessionTime),
) -> Option<SampleRange> {
    let (from, to) = window;
    let begins = if epoch.time_of(run.start())? >= from {
        run.start()
    } else {
        epoch.sample_at(from)?.max(run.start())
    };
    let playing = epoch.sample_at(to)?;
    let ends = if epoch.time_of(playing)? < to {
        playing.checked_add(SampleCount::new(1))?
    } else {
        playing
    };
    SampleRange::new(begins, ends.min(run.end())).filter(|range| !range.is_empty())
}
