//! The session clock: the only place nota reads the time.
//!
//! Everything else takes a [`Clock`], so timing code runs against a
//! [`FakeClock`] in tests. Clippy's `disallowed-methods` bans
//! `Instant::now`, `SystemTime::now` and their `elapsed` shortcuts elsewhere.

use std::time::Instant;

use crate::time::SessionTime;

/// A source of session time. It never goes backwards.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// The current session time.
    fn now(&self) -> SessionTime;
}

/// The real session clock: monotonic, with zero at the moment it started.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// Starts a session clock; session time zero is now.
    #[must_use]
    pub fn start() -> Self {
        Self {
            origin: monotonic_now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> SessionTime {
        // `Instant` is monotonic, so this never goes back. It would take
        // about 584 years to saturate.
        let elapsed = monotonic_now().saturating_duration_since(self.origin);
        SessionTime::from_elapsed(elapsed).unwrap_or(SessionTime::from_nanos(u64::MAX))
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "the session clock is the one place nota reads the monotonic clock"
)]
fn monotonic_now() -> Instant {
    Instant::now()
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
        let clock = SystemClock::start();
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
