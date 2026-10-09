use std::path::Path;

use nota_core::{EpochId, SampleIndex, SampleRange, SessionId, TrackId};
use rusqlite::{Connection, params};

use super::test_dir::TestDir;
use super::*;

pub(crate) fn row(track: u32, epoch: u32, start: u64, end: u64, hash: u8) -> SegmentRow {
    SegmentRow::new(
        TrackId::new(track),
        EpochId::new(epoch),
        SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap(),
        Sha256Digest::new([hash; 32]),
    )
    .unwrap()
}

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);

pub(crate) fn new_session(id: SessionId) -> NewSession {
    NewSession {
        id,
        title: Some(format!("session {}", id.get())),
        language: Some("en".to_owned()),
        tracks: vec![
            Track {
                track: TrackId::new(0),
                kind: TrackKind::Microphone,
                source: Some("Built-in microphone".to_owned()),
            },
            Track {
                track: TrackId::new(1),
                kind: TrackKind::System,
                source: None,
            },
        ],
    }
}

/// A store with sessions 1 and 2 in it.
fn open(dir: &TestDir) -> Store {
    let mut store = Store::open(&dir.db()).unwrap();
    for id in [S1, S2] {
        if store.session(id).unwrap().is_none() {
            store.create_session(&new_session(id)).unwrap();
        }
    }
    store
}

#[test]
fn insert_then_read_and_persist() {
    let dir = TestDir::new("persist");
    let r = row(1, 0, 0, 480, 7);
    {
        let mut store = open(&dir);
        assert_eq!(store.insert_segment(S1, &r).unwrap(), Inserted::New);
        assert_eq!(store.segments(S1).unwrap(), vec![r]);
    }
    assert_eq!(open(&dir).segments(S1).unwrap(), vec![r]);
}

#[test]
fn pragmas_are_set() {
    let dir = TestDir::new("pragmas");
    let store = open(&dir);
    assert_eq!(store.pragma_text("journal_mode"), "wal");
    assert_eq!(store.pragma_text("synchronous"), "2");
    assert_eq!(store.pragma_text("foreign_keys"), "1");
    assert_eq!(store.pragma_text("busy_timeout"), "5000");
    assert_eq!(store.pragma_text("user_version"), "2");
}

#[test]
fn same_row_twice_is_stored_once() {
    let dir = TestDir::new("twice");
    let mut store = open(&dir);
    let r = row(1, 0, 0, 480, 7);
    assert_eq!(store.insert_segment(S1, &r).unwrap(), Inserted::New);
    assert_eq!(
        store.insert_segment(S1, &r).unwrap(),
        Inserted::AlreadyPresent
    );
    assert_eq!(store.segments(S1).unwrap(), vec![r]);
}

#[test]
fn different_row_at_same_start_conflicts() {
    let dir = TestDir::new("conflict");
    let mut store = open(&dir);
    let r = row(1, 0, 100, 580, 7);
    store.insert_segment(S1, &r).unwrap();
    for other in [
        row(1, 0, 100, 581, 7),
        row(1, 1, 100, 580, 7),
        row(1, 0, 100, 580, 8),
    ] {
        match store.insert_segment(S1, &other) {
            Err(StoreError::Conflict { existing }) => assert_eq!(existing, r),
            got => panic!("expected conflict, got {got:?}"),
        }
    }
    assert_eq!(store.segments(S1).unwrap(), vec![r]);
}

#[test]
fn overlapping_rows_of_a_track_conflict() {
    let dir = TestDir::new("overlap");
    let mut store = open(&dir);
    let r = row(1, 0, 100, 200, 7);
    store.insert_segment(S1, &r).unwrap();
    for other in [
        row(1, 0, 50, 101, 1),
        row(1, 0, 199, 300, 1),
        row(1, 0, 120, 130, 1),
        row(1, 0, 0, 1_000, 1),
    ] {
        match store.insert_segment(S1, &other) {
            Err(StoreError::Conflict { existing }) => assert_eq!(existing, r),
            got => panic!("expected conflict for {other:?}, got {got:?}"),
        }
    }
    // Touching is fine, and so is another track.
    for fine in [
        row(1, 0, 0, 100, 2),
        row(1, 0, 200, 300, 3),
        row(2, 0, 150, 160, 4),
    ] {
        assert_eq!(store.insert_segment(S1, &fine).unwrap(), Inserted::New);
    }
    assert_eq!(store.segments(S1).unwrap().len(), 4);
}

