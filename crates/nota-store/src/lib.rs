//! nota's library database.
//!
//! One SQLite database in the data directory holds every session: its
//! title, tracks and state, the rows for the audio segments the recorder
//! has published, the heard text and its revisions ([`transcript`]), the
//! marks and notes made while recording ([`annotations`]), and the tables
//! later work fills (jobs, the timeline). The tables are in [`schema`]; versions
//! and the import of the per-session stores that came before are in
//! [`migrate`].
//!
//! **Recording never waits on it.** The recorder writes journals whether or
//! not the database opens; publishing commits a segment's row before it
//! deletes the journal it came from, and leaves the journal where it is if
//! the row can't be committed. [`Writer`] opens the database when it's
//! first used, and again after a failure, so publishing catches up once the
//! database is back.
//!
//! **One writer.** A process holds one connection, through one [`Writer`]
//! and its clones. Every row is committed durably (`synchronous=FULL`,
//! write-ahead log) before the call that writes it returns.
//!
//! The native library is SQLite, built in through rusqlite's `bundled`
//! feature, so nothing needs installing on the machine. SQLite does its own
//! file I/O, so the database is the one durable write that doesn't go
//! through the recorder's filesystem layer; the recorder's crash tests use a
//! stand-in store, and the `LazyFS` runs check this one on a real filesystem.
//!
//! The database file is created readable by its owner only (`0600`, as
//! the recorder keeps its audio), and SQLite gives its `-wal` and `-shm`
//! files the database file's permissions.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use nota_core::SessionId;
use rusqlite::{Connection, OpenFlags};

pub mod annotations;
pub mod migrate;
pub mod schema;
mod segments;
mod sessions;
pub mod transcript;
mod writer;

pub use annotations::Annotation;
pub use migrate::Adopted;
pub use segments::{Inserted, SegmentRow, Sha256Digest};
pub use sessions::{NewSession, Session, SessionState, Track, TrackKind};
pub use transcript::{Heard, Line, RevisionNumber, StoredUtterance, UtteranceId, Word};
pub use writer::Writer;

/// How long a write waits for another process's write to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a store call failed.
#[derive(Debug)]
pub enum StoreError {
    /// SQLite reported an error.
    Sqlite(rusqlite::Error),
    /// The database file couldn't be created.
    Create(std::io::Error),
    /// A pragma didn't take: its name and what it read back.
    Pragma {
        /// The pragma's name.
        name: &'static str,
        /// The value it read back.
        found: String,
    },
    /// The database's schema version (`PRAGMA user_version`) isn't one this
    /// code knows, or a database without a version already holds tables.
    UnknownSchema(i64),
    /// The file is a per-session store from before the library database:
    /// [`Store::adopt_session`] imports those; it isn't opened as a library.
    PerSessionStore,
    /// A number doesn't fit SQLite's signed 64-bit integer.
    OutOfRange,
    /// A different row already holds this track and start sample in the
    /// session, or some of the new row's samples.
    Conflict {
        /// The row that is stored.
        existing: SegmentRow,
    },
    /// The session isn't in the `session` table.
    NoSession(SessionId),
    /// The session is in the `session` table already.
    SessionExists(SessionId),
    /// The session has no revision with this number.
    NoRevision(SessionId, u32),
    /// The session has no utterance with this number.
    NoUtterance(SessionId, i64),
    /// A stored row doesn't parse: a negative or inverted range, a
    /// wrong-length hash, a number out of its type's range, an unknown
    /// state or kind.
    Corrupt(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "sqlite error: {e}"),
            Self::Create(e) => write!(f, "could not create the database file: {e}"),
            Self::Pragma { name, found } => {
                write!(f, "pragma {name} did not take (read back {found:?})")
            }
            Self::UnknownSchema(v) => write!(f, "unknown database schema version {v}"),
            Self::PerSessionStore => {
                write!(f, "a per-session store, not the library database")
            }
            Self::OutOfRange => write!(f, "a number does not fit a 64-bit signed integer"),
            Self::Conflict { existing } => write!(
                f,
                "a different segment already holds track {} from sample {}",
                existing.track().get(),
                existing.range().start().get()
            ),
            Self::NoSession(id) => write!(f, "session {} is not in the library", id.get()),
            Self::SessionExists(id) => {
                write!(f, "session {} is in the library already", id.get())
            }
            Self::NoRevision(id, n) => {
                write!(f, "session {} has no revision {n}", id.get())
            }
            Self::NoUtterance(id, n) => {
                write!(f, "session {} has no utterance {n}", id.get())
            }
            Self::Corrupt(why) => write!(f, "corrupt row: {why}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Create(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// A session's number as SQLite holds it.
fn session_key(id: SessionId) -> Result<i64, StoreError> {
    i64::try_from(id.get()).map_err(|_| StoreError::OutOfRange)
}

/// A stored session number.
fn parse_session(id: i64) -> Result<SessionId, StoreError> {
    u64::try_from(id)
        .map(SessionId::new)
        .map_err(|_| StoreError::Corrupt(format!("session {id} is negative")))
}

/// Creates the file at `path`, readable and writable by its owner only, if
/// nothing is there. SQLite would create it with the process umask, which
/// usually lets anyone read it. (Its directory entry is made durable by
/// SQLite, which fsyncs the directory when it creates the write-ahead log
/// at the first commit.)
#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "the database's own file; SQLite does the rest of its I/O itself"
)]
fn create_private(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    // `create_new` (O_EXCL) never opens what's there, file or symlink: a
    // second descriptor on a live database would drop SQLite's locks.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(StoreError::Create(e)),
    }
}

