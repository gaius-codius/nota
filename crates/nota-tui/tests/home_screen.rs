//! Golden tests for the Home screen: the UI spec's `Home` mockup, drawn
//! through `ratatui`'s `TestBackend` and compared with insta. The sessions
//! are invented.
//!
//! The design is a draft. When the screen changes on purpose, update the
//! mockup and accept the new snapshot (`cargo insta review`) in the same
//! change.

#![cfg(test)]

use std::time::Duration;

use nota_tui::{Home, Session, Status, Theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const fn minutes(m: u64) -> Duration {
    Duration::from_secs(m * 60)
}

fn session(id: u64, title: &str, status: Status, length: u64, date: &str) -> Session {
    Session {
        id,
        title: title.to_owned(),
        status,
        duration: Some(minutes(length)),
        date: Some(date.to_owned()),
        detail: Vec::new(),
    }
}

/// The mockup's library: an older session stopped by a full disk, pinned
/// to the top, and three others, newest first.
fn mockup_home() -> Home {
    let mut stopped = session(
        3,
        "Joinery · mortise and tenon",
        Status::NeedsYou,
        58,
        "2 Oct",
    );
    stopped.detail = vec!["stopped early: disk full · free space to finish".to_owned()];
    let sessions = vec![
        session(2, "Finishing · oils and waxes", Status::Ready, 65, "9 Sep"),
        stopped,
        session(
            6,
            "Woodwork workshop · hand planes",
            Status::Ready,
            162,
            "today",
        ),
        session(5, "Turning · bowl gouges", Status::Processing, 80, "today"),
    ];
    Home::new(sessions, "parakeet", Theme::default())
}

fn draw(home: &mut Home, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| home.draw(frame)).unwrap();
    terminal
}

fn press(home: &mut Home, code: KeyCode) {
    let _ = home.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn home() {
    let terminal = draw(&mut mockup_home(), 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// With nothing needing the user, the top border says it's ready; moving
/// down selects the next session, and only the selected one's detail
/// shows.
#[test]
fn home_ready() {
    let mut recovered = session(4, "Sharpening · chisels", Status::Ready, 41, "3 Oct");
    recovered.detail = vec!["recovered after a crash · nothing lost".to_owned()];
    let mut home = Home::new(
        vec![
            session(
                6,
                "Woodwork workshop · hand planes",
                Status::Ready,
                162,
                "today",
            ),
            recovered,
        ],
        "parakeet",
        Theme::default(),
    );
    press(&mut home, KeyCode::Down);
    let terminal = draw(&mut home, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

#[test]
fn home_empty() {
    let mut home = Home::new(Vec::new(), "parakeet", Theme::default());
    let terminal = draw(&mut home, 62, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// A session salvage couldn't finish, at 80 columns: its detail names each
/// file it held with the cause, every item from a row of its own, and one
/// too long for a row wraps onto indented rows.
#[test]
fn home_held_files() {
    let mut held = session(
        7,
        "Joinery · dovetails by hand",
        Status::NeedsYou,
        47,
        "4 Oct",
    );
    held.detail = vec![
        "salvage failed: publishing a segment failed: a disk error".to_owned(),
        "journal-000000: couldn't delete it (permission denied); \
         all its audio is published"
            .to_owned(),
        "seg-t1-000000000000.flac.tmp: couldn't remove the temporary file \
         (permission denied); its audio is still to publish"
            .to_owned(),
    ];
    let sessions = vec![
        held,
        session(
            6,
            "Woodwork workshop · hand planes",
            Status::Ready,
            162,
            "today",
        ),
    ];
    let mut home = Home::new(sessions, "parakeet", Theme::default());
    let terminal = draw(&mut home, 80, 22);
    insta::assert_snapshot!(terminal.backend());
}
