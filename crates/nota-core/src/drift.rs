//! Drift: how far a track's device clock runs from the session clock, and
//! the meter that measures it from the stream's own timestamps.
//!
//! A device that runs 100 ppm fast delivers 16,001.6 samples a second
//! where 16,000 were asked for. Timed at the nominal rate, an hour of its
//! audio would run 360 ms past the session clock. Each epoch therefore
//! carries a [`Drift`], and maps its samples at the rate the device
//! actually ran at: the nominal rate times `1 + drift`.
//!
//! The [`DriftMeter`] watches when each buffer was captured, by the
//! stream's timestamps on the session clock, against when the epoch's
//! mapping says its first sample plays:
//! - A buffer captured [`LOSS_MIN`] or more later than its mapping says is
//!   a hole: audio was lost before it ([`Reading::Lost`]), and the caller
//!   opens a new epoch at its capture time, so the loss shows as a gap.
//! - Otherwise the difference is drift. Once the meter has watched
//!   [`MIN_WINDOW`] of audio it knows the device's rate, and when the
//!   mapping is more than [`RETIME_AFTER`] off it asks for a new epoch
//!   ([`Reading::Retime`]) that follows straight on from the old one: for
//!   [`SLEW`] at a rate that closes the difference, then at the measured
//!   rate. No two samples share a moment, and none of the audio is shown
//!   as a gap it wasn't.
//!
//! An epoch's drift never changes once it's opened, so a time handed out
//! for a sample stays that sample's. Correcting drift costs an epoch, and
//! so a journal, each time: twice when the meter first learns the rate,
//! then only when the device's rate wanders.

use std::time::Duration;

use crate::epoch::Epoch;
use crate::time::{SampleCount, SampleIndex, SampleRate, SessionTime};

/// Nanoseconds in a second.
const NANOS_PER_SEC: u128 = 1_000_000_000;
/// Parts per billion in one.
const PPB: i128 = 1_000_000_000;

/// How far a device's clock runs from the session clock, in parts per
/// billion: positive when it delivers more samples a second than its
/// nominal rate. At most [`Drift::MAX_PPB`] either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Drift(i32);

impl Drift {
    /// No drift: the device runs at its nominal rate.
    pub const ZERO: Self = Self(0);

    /// The most drift accepted either way: 1000 ppm, ten times a poor USB
    /// device's. More means a clock or timestamp bug, not a device.
    pub const MAX_PPB: i32 = 1_000_000;

    /// A drift of `ppb` parts per billion, or `None` beyond
    /// [`Self::MAX_PPB`] either way.
    #[must_use]
    pub const fn from_ppb(ppb: i32) -> Option<Self> {
        if ppb > Self::MAX_PPB || ppb < -Self::MAX_PPB {
            None
        } else {
            Some(Self(ppb))
        }
    }

    /// `ppb`, held within [`Self::MAX_PPB`] either way.
    fn clamped(ppb: i128) -> Self {
        let max = i128::from(Self::MAX_PPB);
        // In range after the clamp, so the conversion can't fail.
        Self(i32::try_from(ppb.clamp(-max, max)).unwrap_or(0))
    }

    /// The drift in parts per billion.
    #[must_use]
    pub const fn ppb(self) -> i32 {
        self.0
    }

    /// The rate the device runs at, as samples per second times 10⁹: the
    /// nominal rate times `1 + drift`. Never zero, and at most about
    /// 1.001 × 10¹⁵, so consecutive samples stay at least a nanosecond
    /// apart.
    fn scaled_rate(self, rate: SampleRate) -> u128 {
        let ppb = u128::from(self.0.unsigned_abs());
        let hz = u128::from(rate.hz());
        let one = NANOS_PER_SEC;
        if self.0 >= 0 {
            hz * (one + ppb)
        } else {
            hz * (one - ppb)
        }
    }

    /// How long `count` samples last at `rate` under this drift, rounded up
    /// to a whole nanosecond, as [`SampleCount::duration_at`] does with no
    /// drift. `None` if that doesn't fit in [`SessionTime`]'s range.
    #[must_use]
    pub fn duration_of(self, count: SampleCount, rate: SampleRate) -> Option<Duration> {
        // Can't overflow: u64::MAX * 10^18 is below u128::MAX.
        let scaled = u128::from(count.get()) * NANOS_PER_SEC * NANOS_PER_SEC;
        let nanos = scaled.div_ceil(self.scaled_rate(rate));
        u64::try_from(nanos).ok().map(Duration::from_nanos)
    }

