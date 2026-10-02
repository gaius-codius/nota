//! A session's directory, and its store, bound together once.
//!
//! Publishing and salvage delete journals on the word of the store's rows,
//! so the rows and the files they're checked against must be the same
//! session's. Neither takes a store and a directory as separate arguments:
//! they take a [`SessionStore`], and the only way to make one is
//! [`SessionStore::new`], from a [`SessionDir`], which is made only by
//! [`SessionDir::new`].
//!
//! Recording takes the [`SessionDir`] alone: journals are created and
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

use std::path::{Path, PathBuf};

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
}

/// A session's directory bound to the store holding its segment rows: what
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
/// fn publish(store: &mut nota_store::Store, length: SegmentLength) {
///     let dir = SessionDir::new(SessionId::new(1), StdFs, Path::new("/sessions/1"));
///     let mut session = SessionStore::new(dir, store);
///     let _ = publish_journals(&mut session, length, &[]);
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
#[derive(Debug)]
pub struct SessionStore<S, T> {
    session: SessionDir<S>,
    store: T,
}

impl<S: Fs, T: SegmentStore> SessionStore<S, T> {
    /// Binds `session` to `store`, which must hold this session's rows. The
    /// one place a store and a directory are put together.
    #[must_use]
    pub const fn new(session: SessionDir<S>, store: T) -> Self {
        Self { session, store }
    }

    /// The session's directory.
    #[must_use]
    pub const fn session(&self) -> &SessionDir<S> {
        &self.session
    }

    /// The directory and the store, to work with both at once.
    pub(crate) const fn parts(&mut self) -> (&SessionDir<S>, &mut T) {
        (&self.session, &mut self.store)
    }

    /// Unbinds the store, giving back both parts.
    #[must_use]
    pub fn into_parts(self) -> (SessionDir<S>, T) {
        (self.session, self.store)
    }
}
