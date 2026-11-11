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
//! An `flock` can't say whether anyone holds it shared, short of taking it
//! exclusively, which would refuse a recording that starts at that moment.
//! So each recording also makes an entry of its own, a directory inside the
//! lock's, and holds it exclusively: [`Recordings::running`] asks whether a
//! recording runs by trying each entry, never the lock itself, so it keeps
//! out neither a recording nor a moment playing. An entry is locked before
//! the shared lock is taken and stays locked until after it's let go, so
//! while a recording holds the lock its entry is always seen. A check can
//! only hold an entry its recording hasn't locked yet, and that recording
//! then makes another. An entry is removed as its recording ends; one left
//! by a crash is an empty directory nobody holds, read as no recording.
//!
//! It covers one library. A recording into another data folder isn't seen,
//! although it hears the same speakers, and neither is a [`SessionWriter`]
//! opened without the lock (the recorder's own examples) or a nota from
//! before the entries.
//!
//! [`SessionWriter`]: super::SessionWriter

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fs::Fs;

/// The name of the directory, in a library's data folder, that is locked
/// while a session in the library records.
pub const RECORDING_LOCK: &str = "recording.lock";

/// How many entries a recording makes before giving up: each try after the
/// first follows a name already there or a check holding the entry it just
/// made, both rare.
const ENTRY_TRIES: usize = 64;

