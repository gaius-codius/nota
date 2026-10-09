use std::sync::Arc;

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRate, SessionTime, TrackId,
};
use nota_recorder::fs::FsFile as _;
use nota_recorder::segment::publish_journals;
use nota_recorder::session::{FinishedJournal, SessionLock, SessionWriter};
use nota_store::{NewSession, SegmentRow, Track, TrackKind};

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

/// The session's rows in the library database.
fn segments(library: &Library, id: SessionId) -> Vec<SegmentRow> {
    library.db().with(|db| db.segments(id)).unwrap()
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
    assert_eq!(first.per_session_store(), first.dir.join("nota.db"));
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
        matches!(done[..], [Salvaged::Done(id, _)] if id == crashed.id),
        "{done:?}"
    );
    assert!(!needs_salvage(&dir).unwrap());
    let rows = segments(&library, crashed.id);
    assert_eq!(rows.len(), 1);
    // The clean session has no rows, and holds nothing yet, so it isn't
    // in the library.
    assert!(segments(&library, clean.id).is_empty());
    let listed = library.db().with(|db| db.sessions()).unwrap();
    assert_eq!(
        listed.iter().map(|s| s.id).collect::<Vec<_>>(),
        [crashed.id]
    );
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
        [Salvaged::Done(..)]
    ));
}

#[test]
fn a_library_that_cant_open_leaves_journals_for_later() {
    let tmp = TestDir::new("bad-store");
    let library = Library::open(&tmp.0).unwrap();
    // One with nothing to salvage, which isn't reported.
    library.create().unwrap();
    let session = library.create().unwrap();
    leave_a_journal(&session);
    // At the next start, the database can't be opened.
    drop(library);
    break_db(&tmp.0);
    let library = Library::open(&tmp.0).unwrap();
    let done = library.salvage_all(length()).unwrap();
    assert!(
        matches!(&done[..], [Salvaged::Failed(id, why)] if *id == session.id && why.contains("library")),
        "{done:?}"
    );
    assert!(needs_salvage(&SessionDir::new(session.id, StdFs, &session.audio())).unwrap());
    // A new session is still numbered after every one on disk.
    assert_eq!(library.create().unwrap().id.get(), 3);
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
        matches!(done[..], [Salvaged::Left(id, _)] if id == session.id),
        "{done:?}"
    );
}

/// Records twenty 50-sample frames into `session` and damages the third
/// frame's samples, so two frames read and seventeen with synced audio
/// follow the damage, at a rate where they fill a whole segment window.
/// Returns the journal's file name.
fn leave_a_journal_damaged_in_the_middle(session: &SessionPaths, window: SegmentLength) -> String {
    use nota_recorder::journal::format::{FRAME_HEADER_LEN, HEADER_LEN};

    let lock = SessionDir::new(session.id, StdFs, &session.audio())
        .lock()
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let mut writer =
        SessionWriter::open(&lock, SampleRate::new(1_000).unwrap(), window, clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    for k in 0..20_i16 {
        let audio: Vec<i16> = (0..50).map(|i| k * 50 + i).collect();
        writer.append(MIC, &audio).unwrap();
    }
    let journals = writer.finish().unwrap();
    assert_eq!(journals.len(), 1);
    let name = journals[0].id().file_name();
    let path = session.audio().join(&name);
    let mut bytes = StdFs.read(&path).unwrap();
    bytes[HEADER_LEN + 2 * (FRAME_HEADER_LEN + 100) + FRAME_HEADER_LEN + 7] ^= 0x40;
    StdFs.remove(&path).unwrap();
    StdFs.create(&path).unwrap().write_all(&bytes).unwrap();
    name
}

#[test]
fn salvage_at_start_names_a_journal_it_sets_aside() {
    let tmp = TestDir::new("set-aside");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    let window = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let name = leave_a_journal_damaged_in_the_middle(&session, window);

    let done = library.salvage_all(window).unwrap();
    let [Salvaged::Done(id, aside)] = &done[..] else {
        panic!("{done:?}");
    };
    assert_eq!(*id, session.id);
    let kept = session.audio().join(format!("{name}.unreadable"));
    assert_eq!(aside, std::slice::from_ref(&kept));
    assert!(kept.exists());
    assert!(!session.audio().join(&name).exists());
    // What read before the damage was published.
    assert_eq!(covered(&segments(&library, session.id)), [(0, 100)]);
    assert_eq!(kept.file_name().unwrap(), "journal-000000.unreadable");
}

const MIC: TrackId = TrackId::new(0);

/// The `n`th sample of the test recordings.
fn sample(n: u64) -> i16 {
    i16::try_from(n % 2_000).unwrap()
}

/// Records `count` samples on the mic into `session`, resuming where it
/// left off in a new epoch, and returns the finished journals unpublished.
fn record(session: &SessionPaths, count: u64) -> (SessionLock<StdFs>, Vec<FinishedJournal>) {
    let lock = SessionDir::new(session.id, StdFs, &session.audio())
        .lock()
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let mut writer = SessionWriter::open(&lock, SampleRate::SPEECH, length(), clock).unwrap();
    let from = writer.first_free_sample(MIC);
    let epoch = writer
        .highest_epoch(MIC)
        .map_or(EpochId::new(0), |e| EpochId::new(e.get() + 1));
    writer.start_track(MIC, epoch, from).unwrap();
    let samples: Vec<i16> = (from.get()..from.get() + count).map(sample).collect();
    writer.append(MIC, &samples).unwrap();
    (lock, writer.finish().unwrap())
}

/// A new session's rows, as `nota record` binds them.
fn new_rows(library: &Library, id: SessionId) -> NewSessionRows {
    NewSessionRows::new(
        library.db().clone(),
        NewSession {
            id,
            title: Some(format!("lecture {}", id.get())),
            language: None,
            tracks: vec![Track {
                track: MIC,
                kind: TrackKind::Microphone,
                source: Some("mic".to_owned()),
            }],
        },
    )
}

/// The mic's samples that `rows` cover, as ranges with touching ones
/// joined.
fn covered(rows: &[SegmentRow]) -> Vec<(u64, u64)> {
    let mut joined: Vec<(u64, u64)> = Vec::new();
    for r in rows.iter().filter(|r| r.track() == MIC) {
        let (start, end) = (r.range().start().get(), r.range().end().get());
        match joined.last_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => joined.push((start, end)),
        }
    }
    joined
}

