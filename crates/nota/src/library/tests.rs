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
    let (_, epoch) = writer
        .open_first_epoch(TrackId::new(0), SessionTime::ZERO)
        .unwrap();
    writer.start_track(TrackId::new(0), &epoch).unwrap();
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
    let journal = JournalId::new(0).file_name();
    StdFs.create_dir(&session.audio().join(journal)).unwrap();
    let done = library.salvage_all(length()).unwrap();
    assert!(
        matches!(done[..], [Salvaged::Left(id, _, _)] if id == session.id),
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
    let (_, epoch) = writer.open_first_epoch(MIC, SessionTime::ZERO).unwrap();
    writer.start_track(MIC, &epoch).unwrap();
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
    // The new epoch starts where the earlier audio ends.
    let resumed_at = SampleCount::new(from.get())
        .duration_at(SampleRate::SPEECH)
        .and_then(|d| SessionTime::ZERO.checked_add(d))
        .unwrap();
    let (_, epoch) = writer.open_first_epoch(MIC, resumed_at).unwrap();
    writer.start_track(MIC, &epoch).unwrap();
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
            started_at: None,
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
        .with(|db| db.adopt_session(&NewSession::bare(taken), None))
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
#[expect(clippy::disallowed_methods, reason = "test scaffolding")]
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
    // Its rows came over (an M1 store kept no audio digests), and salvage
    // published the rest after them.
    let after = segments(&library, old.id);
    assert!(
        as_m1_kept(&rows).iter().all(|r| after.contains(r)),
        "{after:?}"
    );
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

/// The publisher's rows and the saver's are clones: whichever writes
/// first adds the session's row, and the other then never adds it again,
/// even once the session is stopped.
#[test]
fn clones_of_new_rows_add_the_session_s_row_once() {
    let tmp = TestDir::new("rows-clones");
    let library = Library::open(&tmp.0).unwrap();
    let id = SessionId::new(4);
    let publisher = new_rows(&library, id);
    let saver = publisher.clone();
    saver.added(id).unwrap();
    assert!(library.db().with(|db| db.session(id)).unwrap().is_some());
    library
        .db()
        .with(|db| db.set_state(id, SessionState::Stopped))
        .unwrap();
    // A fresh one would take the stopped row as another recording's.
    assert!(new_rows(&library, id).added(id).is_err());
    publisher.added(id).unwrap();
    saver.added(id).unwrap();
    assert!(matches!(
        saver.added(SessionId::new(5)),
        Err(StoreError::NoSession(_))
    ));
}

#[test]
fn new_rows_take_their_own_row_back_but_no_one_elses() {
    let tmp = TestDir::new("own-row");
    let library = Library::open(&tmp.0).unwrap();
    let id = SessionId::new(4);
    let mine = || {
        let NewSessionRows { pending, .. } = new_rows(&library, id);
        pending.lock().unwrap().clone().unwrap()
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
    let after = segments(&library, old.id);
    assert!(as_m1_kept(&rows).iter().all(|r| after.contains(r)));
}

/// `rows` as an M1 per-session store keeps them: without audio digests.
fn as_m1_kept(rows: &[SegmentRow]) -> Vec<SegmentRow> {
    rows.iter()
        .map(|r| SegmentRow::new(r.track(), r.epoch(), r.range(), r.sha256()).unwrap())
        .collect()
}

/// A row of `samples` samples on `track`, from `start`.
fn listed_row(track: u32, start: u64, samples: u64) -> SegmentRow {
    let range =
        nota_core::SampleRange::new(SampleIndex::new(start), SampleIndex::new(start + samples))
            .unwrap();
    SegmentRow::new(
        TrackId::new(track),
        EpochId::new(0),
        range,
        nota_store::Sha256Digest::new([7; 32]),
    )
    .unwrap()
}

/// Leaves a journal in `session`'s audio directory, as a stop that
/// couldn't publish everything does.
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding outside the recorder's write path"
)]
fn leave_journal(session: &SessionPaths) {
    let name = JournalId::new(1).file_name();
    std::fs::write(session.audio().join(name), b"").unwrap();
}