    /// The number of whole sample periods at `rate` under this drift that
    /// have *started* within `elapsed`, given the rounding of
    /// [`Self::duration_of`]: the largest `n` whose duration is at most
    /// `elapsed`. `None` on overflow.
    #[must_use]
    pub fn count_within(self, elapsed: Duration, rate: SampleRate) -> Option<SampleCount> {
        let scaled = elapsed.as_nanos().checked_mul(self.scaled_rate(rate))?;
        let count = scaled / (NANOS_PER_SEC * NANOS_PER_SEC);
        u64::try_from(count).ok().map(SampleCount::new)
    }

    /// The drift of a device that delivered `samples` in `elapsed` at
    /// nominal `rate`, held within [`Self::MAX_PPB`]. `None` if no time
    /// passed.
    fn measured(samples: SampleCount, elapsed: Duration, rate: SampleRate) -> Option<Self> {
        let nanos = i128::try_from(elapsed.as_nanos()).ok().filter(|n| *n > 0)?;
        let delivered = i128::from(samples.get()) * PPB * PPB;
        let expected = nanos.checked_mul(i128::from(rate.hz()))?;
        Some(Self::clamped(delivered / expected - PPB))
    }
}

/// How much later than its epoch's mapping a buffer must have been captured
/// to count as audio lost before it, rather than drift: 10 ms, more than
/// [`RETIME_AFTER`] and a timestamp's jitter, and less than the smallest
/// buffer an audio server usually loses whole (about 21 ms at its default
/// quantum).
pub const LOSS_MIN: Duration = Duration::from_millis(10);

/// How far an epoch's mapping may stray from the stream's timestamps
/// before the meter asks for a new epoch to correct it: 4 ms.
pub const RETIME_AFTER: Duration = Duration::from_millis(4);

/// How much of a run's audio the meter watches before it trusts its
/// measured rate: 10 s. A millisecond of timestamp jitter over that is
/// 100 ppm, but the window only grows, and a later retime corrects an early
/// estimate. Until then the mapping runs at the drift it had, so a device
/// more than about 900 ppm off can build up enough to look like a loss.
pub const MIN_WINDOW: Duration = Duration::from_secs(10);

/// How long a run must be before its measured drift is trusted over a
/// longer earlier run's, or is judged against [`DRIFT_LIMIT`]: 60 s. A
/// quantum lost unseen, or a stamp a few milliseconds off, then moves the
/// estimate by under about 100 ppm.
pub const TRUSTED_WINDOW: Duration = Duration::from_secs(60);

/// How long a correcting epoch takes to close the difference it was opened
/// for: 60 s. A 4 ms difference closes at about 67 ppm on top of the
/// measured drift.
pub const SLEW: Duration = Duration::from_secs(60);

/// The drift past which the meter reports the device as suspect
/// ([`DriftMeter::past_limit`]), measured over at least
/// [`TRUSTED_WINDOW`]: 300 ppm, three times a poor USB device's. Its audio
/// is still timed by the drift measured.
pub const DRIFT_LIMIT: Drift = Drift(300_000);

/// What the meter made of one buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// The buffer is where the epoch's mapping says, near enough.
    Steady,
    /// The buffer was captured this much later than the mapping says, at
    /// least [`LOSS_MIN`]: audio was lost before it. The meter didn't learn
    /// from it. A new epoch at its capture time makes the loss a gap; then
    /// call [`DriftMeter::restart`].
    Lost {
        /// How much later than the mapping it was captured.
        hole: Duration,
    },
    /// The mapping has strayed by more than [`RETIME_AFTER`] (or a
    /// correction has run its course): open an epoch at the buffer's first
    /// sample that follows straight on from the current one, with this
    /// drift ([`TrackTimeline::retime`](crate::TrackTimeline::retime)).
    /// Never asked for while the measured drift is at
    /// [`Drift::MAX_PPB`], which only bad stamps give: correcting by it
    /// would only open epoch after epoch.
    Retime(Drift),
}

/// Measures one track's drift from the capture times of its buffers, and
/// says when its timeline needs a new epoch. Pure: it reads no clock, and
/// changes no timeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftMeter {
    /// Where the current run of unbroken audio started: its first buffer's
    /// first sample and capture time.
    run: Option<(SampleIndex, SessionTime)>,
    /// The drift measured over the current run, once it reached
    /// [`MIN_WINDOW`]; or after a restart, the earlier run's, until the new
    /// one is as long or reaches [`TRUSTED_WINDOW`].
    measured: Option<Drift>,
    /// How long a run `measured` was measured over.
    measured_over: Duration,
    /// Whether `measured` comes from the current run.
    from_this_run: bool,
    /// While a correcting epoch closes its difference: the sample at which
    /// it has, when the measured drift takes over.
    slewing_until: Option<SampleIndex>,
    /// Whether the drift measured over a trusted window is past
    /// [`DRIFT_LIMIT`]; `reported` once [`Self::past_limit`] has said so.
    beyond: bool,
    reported: bool,
}

