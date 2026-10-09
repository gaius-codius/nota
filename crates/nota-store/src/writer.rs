//! The process's one connection to the library database.
//!
//! [`Writer`] holds it, and its clones share it, so every thread that
//! writes (the publisher, the screen's thread) goes through one connection,
//! one call at a time. It opens the database when it's first used, not
//! when it's made: making one does no I/O and can't fail, so recording can
//! start without the database. A call that can't open it fails, and the
//! next call tries again. A call that fails in SQLite closes the
//! connection, so the next opens it afresh: a database whose file was
//! replaced, or whose disk came back, is found again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::{Store, StoreError};

/// The library database, opened when first used and shared by its clones.
#[derive(Debug, Clone)]
pub struct Writer {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    path: PathBuf,
    store: Mutex<Option<Store>>,
}

impl Writer {
    /// The library database at `path`, not yet opened.
    #[must_use]
    pub fn new(path: &Path) -> Self {
        Self {
            shared: Arc::new(Shared {
                path: path.to_path_buf(),
                store: Mutex::new(None),
            }),
        }
    }

    /// The database's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.shared.path
    }

    /// Runs `work` on the connection, opening the database first if it
    /// isn't open. Waits while another clone's call runs.
    ///
    /// # Errors
    ///
    /// [`Store::open`]'s error if the database can't be opened, or
    /// `work`'s. After a [`StoreError::Sqlite`] the connection is closed,
    /// and the next call opens it again.
    pub fn with<R>(
        &self,
        work: impl FnOnce(&mut Store) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        // Nothing panics while holding it, and a transaction a panic left
        // open rolls back as it drops, so a poisoned connection is still
        // consistent.
        let mut slot = self
            .shared
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let store = match &mut *slot {
            Some(store) => store,
            None => slot.insert(Store::open(&self.shared.path)?),
        };
        let result = work(store);
        if matches!(result, Err(StoreError::Sqlite(_))) {
            *slot = None;
        }
        result
    }
}

#[cfg(test)]
mod tests;
