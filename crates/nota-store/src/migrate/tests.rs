use std::path::Path;

use nota_core::SessionId;
use rusqlite::params;

use super::*;
use crate::test_dir::TestDir;
use crate::tests::{new_session, raw, row};
use crate::{SegmentRow, SessionState};

/// A per-session store at `path`, as M1 made one, holding `rows`.
fn per_session_store(path: &Path, rows: &[SegmentRow]) {
    let conn = raw(path);
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    conn.execute_batch(schema::V1).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    for r in rows {
        conn.execute(
            "INSERT INTO segment VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
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
fn a_new_file_gets_the_current_schema() {
    let dir = TestDir::new("fresh");
    let store = Store::open(&dir.db()).unwrap();
    assert_eq!(store.pragma_text("user_version"), VERSION.to_string());
    assert_eq!(STEPS.len(), 3);
    assert_eq!(STEPS.len(), usize::try_from(VERSION - FIRST + 1).unwrap());
    // Opening again changes nothing.
    drop(store);
    let again = Store::open(&dir.db()).unwrap();
    assert_eq!(again.pragma_text("user_version"), VERSION.to_string());
}

#[test]
fn unknown_versions_are_refused_and_left_alone() {
    for version in [VERSION + 1, 7, -1] {
        let dir = TestDir::new(&format!("version{version}"));
        raw(&dir.db())
            .pragma_update(None, "user_version", version)
            .unwrap();
        assert!(
            matches!(Store::open(&dir.db()), Err(StoreError::UnknownSchema(v)) if v == version),
            "{version}"
        );
        let found: i64 = raw(&dir.db())
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(found, version);
        // Not even switched to a write-ahead log.
        let mode: String = raw(&dir.db())
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }
}

#[test]
fn a_file_with_tables_but_no_version_is_refused() {
    let dir = TestDir::new("foreign");
    raw(&dir.db())
        .execute_batch("CREATE TABLE notes (body TEXT)")
        .unwrap();
    assert!(matches!(
        Store::open(&dir.db()),
        Err(StoreError::UnknownSchema(0))
    ));
}

#[test]
fn a_per_session_store_is_not_opened_as_the_library() {
    let dir = TestDir::new("notlibrary");
    let path = dir.0.join("nota.db");
    per_session_store(&path, &[row(0, 0, 0, 480, 1)]);
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::PerSessionStore)
    ));
    // And it's left as it was.
    assert_eq!(read_per_session(&path).unwrap(), [row(0, 0, 0, 480, 1)]);
}

#[test]
fn adopting_imports_a_per_session_store_once() {
    let dir = TestDir::new("import");
    let old = dir.0.join("nota.db");
    let rows = [
        row(0, 0, 0, 480, 1),
        row(0, 1, 480, 960, 2),
        row(1, 0, 0, 480, 3),
    ];
    per_session_store(&old, &rows);
    let mut store = Store::open(&dir.db()).unwrap();
    let three = SessionId::new(3);
    assert_eq!(
        store.adopt_session(three, Some(&old)).unwrap(),
        Adopted::Imported(3)
    );
    assert_eq!(store.segments(three).unwrap(), rows);
    let session = store.session(three).unwrap().unwrap();
    assert_eq!(session.state, SessionState::Stopped);
    assert_eq!(session.title, None);

    // Adopted once: a later change to the old store isn't read.
    raw(&old).execute("DELETE FROM segment", []).unwrap();
    assert_eq!(
        store.adopt_session(three, Some(&old)).unwrap(),
        Adopted::Known
    );
    assert_eq!(store.segments(three).unwrap(), rows);
}

