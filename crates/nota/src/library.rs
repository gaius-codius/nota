//! Where sessions live on disk, and salvaging them at start.
//!
//! Every session has a directory under the data directory,
//! `sessions/<number>/`, holding its audio directory (`audio/`: journals,
//! segments, marks) and its store (`nota.db`, one per session for now).
//! Numbers count up from 1 and are never reused: a new session takes the
//! number after the highest there.

use std::io;
use std::path::{Path, PathBuf};

use nota_core::SessionId;
use nota_recorder::fs::{Fs, StdFs};
use nota_recorder::segment::{SegmentLength, needs_salvage, salvage};
use nota_recorder::session::{SessionDir, SessionStore};
use nota_store::Store;

/// A session's directory names.
const SESSIONS: &str = "sessions";
const AUDIO: &str = "audio";
const STORE: &str = "nota.db";

/// The sessions under one data directory.
#[derive(Debug, Clone)]
pub(crate) struct Library {
    root: PathBuf,
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

    /// The session's store.
    pub(crate) fn store(&self) -> PathBuf {
        self.dir.join(STORE)
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
    /// Salvage failed; the journals are still there for the next start.
    Failed(SessionId, String),
}

impl Library {
    /// The sessions under `root`, which is created if it's missing.
    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        ensure_dir(&root.join(SESSIONS))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn sessions(&self) -> PathBuf {
        self.root.join(SESSIONS)
    }

    /// Every session there, in number order.
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

    /// Salvages every session left with journals: as after a crash, or a
    /// stop that couldn't publish everything. A session another nota holds
    /// is left alone.
    pub(crate) fn salvage_all(&self, length: SegmentLength) -> io::Result<Vec<Salvaged>> {
        let mut done = Vec::new();
        for session in self.existing()? {
            if let Some(outcome) = salvage_one(&session, length) {
                done.push(outcome);
            }
        }
        Ok(done)
    }

    /// Makes a new session's directories, numbered after every session
    /// there.
    pub(crate) fn create(&self) -> io::Result<SessionPaths> {
        let mut next = match self.existing()?.last() {
            Some(last) => last.id.get().checked_add(1).ok_or_else(no_number)?,
            None => 1,
        };
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

fn no_number() -> io::Error {
    io::Error::other("couldn't find a free session number")
}

/// Salvages `session` if it has journals left. `None` if it had nothing to
/// do.
fn salvage_one(session: &SessionPaths, length: SegmentLength) -> Option<Salvaged> {
    let dir = SessionDir::new(session.id, StdFs, &session.audio());
    // An audio directory that can't be listed has nothing to salvage now.
    if !needs_salvage(&dir).unwrap_or(false) {
        return None;
    }
    let lock = match dir.lock() {
        Ok(lock) => lock,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            return Some(Salvaged::InUse(session.id));
        }
        Err(e) => return Some(Salvaged::Failed(session.id, e.to_string())),
    };
    let store = match Store::open(&session.store()) {
        Ok(store) => store,
        Err(e) => return Some(Salvaged::Failed(session.id, e.to_string())),
    };
    let mut bound = SessionStore::new(lock, store);
    Some(match salvage(&mut bound, length) {
        Ok(_) if needs_salvage(&dir).unwrap_or(true) => Salvaged::Left(session.id),
        Ok(_) => Salvaged::Done(session.id),
        Err(e) => Salvaged::Failed(session.id, e.to_string()),
    })
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