#[test]
fn sessions_with_the_same_coordinates_coexist() {
    let dir = TestDir::new("coexist");
    let mut store = open(&dir);
    let ours = [row(0, 0, 0, 480, 1), row(1, 0, 0, 480, 2)];
    let theirs = [row(0, 0, 0, 480, 3), row(1, 0, 0, 480, 4)];
    for r in &ours {
        assert_eq!(store.insert_segment(S1, r).unwrap(), Inserted::New);
    }
    // The same track and samples, another session: not a conflict, not
    // "already present", and not overlapping.
    for r in &theirs {
        assert_eq!(store.insert_segment(S2, r).unwrap(), Inserted::New);
    }
    assert_eq!(
        store
            .insert_segment(S2, &row(0, 0, 100, 200, 5))
            .unwrap_err()
            .to_string(),
        StoreError::Conflict {
            existing: theirs[0]
        }
        .to_string()
    );
    // Each session reads its own rows only, so one session's rows never
    // claim another's samples.
    assert_eq!(store.segments(S1).unwrap(), ours);
    assert_eq!(store.segments(S2).unwrap(), theirs);
    assert_eq!(store.segments(SessionId::new(3)).unwrap(), vec![]);
}

#[test]
fn a_row_for_a_session_not_in_the_library_is_refused() {
    let dir = TestDir::new("nosession");
    let mut store = open(&dir);
    let missing = SessionId::new(9);
    assert!(matches!(
        store.insert_segment(missing, &row(0, 0, 0, 10, 1)),
        Err(StoreError::NoSession(id)) if id == missing
    ));
    // And the foreign key refuses it below the API too.
    assert!(
        store
            .conn
            .execute(
                "INSERT INTO segment (session_id, track, epoch, start_sample, end_sample, sha256) \
                 VALUES (9, 0, 0, 0, 10, zeroblob(32))",
                [],
            )
            .is_err()
    );
    assert_eq!(store.segments(missing).unwrap(), vec![]);
}

#[test]
fn rows_are_ordered_by_track_then_start() {
    let dir = TestDir::new("order");
    let mut store = open(&dir);
    let rows = [
        row(2, 0, 0, 10, 1),
        row(1, 0, 20, 30, 2),
        row(1, 0, 0, 10, 3),
        row(2, 0, 10, 20, 4),
    ];
    for r in &rows {
        assert_eq!(store.insert_segment(S1, r).unwrap(), Inserted::New);
    }
    assert_eq!(
        store.segments(S1).unwrap(),
        vec![rows[2], rows[1], rows[0], rows[3]]
    );
}

#[test]
fn empty_range_is_not_a_row() {
    let s = SampleIndex::new(5);
    let empty = SampleRange::new(s, s).unwrap();
    assert!(
        SegmentRow::new(
            TrackId::new(0),
            EpochId::new(0),
            empty,
            Sha256Digest::new([0; 32])
        )
        .is_none()
    );
}

#[test]
fn numbers_beyond_i64_are_out_of_range() {
    let dir = TestDir::new("range");
    let mut store = open(&dir);
    let big = u64::try_from(i64::MAX).unwrap() + 1;
    assert!(matches!(
        store.insert_segment(S1, &row(1, 0, big, big + 5, 1)),
        Err(StoreError::OutOfRange)
    ));
    assert!(matches!(
        store.insert_segment(S1, &row(1, 0, 0, big, 1)),
        Err(StoreError::OutOfRange)
    ));
    assert!(matches!(
        store.insert_segment(SessionId::new(big), &row(1, 0, 0, 5, 1)),
        Err(StoreError::OutOfRange)
    ));
    assert!(matches!(
        store.segments(SessionId::new(big)),
        Err(StoreError::OutOfRange)
    ));
    assert_eq!(store.segments(S1).unwrap(), vec![]);
}

