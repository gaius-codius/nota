//! Processing snapshots use invented session text.
#![cfg(test)]

use nota_tui::{
    Processing, ProcessingAction, ProcessingFailure, ProcessingJob, ProcessingState,
    ProcessingWait, Theme,
};
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

/// A long transcript opened at its first line.
fn transcript() -> Processing {
    let mut screen = screen();
    // Enough lines to need scrolling, with a known first page.
    screen.set_transcript((0..40).map(|n| format!("Line {n}")).collect());
    press(&mut screen, KeyCode::Tab);
    draw(&mut screen, 62, 20);
    screen
}

/// A long transcript opened at its second page.
fn scrolled_transcript() -> Processing {
    let mut screen = transcript();
    // Drawing after the key fixes the position before an update.
    press(&mut screen, KeyCode::PageDown);
    draw(&mut screen, 62, 20);
    screen
}

/// The Summary view keeps the existing layout while a step runs.
#[test]
fn processing_running() {
    // Draw the same invented session as the Summary mockup.
    insta::assert_snapshot!(draw(&mut screen(), 62, 20));
}

/// Tab opens the heard transcript in the existing layout.
#[test]
fn processing_transcript() {
    let mut screen = screen();
    // Open the heard words before comparing the Transcript mockup.
    press(&mut screen, KeyCode::Tab);
    insta::assert_snapshot!(draw(&mut screen, 62, 20));
}

/// A finished step uses a tick and plain words.
#[test]
fn done_is_visible_without_colour() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job("Transcript", ProcessingState::Done)]);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("✓  Transcript · parakeet · done"));
}

/// A waiting step says what it needs.
#[test]
fn waiting_is_visible_without_colour() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Waiting {
            reason: ProcessingWait::Engine,
            progress: 0,
        },
    )]);
    let output = draw(&mut screen, 100, 20);
    assert!(
        output.contains("○  Transcript · parakeet · waiting: waiting for a working speech engine")
    );
}

/// A failed step keeps its explanation.
#[test]
fn failure_is_visible_without_colour() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Failed {
            reason: ProcessingFailure::new("disk full".into()),
            progress: 0,
        },
    )]);
    let output = draw(&mut screen, 80, 20);
    assert!(output.contains("!  Transcript · parakeet · failed: disk full"));
}

/// A waiting step with no work done has an empty band.
#[test]
fn waiting_has_no_progress_before_work_starts() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Waiting {
            reason: ProcessingWait::Queued,
            progress: 0,
        },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert_eq!(output.matches('▰').count(), 0);
}

/// A half-finished step fills half the band.
#[test]
fn running_progress_fills_half_the_band() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Running { progress: 50 },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert_eq!(output.matches('▰').count(), 29);
    assert_eq!(output.matches('▱').count(), 29);
}

/// Progress above 100 is drawn as 100 percent.
#[test]
fn running_progress_is_limited_to_a_full_band() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Running { progress: 255 },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert_eq!(output.matches('▰').count(), 58);
    assert!(output.contains("100%"));
}

/// A finished step fills the whole band.
#[test]
fn finished_work_fills_the_band() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job("Transcript", ProcessingState::Done)]);
    let output = draw(&mut screen, 62, 20);
    assert_eq!(output.matches('▰').count(), 58);
}

/// A paused step keeps the progress already made.
#[test]
fn waiting_keeps_progress() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Waiting {
            reason: ProcessingWait::Engine,
            progress: 40,
        },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert_eq!(output.matches('▰').count(), 23);
}

/// Finished work has a ready heading and top border.
#[test]
fn finished_heading_matches_status() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job("Transcript", ProcessingState::Done)]);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("✓ ready"));
    assert!(output.contains("Transcript ready"));
}

/// Failed work asks for the user in the heading and border.
#[test]
fn failed_heading_matches_status() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Failed {
            reason: ProcessingFailure::new("missing audio".into()),
            progress: 40,
        },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("! needs you"));
    assert!(output.contains("Processing needs you"));
}

