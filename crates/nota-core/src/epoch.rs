//! Track epochs: how a track's sample count maps to session time.
//!
//! A track's stream can stop and reopen during a session (a device change,
//! sleep, a stall), or the audio server can drop some of its audio (an
//! overrun). Each opening, and each overrun, starts an epoch, which pins the
//! next sample to a session time. The sample count carries on across epochs, so the time
//! between them is a gap with no audio, and a mark made during it maps to no
//! sample.
//!
//! Each epoch also carries the [`Drift`] its device's clock was measured at
//! when it opened, and maps its samples at the rate the device actually ran
//! at. When the drift is measured, or changes, the track moves to a new
//! epoch that follows straight on from the old one
//! ([`TrackTimeline::retime`]), with no gap; see [`crate::drift`].
//!
//! An epoch's [`EpochAnchor`] is what has to be kept to time its samples
//! again later: its number, first sample, rate, drift and start. The recorder
//! stores it with the epoch's audio, and [`TrackTimeline::rebuild`] makes a
//! timeline from the anchors again after a crash or for a resumed session.

use std::time::Duration;

use crate::drift::Drift;
use crate::ids::{EpochId, TrackId};
use crate::time::{SampleIndex, SampleRange, SampleRate, SessionTime};

/// One epoch of a track: from `first_sample` on, sample `s` plays at
/// `start + (s - first_sample) / (rate * (1 + drift))`. It runs until the
/// next epoch's first sample, or for the newest epoch, open-ended.
///
/// Only a [`TrackTimeline`] makes epochs, so every epoch has been checked
/// against the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Epoch {
    id: EpochId,
    start: SessionTime,
    first_sample: SampleIndex,
    rate: SampleRate,
    drift: Drift,
    overrun: Duration,
}

impl Epoch {
    /// How far this epoch's start was pushed back because the previous
    /// epoch's audio, at its rate and drift, ran past the requested start:
    /// the error its mapping had built up by this reopening. Zero
    /// normally.
    #[must_use]
    pub const fn overrun(&self) -> Duration {
        self.overrun
    }

    /// The epoch's number within its track.
    #[must_use]
    pub const fn id(&self) -> EpochId {
        self.id
    }

    /// The session time of the epoch's first sample.
    #[must_use]
    pub const fn start(&self) -> SessionTime {
        self.start
    }

    /// The track's sample count when the stream (re)opened.
    #[must_use]
    pub const fn first_sample(&self) -> SampleIndex {
        self.first_sample
    }

    /// The stream's sampling rate in this epoch.
    #[must_use]
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// How far the device's clock ran from the session clock, as this
    /// epoch maps its samples.
    #[must_use]
    pub const fn drift(&self) -> Drift {
        self.drift
    }

    /// What has to be kept to time this epoch's samples again: everything
    /// but its overrun.
    #[must_use]
    pub const fn anchor(&self) -> EpochAnchor {
        EpochAnchor {
            id: self.id,
            start: self.start,
            first_sample: self.first_sample,
            rate: self.rate,
            drift: self.drift,
        }
    }

    /// The session time `sample` plays at, if this epoch's mapping reached
    /// it. `None` if it comes before the epoch or the time overflows. It
    /// doesn't check where the epoch ends; [`TrackTimeline::time_of`] does.
    #[must_use]
    pub fn time_of(&self, sample: SampleIndex) -> Option<SessionTime> {
        let offset = sample.checked_count_since(self.first_sample)?;
        self.start
            .checked_add(self.drift.duration_of(offset, self.rate)?)
    }

    /// Whether an epoch starting at `start` from `first_sample` may come
    /// after this one as it stands: its sample count doesn't go back, and
    /// it starts no earlier than this epoch's audio ends.
    fn check_followed_by(
        &self,
        start: SessionTime,
        first_sample: SampleIndex,
    ) -> Result<(), EpochError> {
        if first_sample < self.first_sample {
            return Err(EpochError::SampleWentBack {
                previous: self.first_sample,
                first_sample,
            });
        }
        let previous_end = self.time_of(first_sample).ok_or(EpochError::TimeOverflow)?;
        if start < previous_end {
            return Err(EpochError::ImplausibleOverrun {
                previous_end,
                start,
            });
        }
        Ok(())
    }

    /// The sample playing at `time` under this epoch's mapping. `None` if
    /// `time` comes before the epoch. It doesn't check where the epoch ends;
    /// [`TrackTimeline::sample_at`] does.
    #[must_use]
    pub fn sample_at(&self, time: SessionTime) -> Option<SampleIndex> {
        let elapsed = time.checked_duration_since(self.start)?;
        let offset = self.drift.count_within(elapsed, self.rate)?;
        self.first_sample.checked_add(offset)
    }
}

/// An epoch as it's stored: from `first_sample` on, the track's samples
/// play at `rate` under `drift` from `start`. Unchecked: only
/// [`TrackTimeline::rebuild`] turns anchors back into epochs, checking each
/// against the one before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochAnchor {
    /// The epoch's number within its track.
    pub id: EpochId,
    /// The session time of the epoch's first sample.
    pub start: SessionTime,
    /// The track's sample count when the epoch opened.
    pub first_sample: SampleIndex,
    /// The stream's sampling rate in the epoch.
    pub rate: SampleRate,
    /// How far the device's clock ran from the session clock, as the epoch
    /// maps its samples. Zero in anchors kept before drift was measured.
    pub drift: Drift,
}

/// The time between two epochs of a track, when there was no audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    after: EpochId,
    from: SessionTime,
    to: SessionTime,
}

impl Gap {
    /// The epoch the gap follows.
    #[must_use]
    pub const fn after(&self) -> EpochId {
        self.after
    }

    /// When the earlier epoch's audio ended.
    #[must_use]
    pub const fn from(&self) -> SessionTime {
        self.from
    }

    /// When the next epoch started.
    #[must_use]
    pub const fn to(&self) -> SessionTime {
        self.to
    }

    /// How long the gap lasted.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        Duration::from_nanos(self.to.as_nanos() - self.from.as_nanos())
    }
}

/// The overrun always allowed, however short the previous epoch: covers a
/// device clock's drift over a short epoch plus scheduling jitter.
const MIN_OVERRUN_ALLOWED_NANOS: u64 = 10_000_000;

