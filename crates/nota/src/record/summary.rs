//! How a recording went: the summary's `Outcome`, and the screen whose
//! result feeds it.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use nota_core::{Clock, TrackId};
use nota_recorder::segment::PublishReport;
use nota_tui::{Annotation, Ended, Event, InputThread, Recording, RunError, Theme};

use super::{MIC, RecordArgs, SYSTEM};
use crate::terminal::Screen;

/// How a recording went, for the summary printed after it.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// The session's directory.
    pub(crate) session: PathBuf,
    /// Things worth saying: sessions salvaged, streams that didn't start,
    /// journals left for salvage.
    pub(crate) notes: Vec<String>,
    /// Segments published.
    pub(crate) segments: usize,
    /// Whether everything recorded was published, with nothing left for
    /// the next start's salvage.
    pub(crate) complete: bool,
}

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
        args.title.clone(),
        listening.to_owned(),
        Arc::clone(clock),
        theme,
    );
    let (annotations, made) = mpsc::channel::<Annotation>();
    let input = InputThread::spawn(ui.clone(), Arc::clone(clock))?;
    let ran = nota_tui::run(screen.terminal(), &mut recording, ui_events, &annotations);
    // Keys may be gone with the terminal; nothing to do about it.
    let _ = input.stop();
    drop(screen);
    let marks = made.try_iter().count();
    let (marks, problem) = match ran {
        Ok(Ended::Stopped | Ended::Closed) => (marks, None),
        Err(RunError::InputLost(kind)) => (marks, Some(format!("the keyboard was lost: {kind}"))),
        Err(RunError::Terminal(e)) => (marks, Some(format!("the screen failed: {e}"))),
        Err(RunError::AnnotationsClosed(_)) => (marks + 1, None),
    };
    Ok(Shown { marks, problem })
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