#[test]
fn listing_gives_each_session_s_title_audio_and_needs() {
    let tmp = TestDir::new("listing");
    let library = Library::open(&tmp.0).unwrap();
    let rate = SampleRate::SPEECH;
    let hz = u64::from(rate.hz());
    let (one, two, three) = (
        library.create().unwrap(),
        library.create().unwrap(),
        library.create().unwrap(),
    );
    library
        .db()
        .with(|db| {
            for (paths, title, started) in [
                (&one, "Joinery", Some(1_760_000_000)),
                (&two, "Turning", None),
            ] {
                db.create_session(&NewSession {
                    id: paths.id,
                    title: Some(title.into()),
                    language: None,
                    started_at: started.and_then(WallTime::from_unix_seconds),
                    tracks: Vec::new(),
                })?;
            }
            // The mic has 90 s in two segments, the system audio 30 s:
            // the session is as long as its longest track.
            db.insert_segment(one.id, &listed_row(0, 0, 60 * hz))?;
            db.insert_segment(one.id, &listed_row(0, 60 * hz, 30 * hz))?;
            db.insert_segment(one.id, &listed_row(1, 0, 30 * hz))?;
            Ok(())
        })
        .unwrap();
    // Two has a journal left; three isn't in the database and has one too.
    leave_journal(&two);
    leave_journal(&three);

    let listed = library.listing(rate).unwrap();
    let attention = |s: &str| Needs::Attention(s.to_owned());
    let left = "audio still to save · nota tries again when it starts";
    assert_eq!(
        listed,
        [
            Listed {
                id: one.id,
                title: Some("Joinery".into()),
                started_at: WallTime::from_unix_seconds(1_760_000_000),
                recorded: Some(Duration::from_secs(90)),
                needs: Needs::Nothing,
            },
            Listed {
                id: two.id,
                title: Some("Turning".into()),
                started_at: None,
                recorded: None,
                needs: attention(left),
            },
            Listed {
                id: three.id,
                title: None,
                started_at: None,
                recorded: None,
                needs: attention(left),
            },
        ]
    );

    // A session another nota is recording isn't a problem.
    let lock = SessionDir::new(two.id, StdFs, &two.audio()).lock().unwrap();
    let listed = library.listing(rate).unwrap();
    assert_eq!(listed[1].needs, Needs::InUse);
    drop(lock);
}

#[test]
fn an_empty_session_directory_isnt_listed() {
    let tmp = TestDir::new("listing-empty");
    let library = Library::open(&tmp.0).unwrap();
    library.create().unwrap();
    assert_eq!(library.listing(SampleRate::SPEECH).unwrap(), []);
}

/// With the database unreadable, every session on disk is still listed,
/// and says why it can't be shown properly.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding outside the recorder's write path"
)]
fn a_database_that_cant_be_read_is_said_on_each_session() {
    let tmp = TestDir::new("listing-no-db");
    let library = Library::open(&tmp.0).unwrap();
    // Made by hand: making it through the library would open the database.
    let session = tmp.0.join(SESSIONS).join("1");
    std::fs::create_dir_all(session.join(AUDIO)).unwrap();
    std::fs::write(session.join("note"), b"x").unwrap();
    // A directory where the database should be.
    std::fs::create_dir(tmp.0.join(LIBRARY_DB)).unwrap();
    let listed = library.listing(SampleRate::SPEECH).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, SessionId::new(1));
    let Needs::Attention(why) = &listed[0].needs else {
        panic!("{listed:?}");
    };
    assert!(
        why.starts_with("the library database can't be read"),
        "{why}"
    );
}

#[test]
fn the_longest_track_sets_the_length() {
    let rate = SampleRate::SPEECH;
    assert_eq!(longest_track(&[], rate), None);
    let rows = [listed_row(1, 0, 16_000), listed_row(0, 0, 8_000)];
    assert_eq!(longest_track(&rows, rate), Some(Duration::from_secs(1)));
}

/// A journal salvage set aside as damaged needs the user: what it couldn't
/// read is still there. Other files named like it don't count.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding outside the recorder's write path"
)]
fn a_journal_set_aside_as_damaged_needs_you() {
    let tmp = TestDir::new("listing-set-aside");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    let journal = JournalId::new(3).file_name();
    std::fs::write(session.audio().join("salvage-findings.unreadable"), b"").unwrap();
    std::fs::write(session.audio().join("notes.unreadable"), b"").unwrap();
    let listed = library.listing(SampleRate::SPEECH).unwrap();
    assert_eq!(listed[0].needs, Needs::Nothing);
    std::fs::write(session.audio().join(format!("{journal}.unreadable")), b"x").unwrap();
    let listed = library.listing(SampleRate::SPEECH).unwrap();
    assert_eq!(
        listed[0].needs,
        Needs::Attention(
            "1 damaged journal set aside · audio salvage couldn't read is kept".to_owned()
        )
    );
}

