//! Typed time units: session time, sample positions and counts, and rates.
//!
//! Session time is whole nanoseconds, never a float, so converting between
//! samples and time is exact integer arithmetic. Sample to time rounds up and
//! time to sample rounds down; with a rate of at most [`SampleRate::MAX_HZ`]
//! that makes the round trip sample → time → sample exact, and gives every
//! sample its own nanosecond.

use std::num::NonZeroU32;
use std::time::Duration;

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// A point on the session clock: the time since the session started, in
/// whole nanoseconds. Monotonic; marks, notes and citations are stored in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SessionTime(u64);

impl SessionTime {
    /// The start of the session.
    pub const ZERO: Self = Self(0);

    /// The session time `nanos` nanoseconds after the start.
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds since the start of the session.
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// The session time `elapsed` after the start, or `None` if it doesn't
    /// fit (about 584 years).
    #[must_use]
    pub fn from_elapsed(elapsed: Duration) -> Option<Self> {
        u64::try_from(elapsed.as_nanos()).ok().map(Self)
    }

    /// The time since the start of the session.
    #[must_use]
    pub const fn elapsed(self) -> Duration {
        Duration::from_nanos(self.0)
    }

    /// This time plus `by`, or `None` on overflow.
    #[must_use]
    pub fn checked_add(self, by: Duration) -> Option<Self> {
        let by = u64::try_from(by.as_nanos()).ok()?;
        self.0.checked_add(by).map(Self)
    }

    /// The time from `earlier` to `self`, or `None` if `earlier` is later.
    #[must_use]
    pub fn checked_duration_since(self, earlier: Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_nanos)
    }
}

/// A sampling rate in hertz: never zero, and at most [`SampleRate::MAX_HZ`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SampleRate(NonZeroU32);

impl SampleRate {
    /// The highest rate accepted. Well above any audio rate, and low enough
    /// that consecutive samples are always at least one nanosecond apart.
    pub const MAX_HZ: u32 = 1_000_000;

    /// 16 kHz, the rate nota records and transcribes at.
    pub const SPEECH: Self = match NonZeroU32::new(16_000) {
        Some(hz) => Self(hz),
        // Evaluated at compile time, so this arm can't be reached at runtime.
        None => unreachable!(),
    };

    /// A rate of `hz` hertz, or `None` if it's zero or above [`Self::MAX_HZ`].
    #[must_use]
    pub const fn new(hz: u32) -> Option<Self> {
        if hz > Self::MAX_HZ {
            return None;
        }
        match NonZeroU32::new(hz) {
            Some(hz) => Some(Self(hz)),
            None => None,
        }
    }

    /// The rate in hertz.
    #[must_use]
    pub const fn hz(self) -> u32 {
        self.0.get()
    }

    fn hz_wide(self) -> u128 {
        u128::from(self.0.get())
    }
}

/// The position of one sample in a track, counted from the track's first
/// sample. It keeps counting across epochs: a reopened stream continues the
/// count, and the gap shows only in session time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SampleIndex(u64);

impl SampleIndex {
    /// The track's first sample.
    pub const ZERO: Self = Self(0);

    /// The sample at position `index`.
    #[must_use]
    pub const fn new(index: u64) -> Self {
        Self(index)
    }

    /// The position as a number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The sample `count` samples later, or `None` on overflow.
    #[must_use]
    pub fn checked_add(self, count: SampleCount) -> Option<Self> {
        self.0.checked_add(count.0).map(Self)
    }

    /// The number of samples from `earlier` up to (not including) `self`, or
    /// `None` if `earlier` is later.
    #[must_use]
    pub fn checked_count_since(self, earlier: Self) -> Option<SampleCount> {
        self.0.checked_sub(earlier.0).map(SampleCount)
    }
}

/// A number of samples: a length, never a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SampleCount(u64);

impl SampleCount {
    /// No samples.
    pub const ZERO: Self = Self(0);

    /// `count` samples.
    #[must_use]
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    /// The count as a number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// How long this many samples last at `rate`, rounded up to a whole
    /// nanosecond. `None` if that doesn't fit in [`SessionTime`]'s range.
    #[must_use]
    pub fn duration_at(self, rate: SampleRate) -> Option<Duration> {
        let hz = rate.hz_wide();
        // Can't overflow: u64::MAX * 10^9 is far below u128::MAX.
        let nanos = (u128::from(self.0) * NANOS_PER_SEC).div_ceil(hz);
        u64::try_from(nanos).ok().map(Duration::from_nanos)
    }

