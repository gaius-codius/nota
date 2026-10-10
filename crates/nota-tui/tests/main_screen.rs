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
use nota_core::recorder::{
    Cause, DeviceChange, Disk, EngineState, Event, Level, Track, TrackRole, Warning, WarningState,
};
use nota_core::{
    Clock, FakeClock, SampleIndex, SampleRange, SampleRate, SessionTime, TrackId, TrackTimeline,
    Utterance,
};
use nota_tui::{Recording, Theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};

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
    main_screen_without_levels(theme, 0..0)
}

/// [`main_screen_in`] with no level for the seconds in `missing`, as when a
/// stretch of the recording was lost.
fn main_screen_without_levels(theme: Theme, missing: std::ops::Range<u64>) -> Recording {
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
        // Drawn even for a missing level, so the others keep their values.
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let peak = Level::FULL_SCALE.peak() >> ((seed >> 16) % 10);
        if missing.contains(&s) {
            continue;
        }
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
    // And ▸ beside it, which shows without colour too.
    let mut plain = main_screen_in(Theme::no_color());
    press(&mut plain, KeyCode::Char('k'));
    let terminal = draw(&mut plain, 100, 20);
    let pointers: Vec<u16> = (0..20)
        .filter(|&y| terminal.backend().buffer()[(69, y)].symbol() == "▸")
        .collect();
    assert_eq!(pointers, [16]);
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

/// The mic and the system audio, as `nota record` names them, so the footer
/// reads `mic + system · 14 MB`.
fn two_tracks(screen: Recording) -> Recording {
    screen.with_tracks(vec![
        Track {
            id: MIC,
            role: TrackRole::Microphone,
            source: "mic".to_owned(),
        },
        Track {
            id: SYSTEM,
            role: TrackRole::System,
            source: "system".to_owned(),
        },
    ])
}

/// The fixture screen with its two tracks named.
fn two_track_screen() -> Recording {
    two_tracks(main_screen())
}

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

/// Tells `screen` that `cause` was raised on `track` at `at` seconds.
fn raise(screen: &mut Recording, cause: Cause, track: Option<TrackId>, at: u64) {
    screen.update(Event::Warning(Warning {
        cause,
        track,
        at: secs(at),
        state: WarningState::Raised,
    }));
}

/// Tells `screen` that `cause` cleared on `track` at `at` seconds.
fn clear(screen: &mut Recording, cause: Cause, track: Option<TrackId>, at: u64) {
    screen.update(Event::Warning(Warning {
        cause,
        track,
        at: secs(at),
        state: WarningState::Cleared,
    }));
}

/// Tells `screen` that `track`'s device changed, at `at` seconds.
fn device(screen: &mut Recording, track: TrackId, change: DeviceChange, at: u64) {
    screen.update(Event::Device {
        track,
        change,
        at: secs(at),
    });
}

/// Tells `screen` that the disk has `minutes` of recording left.
fn disk_left(screen: &mut Recording, minutes: u64) {
    screen.update(Event::Disk(Disk {
        free_bytes: 1_000_000_000,
        left: Some(Duration::from_secs(minutes * 60)),
    }));
}

/// A gap of 92 s before the second epoch of the mic's timeline: the length
/// of a sleep in the snapshot below.
fn gap_of_92_seconds() -> Event {
    let rate = SampleRate::new(1_000).unwrap();
    let mut timeline = TrackTimeline::new(MIC);
    timeline
        .open_epoch(secs(0), SampleIndex::ZERO, rate)
        .unwrap();
    timeline
        .open_epoch(secs(200), SampleIndex::new(108_000), rate)
        .unwrap();
    let gap = timeline.gaps().next().unwrap();
    Event::Gap { track: MIC, gap }
}

/// The top border's text, as drawn.
fn top_row(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect()
}

/// `MainWarning`: the mic lost in a stretch the band shows as `····`, a
/// fault on each track before and after it, and another condition behind
/// the one shown.
#[test]
fn main_warning() {
    let mut screen = two_tracks(main_screen_without_levels(Theme::default(), 1_250..1_600));
    // The first change falls in the stretch with no levels, where the gap
    // wins over the change.
    device(&mut screen, MIC, DeviceChange::Lost, 1_300);
    raise(&mut screen, Cause::Stalled, Some(SYSTEM), 2_775);
    clear(&mut screen, Cause::Stalled, Some(SYSTEM), 2_780);
    raise(&mut screen, Cause::DigitalZeros, Some(MIC), 3_750);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// The mic's device went away.
#[test]
fn main_warning_mic_lost() {
    let mut screen = two_track_screen();
    device(&mut screen, MIC, DeviceChange::Lost, 4_300);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// The mic's stream failed.
#[test]
fn main_warning_mic_not_recording() {
    let mut screen = two_track_screen();
    raise(
        &mut screen,
        Cause::StreamFailed("gone".into()),
        Some(MIC),
        4_300,
    );
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// The mic's stream stalled.
#[test]
fn main_warning_mic_not_responding() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::Stalled, Some(MIC), 4_300);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// Under 15 minutes of disk left is in the accent colour.
#[test]
fn main_warning_disk_12m_left() {
    let mut screen = two_track_screen();
    disk_left(&mut screen, 12);
    raise(&mut screen, Cause::DiskLow, None, 4_300);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// Exact zeros on the mic mean it's muted.
#[test]
fn main_warning_mic_muted() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::DigitalZeros, Some(MIC), 4_300);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// Exact zeros on the system audio, with the mic quiet too: nothing plays,
/// and the quiet is the one other condition.
#[test]
fn main_warning_system_nothing_playing() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::Quiet, Some(MIC), 4_300);
    raise(&mut screen, Cause::DigitalZeros, Some(SYSTEM), 4_300);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// Both tracks below their noise floor for 42 s.
#[test]
fn main_warning_quiet() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::Quiet, Some(MIC), 4_320);
    raise(&mut screen, Cause::Quiet, Some(SYSTEM), 4_326);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// Under an hour of disk left is in gold.
#[test]
fn main_warning_disk_40m_left() {
    let mut screen = two_track_screen();
    disk_left(&mut screen, 40);
    raise(&mut screen, Cause::DiskLow, None, 4_300);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// The transcriber went offline; the audio still records.
#[test]
fn main_warning_live_text_off() {
    let mut screen = two_track_screen();
    screen.update(Event::Engine(EngineState::Offline("crashed".into())));
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// Sleep couldn't be held off.
#[test]
fn main_warning_may_sleep() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::SleepNotHeld, None, 0);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// The library database can't be written.
#[test]
fn main_warning_library_offline() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::LibraryUnavailable, None, 4_300);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// Two conditions at once: the more severe shows, with the count of the
/// rest.
#[test]
fn main_warning_several_at_once() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::SleepNotHeld, None, 0);
    device(&mut screen, MIC, DeviceChange::Lost, 4_300);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// A track that follows the default moved to another device: the footer