/// Acceptance (GAI-335): a session recorded with the library database
/// down from start to stop is adopted at the next start with the title and
/// tracks kept in its directory, and its audio published.
#[test]
fn a_session_recorded_without_the_database_keeps_its_title_and_tracks() {
    let tmp = TestDir::new("kept");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    drop(library);
    break_db(&tmp.0);
    let library = Library::open(&tmp.0).unwrap();
    let rows = new_rows(&library, session.id);
    let row = NewSession {
        id: session.id,
        title: Some("lecture 1".to_owned()),
        language: Some("en".to_owned()),
        started_at: WallTime::from_unix_seconds(1_760_004_000),
        tracks: vec![Track {
            track: MIC,
            kind: TrackKind::Microphone,
            source: Some("mic".to_owned()),
        }],
    };
    session.keep(&row).unwrap();
    // A session holding only its kept row, or one partly written, is left
    // alone, as one just made.
    assert!(is_empty(&session));
    drop(
        StdFs
            .create(&session.dir.join("session.txt.partial"))
            .unwrap(),
    );
    assert!(is_empty(&session));
    StdFs
        .remove(&session.dir.join("session.txt.partial"))
        .unwrap();
    let (lock, finished) = record(&session, 3_000);
    let mut bound = SessionStore::new(lock, rows);
    assert!(publish_journals(&mut bound, length(), &finished).is_err());
    drop(bound);

    // The next start, with the database back.
    mend_db(&tmp.0);
    let library = Library::open(&tmp.0).unwrap();
    let salvaged = library.salvage_all(length()).unwrap();
    assert!(
        matches!(salvaged.as_slice(), [Salvaged::Done(id, aside)] if *id == session.id && aside.is_empty()),
        "{salvaged:?}"
    );
    let adopted = library
        .db()
        .with(|db| db.session(session.id))
        .unwrap()
        .unwrap();
    assert_eq!(adopted.title.as_deref(), Some("lecture 1"));
    assert_eq!(adopted.language.as_deref(), Some("en"));
    assert_eq!(adopted.started_at, row.started_at);
    assert_eq!(adopted.state, SessionState::Stopped);
    assert_eq!(
        library.db().with(|db| db.tracks(session.id)).unwrap(),
        row.tracks
    );
    assert_eq!(covered(&segments(&library, session.id)), [(0, 3_000)]);
}

/// A kept row that doesn't parse doesn't hold up the session's audio: it's
/// adopted with its number alone, as before nota kept one.
#[test]
fn a_kept_row_that_cant_be_read_is_left_out() {
    let tmp = TestDir::new("kept-bad");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    let mut file = StdFs.create(&session.dir.join("session.txt")).unwrap();
    file.write_all(b"nota session 99\ntitle later\n").unwrap();
    drop(file);
    drop(record(&session, 3_000));
    assert!(matches!(
        library.salvage_all(length()).unwrap().as_slice(),
        [Salvaged::Done(..)]
    ));
    let adopted = library
        .db()
        .with(|db| db.session(session.id))
        .unwrap()
        .unwrap();
    assert_eq!(adopted.title, None);
    assert_eq!(library.db().with(|db| db.tracks(session.id)).unwrap(), []);
    assert_eq!(covered(&segments(&library, session.id)), [(0, 3_000)]);
}

