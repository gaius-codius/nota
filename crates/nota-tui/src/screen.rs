//! The Recording screen's state: what it shows and how keys change it.

use std::sync::Arc;

use nota_core::{Clock, SessionTime};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::annotation::{Annotation, Mark, Note};
use crate::band::LevelHistory;
use crate::level::Level;
use crate::text::Utterance;
use crate::theme::Theme;

/// The longest note, in characters. A note is one line; this only keeps a
/// stuck key from growing it without bound.
const MAX_NOTE_CHARS: usize = 500;

/// News from the rest of nota for the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// The level heard at a moment of the session.
    Level {
        /// When it was heard.
        at: SessionTime,
        /// How loud it was.
        level: Level,
    },
    /// New live text.
    Text(Utterance),
    /// Whether a chunk of speech is with the engine, not yet text (drawn as
    /// `░░░` after the transcript).
    Transcribing(bool),
    /// How much of the recording is on disk so far, in bytes.
    Recorded(u64),
}

/// The Recording screen (the UI spec's `Main`).
#[derive(Debug)]
pub struct Recording {
    pub(crate) title: String,
    pub(crate) source: String,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) theme: Theme,
    pub(crate) levels: LevelHistory,
    /// Sorted by start time.
    pub(crate) utterances: Vec<Utterance>,
    pub(crate) transcribing: bool,
    pub(crate) recorded_bytes: u64,
    /// In the order they were added, which is session-time order: each is
    /// stamped with the clock, which never goes back.
    pub(crate) annotations: Vec<Annotation>,
    /// The note being typed, if `n` was pressed.
    pub(crate) draft: Option<Draft>,
}

/// A note being typed: pinned to the moment `n` was pressed.
#[derive(Debug)]
pub(crate) struct Draft {
    pub(crate) at: SessionTime,
    pub(crate) text: String,
}

impl Recording {
    /// A Recording screen for the session `title`, capturing from `source`
    /// (shown in the footer), timed by the session `clock`.
    #[must_use]
    pub fn new(title: String, source: String, clock: Arc<dyn Clock>, theme: Theme) -> Self {
        Self {
            title,
            source,
            clock,
            theme,
            levels: LevelHistory::default(),
            utterances: Vec::new(),
            transcribing: false,
            recorded_bytes: 0,
            annotations: Vec::new(),
            draft: None,
        }
    }

    /// Applies news from the rest of nota.
    pub fn update(&mut self, update: Update) {
        match update {
            Update::Level { at, level } => self.levels.record(at, level),
            Update::Text(utterance) => {
                // Two tracks' text can arrive out of order; keep it sorted by
                // start, after any that started at the same time.
                let index = self
                    .utterances
                    .partition_point(|other| other.start() <= utterance.start());
                self.utterances.insert(index, utterance);
            }
            Update::Transcribing(transcribing) => self.transcribing = transcribing,
            Update::Recorded(bytes) => self.recorded_bytes = bytes,
        }
    }

