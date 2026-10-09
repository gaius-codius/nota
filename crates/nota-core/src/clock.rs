//! The session clock: the only place nota reads the time.
//!
//! Everything else takes a [`Clock`], so timing code runs against a
//! `FakeClock` in tests. Clippy's `disallowed-methods` bans
//! `Instant::now`, `SystemTime::now`, their `elapsed` shortcuts and
//! `rustix::time::clock_gettime` elsewhere.
//!
//! Session time keeps counting while the machine is suspended, so a sleep
//! shows up as a gap between epochs rather than vanishing. On Linux that
//! takes `CLOCK_BOOTTIME`: `Instant` uses `CLOCK_MONOTONIC`, which stops
//! during suspend.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::time::SessionTime;

/// A source of session time. It never goes backwards.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// The current session time.
    fn now(&self) -> SessionTime;
}

/// The real session clock: monotonic, counting through suspend, with zero at
/// the moment it started.
#[derive(Debug)]
pub struct SystemClock {
    origin: Duration,
    /// The latest reading handed out, so `now` can't go back even if the
    /// clock underneath misbehaves.
    latest: AtomicU64,
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
        let origin = monotonic_now().ok_or(ClockUnavailable)?;
        Ok(Self {
            origin,
            latest: AtomicU64::new(0),
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
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let before = self.latest.fetch_max(nanos, Ordering::SeqCst);
        SessionTime::from_nanos(before.max(nanos))
    }
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
    }

    impl FakeClock {
        /// A clock that reads `start` until it's advanced.
        #[must_use]
        pub fn new(start: SessionTime) -> Self {
            Self {
                nanos: AtomicU64::new(start.as_nanos()),
            }
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
}
