//! Where sessions live, on disk and in the library database, and
//! salvaging them at start.
//!
//! Every session has a directory under the data directory,
//! `sessions/<number>/`, holding its audio directory (`audio/`: journals,
//! segments, marks), and a row in the library database
//! (`library.db` in the data directory) with its segment rows. Numbers
//! count up from 1 and are never reused: a new session takes the number
//! after the highest on disk or in the database.
//!
//! **Recording never waits on the database.** A new session's row is
//! added by the publisher, before its first segment row, so a database
//! that can't be opened holds up publishing (the journals stay on disk)
//! and never recording. The next start adds any session directory the
//! database doesn't have, importing its M1 per-session store
//! (`sessions/<number>/nota.db`) if it has one, and salvages it.

use std::io;
use std::path::{Path, PathBuf};

use nota_core::SessionId;
use nota_recorder::fs::{Fs, StdFs};
use nota_recorder::segment::{DurableSegment, SegmentLength, SegmentStore, needs_salvage, salvage};
use nota_recorder::session::{SessionDir, SessionStore};
use nota_store::{NewSession, SegmentRow, SessionState, StoreError, Writer};

/// Names under the data directory, and in a session's directory.
const SESSIONS: &str = "sessions";
const LIBRARY_DB: &str = "library.db";
const AUDIO: &str = "audio";
/// M1's per-session store.
const PER_SESSION_STORE: &str = "nota.db";

/// The sessions under one data directory.
#[derive(Debug, Clone)]
pub(crate) struct Library {
    root: PathBuf,
    db: Writer,
}

/// One session's place on disk.
#[derive(Debug, Clone)]
pub(crate) struct SessionPaths {
    pub(crate) id: SessionId,
    pub(crate) dir: PathBuf,
}

impl SessionPaths {
    /// The session's audio directory.
    pub(crate) fn audio(&self) -> PathBuf {
        self.dir.join(AUDIO)
    }

    /// Where an M1 session kept its own store.
    pub(crate) fn per_session_store(&self) -> PathBuf {
        self.dir.join(PER_SESSION_STORE)
    }
}

/// What salvage did to one session at start.
#[derive(Debug)]
pub(crate) enum Salvaged {
    /// It had journals left, and salvage published them.
    Done(SessionId),
    /// Salvage ran, but left journals it couldn't publish yet (unreadable,
    /// or held back by a segment that doesn't match its row); the next
    /// start tries again.
    Left(SessionId),
    /// Another nota is using it; it's left alone.
    InUse(SessionId),
    /// Salvage failed, or couldn't start because the session couldn't be
    /// added to the library database; the journals are still there for the
    /// next start.
    Failed(SessionId, String),
}

