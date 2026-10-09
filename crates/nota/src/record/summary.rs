//! How a recording went: the screen, whose result feeds the summary, and
//! the summary's notes.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use nota_core::recorder::Command;
pub(crate) use nota_core::recorder::Outcome;
use nota_core::{Clock, TrackId};
use nota_recorder::segment::PublishReport;
use nota_tui::{Ended, Event, InputThread, Recording, RunError, Theme};

use super::{MIC, SYSTEM};
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
    title: &str,
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
        title.to_owned(),
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
    if !report.left().is_empty() {
        let mut note = format!(
            "{} journals weren't published; the next start salvages them",
            report.left().len()
        );
        if let Some(e) = report.errors().last() {
            let _ = write!(note, " (last error: {e})");
        }
        outcome.notes.push(note);
    }
    if !report.set_aside().is_empty() {
        let names = file_names(report.set_aside());
        let (damaged, are) = if report.set_aside().len() == 1 {
            ("a journal was", "is")
        } else {
            ("journals were", "are")
        };
        outcome.notes.push(format!(
            "{damaged} damaged, so part of the recording wasn't published: \
             {names} {are} kept, and what read before the damage was published"
        ));
    }
}

/// The names of `files`, without their directories, joined with commas.
pub(super) fn file_names(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|f| {
            f.file_name().map_or_else(
                || f.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
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
    use nota_core::recorder::{Input, Mark, Note, Setup};
    use nota_core::{
        EpochId, FakeClock, SampleCount, SampleIndex, SampleRate, SessionId, SessionTime,
    };
    use nota_recorder::fs::fake::FakeFs;
    use nota_recorder::fs::{Fs as _, FsFile as _};
    use nota_recorder::segment::{FakeStore, Publisher, SegmentLength};
    use nota_recorder::session::{SessionDir, SessionStore, SessionWriter};

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

    /// A journal damaged in the middle, with synced audio after it, is set
    /// aside; the summary says so and doesn't count it as left.
    #[test]
    fn a_journal_set_aside_is_named_in_the_summary() {
        use nota_recorder::journal::format::{FRAME_HEADER_LEN, HEADER_LEN};

        let session = PathBuf::from("/session");
        let db = PathBuf::from("/db");
        let fs = FakeFs::with_dirs([session.clone(), db.clone()]);
        let lock = SessionDir::new(SessionId::new(1), fs.clone(), &session)
            .lock()
            .unwrap();
        let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
        let mut writer =
            SessionWriter::open(&lock, SampleRate::new(1_000).unwrap(), length, clock).unwrap();
        writer
            .start_track(TrackId::new(0), EpochId::new(0), SampleIndex::ZERO)
            .unwrap();
        for k in 0..20_i16 {
            let audio: Vec<i16> = (0..50).map(|i| k * 50 + i).collect();
            writer.append(TrackId::new(0), &audio).unwrap();
        }
        let journals = writer.finish().unwrap();
        let path = session.join(journals[0].id().file_name());
        let mut bytes = fs.read(&path).unwrap();
        bytes[HEADER_LEN + 2 * (FRAME_HEADER_LEN + 100) + FRAME_HEADER_LEN + 7] ^= 0x40;
        fs.remove(&path).unwrap();
        fs.create(&path).unwrap().write_all(&bytes).unwrap();

        let publisher =
            Publisher::spawn(SessionStore::new(lock, FakeStore::new(&fs, &db)), length).unwrap();
        assert!(publisher.queue().send(journals));
        let report = publisher.finish().unwrap();
        assert_eq!(
            report.set_aside(),
            [session.join("journal-000000.unreadable")]
        );
        assert!(!report.is_complete());

        let mut outcome = Outcome::default();
        note_published(&mut outcome, &report);
        assert!(!outcome.complete);
        assert_eq!(outcome.segments, 1);
        assert_eq!(
            outcome.notes,
            [
                "a journal was damaged, so part of the recording wasn't published: \
                 journal-000000.unreadable is kept, and what read before the damage \
                 was published"
            ]
        );
    }
}
