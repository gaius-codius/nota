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
//! Each start removes those: once it holds its own entry and the shared
//! lock, it takes every other entry exclusively and removes the ones it
//! gets, while it holds them. One a recording or a check holds refuses it
//! and stays. A start's clean-up may also take a recording's new entry
//! before its recorder locks it, or remove it between the recorder's open
//! and its lock, so the recorder makes sure its entry is still there once
//! it holds it, and makes another if not. A check stays read-only.
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
/// first follows a name already there, or a check or another start holding
/// or removing the entry it just made, all rare.
const ENTRY_TRIES: usize = 64;

/// The count in the next entry this process makes, so its entries' names
/// differ (see [`entry_name`]).
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
        // Last, so a start refused here removes nothing: while the clean-up
        // holds an entry a check reads it as a recording, which by now is
        // true.
        self.clear_left();
        Ok(RecordingLock {
            _shared: shared,
            _entry: entry,
        })
    }

    /// Whether a recording other than those holding the entries in `own`
    /// runs in the library: one whose entry is locked. It tries each entry
    /// shared, so checks don't get in each other's way; it never takes the
    /// library's lock, so it keeps out neither a recording nor a moment
    /// playing (a recording whose new entry it's trying makes another), and
    /// it changes nothing on disk.
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
            // Shared: a recording holds its entry exclusively, so this is
            // refused exactly while one does, and not by another check.
            match self.fs.lock_dir_shared(entry) {
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

    /// Removes every entry in the lock's directory that nobody holds: those
    /// crashed recordings left. Each is taken exclusively and removed while
    /// held, so one a recording or a check holds refuses it and stays, this
    /// recording's own included. Nothing it meets is an error: a clean-up
    /// that fails leaves only what a check already reads as no recording,
    /// and must never end a start.
    fn clear_left(&self) {
        let Ok(entries) = self.fs.list(&self.dir) else {
            return;
        };
        for entry in entries {
            if let Ok(held) = self.fs.lock_dir(&entry) {
                let _left = self.fs.remove_dir(&entry);
                // Let go only once it's gone.
                drop(held);
            }
        }
    }

    /// Makes this recording's entry in the lock's directory, making that
    /// first if it isn't there, and locks it exclusively. Its name is the
    /// process's id, a number drawn at random and a count, so a name a
    /// crash left (where ids repeat, as in a PID namespace) is all but never
    /// met again. One that is, and an entry a check or another start's
    /// clean-up meets at the moment it's locked, are passed over for the
    /// next name ([`Self::make_entry`]).
    fn enter(&self) -> io::Result<Entry<S>>
    where
        S: Clone,
    {
        self.make_dir()?;
        let mut passed = None;
        for _ in 0..ENTRY_TRIES {
            match self.make_entry(self.dir.join(entry_name()))? {
                Tried::Held(entry) => return Ok(entry),
                // It's let go or gone at once, but this recording mustn't
                // wait for it.
                Tried::Passed(why) => passed = Some(why),
            }
        }
        Err(io::Error::other(NoEntry { last: passed }))
    }

    /// Makes the entry `dir` and locks it exclusively; an entry made but
    /// not held is removed. A name already there, a lock refused while a
    /// check or a clean-up holds it, and an entry a clean-up removed are
    /// passed over; any other error is the start's.
    fn make_entry(&self, dir: PathBuf) -> io::Result<Tried<S>>
    where
        S: Clone,
    {
        match self.fs.create_dir(&dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(Tried::Passed(e)),
            made => made?,
        }
        let lock = match self.fs.lock_dir(&dir) {
            Ok(lock) => lock,
            Err(e) => {
                let _left = self.fs.remove_dir(&dir);
                return match e.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::NotFound => Ok(Tried::Passed(e)),
                    _ => Err(e),
                };
            }
        };
        let entry = Entry {
            _lock: lock,
            fs: self.fs.clone(),
            dir,
        };
        // A clean-up may have locked and removed the entry between the
        // lock's open and its `flock`, which then holds a directory no
        // longer there, where no check sees it. Names aren't met again, so
        // one still there is this one.
        match self.fs.list(&entry.dir) {
            Ok(_) => Ok(Tried::Held(entry)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Tried::Passed(e)),
            Err(e) => Err(e),
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

/// A new entry's name: `<process id>-<random, in hex>-<count>`. The random
/// part comes from the standard library's hasher keys, drawn afresh for
/// each process, so it needs no clock and no crate.
fn entry_name() -> String {
    use std::hash::BuildHasher;
    let number = NEXT_ENTRY.fetch_add(1, Ordering::Relaxed);
    let random = std::collections::hash_map::RandomState::new().hash_one(number);
    format!("{}-{random:016x}-{number}", std::process::id())
}

/// One try at a recording's entry ([`Recordings::make_entry`]).
enum Tried<S: Fs> {
    /// The entry, made and held.
    Held(Entry<S>),
    /// The name was passed over, for this reason; another is tried.
    Passed(io::Error),
}

/// Every name a recording tried for its entry was already there, or held or
/// removed by a check or another start ([`Recordings::record`]): not a
/// moment playing, so not [`io::ErrorKind::WouldBlock`].
#[derive(Debug)]
struct NoEntry {
    /// Why the last name was passed over.
    last: Option<io::Error>,
}

impl std::fmt::Display for NoEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no entry of its own could be made in {ENTRY_TRIES} tries"
        )?;
        match &self.last {
            Some(last) => write!(f, ": {last}"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for NoEntry {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.last
            .as_ref()
            .map(|last| last as &(dyn std::error::Error + 'static))
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
        // the recording runs. Left behind, it reads as no recording until
        // the next start removes it.
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

    /// A lock refused because it's held is that refusal at once: only a
    /// missing directory is made and the lock tried again, so a moment
    /// asked to play while a recording runs asks for the lock once.
    #[test]
    fn a_held_lock_is_refused_without_a_second_try() {
        let fs = Meddling::new();
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        let _recording = recordings.record().unwrap();
        fs.forget_locks();
        assert_eq!(
            recordings.quiet().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(fs.locks(), [(lock_dir(), false)]);
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
        recordings.fs.create_dir(&lock_dir().join("1-0-0")).unwrap();
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

    /// Past its tries, a recording gives up saying so, with the last
    /// reason: not as a moment playing would refuse it. It leaves none of
    /// its entries behind.
    #[test]
    fn a_recording_gives_up_after_its_tries() {
        let fs = Meddling::new();
        fs.with(|m| m.checked = ENTRY_TRIES);
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        let refused = recordings.record().unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::Other);
        assert_eq!(
            refused.to_string(),
            "no entry of its own could be made in 64 tries: the directory is locked"
        );
        let last = std::error::Error::source(refused.get_ref().unwrap()).unwrap();
        assert_eq!(last.to_string(), "the directory is locked");
        assert_eq!(entries(&fs), Vec::<PathBuf>::new());
    }

    /// Two checks at once don't take each other for a recording: an entry
    /// a crash left, which another check is trying, is still no recording.
    #[test]
    fn checks_dont_take_each_other_for_a_recording() {
        let recordings = library();
        recordings.fs.create_dir(&lock_dir()).unwrap();
        let left = lock_dir().join("1-0-0");
        recordings.fs.create_dir(&left).unwrap();
        // The other check's hold, as `running` takes it.
        let _other_check = recordings.fs.lock_dir_shared(&left).unwrap();
        assert!(!recordings.running(&[]).unwrap());
    }

    /// An entry removed between the listing and its try, as its recording
    /// stopped, is no recording and no error.
    #[test]
    fn an_entry_removed_during_a_check_is_not_running() {
        let fs = Meddling::new();
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        drop(recordings.record().unwrap());
        fs.inner.create_dir(&lock_dir().join("1-0-0")).unwrap();
        fs.with(|m| m.vanish = true);
        assert!(!recordings.running(&[]).unwrap());
        assert_eq!(entries(&fs), Vec::<PathBuf>::new());
    }

    /// Entries' names differ within a process, and carry its id.
    #[test]
    fn entry_names_differ_and_carry_the_process_id() {
        let (first, second) = (entry_name(), entry_name());
        assert_ne!(first, second);
        let pid = format!("{}-", std::process::id());
        assert!(
            first.starts_with(&pid) && second.starts_with(&pid),
            "{first} {second}"
        );
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

    /// Entries a crash left in [`library`]'s lock directory: two, which
    /// nobody holds.
    fn left_by_crashes(fs: &impl Fs) -> [PathBuf; 2] {
        fs.create_dir(&lock_dir()).unwrap();
        let left = [lock_dir().join("1-0-0"), lock_dir().join("2-0-0")];
        for entry in &left {
            fs.create_dir(entry).unwrap();
        }
        left
    }

    /// A start removes every entry nobody holds, leaving only its own.
    #[test]
    fn a_start_removes_the_entries_nobody_holds() {
        let recordings = library();
        let left = left_by_crashes(&recordings.fs);
        let _recording = recordings.record().unwrap();
        let after = entries(&recordings.fs);
        let [own] = &after[..] else {
            panic!("{after:?}")
        };
        assert!(!left.contains(own), "{after:?}");
        // The one left is the recording's, held.
        assert!(recordings.running(&[]).unwrap());
    }

    /// What isn't an entry (a file, a directory with something in it) is
    /// left, and doesn't keep a start from removing the entries beside it.
    #[test]
    fn a_start_removes_left_entries_beside_what_isnt_one() {
        let recordings = library();
        let left = left_by_crashes(&recordings.fs);
        // Named to sort first, so the clean-up meets them before the rest.
        let note = lock_dir().join("0-note");
        let full = lock_dir().join("0-full");
        recordings.fs.create(&note).unwrap();
        recordings.fs.create_dir(&full).unwrap();
        recordings.fs.create(&full.join("inside")).unwrap();
        let _recording = recordings.record().unwrap();
        let after = entries(&recordings.fs);
        assert_eq!(after.len(), 3, "{after:?}");
        assert!(after.contains(&note) && after.contains(&full), "{after:?}");
        assert!(left.iter().all(|entry| !after.contains(entry)), "{after:?}");
    }

    /// A start leaves another recording's entry, which it holds.
    #[test]
    fn a_start_leaves_a_running_recordings_entry() {
        let recordings = library();
        let _first = recordings.record().unwrap();
        let first = entries(&recordings.fs);
        let _second = recordings.record().unwrap();
        let after = entries(&recordings.fs);
        assert_eq!(after.len(), 2, "{after:?}");
        assert!(after.contains(&first[0]), "{after:?}");
        // Without its entry, the first recording would be no longer seen.
        let second: Vec<_> = after.into_iter().filter(|e| *e != first[0]).collect();
        assert!(recordings.running(&second).unwrap());
    }

    /// A start leaves an entry a check holds at that moment, although no
    /// recording does: it can't tell which, so it doesn't remove it.
    #[test]
    fn a_start_leaves_an_entry_a_check_holds() {
        let recordings = library();
        let [checked, unheld] = left_by_crashes(&recordings.fs);
        // The check's hold, as `running` takes it.
        let _check = recordings.fs.lock_dir_shared(&checked).unwrap();
        let _recording = recordings.record().unwrap();
        let after = entries(&recordings.fs);
        assert_eq!(after.len(), 2, "{after:?}");
        assert!(
            after.contains(&checked) && !after.contains(&unheld),
            "{after:?}"
        );
    }

    /// A recording whose new entry another start's clean-up removes before
    /// it locks it makes another and starts, and is seen.
    #[test]
    fn a_recording_whose_entry_is_removed_before_its_lock_makes_another() {
        let fs = Meddling::new();
        fs.with(|m| m.removed = 1);
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        let _recording = recordings.record().unwrap();
        assert_eq!(fs.with(|m| m.removed), 0, "no entry was removed");
        assert_eq!(entries(&fs).len(), 1);
        assert!(recordings.running(&[]).unwrap());
    }

    /// A recording whose new entry another start's clean-up removes between
    /// the lock's open and its `flock` holds a lock on a directory no longer
    /// there: it makes another and starts, and is seen.
    #[test]
    fn a_recording_whose_entry_is_removed_under_its_lock_makes_another() {
        let fs = Meddling::new();
        fs.with(|m| m.unlinked = 1);
        let recordings = Recordings::new(fs.clone(), Path::new("/data"));
        let _recording = recordings.record().unwrap();
        assert_eq!(fs.with(|m| m.unlinked), 0, "no entry was removed");
        assert_eq!(entries(&fs).len(), 1);
        assert!(recordings.running(&[]).unwrap());
    }

    /// Recordings, their clean-ups and checks on the real filesystem, from
    /// three threads at once: every recording starts, each check while one
    /// is held finds it, and an entry a crash left is gone by the end.
    #[test]
    fn recordings_and_checks_on_the_real_filesystem_dont_get_in_each_others_way() {
        let dir = crate::test_dir::TestDir::new("recordings-checks");
        let recordings = Recordings::new(crate::fs::StdFs, &dir.0);
        let held = recordings.record().unwrap();
        let held_entry = recordings.fs.list(recordings.dir()).unwrap();
        let left = recordings.dir().join("1-0-0");
        recordings.fs.create_dir(&left).unwrap();
        let starts = || {
            for _ in 0..300 {
                let recording = recordings.record().unwrap();
                // Seen past the held one: an entry the other thread's
                // clean-up removed under its lock would be held unseen.
                assert!(
                    recordings.running(&held_entry).unwrap(),
                    "a start went unseen"
                );
                drop(recording);
            }
        };
        std::thread::scope(|scope| {
            let checks = scope.spawn(|| (0..500).all(|_| recordings.running(&[]).unwrap()));
            // Two threads, so one's clean-up meets the other's new entries.
            let other = scope.spawn(starts);
            starts();
            other.join().unwrap();
            assert!(checks.join().unwrap(), "a check missed the held recording");
        });
        assert_eq!(recordings.fs.list(recordings.dir()).unwrap().len(), 1);
        drop(held);
        assert!(!recordings.running(&[]).unwrap());
        assert_eq!(
            recordings.fs.list(recordings.dir()).unwrap(),
            Vec::<PathBuf>::new()
        );
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
        /// Whether an entry is removed just before a check tries it.
        vanish: bool,
        /// How many of the next entries another start's clean-up removes
        /// just before they're locked exclusively.
        removed: usize,
        /// How many of the next entries another start's clean-up removes
        /// between the lock's open and its `flock`: the lock is taken, on a
        /// directory no longer there.
        unlinked: usize,
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

    /// Whether `count` was above zero, counting one off it.
    fn take_one(count: &mut usize) -> bool {
        let some = *count > 0;
        *count = count.saturating_sub(1);
        some
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
            let (checked, refused, removed, unlinked) = self.with(|m| {
                m.locks.push((dir.to_path_buf(), false));
                let entry = is_entry(dir);
                (
                    entry && take_one(&mut m.checked),
                    m.refused.filter(|_| entry),
                    entry && take_one(&mut m.removed),
                    entry && take_one(&mut m.unlinked),
                )
            });
            if let Some(kind) = refused {
                return Err(kind.into());
            }
            if removed {
                self.inner.remove_dir(dir)?;
            }
            // A check holds the entry at this moment, as `running` would.
            let _check = checked
                .then(|| self.inner.lock_dir_shared(dir))
                .transpose()?;
            let lock = self.inner.lock_dir(dir)?;
            if unlinked {
                // The fake's locks don't keep a directory from being
                // removed, as an `flock` on a real one doesn't.
                self.inner.remove_dir(dir)?;
            }
            Ok(lock)
        }

        fn lock_dir_shared(&self, dir: &Path) -> io::Result<FakeLock> {
            if self.with(|m| {
                m.locks.push((dir.to_path_buf(), true));
                m.vanish && is_entry(dir)
            }) {
                self.inner.remove_dir(dir)?;
            }
            self.inner.lock_dir_shared(dir)
        }
    }
}
