//! The Recording screen's state: what it shows and how keys change it.

use std::sync::Arc;
use std::time::Duration;

use nota_core::{Clock, SessionTime};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::annotation::{Annotation, Mark, Note};
use crate::band::LevelHistory;
use crate::level::Level;
use crate::text::{Utterance, has_visible_text, is_drawn};
use crate::theme::Theme;

/// How far ahead of the clock a level may be stamped. Levels stamped later
/// than that are dropped: the band's history grows to the latest level, so
/// one bad timestamp mustn't be able to allocate without bound.
const LEVEL_LEAD: Duration = Duration::from_secs(5);

/// The longest note, in characters. A note is one line; this only keeps a
/// stuck key from growing it without bound.
const MAX_NOTE_CHARS: usize = 500;

/// News from the rest of nota for the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// The level heard at a moment of the session. Send one at least every
    /// 250 ms while capturing: a stretch with no level draws as a gap in the
    /// band. A level stamped more than 5 s ahead of the clock is dropped.
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
    /// The note being typed, if `n` was pressed. Kept while the stop
    /// question is open.
    pub(crate) draft: Option<Draft>,
    pub(crate) stop: Stop,
}

/// How far stopping has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Not asked.
    No,
    /// `s` or Ctrl+C was pressed: the footer asks whether to stop.
    Asking,
    /// `y` answered the question: the recording is to stop.
    Confirmed,
}

/// A note being typed: pinned to the moment `n` was pressed.
#[derive(Debug)]
pub(crate) struct Draft {
    pub(crate) at: SessionTime,
    pub(crate) text: String,
}

impl Recording {
    /// A Recording screen for the session `title`, capturing from `source`
    /// (shown in the footer), timed by the session `clock`. Control and
    /// bidirectional formatting characters in either are dropped, as in
    /// the transcript, so they can't reorder or break the row.
    #[must_use]
    pub fn new(title: String, source: String, clock: Arc<dyn Clock>, theme: Theme) -> Self {
        let drawn = |text: String| text.chars().filter(|&c| is_drawn(c)).collect();
        Self {
            title: drawn(title),
            source: drawn(source),
            clock,
            theme,
            levels: LevelHistory::default(),
            utterances: Vec::new(),
            transcribing: false,
            recorded_bytes: 0,
            annotations: Vec::new(),
            draft: None,
            stop: Stop::No,
        }
    }

