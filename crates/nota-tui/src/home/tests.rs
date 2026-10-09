use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyEventState, KeyModifiers};
use ratatui::style::{Color, Modifier};

use super::*;

fn session(id: u64, status: Status) -> Session {
    Session {
        id,
        title: format!("session {id}"),
        status,
        duration: Some(Duration::from_secs(id * 60)),
        date: Some("today".into()),
        detail: Some(format!("detail {id}")),
    }
}

fn home_of(sessions: Vec<Session>, theme: Theme) -> Home {
    Home::new(sessions, "parakeet", theme)
}

fn press(home: &mut Home, code: KeyCode) -> Option<Action> {
    home.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn draw(home: &mut Home, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(62, height)).unwrap();
    terminal.draw(|frame| home.draw(frame)).unwrap();
    terminal.backend().buffer().clone()
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn ids(home: &Home) -> Vec<u64> {
    home.sessions().iter().map(|s| s.id).collect()
}

#[test]
fn sessions_that_need_you_are_pinned_then_newest_first() {
    let home = home_of(
        vec![
            session(1, Status::NeedsYou),
            session(4, Status::Ready),
            session(2, Status::Processing),
            session(5, Status::Ready),
            session(3, Status::NeedsYou),
        ],
        Theme::default(),
    );
    assert_eq!(ids(&home), [3, 1, 5, 4, 2]);
    assert_eq!(home.selected(), Some(3));
}

#[test]
fn r_records_and_q_esc_and_ctrl_c_quit() {
    let mut home = home_of(vec![session(1, Status::Ready)], Theme::default());
    assert_eq!(press(&mut home, KeyCode::Char('R')), Some(Action::Record));
    assert_eq!(press(&mut home, KeyCode::Char('q')), Some(Action::Quit));
    assert_eq!(press(&mut home, KeyCode::Esc), Some(Action::Quit));
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(home.handle_key(ctrl_c), Some(Action::Quit));
    // `r` waits for Setup, and `/` for search: neither does anything yet.
    assert_eq!(press(&mut home, KeyCode::Char('r')), None);
    assert_eq!(press(&mut home, KeyCode::Char('/')), None);
    // Held with Ctrl or Alt, they're other keys.
    let alt_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::ALT | KeyModifiers::SHIFT);
    assert_eq!(home.handle_key(alt_r), None);
    let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert_eq!(home.handle_key(ctrl_q), None);
    let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
    assert_eq!(home.handle_key(shift_r), Some(Action::Record));
    // A key's release isn't a press.
    let mut release = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    release.state = KeyEventState::NONE;
    assert_eq!(home.handle_key(release), None);
}

#[test]
fn the_selection_moves_within_the_list() {
    let mut home = home_of(
        vec![session(1, Status::Ready), session(2, Status::Ready)],
        Theme::default(),
    );
    assert_eq!(home.selected(), Some(2));
    press(&mut home, KeyCode::Up);
    assert_eq!(home.selected(), Some(2));
    press(&mut home, KeyCode::Down);
    assert_eq!(home.selected(), Some(1));
    press(&mut home, KeyCode::Char('j'));
    assert_eq!(home.selected(), Some(1));
    press(&mut home, KeyCode::Char('k'));
    assert_eq!(home.selected(), Some(2));
}

#[test]
fn enter_asks_to_open_the_selected_session() {
    let mut home = home_of(
        vec![session(1, Status::Ready), session(2, Status::NeedsYou)],
        Theme::default(),
    );
    assert_eq!(press(&mut home, KeyCode::Enter), Some(Action::Open(2)));
    press(&mut home, KeyCode::Down);
    assert_eq!(press(&mut home, KeyCode::Enter), Some(Action::Open(1)));
    // Modified Enter and key releases must not open a session.
    let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
    assert_eq!(home.handle_key(alt_enter), None);
    let mut release = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(home.handle_key(release), None);
    // Nothing to open in an empty library.
    let mut empty = home_of(Vec::new(), Theme::default());
    assert_eq!(press(&mut empty, KeyCode::Enter), None);
    assert_eq!(empty.selected(), None);
    press(&mut empty, KeyCode::Down);
    assert_eq!(empty.selected(), None);
}

#[test]
fn only_the_selected_session_shows_its_detail() {
    let mut home = home_of(
        vec![session(1, Status::Ready), session(2, Status::Ready)],
        Theme::default(),
    );
    let buf = draw(&mut home, 20);
    assert!(row(&buf, 12).contains("▸ ✓ session 2"), "{}", row(&buf, 12));
    assert!(row(&buf, 13).contains("detail 2"), "{}", row(&buf, 13));
    assert!(row(&buf, 14).contains("  ✓ session 1"), "{}", row(&buf, 14));
    let all: String = (0..20).map(|y| row(&buf, y)).collect();
    assert!(!all.contains("detail 1"));
}

