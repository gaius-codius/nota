//! The real filesystem, through `std::fs`. The one module allowed to call
//! its write functions; each call says so with `#[expect]`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use super::{Fs, FsFile, Synced, same_directory};

/// The real filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFs;

/// A file opened by [`StdFs::create`].
#[derive(Debug)]
pub struct StdFile(File);

impl Fs for StdFs {
    type File = StdFile;

    #[expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that opens files for writing"
    )]
    fn create(&self, path: &Path) -> io::Result<StdFile> {
        OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(path)
            .map(StdFile)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that renames files"
    )]
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        same_directory(from, to)?;
        std::fs::rename(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        sync_dir(dir)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the durable-write layer is the one place that removes files"
    )]
    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

impl FsFile for StdFile {
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
}

/// Fsyncs a directory, making the names in it durable.
#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "the durable-write layer is the one place that fsyncs"
)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
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

    /// A fresh directory under the target dir, removed when dropped. Tests
    /// can't use a temp-dir crate without a new dependency.
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("nota-recorder-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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
    fn rename_refuses_to_change_directory() {
        let dir = TestDir::new("rename");
        let sub = dir.0.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let fs = StdFs;
        let from = dir.0.join("a");
        let _file = fs.create(&from).unwrap();
        assert_eq!(
            fs.rename(&from, &sub.join("a")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(fs.read(&from).is_ok());
    }

    #[test]
    fn sync_dir_fails_on_a_missing_directory() {
        let dir = TestDir::new("missing");
        assert!(StdFs.sync_dir(&dir.0.join("nope")).is_err());
    }
}
