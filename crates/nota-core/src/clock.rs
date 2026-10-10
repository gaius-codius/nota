//! The session clock: the only place nota reads the time.
//!
//! Everything else takes a [`Clock`], so timing code runs against a
//! `FakeClock` in tests. Clippy's `disallowed-methods` bans
//! `Instant::now`, `SystemTime::now`, their `elapsed` shortcuts and
//! `rustix::time::clock_gettime` elsewhere.
//!
//! The calendar's time, [`wall_now`], is read here too, for saying when
//! a session started. Nothing times anything by it: the system's date can
//! be changed under it.
//!
//! Session time keeps counting while the machine is suspended, so a sleep
//! shows up as a gap between epochs rather than vanishing. On Linux that
//! takes `CLOCK_BOOTTIME`: `Instant` uses `CLOCK_MONOTONIC`, which stops
//! during suspend. The difference between the two grows only while the
//! machine is suspended, so the clock also says how long it has been
//! suspended ([`Clock::suspended`]), and capture opens an epoch after one.
//!
//! A resumed session's clock carries on from a stored session time
//! ([`SystemClock::resume`]), so the new recording comes after the old one.
//!
//! Audio servers stamp their buffers on the clock that stops during
//! suspend (`CLOCK_MONOTONIC` on Linux). [`Clock::awake_to_session`] places
//! such a stamp in session time, by how far the two clocks have parted so
//! far, so a buffer can be timed by when it was captured rather than by
//! when nota got it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::time::{SessionTime, WallTime};

/// A source of session time. It never goes backwards.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// The current session time.
    fn now(&self) -> SessionTime;

    /// How long the machine has been suspended since the clock started, as
    /// far as the clock can tell. It never goes backwards; it grows only
    /// while the machine is suspended, by about the time it was.
    fn suspended(&self) -> Duration;

    /// The session time of `awake`, a recent reading of the system's clock
    /// that stops during suspend (as an audio server stamps its buffers).
    /// `None` if it comes before the session clock started, or the clock
    /// can't place it. A reading from before a suspend is placed as if the
    /// suspend came before it.
    fn awake_to_session(&self, awake: Duration) -> Option<SessionTime>;
}

/// The real session clock: monotonic, counting through suspend, from the
/// session time it started at.
#[derive(Debug)]
pub struct SystemClock {
    origin: Duration,
    /// The session time at `origin`: zero, or where a resumed session left
    /// off.
    base: SessionTime,
    /// The awake clock's lag behind `origin`'s clock when it started: what
    /// the machine had been suspended before.
    asleep_before: Duration,
    /// The latest reading handed out, so `now` can't go back even if the
    /// clock underneath misbehaves.
    latest: AtomicU64,
    /// The longest suspension reported, so `suspended` can't go back
    /// either.
    asleep: AtomicU64,
}

impl SystemClock {
    /// Starts a session clock; session time zero is now. Make one per
    /// session and share it (`Arc<dyn Clock>`): two clocks started at
    /// different moments disagree, and tracks timed by them won't line up.
    ///
    /// # Errors
    ///
    /// [`ClockUnavailable`] if the system clock can't be read (on Linux,
    /// a kernel without `CLOCK_BOOTTIME`, older than 2.6.39).
    pub fn start() -> Result<Self, ClockUnavailable> {
        Self::resume(SessionTime::ZERO)
    }

    /// Starts the clock of a resumed session: session time `from` is now,
    /// and it counts on from there. Pass the end of everything the session
    /// recorded before, so the new recording comes after it.
    ///
    /// # Errors
    ///
    /// As [`Self::start`].
    pub fn resume(from: SessionTime) -> Result<Self, ClockUnavailable> {
        let origin = monotonic_now().ok_or(ClockUnavailable)?;
        Ok(Self {
            origin,
            base: from,
            asleep_before: asleep_now(origin),
            latest: AtomicU64::new(from.as_nanos()),
            asleep: AtomicU64::new(0),
        })
    }
}

