//! The real filesystem, through `std::fs`. The one module allowed to call
//! its write functions; each call says so with `#[expect]`.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::{
    FileSyncer, Fs, FsFile, MAX_READ_LEN, Synced, is_a_directory, same_directory, too_large,
    valid_dir, valid_path,
};

/// The real filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFs;

/// A file opened by [`StdFs::create`].
#[derive(Debug)]
pub struct StdFile(File);

/// A directory locked by [`StdFs::lock_dir`]: an open handle on it holding
/// an exclusive `flock`, released when the handle closes.
#[derive(Debug)]
pub struct StdLock {
    _handle: File,
}

impl Fs for StdFs {
    type File = StdFile;
    type Lock = StdLock;

    #[expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that opens files for writing"
    )]
    fn create(&self, path: &Path) -> io::Result<StdFile> {
        valid_path(path)?;
        let mut options = OpenOptions::new();
        options.append(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(path).map(StdFile)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that creates directories"
    )]
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        valid_path(path)?;
        let mut builder = DirBuilder::new();
        // One level only: a missing parent is an error, as on the fake.
        builder.recursive(false);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(path)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that renames files"
    )]
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        same_directory(from, to)?;
        // Linux would move a directory too; the fake doesn't model that.
        if std::fs::symlink_metadata(from)?.is_dir() {
            return Err(is_a_directory());
        }
        std::fs::rename(from, to)
    }

    fn rename_new(&self, from: &Path, to: &Path) -> io::Result<()> {
        same_directory(from, to)?;
        if std::fs::symlink_metadata(from)?.is_dir() {
            return Err(is_a_directory());
        }
        rename_new(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        valid_dir(dir)?;
        sync_dir(dir)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that removes files"
    )]
    fn remove(&self, path: &Path) -> io::Result<()> {
        valid_path(path)?;
        std::fs::remove_file(path)
    }

    /// Removes an empty directory through the system filesystem.
    #[expect(
        clippy::disallowed_methods,
        reason = "the filesystem layer is the one place that removes empty directories"
    )]
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        valid_path(path)?;
        std::fs::remove_dir(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        read_at_most(path, MAX_READ_LEN)
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        valid_path(path)?;
        sync_file(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        valid_dir(dir)?;
        let mut entries = std::fs::read_dir(dir)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort();
        Ok(entries)
    }

    fn lock_dir(&self, dir: &Path) -> io::Result<StdLock> {
        valid_path(dir)?;
        let handle = File::open(dir)?;
        if !handle.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "lock_dir needs a directory",
            ));
        }
        // `flock(LOCK_EX | LOCK_NB)` on Unix: held per open file
        // description, so a second open in this process is refused too.
        match handle.try_lock() {
            Ok(()) => Ok(StdLock { _handle: handle }),
            Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the directory is locked",
            )),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }

    fn free_space(&self, dir: &Path) -> io::Result<u64> {
        valid_dir(dir)?;
        free_space(dir)
    }
}

/// `statvfs(2)`: the blocks free to an unprivileged user (`f_bavail`, not
/// `f_bfree`, which counts the blocks kept for root) times the fragment
/// size they're counted in. A filesystem that reports no blocks at all (a
/// FUSE one without `statfs`) can't tell. Elsewhere than Unix nota can't
/// tell yet, and the disk check reports no figure.
fn free_space(dir: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        let stat = rustix::fs::statvfs(dir)?;
        if stat.f_blocks == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the filesystem doesn't say how much space it has",
            ));
        }
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "free space isn't read on this platform yet",
        ))
    }
}

/// [`Fs::read`], refusing a file longer than `limit`.
fn read_at_most(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    valid_path(path)?;
    let file = open_to_read(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(is_a_directory());
    }
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if meta.len() > limit {
        return Err(too_large());
    }
    // The length is a hint: the file may grow or shrink meanwhile, so the
    // read itself stops one byte past the limit.
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_or(true, |n| n > limit) {
        return Err(too_large());
    }
    Ok(bytes)
}

/// Opens `path` to read without following a symlink at its last component
/// or waiting on it: on Unix `O_NOFOLLOW | O_NONBLOCK`, so a FIFO with no
/// writer opens at once (and is then refused as not a regular file) rather
/// than blocking. `O_NONBLOCK` changes nothing for a regular file. Elsewhere
/// the type check after it is all the protection there is.
#[cfg_attr(
    unix,
    expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that opens files; this opens one read-only"
    )
)]
fn open_to_read(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};
        let flags = OFlags::from_iter([
            OFlags::RDONLY,
            OFlags::NOFOLLOW,
            OFlags::NONBLOCK,
            OFlags::CLOEXEC,
        ]);
        rustix::fs::open(path, flags, Mode::empty())
            .map(File::from)
            .map_err(io::Error::from)
    }
    #[cfg(not(unix))]
    {
        File::open(path)
    }
}