impl Library {
    /// The sessions under `root`, which is created if it's missing. The
    /// library database isn't opened until it's used.
    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        ensure_dir(&root.join(SESSIONS))?;
        Ok(Self {
            root: root.to_path_buf(),
            db: Writer::new(&root.join(LIBRARY_DB)),
        })
    }

    /// The library database.
    pub(crate) const fn db(&self) -> &Writer {
        &self.db
    }

    fn sessions(&self) -> PathBuf {
        self.root.join(SESSIONS)
    }

    /// Every session on disk, in number order.
    pub(crate) fn existing(&self) -> io::Result<Vec<SessionPaths>> {
        let mut found: Vec<SessionPaths> = StdFs
            .list(&self.sessions())?
            .into_iter()
            .filter_map(|dir| {
                let name = dir.file_name()?.to_str()?;
                let id: u64 = name.parse().ok()?;
                // Only the name a session is given: not `07` or `+7`.
                if id.to_string() != name {
                    return None;
                }
                Some(SessionPaths {
                    id: SessionId::new(id),
                    dir,
                })
            })
            .collect();
        found.sort_by_key(|s| s.id.get());
        Ok(found)
    }

    /// Adds `session` to the library database if it isn't there, with its
    /// per-session store's rows if it has one. A session directory that
    /// can't be listed isn't added, so the next start can still import its
    /// store.
    fn adopt(&self, session: &SessionPaths) -> Result<(), String> {
        let store = session.per_session_store();
        let there = StdFs
            .list(&session.dir)
            .map_err(|e| format!("listing {}: {e}", session.dir.display()))?;
        let per_session = there.contains(&store).then_some(store.as_path());
        self.db
            .with(|db| db.adopt_session(session.id, per_session))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Adds every session on disk to the library database, and salvages
    /// every one left with journals, as after a crash or a stop that
    /// couldn't publish everything. A session the database still says is
    /// recording, which no one is, is marked stopped. A session another
    /// nota holds is left alone, in the database as on disk, and so is one
    /// with nothing in it yet (another nota may have just made it).
    ///
    /// A session that can't be added isn't salvaged (salvage would read no
    /// rows for it): if it has journals, that's reported. The next start
    /// tries again. A database that can't be opened is tried once, not
    /// once for each session.
    pub(crate) fn salvage_all(&self, length: SegmentLength) -> io::Result<Vec<Salvaged>> {
        let sessions = self.existing()?;
        if let Err(e) = self.db.with(|_| Ok(())) {
            return Ok(sessions
                .iter()
                .filter(|s| {
                    needs_salvage(&SessionDir::new(s.id, StdFs, &s.audio())).unwrap_or(false)
                })
                .map(|s| {
                    Salvaged::Failed(s.id, format!("the library database can't be opened: {e}"))
                })
                .collect());
        }
        Ok(sessions
            .iter()
            .filter_map(|session| self.salvage_one(session, length))
            .collect())
    }

    /// Adds `session` to the library database and salvages it if it has
    /// journals left. `None` if there was nothing to report.
    ///
    /// The session is locked first, so a session another nota is
    /// recording is never adopted or marked stopped under it.
    fn salvage_one(&self, session: &SessionPaths, length: SegmentLength) -> Option<Salvaged> {
        if is_empty(session) {
            return None;
        }
        let dir = SessionDir::new(session.id, StdFs, &session.audio());
        // An audio directory that can't be listed has nothing to salvage now.
        let journals = needs_salvage(&dir).unwrap_or(false);
        let lock = match dir.lock() {
            Ok(lock) => lock,
            Err(e) => {
                return journals.then(|| match e.kind() {
                    io::ErrorKind::WouldBlock => Salvaged::InUse(session.id),
                    _ => Salvaged::Failed(session.id, e.to_string()),
                });
            }
        };
        if let Err(e) = self.adopt(session) {
            return journals.then(|| {
                Salvaged::Failed(
                    session.id,
                    format!("adding it to the library database: {e}"),
                )
            });
        }
        // No one is recording it: whatever the database says, it stopped.
        // If this fails, the next start tries again.
        let _stopped = self.db.with(|db| db.stop_recording(session.id));
        if !journals {
            return None;
        }
        let mut bound = SessionStore::new(lock, self.db.clone());
        Some(match salvage(&mut bound, length) {
            Ok(_) if needs_salvage(&dir).unwrap_or(true) => Salvaged::Left(session.id),
            Ok(_) => Salvaged::Done(session.id),
            Err(e) => Salvaged::Failed(session.id, e.to_string()),
        })
    }

    /// Makes a new session's directories, numbered after every session on
    /// disk and, if the database can be read, every session in it.
    pub(crate) fn create(&self) -> io::Result<SessionPaths> {
        let on_disk = self.existing()?.last().map_or(0, |last| last.id.get());
        // A database that can't be read now could hold a number the disk
        // doesn't show only if a session's directory was deleted by hand:
        // nota doesn't delete sessions yet. Rows bound to such a number are
        // refused (`NewSessionRows`), and its journals stay on disk.
        let in_db = self
            .db
            .with(|db| db.sessions())
            .ok()
            .and_then(|sessions| sessions.last().map(|s| s.id.get()))
            .unwrap_or(0);
        let mut next = on_disk.max(in_db).checked_add(1).ok_or_else(no_number)?;
        // Another nota may take the same number first: try the next.
        for _ in 0..16 {
            let dir = self.sessions().join(next.to_string());
            match StdFs.create_dir(&dir) {
                Ok(()) => {
                    StdFs.sync_dir(&self.sessions())?;
                    let paths = SessionPaths {
                        id: SessionId::new(next),
                        dir,
                    };
                    StdFs.create_dir(&paths.audio())?;
                    StdFs.sync_dir(&paths.dir)?;
                    return Ok(paths);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    next = next.checked_add(1).ok_or_else(no_number)?;
                }
                Err(e) => return Err(e),
            }
        }
        Err(no_number())
    }
}