impl Clock for SystemClock {
    fn now(&self) -> SessionTime {
        // A read that fails after `start` succeeded shouldn't happen; if it
        // does, time stands still rather than jumping or panicking.
        let Some(reading) = monotonic_now() else {
            return SessionTime::from_nanos(self.latest.load(Ordering::SeqCst));
        };
        let elapsed = reading.saturating_sub(self.origin);
        // Saturates after about 584 years.
        let nanos = u64::try_from(elapsed.as_nanos())
            .unwrap_or(u64::MAX)
            .saturating_add(self.base.as_nanos());
        let before = self.latest.fetch_max(nanos, Ordering::SeqCst);
        SessionTime::from_nanos(before.max(nanos))
    }

    fn suspended(&self) -> Duration {
        let asleep = monotonic_now().map_or(Duration::ZERO, |reading| {
            asleep_now(reading).saturating_sub(self.asleep_before)
        });
        let nanos = u64::try_from(asleep.as_nanos()).unwrap_or(u64::MAX);
        let before = self.asleep.fetch_max(nanos, Ordering::SeqCst);
        Duration::from_nanos(before.max(nanos))
    }

    fn awake_to_session(&self, awake: Duration) -> Option<SessionTime> {
        if !PLACES_AWAKE_READINGS {
            return None;
        }
        // On the clock that counts through suspend, by how far the two have
        // parted so far.
        let asleep = asleep_now(monotonic_now()?);
        let elapsed = awake.checked_add(asleep)?.checked_sub(self.origin)?;
        self.base.checked_add(elapsed)
    }
}

/// Whether [`SystemClock`] can place a reading of the clock that stops
/// during suspend: on Linux, where audio servers stamp buffers with
/// `CLOCK_MONOTONIC`. Elsewhere not until v2.
const PLACES_AWAKE_READINGS: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// The calendar's time now, to the second, or `None` if the system's date
/// is before 1970 or past the year 292 billion.
#[must_use]
#[expect(
    clippy::disallowed_methods,
    reason = "the clock module is the one place nota reads the calendar's time"
)]
pub fn wall_now() -> Option<WallTime> {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    WallTime::from_unix_seconds(i64::try_from(since.as_secs()).ok()?)
}

/// The system's monotonic clock couldn't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockUnavailable;

impl std::fmt::Display for ClockUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the system's monotonic clock can't be read")
    }
}

impl std::error::Error for ClockUnavailable {}

/// The monotonic clock, counting through suspend, from an arbitrary fixed
/// point. `None` if it can't be read. Uses the `Result`-returning call:
/// rustix's plain `clock_gettime` panics on failure.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[expect(
    clippy::disallowed_methods,
    reason = "the session clock is the one place nota reads the monotonic clock"
)]
fn monotonic_now() -> Option<Duration> {
    let now = rustix::time::clock_gettime_dynamic(rustix::time::DynamicClockId::Boottime).ok()?;
    // Never negative, but a negative reading is refused rather than misread.
    Duration::try_from(now).ok()
}

/// The monotonic clock from an arbitrary fixed point. Windows' `Instant`
/// counts through sleep; macOS's doesn't, so v2 needs `mach_continuous_time`
/// there.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[expect(
    clippy::disallowed_methods,
    reason = "the session clock is the one place nota reads the monotonic clock"
)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the Linux version, whose read can fail"
)]
fn monotonic_now() -> Option<Duration> {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ANCHOR: OnceLock<Instant> = OnceLock::new();
    let now = Instant::now();
    Some(now.saturating_duration_since(*ANCHOR.get_or_init(|| now)))
}

/// How far the clock that stops during suspend lags `boottime`, a reading
/// of the clock that doesn't, taken just before: the time the machine has
/// spent suspended since it booted. Zero if it can't be read. The two reads
/// aren't simultaneous, so it can be off by the time between them.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[expect(
    clippy::disallowed_methods,
    reason = "the session clock is the one place nota reads the monotonic clock"
)]
fn asleep_now(boottime: Duration) -> Duration {
    rustix::time::clock_gettime_dynamic(rustix::time::DynamicClockId::Known(
        rustix::time::ClockId::Monotonic,
    ))
    .ok()
    .and_then(|awake| Duration::try_from(awake).ok())
    .map_or(Duration::ZERO, |awake| boottime.saturating_sub(awake))
}

