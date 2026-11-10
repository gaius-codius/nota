//! The library's recording lock: whether any session in a library is being
//! recorded, in this process or another.
//!
//! A session's own [`SessionLock`](super::SessionLock) keeps out whoever
//! would touch *that* session, but playing a moment of one session while
//! another records would put the speakers into the recording's tracks. So a
//! library has one more lock, on the directory [`RECORDING_LOCK`] in its
//! data folder (not the data folder itself, which the ballast locks; see
//! [`disk`](crate::disk)):
//!
//! - A recording takes it shared, with [`Recordings::record`], and holds it
//!   for the whole recording. Recordings don't keep each other out: two
//!   notas may record into one library at once.
//! - Playback takes it exclusively, with [`Recordings::quiet`], and holds
//!   it while the moment plays: refused while any recording holds it, and
//!   while it's held no recording can start.
//!
//! Both refuse at once, never wait, and end with their guard or the
//! process (an `flock`; see [`Fs::lock_dir_shared`]). The directory is made
//! the first time either is taken, so a library from before the lock needs
//! nothing done to it.
//!
//! It covers one library. A recording into another data folder isn't seen,
//! although it hears the same speakers.

use std::io;
use std::path::{Path, PathBuf};

use crate::fs::Fs;

/// The name of the directory, in a library's data folder, that is locked
/// while a session in the library records.
pub const RECORDING_LOCK: &str = "recording.lock";

/// The recording lock of the library in one data folder. Making it does no
/// I/O.
#[derive(Debug, Clone)]
pub struct Recordings<S> {
    fs: S,
    /// The lock's directory, [`RECORDING_LOCK`] in the data folder.
    dir: PathBuf,
}

impl<S: Fs> Recordings<S> {
    /// The recording lock of the library whose data folder is `data`, on
    /// `fs`.
    #[must_use]
    pub fn new(fs: S, data: &Path) -> Self {
        Self {
            fs,
            dir: data.join(RECORDING_LOCK),
        }
    }

    /// The directory locked.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Counts a recording as running in the library until the returned
    /// guard drops: alongside any other recording, never while a moment
    /// plays (see the module docs).
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::WouldBlock`] while a moment plays; any I/O error,
    /// including a missing data folder.
    pub fn record(&self) -> io::Result<RecordingLock<S>> {
        let guard = self.take(|fs, dir| fs.lock_dir_shared(dir))?;
        Ok(RecordingLock { _guard: guard })
    }

    /// Keeps every recording in the library from starting until the
    /// returned guard drops, if none is running now.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::WouldBlock`] while a recording runs, or while
    /// another [`QuietLock`] is held; any I/O error, including a missing
    /// data folder.
    pub fn quiet(&self) -> io::Result<QuietLock<S>> {
        let guard = self.take(|fs, dir| fs.lock_dir(dir))?;
        Ok(QuietLock { _guard: guard })
    }

    /// Takes the lock with `lock`, making its directory first if it isn't
    /// there yet.
    fn take(&self, lock: impl Fn(&S, &Path) -> io::Result<S::Lock>) -> io::Result<S::Lock> {
        match lock(&self.fs, &self.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match self.fs.create_dir(&self.dir) {
                    // Another nota may make it at the same moment.
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
                // Not synced: a lock directory lost in a crash is made
                // again by the next taker, and holds nothing.
                lock(&self.fs, &self.dir)
            }
            taken => taken,
        }
    }
}

/// A recording counted in its library ([`Recordings::record`]); dropping it
/// lets a moment play again once no other recording holds the lock.
#[derive(Debug)]
pub struct RecordingLock<S: Fs> {
    _guard: S::Lock,
}

/// No recording running in its library, and none able to start, until it's
/// dropped ([`Recordings::quiet`]).
#[derive(Debug)]
pub struct QuietLock<S: Fs> {
    _guard: S::Lock,
}

#[cfg(test)]
mod tests {
    use crate::fs::fake::FakeFs;

    use super::*;

    fn library() -> Recordings<FakeFs> {
        Recordings::new(FakeFs::with_dirs(["/data"]), Path::new("/data"))
    }

    /// Two recordings into one library both start: neither keeps the other
    /// out.
    #[test]
    fn recordings_share_the_library() {
        let recordings = library();
        let _first = recordings.record().unwrap();
        let _second = recordings.record().unwrap();
    }

    /// Quiet is refused while any recording runs, the last one included,
    /// and taken once they've all stopped.
    #[test]
    fn quiet_is_refused_until_every_recording_stops() {
        let recordings = library();
        let first = recordings.record().unwrap();
        let second = recordings.record().unwrap();
        assert_eq!(
            recordings.quiet().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        // One recording is still running.
        assert_eq!(
            recordings.quiet().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(second);
        let _quiet = recordings.quiet().unwrap();
    }

    /// While quiet is held, no recording starts, and only one can be quiet.
    #[test]
    fn a_recording_cant_start_while_quiet_is_held() {
        let recordings = library();
        let quiet = recordings.quiet().unwrap();
        assert_eq!(
            recordings.record().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            recordings.quiet().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(quiet);
        let _recording = recordings.record().unwrap();
    }

    /// The lock's directory is made in the data folder the first time it's
    /// needed, by either side, and left there.
    #[test]
    fn the_lock_directory_is_made_when_first_needed() {
        for take in [
            |r: &Recordings<FakeFs>| r.record().map(drop),
            |r: &Recordings<FakeFs>| r.quiet().map(drop),
        ] {
            let recordings = library();
            assert!(recordings.fs.list(Path::new("/data")).unwrap().is_empty());
            take(&recordings).unwrap();
            assert_eq!(
                recordings.fs.list(Path::new("/data")).unwrap(),
                [PathBuf::from("/data/recording.lock")]
            );
            // A second take finds it there.
            take(&recordings).unwrap();
        }
    }

    /// Without a data folder there is nowhere to make the lock: the error
    /// is the filesystem's.
    #[test]
    fn a_missing_data_folder_is_an_error() {
        let recordings = Recordings::new(FakeFs::new(), Path::new("/data"));
        assert_eq!(
            recordings.record().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            recordings.quiet().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