/// Whether `session` holds nothing yet: an empty audio directory and no
/// per-session store, as `create` leaves it. One that can't be listed
/// isn't taken for empty.
fn is_empty(session: &SessionPaths) -> bool {
    let only_audio = StdFs
        .list(&session.dir)
        .is_ok_and(|there| there == [session.audio()]);
    only_audio
        && StdFs
            .list(&session.audio())
            .is_ok_and(|audio| audio.is_empty())
}

fn no_number() -> io::Error {
    io::Error::other("couldn't find a free session number")
}

/// A new session's rows in the library database, as the recorder's store:
/// the session's own row is added before anything else is read or written,
/// when the database is first there to take it.
#[derive(Debug)]
pub(crate) struct NewSessionRows {
    id: SessionId,
    db: Writer,
    /// The session's row, until it's added.
    pending: Option<NewSession>,
}

impl NewSessionRows {
    /// `session`'s rows in `db`. Does no I/O.
    pub(crate) const fn new(db: Writer, session: NewSession) -> Self {
        Self {
            id: session.id,
            db,
            pending: Some(session),
        }
    }

    /// Adds the session's row if it isn't added yet. Refuses a call for any
    /// other session.
    fn added(&mut self, session: SessionId) -> Result<(), StoreError> {
        if session != self.id {
            return Err(StoreError::NoSession(session));
        }
        let Some(new) = &self.pending else {
            return Ok(());
        };
        self.db.with(|db| match db.create_session(new) {
            // Added by an earlier call whose answer was lost: the same
            // session, as long as it's still the same row.
            Err(StoreError::SessionExists(id)) => match db.session(id)? {
                Some(s) if s.title == new.title && s.state == SessionState::Recording => Ok(()),
                _ => Err(StoreError::SessionExists(id)),
            },
            done => done,
        })?;
        self.pending = None;
        Ok(())
    }
}

impl SegmentStore for NewSessionRows {
    type Error = StoreError;

    fn rows(&mut self, session: SessionId) -> Result<Vec<SegmentRow>, StoreError> {
        self.added(session)?;
        self.db.rows(session)
    }

    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), StoreError> {
        self.added(session)?;
        self.db.insert(session, segment)
    }
}

/// Makes `dir` and any missing parents, each durably.
fn ensure_dir(dir: &Path) -> io::Result<()> {
    match StdFs.list(dir) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let parent = parent_of(dir)
        .ok_or_else(|| io::Error::other(format!("{} has no parent", dir.display())))?;
    ensure_dir(parent)?;
    match StdFs.create_dir(dir) {
        Ok(()) => StdFs.sync_dir(parent),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// The directory `dir` is in: for a relative path with one part, the
/// working directory. `None` for a root.
fn parent_of(dir: &Path) -> Option<&Path> {
    match dir.parent()? {
        p if p.as_os_str().is_empty() => Some(Path::new(".")),
        p => Some(p),
    }
}

#[cfg(test)]
mod tests;