/// A failed step keeps the progress already made.
#[test]
fn failed_work_keeps_progress() {
    let mut screen = Processing::new("Test".into(), "engine".into(), Theme::no_color());
    // Give the page one step so its result can be read without colour.
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Failed {
            reason: ProcessingFailure::new("missing audio".into()),
            progress: 40,
        },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert_eq!(output.matches('▰').count(), 23);
}

/// Page Down moves to the next page of heard words.
#[test]
fn transcript_pages_down() {
    let mut screen = transcript();
    // The first page establishes the position before scrolling.
    assert!(draw(&mut screen, 62, 20).contains("Line 0 "));
    press(&mut screen, KeyCode::PageDown);
    let output = draw(&mut screen, 62, 20);
    assert!(!output.contains("Line 0 "));
    assert!(output.contains("Line 13 "));
}

/// Updating the steps keeps the reader at the same words.
#[test]
fn job_updates_keep_transcript_position() {
    let mut screen = scrolled_transcript();
    // Finish the step while the reader is on the second page.
    screen.set_jobs(vec![job("Transcript", ProcessingState::Done)]);
    assert!(draw(&mut screen, 62, 20).contains("Line 13 "));
}

/// Returning from Summary keeps the reader at the same words.
#[test]
fn tab_changes_keep_transcript_position() {
    let mut screen = scrolled_transcript();
    // Leave Transcript and return without changing its position.
    press(&mut screen, KeyCode::Tab);
    press(&mut screen, KeyCode::Tab);
    assert!(draw(&mut screen, 62, 20).contains("Line 13 "));
}

/// Page Up returns to the previous page of heard words.
#[test]
fn transcript_pages_up() {
    let mut screen = scrolled_transcript();
    // Move back from the second page to the first.
    press(&mut screen, KeyCode::PageUp);
    assert!(draw(&mut screen, 62, 20).contains("Line 0 "));
}

/// Replacing the words with a short transcript brings it into view.
#[test]
fn shorter_transcript_keeps_words_in_view() {
    let mut screen = scrolled_transcript();
    // The old scroll position must not hide a shorter replacement.
    screen.set_transcript(vec!["short replacement".into()]);
    assert!(draw(&mut screen, 62, 20).contains("short replacement"));
}

/// R asks to start another recording.
#[test]
fn record_key_requests_recording() {
    let mut screen = screen();
    // Navigation returns a request for the caller to handle.
    assert_eq!(
        press(&mut screen, KeyCode::Char('r')),
        Some(ProcessingAction::Record)
    );
}

/// Escape asks to return to Home.
#[test]
fn escape_requests_home() {
    let mut screen = screen();
    // Navigation returns a request for the caller to handle.
    assert_eq!(
        press(&mut screen, KeyCode::Esc),
        Some(ProcessingAction::Home)
    );
}

/// Q asks to close nota.
#[test]
fn quit_key_requests_quit() {
    let mut screen = screen();
    // Navigation returns a request for the caller to handle.
    assert_eq!(
        press(&mut screen, KeyCode::Char('q')),
        Some(ProcessingAction::Quit)
    );
}

/// Ctrl+C asks to close nota.
#[test]
fn control_c_requests_quit() {
    let mut screen = screen();
    // Ctrl+C still works when ordinary modified keys do not.
    assert_eq!(
        screen.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(ProcessingAction::Quit)
    );
}

/// Alt+R does not ask for a recording.
#[test]
fn alt_record_is_ignored() {
    let mut screen = screen();
    // A modified recording key is a different key.
    assert_eq!(
        screen.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        None
    );
}

/// A key repeat does not change the view.
#[test]
fn repeat_does_not_change_view() {
    let mut screen = screen();
    // Only the first press of Tab should change the view.
    let mut key = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    key.kind = KeyEventKind::Repeat;
    assert_eq!(screen.key(key), None);
    assert!(draw(&mut screen, 62, 20).contains("Finishing up"));
}

