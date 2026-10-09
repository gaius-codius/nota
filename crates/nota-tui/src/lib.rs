//! nota's terminal UI, drawn with `ratatui` on crossterm.
//!
//! Two screens so far:
//!
//! - [`Home`] (the UI spec's `Home` mockup): the logo, the recent sessions
//!   with their status, and `R` to record with the last settings. [`run_home`]
//!   runs it until it asks for something ([`Action`]).
//! - [`Recording`] (the `Main` mockup): the frame with `● REC` and the
//!   elapsed time, the timeline band (marks and notes above a level
//!   waveform of the whole session), the live transcript, and the keys `m`
//!   (mark), `n` (note) and `s` (stop, which asks first) in the footer.
//!
//! Both draw in the [`Theme`] [`Theme::load`] reads from the Omarchy theme,
//! or with no colour at all under `NO_COLOR`.
//!
//! The screen owns no threads and reads no devices. It talks to the recorder
//! only in the recorder protocol ([`nota_core::recorder`]): the recorder's
//! events and the terminal's keys come in as [`Event`]s over a channel, and
//! [`run()`] draws the screen and sends each [`Command`] it gives (a mark, a
//! note, the stop) out over another, until the stop is confirmed or the
//! recorder closes it.
//! Time comes only from the session [`Clock`], so marks and notes are stamped
//! in session time and tests run on a fake clock.
//!
//! [`Clock`]: nota_core::Clock
//! [`Command`]: nota_core::recorder::Command

mod annotation;
mod band;
mod home;
mod level;
mod logo;
mod render;
mod run;
mod screen;
mod text;
mod theme;

pub use home::{Action, Home, Session, Status};
pub use run::{Ended, Event, InputThread, RunError, run, run_home};
pub use screen::Recording;
pub use theme::{Theme, ThemeError};
