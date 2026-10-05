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
    at: SessionTime,
    text: String,
}

impl Note {
    /// A note at `at` with `text`, trimmed; `None` if it's blank.
    #[must_use]
    pub fn new(at: SessionTime, text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self {
            at,
            text: text.to_owned(),
        })
    }

    /// When `n` was pressed.
    #[must_use]
    pub fn at(&self) -> SessionTime {
        self.at
    }

    /// What was typed, trimmed; never empty.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
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
            Self::Note(note) => note.at(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_trimmed_and_never_blank() {
        let at = SessionTime::from_nanos(9);
        let note = Note::new(at, "  bring clamps \t").unwrap();
        assert_eq!((note.at(), note.text()), (at, "bring clamps"));
        assert_eq!(Note::new(at, " \n\t "), None);
        assert_eq!(Note::new(at, ""), None);
        assert_eq!(Annotation::Note(note).at(), at);
        assert_eq!(Annotation::Mark(Mark { at }).at(), at);
    }
}