    /// Handles a key press. Returns the mark or note it added, to be stored.
    ///
    /// - `m` adds a mark at once, never asking.
    /// - `n` starts a note pinned to this moment. Typing fills it, `⏎` saves
    ///   it, `esc` drops it, and a blank note is dropped too.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Annotation> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if let Some(draft) = &mut self.draft {
            match key.code {
                KeyCode::Enter => return self.finish_note(),
                KeyCode::Esc => self.draft = None,
                KeyCode::Backspace => {
                    draft.text.pop();
                }
                KeyCode::Char(c)
                    if !c.is_control()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && draft.text.chars().count() < MAX_NOTE_CHARS =>
                {
                    draft.text.push(c);
                }
                _ => {}
            }
            return None;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('m') => {
                let mark = Annotation::Mark(Mark {
                    at: self.clock.now(),
                });
                self.annotations.push(mark.clone());
                Some(mark)
            }
            KeyCode::Char('n') => {
                self.draft = Some(Draft {
                    at: self.clock.now(),
                    text: String::new(),
                });
                None
            }
            _ => None,
        }
    }

    /// The marks and notes added so far, in session-time order.
    #[must_use]
    pub fn annotations(&self) -> &[Annotation] {
        &self.annotations
    }

    /// Whether a note is being typed.
    #[must_use]
    pub fn is_typing_note(&self) -> bool {
        self.draft.is_some()
    }

    fn finish_note(&mut self) -> Option<Annotation> {
        let draft = self.draft.take()?;
        let text = draft.text.trim();
        if text.is_empty() {
            return None;
        }
        let note = Annotation::Note(Note {
            at: draft.at,
            text: text.to_owned(),
        });
        self.annotations.push(note.clone());
        Some(note)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nota_core::FakeClock;
    use ratatui::crossterm::event::KeyEventState;

    use super::*;

    fn secs(s: u64) -> SessionTime {
        SessionTime::from_elapsed(Duration::from_secs(s)).unwrap()
    }

    fn screen_at(start: SessionTime) -> (Recording, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::new(start));
        let screen = Recording::new(
            "Test".into(),
            "Mic".into(),
            Arc::clone(&clock) as Arc<dyn Clock>,
            Theme::no_color(),
        );
        (screen, clock)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(screen: &mut Recording, text: &str) {
        for c in text.chars() {
            assert_eq!(screen.handle_key(press(KeyCode::Char(c))), None);
        }
    }

    #[test]
    fn m_marks_the_moment_it_is_pressed() {
        let (mut screen, clock) = screen_at(secs(90));
        let mark = screen.handle_key(press(KeyCode::Char('m')));
        assert_eq!(mark, Some(Annotation::Mark(Mark { at: secs(90) })));
        clock.advance(Duration::from_millis(1_500));
        let mark = screen.handle_key(press(KeyCode::Char('m')));
        let later = secs(90).checked_add(Duration::from_millis(1_500)).unwrap();
        assert_eq!(mark, Some(Annotation::Mark(Mark { at: later })));
        assert_eq!(
            screen.annotations(),
            [
                Annotation::Mark(Mark { at: secs(90) }),
                Annotation::Mark(Mark { at: later })
            ]
        );
    }

    #[test]
    fn a_note_is_pinned_to_when_n_was_pressed_not_when_it_was_saved() {
        let (mut screen, clock) = screen_at(secs(600));
        assert_eq!(screen.handle_key(press(KeyCode::Char('n'))), None);
        assert!(screen.is_typing_note());
        clock.advance(Duration::from_secs(20));
        // `m` and `n` are text while typing.
        type_text(&mut screen, "ask about mn");
        clock.advance(Duration::from_secs(5));
        let note = screen.handle_key(press(KeyCode::Enter));
        let expected = Annotation::Note(Note {
            at: secs(600),
            text: "ask about mn".into(),
        });
        assert_eq!(note, Some(expected.clone()));
        assert_eq!(screen.annotations(), [expected]);
        assert!(!screen.is_typing_note());
    }

    #[test]
    fn notes_can_be_edited_cancelled_and_are_never_blank() {
        let (mut screen, _clock) = screen_at(secs(1));
        screen.handle_key(press(KeyCode::Char('n')));
        type_text(&mut screen, "abx");
        screen.handle_key(press(KeyCode::Backspace));
        type_text(&mut screen, "c ");
        assert_eq!(
            screen.handle_key(press(KeyCode::Enter)),
            Some(Annotation::Note(Note {
                at: secs(1),
                text: "abc".into()
            }))
        );

        screen.handle_key(press(KeyCode::Char('n')));
        type_text(&mut screen, "dropped");
        assert_eq!(screen.handle_key(press(KeyCode::Esc)), None);
        assert!(!screen.is_typing_note());

        screen.handle_key(press(KeyCode::Char('n')));
        type_text(&mut screen, "   ");
        assert_eq!(screen.handle_key(press(KeyCode::Enter)), None);
        assert!(!screen.is_typing_note());
        assert_eq!(screen.annotations().len(), 1);
    }

    #[test]
    fn notes_ignore_control_keys_and_stop_at_the_cap() {
        let (mut screen, _clock) = screen_at(secs(1));
        screen.handle_key(press(KeyCode::Char('n')));
        screen.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        screen.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT));
        screen.handle_key(press(KeyCode::Char('\u{7}')));
        screen.handle_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT));
        type_text(&mut screen, &"a".repeat(MAX_NOTE_CHARS + 10));
        let Some(Annotation::Note(note)) = screen.handle_key(press(KeyCode::Enter)) else {
            panic!("no note");
        };
        assert_eq!(note.text.chars().count(), MAX_NOTE_CHARS);
        assert!(note.text.starts_with("Za"));
    }

    #[test]
    fn only_presses_count_and_modified_keys_do_nothing() {
        let (mut screen, _clock) = screen_at(secs(1));
        let release = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('m'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
            KeyEventState::NONE,
        );
        assert_eq!(screen.handle_key(release), None);
        let ctrl_m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL);
        assert_eq!(screen.handle_key(ctrl_m), None);
        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(screen.handle_key(ctrl_n), None);
        assert_eq!(screen.handle_key(press(KeyCode::Char('x'))), None);
        assert!(screen.annotations().is_empty());
        assert!(!screen.is_typing_note());
    }

    #[test]
    fn text_is_kept_in_start_order() {
        let (mut screen, _clock) = screen_at(secs(1));
        let at = |start, text: &str| {
            Update::Text(Utterance::new(secs(start), secs(start + 1), text.into()).unwrap())
        };
        screen.update(at(10, "b"));
        screen.update(at(5, "a"));
        screen.update(at(20, "d"));
        screen.update(at(10, "c"));
        let order: Vec<_> = screen.utterances.iter().map(Utterance::text).collect();
        assert_eq!(order, ["a", "b", "c", "d"]);
    }

    #[test]
    fn updates_set_state() {
        let (mut screen, _clock) = screen_at(secs(1));
        screen.update(Update::Transcribing(true));
        screen.update(Update::Recorded(42));
        assert!(screen.transcribing);
        assert_eq!(screen.recorded_bytes, 42);
        screen.update(Update::Transcribing(false));
        assert!(!screen.transcribing);
    }
}
