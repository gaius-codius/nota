//! How a recording went: the screen, whose result feeds the summary, and
//! the summary's notes.

use std::fmt::Write as _;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use nota_core::recorder::Command;
pub(crate) use nota_core::recorder::Outcome;
use nota_core::{Clock, TrackId};
use nota_recorder::segment::PublishReport;
use nota_tui::{Ended, Event, InputThread, Recording, RunError, Theme};

use super::{MIC, RecordArgs, SYSTEM};
use crate::terminal::Screen;

/// How the screen went.
pub(super) struct Shown {
    /// Marks and notes made.
    pub(super) marks: usize,
    /// Why it closed, if not as asked: the terminal failed (as after a
    /// hangup). The recording stops all the same.
    pub(super) problem: Option<String>,
}

/// Shows the Recording screen on `screen` until it's closed, then restores
/// the terminal.
///
/// # Errors
///
/// Only if the input thread can't start.
pub(super) fn show(
    mut screen: Screen,
    args: &RecordArgs,
    listening: &str,
    clock: &Arc<dyn Clock>,
    ui: &Sender<Event>,
    ui_events: &Receiver<Event>,
) -> io::Result<Shown> {
    let theme = if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        Theme::no_color()
    } else {
        Theme::default()
    };
    let mut recording = Recording::new(
        args.setup.title.clone(),
        listening.to_owned(),
        Arc::clone(clock),
        theme,
    );
    let (commands, given) = mpsc::channel::<Command>();
    let input = InputThread::spawn(ui.clone(), Arc::clone(clock))?;
    let ran = nota_tui::run(screen.terminal(), &mut recording, ui_events, &commands);
    // Keys may be gone with the terminal; nothing to do about it.
    let _ = input.stop();
    drop(screen);
    let mut marks = marks_in(given.try_iter());
    let problem = match ran {
        Ok(Ended::Stopped | Ended::Closed) => None,
        Err(RunError::InputLost(kind)) => Some(format!("the keyboard was lost: {kind}")),
        Err(RunError::Terminal(e)) => Some(format!("the screen failed: {e}")),
        Err(RunError::CommandsClosed(command)) => {
            marks += marks_in([command]);
            None
        }
    };
    Ok(Shown { marks, problem })
}

/// How many marks and notes `commands` gives. The stop the screen also
/// gives needs nothing more: the screen has closed, and the recording stops
/// as it does however the screen closes.
fn marks_in(commands: impl IntoIterator<Item = Command>) -> usize {
    commands
        .into_iter()
        .filter(|command| match command {
            Command::Mark(_) | Command::Note(_) => true,
            Command::Start(_) | Command::Stop => false,
        })
        .count()
}

pub(super) fn note_published(outcome: &mut Outcome, report: &PublishReport) {
    outcome.segments = report.rows().len();
    outcome.complete = report.is_complete();
    if !report.is_complete() {
        let mut note = format!(
            "{} journals weren't published; the next start salvages them",
            report.left().len()
        );
        if let Some(e) = report.errors().last() {
            let _ = write!(note, " (last error: {e})");
        }
        outcome.notes.push(note);
    }
}

/// How the summary names a track.
pub(super) fn track_name(track: Option<TrackId>) -> &'static str {
    match track {
        Some(MIC) => "the mic",
        Some(SYSTEM) => "the system audio",
        _ => "a track",
    }
}

#[cfg(test)]
mod tests {
    use nota_core::SessionTime;
    use nota_core::recorder::{Input, Mark, Note, Setup};

    use super::*;

    /// The marks and notes the screen gives reach the summary; the start
    /// and the stop aren't counted as either.
    #[test]
    fn the_summary_counts_the_marks_and_notes_given() {
        let at = SessionTime::from_nanos(5);
        let given = [
            Command::Start(Setup {
                title: "Workshop".to_owned(),
                mic: Input::Default,
                system: Input::Default,
            }),
            Command::Mark(Mark { at }),
            Command::Note(Note::new(at, "ask about clamps").unwrap()),
            Command::Mark(Mark { at }),
            Command::Stop,
        ];
        assert_eq!(marks_in(given), 3);
        assert_eq!(marks_in([Command::Stop]), 0);
    }
}