/// A kept row that's there but can't be read yet holds the session back
/// for the next start, rather than adopting it without its title for good.
#[cfg(unix)]
#[test]
fn a_kept_row_that_cant_be_read_yet_is_read_at_a_later_start() {
    use std::os::unix::fs::PermissionsExt as _;
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding: a kept row that can't be read"
    )]
    fn set_mode(file: &Path, mode: u32) {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let tmp = TestDir::new("kept-unreadable");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    let row = NewSession {
        title: Some("lecture 1".to_owned()),
        ..NewSession::bare(session.id)
    };
    session.keep(&row).unwrap();
    drop(record(&session, 3_000));
    let file = session.dir.join("session.txt");
    set_mode(&file, 0o000);
    if StdFs.read(&file).is_ok() {
        // Run as root: permissions don't stop it.
        return;
    }
    let done = library.salvage_all(length()).unwrap();
    set_mode(&file, 0o600);
    assert!(
        matches!(&done[..], [Salvaged::Failed(id, why)] if *id == session.id && why.contains("title and tracks")),
        "{done:?}"
    );
    assert_eq!(
        library.db().with(|db| db.session(session.id)).unwrap(),
        None
    );
    let done = library.salvage_all(length()).unwrap();
    assert!(matches!(done[..], [Salvaged::Done(..)]), "{done:?}");
    let adopted = library
        .db()
        .with(|db| db.session(session.id))
        .unwrap()
        .unwrap();
    assert_eq!(adopted.title.as_deref(), Some("lecture 1"));
}

/// A row whose file goes, with no journals left to rebuild it, is found by
/// the next start's scan and shown until the file is back.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding outside the recorder's write path"
)]
fn a_row_whose_file_is_gone_needs_you_until_it_is_back() {
    use nota_recorder::segment::segment_file_name;
    use nota_store::{IndexedFinding, Problem, Status};

    let tmp = TestDir::new("gone");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    leave_a_journal(&session);
    library.salvage_all(length()).unwrap();
    let [row] = segments(&library, session.id)[..] else {
        panic!("one row expected");
    };
    let rate = SampleRate::SPEECH;
    let needs = |library: &Library| library.listing(rate).unwrap()[0].needs.clone();
    assert_eq!(needs(&library), Needs::Nothing);

    let path = session
        .audio()
        .join(segment_file_name(row.track(), row.range()));
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    // Nothing to salvage: the scan finds it.
    assert!(library.salvage_all(length()).unwrap().is_empty());
    assert_eq!(
        needs(&library),
        Needs::Attention("1 segment missing or changed · only its file brings it back".into())
    );
    let indexed = |library: &Library| library.db().with(|db| db.findings(session.id)).unwrap();
    assert_eq!(
        indexed(&library),
        [IndexedFinding::Row {
            row,
            problem: Problem::Missing,
            status: Status::Unresolved
        }]
    );
    // Found again at the next start: still one.
    library.salvage_all(length()).unwrap();
    assert_eq!(indexed(&library).len(), 1);

    std::fs::write(&path, bytes).unwrap();
    library.salvage_all(length()).unwrap();
    assert_eq!(needs(&library), Needs::Nothing);
    assert_eq!(
        indexed(&library),
        [IndexedFinding::Row {
            row,
            problem: Problem::Missing,
            status: Status::SinceVerified
        }]
    );
}

#[test]
fn new_rows_name_a_row_that_doesnt_parse() {
    let key = nota_store::RowKey {
        track: 2,
        start: -1,
    };
    let corrupt = StoreError::CorruptRow {
        session: SessionId::new(1),
        key,
        why: "odd".into(),
    };
    assert_eq!(
        <NewSessionRows as SegmentStore>::unparsable_row(&corrupt),
        Some(key)
    );
    assert_eq!(
        <NewSessionRows as SegmentStore>::unparsable_row(&StoreError::OutOfRange),
        None
    );
}

/// A session whose audio directory was emptied (every file lost) is still
/// checked at start: the database holds its rows.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding outside the recorder's write path"
)]
fn a_session_whose_files_are_all_gone_needs_you() {
    let tmp = TestDir::new("all-gone");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    leave_a_journal(&session);
    library.salvage_all(length()).unwrap();
    assert_eq!(segments(&library, session.id).len(), 1);
    for path in StdFs.list(&session.audio()).unwrap() {
        std::fs::remove_file(path).unwrap();
    }
    for path in StdFs.list(&session.dir).unwrap() {
        if path != session.audio() {
            std::fs::remove_file(path).unwrap();
        }
    }
    library.salvage_all(length()).unwrap();
    let listed = library.listing(SampleRate::SPEECH).unwrap();
    assert_eq!(
        listed[0].needs,
        Needs::Attention("1 segment missing or changed · only its file brings it back".into())
    );
}

