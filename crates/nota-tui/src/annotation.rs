//! Marks and notes, as the screen keeps them to draw on the band. They're
//! sent on as [`Command`](nota_core::recorder::Command)s.

use nota_core::SessionTime;
use nota_core::recorder::{Mark, Note};

/// A mark or a note the listener added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Annotation {
    /// A mark (◆).
    Mark(Mark),
    /// A note (◇).
    Note(Note),
}

impl Annotation {
    /// The session time it's pinned to.
    pub(crate) fn at(&self) -> SessionTime {
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
    fn each_is_pinned_to_its_moment() {
        let at = SessionTime::from_nanos(9);
        let note = Note::new(at, "bring clamps").unwrap();
        assert_eq!(Annotation::Note(note).at(), at);
        assert_eq!(Annotation::Mark(Mark { at }).at(), at);
    }
}
