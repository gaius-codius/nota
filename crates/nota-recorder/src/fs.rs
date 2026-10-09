//! The filesystem layer: how the recorder touches the disk.
//!
//! Every durable write of audio (the journal, FLAC segments, their renames
//! and directory syncs) goes through an [`Fs`]. The one exception is the
//! database, which SQLite writes itself (see `nota-store`). The real one,
//! [`StdFs`], is the only code here allowed to call `std::fs`'s write
//! functions; clippy's `disallowed-methods` bans them everywhere else, and
//! the one other user (`nota-store`, creating the database file) says so
//! with `#[expect]`. Tests use `FakeFs` (feature
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
//! - [`FsFile::syncer`] gives a handle that fsyncs the file from another
//!   thread while it's still written: the fsync covers at least every
//!   write that returned before it started.
//! - [`FsFile::write_all`] appends; there's no seek. A FLAC segment is
//!   encoded in memory and published as one append, fsync, rename and
//!   directory sync. The bytes aren't durable until
//!   [`FsFile::sync`]; a crash may keep none, some or all of them, and kept
//!   bytes past the last sync may read back as zeros. A failed write may
//!   have appended some of its bytes, and after a failed sync the unsynced
//!   bytes still read back but may never reach the disk.
//! - [`Fs::rename`] moves a file's name within one directory, replacing any
//!   file at the new name. It isn't durable until that directory is synced.
//! - [`Fs::remove`] unlinks a name; durable after a directory sync.
//! - [`Fs::read`] reads a whole regular file, and [`Fs::list`] a
//!   directory's entries, as the running system sees them. A read never
//!   blocks on what's under the name and never holds more than
//!   [`MAX_READ_LEN`] bytes: anything else under a name nota reads (a
//!   symlink, a FIFO, a device, a file too large for anything nota writes)
//!   is an error, like a file that can't be read.
//! - [`Fs::lock_dir`] takes an advisory lock on a directory, without
//!   waiting: one holder at a time, across processes too. It isn't durable;
//!   it ends when its guard drops or the process dies.
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

pub use real::{StdFile, StdFs, StdLock, StdSyncer};

/// A filesystem the recorder writes through.
pub trait Fs: Send + Sync + fmt::Debug {
    /// An open file, written by appending.
    type File: FsFile;

    /// A held directory lock ([`Fs::lock_dir`]); dropping it unlocks.
    type Lock: Send + Sync + fmt::Debug;

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

    /// Renames the file `from` to `to`, replacing any file at `to`. Both must
    /// be in the same directory, and `from` can't be a directory, which keeps
    /// the crash model simple and is all the write path needs
    /// (`seg.flac.tmp` to `seg.flac`). Durable only after [`Fs::sync_dir`] on
    /// that directory.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if the directories differ;
    /// [`io::ErrorKind::IsADirectory`] if `from` or `to` is a directory; any
    /// I/O error, including a missing `from`.
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
    /// (durable or not). Only a regular file is read, not followed through
    /// a symlink, and only up to [`MAX_READ_LEN`] bytes, so whatever is put
    /// under a name in a session directory can't hang the reader or run it
    /// out of memory.
    ///
    /// # Errors
    ///
    /// Any I/O error, including [`io::ErrorKind::NotFound`];
    /// [`io::ErrorKind::IsADirectory`] for a directory, an error of kind
    /// [`io::ErrorKind::InvalidInput`] for anything else that isn't a regular
    /// file (a symlink may give the kernel's `ELOOP` instead), and
    /// [`io::ErrorKind::FileTooLarge`] for a file longer than
    /// [`MAX_READ_LEN`].
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// The paths of the files and directories in `dir`, sorted, as the
    /// running system sees them.
    ///
    /// # Errors
    ///
    /// Any I/O error, including [`io::ErrorKind::NotFound`].
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;

    /// Locks the directory `dir` for whoever holds the returned guard, or
    /// refuses at once if anyone else holds it: another process, or another
    /// guard in this one (the real filesystem uses `flock`, which is held
    /// per open of the directory). The lock is advisory: it keeps out only
    /// those who ask for it. It lasts until the guard drops or the process
    /// ends, crash included.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::WouldBlock`] if the lock is held; any I/O error,
    /// including a missing directory or one that's a file.
    fn lock_dir(&self, dir: &Path) -> io::Result<Self::Lock>;
}

/// The longest file [`Fs::read`] reads: 1 GiB. Nothing nota writes comes
/// near it: a journal is at most one segment window of one track (about
/// 10 MB for five minutes at 16 kHz), and a segment is that window's audio
/// as FLAC.
pub const MAX_READ_LEN: u64 = 1 << 30;

/// A file opened by [`Fs::create`].
pub trait FsFile: Send + fmt::Debug {
    /// A handle that fsyncs this file from any thread ([`Self::syncer`]).
    type Syncer: FileSyncer;

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
    /// successful one can be assumed durable, even after a later fsync
    /// succeeds: the kernel may have dropped those bytes from its write-back
    /// while they still read back. Treat the file as broken.
    fn sync(&mut self) -> io::Result<Synced>;

    /// A handle that fsyncs this file, from any thread, while it's still
    /// being written here. Each of its fsyncs covers at least what was
    /// written before that fsync started.
    ///
    /// # Errors
    ///
    /// Any I/O error (the real filesystem duplicates the file descriptor).
    fn syncer(&self) -> io::Result<Self::Syncer>;
}

/// Fsyncs one file from any thread: from [`FsFile::syncer`].
pub trait FileSyncer: Send + Sync + fmt::Debug + 'static {
    /// As [`FsFile::sync`]: waits until everything written to the file
    /// before the call is on disk, and returns the proof.
    ///
    /// # Errors
    ///
    /// As [`FsFile::sync`]: after an error, treat the file as broken.
    fn sync(&self) -> io::Result<Synced>;
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

/// Checks a directory to sync or list: the root, or a [`valid_path`].
fn valid_dir(dir: &Path) -> io::Result<()> {
    if dir.has_root() && dir.parent().is_none() {
        Ok(())
    } else {
        valid_path(dir)
    }
}

/// As Linux's read(2), unlink(2) and rename(2) onto a directory: `EISDIR`.
/// [`Fs::rename`] refuses a directory to move with it too.
fn is_a_directory() -> io::Error {
    io::Error::new(io::ErrorKind::IsADirectory, "is a directory")
}

/// A file longer than [`MAX_READ_LEN`], as [`Fs::read`] refuses it.
fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        "longer than any file nota writes",
    )
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