#[test]
fn sessions_and_tracks_read_back() {
    let dir = TestDir::new("sessions");
    let mut store = open(&dir);
    let one = new_session(S1);
    assert_eq!(
        store.session(S1).unwrap(),
        Some(Session {
            id: S1,
            title: one.title.clone(),
            language: one.language.clone(),
            state: SessionState::Recording,
        })
    );
    assert_eq!(store.tracks(S1).unwrap(), one.tracks);
    store.set_state(S1, SessionState::Stopped).unwrap();
    assert_eq!(
        store.session(S1).unwrap().unwrap().state,
        SessionState::Stopped
    );
    assert_eq!(
        store
            .sessions()
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect::<Vec<_>>(),
        [S1, S2]
    );
    assert!(matches!(
        store.create_session(&new_session(S1)),
        Err(StoreError::SessionExists(id)) if id == S1
    ));
    let missing = SessionId::new(7);
    assert_eq!(store.session(missing).unwrap(), None);
    assert!(matches!(
        store.set_state(missing, SessionState::Stopped),
        Err(StoreError::NoSession(id)) if id == missing
    ));
    // A session whose tracks repeat a number isn't added at all.
    let mut twice = new_session(SessionId::new(3));
    twice.tracks[1].track = TrackId::new(0);
    assert!(matches!(
        store.create_session(&twice),
        Err(StoreError::Sqlite(_))
    ));
    assert_eq!(store.session(SessionId::new(3)).unwrap(), None);
}

#[test]
fn unknown_states_and_kinds_are_corrupt() {
    let dir = TestDir::new("unknown");
    let store = open(&dir);
    store
        .conn
        .execute(
            "UPDATE track SET kind = 'loopback' WHERE session_id = 1",
            [],
        )
        .unwrap();
    store
        .conn
        .execute("UPDATE session SET state = 'paused' WHERE id = 2", [])
        .unwrap();
    assert!(matches!(store.tracks(S1), Err(StoreError::Corrupt(m)) if m.contains("loopback")));
    assert!(matches!(store.session(S2), Err(StoreError::Corrupt(m)) if m.contains("paused")));
    assert!(matches!(store.sessions(), Err(StoreError::Corrupt(_))));
}

#[cfg(unix)]
#[test]
fn database_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::new("mode");
    let mut store = open(&dir);
    store.insert_segment(S1, &row(1, 0, 0, 480, 7)).unwrap();
    for name in ["library.db", "library.db-wal", "library.db-shm"] {
        let mode = std::fs::metadata(dir.0.join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
}

#[cfg(unix)]
#[test]
fn an_unwritable_path_is_a_create_error() {
    let dir = TestDir::new("create");
    let path = dir.0.join("missing").join("library.db");
    assert!(matches!(Store::open(&path), Err(StoreError::Create(_))));
}

#[test]
fn corrupt_rows_are_reported() {
    let cases: [(i64, i64, &str); 4] = [
        (-1, 0, "track"),
        (1 << 33, 0, "track"),
        (1, -5, "epoch"),
        (1, 1 << 40, "epoch"),
    ];
    for (i, (track, epoch, what)) in cases.into_iter().enumerate() {
        let dir = TestDir::new(&format!("corrupt{i}"));
        let store = open(&dir);
        // Past the table's checks, as a damaged file could be.
        store
            .conn
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO segment (session_id, track, epoch, start_sample, end_sample, sha256) \
                 VALUES (1, ?1, ?2, 0, 10, zeroblob(32))",
                params![track, epoch],
            )
            .unwrap();
        match store.segments(S1) {
            Err(StoreError::Corrupt(msg)) => assert!(msg.contains(what), "{msg}"),
            got => panic!("expected corrupt, got {got:?}"),
        }
    }
}

#[test]
fn checks_stop_bad_rows() {
    let dir = TestDir::new("checks");
    let store = open(&dir);
    for values in [
        "(1, 1, 0, 10, 10, zeroblob(32))",
        "(1, 1, 0, -1, 10, zeroblob(32))",
        "(1, 1, 0, 0, 10, zeroblob(31))",
        "(1, -1, 0, 0, 10, zeroblob(32))",
        "(1, 4294967296, 0, 0, 10, zeroblob(32))",
        "(1, 1, -1, 0, 10, zeroblob(32))",
    ] {
        let sql = format!(
            "INSERT INTO segment (session_id, track, epoch, start_sample, end_sample, sha256) \
             VALUES {values}"
        );
        assert!(store.conn.execute(&sql, []).is_err(), "{sql}");
    }
}