impl FsFile for StdFile {
    type Syncer = StdSyncer;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that fsyncs, so only it can make a `Synced`"
    )]
    fn sync(&mut self) -> io::Result<Synced> {
        // fsync. On macOS (v2) it doesn't flush the drive's cache; that
        // platform will need F_FULLFSYNC here.
        self.0.sync_all()?;
        Ok(Synced::after_fsync())
    }

    fn syncer(&self) -> io::Result<StdSyncer> {
        self.0.try_clone().map(StdSyncer)
    }
}

/// Fsyncs a [`StdFile`] from another thread, through its own file
/// descriptor for the same open file.
#[derive(Debug)]
pub struct StdSyncer(File);

impl FileSyncer for StdSyncer {
    #[expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that fsyncs, so only it can make a `Synced`"
    )]
    fn sync(&self) -> io::Result<Synced> {
        // fsync flushes the file, whichever descriptor asks: every write
        // that returned before it, through any of them, is covered.
        self.0.sync_all()?;
        Ok(Synced::after_fsync())
    }
}

/// Fsyncs a directory, making the names in it durable.
#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "the durable-write layer is the one place that fsyncs"
)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    let handle = File::open(dir)?;
    // Fsyncing a file by mistake would succeed and make no name durable.
    if !handle.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "sync_dir needs a directory",
        ));
    }
    handle.sync_all()
}

/// Renames `from` to `to` unless something is at `to`, atomically:
/// `renameat2(RENAME_NOREPLACE)`, which fails with `EEXIST` then.
/// Elsewhere than Linux nota has no atomic way not to replace: it refuses.
#[cfg_attr(
    target_os = "linux",
    expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that renames files"
    )
)]
fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        Ok(renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE)?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (from, to);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "renaming without replacing needs Linux",
        ))
    }
}

/// Fsyncs the regular file at `path`, opened read-only as [`open_to_read`]
/// opens it: Linux syncs a file's data through any descriptor. (Windows
/// needs write access to flush; there this fails, and nothing that needs
/// it runs.)
#[expect(
    clippy::disallowed_methods,
    reason = "the durable-write layer is the one place that fsyncs"
)]
fn sync_file(path: &Path) -> io::Result<()> {
    let file = open_to_read(path)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        return Err(is_a_directory());
    }
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    file.sync_all()
}

