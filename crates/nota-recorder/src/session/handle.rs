//! A session's directory, its one owner, and its store, bound together once.
//!
//! # One owner
//!
//! Salvage deletes every journal in a session's directory, so it must never
//! run while a writer is still appending to one: it would publish the live
//! journal's prefix and unlink it, and everything recorded after would go
//! into an unlinked file. Whoever records, publishes or salvages a session
//! first takes its [`SessionLock`]: an advisory lock (`flock`) on the
//! directory, refused at once (never waited for) while anyone else holds it,
//! in this process or another. Clones of a [`SessionLock`] are the same
//! owner, so a writer and the thread publishing its finished journals can
//! share it. Within the owner, [`salvage`](crate::segment::salvage)
//! refuses while a [`SessionWriter`](super::SessionWriter) is open or
//! journals are being published, and nothing else starts while it runs;
//! there's one writer at a time, and one publishing run (see [`Use`]).
//!
//! # The store
//!
//! Publishing and salvage delete journals on the word of the store's rows,
//! so the rows and the files they're checked against must be the same
//! session's. Neither takes a store and a directory as separate arguments:
//! they take a [`SessionStore`], and the only way to make one is
//! [`SessionStore::new`], from a [`SessionLock`], which is made only by
//! [`SessionDir::lock`].
//!
//! Recording takes the [`SessionLock`] alone: journals are created and
//! rotated whether or not the store works, or even opens, and publishing
//! catches up once it does.
//!
//! The session's id is checked against the finished journals handed to
//! publishing, which name their session, so one session's journals can't be
//! published, and deleted, through another's handle. Rows are scoped by
//! session in the library database, and every store call made through a
//! [`SessionStore`] names the lock's session (see `Rows`), so it reads and
//! writes only its own session's rows. The database's foreign key refuses a
//! row for a session not in its sessions table. A row's claim is also
//! checked against its file's hash (see
//! [`publish_journals`](crate::segment::publish_journals)), which is what
//! keeps a wrong row from letting a journal go.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use nota_core::SessionId;
use nota_store::SegmentRow;

use crate::fs::Fs;
use crate::segment::{DurableSegment, SegmentStore};

/// One session's identity and directory, on one filesystem. Making it does
/// no I/O.
#[derive(Debug, Clone)]
pub struct SessionDir<S> {
    id: SessionId,
    fs: S,
    dir: PathBuf,
}

impl<S: Fs> SessionDir<S> {
    /// Session `id`, recorded into `dir` on `fs`.
    #[must_use]
    pub fn new(id: SessionId, fs: S, dir: &Path) -> Self {
        Self {
            id,
            fs,
            dir: dir.to_path_buf(),
        }
    }

    /// The session.
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// The filesystem the session's files are on.
    #[must_use]
    pub const fn fs(&self) -> &S {
        &self.fs
    }

