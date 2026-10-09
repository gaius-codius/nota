//! Schema versions, and importing the per-session stores that came before
//! the library database.
//!
//! The version is SQLite's `user_version`. Opening a library database
//! brings it to [`VERSION`] one step at a time, each step and the new
//! version in one transaction, so a crash leaves the old version or the new
//! one. A newer version than this code knows is refused, never changed.
//!
//! | Version | Schema |
//! |---|---|
//! | 0 | an empty file: nothing created yet |
//! | 1 | the M1 per-session store, `sessions/<n>/nota.db`: a `segment` table with no session column. Never a library database; imported by [`Store::adopt_session`] |
//! | 2 | the first library schema ([`crate::schema`]) |
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

use nota_core::SessionId;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::schema;
use crate::segments::{self, parse_row, raw_from_row, session_exists};
use crate::sessions::{SessionState, insert_session};
use crate::{Store, StoreError, session_key};

/// The schema version this code writes and understands.
pub const VERSION: i64 = 2;

/// The per-session store's version.
const PER_SESSION: i64 = 1;

/// One step: the SQL that takes a database from the version before `to` to
/// `to`.
struct Step {
    to: i64,
    sql: &'static str,
}

/// Every step, in order. Version 2 is created from an empty file; there's
/// no step from 1, which is never a library.
const STEPS: &[Step] = &[Step {
    to: 2,
    sql: schema::V2,
}];

/// Brings the database to [`VERSION`].
pub(crate) fn upgrade(conn: &mut Connection) -> Result<(), StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    match version {
        VERSION => return Ok(()),
        PER_SESSION => return Err(StoreError::PerSessionStore),
        0 => {
            let tables: i64 =
                tx.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
            if tables != 0 {
                return Err(StoreError::UnknownSchema(0));
            }
        }
        other if !(0..VERSION).contains(&other) => return Err(StoreError::UnknownSchema(other)),
        _ => {}
    }
    for step in STEPS.iter().filter(|step| step.to > version) {
        tx.execute_batch(step.sql)?;
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
    // never created: a store that isn't there holds nothing to import.
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(crate::BUSY_TIMEOUT)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    match version {
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
    /// Adds session `id`, found on disk, if it isn't in the library: its
    /// row, in [`SessionState::Stopped`] with no title, and the segment
    /// rows of its per-session store at `per_session`, if it had one, in
    /// one transaction. A session already in the library is left as it is,
    /// and its per-session store isn't read.
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
    /// SQLite's integer, and [`StoreError::Sqlite`] for any SQLite failure.
    /// Nothing is added then.
    pub fn adopt_session(
        &mut self,
        id: SessionId,
        per_session: Option<&Path>,
    ) -> Result<Adopted, StoreError> {
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
        insert_session(&tx, id, None, None, SessionState::Stopped)?;
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
