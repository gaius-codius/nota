use std::sync::Arc;

use nota_core::{Clock, EpochId, FakeClock, SampleIndex, SampleRate, SessionTime, TrackId};
use nota_recorder::session::SessionWriter;

use super::*;

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-library-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn length() -> SegmentLength {
    SegmentLength::default_at(SampleRate::SPEECH)
}

fn ids(library: &Library) -> Vec<u64> {
    library
        .existing()
        .unwrap()
        .iter()
        .map(|s| s.id.get())
        .collect()
}

#[test]
fn sessions_are_numbered_from_one_and_never_reused() {
    let tmp = TestDir::new("numbers");
    // The data directory and its parents are made as needed.
    let library = Library::open(&tmp.0.join("a/b")).unwrap();
    assert!(ids(&library).is_empty());
    let first = library.create().unwrap();
    assert_eq!(first.id, SessionId::new(1));
    assert_eq!(first.dir, tmp.0.join("a/b/sessions/1"));
    assert!(StdFs.list(&first.audio()).unwrap().is_empty());
    assert_eq!(first.store(), first.dir.join("nota.db"));
    library.create().unwrap();
    // Something else under sessions/, and a gap: the next follows the
    // highest number.
    StdFs.create_dir(&tmp.0.join("a/b/sessions/7")).unwrap();
    StdFs.create_dir(&tmp.0.join("a/b/sessions/notes")).unwrap();
    // Not a name a session is given.
    StdFs.create_dir(&tmp.0.join("a/b/sessions/09")).unwrap();
    StdFs.create_dir(&tmp.0.join("a/b/sessions/+10")).unwrap();
    assert_eq!(ids(&library), [1, 2, 7]);
    assert_eq!(library.create().unwrap().id, SessionId::new(8));
    // Opening again keeps them.
    let again = Library::open(&tmp.0.join("a/b")).unwrap();
    assert_eq!(ids(&again), [1, 2, 7, 8]);
}

/// Records a little into `session` and leaves its journal unpublished, as
/// a crash would.
fn leave_a_journal(session: &SessionPaths) {
    let lock = SessionDir::new(session.id, StdFs, &session.audio())
        .lock()
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let mut writer = SessionWriter::open(&lock, SampleRate::SPEECH, length(), clock).unwrap();
    writer
        .start_track(TrackId::new(0), EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    writer.append(TrackId::new(0), &[1, 2, 3]).unwrap();
    writer.finish().unwrap();
}

#[test]
fn salvage_at_start_publishes_what_was_left_once() {
    let tmp = TestDir::new("salvage");
    let library = Library::open(&tmp.0).unwrap();
    let clean = library.create().unwrap();
    let crashed = library.create().unwrap();
    leave_a_journal(&crashed);
    let dir = SessionDir::new(crashed.id, StdFs, &crashed.audio());
    assert!(needs_salvage(&dir).unwrap());

    let done = library.salvage_all(length()).unwrap();
    assert!(
        matches!(done[..], [Salvaged::Done(id)] if id == crashed.id),
        "{done:?}"
    );
    assert!(!needs_salvage(&dir).unwrap());
    let rows = Store::open(&crashed.store()).unwrap().segments().unwrap();
    assert_eq!(rows.len(), 1);
    // The clean session wasn't touched: no store made for it.
    assert!(!StdFs.list(&clean.dir).unwrap().contains(&clean.store()));
    // The next start finds nothing to do.
    assert!(library.salvage_all(length()).unwrap().is_empty());
}

#[test]
fn a_session_in_use_is_left_alone() {
    let tmp = TestDir::new("in-use");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    leave_a_journal(&session);
    let held = SessionDir::new(session.id, StdFs, &session.audio())
        .lock()
        .unwrap();
    let done = library.salvage_all(length()).unwrap();
    assert!(
        matches!(done[..], [Salvaged::InUse(id)] if id == session.id),
        "{done:?}"
    );
    drop(held);
    assert!(matches!(
        library.salvage_all(length()).unwrap()[..],
        [Salvaged::Done(_)]
    ));
}

#[test]
fn a_store_that_cant_open_fails_that_session_only() {
    let tmp = TestDir::new("bad-store");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    leave_a_journal(&session);
    // A directory where the store should be.
    StdFs.create_dir(&session.store()).unwrap();
    let done = library.salvage_all(length()).unwrap();
    assert!(
        matches!(&done[..], [Salvaged::Failed(id, _)] if *id == session.id),
        "{done:?}"
    );
    assert!(needs_salvage(&SessionDir::new(session.id, StdFs, &session.audio())).unwrap());
}

#[test]
fn session_numbers_that_run_out_are_an_error() {
    let tmp = TestDir::new("overflow");
    let library = Library::open(&tmp.0).unwrap();
    StdFs
        .create_dir(&tmp.0.join("sessions").join(u64::MAX.to_string()))
        .unwrap();
    let err = library.create().unwrap_err();
    assert!(err.to_string().contains("free session number"), "{err}");
}

#[test]
fn a_relative_directory_is_in_the_working_directory() {
    assert_eq!(parent_of(Path::new("rec")), Some(Path::new(".")));
    assert_eq!(parent_of(Path::new("a/rec")), Some(Path::new("a")));
    assert_eq!(parent_of(Path::new("/rec")), Some(Path::new("/")));
    assert_eq!(parent_of(Path::new("/")), None);
}

#[test]
fn salvage_that_leaves_journals_says_so() {
    let tmp = TestDir::new("left");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    // A directory under a journal's name: there, but it can't be read.
    let journal = nota_recorder::journal::JournalId::new(0).file_name();
    StdFs.create_dir(&session.audio().join(journal)).unwrap();
    let done = library.salvage_all(length()).unwrap();
    assert!(
        matches!(done[..], [Salvaged::Left(id)] if id == session.id),
        "{done:?}"
    );
}