    /// The session's directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Takes ownership of the session: locks its directory, or refuses at
    /// once if anyone else holds it (see the module docs).
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::WouldBlock`] if the session is owned elsewhere: by
    /// another process, or another [`SessionLock`] in this one. Any I/O
    /// error, including a missing directory.
    pub fn lock(&self) -> io::Result<SessionLock<S>>
    where
        S: Clone,
    {
        let guard = self.fs.lock_dir(&self.dir)?;
        Ok(SessionLock {
            session: self.clone(),
            held: Arc::new(Held {
                _guard: guard,
                using: Mutex::new(Using::default()),
            }),
        })
    }
}

/// Ownership of one session: its directory locked by this process. Made
/// only by [`SessionDir::lock`]; the lock is released when the last clone
/// drops.
#[derive(Debug)]
pub struct SessionLock<S: Fs> {
    session: SessionDir<S>,
    held: Arc<Held<S::Lock>>,
}

impl<S: Fs + Clone> Clone for SessionLock<S> {
    /// The same owner: the clone shares the lock, and what it's being used
    /// for.
    fn clone(&self) -> Self {
        Self {
            session: self.session.clone(),
            held: Arc::clone(&self.held),
        }
    }
}

impl<S: Fs> SessionLock<S> {
    /// The session owned.
    #[must_use]
    pub const fn session(&self) -> &SessionDir<S> {
        &self.session
    }

    /// Marks the session as in use for `what`, until the returned guard
    /// drops; refused, with the use in the way, if it's in use for anything
    /// `what` excludes (see [`Use`]).
    pub(crate) fn begin(&self, what: Use) -> Result<InUse<S::Lock>, Use> {
        let mut using = self.held.using();
        if let Some(busy) = using.in_the_way_of(what) {
            return Err(busy);
        }
        *using.flag(what) = true;
        drop(using);
        Ok(InUse {
            held: Arc::clone(&self.held),
            what,
        })
    }
}

/// The lock and what the owner is doing with the session.
#[derive(Debug)]
struct Held<L> {
    _guard: L,
    using: Mutex<Using>,
}

impl<L> Held<L> {
    /// Nothing panics while holding it, so a poisoned state is still
    /// consistent.
    fn using(&self) -> MutexGuard<'_, Using> {
        self.using.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Which uses are under way.
#[derive(Debug, Default)]
struct Using {
    recording: bool,
    publishing: bool,
    salvaging: bool,
}

impl Using {
    fn flag(&mut self, what: Use) -> &mut bool {
        match what {
            Use::Recording => &mut self.recording,
            Use::Publishing => &mut self.publishing,
            Use::Salvaging => &mut self.salvaging,
        }
    }

    /// A use under way that `what` can't run alongside, if any.
    fn in_the_way_of(&self, what: Use) -> Option<Use> {
        if self.salvaging {
            return Some(Use::Salvaging);
        }
        match what {
            Use::Recording | Use::Salvaging if self.recording => Some(Use::Recording),
            Use::Publishing | Use::Salvaging if self.publishing => Some(Use::Publishing),
            _ => None,
        }
    }
}

/// What a session's owner is doing with it. Each excludes another of its
/// kind; salvage excludes everything, since it takes every journal as
/// finished and decides what the directory holds. Recording and publishing
/// its finished journals run together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// A [`SessionWriter`](super::SessionWriter) is open.
    Recording,
    /// [`publish_journals`](crate::segment::publish_journals) is running.
    Publishing,
    /// [`salvage`](crate::segment::salvage) is running.
    Salvaging,
}

/// The session in use for one thing, until dropped. Holds the lock too, so
/// the session stays owned while it's in use.
#[derive(Debug)]
pub(crate) struct InUse<L> {
    held: Arc<Held<L>>,
    what: Use,
}

impl<L> Drop for InUse<L> {
    fn drop(&mut self) {
        *self.held.using().flag(self.what) = false;
    }
}

/// An owned session bound to the store holding its segment rows: what
/// publishing and salvage take. Making it does no I/O and doesn't touch the
/// store.
///
/// ```
/// use std::path::Path;
///
/// use nota_core::SessionId;
/// use nota_recorder::fs::StdFs;
/// use nota_recorder::segment::{SegmentLength, publish_journals};
/// use nota_recorder::session::{SessionDir, SessionStore};
///
/// fn publish(store: &mut nota_store::Store, length: SegmentLength) -> std::io::Result<()> {
///     let dir = SessionDir::new(SessionId::new(1), StdFs, Path::new("/sessions/1"));
///     let mut session = SessionStore::new(dir.lock()?, store);
///     let _ = publish_journals(&mut session, length, &[]);
///     Ok(())
/// }
/// ```
///
/// A store and a directory can't be passed separately:
///
/// ```compile_fail,E0061
/// use std::path::Path;
///
/// use nota_recorder::fs::StdFs;
/// use nota_recorder::segment::{SegmentLength, publish_journals};
///
/// fn publish(store: &mut nota_store::Store, length: SegmentLength) {
///     let _ = publish_journals(&StdFs, store, Path::new("/sessions/1"), length, &[]);
/// }
/// ```
///
/// or bound any other way than [`SessionStore::new`]:
///
/// ```compile_fail,E0451
/// use std::path::Path;
///
/// use nota_core::SessionId;
/// use nota_recorder::fs::StdFs;
/// use nota_recorder::session::{SessionDir, SessionStore};
///
/// fn bind(store: &mut nota_store::Store) {
///     let session = SessionDir::new(SessionId::new(1), StdFs, Path::new("/sessions/1"));
///     let _ = SessionStore { session, store };
/// }
/// ```
///
/// and a directory that isn't owned can't be bound at all:
///
/// ```compile_fail,E0308
/// use std::path::Path;
///
/// use nota_core::SessionId;
/// use nota_recorder::fs::StdFs;
/// use nota_recorder::session::{SessionDir, SessionStore};
///
/// fn bind(store: &mut nota_store::Store) {
///     let session = SessionDir::new(SessionId::new(1), StdFs, Path::new("/sessions/1"));
///     let _ = SessionStore::new(session, store);
/// }
/// ```
#[derive(Debug)]
pub struct SessionStore<S: Fs, T> {
    session: SessionLock<S>,
    store: T,
}

impl<S: Fs, T: SegmentStore> SessionStore<S, T> {
    /// Binds the owned `session` to `store`, which must hold this session's
    /// rows. The one place a store and a directory are put together.
    #[must_use]
    pub const fn new(session: SessionLock<S>, store: T) -> Self {
        Self { session, store }
    }

    /// The session's directory.
    #[must_use]
    pub const fn session(&self) -> &SessionDir<S> {
        self.session.session()
    }

    /// The ownership the store is bound under.
    #[must_use]
    pub const fn lock(&self) -> &SessionLock<S> {
        &self.session
    }

    /// The directory and the store's rows of this session, to work with
    /// both at once.
    pub(crate) fn parts(&mut self) -> (&SessionDir<S>, Rows<'_, T>) {
        let session = self.session.session();
        let rows = Rows {
            id: session.id(),
            store: &mut self.store,
        };
        (session, rows)
    }

    /// Unbinds the store, giving back both parts.
    #[must_use]
    pub fn into_parts(self) -> (SessionLock<S>, T) {
        (self.session, self.store)
    }
}

/// A store seen through one session: every call names that session, so
/// nothing that works with it can reach another session's rows.
pub(crate) struct Rows<'a, T> {
    id: SessionId,
    store: &'a mut T,
}

impl<T: SegmentStore> Rows<'_, T> {
    /// Every committed row of the session.
    pub(crate) fn rows(&mut self) -> Result<Vec<SegmentRow>, T::Error> {
        self.store.rows(self.id)
    }

    /// Commits `segment`'s row for the session.
    pub(crate) fn insert(&mut self, segment: &DurableSegment) -> Result<(), T::Error> {
        self.store.insert(self.id, segment)
    }
}
