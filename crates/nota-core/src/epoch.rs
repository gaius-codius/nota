//! Track epochs: how a track's sample count maps to session time.
//!
//! A track's stream can stop and reopen during a session (a device change,
//! sleep, a stall). Each opening starts an epoch, which pins the next sample
//! to a session time. The sample count carries on across epochs, so the time
//! between them is a gap with no audio, and a mark made during it maps to no
//! sample.

use std::time::Duration;

use crate::ids::{EpochId, TrackId};
use crate::time::{SampleIndex, SampleRate, SessionTime};

/// One epoch of a track: from `first_sample` on, sample `s` plays at
/// `start + (s - first_sample) / rate`. It runs until the next epoch's first
/// sample, or for the newest epoch, open-ended.
///
/// Only a [`TrackTimeline`] makes epochs, so every epoch has been checked
/// against the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Epoch {
    id: EpochId,
    start: SessionTime,
    first_sample: SampleIndex,
    rate: SampleRate,
}

impl Epoch {
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

    /// The session time `sample` plays at, if this epoch's mapping reached
    /// it. `None` if it comes before the epoch or the time overflows. It
    /// doesn't check where the epoch ends; [`TrackTimeline::time_of`] does.
    #[must_use]
    pub fn time_of(&self, sample: SampleIndex) -> Option<SessionTime> {
        let offset = sample.checked_count_since(self.first_sample)?;
        self.start.checked_add(offset.duration_at(self.rate)?)
    }

    /// The sample playing at `time` under this epoch's mapping. `None` if
    /// `time` comes before the epoch. It doesn't check where the epoch ends;
    /// [`TrackTimeline::sample_at`] does.
    #[must_use]
    pub fn sample_at(&self, time: SessionTime) -> Option<SampleIndex> {
        let elapsed = time.checked_duration_since(self.start)?;
        let offset = crate::time::SampleCount::started_within(elapsed, self.rate)?;
        self.first_sample.checked_add(offset)
    }
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

/// A newly opened epoch, as [`TrackTimeline::open_epoch`] placed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenedEpoch {
    /// The new epoch.
    pub id: EpochId,
    /// Where it starts: the requested start, or later if the previous
    /// epoch's audio hadn't ended by then.
    pub start: SessionTime,
    /// How far the previous epoch's audio, timed at its nominal rate, ran
    /// past the requested start. Zero normally; more means the device's
    /// clock ran fast against the session clock, and this is the drift it
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
    /// The previous epoch's end can't be expressed in session time.
    TimeOverflow,
    /// The track already has as many epochs as an [`EpochId`] can number.
    TooManyEpochs,
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
            Self::TimeOverflow => f.write_str("the previous epoch ends beyond the session clock"),
            Self::TooManyEpochs => f.write_str("the track has run out of epoch numbers"),
        }
    }
}

impl std::error::Error for EpochError {}

/// A track's epochs, in order. Session time and the sample count both only
/// move forward through it, so every sample has exactly one session time and
/// every session time at most one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackTimeline {
    track: TrackId,
    epochs: Vec<Epoch>,
}

impl TrackTimeline {
    /// A track with no epochs yet.
    #[must_use]
    pub const fn new(track: TrackId) -> Self {
        Self {
            track,
            epochs: Vec::new(),
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
    /// the track's sample count so far, at `rate`.
    ///
    /// Opening without a sample in between (a stream that fails straight
    /// away) is allowed: the earlier epoch just holds no samples.
    ///
    /// If the previous epoch's audio, timed at its nominal rate, runs past
    /// `start` (a device clock running fast), the new epoch starts where
    /// that audio ends instead, so no two samples share a moment, and the
    /// overrun is reported. Correcting drift itself is the clock's later
    /// work; this keeps the timeline consistent meanwhile.
    ///
    /// # Errors
    ///
    /// Refuses an epoch whose first sample is before the previous epoch's,
    /// one whose previous epoch ends beyond the session clock, and one past
    /// the last [`EpochId`]. The timeline is unchanged when it refuses.
    pub fn open_epoch(
        &mut self,
        start: SessionTime,
        first_sample: SampleIndex,
        rate: SampleRate,
    ) -> Result<OpenedEpoch, EpochError> {
        let id =
            EpochId::new(u32::try_from(self.epochs.len()).map_err(|_| EpochError::TooManyEpochs)?);
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
                opened.start = previous_end;
                opened.overrun = overrun;
            }
        }
        self.epochs.push(Epoch {
            id,
            start: opened.start,
            first_sample,
            rate,
        });
        Ok(opened)
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
        assert!(!EpochError::TimeOverflow.to_string().is_empty());
        assert!(!EpochError::TooManyEpochs.to_string().is_empty());
    }

    /// One epoch to open: how long after the previous epoch's audio it
    /// starts, how many samples it holds, and its rate.
    #[derive(Debug, Clone)]
    struct Opening {
        gap_nanos: u64,
        early_nanos: u64,
        samples: u64,
        rate: SampleRate,
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
            prop_oneof![4 => Just(0u64), 1 => 0..1_000_000_000u64],
            prop_oneof![Just(0u64), 0..10_000_000u64],
            any_rate(),
        )
            .prop_map(|(gap_nanos, early_nanos, samples, rate)| Opening {
                gap_nanos,
                early_nanos,
                samples,
                rate,
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
            let opened = timeline.open_epoch(requested, first, opening.rate).unwrap();
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
                    SampleCount::new(opening.samples)
                        .duration_at(opening.rate)
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
    }
}
