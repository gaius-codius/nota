//! The filesystem layer: the only way the recorder touches the disk.
//!
//! Every durable write (the journal, FLAC segments, their renames and
//! directory syncs) goes through an [`Fs`]. The real one, [`StdFs`], is the
//! only code in nota allowed to call `std::fs`'s write functions; clippy's
//! `disallowed-methods` bans them everywhere else. Tests use `FakeFs` (feature
//! `fake-fs`), which records every operation and can simulate a crash that
//! loses whatever wasn't fsync'd, so a crash after each operation of a write
//! path is a fast, exhaustive unit test (see `crash`).
//!
//! The operations are exactly what the write path needs, with the
//! guarantees it may rely on:
//! - [`Fs::create`] makes a new, empty file and fails if the name exists. The
//!   file's directory entry isn't durable until [`Fs::sync_dir`].
//! - [`Fs::create_dir`] makes a directory; its entry, in its parent, isn't
//!   durable until [`Fs::sync_dir`] on the parent.
//! - [`FsFile::write_all`] appends; there's no seek. A FLAC segment is
//!   encoded in memory and published as one append, fsync, rename and
//!   directory sync. The bytes aren't durable until
//!   [`FsFile::sync`]; a crash may keep none, some or all of them, and kept
//!   bytes past the last sync may read back as zeros.
//! - [`Fs::rename`] moves a name within one directory, replacing any file
//!   at the new name. It isn't durable until that directory is synced.
//! - [`Fs::remove`] unlinks a name; durable after a directory sync.
//! - [`Fs::read`] reads a whole file, and [`Fs::list`] a directory's
//!   entries, as the running system sees them.
//!
//! Paths are absolute, or at least have a non-empty directory part:
//! `journal` alone is refused, so the fake and the real filesystem agree.
//! New files are private to the user (mode 0600, directories 0700): they
//! hold recordings.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(any(test, feature = "fake-fs"))]
pub mod crash;
#[cfg(any(test, feature = "fake-fs"))]
pub mod fake;
mod real;

pub use real::{StdFile, StdFs};

/// A filesystem the recorder writes through.
pub trait Fs: Send + Sync + fmt::Debug {
    /// An open file, written by appending.
    type File: FsFile;

    /// Creates a new, empty file at `path` for appending. Fails with
    /// [`io::ErrorKind::AlreadyExists`] if anything is there already. The
    /// new name is durable only after [`Fs::sync_dir`] on its directory.
    ///
    /// # Errors
    ///
    /// Any I/O error, including a missing directory.
    fn create(&self, path: &Path) -> io::Result<Self::File>;

    /// Creates the directory `path`, whose parent must exist. Fails with
    /// [`io::ErrorKind::AlreadyExists`] if anything is there already. The
    /// new name is durable only after [`Fs::sync_dir`] on the parent.
    ///
    /// # Errors
    ///
    /// Any I/O error, including a missing parent.
    fn create_dir(&self, path: &Path) -> io::Result<()>;

    /// Renames `from` to `to`, replacing any file at `to`. Both must be in the
    /// same directory, which keeps the crash model simple and is all the
    /// write path needs (`seg.flac.tmp` to `seg.flac`). Durable only after
    /// [`Fs::sync_dir`] on that directory.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if the directories differ; any I/O
    /// error, including a missing `from`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Makes every create, rename and remove in `dir` so far durable.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;

    /// Removes the file at `path`. Durable only after [`Fs::sync_dir`] on
    /// its directory.
    ///
    /// # Errors
    ///
    /// Any I/O error, including [`io::ErrorKind::NotFound`].
    fn remove(&self, path: &Path) -> io::Result<()>;

    /// Reads the whole file at `path`, as the running system sees it
    /// (durable or not).
    ///
    /// # Errors
    ///
    /// Any I/O error, including [`io::ErrorKind::NotFound`].
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// The paths of the files and directories in `dir`, sorted, as the
    /// running system sees them.
    ///
    /// # Errors
    ///
    /// Any I/O error, including [`io::ErrorKind::NotFound`].
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
}

/// A file opened by [`Fs::create`].
pub trait FsFile: Send + fmt::Debug {
    /// Appends `bytes` to the file. They aren't durable until [`Self::sync`].
    ///
    /// # Errors
    ///
    /// Any I/O error. After an error, some of `bytes` may have been written.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Waits until everything written so far is on disk (fsync), and returns
    /// the proof.
    ///
    /// # Errors
    ///
    /// Any I/O error. After a failed fsync, nothing written since the last
    /// successful one can be assumed durable.
    fn sync(&mut self) -> io::Result<Synced>;
}

/// Proof that an fsync completed. Only this module's filesystems can make
/// one, so a durable position can't be claimed without a sync behind it.
#[derive(Debug)]
pub struct Synced(());

impl Synced {
    /// Called by a filesystem once its fsync has returned successfully.
    const fn after_fsync() -> Self {
        Self(())
    }
}

/// Checks that `path` has a non-empty directory part and a file name.
fn valid_path(path: &Path) -> io::Result<()> {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(_)) if !dir.as_os_str().is_empty() => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a path needs a directory and a name",
        )),
    }
}

/// Checks that `from` and `to` are valid paths in the same directory, as
/// [`Fs::rename`] requires.
fn same_directory(from: &Path, to: &Path) -> io::Result<()> {
    valid_path(from)?;
    valid_path(to)?;
    if from.parent() == to.parent() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a rename must stay within one directory",
        ))
    }
}