    /// The number of whole sample periods at `rate` that have *started*
    /// within `elapsed`, given the rounding of [`Self::duration_at`]:
    /// the largest `n` with `n.duration_at(rate) <= elapsed`. `None` on
    /// overflow.
    #[must_use]
    pub fn started_within(elapsed: Duration, rate: SampleRate) -> Option<Self> {
        // Can't overflow: Duration's nanoseconds fit in 94 bits, the rate in 20.
        let count = elapsed.as_nanos() * rate.hz_wide() / NANOS_PER_SEC;
        u64::try_from(count).ok().map(Self)
    }
}

/// A half-open run of samples, `start..end`, in one track. `start <= end`
/// always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SampleRange {
    start: SampleIndex,
    end: SampleIndex,
}

impl SampleRange {
    /// The samples from `start` up to (not including) `end`, or `None` if
    /// `end` comes before `start`.
    #[must_use]
    pub fn new(start: SampleIndex, end: SampleIndex) -> Option<Self> {
        (start <= end).then_some(Self { start, end })
    }

    /// `len` samples from `start`, or `None` if the end overflows.
    #[must_use]
    pub fn starting_at(start: SampleIndex, len: SampleCount) -> Option<Self> {
        let end = start.checked_add(len)?;
        Some(Self { start, end })
    }

    /// The first sample in the range.
    #[must_use]
    pub const fn start(self) -> SampleIndex {
        self.start
    }

    /// The sample just after the range.
    #[must_use]
    pub const fn end(self) -> SampleIndex {
        self.end
    }

    /// The number of samples in the range.
    #[must_use]
    pub const fn len(self) -> SampleCount {
        SampleCount(self.end.0 - self.start.0)
    }

