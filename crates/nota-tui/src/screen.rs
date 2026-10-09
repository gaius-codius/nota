//! The Recording screen's state: what it shows and how keys change it.

use std::sync::Arc;
use std::time::Duration;

use nota_core::recorder::{Command, Event, Mark, Note};
use nota_core::{Clock, SessionTime, Utterance};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::annotation::Annotation;
use crate::band::LevelHistory;
use crate::text::{has_visible_text, is_drawn};
use crate::theme::Theme;

/// How far ahead of the clock a level may be stamped. Levels stamped later
/// than that are dropped: the band's history grows to the latest level, so
/// one bad timestamp mustn't be able to allocate without bound.
const LEVEL_LEAD: Duration = Duration::from_secs(5);

/// The longest note, in characters. A note is one line; this only keeps a
/// stuck key from growing it without bound.
const MAX_NOTE_CHARS: usize = 500;

/// How soon after the stop question opens a `y` counts as typing rather
/// than an answer. A word typed without `n` first, like "system", opens the
/// question with its `s` and would confirm it with the `y` straight after.
pub(crate) const STOP_GUARD: Duration = Duration::from_millis(500);

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
    /// `s` or Ctrl+C was pressed at `since`: the footer asks whether to
    /// stop.
    Asking {
        /// When the question opened, for [`STOP_GUARD`].
        since: SessionTime,
    },
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

    /// Applies what the recorder reports. A level stamped more than 5 s
    /// ahead of the clock is dropped. Closing on [`Event::Stopping`] and
    /// [`Event::Stopped`] is [`run`](crate::run())'s business.
    pub fn update(&mut self, event: Event) {
        match event {
            Event::Level { at, level, .. } => {
                let limit = self.clock.now().checked_add(LEVEL_LEAD);
                if limit.is_none_or(|limit| at <= limit) {
                    self.levels.record(at, level);
                }
            }
            // Nothing to draw, and it would take the margin from the
            // utterance before it.
            Event::Text(utterance) if !has_visible_text(utterance.text()) => {}
            Event::Text(utterance) => {
                // Two tracks' text can arrive out of order; keep it sorted by
                // start, after any that started at the same time.
                let index = self
                    .utterances
                    .partition_point(|other| other.start() <= utterance.start());
                self.utterances.insert(index, utterance);
            }
            Event::Transcribing(transcribing) => self.transcribing = transcribing,
            Event::Recorded(bytes) => self.recorded_bytes = bytes,
            // Not shown yet. Warnings, device changes, the disk and the
            // transcriber's state get their words and their place on the
            // band with the UI spec's pending changes; durable progress,
            // epochs and gaps go on the band with them.
            Event::Engine(_)
            | Event::Warning(_)
            | Event::Device { .. }
            | Event::Disk(_)
            | Event::Durable { .. }
            | Event::Epoch { .. }
            | Event::Gap { .. }
            | Event::Stopping
            | Event::Stopped(_) => {}
        }
    }

    /// Handles a key press, as if it was pressed now. Returns the command
    /// it gives the recorder: a mark or note to store, or the stop.
    ///
    /// - `m` adds a mark at once, never asking.
    /// - `n` starts a note pinned to this moment. Typing fills it, `⏎` saves
    ///   it, `esc` drops it, and a blank note is dropped too.
    /// - `s` asks whether to stop the recording, and so does Ctrl+C, even
    ///   while typing a note. `y` stops it, unless it comes within half a
    ///   second of the question opening: that's typing, not an answer. Any
    ///   other key keeps recording (and does nothing else), back to the note
    ///   if one was being typed, except the keys that asked: `s` and Ctrl+C
    ///   leave the question open as it was, so a double press or a held key
    ///   neither answers nor dismisses it.
    ///
    /// All of them work with Caps Lock on.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Command> {
        self.handle_key_at(key, self.clock.now())
    }

    /// Handles a key pressed at session time `at`, which a mark or a new
    /// note is pinned to. See [`Recording::handle_key`].
    pub fn handle_key_at(&mut self, key: KeyEvent, at: SessionTime) -> Option<Command> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let plain = !key.modifiers.intersects(
            KeyModifiers::CONTROL
                | KeyModifiers::ALT
                | KeyModifiers::SUPER
                | KeyModifiers::HYPER
                | KeyModifiers::META,
        );
        match self.stop {
            Stop::No => {}
            Stop::Asking { since } => {
                let answered = at
                    .checked_duration_since(since)
                    .is_some_and(|after| after >= STOP_GUARD);
                self.stop = match key.code {
                    KeyCode::Char('y' | 'Y') if plain && answered => Stop::Confirmed,
                    // Asked again, as by a held `s` repeating: still asking,
                    // and still since the first.
                    KeyCode::Char('s' | 'S') if plain => self.stop,
                    _ if is_ctrl_c(key) => self.stop,
                    // Only reported once the terminal's keyboard enhancement
                    // flags are on, which nota doesn't turn on yet: Shift
                    // pressed for a capital Y isn't an answer.
                    KeyCode::Modifier(_) => self.stop,
                    _ => Stop::No,
                };
                return (self.stop == Stop::Confirmed).then_some(Command::Stop);
            }
            // The screen is closing: nothing more to add.
            Stop::Confirmed => return None,
        }
        if is_ctrl_c(key) {
            self.stop = Stop::Asking { since: at };
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
                let mark = Mark { at };
                self.annotations.push(Annotation::Mark(mark));
                Some(Command::Mark(mark))
            }
            KeyCode::Char('n' | 'N') => {
                self.draft = Some(Draft {
                    at,
                    text: String::new(),
                });
                None
            }
            KeyCode::Char('s' | 'S') => {
                self.stop = Stop::Asking { since: at };
                None
            }
            _ => None,
        }
    }

    /// The marks and notes added so far, in session-time order.
    #[cfg(test)]
    pub(crate) fn annotations(&self) -> &[Annotation] {
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
        matches!(self.stop, Stop::Asking { .. })
    }

    /// Whether `y` has confirmed the stop. Once true it stays true, and the
    /// screen takes no more keys.
    #[must_use]
    pub fn stop_confirmed(&self) -> bool {
        self.stop == Stop::Confirmed
    }

    /// Saves the note being typed, if there is one and it isn't blank, as if
    /// `⏎` had been pressed. For when the screen closes mid-note.
    pub fn save_draft(&mut self) -> Option<Command> {
        let draft = self.draft.take()?;
        let note = Note::new(draft.at, &draft.text)?;
        self.annotations.push(Annotation::Note(note.clone()));
        Some(Command::Note(note))
    }
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use nota_core::recorder::Level;
    use nota_core::{FakeClock, TrackId};
    use ratatui::crossterm::event::{KeyEventState, ModifierKeyCode};

    use super::*;
    use crate::text::heard;

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
        assert_eq!(mark, Some(Command::Mark(Mark { at: secs(90) })));
        clock.advance(Duration::from_millis(1_500));
        let mark = screen.handle_key(press(KeyCode::Char('m')));
        let later = secs(90).checked_add(Duration::from_millis(1_500)).unwrap();
        assert_eq!(mark, Some(Command::Mark(Mark { at: later })));
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
        let expected = Note::new(secs(600), "ask about mn").unwrap();
        assert_eq!(note, Some(Command::Note(expected.clone())));
        assert_eq!(screen.annotations(), [Annotation::Note(expected)]);
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
            Some(Command::Note(Note::new(secs(1), "abc").unwrap()))
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
        let Some(Command::Note(note)) = screen.handle_key(press(KeyCode::Enter)) else {
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
        assert_eq!(mark, Some(Command::Mark(Mark { at: secs(40) })));
        screen.handle_key_at(press(KeyCode::Char('N')), secs(41));
        type_text(&mut screen, "x");
        let note = screen.handle_key_at(press(KeyCode::Enter), secs(99));
        assert_eq!(note, Some(Command::Note(Note::new(secs(41), "x").unwrap())));
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    #[test]
    fn s_asks_before_stopping_and_y_stops() {
        let (mut screen, clock) = screen_at(secs(1));
        assert_eq!(screen.handle_key(press(KeyCode::Char('s'))), None);
        assert!(screen.is_confirming_stop());
        assert!(!screen.stop_confirmed());
        assert!(screen.annotations().is_empty());
        clock.advance(STOP_GUARD);
        assert_eq!(
            screen.handle_key(press(KeyCode::Char('y'))),
            Some(Command::Stop)
        );
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
        let (mut screen, clock) = screen_at(secs(1));
        screen.handle_key(press(KeyCode::Char('S')));
        assert!(screen.is_confirming_stop());
        clock.advance(STOP_GUARD);
        assert_eq!(
            screen.handle_key(press(KeyCode::Char('Y'))),
            Some(Command::Stop)
        );
        assert!(screen.stop_confirmed());
    }

    #[test]
    fn any_other_key_keeps_recording_and_does_nothing_else() {
        for key in [
            press(KeyCode::Char('n')),
            press(KeyCode::Char('N')),
            press(KeyCode::Esc),
            press(KeyCode::Char('m')),
            press(KeyCode::Char('x')),
            press(KeyCode::Enter),
            press(KeyCode::Backspace),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::SUPER),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::META),
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT),
        ] {
            let (mut screen, clock) = screen_at(secs(1));
            screen.handle_key(press(KeyCode::Char('s')));
            clock.advance(STOP_GUARD);
            assert_eq!(screen.handle_key(key), None, "{key:?}");
            assert!(!screen.is_confirming_stop(), "{key:?}");
            assert!(!screen.stop_confirmed(), "{key:?}");
            // The key only answered the question: `n` didn't start a note,
            // `m` didn't mark.
            assert!(!screen.is_typing_note(), "{key:?}");
            assert!(screen.annotations().is_empty(), "{key:?}");
            // And the keys work again.
            assert!(screen.handle_key(press(KeyCode::Char('m'))).is_some());
        }
    }

    #[test]
    fn a_held_s_keeps_asking_from_the_first_press() {
        let (mut screen, clock) = screen_at(secs(1));
        screen.handle_key(press(KeyCode::Char('s')));
        // Key repeat: a press every 30 ms after a 300 ms delay.
        clock.advance(Duration::from_millis(300));
        for _ in 0..10 {
            screen.handle_key(press(KeyCode::Char('s')));
            assert!(screen.is_confirming_stop());
            clock.advance(Duration::from_millis(30));
        }
        screen.handle_key(press(KeyCode::Char('S')));
        assert!(screen.is_confirming_stop());
        // 630 ms after the first `s`, though only 30 after the last.
        screen.handle_key(press(KeyCode::Char('y')));
        assert!(screen.stop_confirmed());
    }

    #[test]
    fn a_modifier_alone_leaves_the_question_open() {
        let (mut screen, clock) = screen_at(secs(1));
        screen.handle_key(press(KeyCode::Char('s')));
        clock.advance(STOP_GUARD);
        screen.handle_key(KeyEvent::new(
            KeyCode::Modifier(ModifierKeyCode::LeftShift),
            KeyModifiers::SHIFT,
        ));
        assert!(screen.is_confirming_stop());
        screen.handle_key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT));
        assert!(screen.stop_confirmed());
    }

    #[test]
    fn y_straight_after_the_question_is_typing_and_keeps_recording() {
        let after = |ms| secs(10).checked_add(Duration::from_millis(ms)).unwrap();
        let just_under = STOP_GUARD.checked_sub(Duration::from_nanos(1)).unwrap();
        for (y_at, stops) in [
            (secs(10), false),
            (after(50), false),
            (secs(10).checked_add(just_under).unwrap(), false),
            (secs(10).checked_add(STOP_GUARD).unwrap(), true),
            (after(4_000), true),
            // Stamped before the question: never an answer.
            (secs(9), false),
        ] {
            let (mut screen, _clock) = screen_at(secs(10));
            screen.handle_key_at(press(KeyCode::Char('s')), secs(10));
            screen.handle_key_at(press(KeyCode::Char('y')), y_at);
            assert_eq!(screen.stop_confirmed(), stops, "{y_at:?}");
            assert!(!screen.is_confirming_stop(), "{y_at:?}");
        }
    }

    #[test]
    fn typing_a_word_without_n_first_never_stops() {
        for word in ["system", "symbol", "syringe", "easy", "stay", "SYSTEM"] {
            let (mut screen, clock) = screen_at(secs(1));
            for c in word.chars() {
                screen.handle_key(press(KeyCode::Char(c)));
                clock.advance(Duration::from_millis(80));
            }
            assert!(!screen.stop_confirmed(), "{word}");
            assert!(!screen.is_confirming_stop(), "{word}");
        }
    }

    #[test]
    fn ctrl_c_asks_and_a_second_does_not_stop() {
        let (mut screen, clock) = screen_at(secs(1));
        screen.handle_key(ctrl_c());
        assert!(screen.is_confirming_stop());
        clock.advance(STOP_GUARD);
        screen.handle_key(ctrl_c());
        assert!(screen.is_confirming_stop());
        assert!(!screen.stop_confirmed());
        // It didn't open the question again either: `y` still answers.
        screen.handle_key(press(KeyCode::Char('y')));
        assert!(screen.stop_confirmed());
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
        // `x` answers the question; it isn't typed into the note.
        screen.handle_key(press(KeyCode::Char('x')));
        assert!(!screen.is_confirming_stop());
        assert!(screen.is_typing_note());
        type_text(&mut screen, " done");
        assert_eq!(
            screen.handle_key(press(KeyCode::Enter)),
            Some(Command::Note(Note::new(secs(5), "half done").unwrap()))
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
            Some(Command::Note(Note::new(secs(5), "sS").unwrap()))
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
            screen.update(Event::Text(heard(1, 2, text)));
        }
        assert!(screen.utterances.is_empty());
    }

    #[test]
    fn text_is_kept_in_start_order() {
        let (mut screen, _clock) = screen_at(secs(1));
        let at = |start, text: &str| Event::Text(heard(start, start + 1, text));
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
        let note = Note::new(secs(30), "half typed").unwrap();
        assert_eq!(screen.save_draft(), Some(Command::Note(note.clone())));
        assert_eq!(screen.annotations(), [Annotation::Note(note)]);
        assert!(!screen.is_typing_note());
        screen.handle_key(press(KeyCode::Char('n')));
        assert_eq!(screen.save_draft(), None);
        assert!(!screen.is_typing_note());
    }

    #[test]
    fn levels_far_ahead_of_the_clock_are_dropped() {
        let (mut screen, _clock) = screen_at(secs(10));
        let level = Level::from_peak(100);
        screen.update(Event::Level {
            track: TrackId::new(0),
            at: secs(15),
            level,
        });
        screen.update(Event::Level {
            track: TrackId::new(0),
            at: SessionTime::from_nanos(u64::MAX),
            level,
        });
        screen.update(Event::Level {
            track: TrackId::new(0),
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
        screen.update(Event::Transcribing(true));
        screen.update(Event::Recorded(42));
        assert!(screen.transcribing);
        assert_eq!(screen.recorded_bytes, 42);
        screen.update(Event::Transcribing(false));
        assert!(!screen.transcribing);
    }
}