/// The most a previous epoch of `length` can overrun the next one's
/// requested start before it's refused: 1000 ppm, ten times a poor USB
/// device's drift, plus [`MIN_OVERRUN_ALLOWED_NANOS`].
fn max_overrun(length: Duration) -> Duration {
    (length / 1_000).saturating_add(Duration::from_nanos(MIN_OVERRUN_ALLOWED_NANOS))
}

/// A newly opened epoch, as [`TrackTimeline::open_epoch`] placed it. The
/// epoch keeps its overrun too ([`Epoch::overrun`]), so ignoring this loses
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenedEpoch {
    /// The new epoch.
    pub id: EpochId,
    /// Where it starts: the requested start, or later if the previous
    /// epoch's audio hadn't ended by then.
    pub start: SessionTime,
    /// How far the previous epoch's audio, timed at its rate and drift, ran
    /// past the requested start. Zero normally; more means the device's
    /// clock ran faster than its drift says, and this is the error it
    /// built up.
    pub overrun: Duration,
}

/// Why a new epoch was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochError {
    /// The new epoch's first sample is before the previous epoch's. A
    /// track's sample count never goes back.
    SampleWentBack {
        /// The previous epoch's first sample.
        previous: SampleIndex,
        /// The refused first sample.
        first_sample: SampleIndex,
    },
    /// The new epoch was requested so long before the previous epoch's
    /// audio ended that drift can't explain it: more audio arrived than
    /// time passed, by more than 1000 ppm plus 10 ms.
    ImplausibleOverrun {
        /// When the previous epoch's audio ended, at its rate and drift.
        previous_end: SessionTime,
        /// The refused start.
        start: SessionTime,
    },
    /// The previous epoch's end can't be expressed in session time.
    TimeOverflow,
    /// The track already has as many epochs as an [`EpochId`] can number.
    TooManyEpochs,
    /// An epoch to follow straight on from another
    /// ([`TrackTimeline::retime`]) was asked for before the track had one.
    NoEpoch,
    /// An epoch to follow ([`TrackTimeline::follow`]) isn't the next one
    /// the timeline would open: one was missed, or came twice.
    NotNext {
        /// The id the next epoch would have.
        expected: EpochId,
        /// The id it had.
        got: EpochId,
    },
    /// An anchor to rebuild from ([`TrackTimeline::rebuild`]) isn't
    /// numbered above the one before it.
    NotAfter {
        /// The previous anchor's id.
        previous: EpochId,
        /// The refused anchor's id.
        got: EpochId,
    },
}

impl std::fmt::Display for EpochError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SampleWentBack {
                previous,
                first_sample,
            } => write!(
                f,
                "a new epoch's first sample ({}) is before the previous epoch's ({})",
                first_sample.get(),
                previous.get()
            ),
            Self::ImplausibleOverrun {
                previous_end,
                start,
            } => write!(
                f,
                "a new epoch was requested at {} ns, too long before the previous epoch's \
                 audio ended at {} ns to be drift",
                start.as_nanos(),
                previous_end.as_nanos()
            ),
            Self::TimeOverflow => f.write_str("the previous epoch ends beyond the session clock"),
            Self::TooManyEpochs => f.write_str("the track has run out of epoch numbers"),
            Self::NoEpoch => f.write_str("the track has no epoch to follow on from"),
            Self::NotNext { expected, got } => write!(
                f,
                "epoch {} came where epoch {} was due",
                got.get(),
                expected.get()
            ),
            Self::NotAfter { previous, got } => write!(
                f,
                "epoch {} came after epoch {}, not above it",
                got.get(),
                previous.get()
            ),
        }
    }
}

impl std::error::Error for EpochError {}

/// A track's epochs, in order. Session time and the sample count both only
/// move forward through it, so every sample has exactly one session time and
/// every session time at most one sample.
///
/// Epoch numbers only go up. Each epoch opened is numbered one above the
/// newest; a rebuilt timeline may skip numbers, where an epoch held no audio
/// and so left no anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackTimeline {
    track: TrackId,
    epochs: Vec<Epoch>,
    /// The number the first epoch opened gets, while there's none.
    first: EpochId,
}

impl TrackTimeline {
    /// A track with no epochs yet. Its first epoch is numbered 0.
    #[must_use]
    pub const fn new(track: TrackId) -> Self {
        Self {
            track,
            epochs: Vec::new(),
            first: EpochId::new(0),
        }
    }

    /// A track with no epochs here, whose earlier ones went up to `used`:
    /// its first epoch is numbered above it. For a resumed track whose
    /// epochs' times aren't known; [`Self::rebuild`] when they are.
    ///
    /// # Errors
    ///
    /// [`EpochError::TooManyEpochs`] if `used` is the last [`EpochId`].
    pub fn starting_after(track: TrackId, used: EpochId) -> Result<Self, EpochError> {
        Ok(Self {
            track,
            epochs: Vec::new(),
            first: used.next().ok_or(EpochError::TooManyEpochs)?,
        })
    }

    /// The timeline `anchors` describe, oldest first, so samples map to
    /// session time as they did when the epochs were opened. Numbers may
    /// skip, but only go up. The epochs report no overrun: that isn't
    /// stored.
    ///
    /// # Errors
    ///
    /// [`EpochError::NotAfter`] for an anchor numbered no higher than the
    /// one before it; otherwise as [`Self::follow`] refuses an epoch that
    /// starts before the previous one's audio ends, or whose first sample
    /// is before the previous one's.
    pub fn rebuild(
        track: TrackId,
        anchors: impl IntoIterator<Item = EpochAnchor>,
    ) -> Result<Self, EpochError> {
        let mut timeline = Self::new(track);
        for anchor in anchors {
            if let Some(previous) = timeline.epochs.last() {
                if anchor.id <= previous.id {
                    return Err(EpochError::NotAfter {
                        previous: previous.id,
                        got: anchor.id,
                    });
                }
                previous.check_followed_by(anchor.start, anchor.first_sample)?;
            }
            timeline.epochs.push(Epoch {
                id: anchor.id,
                start: anchor.start,
                first_sample: anchor.first_sample,
                rate: anchor.rate,
                drift: anchor.drift,
                overrun: Duration::ZERO,
            });
        }
        Ok(timeline)
    }