#[test]
fn the_top_border_counts_the_sessions_that_need_you() {
    let mut home = home_of(
        vec![
            session(1, Status::NeedsYou),
            session(2, Status::NeedsYou),
            session(3, Status::Ready),
        ],
        Theme::default(),
    );
    assert!(row(&draw(&mut home, 20), 0).ends_with(" ! 2 needs you ─╮"));
    home.set_busy(Some("finishing the recording".into()));
    let buf = draw(&mut home, 20);
    let top = row(&buf, 0);
    assert!(top.ends_with(" ◐ finishing the recording ─╮"), "{top}");
    // No keys are read while it's busy, so none are offered.
    let bottom = row(&buf, 19);
    assert!(bottom.starts_with("╰─  ───"), "{bottom}");
    assert!(!bottom.contains('R'), "{bottom}");
}

#[test]
fn the_selection_stays_in_view() {
    let sessions = (1..=20).map(|id| session(id, Status::Ready)).collect();
    let mut home = home_of(sessions, Theme::default());
    // 20 rows leave 7 for the list.
    for _ in 0..10 {
        press(&mut home, KeyCode::Down);
    }
    assert_eq!(home.selected(), Some(10));
    let buf = draw(&mut home, 20);
    // The selected row and its detail are the last two shown.
    assert!(
        row(&buf, 17).contains("▸ ✓ session 10"),
        "{}",
        row(&buf, 17)
    );
    assert!(row(&buf, 18).contains("detail 10"), "{}", row(&buf, 18));
    // Back to the top: the first session shows again.
    for _ in 0..10 {
        press(&mut home, KeyCode::Up);
    }
    let buf = draw(&mut home, 20);
    assert!(
        row(&buf, 12).contains("▸ ✓ session 20"),
        "{}",
        row(&buf, 12)
    );
}

#[test]
fn a_long_title_is_cut_before_the_duration_and_date() {
    let mut long = session(1, Status::Ready);
    long.title = "a very long title ".repeat(6);
    let mut home = home_of(vec![long], Theme::default());
    let line = row(&draw(&mut home, 20), 12);
    assert!(line.ends_with("a …     1m  today  │"), "{line}");
}

#[test]
fn durations_read_in_hours_and_minutes() {
    let d = |s| duration(Duration::from_secs(s));
    assert_eq!(d(0), "<1m");
    assert_eq!(d(59), "<1m");
    assert_eq!(d(60), "1m");
    assert_eq!(d(3_599), "59m");
    assert_eq!(d(3_600), "1h 00m");
    assert_eq!(d(9_720), "2h 42m");
}

#[test]
fn control_characters_in_the_text_are_dropped() {
    let mut odd = session(1, Status::Ready);
    odd.title = "tab\there\u{202e}rtl".into();
    let home = home_of(vec![odd], Theme::default());
    assert_eq!(home.sessions()[0].title, "tabherertl");
}

#[test]
fn colour_roles() {
    let theme = Theme::from_colors_toml(
        "accent = \"#E4744A\"\nyellow = \"#E8B25C\"\ngreen = \"#A8B36A\"\n\
         lighter_background = \"#10121A\"\nselection = \"#272539\"\n",
    )
    .unwrap();
    let mut home = home_of(
        vec![session(1, Status::Ready), session(2, Status::NeedsYou)],
        theme,
    );
    let buf = draw(&mut home, 20);
    // The selected `!` row: ▸ in the accent, `!` in gold, on the highlight.
    assert_eq!(buf[(2, 12)].symbol(), "▸");
    assert_eq!(buf[(2, 12)].fg, Color::Rgb(0xE4, 0x74, 0x4A));
    assert_eq!(buf[(4, 12)].symbol(), "!");
    assert_eq!(buf[(4, 12)].fg, Color::Rgb(0xE8, 0xB2, 0x5C));
    assert_eq!(buf[(30, 12)].bg, Color::Rgb(0x27, 0x25, 0x39));
    // A ready row: ✓ in green, on the panel.
    assert_eq!(buf[(4, 14)].symbol(), "✓");
    assert_eq!(buf[(4, 14)].fg, Color::Rgb(0xA8, 0xB3, 0x6A));
    assert_eq!(buf[(30, 14)].bg, Color::Rgb(0x10, 0x12, 0x1A));
    // The panel stops at the border.
    for y in 0..20 {
        for x in 0..62 {
            let on_border = y == 0 || y == 19 || x == 0 || x == 61;
            if on_border {
                assert_eq!(buf[(x, y)].bg, Color::Reset, "({x}, {y})");
            }
        }
    }
}

#[test]
fn no_color_draws_no_colour() {
    let mut home = home_of(
        vec![session(1, Status::Ready), session(2, Status::NeedsYou)],
        Theme::no_color(),
    );
    let check = |home: &mut Home| {
        for cell in &draw(home, 20).content {
            assert_eq!(cell.fg, Color::Reset, "{cell:?}");
            assert_eq!(cell.bg, Color::Reset, "{cell:?}");
            assert_eq!(cell.modifier, Modifier::empty(), "{cell:?}");
        }
    };
    check(&mut home);
    home.set_busy(Some("finishing".into()));
    check(&mut home);
    assert_eq!(press(&mut home, KeyCode::Enter), Some(Action::Open(2)));
    check(&mut home);
}

