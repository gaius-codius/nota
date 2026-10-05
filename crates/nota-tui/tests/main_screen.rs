//! Golden test for the Recording screen: the UI spec's `Main` mockup, drawn
//! through `ratatui`'s `TestBackend` and compared with insta. The transcript
//! lines are invented; the mockup's own come from a real workshop.
//!
//! The design is a draft. When the screen changes on purpose, update the
//! mockup and accept the new snapshot (`cargo insta review`) in the same
//! change.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use nota_core::{Clock, FakeClock, SessionTime};
use nota_tui::{Level, Recording, Theme, Update, Utterance};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn secs(s: u64) -> SessionTime {
    SessionTime::from_elapsed(Duration::from_secs(s)).unwrap()
}

fn press(screen: &mut Recording, code: KeyCode) {
    let _ = screen.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn press_at(screen: &mut Recording, code: KeyCode, at: SessionTime) {
    let _ = screen.handle_key_at(KeyEvent::new(code, KeyModifiers::NONE), at);
}

/// The mockup's moment: 1:12:48 into a session, five marks and notes
/// already on the band, the newest two beside the transcript, and a chunk
/// with the engine.
fn main_screen() -> Recording {
    let clock = Arc::new(FakeClock::new(secs(4_368)));
    let mut screen = Recording::new(
        "Woodwork workshop".into(),
        "Brave".into(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Theme::default(),
    );

    // An invented waveform: a level about once per column, from a fixed
    // pseudo-random sequence, in 6 dB steps from full scale down to -60 dBFS.
    let mut seed: u32 = 0x2545_f491;
    for s in (0..=4_368).step_by(70) {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let peak = Level::FULL_SCALE.peak() >> ((seed >> 16) % 10);
        screen.update(Update::Level {
            at: secs(s),
            level: Level::from_peak(peak),
        });
    }

    let lines = [
        (
            4_040,
            4_062,
            "…and of course we need to sand along the grain first. If the board is between eighteen and twenty millimetres we can plane it by hand.",
        ),
        (
            4_063,
            4_070,
            "Anything thicker goes straight through the machine.",
        ),
        (
            4_072,
            4_100,
            "…so the cupping could be from the drying shed, and we did look at that in the last session.",
        ),
        (
            4_140,
            4_170,
            "Remember that for a wide panel you check the moisture, the stain and the clamps before anything else.",
        ),
        (
            4_190,
            4_220,
            "The table is getting full sun most days, so the joints may open up in summer as well.",
        ),
        (
            4_225,
            4_255,
            "Then we have the finish itself, and we want to rule out a reaction, so test the oil on a scrap first…",
        ),
    ];
    for (start, end, text) in lines {
        let utterance = Utterance::new(secs(start), secs(end), text.into()).unwrap();
        screen.update(Update::Text(utterance));
    }
    screen.update(Update::Transcribing(true));
    screen.update(Update::Recorded(14_200_000));

    // Marks and notes at the moments their keys were pressed; a note's text
    // is typed after `n`. The last two fall beside the transcript.
    for (at, note) in [
        (678, None),
        (1_657, Some("ask about the grain filler")),
        (2_486, None),
        (3_465, Some("bring the long clamps")),
        (4_143, None),
        (4_200, Some("sun on the table")),
    ] {
        match note {
            None => press_at(&mut screen, KeyCode::Char('m'), secs(at)),
            Some(text) => {
                press_at(&mut screen, KeyCode::Char('n'), secs(at));
                for c in text.chars() {
                    press_at(&mut screen, KeyCode::Char(c), secs(at + 2));
                }
                press_at(&mut screen, KeyCode::Enter, secs(at + 4));
            }
        }
    }
    screen
}

fn draw(screen: &Recording, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| screen.draw(frame)).unwrap();
    terminal
}

#[test]
fn main() {
    let terminal = draw(&main_screen(), 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn main_while_typing_a_note() {
    let mut screen = main_screen();
    press(&mut screen, KeyCode::Char('n'));
    for c in "check the glue".chars() {
        press(&mut screen, KeyCode::Char(c));
    }
    let mut terminal = draw(&screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
    // The cursor sits after the typed text in the bottom border.
    let cursor = terminal.get_cursor_position().unwrap();
    assert_eq!((cursor.x, cursor.y), (5 + 14, 19));
}

#[test]
fn too_small() {
    let terminal = draw(&main_screen(), 59, 20);
    insta::assert_snapshot!(terminal.backend());
}
