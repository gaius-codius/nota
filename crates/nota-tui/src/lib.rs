//! nota's terminal UI, drawn with `ratatui` on crossterm.
//!
//! For now it has one screen, [`Recording`] (the UI spec's `Main` mockup):
//! the frame with `● REC` and the elapsed time, the timeline band (marks and
//! notes above a level waveform of the whole session), the live transcript,
//! and the keys `m` (mark) and `n` (note) in the footer.
//!
//! The screen owns no threads and reads no devices. The rest of nota sends it
//! [`Update`]s (levels, text, bytes written) and key presses as [`Event`]s
//! over a channel, and [`run`] draws it and sends each new mark and note out
//! over another channel. Time comes only from the session [`Clock`], so marks
//! and notes are stored in session time and tests run on a fake clock.
//!
//! [`Clock`]: nota_core::Clock

mod annotation;
mod band;
mod level;
mod render;
mod run;
mod screen;
mod text;
mod theme;

pub use annotation::{Annotation, Mark, Note};
pub use level::Level;
pub use run::{Event, InputThread, RunError, run};
pub use screen::{Recording, Update};
pub use text::Utterance;
pub use theme::Theme;
