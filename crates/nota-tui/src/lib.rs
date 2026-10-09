//! nota's terminal UI, drawn with `ratatui` on crossterm.
//!
//! For now it has one screen, [`Recording`] (the UI spec's `Main` mockup):
//! the frame with `● REC` and the elapsed time, the timeline band (marks and
//! notes above a level waveform of the whole session), the live transcript,
//! and the keys `m` (mark), `n` (note) and `s` (stop, which asks first) in
//! the footer.
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
mod level;
mod render;
mod run;
mod screen;
mod text;
mod theme;

pub use run::{Ended, Event, InputThread, RunError, run};
pub use screen::Recording;
pub use theme::Theme;