    /// Applies news from the rest of nota.
    pub fn update(&mut self, update: Update) {
        match update {
            Update::Level { at, level } => {
                let limit = self.clock.now().checked_add(LEVEL_LEAD);
                if limit.is_none_or(|limit| at <= limit) {
                    self.levels.record(at, level);
                }
            }
            // Nothing to draw, and it would take the margin from the
            // utterance before it.
            Update::Text(utterance) if !has_visible_text(utterance.text()) => {}
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

    /// Handles a key press, as if it was pressed now. Returns the mark or
    /// note it added, to be stored.
    ///
    /// - `m` adds a mark at once, never asking.
    /// - `n` starts a note pinned to this moment. Typing fills it, `⏎` saves
    ///   it, `esc` drops it, and a blank note is dropped too.
    /// - `s` asks whether to stop the recording, and so does Ctrl+C, even
    ///   while typing a note. `y` stops it; `n` or `esc` keeps recording,
    ///   back to the note if one was being typed. Every other key is ignored
    ///   while asking, Ctrl+C too, so only `y` can end a lecture.
    ///
    /// All of them work with Caps Lock on.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Annotation> {
        self.handle_key_at(key, self.clock.now())
    }

    /// Handles a key pressed at session time `at`, which a mark or a new
    /// note is pinned to. See [`Recording::handle_key`].
    pub fn handle_key_at(&mut self, key: KeyEvent, at: SessionTime) -> Option<Annotation> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match self.stop {
            Stop::No => {}
            Stop::Asking => {
                match key.code {
                    KeyCode::Char('y' | 'Y') if plain => self.stop = Stop::Confirmed,
                    KeyCode::Char('n' | 'N') if plain => self.stop = Stop::No,
                    KeyCode::Esc => self.stop = Stop::No,
                    _ => {}
                }
                return None;
            }
            // The screen is closing: nothing more to add.
            Stop::Confirmed => return None,
        }
        if matches!(key.code, KeyCode::Char('c' | 'C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.stop = Stop::Asking;
            return None;
        }
        if let Some(draft) = &mut self.draft {
            match key.code {
                KeyCode::Enter => return self.save_draft(),
                KeyCode::Esc => self.draft = None,
                KeyCode::Backspace => {
                    draft.text.pop();
                }
                KeyCode::Char(c)
                    if is_drawn(c) && plain && draft.text.chars().count() < MAX_NOTE_CHARS =>
                {
                    draft.text.push(c);
                }
                _ => {}
            }
            return None;
        }
        if !plain {
            return None;
        }
        match key.code {
            KeyCode::Char('m' | 'M') => {
                let mark = Annotation::Mark(Mark { at });
                self.annotations.push(mark.clone());
                Some(mark)
            }
            KeyCode::Char('n' | 'N') => {
                self.draft = Some(Draft {
                    at,
                    text: String::new(),
                });
                None
            }
            KeyCode::Char('s' | 'S') => {
                self.stop = Stop::Asking;
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

    /// Whether the footer is asking whether to stop the recording.
    #[must_use]
    pub fn is_confirming_stop(&self) -> bool {
        self.stop == Stop::Asking
    }

    /// Whether `y` has confirmed the stop. Once true it stays true, and the
    /// screen takes no more keys.
    #[must_use]
    pub fn stop_confirmed(&self) -> bool {
        self.stop == Stop::Confirmed
    }

    /// Saves the note being typed, if there is one and it isn't blank, as if
    /// `⏎` had been pressed. For when the screen closes mid-note.
    pub fn save_draft(&mut self) -> Option<Annotation> {
        let draft = self.draft.take()?;
        let note = Annotation::Note(Note::new(draft.at, &draft.text)?);
        self.annotations.push(note.clone());
        Some(note)
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn the_title_and_source_keep_only_what_is_drawn() {
        let screen = Recording::new(
            "Lab\u{202e}3\u{7}".into(),
            "mic\u{2066} + system".into(),
            Arc::new(FakeClock::new(SessionTime::ZERO)),
            Theme::no_color(),
        );
        assert_eq!(screen.title, "Lab3");
        assert_eq!(screen.source, "mic + system");
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
        let expected = Annotation::Note(Note::new(secs(600), "ask about mn").unwrap());
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
            Some(Annotation::Note(Note::new(secs(1), "abc").unwrap()))
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
        assert_eq!(note.text().chars().count(), MAX_NOTE_CHARS);
        assert!(note.text().starts_with("Za"));
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
    fn keys_carry_their_own_time() {
        let (mut screen, _clock) = screen_at(secs(100));
        let mark = screen.handle_key_at(press(KeyCode::Char('M')), secs(40));
        assert_eq!(mark, Some(Annotation::Mark(Mark { at: secs(40) })));
        screen.handle_key_at(press(KeyCode::Char('N')), secs(41));
        type_text(&mut screen, "x");
        let note = screen.handle_key_at(press(KeyCode::Enter), secs(99));
        assert_eq!(
            note,
            Some(Annotation::Note(Note::new(secs(41), "x").unwrap()))
        );
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    #[test]
    fn s_asks_before_stopping_and_y_stops() {
        let (mut screen, _clock) = screen_at(secs(1));
        assert_eq!(screen.handle_key(press(KeyCode::Char('s'))), None);
        assert!(screen.is_confirming_stop());
        assert!(!screen.stop_confirmed());
        assert!(screen.annotations().is_empty());
        assert_eq!(screen.handle_key(press(KeyCode::Char('y'))), None);
        assert!(screen.stop_confirmed());
        assert!(!screen.is_confirming_stop());
        // It stays stopped, and takes no more keys.
        assert_eq!(screen.handle_key(press(KeyCode::Char('m'))), None);
        assert_eq!(screen.handle_key(press(KeyCode::Char('n'))), None);
        assert!(screen.stop_confirmed());
        assert!(screen.annotations().is_empty());
        assert!(!screen.is_typing_note());
    }

    #[test]
    fn caps_lock_stops_too() {
        let (mut screen, _clock) = screen_at(secs(1));
        screen.handle_key(press(KeyCode::Char('S')));
        assert!(screen.is_confirming_stop());
        screen.handle_key(press(KeyCode::Char('Y')));
        assert!(screen.stop_confirmed());
    }

    #[test]
    fn n_and_esc_keep_recording() {
        for cancel in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
            let (mut screen, _clock) = screen_at(secs(1));
            screen.handle_key(press(KeyCode::Char('s')));
            assert_eq!(screen.handle_key(press(cancel)), None);
            assert!(!screen.is_confirming_stop(), "{cancel:?}");
            assert!(!screen.stop_confirmed(), "{cancel:?}");
            // `n` answered the question; it didn't start a note.
            assert!(!screen.is_typing_note(), "{cancel:?}");
            // And the keys work again.
            assert!(screen.handle_key(press(KeyCode::Char('m'))).is_some());
        }
    }

    #[test]
    fn other_keys_do_nothing_while_asking() {
        let (mut screen, _clock) = screen_at(secs(1));
        screen.handle_key(press(KeyCode::Char('s')));
        for key in [
            press(KeyCode::Char('m')),
            press(KeyCode::Char('x')),
            press(KeyCode::Char('s')),
            press(KeyCode::Enter),
            ctrl_c(),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT),
        ] {
            assert_eq!(screen.handle_key(key), None, "{key:?}");
            assert!(screen.is_confirming_stop(), "{key:?}");
            assert!(!screen.stop_confirmed(), "{key:?}");
        }
        assert!(screen.annotations().is_empty());
        assert!(!screen.is_typing_note());
    }

    #[test]
    fn ctrl_c_asks_and_a_second_does_not_stop() {
        let (mut screen, _clock) = screen_at(secs(1));
        screen.handle_key(ctrl_c());
        assert!(screen.is_confirming_stop());
        screen.handle_key(ctrl_c());
        assert!(screen.is_confirming_stop());
        assert!(!screen.stop_confirmed());
        // Caps Lock on.
        let (mut screen, _clock) = screen_at(secs(1));
        screen.handle_key(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert!(screen.is_confirming_stop());
    }

    #[test]
    fn ctrl_c_while_typing_keeps_the_note() {
        let (mut screen, _clock) = screen_at(secs(5));
        screen.handle_key(press(KeyCode::Char('n')));
        type_text(&mut screen, "half");
        screen.handle_key(ctrl_c());
        assert!(screen.is_confirming_stop());
        // Not typed into the note while asking.
        screen.handle_key(press(KeyCode::Char('x')));
        screen.handle_key(press(KeyCode::Esc));
        assert!(!screen.is_confirming_stop());
        assert!(screen.is_typing_note());
        type_text(&mut screen, " done");
        assert_eq!(
            screen.handle_key(press(KeyCode::Enter)),
            Some(Annotation::Note(Note::new(secs(5), "half done").unwrap()))
        );
    }

    #[test]
    fn s_is_a_letter_in_a_note() {
        let (mut screen, _clock) = screen_at(secs(5));
        screen.handle_key(press(KeyCode::Char('n')));
        type_text(&mut screen, "sS");
        assert!(!screen.is_confirming_stop());
        assert_eq!(
            screen.handle_key(press(KeyCode::Enter)),
            Some(Annotation::Note(Note::new(secs(5), "sS").unwrap()))
        );
    }

    #[test]
    fn releases_do_not_ask_or_answer() {
        let release = |code| {
            KeyEvent::new_with_kind_and_state(
                code,
                KeyModifiers::NONE,
                KeyEventKind::Release,
                KeyEventState::NONE,
            )
        };
        let (mut screen, _clock) = screen_at(secs(1));
        screen.handle_key(release(KeyCode::Char('s')));
        assert!(!screen.is_confirming_stop());
        screen.handle_key(press(KeyCode::Char('s')));
        screen.handle_key(release(KeyCode::Char('y')));
        assert!(screen.is_confirming_stop());
        assert!(!screen.stop_confirmed());
    }

    #[test]
    fn blank_text_is_not_kept() {
        let (mut screen, _clock) = screen_at(secs(1));
        for text in ["", "  ", "\u{202e}\u{7}"] {
            let utterance = Utterance::new(secs(1), secs(2), text.into()).unwrap();
            screen.update(Update::Text(utterance));
        }
        assert!(screen.utterances.is_empty());
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
    fn a_draft_can_be_saved_without_enter() {
        let (mut screen, clock) = screen_at(secs(30));
        assert_eq!(screen.save_draft(), None);
        screen.handle_key(press(KeyCode::Char('n')));
        type_text(&mut screen, " half typed ");
        clock.advance(Duration::from_secs(3));
        let note = Annotation::Note(Note::new(secs(30), "half typed").unwrap());
        assert_eq!(screen.save_draft(), Some(note.clone()));
        assert_eq!(screen.annotations(), [note]);
        assert!(!screen.is_typing_note());
        screen.handle_key(press(KeyCode::Char('n')));
        assert_eq!(screen.save_draft(), None);
        assert!(!screen.is_typing_note());
    }

    #[test]
    fn levels_far_ahead_of_the_clock_are_dropped() {
        let (mut screen, _clock) = screen_at(secs(10));
        let level = Level::from_peak(100);
        screen.update(Update::Level {
            at: secs(15),
            level,
        });
        screen.update(Update::Level {
            at: SessionTime::from_nanos(u64::MAX),
            level,
        });
        screen.update(Update::Level {
            at: secs(15).checked_add(Duration::from_nanos(1)).unwrap(),
            level,
        });
        // Only the first was kept: one column, the last of 61 bins up to 15 s.
        let columns = screen.levels.columns(secs(15), 61);
        assert_eq!(columns.iter().flatten().count(), 1);
        assert_eq!(columns[60], Some(level));
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
