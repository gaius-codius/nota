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
//! share it. Within the owner, salvage and recording exclude each other:
//! [`salvage`](crate::segment::salvage) refuses while a
//! [`SessionWriter`](super::SessionWriter) is open, and a writer can't open
//! while salvage runs or another writer is open.
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
//! published, and deleted, through another's handle. Rows carry no session
//! column yet, so the binding can't be checked against the rows themselves,
//! and nothing checks that the store given to [`SessionStore::new`] is this
//! session's; that comes with the library schema. Until then a row's claim
//! is checked against its file's hash (see
//! [`publish_journals`](crate::segment::publish_journals)), which is what
//! keeps a wrong row from letting a journal go.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use nota_core::SessionId;

use crate::fs::Fs;
use crate::segment::SegmentStore;

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
                using: AtomicU8::new(Use::Idle as u8),
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
    /// drops; refused if it's already in use for anything.
    pub(crate) fn begin(&self, what: Use) -> Result<InUse<S::Lock>, Use> {
        self.held
            .using
            .compare_exchange(
                Use::Idle as u8,
                what as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(Use::from_u8)?;
        Ok(InUse {
            held: Arc::clone(&self.held),
        })
    }
}

/// The lock and what the owner is doing with the session.
#[derive(Debug)]
struct Held<L> {
    _guard: L,
    using: AtomicU8,
}

/// What a session's owner is doing with it: at most one of these at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Use {
    /// Nothing that excludes the others.
    Idle = 0,
    /// A [`SessionWriter`](super::SessionWriter) is open.
    Recording = 1,
    /// [`salvage`](crate::segment::salvage) is running.
    Salvaging = 2,
}

impl Use {
    const fn from_u8(n: u8) -> Self {
        match n {
            1 => Self::Recording,
            2 => Self::Salvaging,
            _ => Self::Idle,
        }
    }
}

/// The session in use for one thing; back to idle when dropped. Holds the
/// lock too, so the session stays owned while it's in use.
#[derive(Debug)]
pub(crate) struct InUse<L> {
    held: Arc<Held<L>>,
}

impl<L> Drop for InUse<L> {
    fn drop(&mut self) {
        self.held.using.store(Use::Idle as u8, Ordering::Release);
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

    /// The directory and the store, to work with both at once.
    pub(crate) const fn parts(&mut self) -> (&SessionDir<S>, &mut T) {
        (self.session.session(), &mut self.store)
    }

    /// Unbinds the store, giving back both parts.
    #[must_use]
    pub fn into_parts(self) -> (SessionLock<S>, T) {
        (self.session, self.store)
    }
}