    /// The number the next epoch opened gets: one above the newest, or the
    /// first number while there's none.
    fn next_id(&self) -> Result<EpochId, EpochError> {
        match self.epochs.last() {
            Some(newest) => newest.id.next().ok_or(EpochError::TooManyEpochs),
            None => Ok(self.first),
        }
    }

    /// The track this timeline belongs to.
    #[must_use]
    pub const fn track(&self) -> TrackId {
        self.track
    }

    /// Every epoch, oldest first.
    #[must_use]
    pub fn epochs(&self) -> &[Epoch] {
        &self.epochs
    }

    /// The newest epoch: the one samples are arriving in now.
    #[must_use]
    pub fn current(&self) -> Option<&Epoch> {
        self.epochs.last()
    }

    /// Records that the stream (re)opened at `start`, with `first_sample` as
    /// the track's sample count so far, at `rate`. The new epoch keeps the
    /// drift of the one before (a stream reopened on the same device runs
    /// at the same rate), or none for the first; see
    /// [`Self::open_epoch_drifting`] to give it.
    ///
    /// Opening without a sample in between (a stream that fails straight
    /// away) is allowed: the earlier epoch just holds no samples.
    ///
    /// If the previous epoch's audio, timed at its rate and drift, runs
    /// past `start` (a device clock running faster than its drift says),
    /// the new epoch starts where that audio ends instead, so no two samples
    /// share a moment, and the overrun is reported. A real gap shorter than
    /// the error built up is absorbed: it shows as a smaller overrun, not
    /// as a gap. With the drift measured ([`crate::drift`]) that error stays
    /// within a few milliseconds.
    ///
    /// # Errors
    ///
    /// Refuses an epoch whose first sample is before the previous epoch's;
    /// one whose overrun is more than any real device's drift (1000 ppm of
    /// the previous epoch, plus 10 ms), which means a caller or clock bug;
    /// one whose previous epoch ends beyond the session clock; and one
    /// numbered past the last [`EpochId`]. The timeline is unchanged when it
    /// refuses.
    pub fn open_epoch(
        &mut self,
        start: SessionTime,
        first_sample: SampleIndex,
        rate: SampleRate,
    ) -> Result<OpenedEpoch, EpochError> {
        let drift = self.current().map_or(Drift::ZERO, Epoch::drift);
        self.open_epoch_drifting(start, first_sample, rate, drift)
    }

    /// [`Self::open_epoch`], with the new epoch's samples mapped under
    /// `drift`.
    ///
    /// # Errors
    ///
    /// As [`Self::open_epoch`].
    pub fn open_epoch_drifting(
        &mut self,
        start: SessionTime,
        first_sample: SampleIndex,
        rate: SampleRate,
        drift: Drift,
    ) -> Result<OpenedEpoch, EpochError> {
        let id = self.next_id()?;
        let mut opened = OpenedEpoch {
            id,
            start,
            overrun: Duration::ZERO,
        };
        if let Some(previous) = self.epochs.last() {
            if first_sample < previous.first_sample {
                return Err(EpochError::SampleWentBack {
                    previous: previous.first_sample,
                    first_sample,
                });
            }
            let previous_end = previous
                .time_of(first_sample)
                .ok_or(EpochError::TimeOverflow)?;
            if let Some(overrun) = previous_end.checked_duration_since(start)
                && !overrun.is_zero()
            {
                let length = previous_end
                    .checked_duration_since(previous.start)
                    .unwrap_or(Duration::ZERO);
                if overrun > max_overrun(length) {
                    return Err(EpochError::ImplausibleOverrun {
                        previous_end,
                        start,
                    });
                }
                opened.start = previous_end;
                opened.overrun = overrun;
            }
        }
        self.epochs.push(Epoch {
            id,
            start: opened.start,
            first_sample,
            rate,
            drift,
            overrun: opened.overrun,
        });
        Ok(opened)
    }

    /// Opens an epoch at `first_sample` that follows straight on from the
    /// current one, at its rate, with its samples from there on mapped
    /// under `drift`: it starts when the current epoch says `first_sample`
    /// plays, so there's no gap between them. For correcting drift, while
    /// the audio runs on unbroken.
    ///
    /// # Errors
    ///
    /// [`EpochError::NoEpoch`] if there's no epoch to follow on from;
    /// otherwise as [`Self::open_epoch`]. The timeline is unchanged when it
    /// refuses.
    pub fn retime(
        &mut self,
        first_sample: SampleIndex,
        drift: Drift,
    ) -> Result<OpenedEpoch, EpochError> {
        let current = self.current().ok_or(EpochError::NoEpoch)?;
        let rate = current.rate;
        if first_sample < current.first_sample {
            return Err(EpochError::SampleWentBack {
                previous: current.first_sample,
                first_sample,
            });
        }
        let start = current
            .time_of(first_sample)
            .ok_or(EpochError::TimeOverflow)?;
        self.open_epoch_drifting(start, first_sample, rate, drift)
    }

    /// A timeline that follows another from `epoch` on: its first epoch,
    /// whatever its number. For a thread that starts following a track
    /// partway, such as one resumed above earlier epochs.
    #[must_use]
    pub fn following(track: TrackId, epoch: &Epoch) -> Self {
        Self {
            track,
            epochs: vec![*epoch],
            first: epoch.id,
        }
    }

    /// Adds `epoch`, which another timeline of this track opened, so this
    /// one maps samples to session time the same way: for a thread that
    /// follows a track the recorder is timing. Each epoch must come once,
    /// in order.
    ///
    /// # Errors
    ///
    /// [`EpochError::NotNext`] unless `epoch` is the next this timeline
    /// would open; otherwise as [`Self::open_epoch`] would refuse it, except
    /// that an epoch starting before the previous one's audio ended is
    /// always [`EpochError::ImplausibleOverrun`]: the timeline that opened
    /// it would have moved it. The timeline is unchanged when it refuses.
    pub fn follow(&mut self, epoch: &Epoch) -> Result<(), EpochError> {
        let expected = self.next_id()?;
        if epoch.id != expected {
            return Err(EpochError::NotNext {
                expected,
                got: epoch.id,
            });
        }
        if let Some(previous) = self.epochs.last() {
            previous.check_followed_by(epoch.start, epoch.first_sample)?;
        }
        self.epochs.push(*epoch);
        Ok(())
    }

