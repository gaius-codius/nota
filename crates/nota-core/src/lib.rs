//! Shared types for nota: the session clock, track epochs, typed time units,
//! and the messages between the recorder and the speech engine.
//!
//! Time has one source, the [`Clock`]. Each track maps its sample count to
//! session time through a [`TrackTimeline`] of epochs; a new epoch starts
//! whenever the track's stream is reopened, and the time between epochs is a
//! gap with no audio.
//!
//! ```
//! use std::time::Duration;
//! use nota_core::{SampleCount, SampleIndex, SampleRate, SessionTime, TrackId, TrackTimeline};
//!
//! let mut mic = TrackTimeline::new(TrackId::new(0));
//! mic.open_epoch(SessionTime::ZERO, SampleIndex::ZERO, SampleRate::SPEECH)?;
//!
//! // One second of audio, then the device drops out for half a second.
//! let captured = SampleIndex::ZERO.checked_add(SampleCount::new(16_000)).unwrap();
//! let reopened = SessionTime::from_nanos(1_500_000_000);
//! mic.open_epoch(reopened, captured, SampleRate::SPEECH)?;
//!
//! assert_eq!(mic.time_of(captured), Some(reopened));
//! let gap = mic.gaps().next().unwrap();
//! assert_eq!(gap.duration(), Duration::from_millis(500));
//! # Ok::<(), nota_core::EpochError>(())
//! ```

pub mod clock;
pub mod epoch;
pub mod ids;
pub mod messages;
pub mod time;

#[cfg(any(test, feature = "fake-clock"))]
pub use clock::FakeClock;
pub use clock::{Clock, SystemClock};
pub use epoch::{Epoch, EpochError, Gap, TrackTimeline};
pub use ids::{EpochId, TrackId};
pub use time::{SampleCount, SampleIndex, SampleRange, SampleRate, SessionTime};