/// Windows can't open a directory as a `File`, and NTFS journals its
/// metadata, so there's nothing to sync. nota v1 records on Linux only; this
/// keeps the other platforms building.
#[cfg(not(unix))]
fn sync_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::NotFound, "no such directory"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;

    /// Directory removal refuses a file without removing it.
    #[test]
    fn remove_dir_refuses_a_file() {
        let dir = TestDir::new("remove-file");
        let file = dir.0.join("file");
        drop(StdFs.create(&file).unwrap());
        // A file passed to directory cleanup must remain untouched.
        assert_eq!(
            StdFs.remove_dir(&file).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(StdFs.list(&dir.0).unwrap(), [file]);
    }

    /// Directory removal refuses a directory containing a file.
    #[test]
    fn remove_dir_refuses_a_nonempty_directory() {
        let dir = TestDir::new("remove-nonempty");
        let child = dir.0.join("child");
        StdFs.create_dir(&child).unwrap();
        drop(StdFs.create(&child.join("file")).unwrap());
        // A live session's files must stop empty-directory cleanup.
        assert_eq!(
            StdFs.remove_dir(&child).unwrap_err().kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
    }

    /// Removing an empty directory removes its name from the parent.
    #[test]
    fn remove_dir_removes_an_empty_directory() {
        let dir = TestDir::new("remove-empty");
        let child = dir.0.join("child");
        StdFs.create_dir(&child).unwrap();
        // Sync the parent exactly as startup rollback does after removal.
        StdFs.remove_dir(&child).unwrap();
        StdFs.sync_dir(&dir.0).unwrap();
        assert!(StdFs.list(&dir.0).unwrap().is_empty());
        assert_eq!(
            StdFs.remove_dir(&child).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    /// Empty-directory cleanup refuses the filesystem root.
    #[test]
    fn remove_dir_refuses_the_root() {
        // A path without a parent cannot be a session's empty scaffolding.
        assert_eq!(
            StdFs.remove_dir(Path::new("/")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn create_write_sync_rename_read_remove() {
        let dir = TestDir::new("ops");
        let tmp = dir.0.join("seg.flac.tmp");
        let done = dir.0.join("seg.flac");
        let fs = StdFs;

        let mut file = fs.create(&tmp).unwrap();
        file.write_all(b"hello ").unwrap();
        file.write_all(b"world").unwrap();
        file.sync().unwrap();
        fs.sync_dir(&dir.0).unwrap();
        assert_eq!(fs.read(&tmp).unwrap(), b"hello world");

        fs.rename(&tmp, &done).unwrap();
        fs.sync_dir(&dir.0).unwrap();
        assert_eq!(fs.read(&done).unwrap(), b"hello world");
        assert_eq!(fs.read(&tmp).unwrap_err().kind(), io::ErrorKind::NotFound);

        fs.remove(&done).unwrap();
        assert_eq!(fs.read(&done).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    /// `statvfs` on a real directory: some space, and shrinking by about
    /// what a written file takes. Other processes write too, so only
    /// roughly.
    #[cfg(unix)]
    #[test]
    fn free_space_reads_statvfs() {
        let dir = TestDir::new("free");
        let fs = StdFs;
        let before = fs.free_space(&dir.0).unwrap();
        // Any disk the tests run on has a megabyte free.
        assert!(before > 1 << 20, "{before}");
        assert_eq!(
            fs.free_space(&dir.0.join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(fs.free_space(Path::new("journal")).is_err());
    }

    #[test]
    fn create_refuses_an_existing_file() {
        let dir = TestDir::new("exists");
        let path = dir.0.join("journal");
        let fs = StdFs;
        let _first = fs.create(&path).unwrap();
        assert_eq!(
            fs.create(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn a_file_syncs_by_name_but_nothing_else_does() {
        let dir = TestDir::new("sync-file");
        let fs = StdFs;
        let path = dir.0.join("kept");
        let mut file = fs.create(&path).unwrap();
        file.write_all(b"abc").unwrap();
        fs.sync_file(&path).unwrap();
        assert_eq!(fs.read(&path).unwrap(), b"abc");
        assert_eq!(
            fs.sync_file(&dir.0).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(
            fs.sync_file(&dir.0.join("none")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn renaming_without_replacing_keeps_what_is_there() {
        let dir = TestDir::new("rename-new");
        let fs = StdFs;
        let (a, b, c) = (dir.0.join("a"), dir.0.join("b"), dir.0.join("c"));
        for (path, body) in [(&a, b"a"), (&b, b"b")] {
            fs.create(path).unwrap().write_all(body).unwrap();
        }
        assert_eq!(
            fs.rename_new(&a, &b).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs.read(&a).unwrap(), b"a");
        assert_eq!(fs.read(&b).unwrap(), b"b");
        fs.rename_new(&a, &c).unwrap();
        assert_eq!(fs.read(&c).unwrap(), b"a");
        assert_eq!(fs.read(&a).unwrap_err().kind(), io::ErrorKind::NotFound);
        let sub = dir.0.join("sub");
        fs.create_dir(&sub).unwrap();
        assert_eq!(
            fs.rename_new(&c, &sub.join("c")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            fs.rename_new(&sub, &dir.0.join("d")).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
    }

    #[test]
    fn rename_refuses_to_change_directory() {
        let dir = TestDir::new("rename");
        let sub = dir.0.join("sub");
        let fs = StdFs;
        fs.create_dir(&sub).unwrap();
        let from = dir.0.join("a");
        let _file = fs.create(&from).unwrap();
        assert_eq!(
            fs.rename(&from, &sub.join("a")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(fs.read(&from).is_ok());
    }

    #[test]
    fn directories_listing_and_private_modes() {
        let dir = TestDir::new("dirs");
        let fs = StdFs;
        let session = dir.0.join("session");
        fs.create_dir(&session).unwrap();
        assert_eq!(
            fs.create_dir(&session).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        fs.sync_dir(&dir.0).unwrap();
        let _b = fs.create(&session.join("b")).unwrap();
        let _a = fs.create(&session.join("a")).unwrap();
        assert_eq!(
            fs.list(&session).unwrap(),
            [session.join("a"), session.join("b")]
        );
        assert_eq!(fs.list(&dir.0).unwrap(), std::slice::from_ref(&session));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&session), 0o700);
            assert_eq!(mode(&session.join("a")), 0o600);
        }
        assert!(fs.list(&dir.0.join("nope")).is_err());
        assert!(fs.create_dir(&dir.0.join("no/parent")).is_err());
    }

    #[test]
    fn bare_relative_names_are_refused() {
        let fs = StdFs;
        for path in ["journal", "/"] {
            assert_eq!(
                fs.create(Path::new(path)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{path}"
            );
        }
        assert_eq!(
            fs.create_dir(Path::new("session")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            fs.rename(Path::new("a"), Path::new("b"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let bare = Path::new("journal");
        let kinds = [
            fs.remove(bare).unwrap_err().kind(),
            fs.read(bare).unwrap_err().kind(),
            fs.list(bare).unwrap_err().kind(),
            fs.sync_dir(bare).unwrap_err().kind(),
            fs.lock_dir(bare).unwrap_err().kind(),
        ];
        assert_eq!(kinds, [io::ErrorKind::InvalidInput; 5]);
        assert_eq!(
            fs.list(Path::new("/tmp/..")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        // The root is a directory to sync or list, though not a name.
        fs.sync_dir(Path::new("/")).unwrap();
        assert!(fs.list(Path::new("/")).is_ok());
        assert_eq!(
            fs.remove(Path::new("/")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_locked_directory_refuses_a_second_lock_until_released() {
        let dir = TestDir::new("lock");
        let fs = StdFs;
        let held = fs.lock_dir(&dir.0).unwrap();
        // A second open of the directory, as another process would have.
        assert_eq!(
            fs.lock_dir(&dir.0).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(held);
        let _again = fs.lock_dir(&dir.0).unwrap();
    }

    #[test]
    fn lock_dir_needs_an_existing_directory() {
        let dir = TestDir::new("lock-missing");
        assert_eq!(
            StdFs.lock_dir(&dir.0.join("nope")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let file = dir.0.join("journal");
        let _f = StdFs.create(&file).unwrap();
        assert_eq!(
            StdFs.lock_dir(&file).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
    }

    #[test]
    fn sync_dir_fails_on_a_missing_directory_or_a_file() {
        let dir = TestDir::new("missing");
        assert!(StdFs.sync_dir(&dir.0.join("nope")).is_err());
        let file = dir.0.join("journal");
        let _f = StdFs.create(&file).unwrap();
        assert_eq!(
            StdFs.sync_dir(&file).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
    }

    #[test]
    fn a_syncer_fsyncs_the_same_file_from_another_thread() {
        let dir = TestDir::new("syncer");
        let path = dir.0.join("journal");
        let mut file = StdFs.create(&path).unwrap();
        let syncer = file.syncer().unwrap();
        file.write_all(b"before").unwrap();
        std::thread::spawn(move || syncer.sync().map(|_| ()))
            .join()
            .unwrap()
            .unwrap();
        file.write_all(b" after").unwrap();
        assert_eq!(StdFs.read(&path).unwrap(), b"before after");
    }

    #[cfg(unix)]
    #[expect(
        clippy::disallowed_methods,
        reason = "a test plants a symlink in its own scratch directory"
    )]
    fn symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    /// A FIFO with no writer, a symlink and a directory under names nota
    /// reads are refused at once: never waited on, never followed.
    #[cfg(unix)]
    #[test]
    fn read_refuses_what_isnt_a_regular_file_without_blocking() {
        let dir = TestDir::new("not-regular");
        let fs = StdFs;
        let fifo = dir.0.join("salvage-findings");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let target = dir.0.join("journal-000001");
        let mut file = fs.create(&target).unwrap();
        file.write_all(b"audio").unwrap();
        let link = dir.0.join("journal-000002");
        symlink(&target, &link);

        // On a thread, so a read that blocks fails the test instead of
        // hanging it.
        let (sent, got) = std::sync::mpsc::channel();
        let (fifo_path, link_path) = (fifo, link);
        let _reader = std::thread::spawn(move || {
            for path in [fifo_path, link_path] {
                let _ = sent.send(StdFs.read(&path).map_err(|e| e.kind()));
            }
        });
        let within = std::time::Duration::from_secs(10);
        let from_fifo = got.recv_timeout(within).expect("reading a FIFO blocked");
        assert_eq!(from_fifo, Err(io::ErrorKind::InvalidInput));
        let through_link = got.recv_timeout(within).unwrap();
        assert!(through_link.is_err(), "{through_link:?}");
        assert_eq!(
            fs.read(&dir.0.join("..").join(dir.0.file_name().unwrap()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(fs.read(&target).unwrap(), b"audio");
    }

    #[test]
    fn read_refuses_a_file_longer_than_its_limit() {
        let dir = TestDir::new("limit");
        let fs = StdFs;
        let path = dir.0.join("journal-000001");
        let mut file = fs.create(&path).unwrap();
        file.write_all(b"12345").unwrap();
        assert_eq!(read_at_most(&path, 5).unwrap(), b"12345");
        assert_eq!(
            read_at_most(&path, 4).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        // A file whose length says nothing (as /proc's say 0) is cut off by
        // the read itself.
        let proc = Path::new("/proc/self/status");
        if proc.exists() {
            assert_eq!(
                read_at_most(proc, 4).unwrap_err().kind(),
                io::ErrorKind::FileTooLarge
            );
            assert!(read_at_most(proc, MAX_READ_LEN).is_ok());
        }
    }
}