#[test]
fn a_small_terminal_asks_for_more_room() {
    let mut home = home_of(vec![session(1, Status::Ready)], Theme::default());
    let mut terminal = Terminal::new(TestBackend::new(59, 20)).unwrap();
    terminal.draw(|frame| home.draw(frame)).unwrap();
    let text = format!("{}", terminal.backend());
    assert!(text.contains("make the window larger"), "{text}");
}

#[test]
fn a_notice_shows_until_the_next_key() {
    let mut home = home_of(vec![session(1, Status::Ready)], Theme::default());
    home.set_notice(Some("couldn't record: nothing to record".into()));
    let buf = draw(&mut home, 20);
    assert!(
        row(&buf, 10).starts_with("│ ! couldn't record: nothing to record "),
        "{}",
        row(&buf, 10)
    );
    press(&mut home, KeyCode::Down);
    let buf = draw(&mut home, 20);
    assert_eq!(row(&buf, 10).trim_matches(['│', ' ']), "");
}

#[test]
fn a_new_list_keeps_the_selection_on_its_session() {
    let mut home = home_of(
        vec![
            session(1, Status::NeedsYou),
            session(2, Status::Ready),
            session(3, Status::Ready),
        ],
        Theme::default(),
    );
    press(&mut home, KeyCode::Down);
    press(&mut home, KeyCode::Down);
    assert_eq!(home.selected(), Some(2));
    assert_eq!(press(&mut home, KeyCode::Enter), Some(Action::Open(2)));
    // Session 1 no longer needs you, so it moves below 2; session 2's
    // detail changes, and the selected row shows the new one.
    let mut two = session(2, Status::Ready);
    two.detail = Some("recovered".into());
    home.set_sessions(vec![
        session(1, Status::Ready),
        two,
        session(3, Status::Ready),
    ]);
    assert_eq!(ids(&home), [3, 2, 1]);
    assert_eq!(home.selected(), Some(2));
    assert_eq!(press(&mut home, KeyCode::Enter), Some(Action::Open(2)));
    let buf = draw(&mut home, 20);
    let list: String = (0..20).map(|y| row(&buf, y)).collect();
    assert!(list.contains("recovered"), "{list}");
    // Text is cleaned as it is by `new`.
    home.set_sessions(vec![Session {
        title: "a\u{202e}b".into(),
        ..session(2, Status::Ready)
    }]);
    assert_eq!(home.sessions()[0].title, "ab");
}

#[test]
fn a_new_list_without_the_selected_session_keeps_its_position() {
    let mut home = home_of(
        vec![
            session(1, Status::Ready),
            session(2, Status::Ready),
            session(3, Status::Ready),
        ],
        Theme::default(),
    );
    press(&mut home, KeyCode::Down);
    assert_eq!(press(&mut home, KeyCode::Enter), Some(Action::Open(2)));
    assert_eq!(home.selected(), Some(2));
    // Session 2 has gone: the selection stays at the second row.
    home.set_sessions(vec![
        session(1, Status::Ready),
        session(3, Status::Ready),
        session(4, Status::Ready),
    ]);
    assert_eq!(home.selected(), Some(3));
    assert_eq!(press(&mut home, KeyCode::Enter), Some(Action::Open(3)));
    // Past the end of a shorter list: the last row.
    press(&mut home, KeyCode::Down);
    press(&mut home, KeyCode::Down);
    home.set_sessions(vec![session(4, Status::Ready)]);
    assert_eq!(home.selected(), Some(4));
    home.set_sessions(Vec::new());
    assert_eq!(home.selected(), None);
    home.set_sessions(vec![session(5, Status::Ready)]);
    assert_eq!(home.selected(), Some(5));
}

#[test]
fn a_notice_not_yet_drawn_outlasts_a_key() {
    let mut home = home_of(vec![session(1, Status::Ready)], Theme::default());
    home.set_notice(Some("the sessions couldn't be listed".into()));
    // A key before the first draw leaves the notice.
    press(&mut home, KeyCode::Down);
    assert_eq!(home.notice(), Some("the sessions couldn't be listed"));
    // Too small to show it: a key leaves it too.
    draw(&mut home, 5);
    press(&mut home, KeyCode::Down);
    assert_eq!(home.notice(), Some("the sessions couldn't be listed"));
    let buf = draw(&mut home, 20);
    assert!(
        row(&buf, 10).contains("couldn't be listed"),
        "{}",
        row(&buf, 10)
    );
    press(&mut home, KeyCode::Down);
    assert_eq!(home.notice(), None);
}