/// The library database's files under `root`, with `suffix` added.
fn db_files(root: &Path, suffix: &str) -> [PathBuf; 3] {
    ["", "-wal", "-shm"].map(|part| root.join(format!("library.db{part}{suffix}")))
}

/// Takes the library database under `root` away: its files are moved
/// aside, and a file that isn't a database is put in their place.
fn break_db(root: &Path) {
    for (from, to) in db_files(root, "").iter().zip(&db_files(root, ".aside")) {
        match StdFs.rename(from, to) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            done => done.unwrap(),
        }
    }
    let mut junk = StdFs.create(&root.join("library.db")).unwrap();
    junk.write_all(&[0xA5; 4_096]).unwrap();
}

/// Brings back what [`break_db`] took away.
fn mend_db(root: &Path) {
    StdFs.remove(&root.join("library.db")).unwrap();
    for (from, to) in db_files(root, ".aside").iter().zip(&db_files(root, "")) {
        match StdFs.rename(from, to) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            done => done.unwrap(),
        }
    }
}

/// Removes the library database under `root`, as before there was one.
fn remove_db(root: &Path) {
    for file in db_files(root, "") {
        match StdFs.remove(&file) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            done => done.unwrap(),
        }
    }
}

#[test]
fn a_new_sessions_row_is_added_before_its_first_segment() {
    let tmp = TestDir::new("new-rows");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    assert!(
        library
            .db()
            .with(|db| db.session(session.id))
            .unwrap()
            .is_none()
    );
    let (lock, finished) = record(&session, 3_000);
    let mut bound = SessionStore::new(lock, new_rows(&library, session.id));
    publish_journals(&mut bound, length(), &finished).unwrap();
    let row = library
        .db()
        .with(|db| db.session(session.id))
        .unwrap()
        .unwrap();
    assert_eq!(row.title.as_deref(), Some("lecture 1"));
    assert_eq!(row.state, SessionState::Recording);
    assert_eq!(
        library.db().with(|db| db.tracks(session.id)).unwrap()[0].kind,
        TrackKind::Microphone
    );
    assert_eq!(covered(&segments(&library, session.id)), [(0, 3_000)]);
    // At the next start no one is recording it: it's marked stopped.
    drop(bound);
    assert!(library.salvage_all(length()).unwrap().is_empty());
    assert_eq!(
        library
            .db()
            .with(|db| db.session(session.id))
            .unwrap()
            .unwrap()
            .state,
        SessionState::Stopped
    );
}

