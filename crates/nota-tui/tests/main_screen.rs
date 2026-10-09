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

use nota_core::messages::Transcript;
use nota_core::recorder::{Event, Level};
use nota_core::{
    Clock, FakeClock, SampleIndex, SampleRange, SampleRate, SessionTime, TrackId, TrackTimeline,
    Utterance,
};
use nota_tui::{Recording, Theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier};

fn secs(s: u64) -> SessionTime {
    SessionTime::from_elapsed(Duration::from_secs(s)).unwrap()
}

/// What the mic heard from `start` to `end` seconds into the session.
fn heard(start: u64, end: u64, text: &str) -> Utterance {
    let track = TrackId::new(0);
    let mut timeline = TrackTimeline::new(track);
    timeline
        .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, SampleRate::SPEECH)
        .unwrap();
    let sample = |s: u64| SampleIndex::new(s * u64::from(SampleRate::SPEECH.hz()));
    let range = SampleRange::new(sample(start), sample(end)).unwrap();
    let transcript = Transcript::new(track, range, text.to_owned()).unwrap();
    Utterance::place(transcript, &timeline).unwrap()
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
    main_screen_in(Theme::default())
}

/// [`main_screen`] drawn in `theme`.
fn main_screen_in(theme: Theme) -> Recording {
    let clock = Arc::new(FakeClock::new(secs(4_368)));
    let mut screen = Recording::new(
        "Woodwork workshop".into(),
        "Brave".into(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        theme,
    );

    // An invented waveform: a level about once per column, from a fixed
    // pseudo-random sequence, in 6 dB steps from full scale down to -60 dBFS.
    let mut seed: u32 = 0x2545_f491;
    for s in (0..=4_368).step_by(70) {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let peak = Level::FULL_SCALE.peak() >> ((seed >> 16) % 10);
        screen.update(Event::Level {
            track: TrackId::new(0),
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
        screen.update(Event::Text(heard(start, end, text)));
    }
    screen.update(Event::Transcribing(true));
    screen.update(Event::Recorded(14_200_000));

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

fn draw(screen: &mut Recording, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| screen.draw(frame)).unwrap();
    terminal
}

#[test]
fn main() {
    let terminal = draw(&mut main_screen(), 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// `MainWide`: from 100 columns, the marks-and-notes panel beside the main
/// panel, which keeps its layout.
#[test]
fn main_wide() {
    let mut screen = main_screen();
    // Earlier speech, which the first marks show.
    for (start, end, text) in [
        (
            660,
            690,
            "Plane with the grain, never across it, or the surface tears out.",
        ),
        (
            2_470,
            2_500,
            "Mark the face side first so every cut starts from one edge.",
        ),
    ] {
        screen.update(Event::Text(heard(start, end, text)));
    }
    let terminal = draw(&mut screen, 100, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// The breakpoint: one column below 100 is the narrow layout, stretched.
#[test]
fn one_column_narrower_has_no_panel() {
    let terminal = draw(&mut main_screen(), 99, 20);
    let text = format!("{}", terminal.backend());
    assert!(!text.contains("marks & notes"), "{text}");
    assert!(text.contains("──── Brave · 14 MB ─╯\""), "{text}");
}

/// A selected entry is drawn on the highlight.
#[test]
fn main_wide_with_a_selection() {
    let mut screen = main_screen();
    press(&mut screen, KeyCode::Char('k'));
    press(&mut screen, KeyCode::Char('k'));
    let terminal = draw(&mut screen, 100, 20);
    let buffer = terminal.backend().buffer();
    let lit: Vec<u16> = (0..20)
        .filter(|&y| buffer[(70, y)].modifier.contains(Modifier::REVERSED))
        .collect();
    // The fifth of six entries: its time and its text.
    assert_eq!(lit, [13, 14]);
}

#[test]
fn main_while_typing_a_note() {
    let mut screen = main_screen();
    press(&mut screen, KeyCode::Char('n'));
    for c in "check the glue".chars() {
        press(&mut screen, KeyCode::Char(c));
    }
    let mut terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
    // The cursor sits after the typed text in the bottom border.
    let cursor = terminal.get_cursor_position().unwrap();
    assert_eq!((cursor.x, cursor.y), (5 + 14, 19));
}

#[test]
fn main_confirming_stop() {
    let mut screen = main_screen();
    press(&mut screen, KeyCode::Char('s'));
    assert!(screen.is_confirming_stop());
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn too_small() {
    let terminal = draw(&mut main_screen(), 59, 20);
    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn too_small_confirming_stop() {
    let mut screen = main_screen();
    press(&mut screen, KeyCode::Char('s'));
    let terminal = draw(&mut screen, 30, 8);
    insta::assert_snapshot!(terminal.backend());
}

/// With `NO_COLOR`, no cell has a colour or any emphasis: not the frame,
/// the band, the transcript, nor the footer while a note is typed or the
/// stop question is open.
#[test]
fn no_color_draws_no_colour() {
    let mut screen = main_screen_in(Theme::no_color());
    let check = |screen: &mut Recording| {
        let terminal = draw(screen, 62, 20);
        let buffer = terminal.backend().buffer();
        for cell in &buffer.content {
            assert_eq!(cell.fg, Color::Reset, "{cell:?}");
            assert_eq!(cell.bg, Color::Reset, "{cell:?}");
            assert_eq!(cell.modifier, Modifier::empty(), "{cell:?}");
        }
    };
    check(&mut screen);
    press(&mut screen, KeyCode::Char('n'));
    press(&mut screen, KeyCode::Char('x'));
    check(&mut screen);
    press(&mut screen, KeyCode::Char('s'));
    check(&mut screen);
}

/// The wax panel (the theme's `lighter_background`) fills the frame's
/// inside and stops at the border.
#[test]
fn the_panel_is_behind_the_content_not_the_border() {
    let theme =
        Theme::from_colors_toml("lighter_background = \"#10121A\"\nforeground = \"#EDE3D6\"\n")
            .unwrap();
    let panel = Color::Rgb(0x10, 0x12, 0x1A);
    let terminal = draw(&mut main_screen_in(theme), 62, 20);
    let buffer = terminal.backend().buffer();
    for y in 0..20 {
        for x in 0..62 {
            let on_border = y == 0 || y == 19 || x == 0 || x == 61;
            let expected = if on_border { Color::Reset } else { panel };
            assert_eq!(buffer[(x, y)].bg, expected, "({x}, {y})");
        }
    }
}