    /// Whether the range holds no samples.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.0 == self.end.0
    }

    /// Whether `sample` is in the range.
    #[must_use]
    pub fn contains(self, sample: SampleIndex) -> bool {
        self.start <= sample && sample < self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn any_rate() -> impl Strategy<Value = SampleRate> {
        prop_oneof![
            Just(SampleRate::SPEECH),
            Just(SampleRate::new(44_100).unwrap()),
            Just(SampleRate::new(48_000).unwrap()),
            (1..=SampleRate::MAX_HZ).prop_map(|hz| SampleRate::new(hz).unwrap()),
        ]
    }

    #[test]
    fn rate_rejects_zero_and_too_fast() {
        assert_eq!(SampleRate::new(0), None);
        assert_eq!(SampleRate::new(SampleRate::MAX_HZ + 1), None);
        assert_eq!(
            SampleRate::new(SampleRate::MAX_HZ).map(SampleRate::hz),
            Some(1_000_000)
        );
        assert_eq!(SampleRate::new(1).map(SampleRate::hz), Some(1));
        assert_eq!(SampleRate::SPEECH.hz(), 16_000);
    }

    #[test]
    fn speech_rate_durations_are_exact() {
        let rate = SampleRate::SPEECH;
        assert_eq!(
            SampleCount::new(16_000).duration_at(rate),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            SampleCount::new(1).duration_at(rate),
            Some(Duration::from_nanos(62_500))
        );
        assert_eq!(
            SampleCount::started_within(Duration::from_secs(1), rate),
            Some(SampleCount::new(16_000))
        );
        assert_eq!(
            SampleCount::started_within(Duration::from_nanos(62_499), rate),
            Some(SampleCount::ZERO)
        );
        assert_eq!(
            SampleCount::started_within(Duration::from_nanos(62_500), rate),
            Some(SampleCount::new(1))
        );
    }

    #[test]
    fn durations_round_up_at_inexact_rates() {
        let rate = SampleRate::new(44_100).unwrap();
        // 10^9 / 44100 = 22675.73..., rounded up.
        assert_eq!(
            SampleCount::new(1).duration_at(rate),
            Some(Duration::from_nanos(22_676))
        );
        assert_eq!(
            SampleCount::started_within(Duration::from_nanos(22_675), rate),
            Some(SampleCount::ZERO)
        );
        assert_eq!(
            SampleCount::started_within(Duration::from_nanos(22_676), rate),
            Some(SampleCount::new(1))
        );
    }

    #[test]
    fn duration_overflow_is_none() {
        assert_eq!(
            SampleCount::new(u64::MAX).duration_at(SampleRate::SPEECH),
            None
        );
        assert_eq!(
            SampleCount::started_within(
                Duration::MAX,
                SampleRate::new(SampleRate::MAX_HZ).unwrap()
            ),
            None
        );
    }

    #[test]
    fn session_time_arithmetic() {
        let t = SessionTime::from_nanos(1_000);
        assert_eq!(t.as_nanos(), 1_000);
        assert_eq!(t.elapsed(), Duration::from_nanos(1_000));
        assert_eq!(
            t.checked_add(Duration::from_nanos(5)),
            Some(SessionTime::from_nanos(1_005))
        );
        assert_eq!(
            SessionTime::from_nanos(u64::MAX).checked_add(Duration::from_nanos(1)),
            None
        );
        assert_eq!(SessionTime::ZERO.checked_add(Duration::MAX), None);
        assert_eq!(
            t.checked_duration_since(SessionTime::ZERO),
            Some(Duration::from_nanos(1_000))
        );
        assert_eq!(SessionTime::ZERO.checked_duration_since(t), None);
        assert_eq!(
            SessionTime::from_elapsed(Duration::from_secs(2)).map(SessionTime::as_nanos),
            Some(2_000_000_000)
        );
        assert_eq!(SessionTime::from_elapsed(Duration::MAX), None);
    }

    #[test]
    fn sample_index_arithmetic() {
        let s = SampleIndex::new(10);
        assert_eq!(
            s.checked_add(SampleCount::new(5)),
            Some(SampleIndex::new(15))
        );
        assert_eq!(
            SampleIndex::new(u64::MAX).checked_add(SampleCount::new(1)),
            None
        );
        assert_eq!(
            s.checked_count_since(SampleIndex::new(4)),
            Some(SampleCount::new(6))
        );
        assert_eq!(s.checked_count_since(s), Some(SampleCount::ZERO));
        assert_eq!(SampleIndex::new(4).checked_count_since(s), None);
    }

    #[test]
    fn sample_range_holds_its_invariant() {
        let a = SampleIndex::new(3);
        let b = SampleIndex::new(7);
        assert_eq!(SampleRange::new(b, a), None);
        let r = SampleRange::new(a, b).unwrap();
        assert_eq!((r.start(), r.end(), r.len()), (a, b, SampleCount::new(4)));
        assert!(!r.is_empty());
        assert!(r.contains(a));
        assert!(r.contains(SampleIndex::new(6)));
        assert!(!r.contains(b));
        assert!(!r.contains(SampleIndex::new(2)));
        let empty = SampleRange::new(a, a).unwrap();
        assert!(empty.is_empty());
        assert!(!empty.contains(a));
        assert_eq!(SampleRange::starting_at(a, SampleCount::new(4)), Some(r));
        assert_eq!(
            SampleRange::starting_at(SampleIndex::new(u64::MAX), SampleCount::new(1)),
            None
        );
    }

    proptest! {
        /// Sample → time → sample is exact at every allowed rate.
        #[test]
        fn count_duration_round_trip(n in 0..u64::MAX / 2, rate in any_rate()) {
            if let Some(d) = SampleCount::new(n).duration_at(rate) {
                prop_assert_eq!(SampleCount::started_within(d, rate), Some(SampleCount::new(n)));
            }
        }

        /// Consecutive samples are always at least a nanosecond apart, so no
        /// two samples share a session time.
        #[test]
        fn durations_strictly_increase(n in 0..u64::MAX / 4, rate in any_rate()) {
            let a = SampleCount::new(n).duration_at(rate);
            let b = SampleCount::new(n + 1).duration_at(rate);
            if let (Some(a), Some(b)) = (a, b) {
                prop_assert!(b > a);
            }
        }

        /// Time → sample picks the sample playing at that moment: the last one
        /// that started at or before it.
        #[test]
        fn started_within_is_the_last_sample_started(nanos in any::<u64>(), rate in any_rate()) {
            let d = Duration::from_nanos(nanos);
            let n = SampleCount::started_within(d, rate).unwrap();
            prop_assert!(n.duration_at(rate).unwrap() <= d);
            if let Some(next) = SampleCount::new(n.get() + 1).duration_at(rate) {
                prop_assert!(next > d);
            }
        }
    }
}
