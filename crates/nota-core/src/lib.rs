//! Shared types for nota: the session clock, track epochs, typed time units,
//! the messages between the recorder and the speech engine, with their wire
//! encoding (the engine protocol), the engine's tie to its recorder, the
//! messages between the screens and the recorder (the recorder protocol),
//! and heard speech placed in session time.
//!
//! Time has one source, the [`Clock`]. Each track maps its sample count to
//! session time through a [`TrackTimeline`] of epochs; a new epoch starts
//! whenever the track's stream is reopened or audio is lost, and the time
//! between epochs is a gap with no audio. Each epoch maps its samples under a
//! [`Drift`] measured from the stream's timestamps, so a device clock that
//! runs fast or slow doesn't pull its audio away from session time.
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
pub mod drift;
pub mod epoch;
pub mod ids;
pub mod lifeline;
pub mod messages;
pub mod protocol;
pub mod recorder;
pub mod time;
pub mod utterance;

#[cfg(any(test, feature = "fake-clock"))]
pub use clock::FakeClock;
pub use clock::{Clock, ClockUnavailable, SystemClock, wall_now};
pub use drift::{Drift, DriftMeter, Stamp};
pub use epoch::{Epoch, EpochAnchor, EpochError, Gap, OpenedEpoch, TrackTimeline};
pub use ids::{EpochId, SessionId, TrackId};
pub use time::{SampleCount, SampleIndex, SampleRange, SampleRate, SessionTime, WallTime};
pub use utterance::{Utterance, Word};
