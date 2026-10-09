//! Processing snapshots use invented session text.
#![cfg(test)]

use nota_tui::{Processing, ProcessingAction, ProcessingJob, ProcessingState, Theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

fn screen() -> Processing {
    let mut screen = Processing::new(
        "Woodwork workshop".into(),
        "parakeet".into(),
        Theme::default(),
    );
    screen.set_jobs(vec![
        job("Recording saved", ProcessingState::Done),
        job(
            "Final transcript",
            ProcessingState::Running { progress: 40 },
        ),
    ]);
    screen.set_transcript(vec!["Use a sharp plane on the edge of the board.".into()]);
    screen
}

fn job(name: &str, state: ProcessingState) -> ProcessingJob {
    ProcessingJob {
        name: name.into(),
        engine: "parakeet".into(),
        state,
    }
}

fn draw(screen: &mut Processing, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| screen.draw(frame)).unwrap();
    terminal.backend().to_string()
}

fn press(screen: &mut Processing, code: KeyCode) -> Option<ProcessingAction> {
    screen.key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn processing_running() {
    insta::assert_snapshot!(draw(&mut screen(), 62, 20));
}

#[test]
fn processing_transcript() {
    let mut screen = screen();
    press(&mut screen, KeyCode::Tab);
    insta::assert_snapshot!(draw(&mut screen, 62, 20));
}

#[test]
fn states_and_progress_are_visible_without_color() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    screen.set_jobs(vec![
        job("Saved", ProcessingState::Done),
        job(
            "Transcript",
            ProcessingState::Waiting {
                reason: "engine offline".into(),
                progress: 0,
            },
        ),
        job(
            "Conversion",
            ProcessingState::Failed {
                reason: "disk full".into(),
                progress: 0,
            },
        ),
    ]);
    let output = draw(&mut screen, 80, 20);
    assert!(output.contains("✓  Saved · parakeet · done"));
    assert!(output.contains("○  Transcript · parakeet · waiting: engine offline"));
    assert!(output.contains("!  Conversion · parakeet · failed: disk full"));
    assert_eq!(output.matches('▰').count(), 0);
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Running { progress: 50 },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert_eq!(output.matches('▰').count(), 29);
    assert_eq!(output.matches('▱').count(), 29);
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Running { progress: 255 },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert_eq!(output.matches('▰').count(), 58);
    assert!(output.contains("100%"));
    screen.set_jobs(vec![job("Transcript", ProcessingState::Done)]);
    assert_eq!(draw(&mut screen, 62, 20).matches('▰').count(), 58);
}

#[test]
fn transcript_scrolls_and_survives_job_updates_and_tab_changes() {
    let mut screen = screen();
    screen.set_transcript((0..40).map(|n| format!("Line {n}")).collect());
    press(&mut screen, KeyCode::Tab);
    assert!(draw(&mut screen, 62, 20).contains("Line 0 "));
    press(&mut screen, KeyCode::PageDown);
    let scrolled = draw(&mut screen, 62, 20);
    assert!(!scrolled.contains("Line 0 "));
    assert!(scrolled.contains("Line 13 "));
    screen.set_jobs(vec![job("Transcript", ProcessingState::Done)]);
    press(&mut screen, KeyCode::Tab);
    press(&mut screen, KeyCode::Tab);
    assert!(draw(&mut screen, 62, 20).contains("Line 13 "));
    press(&mut screen, KeyCode::PageUp);
    assert!(draw(&mut screen, 62, 20).contains("Line 0 "));
    screen.set_transcript(vec!["short replacement".into()]);
    assert!(draw(&mut screen, 62, 20).contains("short replacement"));
}

#[test]
fn navigation_does_not_mutate_jobs_and_only_accepts_presses() {
    let mut screen = screen();
    assert_eq!(
        press(&mut screen, KeyCode::Char('r')),
        Some(ProcessingAction::Record)
    );
    assert_eq!(
        press(&mut screen, KeyCode::Esc),
        Some(ProcessingAction::Home)
    );
    assert_eq!(
        press(&mut screen, KeyCode::Char('q')),
        Some(ProcessingAction::Quit)
    );
    assert_eq!(
        screen.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(ProcessingAction::Quit)
    );
    assert_eq!(
        screen.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        None
    );
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        let mut key = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        key.kind = kind;
        assert_eq!(screen.key(key), None);
    }
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("Finishing up"));
    assert!(output.contains("40%"));
}

#[test]
fn small_terminals_and_untrusted_text_draw_safely() {
    let mut screen = screen();
    assert!(!draw(&mut screen, 59, 20).contains("Finishing up"));
    assert!(!draw(&mut screen, 62, 19).contains("Finishing up"));
    screen.set_transcript(vec!["a\u{202e}b\u{1b}c".into()]);
    press(&mut screen, KeyCode::Tab);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("abc"));
    assert!(!output.contains('\u{202e}'));
}

#[test]
fn refresh_errors_keep_the_transcript_readable_and_clear_after_recovery() {
    let mut screen = screen();
    screen.set_notice(Some("Cannot refresh session: database busy".into()));
    press(&mut screen, KeyCode::Tab);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("! Cannot refresh session: database busy"));
    assert!(output.contains("Use a sharp plane"));
    screen.set_notice(None);
    let output = draw(&mut screen, 62, 20);
    assert!(!output.contains("database busy"));
    assert!(output.contains("Use a sharp plane"));
}

#[test]
fn paused_progress_is_kept_and_finished_or_failed_work_has_an_honest_heading() {
    let mut screen = screen();
    screen.set_jobs(vec![job(
        "Final transcript",
        ProcessingState::Waiting {
            reason: "engine offline".into(),
            progress: 40,
        },
    )]);
    assert_eq!(draw(&mut screen, 62, 20).matches('▰').count(), 23);
    screen.set_jobs(vec![job("Final transcript", ProcessingState::Done)]);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("✓ ready"));
    assert!(output.contains("Transcript ready"));
    screen.set_jobs(vec![job(
        "Final transcript",
        ProcessingState::Failed {
            reason: "missing audio".into(),
            progress: 40,
        },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("! needs you"));
    assert!(output.contains("Processing needs you"));
    assert_eq!(output.matches('▰').count(), 23);
    screen.tick(nota_core::SessionTime::from_nanos(250_000_000));
    assert!(draw(&mut screen, 62, 20).contains("Summary ◓"));
}