/// Suspend isn't counted here yet: Windows' `Instant` counts through sleep,
/// so session time does too, but a sleep opens no epoch until v2.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const fn asleep_now(_boottime: Duration) -> Duration {
    Duration::ZERO
}

#[cfg(any(test, feature = "fake-clock"))]
pub use fake::FakeClock;

#[cfg(any(test, feature = "fake-clock"))]
mod fake {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::Clock;
    use crate::time::SessionTime;

    /// A clock for tests that moves only when told to.
    #[derive(Debug, Default)]
    pub struct FakeClock {
        nanos: AtomicU64,
        asleep: AtomicU64,
    }

    impl FakeClock {
        /// A clock that reads `start` until it's advanced.
        #[must_use]
        pub fn new(start: SessionTime) -> Self {
            Self {
                nanos: AtomicU64::new(start.as_nanos()),
                asleep: AtomicU64::new(0),
            }
        }

        /// Moves the clock forward by `by` as a suspend of that long would:
        /// session time and [`Clock::suspended`] both move on by it.
        pub fn suspend(&self, by: Duration) {
            self.advance(by);
            let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
            let _ = self
                .asleep
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |asleep| {
                    Some(asleep.saturating_add(by))
                });
        }

        /// Moves the clock forward by `by`, stopping at the largest session
        /// time rather than wrapping. It can't move backwards.
        pub fn advance(&self, by: Duration) {
            let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
            let _ = self
                .nanos
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |now| {
                    Some(now.saturating_add(by))
                });
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> SessionTime {
            SessionTime::from_nanos(self.nanos.load(Ordering::SeqCst))
        }

        fn suspended(&self) -> Duration {
            Duration::from_nanos(self.asleep.load(Ordering::SeqCst))
        }