/// Elsewhere the file's permissions come from its directory.
#[cfg(not(unix))]
fn create_private(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

/// Opens the SQLite file at `path`, never through a symlink, with a busy
/// timeout for another process's writes.
fn connect(path: &Path, flags: OpenFlags) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(path, flags.union(OpenFlags::SQLITE_OPEN_NOFOLLOW))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// Sets the pragmas every connection needs, and checks each took:
/// `journal_mode=WAL`, `synchronous=FULL` and `foreign_keys=ON`.
fn configure(conn: &Connection) -> Result<(), StoreError> {
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Pragma {
            name: "journal_mode",
            found: mode,
        });
    }
    for (name, set, wanted) in [
        ("synchronous", "PRAGMA synchronous = FULL", 2),
        ("foreign_keys", "PRAGMA foreign_keys = ON", 1),
    ] {
        conn.execute_batch(set)?;
        let found: i64 = conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?;
        if found != wanted {
            return Err(StoreError::Pragma {
                name,
                found: found.to_string(),
            });
        }
    }
    Ok(())
}

/// A connection to the library database. Most callers use it through a
/// [`Writer`], which holds the process's one connection.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the library database at `path`, and brings its
    /// schema to the current version ([`migrate`]). A new file is readable
    /// by its owner only; an existing one keeps its permissions. A symlink
    /// at `path` is refused, and so is a file whose version isn't known,
    /// before anything in it is changed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Create`] if a new file can't be created,
    /// [`StoreError::Pragma`] if a pragma didn't take,
    /// [`StoreError::UnknownSchema`] if the schema version isn't known,
    /// [`StoreError::PerSessionStore`] for a per-session store, and
    /// [`StoreError::Sqlite`] for any other SQLite failure, including a
    /// symlink.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        create_private(path)?;
        let mut conn = connect(path, OpenFlags::default())?;
        migrate::check(&conn)?;
        configure(&conn)?;
        migrate::upgrade(&mut conn)?;
        Ok(Self { conn })
    }

    /// A pragma's current value on this connection, as text.
    #[cfg(test)]
    fn pragma_text(&self, name: &str) -> String {
        self.conn
            .query_row(&format!("PRAGMA {name}"), [], |r| {
                r.get::<_, rusqlite::types::Value>(0)
            })
            .map(|v| match v {
                rusqlite::types::Value::Text(t) => t,
                rusqlite::types::Value::Integer(i) => i.to_string(),
                other => format!("{other:?}"),
            })
            .unwrap()
    }
}

#[cfg(test)]
mod test_dir {
    use std::path::PathBuf;

    /// A fresh directory under the system temp dir, removed when dropped.
    #[derive(Debug)]
    pub(crate) struct TestDir(pub(crate) PathBuf);

    impl TestDir {
        #[expect(
            clippy::disallowed_methods,
            reason = "test scaffolding outside the recorder's write path"
        )]
        pub(crate) fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("nota-store-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// The library database's path in the directory.
        pub(crate) fn db(&self) -> PathBuf {
            self.0.join("library.db")
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
}

#[cfg(test)]
mod tests;