/// One session's row that doesn't parse doesn't stop the listing: that
/// session is listed without its length, and every other as usual.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "plants a row the schema's checks refuse, past nota-store, as a damaged database could hold"
)]
fn a_row_that_doesnt_parse_is_one_sessions_problem_in_the_listing() {
    let tmp = TestDir::new("unparsable-listing");
    let library = Library::open(&tmp.0).unwrap();
    let (one, two) = (library.create().unwrap(), library.create().unwrap());
    let hz = u64::from(SampleRate::SPEECH.hz());
    library
        .db()
        .with(|db| {
            db.create_session(&NewSession::bare(one.id))?;
            db.create_session(&NewSession::bare(two.id))?;
            db.insert_segment(two.id, &listed_row(0, 0, 60 * hz))
        })
        .unwrap();
    let raw = rusqlite::Connection::open(library.db_path()).unwrap();
    raw.execute_batch("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    raw.execute(
        "INSERT INTO segment (session_id, track, epoch, start_sample, end_sample, sha256) \
         VALUES (?1, 0, 0, 0, 10, zeroblob(3))",
        [i64::try_from(one.id.get()).unwrap()],
    )
    .unwrap();
    let listed = library.listing(SampleRate::SPEECH).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].recorded, None);
    assert_eq!(listed[1].recorded, Some(Duration::from_secs(60)));
    assert_eq!(listed[1].needs, Needs::Nothing);
}

/// Home reads the kept title and date when the session has no database row.
#[test]
fn listing_uses_kept_metadata_without_a_database_row() {
    let tmp = TestDir::new("listing-kept-no-row");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    let kept = NewSession {
        title: Some("Woodland ecology".into()),
        started_at: WallTime::from_unix_seconds(1_760_004_000),
        ..NewSession::bare(session.id)
    };
    session.keep(&kept).unwrap();
    leave_journal(&session);
    assert_eq!(
        library.db().with(|db| db.session(session.id)).unwrap(),
        None
    );

    let listed = library.listing(SampleRate::SPEECH).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, session.id);
    assert_eq!(listed[0].title, kept.title);
    assert_eq!(listed[0].started_at, kept.started_at);
    assert_eq!(listed[0].recorded, None);
    assert!(matches!(listed[0].needs, Needs::Attention(_)));
    // Reading the fallback must not turn the listing into adoption.
    assert_eq!(
        library.db().with(|db| db.session(session.id)).unwrap(),
        None
    );
    assert_eq!(
        kept::read(&StdFs, &session.dir, session.id).unwrap(),
        Some(kept)
    );
}

/// Home reads the kept title and date when the database cannot be opened.
#[test]
fn listing_uses_kept_metadata_when_the_database_cant_be_read() {
    let tmp = TestDir::new("listing-kept-bad-db");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    let kept = NewSession {
        title: Some("River habitats".into()),
        started_at: WallTime::from_unix_seconds(1_760_004_001),
        ..NewSession::bare(session.id)
    };
    session.keep(&kept).unwrap();
    leave_journal(&session);
    drop(library);
    // Break the database so this proves fallback after a read failure.
    break_db(&tmp.0);
    let library = Library::open(&tmp.0).unwrap();

    let listed = library.listing(SampleRate::SPEECH).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, session.id);
    assert_eq!(listed[0].title, kept.title);
    assert_eq!(listed[0].started_at, kept.started_at);
    assert_eq!(listed[0].recorded, None);
    let Needs::Attention(why) = &listed[0].needs else {
        panic!("{listed:?}");
    };
    assert!(why.starts_with("audio still to save"), "{why}");
}

/// A database row takes precedence even when its title and date are unknown.
#[test]
fn listing_database_metadata_takes_precedence_over_kept_metadata() {
    let tmp = TestDir::new("listing-kept-precedence");
    let library = Library::open(&tmp.0).unwrap();
    // A later row must not resurrect an older title, including when it clears one.
    for (title, started_at) in [
        (
            Some("Edited title".to_owned()),
            WallTime::from_unix_seconds(1_760_008_000),
        ),
        (None, None),
    ] {
        let session = library.create().unwrap();
        session
            .keep(&NewSession {
                title: Some("Original title".into()),
                started_at: WallTime::from_unix_seconds(1_760_004_000),
                ..NewSession::bare(session.id)
            })
            .unwrap();
        library
            .db()
            .with(|db| {
                db.create_session(&NewSession {
                    title: title.clone(),
                    started_at,
                    ..NewSession::bare(session.id)
                })
            })
            .unwrap();
        let listed = library.listing(SampleRate::SPEECH).unwrap();
        let row = listed.iter().find(|row| row.id == session.id).unwrap();
        assert_eq!(row.title, title);
        assert_eq!(row.started_at, started_at);
        assert_eq!(row.recorded, None);
        assert_eq!(row.needs, Needs::Nothing);
    }
}

