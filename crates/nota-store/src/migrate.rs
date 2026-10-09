//! Schema versions, and importing the per-session stores that came before
//! the library database.
//!
//! The version is SQLite's `user_version`. Opening a library database
//! brings it to [`VERSION`]: every step it needs and the new version in one
//! transaction, so a crash leaves the old version or the new one. A version
//! this code doesn't know, newer or not a library's, is refused before
//! anything in the file is changed.
//!
//! | Version | Schema |
//! |---|---|
//! | 0 | an empty file: nothing created yet |
//! | 1 | the M1 per-session store, `sessions/<n>/nota.db`: a `segment` table with no session column. Never a library database; imported by [`Store::adopt_session`] |
//! | 2 | the first library schema ([`crate::schema`]) |
//! | 3 | each session's start time, `session.started_at` |
//! | 4 | triggers that keep the heard text and its revisions append-only |
//! | 5 | the job queue's progress and waits, and the final pass's text |
//!
//! # The per-session stores
//!
//! M1's recordings kept one store per session, next to its audio. When the
//! library finds a session directory that isn't in the database, it adopts
//! it: the session's row and, if it has a per-session store, that store's
//! segment rows go in, in one transaction. A session in the database is
//! never adopted again, so its old store isn't read again; the file is left
//! where it is.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::schema;
use crate::segments::{self, parse_row, raw_from_row, session_exists};
use crate::sessions::{NewSession, SessionState, insert_session, insert_tracks};
use crate::{Store, StoreError, session_key};

/// The schema version this code writes and understands.
pub const VERSION: i64 = 5;

/// The first library version: what [`STEPS`]' first step makes.
const FIRST: i64 = 2;

/// The per-session store's version.
const PER_SESSION: i64 = 1;

/// The SQL that makes each version from the one before, in order: version
/// 2 (from an empty file), then 3, 4 and 5. There's no step from 1, which is never
/// a library. [`upgrade`] runs the steps above the file's version.
const STEPS: &[&str] = &[schema::V2, schema::V3, schema::V4, schema::V5];

fn version(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// The library database's version, if it's one this code can open: the
/// current one, an earlier library version it upgrades, or 0 for an empty
/// file. Reads, never writes. The version
/// and the tables are read in one statement, so another process's commit
/// can't fall between them.
pub(crate) fn check(conn: &Connection) -> Result<i64, StoreError> {
    let (version, tables): (i64, i64) = conn.query_row(
        "SELECT (SELECT user_version FROM pragma_user_version), \
                (SELECT count(*) FROM sqlite_schema)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    match (version, tables) {
        (FIRST..=VERSION, _) => Ok(version),
        (PER_SESSION, _) => Err(StoreError::PerSessionStore),
        (0, 0) => Ok(0),
        (other, _) => Err(StoreError::UnknownSchema(other)),
    }
}

/// Brings the database to [`VERSION`]. A database at the current version
/// is only read, so opening one takes no write lock.
pub(crate) fn upgrade(conn: &mut Connection) -> Result<(), StoreError> {
    upgrade_with(conn, STEPS)
}

/// [`upgrade`] with `steps` as the steps, so a test can make one fail
/// after another has run.
fn upgrade_with(conn: &mut Connection, steps: &[&str]) -> Result<(), StoreError> {
    if check(conn)? == VERSION {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another process may have upgraded it since the check.
    let from = check(&tx)?;
    if from == VERSION {
        return Ok(());
    }
    // The steps above the file's version: all of them for an empty file.
    let done = usize::try_from((from - FIRST + 1).max(0)).unwrap_or(0);
    for step in steps.iter().skip(done) {
        tx.execute_batch(step)?;
    }
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;
    Ok(())
}

/// What [`Store::adopt_session`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adopted {
    /// The session was in the library already; nothing changed.
    Known,
    /// The session was added, with no segment rows.
    Added,
    /// The session was added with this many segment rows from its
    /// per-session store.
    Imported(usize),
}

/// The segment rows in the per-session store at `path`, which must exist.
fn read_per_session(path: &Path) -> Result<Vec<crate::SegmentRow>, StoreError> {
    // Read-write, so SQLite can recover a write-ahead log a crash left, but
    // never created (a store that isn't there holds nothing to import), and
    // never through a symlink.
    let conn = crate::connect(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    match version(&conn)? {
        PER_SESSION => {}
        // Created, but its schema never committed: it holds no rows.
        0 => {
            let tables: i64 =
                conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
            if tables == 0 {
                return Ok(Vec::new());
            }
            return Err(StoreError::UnknownSchema(0));
        }
        other => return Err(StoreError::UnknownSchema(other)),
    }
    let mut stmt = conn.prepare(
        "SELECT track, epoch, start_sample, end_sample, sha256 \
         FROM segment ORDER BY track, start_sample",
    )?;
    let raws = stmt
        .query_map([], raw_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    raws.into_iter().map(parse_row).collect()
}

impl Store {
    /// Adds `session`, found on disk, if it isn't in the library: its row,
    /// in [`SessionState::Stopped`], with whatever the disk kept of it
    /// (title, language, start time, tracks: [`NewSession::bare`] if
    /// nothing), and the segment rows of its per-session store at
    /// `per_session`, if it had one, in one transaction. A session already
    /// in the library is left as it is, and its per-session store isn't
    /// read.
    ///
    /// The per-session store is opened but not changed, except that SQLite
    /// recovers a write-ahead log a crash left in it.
    ///
    /// # Errors
    ///
    /// [`StoreError::UnknownSchema`] if the per-session store's version
    /// isn't 1, [`StoreError::Corrupt`] if one of its rows doesn't parse,
    /// [`StoreError::Conflict`] if two of its rows overlap,
    /// [`StoreError::OutOfRange`] if the session's number doesn't fit
    /// SQLite's integer, and [`StoreError::Sqlite`] for any SQLite failure,
    /// including two tracks with one number. Nothing is added then.
    pub fn adopt_session(
        &mut self,
        session: &NewSession,
        per_session: Option<&Path>,
    ) -> Result<Adopted, StoreError> {
        let id = session.id;
        let key = session_key(id)?;
        if session_exists(&self.conn, key)? {
            return Ok(Adopted::Known);
        }
        let rows = match per_session {
            Some(path) => Some(read_per_session(path)?),
            None => None,
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Another process may have adopted it since the check above.
        if session_exists(&tx, key)? {
            return Ok(Adopted::Known);
        }
        insert_session(
            &tx,
            id,
            session.title.as_deref(),
            session.language.as_deref(),
            session.started_at,
            SessionState::Stopped,
        )?;
        insert_tracks(&tx, id, &session.tracks)?;
        for row in rows.iter().flatten() {
            segments::insert(&tx, id, row)?;
        }
        tx.commit()?;
        Ok(match rows {
            Some(rows) => Adopted::Imported(rows.len()),
            None => Adopted::Added,
        })
    }
}

#[cfg(test)]
mod tests;
