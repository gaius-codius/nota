//! The real filesystem, through `std::fs`. The one module allowed to call
//! its write functions; each call says so with `#[expect]`.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{Fs, FsFile, Synced, same_directory, valid_path};

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

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let mut entries = std::fs::read_dir(dir)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort();
        Ok(entries)
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
}