/// A session without either source of metadata keeps its unknown title and date.
#[test]
fn listing_without_kept_metadata_still_has_unknown_title_and_start() {
    let tmp = TestDir::new("listing-no-kept");
    let library = Library::open(&tmp.0).unwrap();
    let session = library.create().unwrap();
    leave_journal(&session);
    // An unreadable database must not invent metadata absent from the session.
    for library in [Some(library), None] {
        let library = library.unwrap_or_else(|| {
            break_db(&tmp.0);
            Library::open(&tmp.0).unwrap()
        });
        let listed = library.listing(SampleRate::SPEECH).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, session.id);
        assert_eq!(listed[0].title, None);
        assert_eq!(listed[0].started_at, None);
        assert_eq!(listed[0].recorded, None);
        assert!(matches!(listed[0].needs, Needs::Attention(_)));
    }
}

/// A second start mustn't spend the ballast of a live recording, even
/// before that recording has made its first journal.
#[test]
fn startup_keeps_a_live_recordings_ballast() {
    use nota_recorder::disk::{Ballast, Freed};
    use nota_recorder::fs::fake::FakeFs;
    let root = PathBuf::from("/data");
    let audio = root.join("sessions/1/audio");
    let fs = FakeFs::with_dirs([audio.clone()]);
    Ballast::keep(&fs, &root, 1_024, || false).unwrap();
    let _recording = fs.lock_dir(&audio).unwrap();
    // Home recovery takes no reserve from a session still being started.
    let watch = startup_watch(&fs, &root, 1_024).unwrap();
    fs.set_capacity(Some(1_024));
    let mut output = watch.fs().create(&root.join("old-segment.tmp")).unwrap();
    assert_eq!(
        output.write_all(&[7; 20]).unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(watch.full().map(|f| f.ballast), Some(Freed::None));
    assert!(Ballast::find(&fs, &root, 1_024).unwrap().is_some());
}

/// Once another recording releases its lock, recovery can use the ballast.
#[test]
fn startup_can_use_ballast_after_the_live_recording_stops() {
    use nota_recorder::disk::Ballast;
    use nota_recorder::fs::fake::FakeFs;
    let root = PathBuf::from("/data");
    let audio = root.join("sessions/1/audio");
    let fs = FakeFs::with_dirs([audio.clone()]);
    Ballast::keep(&fs, &root, 1_024, || false).unwrap();
    let recording = fs.lock_dir(&audio).unwrap();
    assert!(!startup_watch(&fs, &root, 1_024).unwrap().holds_ballast());
    // Releasing the session gives recovery its finishing room back.
    drop(recording);
    assert!(startup_watch(&fs, &root, 1_024).unwrap().holds_ballast());
}

/// A recording that starts between recovery's steps, after recovery took
/// the ballast and before it met the full disk, keeps the ballast.
#[test]
fn a_recording_started_during_recovery_keeps_its_ballast() {
    use nota_recorder::disk::{Ballast, Freed};
    use nota_recorder::fs::fake::FakeFs;
    let root = PathBuf::from("/data");
    let audio = root.join("sessions/2/audio");
    let fs = FakeFs::with_dirs([root.join("sessions/1/audio"), audio.clone()]);
    Ballast::keep(&fs, &root, 1_024, || false).unwrap();
    let watch = startup_watch(&fs, &root, 1_024).unwrap();
    assert!(watch.holds_ballast());
    // Another nota starts recording while this one salvages.
    let _recording = fs.lock_dir(&audio).unwrap();
    fs.set_capacity(Some(1_024));
    let mut output = watch.fs().create(&root.join("old-segment.tmp")).unwrap();
    assert_eq!(
        output.write_all(&[7; 20]).unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(watch.full().map(|f| f.ballast), Some(Freed::None));
    assert!(Ballast::find(&fs, &root, 1_024).unwrap().is_some());
}

/// The start's own watch, once its recording starts, frees the ballast
/// for that recording, though its own session is locked by then.
#[test]
fn a_started_recording_frees_the_ballast_startup_held() {
    use nota_recorder::disk::{Ballast, Freed};
    use nota_recorder::fs::fake::FakeFs;
    let root = PathBuf::from("/data");
    let audio = root.join("sessions/1/audio");
    let fs = FakeFs::with_dirs([audio.clone()]);
    Ballast::keep(&fs, &root, 1_024, || false).unwrap();
    let watch = startup_watch(&fs, &root, 1_024).unwrap();
    // The start locks its own session, then starts recording.
    let _own = fs.lock_dir(&audio).unwrap();
    watch.start_recording();
    fs.set_capacity(Some(1_024));
    let mut journal = watch.fs().create(&audio.join("journal-000000")).unwrap();
    assert_eq!(
        journal.write_all(&[7; 20]).unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(watch.full().map(|f| f.ballast), Some(Freed::Freed));
    assert!(Ballast::find(&fs, &root, 1_024).unwrap().is_none());
}

/// Salvage locks the session it salvages through the startup watch: that
/// lock is startup's own, so a full disk still frees the ballast.
#[test]
fn startup_frees_the_ballast_while_salvage_holds_its_session() {
    use nota_recorder::disk::{Ballast, Freed};
    use nota_recorder::fs::fake::FakeFs;
    let root = PathBuf::from("/data");
    let audio = root.join("sessions/1/audio");
    let fs = FakeFs::with_dirs([audio.clone()]);
    Ballast::keep(&fs, &root, 1_024, || false).unwrap();
    let watch = startup_watch(&fs, &root, 1_024).unwrap();
    // As `salvage_one` does, before it writes anything.
    let _salvaging = watch.fs().lock_dir(&audio).unwrap();
    fs.set_capacity(Some(1_024));
    let mut output = watch.fs().create(&audio.join("seg.tmp")).unwrap();
    assert_eq!(
        output.write_all(&[7; 20]).unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(watch.full().map(|f| f.ballast), Some(Freed::Freed));
    assert!(Ballast::find(&fs, &root, 1_024).unwrap().is_none());
}

/// Once salvage lets a session go, a recording that locks it next claims
/// the ballast: the watch counts only the locks it still holds as its own.
#[test]
fn a_session_salvage_let_go_is_claimed_by_the_recording_that_takes_it() {
    use nota_recorder::disk::{Ballast, Freed};
    use nota_recorder::fs::fake::FakeFs;
    let root = PathBuf::from("/data");
    let audio = root.join("sessions/1/audio");
    let fs = FakeFs::with_dirs([audio.clone()]);
    Ballast::keep(&fs, &root, 1_024, || false).unwrap();
    let watch = startup_watch(&fs, &root, 1_024).unwrap();
    drop(watch.fs().lock_dir(&audio).unwrap());
    // Another nota resumes recording the session salvage just finished.
    let _resumed = fs.lock_dir(&audio).unwrap();
    watch.note_full(None);
    assert_eq!(watch.full().map(|f| f.ballast), Some(Freed::None));
    assert!(Ballast::find(&fs, &root, 1_024).unwrap().is_some());
}

/// If the sessions can't be listed when the disk fills, one of them may
/// be recording: the ballast is kept.
#[test]
fn startup_keeps_the_ballast_when_it_cant_list_the_sessions() {
    use nota_recorder::disk::{Ballast, Freed};
    use nota_recorder::fs::fake::FakeFs;
    let root = PathBuf::from("/data");
    let sessions = root.join("sessions");
    let fs = FakeFs::with_dirs([sessions.clone()]);
    Ballast::keep(&fs, &root, 1_024, || false).unwrap();
    let watch = startup_watch(&fs, &root, 1_024).unwrap();
    // Gone by the time the disk fills: listing it fails.
    fs.remove_dir(&sessions).unwrap();
    watch.note_full(None);
    assert_eq!(watch.full().map(|f| f.ballast), Some(Freed::None));
    assert!(Ballast::find(&fs, &root, 1_024).unwrap().is_some());
}