#[test]
fn a_session_number_the_library_holds_already_is_refused() {
    let tmp = TestDir::new("taken");
    let library = Library::open(&tmp.0).unwrap();
    let taken = SessionId::new(5);
    library
        .db()
        .with(|db| db.adopt_session(taken, None))
        .unwrap();
    // New sessions are numbered after the database's too.
    assert_eq!(library.create().unwrap().id.get(), 6);
    // Rows bound to a session the database already has under another
    // title are refused, so they can't claim its samples.
    let mut rows = new_rows(&library, taken);
    assert!(matches!(
        rows.rows(taken),
        Err(StoreError::SessionExists(id)) if id == taken
    ));
}

#[test]
fn a_resumed_session_records_without_the_database_and_publishes_once_it_is_back() {
    let tmp = TestDir::new("db-gone");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    let (lock, finished) = record(&session, 2_000);
    let mut bound = SessionStore::new(lock, new_rows(&library, session.id));
    publish_journals(&mut bound, length(), &finished).unwrap();
    drop(bound);
    drop(library);

    // The database goes.
    break_db(&tmp.0);
    let library = Library::open(&tmp.0).unwrap();

    // The session is resumed and records; publishing fails, and leaves
    // every journal where it is.
    let (lock, finished) = record(&session, 3_000);
    assert!(!finished.is_empty());
    let mut bound = SessionStore::new(lock, library.db().clone());
    assert!(publish_journals(&mut bound, length(), &finished).is_err());
    let dir = SessionDir::new(session.id, StdFs, &session.audio());
    assert!(needs_salvage(&dir).unwrap());

    // The database comes back, and the same handle publishes them.
    mend_db(&tmp.0);
    publish_journals(&mut bound, length(), &finished).unwrap();
    assert!(!needs_salvage(&dir).unwrap());
    let rows = segments(&library, session.id);
    assert_eq!(covered(&rows), [(0, 5_000)]);
    assert_eq!(
        rows.iter().map(|r| r.epoch().get()).max(),
        Some(1),
        "the resumed audio is in a new epoch"
    );
}

/// Makes the per-session store an M1b recording kept, at `path`, holding
/// `rows`.
fn per_session_store(path: &Path, rows: &[SegmentRow]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    conn.execute_batch(nota_store::schema::V1).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    for r in rows {
        conn.execute(
            "INSERT INTO segment VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                r.track().get(),
                r.epoch().get(),
                i64::try_from(r.range().start().get()).unwrap(),
                i64::try_from(r.range().end().get()).unwrap(),
                r.sha256().as_bytes().as_slice()
            ],
        )
        .unwrap();
    }
}

#[test]
fn an_m1b_session_directory_is_adopted_and_salvaged() {
    let tmp = TestDir::new("m1b");
    // A session recorded and published, then a second recording in it left
    // unpublished, as a crash would.
    let library = Library::open(&tmp.0).unwrap();
    let old = library.create().unwrap();
    let (lock, finished) = record(&old, 4_000);
    let mut bound = SessionStore::new(lock, new_rows(&library, old.id));
    publish_journals(&mut bound, length(), &finished).unwrap();
    drop(bound);
    let rows = segments(&library, old.id);
    assert_eq!(covered(&rows), [(0, 4_000)]);
    drop(record(&old, 1_500));
    drop(library);
    // As M1b left it: its rows in its own store, and no library database.
    per_session_store(&old.per_session_store(), &rows);
    remove_db(&tmp.0);

    let library = Library::open(&tmp.0).unwrap();
    let done = library.salvage_all(length()).unwrap();
    assert!(
        matches!(done[..], [Salvaged::Done(id, _)] if id == old.id),
        "{done:?}"
    );
    // Its rows came over, and salvage published the rest after them.
    let after = segments(&library, old.id);
    assert!(rows.iter().all(|r| after.contains(r)), "{after:?}");
    assert_eq!(covered(&after), [(0, 5_500)]);
    let row = library.db().with(|db| db.session(old.id)).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Stopped);
    assert_eq!(row.title, None);
    // The old store is left where it was.
    assert!(
        StdFs
            .list(&old.dir)
            .unwrap()
            .contains(&old.per_session_store())
    );

    // A new session at the same track and samples: each keeps its own.
    let new = library.create().unwrap();
    assert_eq!(new.id.get(), 2);
    let (lock, finished) = record(&new, 4_000);
    let mut bound = SessionStore::new(lock, new_rows(&library, new.id));
    publish_journals(&mut bound, length(), &finished).unwrap();
    assert_eq!(covered(&segments(&library, new.id)), [(0, 4_000)]);
    assert_eq!(segments(&library, old.id), after);
    // And the next start finds nothing more to do.
    drop(bound);
    assert!(library.salvage_all(length()).unwrap().is_empty());
}