#[test]
fn adopting_a_session_without_a_store_adds_it_empty() {
    let dir = TestDir::new("nostore");
    let mut store = Store::open(&dir.db()).unwrap();
    let id = SessionId::new(4);
    assert_eq!(store.adopt_session(id, None).unwrap(), Adopted::Added);
    assert_eq!(store.segments(id).unwrap(), vec![]);
    assert_eq!(store.adopt_session(id, None).unwrap(), Adopted::Known);
    // A session made by `nota record` is known too.
    store
        .create_session(&new_session(SessionId::new(5)))
        .unwrap();
    assert_eq!(
        store.adopt_session(SessionId::new(5), None).unwrap(),
        Adopted::Known
    );
}

#[test]
fn a_store_created_but_never_given_its_schema_imports_nothing() {
    let dir = TestDir::new("empty");
    let old = dir.0.join("nota.db");
    drop(raw(&old));
    let mut store = Store::open(&dir.db()).unwrap();
    assert_eq!(
        store.adopt_session(SessionId::new(1), Some(&old)).unwrap(),
        Adopted::Imported(0)
    );
}

#[test]
fn a_bad_per_session_store_adopts_nothing() {
    let dir = TestDir::new("bad");
    let mut store = Store::open(&dir.db()).unwrap();
    let id = SessionId::new(6);

    let wrong_version = dir.0.join("v2.db");
    per_session_store(&wrong_version, &[]);
    raw(&wrong_version)
        .pragma_update(None, "user_version", 2)
        .unwrap();
    assert!(matches!(
        store.adopt_session(id, Some(&wrong_version)),
        Err(StoreError::UnknownSchema(2))
    ));

    let corrupt = dir.0.join("corrupt.db");
    per_session_store(&corrupt, &[row(0, 0, 0, 480, 1)]);
    raw(&corrupt)
        .execute("UPDATE segment SET epoch = -3", [])
        .unwrap();
    assert!(matches!(
        store.adopt_session(id, Some(&corrupt)),
        Err(StoreError::Corrupt(m)) if m.contains("epoch")
    ));

    #[cfg(unix)]
    {
        let link = dir.0.join("link.db");
        #[expect(
            clippy::disallowed_methods,
            reason = "test scaffolding: a symlinked store"
        )]
        std::os::unix::fs::symlink(&wrong_version, &link).unwrap();
        assert!(matches!(
            store.adopt_session(id, Some(&link)),
            Err(StoreError::Sqlite(_))
        ));
    }

    let missing = dir.0.join("missing.db");
    assert!(matches!(
        store.adopt_session(id, Some(&missing)),
        Err(StoreError::Sqlite(_))
    ));
    // Opening it didn't create it.
    assert!(std::fs::metadata(&missing).is_err());

    // Nothing was added, so a later try can still import it.
    assert_eq!(store.session(id).unwrap(), None);
    assert_eq!(store.segments(id).unwrap(), vec![]);
}

#[test]
fn imported_sessions_with_the_same_coordinates_coexist() {
    let dir = TestDir::new("twosessions");
    let rows = [row(0, 0, 0, 480, 1), row(1, 0, 0, 480, 2)];
    let other = [row(0, 0, 0, 480, 7), row(1, 0, 0, 480, 8)];
    let (a, b) = (dir.0.join("a.db"), dir.0.join("b.db"));
    per_session_store(&a, &rows);
    per_session_store(&b, &other);
    let mut store = Store::open(&dir.db()).unwrap();
    store.adopt_session(SessionId::new(1), Some(&a)).unwrap();
    store.adopt_session(SessionId::new(2), Some(&b)).unwrap();
    assert_eq!(store.segments(SessionId::new(1)).unwrap(), rows);
    assert_eq!(store.segments(SessionId::new(2)).unwrap(), other);
}

/// A version 2 library, as M2's first schema made it, with one session.
fn version_2(path: &Path) {
    let conn = raw(path);
    conn.execute_batch(schema::V2).unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    conn.execute(
        "INSERT INTO session (id, title, language, state) VALUES (1, 'old', NULL, 'stopped')",
        [],
    )
    .unwrap();
}