    /// When the samples in `range` were heard: from the session time of its
    /// first sample to the end of its last, timed by the epoch that last
    /// sample is in. `None` if the range is empty, starts before the first
    /// epoch, or the time overflows.
    ///
    /// A range that runs across epochs (audio either side of a reopened
    /// stream) spans the gap between them as well.
    #[must_use]
    pub fn span_of(&self, range: SampleRange) -> Option<(SessionTime, SessionTime)> {
        if range.is_empty() {
            return None;
        }
        let last = SampleIndex::new(range.end().get() - 1);
        let start = self.time_of(range.start())?;
        let end = self.epoch_of(last)?.time_of(range.end())?;
        Some((start, end))
    }

    /// The epoch `sample` belongs to, or `None` if it's before the first.
    #[must_use]
    pub fn epoch_of(&self, sample: SampleIndex) -> Option<&Epoch> {
        let after = self.epochs.partition_point(|e| e.first_sample <= sample);
        self.epochs.get(after.checked_sub(1)?)
    }

    /// The session time `sample` plays at, or `None` if it's before the first
    /// epoch or the time overflows.
    #[must_use]
    pub fn time_of(&self, sample: SampleIndex) -> Option<SessionTime> {
        self.epoch_of(sample)?.time_of(sample)
    }

    /// The sample playing at `time`, or `None` if `time` is before the first
    /// epoch or in a gap between two.
    ///
    /// The newest epoch runs on without end, so an answer past the audio
    /// captured so far is provisional: if the stream reopens, that time may
    /// fall in a gap instead. Store moments as [`SessionTime`] and resolve
    /// them to samples when needed, rather than keeping the sample.
    #[must_use]
    pub fn sample_at(&self, time: SessionTime) -> Option<SampleIndex> {
        let after = self.epochs.partition_point(|e| e.start <= time);
        let index = after.checked_sub(1)?;
        let sample = self.epochs.get(index)?.sample_at(time)?;
        match self.epochs.get(after) {
            Some(next) if sample >= next.first_sample => None,
            _ => Some(sample),
        }
    }