        /// The fake's clock that stops during suspend reads its session
        /// time less the time it has spent suspended.
        fn awake_to_session(&self, awake: Duration) -> Option<SessionTime> {
            SessionTime::from_elapsed(awake.checked_add(self.suspended())?)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn system_clock_starts_near_zero_and_never_goes_back() {
        let clock = SystemClock::start().unwrap();
        let mut last = clock.now();
        // Generous: only a stalled test machine would take a minute here.
        assert!(last.elapsed() < Duration::from_secs(60));
        for _ in 0..1_000 {
            let now = clock.now();
            assert!(now >= last);
            last = now;
        }
    }

    #[test]
    fn system_clock_moves_forward() {
        let clock = SystemClock::start().unwrap();
        let first = clock.now();
        // The monotonic clock ticks in nanoseconds; a busy loop sees it move
        // long before the cap. No sleep, per the clippy ban.
        let moved = (0..100_000_000).any(|_| clock.now() > first);
        assert!(moved, "the session clock never advanced past {first:?}");
    }

    /// A resumed clock carries on from the time it's given.
    #[test]
    fn a_resumed_clock_carries_on_from_its_start() {
        let from = SessionTime::from_nanos(3_600_000_000_000);
        let clock = SystemClock::resume(from).unwrap();
        let now = clock.now();
        assert!(now >= from, "{now:?}");
        // Generous: only a stalled test machine would take a minute here.
        assert!(
            now.checked_duration_since(from).unwrap() < Duration::from_secs(60),
            "{now:?}"
        );
        // And it counts on from there, not stands still at it.
        let moved = (0..100_000_000).any(|_| clock.now() > now);
        assert!(moved, "the resumed clock never advanced past {now:?}");
    }

    /// A clock that started on an awake machine counts no suspend, and the
    /// count never goes back.
    #[test]
    fn system_clock_counts_no_suspend_while_awake() {
        let clock = SystemClock::start().unwrap();
        let mut last = clock.suspended();
        // The two clocks are read a moment apart, so allow that much.
        assert!(last < Duration::from_millis(100), "{last:?}");
        for _ in 0..1_000 {
            let now = clock.suspended();
            assert!(now >= last);
            last = now;
        }
    }

    /// A fake suspend moves session time and the suspend count alike;
    /// advancing moves only session time.
    #[test]
    fn a_fake_suspend_counts_as_suspended() {
        let clock = FakeClock::new(SessionTime::from_nanos(5));
        clock.advance(Duration::from_nanos(10));
        assert_eq!(clock.suspended(), Duration::ZERO);
        clock.suspend(Duration::from_secs(2));
        assert_eq!(clock.now(), SessionTime::from_nanos(2_000_000_015));
        assert_eq!(clock.suspended(), Duration::from_secs(2));
        clock.suspend(Duration::MAX);
        assert_eq!(clock.suspended(), Duration::from_nanos(u64::MAX));
    }

    #[test]
    fn fake_clock_moves_only_when_advanced() {
        let clock = FakeClock::new(SessionTime::from_nanos(5));
        assert_eq!(clock.now(), SessionTime::from_nanos(5));
        assert_eq!(clock.now(), SessionTime::from_nanos(5));
        clock.advance(Duration::from_nanos(10));
        assert_eq!(clock.now(), SessionTime::from_nanos(15));
        clock.advance(Duration::ZERO);
        assert_eq!(clock.now(), SessionTime::from_nanos(15));
    }

    #[test]
    fn fake_clock_saturates_instead_of_wrapping() {
        let clock = FakeClock::new(SessionTime::from_nanos(u64::MAX - 1));
        clock.advance(Duration::from_nanos(5));
        assert_eq!(clock.now(), SessionTime::from_nanos(u64::MAX));
        let clock = FakeClock::default();
        clock.advance(Duration::MAX);
        assert_eq!(clock.now(), SessionTime::from_nanos(u64::MAX));
    }

    /// A reading of the clock that stops during suspend, taken just now,
    /// is placed at about the session time now.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads the clock an audio server stamps buffers with"
    )]
    fn an_awake_reading_is_placed_at_the_session_time_it_was_taken() {
        let clock = SystemClock::resume(SessionTime::from_nanos(7_000_000_000)).unwrap();
        let before = clock.now();
        let awake = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let after = clock.now();
        let placed = clock
            .awake_to_session(Duration::try_from(awake).unwrap())
            .unwrap();
        // The two clocks are read a moment apart, so allow that much.
        let slack = 100_000_000;
        assert!(
            placed.as_nanos() + slack >= before.as_nanos(),
            "{placed:?} {before:?}"
        );
        assert!(
            placed.as_nanos() <= after.as_nanos() + slack,
            "{placed:?} {after:?}"
        );
        // A reading from before the clock started has no session time.
        assert_eq!(clock.awake_to_session(Duration::ZERO), None);
    }

    /// The fake places an awake reading after the time it spent suspended.
    #[test]
    fn a_fake_places_awake_readings_after_its_suspends() {
        let clock = FakeClock::new(SessionTime::ZERO);
        assert_eq!(
            clock.awake_to_session(Duration::from_secs(1)),
            Some(SessionTime::from_nanos(1_000_000_000))
        );
        clock.suspend(Duration::from_secs(5));
        assert_eq!(
            clock.awake_to_session(Duration::from_secs(1)),
            Some(SessionTime::from_nanos(6_000_000_000))
        );
        assert_eq!(clock.awake_to_session(Duration::MAX), None);
    }

    #[test]
    fn clock_unavailable_describes_itself() {
        assert!(ClockUnavailable.to_string().contains("clock"));
    }

    #[test]
    fn clocks_are_shareable_across_threads() {
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::from_nanos(42)));
        let seen = std::thread::spawn({
            let clock = Arc::clone(&clock);
            move || clock.now()
        })
        .join()
        .unwrap();
        assert_eq!(seen, SessionTime::from_nanos(42));
    }

    /// The calendar's time is read, and it's after this code was written.
    #[test]
    fn the_wall_clock_reads_the_calendar() {
        let now = wall_now().unwrap();
        // 2026-01-01 00:00 UTC.
        assert!(now.unix_seconds() > 1_767_225_600, "{now:?}");
    }
}