#[test]
fn error_display_is_specific() {
    use std::error::Error as _;
    let existing = row(3, 0, 42, 50, 1);
    let cases: [(StoreError, &str); 10] = [
        (StoreError::Sqlite(rusqlite::Error::InvalidQuery), "sqlite"),
        (
            StoreError::Create(std::io::Error::other("disk on fire")),
            "create the database file: disk on fire",
        ),
        (
            StoreError::Pragma {
                name: "journal_mode",
                found: "delete".to_owned(),
            },
            "journal_mode",
        ),
        (StoreError::UnknownSchema(7), "7"),
        (StoreError::PerSessionStore, "per-session"),
        (StoreError::OutOfRange, "64-bit"),
        (StoreError::Conflict { existing }, "42"),
        (
            StoreError::NoSession(SessionId::new(31)),
            "session 31 is not",
        ),
        (
            StoreError::SessionExists(SessionId::new(32)),
            "session 32 is in",
        ),
        (StoreError::Corrupt("bad hash".to_owned()), "bad hash"),
    ];
    for (err, needle) in cases {
        let text = err.to_string();
        assert!(text.contains(needle), "{text}");
    }
    assert!(
        StoreError::Sqlite(rusqlite::Error::InvalidQuery)
            .source()
            .is_some()
    );
    assert!(
        StoreError::Create(std::io::Error::other("x"))
            .source()
            .is_some()
    );
    assert!(StoreError::OutOfRange.source().is_none());
}

/// A connection straight to the file, for tests that set a database up by
/// hand.
pub(crate) fn raw(path: &Path) -> Connection {
    Connection::open(path).unwrap()
}

#[test]
fn stopping_a_recording_session_writes_once() {
    let dir = TestDir::new("stop");
    let mut store = open(&dir);
    assert!(store.stop_recording(S1).unwrap());
    assert_eq!(
        store.session(S1).unwrap().unwrap().state,
        SessionState::Stopped
    );
    // Stopped already, or not there: nothing to do.
    assert!(!store.stop_recording(S1).unwrap());
    assert!(!store.stop_recording(SessionId::new(9)).unwrap());
    assert_eq!(
        store.session(S2).unwrap().unwrap().state,
        SessionState::Recording
    );
}

#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding: a symlink where the database goes"
)]
#[test]
fn a_symlink_is_never_followed() {
    let dir = TestDir::new("symlink");
    let target = dir.0.join("elsewhere.db");
    // To a database that's there, and to nothing.
    drop(open(&dir));
    std::fs::rename(dir.db(), &target).unwrap();
    std::os::unix::fs::symlink(&target, dir.db()).unwrap();
    assert!(matches!(Store::open(&dir.db()), Err(StoreError::Sqlite(_))));
    std::fs::remove_file(dir.db()).unwrap();
    std::os::unix::fs::symlink(dir.0.join("nothing.db"), dir.db()).unwrap();
    assert!(Store::open(&dir.db()).is_err());
    assert!(std::fs::symlink_metadata(dir.0.join("nothing.db")).is_err());
}

/// Two first opens at once: one may be told the file is busy (a [`Writer`]
/// opens again at its next call), but none fails otherwise, and the schema
/// is made once, whole.
#[test]
fn a_fresh_file_opened_twice_at_once_gets_one_schema() {
    let dir = TestDir::new("race");
    let path = dir.db();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || Store::open(&path).map(|_| ()))
        })
        .collect();
    for thread in threads {
        match thread.join().unwrap() {
            Ok(()) => {}
            Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(e, _)))
                if e.code == rusqlite::ErrorCode::DatabaseBusy => {}
            Err(e) => panic!("{e}"),
        }
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.pragma_text("user_version"), "2");
    assert_eq!(store.sessions().unwrap(), vec![]);
}

#[test]
fn a_full_disk_is_told_from_other_sqlite_errors() {
    let failure = |code| {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ))
    };
    assert!(failure(rusqlite::ffi::SQLITE_FULL).is_disk_full());
    assert!(!failure(rusqlite::ffi::SQLITE_IOERR).is_disk_full());
    assert!(!failure(rusqlite::ffi::SQLITE_BUSY).is_disk_full());
    assert!(!StoreError::OutOfRange.is_disk_full());
}