    /// The gaps between epochs, oldest first. Epochs that follow straight on
    /// from the one before have no gap.
    pub fn gaps(&self) -> impl Iterator<Item = Gap> + '_ {
        self.epochs.windows(2).filter_map(|pair| {
            let [previous, next] = pair else { return None };
            let from = previous.time_of(next.first_sample)?;
            (next.start > from).then_some(Gap {
                after: previous.id,
                from,
                to: next.start,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::SampleCount;
    use proptest::prelude::*;

    const SPEECH: SampleRate = SampleRate::SPEECH;

    fn t(nanos: u64) -> SessionTime {
        SessionTime::from_nanos(nanos)
    }

    fn s(index: u64) -> SampleIndex {
        SampleIndex::new(index)
    }

    #[test]
    fn empty_timeline_maps_nothing() {
        let timeline = TrackTimeline::new(TrackId::new(1));
        assert_eq!(timeline.track(), TrackId::new(1));
        assert!(timeline.epochs().is_empty());
        assert_eq!(timeline.current(), None);
        assert_eq!(timeline.time_of(s(0)), None);
        assert_eq!(timeline.sample_at(t(0)), None);
        assert_eq!(timeline.gaps().count(), 0);
    }

    #[test]
    fn one_epoch_maps_at_the_rate() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        let opened = timeline.open_epoch(t(1_000_000_000), s(0), SPEECH).unwrap();
        let id = opened.id;
        assert_eq!(
            opened,
            OpenedEpoch {
                id: EpochId::new(0),
                start: t(1_000_000_000),
                overrun: Duration::ZERO
            }
        );
        assert_eq!(timeline.current().map(Epoch::id), Some(id));
        // 16 kHz: one sample every 62.5 µs.
        assert_eq!(timeline.time_of(s(0)), Some(t(1_000_000_000)));
        assert_eq!(timeline.time_of(s(1)), Some(t(1_000_062_500)));
        assert_eq!(timeline.time_of(s(16_000)), Some(t(2_000_000_000)));
        assert_eq!(timeline.sample_at(t(999_999_999)), None);
        assert_eq!(timeline.sample_at(t(1_000_000_000)), Some(s(0)));
        assert_eq!(timeline.sample_at(t(1_000_062_499)), Some(s(0)));
        assert_eq!(timeline.sample_at(t(1_000_062_500)), Some(s(1)));
    }

    #[test]
    fn first_epoch_may_start_mid_count() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(100), SPEECH).unwrap();
        assert_eq!(timeline.time_of(s(99)), None);
        assert_eq!(timeline.time_of(s(100)), Some(t(0)));
        assert_eq!(timeline.epoch_of(s(99)), None);
    }

    #[test]
    fn reopened_stream_leaves_a_gap() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        // One second captured, then the stream reopens at 1.5 s.
        let second = timeline
            .open_epoch(t(1_500_000_000), s(16_000), SPEECH)
            .unwrap();
        assert_eq!(
            second,
            OpenedEpoch {
                id: EpochId::new(1),
                start: t(1_500_000_000),
                overrun: Duration::ZERO
            }
        );
        assert_eq!(timeline.time_of(s(15_999)), Some(t(999_937_500)));
        assert_eq!(timeline.time_of(s(16_000)), Some(t(1_500_000_000)));
        assert_eq!(timeline.sample_at(t(999_937_500)), Some(s(15_999)));
        assert_eq!(timeline.sample_at(t(999_999_999)), Some(s(15_999)));
        assert_eq!(timeline.sample_at(t(1_000_000_000)), None);
        assert_eq!(timeline.sample_at(t(1_499_999_999)), None);
        assert_eq!(timeline.sample_at(t(1_500_000_000)), Some(s(16_000)));
        let gaps: Vec<Gap> = timeline.gaps().collect();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].after(), EpochId::new(0));
        assert_eq!(gaps[0].from(), t(1_000_000_000));
        assert_eq!(gaps[0].to(), t(1_500_000_000));
        assert_eq!(gaps[0].duration(), Duration::from_millis(500));
        assert_eq!(
            timeline.epoch_of(s(15_999)).map(Epoch::id),
            Some(EpochId::new(0))
        );
        assert_eq!(
            timeline.epoch_of(s(16_000)).map(Epoch::id),
            Some(EpochId::new(1))
        );
    }

    #[test]
    fn seamless_reopen_has_no_gap() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        timeline
            .open_epoch(t(1_000_000_000), s(16_000), SPEECH)
            .unwrap();
        assert_eq!(timeline.gaps().count(), 0);
        assert_eq!(timeline.sample_at(t(999_999_999)), Some(s(15_999)));
        assert_eq!(timeline.sample_at(t(1_000_000_000)), Some(s(16_000)));
    }

    #[test]
    fn empty_epoch_is_allowed() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        timeline.open_epoch(t(10), s(0), SPEECH).unwrap();
        timeline.open_epoch(t(10), s(0), SPEECH).unwrap();
        assert_eq!(timeline.epochs().len(), 3);
        assert_eq!(
            timeline.epoch_of(s(0)).map(Epoch::id),
            Some(EpochId::new(2))
        );
        assert_eq!(timeline.time_of(s(0)), Some(t(10)));
        assert_eq!(timeline.sample_at(t(5)), None);
        assert_eq!(timeline.sample_at(t(10)), Some(s(0)));
        let gaps: Vec<Gap> = timeline.gaps().collect();
        assert_eq!(gaps.len(), 1);
        assert_eq!(
            (gaps[0].after(), gaps[0].from(), gaps[0].to()),
            (EpochId::new(0), t(0), t(10))
        );
    }

    #[test]
    fn rate_can_change_between_epochs() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        let rate = SampleRate::new(48_000).unwrap();
        timeline
            .open_epoch(t(1_000_000_000), s(16_000), rate)
            .unwrap();
        assert_eq!(timeline.time_of(s(64_000)), Some(t(2_000_000_000)));
        assert_eq!(timeline.sample_at(t(2_000_000_000)), Some(s(64_000)));
    }

    #[test]
    fn refuses_a_sample_count_that_goes_back() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(100), SPEECH).unwrap();
        let before = timeline.clone();
        assert_eq!(
            timeline.open_epoch(t(1_000_000_000), s(99), SPEECH),
            Err(EpochError::SampleWentBack {
                previous: s(100),
                first_sample: s(99)
            })
        );
        assert_eq!(timeline, before);
    }

    #[test]
    fn a_fast_device_clock_pushes_the_next_epoch_back() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        // A device 100 ppm fast delivers 57_605_760 samples in an hour (60 min
        // 0.36 s at the nominal rate); the stream reopens 100 ms after the hour.
        let opened = timeline
            .open_epoch(t(3_600_100_000_000), s(57_605_760), SPEECH)
            .unwrap();
        assert_eq!(
            opened,
            OpenedEpoch {
                id: EpochId::new(1),
                start: t(3_600_360_000_000),
                overrun: Duration::from_millis(260),
            }
        );
        assert_eq!(
            timeline.current().map(Epoch::start),
            Some(t(3_600_360_000_000))
        );
        // Still one moment per sample, and no gap.
        assert_eq!(timeline.gaps().count(), 0);
        let last = s(57_605_759);
        assert_eq!(
            timeline.sample_at(timeline.time_of(last).unwrap()),
            Some(last)
        );
        let first = s(57_605_760);
        assert_eq!(
            timeline.sample_at(timeline.time_of(first).unwrap()),
            Some(first)
        );
    }

    #[test]
    fn the_epoch_keeps_its_overrun() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        timeline
            .open_epoch(t(999_000_000), s(16_000), SPEECH)
            .unwrap();
        let overruns: Vec<Duration> = timeline.epochs().iter().map(Epoch::overrun).collect();
        assert_eq!(overruns, [Duration::ZERO, Duration::from_millis(1)]);
    }

    #[test]
    fn refuses_an_overrun_drift_cannot_explain() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        let before = timeline.clone();
        // 1000 s of audio allows 1 s + 10 ms of overrun, and no more.
        let end = 1_000_000_000_000;
        let allowed = t(end - 1_010_000_000);
        let refused = t(end - 1_010_000_001);
        assert_eq!(
            timeline
                .clone()
                .open_epoch(allowed, s(16_000_000), SPEECH)
                .map(|o| o.overrun),
            Ok(Duration::from_millis(1_010))
        );
        assert_eq!(
            timeline.open_epoch(refused, s(16_000_000), SPEECH),
            Err(EpochError::ImplausibleOverrun {
                previous_end: t(end),
                start: refused
            })
        );
        assert_eq!(timeline, before);
    }

    #[test]
    fn refuses_a_start_well_before_an_empty_previous_epoch() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(1_000_000_000), s(0), SPEECH).unwrap();
        // An empty epoch still allows the 10 ms minimum.
        assert!(
            timeline
                .clone()
                .open_epoch(t(990_000_000), s(0), SPEECH)
                .is_ok()
        );
        assert_eq!(
            timeline.open_epoch(t(989_999_999), s(0), SPEECH),
            Err(EpochError::ImplausibleOverrun {
                previous_end: t(1_000_000_000),
                start: t(989_999_999),
            })
        );
    }

    #[test]
    fn an_overrun_of_one_nanosecond_is_reported() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        let opened = timeline
            .open_epoch(t(999_999_999), s(16_000), SPEECH)
            .unwrap();
        assert_eq!(opened.start, t(1_000_000_000));
        assert_eq!(opened.overrun, Duration::from_nanos(1));
    }

    #[test]
    fn refuses_an_epoch_after_an_unrepresentable_end() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(u64::MAX - 10), s(0), SPEECH).unwrap();
        assert_eq!(
            timeline.open_epoch(t(u64::MAX), s(16_000), SPEECH),
            Err(EpochError::TimeOverflow)
        );
        assert_eq!(timeline.epochs().len(), 1);
    }

    #[test]
    fn errors_describe_themselves() {
        let text = EpochError::SampleWentBack {
            previous: s(5),
            first_sample: s(3),
        }
        .to_string();
        assert!(text.contains('3') && text.contains('5'), "{text}");
        let text = EpochError::ImplausibleOverrun {
            previous_end: t(9),
            start: t(7),
        }
        .to_string();
        assert!(text.contains("7 ns") && text.contains("9 ns"), "{text}");
        assert!(!EpochError::TimeOverflow.to_string().is_empty());
        assert!(!EpochError::TooManyEpochs.to_string().is_empty());
    }

    /// An epoch's anchor is its number, start, first sample and rate.
    #[test]
    fn an_epoch_s_anchor_is_what_times_it() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(5), s(7), SPEECH).unwrap();
        assert_eq!(
            timeline.current().map(Epoch::anchor),
            Some(EpochAnchor {
                id: EpochId::new(0),
                start: t(5),
                first_sample: s(7),
                rate: SPEECH,
                drift: Drift::ZERO,
            })
        );
    }

    /// A rebuilt timeline maps samples as the one its anchors came from,
    /// and gaps in the numbering (epochs that held no audio) are kept.
    #[test]
    fn a_rebuilt_timeline_times_samples_as_before() {
        let mut timeline = TrackTimeline::new(TrackId::new(3));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        timeline
            .open_epoch(t(1_500_000_000), s(16_000), SPEECH)
            .unwrap();
        timeline
            .open_epoch(t(2_000_000_000), s(16_000), SPEECH)
            .unwrap();
        // Epoch 1 held no audio, so nothing stored its anchor.
        let anchors = [timeline.epochs()[0].anchor(), timeline.epochs()[2].anchor()];
        let rebuilt = TrackTimeline::rebuild(TrackId::new(3), anchors).unwrap();
        assert_eq!(rebuilt.track(), TrackId::new(3));
        let ids: Vec<EpochId> = rebuilt.epochs().iter().map(Epoch::id).collect();
        assert_eq!(ids, [EpochId::new(0), EpochId::new(2)]);
        for sample in [0, 15_999, 16_000, 20_000] {
            assert_eq!(rebuilt.time_of(s(sample)), timeline.time_of(s(sample)));
        }
        let gaps: Vec<(SessionTime, SessionTime)> =
            rebuilt.gaps().map(|g| (g.from(), g.to())).collect();
        assert_eq!(gaps, [(t(1_000_000_000), t(2_000_000_000))]);
    }

    /// An epoch opened on a rebuilt timeline is numbered above its newest,
    /// and is checked against it.
    #[test]
    fn a_rebuilt_timeline_opens_above_its_newest_epoch() {
        let anchor = EpochAnchor {
            id: EpochId::new(4),
            start: t(0),
            first_sample: s(0),
            rate: SPEECH,
            drift: Drift::ZERO,
        };
        let mut rebuilt = TrackTimeline::rebuild(TrackId::new(0), [anchor]).unwrap();
        // Its second of audio ends at 1 s: an epoch requested well before
        // that is refused, as on the timeline that opened it.
        assert_eq!(
            rebuilt.clone().open_epoch(t(0), s(16_000), SPEECH),
            Err(EpochError::ImplausibleOverrun {
                previous_end: t(1_000_000_000),
                start: t(0)
            })
        );
        let opened = rebuilt
            .open_epoch(t(3_000_000_000), s(16_000), SPEECH)
            .unwrap();
        assert_eq!(opened.id, EpochId::new(5));
        assert_eq!(rebuilt.epochs()[0].overrun(), Duration::ZERO);
    }

    /// Anchors that couldn't have come from one timeline are refused.
    #[test]
    fn rebuilding_refuses_anchors_out_of_order() {
        let anchor = |id: u32, start: u64, first: u64| EpochAnchor {
            id: EpochId::new(id),
            start: t(start),
            first_sample: s(first),
            rate: SPEECH,
            drift: Drift::ZERO,
        };
        let track = TrackId::new(0);
        assert_eq!(
            TrackTimeline::rebuild(track, [anchor(2, 0, 0), anchor(2, 10, 0)]),
            Err(EpochError::NotAfter {
                previous: EpochId::new(2),
                got: EpochId::new(2)
            })
        );
        assert_eq!(
            TrackTimeline::rebuild(track, [anchor(0, 0, 100), anchor(1, 10, 99)]),
            Err(EpochError::SampleWentBack {
                previous: s(100),
                first_sample: s(99)
            })
        );
        // A second of audio ends at 1 s; the next starts a nanosecond early.
        assert_eq!(
            TrackTimeline::rebuild(track, [anchor(0, 0, 0), anchor(1, 999_999_999, 16_000)]),
            Err(EpochError::ImplausibleOverrun {
                previous_end: t(1_000_000_000),
                start: t(999_999_999)
            })
        );
        assert_eq!(
            TrackTimeline::rebuild(
                track,
                [anchor(0, u64::MAX - 10, 0), anchor(1, u64::MAX, 16_000)]
            ),
            Err(EpochError::TimeOverflow)
        );
        assert_eq!(
            TrackTimeline::rebuild(track, [anchor(0, 0, 0), anchor(1, 1_000_000_000, 16_000)])
                .map(|t| t.epochs().len()),
            Ok(2)
        );
    }

    /// A timeline resumed above an epoch numbers its first one above it.
    #[test]
    fn a_timeline_started_after_an_epoch_numbers_above_it() {
        let mut timeline = TrackTimeline::starting_after(TrackId::new(0), EpochId::new(6)).unwrap();
        assert!(timeline.epochs().is_empty());
        assert_eq!(
            timeline.open_epoch(t(0), s(9), SPEECH).map(|o| o.id),
            Ok(EpochId::new(7))
        );
        assert_eq!(
            timeline.open_epoch(t(1), s(9), SPEECH).map(|o| o.id),
            Ok(EpochId::new(8))
        );
        assert_eq!(
            TrackTimeline::starting_after(TrackId::new(0), EpochId::new(u32::MAX)),
            Err(EpochError::TooManyEpochs)
        );
    }

    /// The last epoch number can't be followed by another, opened or
    /// followed.
    #[test]
    fn no_epoch_comes_after_the_last_number() {
        let anchor = EpochAnchor {
            id: EpochId::new(u32::MAX),
            start: t(0),
            first_sample: s(0),
            rate: SPEECH,
            drift: Drift::ZERO,
        };
        let mut timeline = TrackTimeline::rebuild(TrackId::new(0), [anchor]).unwrap();
        assert_eq!(
            timeline.open_epoch(t(1), s(0), SPEECH),
            Err(EpochError::TooManyEpochs)
        );
        let mut follower = TrackTimeline::following(TrackId::new(0), &timeline.epochs()[0]);
        let other = TrackTimeline::rebuild(TrackId::new(0), [anchor]).unwrap();
        assert_eq!(
            follower.follow(&other.epochs()[0]),
            Err(EpochError::TooManyEpochs)
        );
    }

    /// A follower that starts partway takes the epoch it starts at, whatever
    /// its number, and then the next in turn.
    #[test]
    fn a_follower_can_start_at_any_epoch() {
        let mut timeline = TrackTimeline::starting_after(TrackId::new(1), EpochId::new(2)).unwrap();
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        timeline
            .open_epoch(t(2_000_000_000), s(16_000), SPEECH)
            .unwrap();
        let [first, second] = timeline.epochs() else {
            panic!("{timeline:?}")
        };
        let mut follower = TrackTimeline::following(TrackId::new(1), first);
        follower.follow(second).unwrap();
        assert_eq!(follower, timeline);
    }

    /// A reopened stream keeps the drift its device was measured at; the
    /// first epoch has none.
    #[test]
    fn a_new_epoch_keeps_the_drift_before_it() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(0), SPEECH).unwrap();
        let fast = Drift::from_ppb(100_000).unwrap();
        timeline.retime(s(16_000), fast).unwrap();
        timeline
            .open_epoch(t(5_000_000_000), s(32_000), SPEECH)
            .unwrap();
        let drifts: Vec<Drift> = timeline.epochs().iter().map(Epoch::drift).collect();
        assert_eq!(drifts, [Drift::ZERO, fast, fast]);
    }

    /// A retimed epoch starts where the one before says its first sample
    /// plays, so there's no gap, and maps its samples under its own drift.
    #[test]
    fn a_retimed_epoch_follows_straight_on() {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(1_000), s(0), SPEECH).unwrap();
        let fast = Drift::from_ppb(100_000).unwrap();
        let opened = timeline.retime(s(16_000), fast).unwrap();
        assert_eq!(
            opened,
            OpenedEpoch {
                id: EpochId::new(1),
                start: t(1_000_001_000),
                overrun: Duration::ZERO
            }
        );
        assert_eq!(timeline.gaps().count(), 0);
        // 16,000 samples from a device 100 ppm fast take 1 s / 1.0001.
        assert_eq!(timeline.time_of(s(32_000)), Some(t(1_999_901_010)));
        assert_eq!(timeline.sample_at(t(1_999_901_010)), Some(s(32_000)));
        assert_eq!(timeline.epochs()[1].anchor().drift, fast);
    }

    /// Retiming needs an epoch to follow on from, and a sample count that
    /// doesn't go back; refused, the timeline is unchanged.
    #[test]
    fn retiming_refuses_what_it_cant_follow_on_from() {
        let mut empty = TrackTimeline::new(TrackId::new(0));
        assert_eq!(empty.retime(s(0), Drift::ZERO), Err(EpochError::NoEpoch));
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        timeline.open_epoch(t(0), s(100), SPEECH).unwrap();
        let before = timeline.clone();
        assert_eq!(
            timeline.retime(s(99), Drift::ZERO),
            Err(EpochError::SampleWentBack {
                previous: s(100),
                first_sample: s(99)
            })
        );
        let mut late = TrackTimeline::new(TrackId::new(0));
        late.open_epoch(t(u64::MAX - 10), s(0), SPEECH).unwrap();
        assert_eq!(
            late.retime(s(16_000), Drift::ZERO),
            Err(EpochError::TimeOverflow)
        );
        assert_eq!(timeline, before);
        assert!(!EpochError::NoEpoch.to_string().is_empty());
    }

    /// The anchor error names both epochs.
    #[test]
    fn not_after_describes_itself() {
        let text = EpochError::NotAfter {
            previous: EpochId::new(8),
            got: EpochId::new(3),
        }
        .to_string();
        assert!(text.contains('8') && text.contains('3'), "{text}");
    }

    /// One epoch to open: how long after the previous epoch's audio it
    /// starts, how many samples it holds, its rate and its drift.
    #[derive(Debug, Clone)]
    struct Opening {
        gap_nanos: u64,
        early_nanos: u64,
        samples: u64,
        rate: SampleRate,
        drift: Drift,
    }

    fn any_drift() -> impl Strategy<Value = Drift> {
        prop_oneof![
            2 => Just(Drift::ZERO),
            1 => (-Drift::MAX_PPB..=Drift::MAX_PPB).prop_map(|ppb| Drift::from_ppb(ppb).unwrap()),
        ]
    }

    fn any_rate() -> impl Strategy<Value = SampleRate> {
        prop_oneof![
            4 => Just(SPEECH),
            1 => Just(SampleRate::new(44_100).unwrap()),
            1 => (1..=SampleRate::MAX_HZ).prop_map(|hz| SampleRate::new(hz).unwrap()),
        ]
    }

    fn any_openings() -> impl Strategy<Value = (u64, u64, Vec<Opening>)> {
        let opening = (
            prop_oneof![Just(0u64), 0..10_000_000_000u64],
            // Usually on time; sometimes requested early, as a fast device
            // clock would.
            prop_oneof![4 => Just(0u64), 1 => 0..=MIN_OVERRUN_ALLOWED_NANOS],
            prop_oneof![Just(0u64), 0..10_000_000u64],
            any_rate(),
            any_drift(),
        )
            .prop_map(|(gap_nanos, early_nanos, samples, rate, drift)| Opening {
                gap_nanos,
                early_nanos,
                samples,
                rate,
                drift,
            });
        (
            0..1_000_000_000_000u64,
            0..1_000_000u64,
            prop::collection::vec(opening, 1..8),
        )
    }

    /// The gap actually left before an opening: what was asked for, less
    /// any part of it the device clock's overrun used up.
    fn effective_gap(opening: &Opening) -> u64 {
        opening.gap_nanos.saturating_sub(opening.early_nanos)
    }

    /// Opens every epoch, each requested `gap_nanos - early_nanos` after the
    /// previous one's audio ended, and checks where `open_epoch` put it.
    /// Returns the timeline and each epoch's start and sample range.
    fn build(
        first_start: u64,
        first_sample: u64,
        openings: &[Opening],
    ) -> (TrackTimeline, Vec<(SessionTime, SampleIndex, SampleIndex)>) {
        let mut timeline = TrackTimeline::new(TrackId::new(0));
        let mut spans = Vec::new();
        let mut start = t(first_start);
        let mut first = s(first_sample);
        for (i, opening) in openings.iter().enumerate() {
            let mut requested = start;
            if i > 0 {
                let ended = start;
                let asked = ended.as_nanos() + opening.gap_nanos;
                requested = t(asked.saturating_sub(opening.early_nanos));
                start = t(ended.as_nanos() + effective_gap(opening));
            }
            let opened = timeline
                .open_epoch_drifting(requested, first, opening.rate, opening.drift)
                .unwrap();
            assert_eq!(opened.start, start);
            assert_eq!(
                opened.overrun,
                start.checked_duration_since(requested).unwrap(),
                "the overrun is how far the start moved"
            );
            let end = first
                .checked_add(SampleCount::new(opening.samples))
                .unwrap();
            spans.push((start, first, end));
            start = start
                .checked_add(
                    opening
                        .drift
                        .duration_of(SampleCount::new(opening.samples), opening.rate)
                        .unwrap(),
                )
                .unwrap();
            first = end;
        }
        (timeline, spans)
    }

    proptest! {
        /// Within an epoch, sample → session time → sample is exact.
        #[test]
        fn round_trips_within_every_epoch(
            (first_start, first_sample, openings) in any_openings(),
            picks in prop::collection::vec(any::<prop::sample::Index>(), 1..16),
        ) {
            let (timeline, spans) = build(first_start, first_sample, &openings);
            for (epoch, (_, first, end)) in timeline.epochs().iter().zip(&spans) {
                let len = end.get() - first.get();
                if len == 0 {
                    continue;
                }
                for pick in &picks {
                    let sample = s(first.get() + pick.index(usize::try_from(len).unwrap()) as u64);
                    let time = timeline.time_of(sample).unwrap();
                    prop_assert_eq!(timeline.epoch_of(sample).map(Epoch::id), Some(epoch.id()));
                    prop_assert_eq!(timeline.sample_at(time), Some(sample));
                }
                // The epoch's last sample, the one most likely to round wrong.
                let last = s(end.get() - 1);
                prop_assert_eq!(timeline.sample_at(timeline.time_of(last).unwrap()), Some(last));
            }
        }

        /// The time between epochs is exactly the gap that was there, and no
        /// moment inside a gap maps to a sample.
        #[test]
        fn gaps_between_epochs_are_preserved(
            (first_start, first_sample, openings) in any_openings(),
            within in any::<prop::sample::Index>(),
        ) {
            let (timeline, spans) = build(first_start, first_sample, &openings);
            let expected: Vec<(EpochId, u64)> = openings
                .iter()
                .enumerate()
                .skip(1)
                .filter(|(_, o)| effective_gap(o) > 0)
                .map(|(i, o)| (EpochId::new(u32::try_from(i - 1).unwrap()), effective_gap(o)))
                .collect();
            let gaps: Vec<Gap> = timeline.gaps().collect();
            prop_assert_eq!(
                gaps.iter().map(|g| (g.after(), u64::try_from(g.duration().as_nanos()).unwrap()))
                    .collect::<Vec<_>>(),
                expected
            );
            for gap in &gaps {
                let next_index = usize::try_from(gap.after().get()).unwrap() + 1;
                let next = &spans[next_index];
                // The next epoch's first sample sits exactly the gap after the
                // previous epoch's audio ended.
                prop_assert_eq!(gap.to(), next.0);
                prop_assert_eq!(timeline.epochs()[next_index].time_of(next.1), Some(gap.to()));
                let len = usize::try_from(gap.duration().as_nanos()).unwrap();
                let inside = t(gap.from().as_nanos() + within.index(len) as u64);
                prop_assert_eq!(timeline.sample_at(inside), None);
                // An epoch with no samples has no audio to find; its first
                // sample belongs to whichever epoch opened next.
                if next.2 > next.1 {
                    prop_assert_eq!(timeline.time_of(next.1), Some(gap.to()));
                    prop_assert_eq!(timeline.sample_at(gap.to()), Some(next.1));
                }
            }
        }

        /// Session time never goes back as the sample count goes forward,
        /// across epoch boundaries too.
        #[test]
        fn time_is_monotonic_across_epochs(
            (first_start, first_sample, openings) in any_openings(),
        ) {
            let (timeline, spans) = build(first_start, first_sample, &openings);
            let mut previous: Option<SessionTime> = None;
            for (_, first, end) in &spans {
                let mut samples = vec![first.get(), end.get().saturating_sub(1)];
                samples.dedup();
                for sample in samples {
                    if sample < first.get() || sample >= end.get() {
                        continue;
                    }
                    let time = timeline.time_of(s(sample)).unwrap();
                    if let Some(previous) = previous {
                        prop_assert!(time > previous);
                    }
                    previous = Some(time);
                }
            }
        }

        /// Rebuilt from the anchors of the epochs that hold audio, a
        /// timeline times every sample as the one that opened them.
        #[test]
        fn a_rebuilt_timeline_times_every_sample_the_same(
            (first_start, first_sample, openings) in any_openings(),
            picks in prop::collection::vec(any::<prop::sample::Index>(), 1..16),
        ) {
            let (timeline, spans) = build(first_start, first_sample, &openings);
            // Only an epoch that held audio leaves an anchor; the newest is
            // kept anyway, as a resumed session's marks keep it.
            let last = timeline.epochs().len() - 1;
            let anchors: Vec<EpochAnchor> = timeline
                .epochs()
                .iter()
                .zip(&spans)
                .enumerate()
                .filter(|(i, (_, (_, first, end)))| end > first || *i == last)
                .map(|(_, (epoch, _))| epoch.anchor())
                .collect();
            let rebuilt = TrackTimeline::rebuild(TrackId::new(0), anchors).unwrap();
            for (_, first, end) in &spans {
                let len = end.get() - first.get();
                if len == 0 {
                    continue;
                }
                for pick in &picks {
                    let sample = s(first.get() + pick.index(usize::try_from(len).unwrap()) as u64);
                    prop_assert_eq!(rebuilt.time_of(sample), timeline.time_of(sample));
                    let time = rebuilt.time_of(sample).unwrap();
                    prop_assert_eq!(rebuilt.sample_at(time), Some(sample));
                }
            }
            prop_assert_eq!(
                rebuilt.current().map(Epoch::id),
                timeline.current().map(Epoch::id)
            );
        }
    }
}
