use nota_core::SessionId;

use super::*;
use crate::test_dir::TestDir;
use crate::tests::new_session;

/// The connection's `cache_size`: a setting of the connection, not the
/// file, so it tells whether a call used the same connection.
fn cache_size(writer: &Writer) -> i64 {
    writer
        .with(|store| {
            Ok(store
                .conn
                .query_row("PRAGMA cache_size", [], |r| r.get(0))?)
        })
        .unwrap()
}

fn set_cache_size(writer: &Writer, pages: i64) {
    writer
        .with(|store| Ok(store.conn.pragma_update(None, "cache_size", pages)?))
        .unwrap();
}

#[test]
fn making_one_opens_nothing() {
    let dir = TestDir::new("lazy");
    let writer = Writer::new(&dir.db());
    assert_eq!(writer.path(), dir.db());
    assert!(std::fs::metadata(dir.db()).is_err());
    writer.with(|store| store.sessions()).unwrap();
    assert!(std::fs::metadata(dir.db()).is_ok());
}

#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding: makes the database's directory appear"
)]
#[test]
fn a_database_that_cant_open_is_tried_again_on_the_next_call() {
    let dir = TestDir::new("retry");
    let path = dir.0.join("later").join("library.db");
    let writer = Writer::new(&path);
    let session = new_session(SessionId::new(1));
    for _ in 0..2 {
        assert!(matches!(
            writer.with(|store| store.create_session(&session)),
            Err(StoreError::Create(_))
        ));
    }
    std::fs::create_dir(dir.0.join("later")).unwrap();
    writer.with(|store| store.create_session(&session)).unwrap();
    assert_eq!(writer.with(|store| store.sessions()).unwrap().len(), 1);
}

#[test]
fn clones_share_one_connection() {
    let dir = TestDir::new("shared");
    let writer = Writer::new(&dir.db());
    let clone = writer.clone();
    set_cache_size(&writer, -123);
    assert_eq!(cache_size(&clone), -123);
}

#[test]
fn a_sqlite_error_closes_the_connection_and_others_keep_it() {
    let dir = TestDir::new("reopen");
    let writer = Writer::new(&dir.db());
    set_cache_size(&writer, -123);
    // Not SQLite's error: the connection stays.
    assert!(matches!(
        writer.with(|store| store.set_state(SessionId::new(9), crate::SessionState::Stopped)),
        Err(StoreError::NoSession(_))
    ));
    assert_eq!(cache_size(&writer), -123);
    // SQLite's: the next call has a new connection.
    assert!(matches!(
        writer.with(|store| Ok(store.conn.execute("NOT SQL", [])?)),
        Err(StoreError::Sqlite(_))
    ));
    assert_ne!(cache_size(&writer), -123);
}

#[test]
fn calls_from_many_threads_are_one_at_a_time() {
    let dir = TestDir::new("threads");
    let writer = Writer::new(&dir.db());
    let threads: Vec<_> = (1..=8)
        .map(|n| {
            let writer = writer.clone();
            std::thread::spawn(move || {
                writer
                    .with(|store| store.create_session(&new_session(SessionId::new(n))))
                    .unwrap();
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(writer.with(|store| store.sessions()).unwrap().len(), 8);
}