/// names it.
#[test]
fn main_event_route_change() {
    let mut screen = two_track_screen();
    device(
        &mut screen,
        MIC,
        DeviceChange::Changed("Headphones".into()),
        4_366,
    );
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// A lost device came back.
#[test]
fn main_event_device_back() {
    let mut screen = two_track_screen();
    device(&mut screen, MIC, DeviceChange::Lost, 4_300);
    device(&mut screen, MIC, DeviceChange::Changed("mic".into()), 4_366);
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// The machine slept, and the gap after it says for how long.
#[test]
fn main_event_slept() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::Slept, None, 4_366);
    screen.update(gap_of_92_seconds());
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// `MainStopped`: a full disk stopped the recording at 1:12:48.
#[test]
fn main_stopped() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::DiskFull, None, 4_368);
    assert_eq!(screen.stopped_by_full_disk(), Some(secs(4_368)));
    let terminal = draw(&mut screen, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// The top border's cell style at the first cell showing `glyph`.
fn style_of(terminal: &Terminal<TestBackend>, glyph: &str) -> Style {
    let buffer = terminal.backend().buffer();
    let x = (0..buffer.area.width)
        .find(|&x| buffer[(x, 0)].symbol() == glyph)
        .unwrap_or_else(|| panic!("no {glyph} in {}", top_row(terminal)));
    buffer[(x, 0)].style()
}

/// A warning is drawn in its severity's colour: accent when audio is being
/// lost, gold for something worth a look, the plain text colour for a route
/// change and green for a device back.
#[test]
fn a_warning_is_drawn_in_its_severity_colour() {
    let theme = Theme::default();
    let fg = |style: Style| Style::new().fg(style.fg.unwrap());
    let drawn = |screen: &mut Recording, glyph: &str| {
        let terminal = draw(screen, 62, 20);
        fg(style_of(&terminal, glyph))
    };

    let mut lost = two_track_screen();
    device(&mut lost, MIC, DeviceChange::Lost, 4_300);
    assert_eq!(drawn(&mut lost, "⚠"), fg(theme.accent));

    let mut muted = two_track_screen();
    raise(&mut muted, Cause::DigitalZeros, Some(MIC), 4_300);
    assert_eq!(drawn(&mut muted, "⚠"), fg(theme.gold));

    let mut route = two_track_screen();
    device(
        &mut route,
        MIC,
        DeviceChange::Changed("Headphones".into()),
        4_366,
    );
    assert_eq!(drawn(&mut route, "↪"), fg(theme.text));

    let mut back = two_track_screen();
    device(&mut back, MIC, DeviceChange::Lost, 4_300);
    device(&mut back, MIC, DeviceChange::Changed("mic".into()), 4_366);
    assert_eq!(drawn(&mut back, "✓"), fg(theme.green));
}

/// The stopped screen's `■ stopped` is in the accent colour.
#[test]
fn the_stop_is_drawn_in_the_accent_colour() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::DiskFull, None, 4_368);
    let terminal = draw(&mut screen, 62, 20);
    let style = style_of(&terminal, "■");
    assert_eq!(style.fg, Theme::default().accent.fg);
}

/// The top border goes back to `● REC` and the clock once the warning's
/// cause clears.
#[test]
fn a_cleared_warning_leaves_the_top_border() {
    let mut screen = two_track_screen();
    raise(&mut screen, Cause::Stalled, Some(SYSTEM), 4_300);
    let drawn = top_row(&draw(&mut screen, 62, 20));
    assert!(
        drawn.contains("⚠ system not responding · 01:12:48"),
        "{drawn}"
    );
    // Drawn again after the clear, on the same screen.
    clear(&mut screen, Cause::Stalled, Some(SYSTEM), 4_310);
    let drawn = top_row(&draw(&mut screen, 62, 20));
    assert!(drawn.contains("● REC 01:12:48"), "{drawn}");
    assert!(!drawn.contains('⚠'), "{drawn}");
}