impl DriftMeter {
    /// A meter that has measured nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            run: None,
            measured: None,
            measured_over: Duration::ZERO,
            from_this_run: false,
            slewing_until: None,
            beyond: false,
            reported: false,
        }
    }

    /// The drift measured so far, once a run has reached [`MIN_WINDOW`].
    #[must_use]
    pub const fn measured(&self) -> Option<Drift> {
        self.measured
    }

    /// Starts a new run at the next buffer: audio was lost or went
    /// uncaptured, so the sample count no longer follows the time. The
    /// drift measured so far stays, as the device's best estimate, and any
    /// correction under way ends with the epoch it ran in.
    pub fn restart(&mut self) {
        self.run = None;
        self.slewing_until = None;
        self.from_this_run = false;
    }

    /// The measured drift, once, the first time a trusted window's is past
    /// [`DRIFT_LIMIT`] either way.
    pub fn past_limit(&mut self) -> Option<Drift> {
        if self.beyond && !self.reported {
            self.reported = true;
            return self.measured;
        }
        None
    }

    /// Reads a buffer whose first sample, `first`, was captured at `at`,
    /// against `epoch`, the one it's recorded in. A run's first buffer only
    /// starts the run.
    pub fn observe(&mut self, epoch: &Epoch, first: SampleIndex, at: SessionTime) -> Reading {
        let Some(mapped) = epoch.time_of(first) else {
            return Reading::Steady;
        };
        let Some((run_first, run_at)) = self.run else {
            self.run = Some((first, at));
            return Reading::Steady;
        };
        let off = i128::from(at.as_nanos()) - i128::from(mapped.as_nanos());
        if off >= nanos(LOSS_MIN) {
            return Reading::Lost {
                hole: Duration::from_nanos(u64::try_from(off).unwrap_or(u64::MAX)),
            };
        }
        self.learn(first, at, run_first, run_at, epoch.rate());
        if let Some(until) = self.slewing_until {
            if first < until {
                return Reading::Steady;
            }
            self.slewing_until = None;
            return self.measured.map_or(Reading::Steady, Reading::Retime);
        }
        match self.measured {
            Some(measured)
                if off.abs() > nanos(RETIME_AFTER)
                    && measured.0.unsigned_abs() < Drift::MAX_PPB.unsigned_abs() =>
            {
                self.slewing_until = SampleCount::started_within(SLEW, epoch.rate())
                    .and_then(|n| first.checked_add(n));
                // Behind the stream (`off` positive), the mapping must give
                // each sample more time: a lower rate, so less drift.
                let closing = off * PPB / nanos(SLEW);
                Reading::Retime(Drift::clamped(i128::from(measured.0) - closing))
            }
            _ => Reading::Steady,
        }
    }

    /// Updates the measured drift from the run so far, if it's long enough:
    /// [`MIN_WINDOW`], and after a restart, as long as the run measured
    /// before or [`TRUSTED_WINDOW`], whichever is shorter.
    fn learn(
        &mut self,
        first: SampleIndex,
        at: SessionTime,
        run_first: SampleIndex,
        run_at: SessionTime,
        rate: SampleRate,
    ) {
        let Some(elapsed) = at.checked_duration_since(run_at) else {
            return;
        };
        let enough = self.from_this_run || elapsed >= self.measured_over.min(TRUSTED_WINDOW);
        if elapsed < MIN_WINDOW || !enough {
            return;
        }
        let samples = first.saturating_count_since(run_first);
        if let Some(drift) = Drift::measured(samples, elapsed, rate) {
            self.measured = Some(drift);
            self.measured_over = elapsed;
            self.from_this_run = true;
            if elapsed >= TRUSTED_WINDOW {
                self.beyond = drift.0.unsigned_abs() > DRIFT_LIMIT.0.unsigned_abs();
            }
        }
    }
}

/// `duration` in nanoseconds, signed for comparing differences.
fn nanos(duration: Duration) -> i128 {
    i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX)
}

#[cfg(test)]
mod tests;