/// The number in the next entry this process makes, so its entries' names
/// differ (see [`Recordings::enter`]).
static NEXT_ENTRY: AtomicU64 = AtomicU64::new(0);

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
    pub fn record(&self) -> io::Result<RecordingLock<S>>
    where
        S: Clone,
    {
        // The entry first, so a check sees the recording for as long as
        // it holds the shared lock. Refused, the entry goes with it.
        let entry = self.enter()?;
        let shared = self.take(S::lock_dir_shared)?;
        Ok(RecordingLock {
            _shared: shared,
            _entry: entry,
        })
    }

    /// Whether a recording other than those holding the entries in `own`
    /// runs in the library: one whose entry is locked. It locks nothing a
    /// recording or a moment playing needs, and changes nothing on disk.
    /// `own` is for a caller that counts its own recording: the paths it
    /// locked through a [`DiskWatch`](crate::disk::DiskWatch)'s filesystem,
    /// say. A library that has never recorded has no entries.
    ///
    /// # Errors
    ///
    /// Any I/O error listing the lock's directory, or trying an entry,
    /// other than its being missing.
    pub fn running(&self, own: &[PathBuf]) -> io::Result<bool> {
        let entries = match self.fs.list(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            listed => listed?,
        };
        for entry in entries.iter().filter(|entry| !own.contains(entry)) {
            match self.fs.lock_dir(entry) {
                // Nobody holds it: its recording ended in a crash, or
                // hasn't locked it yet and will make another.
                Ok(_unheld) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                // Removed as its recording ended, or not an entry at all.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(false)
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
        let guard = self.take(S::lock_dir)?;
        Ok(QuietLock { _guard: guard })
    }

    /// Takes the lock with `lock`, making its directory first if it isn't
    /// there yet.
    fn take(&self, lock: impl Fn(&S, &Path) -> io::Result<S::Lock>) -> io::Result<S::Lock> {
        match lock(&self.fs, &self.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.make_dir()?;
                lock(&self.fs, &self.dir)
            }
            taken => taken,
        }
    }

    /// Makes this recording's entry in the lock's directory, making that
    /// first if it isn't there, and locks it exclusively. A name already
    /// there (a crash's, or another process's with the same id in another
    /// namespace) and an entry a check holds at the moment it's locked are
    /// passed over for the next name.
    fn enter(&self) -> io::Result<Entry<S>>
    where
        S: Clone,
    {
        self.make_dir()?;
        let mut passed = None;
        for _ in 0..ENTRY_TRIES {
            let number = NEXT_ENTRY.fetch_add(1, Ordering::Relaxed);
            let dir = self.dir.join(format!("{}-{number}", std::process::id()));
            match self.make_entry(dir) {
                Ok(entry) => return Ok(entry),
                // A name already there, or a check trying the new entry:
                // it's let go at once, but this recording mustn't wait.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::WouldBlock
                    ) =>
                {
                    passed = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(passed.unwrap_or_else(|| io::Error::other("no entry was tried")))
    }

    /// Makes the entry `dir` and locks it exclusively; an entry made but
    /// not locked is removed.
    fn make_entry(&self, dir: PathBuf) -> io::Result<Entry<S>>
    where
        S: Clone,
    {
        self.fs.create_dir(&dir)?;
        match self.fs.lock_dir(&dir) {
            Ok(lock) => Ok(Entry {
                _lock: lock,
                fs: self.fs.clone(),
                dir,
            }),
            Err(e) => {
                let _left = self.fs.remove_dir(&dir);
                Err(e)
            }
        }
    }

    /// Makes the lock's directory if it isn't there yet.
    fn make_dir(&self) -> io::Result<()> {
        match self.fs.create_dir(&self.dir) {
            // Another nota may make it at the same moment.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            // Not synced: a lock directory lost in a crash is made again by
            // the next taker, and holds nothing.
            made => made,
        }
    }
}

/// A recording counted in its library ([`Recordings::record`]); dropping it
/// lets a moment play again once no other recording holds the lock.
#[derive(Debug)]
pub struct RecordingLock<S: Fs> {
    /// The library's lock, held shared. Declared first, so it's let go
    /// before the entry: until then, a check sees the recording.
    _shared: S::Lock,
    /// The recording's own entry, held exclusively.
    _entry: Entry<S>,
}

/// One recording's entry in the lock's directory, locked exclusively, and
/// removed as it drops (see the module docs).
#[derive(Debug)]
struct Entry<S: Fs> {
    /// The entry's lock, let go only after the entry is removed.
    _lock: S::Lock,
    /// The filesystem the entry is on.
    fs: S,
    /// The entry.
    dir: PathBuf,
}

impl<S: Fs> Drop for Entry<S> {
    fn drop(&mut self) {
        // Removed while still locked, so no check finds it unheld while
        // the recording runs. Left behind, it reads as no recording.
        let _left = self.fs.remove_dir(&self.dir);
    }
}

/// No recording running in its library, and none able to start, until it's
/// dropped ([`Recordings::quiet`]).
#[derive(Debug)]
pub struct QuietLock<S: Fs> {
    _guard: S::Lock,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use crate::fs::fake::{FakeFile, FakeFs, FakeLock};

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

    /// A directory another nota made first (`create_dir` finds it there)
    /// is no error: the lock is tried again, and its answer is the one
    /// given. Here nothing was made after all, so that answer is
    /// `NotFound`.
    #[test]
    fn a_lock_directory_made_meanwhile_is_tried_again() {
        let recordings = library();
        recordings.fs.fail_after(0, io::ErrorKind::AlreadyExists);
        assert_eq!(
            recordings.quiet().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    /// A lock directory that can't be made is that error.
    #[test]
    fn a_lock_directory_that_cant_be_made_is_its_error() {
        let recordings = library();
        recordings.fs.fail_after(0, io::ErrorKind::PermissionDenied);
        assert_eq!(
            recordings.quiet().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
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

    /// The lock's directory in [`library`].
    fn lock_dir() -> PathBuf {
        PathBuf::from("/data/recording.lock")
    }

    /// The entries in the lock's directory on `fs`.
    fn entries(fs: &impl Fs) -> Vec<PathBuf> {
        fs.list(&lock_dir()).unwrap()
    }

    /// A counted recording is running from the moment `record` returns,
    /// before it has made anything else, and not once it's dropped.
    #[test]
    fn a_counted_recording_is_running_until_it_drops() {
        let recordings = library();
        let recording = recordings.record().unwrap();
        assert!(recordings.running(&[]).unwrap());
        drop(recording);
        assert!(!recordings.running(&[]).unwrap());
    }

    /// Asking a library that has never recorded finds nothing running and
    /// leaves its data folder as it was: a check makes nothing.
    #[test]
    fn a_library_that_never_recorded_has_nothing_running() {
        let recordings = library();
        assert!(!recordings.running(&[]).unwrap());
        assert!(recordings.fs.list(Path::new("/data")).unwrap().is_empty());
    }

    /// A moment playing holds the lock exclusively, but it's no recording.
    #[test]
    fn a_moment_playing_is_not_a_recording() {
        let recordings = library();
        let _quiet = recordings.quiet().unwrap();
        assert!(!recordings.running(&[]).unwrap());
    }

    /// A check made while a recording runs, or while a moment plays,
    /// never asks for the library's lock: so it can't keep a recording from
    /// starting, or a moment from playing, at the moment it runs.
    #[test]
    fn a_check_never_asks_for_the_library_lock() {
        let fs = Meddling::new();
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        let recording = recordings.record().unwrap();
        fs.forget_locks();
        assert!(recordings.running(&[]).unwrap());
        let during_record = fs.locks();
        drop(recording);
        let _quiet = recordings.quiet().unwrap();
        // Only the check's locks from here.
        fs.forget_locks();
        assert!(!recordings.running(&[]).unwrap());
        let asked: Vec<_> = during_record.into_iter().chain(fs.locks()).collect();
        assert!(asked.iter().all(|(dir, _)| *dir != lock_dir()), "{asked:?}");
    }

    /// Each recording holds an entry of its own, removed as it stops.
    #[test]
    fn each_recording_has_its_own_entry_until_it_stops() {
        let recordings = library();
        let first = recordings.record().unwrap();
        let second = recordings.record().unwrap();
        assert_eq!(entries(&recordings.fs).len(), 2);
        drop(first);
        drop(second);
        assert_eq!(entries(&recordings.fs), Vec::<PathBuf>::new());
    }

    /// A recording's own entry is passed over: with only it running, a
    /// check that names it finds nothing else, and with a second, it does.
    #[test]
    fn a_check_passes_over_the_entries_it_is_given() {
        let recordings = library();
        let _own = recordings.record().unwrap();
        let own = entries(&recordings.fs);
        assert!(!recordings.running(&own).unwrap());
        let _other = recordings.record().unwrap();
        assert!(recordings.running(&own).unwrap());
    }

    /// An entry left by a crash, which nobody holds, is no recording; nor
    /// is a stray file in the lock's directory.
    #[test]
    fn what_a_crash_or_a_stray_file_leaves_is_not_running() {
        let recordings = library();
        recordings.fs.create_dir(&lock_dir()).unwrap();
        recordings.fs.create_dir(&lock_dir().join("1-0")).unwrap();
        recordings.fs.create(&lock_dir().join("note")).unwrap();
        assert!(!recordings.running(&[]).unwrap());
    }

    /// A lock's directory that can't be listed is the check's error: it
    /// can't say whether a recording runs.
    #[test]
    fn a_lock_directory_that_cant_be_listed_is_an_error() {
        let recordings = library();
        recordings.fs.create(&lock_dir()).unwrap();
        assert_eq!(
            recordings.running(&[]).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
    }

    /// A recording that starts at the moment a check tries its new entry
    /// still starts: it leaves that entry and holds another, and only one
    /// is left in the lock's directory.
    #[test]
    fn a_recording_started_during_a_check_still_starts() {
        let fs = Meddling::new();
        fs.with(|m| m.checked = 1);
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        let _recording = recordings.record().unwrap();
        assert_eq!(fs.with(|m| m.checked), 0, "no check met the entry");
        assert_eq!(entries(&fs).len(), 1);
        assert!(recordings.running(&[]).unwrap());
    }

    /// An entry's name already taken (a crash left it) is passed over.
    #[test]
    fn an_entry_name_already_there_is_passed_over() {
        let fs = Meddling::new();
        fs.with(|m| m.taken = 1);
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        let _recording = recordings.record().unwrap();
        assert_eq!(fs.with(|m| m.taken), 0, "no name was taken");
        assert!(recordings.running(&[]).unwrap());
    }

    /// Past its tries, a recording gives up with the last reason, and
    /// leaves none of its entries behind.
    #[test]
    fn a_recording_gives_up_after_its_tries() {
        let fs = Meddling::new();
        fs.with(|m| m.checked = ENTRY_TRIES);
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        assert_eq!(
            recordings.record().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(entries(&fs), Vec::<PathBuf>::new());
    }

    /// An entry that can't be locked for another reason is that error, and
    /// is removed.
    #[test]
    fn an_entry_that_cant_be_locked_is_its_error() {
        let fs = Meddling::new();
        fs.with(|m| m.refused = Some(io::ErrorKind::PermissionDenied));
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        assert_eq!(
            recordings.record().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(entries(&fs), Vec::<PathBuf>::new());
    }

    /// An entry that can't be made is that error.
    #[test]
    fn an_entry_that_cant_be_made_is_its_error() {
        let recordings = library();
        recordings.fs.create_dir(&lock_dir()).unwrap();
        // The lock's directory is there; the entry is the next thing made.
        recordings.fs.fail_after(1, io::ErrorKind::PermissionDenied);
        assert_eq!(
            recordings.record().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    /// A start refused because a moment plays leaves no entry: a check
    /// after it finds nothing running.
    #[test]
    fn a_refused_recording_leaves_no_entry() {
        let recordings = library();
        let _quiet = recordings.quiet().unwrap();
        assert_eq!(
            recordings.record().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(entries(&recordings.fs), Vec::<PathBuf>::new());
    }

    /// Recordings and checks on the real filesystem, from two threads at
    /// once: every recording starts, and each check while one is held
    /// finds it.
    #[test]
    fn recordings_and_checks_on_the_real_filesystem_dont_get_in_each_others_way() {
        let dir = crate::test_dir::TestDir::new("recordings-checks");
        let recordings = Recordings::new(crate::fs::StdFs, &dir.0);
        let held = recordings.record().unwrap();
        std::thread::scope(|scope| {
            let checks = scope.spawn(|| (0..500).all(|_| recordings.running(&[]).unwrap()));
            for _ in 0..500 {
                drop(recordings.record().unwrap());
            }
            assert!(checks.join().unwrap(), "a check missed the held recording");
        });
        drop(held);
        assert!(!recordings.running(&[]).unwrap());
    }

    /// What [`Meddling`] does next, and the locks it was asked for.
    #[derive(Debug, Default)]
    struct Meddle {
        /// How many of the next entries a check holds as they're locked.
        checked: usize,
        /// How many of the next entries' names are already there.
        taken: usize,
        /// The error every entry's lock fails with, if any.
        refused: Option<io::ErrorKind>,
        /// Every lock asked for: its directory, and whether shared.
        locks: Vec<(PathBuf, bool)>,
    }

    /// A [`FakeFs`] with a data folder, `/data`, where a check, a name
    /// already there or a refusal meets a recording's entry as told.
    #[derive(Debug, Clone)]
    struct Meddling {
        inner: FakeFs,
        meddle: Arc<Mutex<Meddle>>,
    }

    impl Meddling {
        fn new() -> Self {
            Self {
                inner: FakeFs::with_dirs(["/data"]),
                meddle: Arc::default(),
            }
        }

        /// Runs `f` on what it does next.
        fn with<T>(&self, f: impl FnOnce(&mut Meddle) -> T) -> T {
            f(&mut self.meddle.lock().unwrap_or_else(PoisonError::into_inner))
        }

        /// The locks asked for since the last [`Self::forget_locks`].
        fn locks(&self) -> Vec<(PathBuf, bool)> {
            self.with(|m| m.locks.clone())
        }

        fn forget_locks(&self) {
            self.with(|m| m.locks.clear());
        }
    }

    /// Whether `path` is an entry in [`lock_dir`].
    fn is_entry(path: &Path) -> bool {
        path.parent() == Some(lock_dir().as_path())
    }

    impl Fs for Meddling {
        type File = FakeFile;
        type Lock = FakeLock;

        fn create(&self, path: &Path) -> io::Result<FakeFile> {
            self.inner.create(path)
        }

        fn create_dir(&self, path: &Path) -> io::Result<()> {
            if is_entry(path)
                && self.with(|m| {
                    let taken = m.taken > 0;
                    m.taken = m.taken.saturating_sub(1);
                    taken
                })
            {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            self.inner.create_dir(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }

        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.inner.sync_dir(dir)
        }

        fn remove(&self, path: &Path) -> io::Result<()> {
            self.inner.remove(path)
        }

        fn remove_dir(&self, path: &Path) -> io::Result<()> {
            self.inner.remove_dir(path)
        }

        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.inner.read(path)
        }

        fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            self.inner.list(dir)
        }

        fn lock_dir(&self, dir: &Path) -> io::Result<FakeLock> {
            let (checked, refused) = self.with(|m| {
                m.locks.push((dir.to_path_buf(), false));
                let checked = is_entry(dir) && m.checked > 0;
                if checked {
                    m.checked -= 1;
                }
                (checked, m.refused.filter(|_| is_entry(dir)))
            });
            if let Some(kind) = refused {
                return Err(kind.into());
            }
            // A check holds the entry at this moment, as `running` would.
            let _check = checked.then(|| self.inner.lock_dir(dir)).transpose()?;
            self.inner.lock_dir(dir)
        }

        fn lock_dir_shared(&self, dir: &Path) -> io::Result<FakeLock> {
            self.with(|m| m.locks.push((dir.to_path_buf(), true)));
            self.inner.lock_dir_shared(dir)
        }
    }
}