#[test]
fn new_rows_take_their_own_row_back_but_no_one_elses() {
    let tmp = TestDir::new("own-row");
    let library = Library::open(&tmp.0).unwrap();
    let id = SessionId::new(4);
    let mine = || {
        let NewSessionRows { pending, .. } = new_rows(&library, id);
        pending.unwrap()
    };
    // Added by a call whose answer was lost: the same title, still
    // recording. Taken as added.
    library.db().with(|db| db.create_session(&mine())).unwrap();
    assert_eq!(new_rows(&library, id).rows(id).unwrap(), vec![]);
    // The same title, but stopped: not this recording's.
    library
        .db()
        .with(|db| db.set_state(id, SessionState::Stopped))
        .unwrap();
    assert!(matches!(
        new_rows(&library, id).rows(id),
        Err(StoreError::SessionExists(_))
    ));
    // Recording, but another title.
    let other = SessionId::new(5);
    let mut theirs = mine();
    theirs.id = other;
    theirs.title = Some("another lecture".to_owned());
    library.db().with(|db| db.create_session(&theirs)).unwrap();
    assert!(matches!(
        new_rows(&library, other).rows(other),
        Err(StoreError::SessionExists(_))
    ));
    // Asked about another session than its own.
    assert!(matches!(
        new_rows(&library, SessionId::new(6)).rows(other),
        Err(StoreError::NoSession(s)) if s == other
    ));
}

#[test]
fn a_new_session_records_without_the_database_and_is_added_once_it_is_back() {
    let tmp = TestDir::new("new-db-gone");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    drop(library);
    break_db(&tmp.0);
    let library = Library::open(&tmp.0).unwrap();
    let (lock, finished) = record(&session, 3_000);
    let mut bound = SessionStore::new(lock, new_rows(&library, session.id));
    assert!(publish_journals(&mut bound, length(), &finished).is_err());
    assert!(needs_salvage(&SessionDir::new(session.id, StdFs, &session.audio())).unwrap());
    mend_db(&tmp.0);
    publish_journals(&mut bound, length(), &finished).unwrap();
    let row = library
        .db()
        .with(|db| db.session(session.id))
        .unwrap()
        .unwrap();
    assert_eq!(row.title.as_deref(), Some("lecture 1"));
    assert_eq!(covered(&segments(&library, session.id)), [(0, 3_000)]);
}

#[test]
fn a_session_with_nothing_in_it_yet_is_left_alone() {
    let tmp = TestDir::new("fresh");
    let library = Library::open(&tmp.0).unwrap();
    // As another nota leaves it between making it and locking it.
    let fresh = library.create().unwrap();
    assert!(library.salvage_all(length()).unwrap().is_empty());
    assert_eq!(library.db().with(|db| db.session(fresh.id)).unwrap(), None);
    // Once it holds anything, it's adopted.
    drop(record(&fresh, 10));
    library.salvage_all(length()).unwrap();
    assert!(
        library
            .db()
            .with(|db| db.session(fresh.id))
            .unwrap()
            .is_some()
    );
}

#[cfg(unix)]
#[test]
fn a_session_directory_that_cant_be_listed_is_adopted_later() {
    use std::os::unix::fs::PermissionsExt as _;
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding: a session directory that can be entered but not listed"
    )]
    fn set_mode(dir: &Path, mode: u32) {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let tmp = TestDir::new("unlisted");
    let library = Library::open(&tmp.0).unwrap();
    let old = library.create().unwrap();
    let (lock, finished) = record(&old, 3_000);
    let mut bound = SessionStore::new(lock, new_rows(&library, old.id));
    publish_journals(&mut bound, length(), &finished).unwrap();
    drop(bound);
    let rows = segments(&library, old.id);
    drop(record(&old, 500));
    drop(library);
    per_session_store(&old.per_session_store(), &rows);
    remove_db(&tmp.0);

    let library = Library::open(&tmp.0).unwrap();
    set_mode(&old.dir, 0o300);
    let done = library.salvage_all(length()).unwrap();
    set_mode(&old.dir, 0o700);
    assert!(
        matches!(&done[..], [Salvaged::Failed(id, why)] if *id == old.id && why.contains("listing")),
        "{done:?}"
    );
    assert_eq!(library.db().with(|db| db.session(old.id)).unwrap(), None);
    // Listed again, the old store's rows come over.
    let done = library.salvage_all(length()).unwrap();
    assert!(matches!(done[..], [Salvaged::Done(..)]), "{done:?}");
    assert!(rows.iter().all(|r| segments(&library, old.id).contains(r)));
}
