//! How a recording went: the screen, whose result feeds the summary, and
//! the summary's notes.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use nota_core::recorder::Command;
pub(crate) use nota_core::recorder::Outcome;
use nota_core::{Clock, TrackId};
use nota_recorder::segment::PublishReport;
use nota_store::Annotation;
use nota_tui::{Ended, Event, InputThread, Recording, RunError, Theme};

use super::save::{Saved, ToSave};
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

/// Shows the Recording screen on `screen` until it's closed, handing each
/// mark and note to the saver (`save`) as it's made. The terminal is
/// handed back as it is, unless it failed: then it's restored here, and
/// `None` comes back.
///
/// # Errors
///
/// Only if the input thread, or the thread that passes marks and notes
/// on, can't start; the terminal is restored.
pub(super) fn show(
    mut screen: Screen,
    title: &str,
    listening: &str,
    clock: &Arc<dyn Clock>,
    ui: &Sender<Event>,
    ui_events: &Receiver<Event>,
    save: &Sender<ToSave>,
) -> io::Result<(Shown, Option<Screen>)> {
    let mut recording = Recording::new(
        title.to_owned(),
        listening.to_owned(),
        Arc::clone(clock),
        Theme::load(),
    );
    let (commands, given) = mpsc::channel::<Command>();
    let passing = {
        let save = save.clone();
        thread::Builder::new()
            .name("nota-marks".into())
            .spawn(move || pass_on(given, &save))?
    };
    let input = InputThread::spawn(ui.clone(), Arc::clone(clock))?;
    // A new screen (after Home, say): drawn whole, not as changes to the
    // last one's cells.
    let ran = screen
        .clear()
        .map_err(RunError::Terminal)
        .and_then(|()| nota_tui::run(screen.terminal(), &mut recording, ui_events, &commands));
    // Keys may be gone with the terminal; nothing to do about it.
    let _ = input.stop();
    // The screen is done giving commands: the thread passes on the last.
    drop(commands);
    let mut marks = passing.join().unwrap_or(0);
    let problem = match ran {
        Ok(Ended::Stopped | Ended::Closed) => None,
        Err(RunError::InputLost(kind)) => Some(format!("the keyboard was lost: {kind}")),
        Err(RunError::Terminal(e)) => Some(format!("the screen failed: {e}")),
        // The thread that passes them on stopped: this one is saved here.
        Err(RunError::CommandsClosed(command)) => {
            marks += pass_on([command], save);
            None
        }
    };
    let screen = problem.is_none().then_some(screen);
    Ok((Shown { marks, problem }, screen))
}

/// Hands each mark and note `commands` gives to the saver, as it comes,
/// and says how many there were. The start and the stop the screen also
/// gives need nothing more: the screen has closed, and the recording stops
/// as it does however the screen closes.
fn pass_on(commands: impl IntoIterator<Item = Command>, save: &Sender<ToSave>) -> usize {
    let mut made = 0;
    for command in commands {
        let annotation = match command {
            Command::Mark(mark) => Annotation::Mark(mark),
            Command::Note(note) => Annotation::Note(note),
            Command::Start(_) | Command::Stop => continue,
        };
        made += 1;
        // A saver that has stopped has said why in its report.
        let _ = save.send(ToSave::Annotation(annotation));
    }
    made
}

/// What the summary says of what the saver couldn't store, of `made`
/// marks and notes.
pub(super) fn note_saved(outcome: &mut Outcome, saved: &Saved, made: usize) {
    let why = saved
        .error
        .as_deref()
        .map(|e| format!(" (last error: {e})"))
        .unwrap_or_default();
    if saved.lost_text > 0 {
        outcome.notes.push(format!(
            "{} lines of live text weren't saved to the library{why}; the audio has them",
            saved.lost_text
        ));
    }
    if saved.lost_annotations > 0 {
        outcome.notes.push(format!(
            "{} of {made} marks and notes weren't saved to the library{why}",
            saved.lost_annotations
        ));
    }
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

    /// The marks and notes the screen gives are handed to the saver, in
    /// order, and counted; the start and the stop aren't either.
    #[test]
    fn marks_and_notes_given_are_handed_to_the_saver() {
        let at = SessionTime::from_nanos(5);
        let note = Note::new(at, "ask about clamps").unwrap();
        let given = [
            Command::Start(Setup {
                title: "Workshop".to_owned(),
                mic: Input::Default,
                system: Input::Default,
            }),
            Command::Mark(Mark { at }),
            Command::Note(note.clone()),
            Command::Mark(Mark { at }),
            Command::Stop,
        ];
        let (save, saved) = mpsc::channel();
        assert_eq!(pass_on(given, &save), 3);
        assert_eq!(
            saved.try_iter().collect::<Vec<_>>(),
            [
                ToSave::Annotation(Annotation::Mark(Mark { at })),
                ToSave::Annotation(Annotation::Note(note)),
                ToSave::Annotation(Annotation::Mark(Mark { at })),
            ]
        );
        assert_eq!(pass_on([Command::Stop], &save), 0);
        // A saver that has gone doesn't stop the counting.
        drop(saved);
        assert_eq!(pass_on([Command::Mark(Mark { at })], &save), 1);
    }

    #[test]
    fn the_summary_says_what_wasn_t_saved() {
        let mut outcome = Outcome::default();
        note_saved(&mut outcome, &Saved::default(), 2);
        assert!(outcome.notes.is_empty());
        let saved = Saved {
            text: 4,
            annotations: 1,
            lost_text: 3,
            lost_annotations: 1,
            error: Some("disk I/O error".to_owned()),
        };
        note_saved(&mut outcome, &saved, 2);
        assert_eq!(
            outcome.notes,
            [
                "3 lines of live text weren't saved to the library \
                 (last error: disk I/O error); the audio has them",
                "1 of 2 marks and notes weren't saved to the library \
                 (last error: disk I/O error)"
            ]
        );
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