/// A version 2 library gains `started_at`, null on the sessions it had,
/// and keeps them; new sessions get their start time.
#[test]
fn version_2_upgrades_to_3_keeping_its_sessions() {
    let dir = TestDir::new("v2");
    version_2(&dir.db());
    let mut store = Store::open(&dir.db()).unwrap();
    assert_eq!(store.pragma_text("user_version"), VERSION.to_string());
    let old = store.session(SessionId::new(1)).unwrap().unwrap();
    assert_eq!(old.title.as_deref(), Some("old"));
    assert_eq!(old.state, SessionState::Stopped);
    assert_eq!(old.started_at, None);
    let new = new_session(SessionId::new(2));
    store.create_session(&new).unwrap();
    let read = store.session(SessionId::new(2)).unwrap().unwrap();
    assert!(read.started_at.is_some());
    assert_eq!(read.started_at, new.started_at);
}

/// An upgrade that fails partway changes nothing: the steps that ran are
/// rolled back with it, and the version stays as it was.
#[test]
fn a_failed_upgrade_changes_nothing() {
    // From an empty file: version 2's step runs, then the next fails.
    let dir = TestDir::new("fails-after-a-step");
    let mut conn = raw(&dir.db());
    let failing = [
        schema::V2,
        "ALTER TABLE no_such_table ADD COLUMN x INTEGER;",
    ];
    assert!(matches!(
        upgrade_with(&mut conn, &failing),
        Err(StoreError::Sqlite(_))
    ));
    let tables: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tables, 0, "version 2's tables survived the failed upgrade");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 0);

    // From version 2: the real step fails, and the file keeps what it held.
    let dir = TestDir::new("v2-fails");
    version_2(&dir.db());
    // A column of that name already there makes the V3 step fail.
    raw(&dir.db())
        .execute_batch("ALTER TABLE session ADD COLUMN started_at TEXT;")
        .unwrap();
    assert!(matches!(Store::open(&dir.db()), Err(StoreError::Sqlite(_))));
    let conn = raw(&dir.db());
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let title: String = conn
        .query_row("SELECT title FROM session WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "old");
}

/// A start time before 1970 can't be stored, and one that's there anyway
/// (written by something else) is refused on reading, not misread.
#[test]
fn a_start_before_1970_is_refused() {
    let dir = TestDir::new("v3-negative");
    let mut store = Store::open(&dir.db()).unwrap();
    store
        .create_session(&new_session(SessionId::new(1)))
        .unwrap();
    let conn = raw(&dir.db());
    assert!(
        conn.execute("UPDATE session SET started_at = -1 WHERE id = 1", [])
            .is_err()
    );
    conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    conn.execute("UPDATE session SET started_at = -1 WHERE id = 1", [])
        .unwrap();
    drop(conn);
    assert!(matches!(
        store.session(SessionId::new(1)),
        Err(StoreError::Corrupt(_))
    ));
}

/// A version 3 library gains the triggers that keep heard text
/// append-only, and keeps the text it had.
#[test]
fn version_3_upgrades_to_4_and_its_heard_text_becomes_append_only() {
    let dir = TestDir::new("v3");
    {
        let conn = raw(&dir.db());
        conn.execute_batch(schema::V2).unwrap();
        conn.execute_batch(schema::V3).unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute_batch(
            "INSERT INTO session (id, state) VALUES (1, 'stopped');
             INSERT INTO utterance (id, session_id, track, start_ns, end_ns, text, engine, model)
             VALUES (1, 1, 0, 0, 10, 'heard before', 'e', 'm');
             UPDATE utterance SET text = 'still changeable at 3' WHERE id = 1;",
        )
        .unwrap();
    }
    let store = Store::open(&dir.db()).unwrap();
    assert_eq!(store.pragma_text("user_version"), "4");
    let heard = store.utterances(SessionId::new(1)).unwrap();
    assert_eq!(heard[0].heard.utterance.text(), "still changeable at 3");
    drop(store);
    assert!(
        raw(&dir.db())
            .execute("UPDATE utterance SET text = 'changed' WHERE id = 1", [])
            .is_err()
    );
}