/// A key release does not change the view.
#[test]
fn release_does_not_change_view() {
    let mut screen = screen();
    // Only the first press of Tab should change the view.
    let mut key = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    key.kind = KeyEventKind::Release;
    assert_eq!(screen.key(key), None);
    assert!(draw(&mut screen, 62, 20).contains("Finishing up"));
}

/// Navigation leaves the processing steps unchanged.
#[test]
fn navigation_keeps_jobs() {
    let mut screen = screen();
    // Returning a Home request must not finish or drop the running step.
    press(&mut screen, KeyCode::Esc);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("Finishing up"));
    assert!(output.contains("40%"));
}

/// A terminal below the minimum size shows the size hint.
#[test]
fn narrow_terminal_shows_size_hint() {
    // The full page cannot fit at this size.
    assert!(!draw(&mut screen(), 59, 20).contains("Finishing up"));
}

/// A terminal below the minimum size shows the size hint.
#[test]
fn short_terminal_shows_size_hint() {
    // The full page cannot fit at this size.
    assert!(!draw(&mut screen(), 62, 19).contains("Finishing up"));
}

/// Control and formatting characters cannot reorder the heard words.
#[test]
fn transcript_drops_control_characters() {
    let mut screen = screen();
    // Open text that would reorder or escape a row if drawn unchanged.
    screen.set_transcript(vec!["a\u{202e}b\u{1b}c".into()]);
    press(&mut screen, KeyCode::Tab);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("abc"));
    assert!(!output.contains('\u{202e}'));
}

/// A refresh error appears above the heard words without hiding them.
#[test]
fn refresh_error_keeps_transcript_readable() {
    let mut screen = screen();
    // A failed refresh leaves the last transcript available.
    screen.set_notice(Some("Cannot refresh session: database busy".into()));
    press(&mut screen, KeyCode::Tab);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("! Cannot refresh session: database busy"));
    assert!(output.contains("Use a sharp plane"));
}

/// A successful refresh clears the problem from the page.
#[test]
fn recovered_refresh_clears_notice() {
    let mut screen = screen();
    // Show the error first, then clear it as a successful refresh would.
    screen.set_notice(Some("Cannot refresh session: database busy".into()));
    draw(&mut screen, 62, 20);
    screen.set_notice(None);
    assert!(!draw(&mut screen, 62, 20).contains("database busy"));
}

/// The supplied session time moves the Summary spinner.
#[test]
fn spinner_uses_session_time() {
    let mut screen = screen();
    // One quarter second advances the spinner by one phase.
    screen.tick(nota_core::SessionTime::from_nanos(250_000_000));
    assert!(draw(&mut screen, 62, 20).contains("Summary ◓"));
}

/// A failure keeps the supplied explanation before it is drawn.
#[test]
fn failure_keeps_original_explanation() {
    // Store the original text so preparing a row cannot change the reason.
    let reason = ProcessingFailure::new("audio\u{202e} missing".into());
    assert_eq!(reason.as_str(), "audio\u{202e} missing");
}

/// A failure explanation cannot reorder or break a row.
#[test]
fn failure_drops_control_characters_when_drawn() {
    let mut screen = screen();
    // The reason comes from outside the screen and may contain formatting.
    screen.set_jobs(vec![job(
        "Transcript",
        ProcessingState::Failed {
            reason: ProcessingFailure::new("a\u{202e}b\u{1b}c".into()),
            progress: 0,
        },
    )]);
    let output = draw(&mut screen, 62, 20);
    assert!(output.contains("failed: abc"));
    assert!(!output.contains('\u{202e}'));
}

/// An audio check explanation cannot reorder or break a row.
#[test]
fn audio_check_drops_control_characters_when_drawn() {
    let mut screen = screen();
    // The check's error text is prepared just like a failure explanation.
    screen.set_jobs(vec![job(
        "Saved",
        ProcessingState::Waiting {
            reason: ProcessingWait::AudioUnchecked("a\u{202e}b\u{1b}c".into()),
            progress: 0,
        },
    )]);
    let output = draw(&mut screen, 100, 20);
    assert!(output.contains("saved audio can't be checked: abc"));
    assert!(!output.contains('\u{202e}'));
}
