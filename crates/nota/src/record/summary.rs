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
    let passed = passing.join().unwrap_or(0);
    let (more, problem) = ended(ran, save);
    let screen = problem.is_none().then_some(screen);
    Ok((
        Shown {
            marks: passed + more,
            problem,
        },
        screen,
    ))
}

/// How the screen's loop ended: how many more marks and notes it gave
/// (handed to the saver), and the problem, if it didn't end as asked.
fn ended<E: std::fmt::Display>(
    ran: Result<Ended, RunError<E>>,
    save: &Sender<ToSave>,
) -> (usize, Option<String>) {
    match ran {
        Ok(Ended::Stopped | Ended::Closed) => (0, None),
        Err(RunError::InputLost(kind)) => (0, Some(format!("the keyboard was lost: {kind}"))),
        Err(RunError::Terminal(e)) => (0, Some(format!("the screen failed: {e}"))),
        // The thread that passes them on stopped, which ends the screen:
        // the one it couldn't take is saved here.
        Err(RunError::CommandsClosed(command)) => (
            pass_on([command], save),
            Some(
                "marks and notes could no longer be passed on, so the recording stopped".to_owned(),
            ),
        ),
    }
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
    outcome.notes.extend(held_notes(report.held()));
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

/// Names the files a publish or salvage run couldn't use or clean up.
pub(super) fn held_notes(held: &nota_recorder::segment::Published) -> Vec<String> {
    let mut notes = Vec::new();
    for (path, kind) in held.blocked() {
        notes.push(format!(
            "{}: {}; its audio is still to publish",
            file_name(path),
            file_cause(*kind)
        ));
    }
    notes.extend(cleanup_notes(held));
    for (path, kind) in held.temps_kept() {
        notes.push(format!(
            "{}: couldn't remove the temporary file ({})",
            file_name(path),
            file_cause(*kind)
        ));
    }
    for (journal, kind) in held.unread() {
        notes.push(format!(
            "{}: couldn't read it ({})",
            journal.id().file_name(),
            file_cause(*kind)
        ));
    }
    notes
}

/// Names journals kept only because deleting or setting them aside failed.
fn cleanup_notes(held: &nota_recorder::segment::Published) -> Vec<String> {
    let mut notes = Vec::new();
    for (id, kind) in held.not_deleted() {
        notes.push(format!(
            "{}: couldn't delete it ({}); all its audio is published",
            id.file_name(),
            file_cause(*kind)
        ));
    }
    for (path, kind) in held.not_set_aside() {
        let why = if *kind == io::ErrorKind::AlreadyExists {
            "aside name taken".to_owned()
        } else {
            format!("couldn't set it aside ({})", file_cause(*kind))
        };
        notes.push(format!(
            "{}: {why}; the damaged journal is kept",
            file_name(path)
        ));
    }
    notes
}

/// The file's own name, so startup and the summary use the same spelling.
fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The cause of a name that can't be used.
fn file_cause(kind: io::ErrorKind) -> String {
    match kind {
        io::ErrorKind::IsADirectory | io::ErrorKind::DirectoryNotEmpty => {
            "a directory is in the way".to_owned()
        }
        io::ErrorKind::AlreadyExists => "the name is taken".to_owned(),
        io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        _ => kind.to_string(),
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

    /// A mark or note the screen couldn't hand over is saved, counted,
    /// and the stop it caused is explained.
    #[test]
    fn a_command_the_screen_couldnt_hand_over_is_saved() {
        let at = SessionTime::from_nanos(9);
        let note = Note::new(at, "last thought").unwrap();
        let (save, saved) = mpsc::channel();
        let (more, problem) = ended(
            Err(RunError::<io::Error>::CommandsClosed(Command::Note(
                note.clone(),
            ))),
            &save,
        );
        assert_eq!(more, 1);
        assert!(problem.unwrap().contains("marks and notes"));
        assert_eq!(
            saved.try_iter().collect::<Vec<_>>(),
            [ToSave::Annotation(Annotation::Note(note))]
        );
        assert_eq!(
            ended(Ok::<_, RunError<io::Error>>(Ended::Stopped), &save),
            (0, None)
        );
        assert_eq!(
            ended(
                Err(RunError::<io::Error>::CommandsClosed(Command::Stop)),
                &save
            )
            .0,
            0
        );
        assert!(saved.try_iter().next().is_none());
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

    /// Whether the fixture keeps all its audio or has damage between frames.
    #[derive(Clone, Copy)]
    enum JournalAudio {
        Intact,
        Damaged,
    }

    /// A finished journal ready for the real publisher to inspect.
    struct PublishFixture {
        fs: FakeFs,
        lock: nota_recorder::session::SessionLock<FakeFs>,
        journals: Vec<nota_recorder::session::FinishedJournal>,
        length: SegmentLength,
    }

    impl PublishFixture {
        /// Records synced frames, retaining the journal until publication.
        fn recorded(audio: JournalAudio) -> Self {
            let session = PathBuf::from("/held-session");
            let fs = FakeFs::with_dirs([session.clone(), PathBuf::from("/held-db")]);
            let lock = SessionDir::new(SessionId::new(1), fs.clone(), &session)
                .lock()
                .unwrap();
            let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
            let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
            let mut writer =
                SessionWriter::open(&lock, SampleRate::new(1_000).unwrap(), length, clock).unwrap();
            writer
                .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
                .unwrap();
            for _ in 0..20 {
                writer.append(MIC, &[7; 50]).unwrap();
            }
            let journals = writer.finish().unwrap();
            let fixture = Self {
                fs,
                lock,
                journals,
                length,
            };
            if matches!(audio, JournalAudio::Damaged) {
                fixture.damage_middle_frame();
            }
            fixture
        }

        /// Damage after two frames leaves readable audio on both sides.
        fn damage_middle_frame(&self) {
            use nota_recorder::journal::format::{FRAME_HEADER_LEN, HEADER_LEN};
            let path = self.journal_path();
            let mut bytes = self.fs.read(&path).unwrap();
            bytes[HEADER_LEN + 2 * (FRAME_HEADER_LEN + 100) + FRAME_HEADER_LEN + 7] ^= 0x40;
            self.fs.remove(&path).unwrap();
            self.fs.create(&path).unwrap().write_all(&bytes).unwrap();
        }

        fn journal_path(&self) -> PathBuf {
            PathBuf::from("/held-session").join(self.journals[0].id().file_name())
        }

        /// Finishing retries pending work and produces the summary's report.
        fn publish(self) -> (PublishReport, Outcome) {
            let publisher = Publisher::spawn(
                SessionStore::new(
                    self.lock,
                    FakeStore::new(&self.fs, &PathBuf::from("/held-db")),
                ),
                self.length,
            )
            .unwrap();
            assert!(publisher.queue().send(self.journals));
            let report = publisher.finish().unwrap();
            let mut outcome = Outcome::default();
            note_published(&mut outcome, &report);
            (report, outcome)
        }
    }

    /// A directory occupying the final segment name explains unpublished audio.
    #[test]
    fn the_summary_names_a_directory_blocking_publication() {
        let fixture = PublishFixture::recorded(JournalAudio::Intact);
        // The writer has finished; only publication meets the obstructing directory.
        fixture
            .fs
            .create_dir(&PathBuf::from("/held-session/seg-t0-000000000000.flac"))
            .unwrap();
        let (report, outcome) = fixture.publish();
        assert_eq!(report.left().len(), 1);
        assert!(!outcome.complete);
        assert!(outcome.notes.iter().any(|note| note ==
            "seg-t0-000000000000.flac: a directory is in the way; its audio is still to publish"));
    }

    /// An undeletable journal explains cleanup without claiming audio is unsaved.
    #[test]
    fn the_summary_says_an_undeletable_journals_audio_is_published() {
        use nota_recorder::fs::fake::Fault;
        let fixture = PublishFixture::recorded(JournalAudio::Intact);
        // Refuse only cleanup, after the segment and its row become durable.
        fixture.fs.fail_on(
            &fixture.journal_path(),
            Fault::Remove,
            io::ErrorKind::PermissionDenied,
        );
        let (report, outcome) = fixture.publish();
        assert!(report.left().is_empty());
        assert!(outcome.complete);
        assert_eq!(outcome.segments, 1);
        assert_eq!(
            outcome.notes,
            ["journal-000000: couldn't delete it (permission denied); all its audio is published"]
        );
    }

    /// A taken aside name explains why a damaged journal remains under its name.
    #[test]
    fn the_summary_names_a_taken_aside_name() {
        let fixture = PublishFixture::recorded(JournalAudio::Damaged);
        // Preserve an earlier aside file, forcing the collision path to keep both.
        fixture
            .fs
            .create(&PathBuf::from("/held-session/journal-000000.unreadable"))
            .unwrap()
            .write_all(b"previous damaged journal")
            .unwrap();
        let (report, outcome) = fixture.publish();
        assert_eq!(report.left().len(), 1);
        assert!(!outcome.complete);
        assert!(report.set_aside().is_empty());
        assert!(outcome.notes.iter().any(|note| note
            == "journal-000000: aside name taken; the damaged journal is kept"));
    }
}
