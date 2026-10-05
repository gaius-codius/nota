//! Marks and notes: what the listener adds while recording, each at a point
//! in session time.

use nota_core::SessionTime;

/// A mark (◆): "this matters", at the moment `m` was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// When `m` was pressed.
    pub at: SessionTime,
}

/// A note (◇): the listener's text, pinned to the moment `n` was pressed,
/// not the moment typing finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// When `n` was pressed.
    pub at: SessionTime,
    /// What was typed, trimmed; never empty.
    pub text: String,
}

/// A mark or a note, as the screen hands it on to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    /// A mark (◆).
    Mark(Mark),
    /// A note (◇).
    Note(Note),
}

impl Annotation {
    /// The session time it's pinned to.
    #[must_use]
    pub fn at(&self) -> SessionTime {
        match self {
            Self::Mark(mark) => mark.at,
            Self::Note(note) => note.at,
        }
    }
}
