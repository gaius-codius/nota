//! The disk check and the ballast: how a recording meets a full disk.
//!
//! - **The check** ([`check`]): the space free where the recording goes,
//!   and how long it lasts at the recording's rate ([`Usage`]): about 60 MB
//!   of FLAC an hour for each 16 kHz track, after keeping back what
//!   finishing the open segments needs (each track's journal being written,
//!   the one before it still being published, and that one's FLAC). It's
//!   made before recording and again while recording ([`DiskMonitor`]).
//!   A low disk never stops a start: nota records what fits, and warns.
//! - **The ballast** ([`Ballast`]): a file of [`BALLAST_LEN`] bytes kept in
//!   the data directory, written through the durable-write layer, so a full
//!   disk still has room to finish. It's named for its size, so one of
//!   another size (a test run's) is never taken for it. It's made only
//!   while there's room for it twice over, by one process at a time (a lock
//!   on the data directory), fsynced every few megabytes so its write-back
//!   never holds up the journals' fsyncs for long, and its bytes don't
//!   compress, so a filesystem that compresses still sets aside all of it.
//!   (On a filesystem with snapshots, removing it frees nothing while a
//!   snapshot holds it.)
//! - **The watch** ([`DiskWatch`], through [`WatchedFs`]): every filesystem
//!   operation the recording makes goes through a [`WatchedFs`]. The first
//!   to fail for want of space (`ENOSPC`, or `EDQUOT` for a quota) frees the
//!   ballast before its error is returned, so what the recording does next
//!   about the failure finds room: the session writer's replacement journal
//!   (see [`session`](crate::session)), the publisher's next try. The disk
//!   is marked full from then on. SQLite writes the library database
//!   itself, not through an [`Fs`]: a [`WatchedStore`] notes a row commit
//!   that failed for want of space the same way. A check that finds less
//!   than [`FULL_FLOOR`] free, after an earlier check this recording found
//!   more, marks it full too, for whatever else meets the full disk first.
//!   A watch made at startup, to salvage or start before any recording
//!   runs, can [share](DiskWatch::share) the ballast: before freeing it,
//!   it asks again whether a live recording has claimed it since.
//! - **The monitor** ([`DiskMonitor`]): a thread that makes the check every
//!   [`CHECK_INTERVAL`], keeps the ballast, and reports the space, the
//!   [low-disk warning](LOW_WARNING) and the full disk. Its first check, and
//!   taking a ballast already there, happen before [`DiskMonitor::spawn`]
//!   returns, so before the recording writes. A full disk stops the
//!   recording: its open segments finish into the ballast's room.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::SampleRate;
use nota_core::recorder::{Disk, WarningState};

use crate::fs::{FileSyncer, Fs, FsFile, Synced, is_disk_full};
use crate::segment::{DurableSegment, SegmentLength, SegmentStore};

/// What every ballast's file name starts with, in the data directory.
const BALLAST_PREFIX: &str = "ballast-";

/// The file name of a ballast of `len` bytes: `ballast-<len>`. Named for
/// its size, so a ballast of another size is never taken for it.
#[must_use]
pub fn ballast_file_name(len: u64) -> String {
    format!("{BALLAST_PREFIX}{len}")
}

/// What a ballast is written under until it's whole: only a whole one,
/// fsynced, is renamed to its [name](ballast_file_name).
fn ballast_temp_name(len: u64) -> String {
    format!("{BALLAST_PREFIX}{len}.tmp")
}

/// How much of the ballast is written between its fsyncs: a few
/// megabytes, so its write-back never stalls the journals' fsyncs on the
/// same disk for long.
const BALLAST_SYNC_EVERY: u64 = 8 << 20;

/// The ballast's size: 256 MB, minutes of finishing at any rate nota
/// records, and little enough to set aside on any disk worth recording to.
pub const BALLAST_LEN: u64 = 256 * 1024 * 1024;

/// A check that finds less than this free (1 MiB), after an earlier check
/// of the recording found more, marks the disk full: writes are failing,
/// or about to. A recording that starts with less records what fits, with
/// the warning, until a write fails: a low disk never stops a start. It's
/// far below the [reserve](Usage::reserve), which only shortens the
/// estimate.
pub const FULL_FLOOR: u64 = 1024 * 1024;

/// Under this much recording left, the low-disk warning holds.
pub const LOW_WARNING: Duration = Duration::from_hours(1);

/// How often the monitor checks the free space.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Bytes a 16-bit sample takes in a journal, before framing.
const SAMPLE_BYTES: u64 = 2;

/// How fast a recording fills its disk, and what it must keep back to
/// finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    tracks: u64,
    rate: SampleRate,
    length: SegmentLength,
}

impl Usage {
    /// `tracks` tracks at `rate`, in segments of `length`.
    #[must_use]
    pub fn new(tracks: usize, rate: SampleRate, length: SegmentLength) -> Self {
        Self {
            tracks: u64::try_from(tracks).unwrap_or(u64::MAX),
            rate,
            length,
        }
    }

    /// The FLAC the recording publishes a second, every track together:
    /// 60 MB an hour a track at 16 kHz, in proportion at other rates.
    /// Speech compresses to about half its 16-bit size; this is a little
    /// over that, so the estimate errs short.
    #[must_use]
    pub fn bytes_per_second(&self) -> u64 {
        // 60 MB / 3,600 s / 16,000 Hz = 25/24 bytes a sample.
        let per_track = u64::from(self.rate.hz()).saturating_mul(25).div_ceil(24);
        per_track.saturating_mul(self.tracks).max(1)
    }

    /// What finishing the open segments needs: for each track, the journal
    /// being written and the one before it, still being published (a
    /// window each, with a quarter more for their framing), and the FLAC
    /// of one window.
    #[must_use]
    pub fn reserve(&self) -> u64 {
        let window = self.length.samples().get();
        let journal = window.saturating_mul(SAMPLE_BYTES).saturating_mul(5) / 4;
        let flac = window
            .saturating_mul(self.bytes_per_second() / self.tracks.max(1))
            .div_ceil(u64::from(self.rate.hz()));
        journal
            .saturating_mul(2)
            .saturating_add(flac)
            .saturating_mul(self.tracks)
    }

    /// How long `free` bytes last: what's past the [reserve](Self::reserve),
    /// at [`Self::bytes_per_second`], to the second; zero if nothing is.
    #[must_use]
    pub fn time_left(&self, free: u64) -> Duration {
        Duration::from_secs(free.saturating_sub(self.reserve()) / self.bytes_per_second())
    }
}

/// The disk check: the space free on the filesystem holding `dir`, and how
/// long a recording using it as `usage` says can go on.
///
/// # Errors
///
/// As [`Fs::free_space`].
pub fn check<S: Fs>(fs: &S, dir: &Path, usage: Usage) -> io::Result<Disk> {
    let free_bytes = fs.free_space(dir)?;
    Ok(Disk {
        free_bytes,
        left: Some(usage.time_left(free_bytes)),
    })
}

/// The ballast file: room set aside in the data directory, freed when the
/// disk fills so the recording can finish.
#[derive(Debug, PartialEq, Eq)]
pub struct Ballast {
    path: PathBuf,
}

impl Ballast {
    /// The ballast of `len` bytes already in `dir`, if there is one. Only a
    /// whole one is ever under its name.
    ///
    /// # Errors
    ///
    /// If `dir` can't be listed.
    pub fn find<S: Fs>(fs: &S, dir: &Path, len: u64) -> io::Result<Option<Self>> {
        let path = dir.join(ballast_file_name(len));
        Ok(fs.list(dir)?.contains(&path).then_some(Self { path }))
    }

    /// Keeps the ballast in `dir`: the one already there, or a new one of
    /// `len` bytes if the disk has room for it twice over. `None` if there
    /// isn't room, another process is making one (it holds the data
    /// directory's lock), or `give_up` said to stop while it was being
    /// written: nota records without one.
    ///
    /// It's made under a lock on `dir`, so two processes never write one at
    /// once. It's written as a temp file, fsynced every 8 MiB, renamed and
    /// its directory synced, so the name only ever holds a whole ballast.
    /// Leftovers are removed first: temp files an interrupted run left, and
    /// ballasts of other sizes. If writing fails, the temp file is removed
    /// again, best effort.
    ///
    /// # Errors
    ///
    /// Any I/O error. Running out of room while writing is an error too.
    pub fn keep<S: Fs>(
        fs: &S,
        dir: &Path,
        len: u64,
        give_up: impl Fn() -> bool,
    ) -> io::Result<Option<Self>> {
        if let Some(there) = Self::find(fs, dir, len)? {
            return Ok(Some(there));
        }
        let _making = match fs.lock_dir(dir) {
            Ok(lock) => lock,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        let path = dir.join(ballast_file_name(len));
        let temp = dir.join(ballast_temp_name(len));
        let there = fs.list(dir)?;
        if there.contains(&path) {
            // Made by another process while this one waited to look.
            return Ok(Some(Self { path }));
        }
        let leftovers: Vec<&PathBuf> = there
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(BALLAST_PREFIX))
            })
            .collect();
        for leftover in &leftovers {
            fs.remove(leftover)?;
        }
        if !leftovers.is_empty() {
            fs.sync_dir(dir)?;
        }
        if fs.free_space(dir)? < len.saturating_mul(2) {
            return Ok(None);
        }
        match write_ballast(fs, &temp, len, &give_up) {
            Ok(true) => {}
            Ok(false) => {
                let _gone = fs.remove(&temp);
                return Ok(None);
            }
            Err(e) => {
                let _gone = fs.remove(&temp);
                return Err(e);
            }
        }
        fs.rename(&temp, &path)?;
        fs.sync_dir(dir)?;
        Ok(Some(Self { path }))
    }

    /// Where it is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Frees it: removes it and syncs its directory. The directory sync
    /// matters beyond durability: ext4 hands a removed file's blocks back
    /// only once the journal commits, which the sync forces. One already
    /// gone (another process sharing it freed it) is freed all the same.
    fn free<S: Fs>(self, fs: &S) -> io::Result<()> {
        match fs.remove(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        match self.path.parent() {
            Some(dir) => fs.sync_dir(dir),
            None => Ok(()),
        }
    }
}

/// Writes `len` bytes that don't compress to `temp`, a megabyte at a time,
/// fsyncing every [`BALLAST_SYNC_EVERY`] and at the end. `false` if
/// `give_up` said to stop between writes.
fn write_ballast<S: Fs>(
    fs: &S,
    temp: &Path,
    len: u64,
    give_up: &impl Fn() -> bool,
) -> io::Result<bool> {
    /// A megabyte. Written out, and the loop counted in whole chunks, so
    /// no change to either can make the loop run forever.
    const CHUNK: usize = 1_048_576;
    let chunk = noise(CHUNK);
    let mut file = fs.create(temp)?;
    let mut left = len;
    let mut unsynced = 0;
    for _ in 0..len.div_ceil(CHUNK as u64) {
        if give_up() {
            return Ok(false);
        }
        let n = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
        file.write_all(chunk.get(..n).unwrap_or(&chunk))?;
        left = left.saturating_sub(n as u64);
        unsynced += n as u64;
        if unsynced >= BALLAST_SYNC_EVERY && left > 0 {
            file.sync()?;
            unsynced = 0;
        }
    }
    file.sync()?;
    Ok(true)
}

/// `len` bytes of xorshift noise: no filesystem's compression shrinks them.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = vec![0; len];
    for word in out.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        word.copy_from_slice(bytes.get(..word.len()).unwrap_or(&bytes));
    }
    out
}

/// The disk filled: what failed for want of space, and what became of the
/// ballast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Full {
    /// The file or directory whose operation failed with `ENOSPC` or
    /// `EDQUOT` (for the library database, its file); `None` when a check
    /// found less than [`FULL_FLOOR`] free.
    pub path: Option<PathBuf>,
    /// The ballast.
    pub ballast: Freed,
}

/// What a full disk did with the ballast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freed {
    /// It was freed: its room is the recording's.
    Freed,
    /// None was freed: none was held (the disk hadn't room for one, it
    /// couldn't be made, or it was still being made), or a live recording
    /// claimed the one held (see [`DiskWatch::share`]), which kept it.
    None,
    /// Removing it failed, with this kind of error.
    Failed(io::ErrorKind),
}

/// What the filesystem wrapper and the monitor share: the ballast, and
/// whether the disk has filled. Make one per recording, with the filesystem
/// the recording writes to; give the recording [`DiskWatch::fs`].
#[derive(Debug)]
pub struct DiskWatch<S> {
    fs: S,
    state: Mutex<WatchState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct WatchState {
    ballast: Option<Ballast>,
    full: Option<Full>,
    /// The monitor was asked to stop.
    stopping: bool,
    /// Asks whether a live recording claims the ballast, while it's
    /// shared ([`DiskWatch::share`]).
    claimed: Option<Claimed>,
    /// The directories locked through [`DiskWatch::fs`] and not yet
    /// unlocked: the watch's own, never a live recording's.
    held: Vec<PathBuf>,
}

/// Whether a live recording claims a shared ballast now, given the
/// directories the watch holds itself: `true` keeps it.
struct Claimed(Box<ClaimCheck>);

/// What [`DiskWatch::share`] is given: told the directories the watch
/// holds itself, whether a live recording claims the ballast.
type ClaimCheck = dyn Fn(&[PathBuf]) -> bool + Send + Sync;

impl fmt::Debug for Claimed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Claimed(..)")
    }
}

impl<S: Fs + Clone> DiskWatch<S> {
    /// A watch over `fs`, with no ballast yet and the disk not full.
    #[must_use]
    pub fn new(fs: S) -> Arc<Self> {
        Arc::new(Self {
            fs,
            state: Mutex::new(WatchState::default()),
            changed: Condvar::new(),
        })
    }

    /// `fs`, watched: the first operation through it that fails for want of
    /// space frees the ballast before returning its error.
    #[must_use]
    pub fn fs(self: &Arc<Self>) -> WatchedFs<S> {
        WatchedFs {
            inner: self.fs.clone(),
            watch: Arc::clone(self),
        }
    }

    /// Holds `ballast` to free when the disk fills, or frees it at once if
    /// the disk has filled already.
    pub fn hold(&self, ballast: Ballast) {
        let mut guard = self.lock();
        let state = &mut *guard;
        match state.full.as_mut() {
            Some(full) => {
                let freed = spend(&self.fs, state.claimed.as_ref(), &state.held, ballast);
                if full.ballast == Freed::None {
                    full.ballast = freed;
                }
            }
            None => state.ballast = Some(ballast),
        }
    }

    /// Shares the ballast with the recordings in its data directory, until
    /// [`Self::start_recording`]: before freeing it, asks `claimed` whether
    /// a live recording claims it now, and if one does, leaves it on disk
    /// for that recording and frees nothing ([`Freed::None`]); this watch
    /// lets it go, and the next startup finds it again. For a watch made at
    /// startup, whose ballast a recording that starts meanwhile needs to
    /// finish. `claimed` is given the directories locked through
    /// [`Self::fs`] and still held, salvage's session among them: those are
    /// the startup's own, not a recording's. It's asked with the watch
    /// locked, so it mustn't use the watch: give it the filesystem under it.
    pub fn share(&self, claimed: impl Fn(&[PathBuf]) -> bool + Send + Sync + 'static) {
        self.lock().claimed = Some(Claimed(Box::new(claimed)));
    }

    /// Notes that the disk is full, `path`'s operation having failed for
    /// want of space (or a check having found no room, with `None`). The
    /// first note frees the ballast, before returning, so whoever noted it
    /// finds room when it tries again; later ones change nothing. Notes
    /// wait for the first one's freeing.
    pub fn note_full(&self, path: Option<&Path>) {
        let mut state = self.lock();
        if state.full.is_some() {
            return;
        }
        let ballast = match state.ballast.take() {
            Some(ballast) => spend(&self.fs, state.claimed.as_ref(), &state.held, ballast),
            None => Freed::None,
        };
        state.full = Some(Full {
            path: path.map(Path::to_path_buf),
            ballast,
        });
        self.changed.notify_all();
    }

    /// Starts recording after startup succeeded. A full disk startup got
    /// past belongs to recovery, before the monitor or any writer runs. The
    /// ballast is this recording's own from now on: no longer
    /// [shared](Self::share).
    pub fn start_recording(&self) {
        let mut state = self.lock();
        state.full = None;
        state.claimed = None;
    }

    /// Whether the disk has filled, and how.
    #[must_use]
    pub fn full(&self) -> Option<Full> {
        self.lock().full.clone()
    }

    /// Whether a ballast is held, not yet freed.
    #[must_use]
    pub fn holds_ballast(&self) -> bool {
        self.lock().ballast.is_some()
    }

    /// Waits up to `timeout` until the monitor is asked to stop, or, if
    /// `until_full`, the disk fills.
    fn wait(&self, timeout: Duration, until_full: bool) {
        let state = self.lock();
        let _woken = self
            .changed
            .wait_timeout_while(state, timeout, |s| {
                !(s.stopping || until_full && s.full.is_some())
            })
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Whether the monitor was asked to stop.
    fn stopping(&self) -> bool {
        self.lock().stopping
    }

    /// Asks the monitor to stop. A watch has one monitor, once.
    fn stop_monitor(&self) {
        self.lock().stopping = true;
        self.changed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Frees `ballast`, unless `claimed` says a live recording claims it, the
/// watch holding `held` itself: then it's left on disk for that
/// recording, and nothing is freed.
fn spend<S: Fs>(fs: &S, claimed: Option<&Claimed>, held: &[PathBuf], ballast: Ballast) -> Freed {
    if claimed.is_some_and(|claimed| (claimed.0)(held)) {
        return Freed::None;
    }
    freed(ballast.free(fs))
}

fn freed(result: io::Result<()>) -> Freed {
    match result {
        Ok(()) => Freed::Freed,
        Err(e) => Freed::Failed(e.kind()),
    }
}

/// A filesystem whose operations a [`DiskWatch`] watches: from
/// [`DiskWatch::fs`]. It does what the filesystem under it does, and
/// returns what it returns; an error for want of space is noted on the
/// watch first, which frees the ballast.
#[derive(Debug)]
pub struct WatchedFs<S> {
    inner: S,
    watch: Arc<DiskWatch<S>>,
}

impl<S: Clone> Clone for WatchedFs<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            watch: Arc::clone(&self.watch),
        }
    }
}

/// A file opened through a [`WatchedFs`].
#[derive(Debug)]
pub struct WatchedFile<S: Fs> {
    file: S::File,
    path: PathBuf,
    watch: Arc<DiskWatch<S>>,
}

/// A directory locked through a [`WatchedFs`]: counted as the watch's own
/// (see [`DiskWatch::share`]) until it's dropped, which unlocks it.
#[derive(Debug)]
pub struct WatchedLock<S: Fs> {
    /// Held until this is dropped.
    _lock: S::Lock,
    dir: PathBuf,
    watch: Arc<DiskWatch<S>>,
}

impl<S: Fs> Drop for WatchedLock<S> {
    /// No longer the watch's own: from here, whoever locks it next may be a
    /// recording. (The lock itself goes just after, with the fields.)
    fn drop(&mut self) {
        let mut state = self
            .watch
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = state.held.iter().position(|d| *d == self.dir) {
            state.held.swap_remove(at);
        }
    }
}

/// Fsyncs a [`WatchedFile`] from any thread.
#[derive(Debug)]
pub struct WatchedSyncer<S: Fs> {
    syncer: <S::File as FsFile>::Syncer,
    path: PathBuf,
    watch: Arc<DiskWatch<S>>,
}

/// Notes `result`'s error on `watch` if it's for want of space.
fn seen<S: Fs + Clone, T>(
    watch: &DiskWatch<S>,
    path: &Path,
    result: io::Result<T>,
) -> io::Result<T> {
    if let Err(e) = &result
        && is_disk_full(e)
    {
        watch.note_full(Some(path));
    }
    result
}

impl<S: Fs + Clone + 'static> Fs for WatchedFs<S> {
    type File = WatchedFile<S>;
    type Lock = WatchedLock<S>;

    fn create(&self, path: &Path) -> io::Result<Self::File> {
        let file = seen(&self.watch, path, self.inner.create(path))?;
        Ok(WatchedFile {
            file,
            path: path.to_path_buf(),
            watch: Arc::clone(&self.watch),
        })
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        seen(&self.watch, path, self.inner.create_dir(path))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        seen(&self.watch, to, self.inner.rename(from, to))
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        seen(&self.watch, dir, self.inner.sync_dir(dir))
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        seen(&self.watch, path, self.inner.remove(path))
    }

    /// Removes an empty directory and reports a full disk to the watch.
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        seen(&self.watch, path, self.inner.remove_dir(path))
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        seen(&self.watch, path, self.inner.sync_file(path))
    }

    fn rename_new(&self, from: &Path, to: &Path) -> io::Result<()> {
        seen(&self.watch, to, self.inner.rename_new(from, to))
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.list(dir)
    }

    /// Locks `dir`, and counts it as the watch's own while it's held.
    fn lock_dir(&self, dir: &Path) -> io::Result<Self::Lock> {
        let lock = self.inner.lock_dir(dir)?;
        self.watch.lock().held.push(dir.to_path_buf());
        Ok(WatchedLock {
            _lock: lock,
            dir: dir.to_path_buf(),
            watch: Arc::clone(&self.watch),
        })
    }

    fn free_space(&self, dir: &Path) -> io::Result<u64> {
        self.inner.free_space(dir)
    }
}

impl<S: Fs + Clone + 'static> FsFile for WatchedFile<S> {
    type Syncer = WatchedSyncer<S>;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        seen(&self.watch, &self.path, self.file.write_all(bytes))
    }

    fn sync(&mut self) -> io::Result<Synced> {
        seen(&self.watch, &self.path, self.file.sync())
    }

    fn syncer(&self) -> io::Result<Self::Syncer> {
        Ok(WatchedSyncer {
            syncer: self.file.syncer()?,
            path: self.path.clone(),
            watch: Arc::clone(&self.watch),
        })
    }
}

impl<S: Fs + Clone + 'static> FileSyncer for WatchedSyncer<S> {
    fn sync(&self) -> io::Result<Synced> {
        seen(&self.watch, &self.path, self.syncer.sync())
    }
}

/// What the [`DiskMonitor`] reports, as it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskReport {
    /// A check: the space free and how long it lasts. The first comes at
    /// once, before the ballast is made.
    Space(Disk),
    /// Under [`LOW_WARNING`] left ([`WarningState::Raised`]), or no longer
    /// ([`WarningState::Cleared`]).
    Low(WarningState),
    /// The disk filled; once. Stop the recording: its open segments finish
    /// into the room the ballast left.
    Full(Full),
    /// The space couldn't be checked, for this reason; once, until a check
    /// works again. Recording goes on.
    Unchecked(String),
    /// After the first check: whether a ballast is held, or why it couldn't
    /// be made. No room for one is no error: there's just none.
    Ballast(Result<bool, String>),
}

/// Where the monitor looks, and what it keeps.
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// The data directory, where the ballast is kept.
    pub data_dir: PathBuf,
    /// The session's audio directory, whose disk is checked.
    pub audio_dir: PathBuf,
    /// The recording's rate of use.
    pub usage: Usage,
    /// The ballast's size: [`BALLAST_LEN`], or less in tests.
    pub ballast_len: u64,
    /// How often to check: [`CHECK_INTERVAL`], or less in tests.
    pub interval: Duration,
}

/// How the disk fared over the recording: what the monitor returns when
/// it stops.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiskSummary {
    /// The disk filled, and how.
    pub full: Option<Full>,
    /// Why the ballast couldn't be kept, if it couldn't (no room is no
    /// error: the ballast is just missing then).
    pub ballast_error: Option<String>,
    /// Whether a ballast was held, once the monitor had kept one.
    pub ballast_held: bool,
    /// Whether the low-disk warning was raised at some point.
    pub low: bool,
    /// Why the space couldn't be checked, the first time it couldn't.
    pub unchecked: Option<String>,
}

/// The disk monitor: a thread that checks the free space every so often,
/// keeps the ballast and reports what it finds (see the module docs).
///
/// Dropped without [`DiskMonitor::stop`], its thread is asked to stop but
/// not waited for.
#[derive(Debug)]
pub struct DiskMonitor<S: Fs + Clone> {
    watch: Arc<DiskWatch<S>>,
    thread: Option<JoinHandle<DiskSummary>>,
}

impl<S: Fs + Clone> Drop for DiskMonitor<S> {
    fn drop(&mut self) {
        self.watch.stop_monitor();
    }
}

/// The monitor's thread panicked; what it found isn't known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorPanicked;

impl fmt::Display for MonitorPanicked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the disk monitor stopped unexpectedly")
    }
}

impl std::error::Error for MonitorPanicked {}

impl<S: Fs + Clone + 'static> DiskMonitor<S> {
    /// Starts the monitor on `watch`, reporting to `report`. Before it
    /// returns, the first check is made and reported, and a ballast already
    /// in the data directory is held, so both come before the recording's
    /// first write. Its thread then makes the ballast if there's none, and
    /// checks every [`MonitorConfig::interval`] until it's stopped,
    /// reporting a full disk as soon as it's noted.
    ///
    /// # Errors
    ///
    /// If the thread can't be spawned.
    pub fn spawn(
        watch: Arc<DiskWatch<S>>,
        config: MonitorConfig,
        mut report: impl FnMut(DiskReport) + Send + 'static,
    ) -> io::Result<Self> {
        let mut checks = Checks::default();
        checks.run(&watch, &config, &mut report);
        let found = Ballast::find(&watch.fs, &config.data_dir, config.ballast_len);
        if let Ok(Some(ballast)) = found {
            watch.hold(ballast);
        }
        let shared = Arc::clone(&watch);
        let thread = thread::Builder::new()
            .name("nota-disk".into())
            .spawn(move || monitor(&shared, &config, checks, report))?;
        Ok(Self {
            watch,
            thread: Some(thread),
        })
    }

    /// Whether the disk has filled so far, and how.
    #[must_use]
    pub fn full(&self) -> Option<Full> {
        self.watch.full()
    }

    /// Stops the monitor and returns how the disk fared.
    ///
    /// # Errors
    ///
    /// [`MonitorPanicked`] if its thread panicked.
    pub fn stop(mut self) -> Result<DiskSummary, MonitorPanicked> {
        self.watch.stop_monitor();
        self.thread
            .take()
            .ok_or(MonitorPanicked)?
            .join()
            .map_err(|_| MonitorPanicked)
    }
}

/// The monitor's thread, after the first check.
fn monitor<S: Fs + Clone>(
    watch: &DiskWatch<S>,
    config: &MonitorConfig,
    mut checks: Checks,
    mut report: impl FnMut(DiskReport),
) -> DiskSummary {
    let mut summary = DiskSummary::default();
    if !watch.holds_ballast() && watch.full().is_none() {
        let give_up = || watch.full().is_some() || watch.stopping();
        match Ballast::keep(&watch.fs, &config.data_dir, config.ballast_len, give_up) {
            Ok(Some(ballast)) => {
                watch.hold(ballast);
                // The space just changed by the ballast's size.
                checks.run(watch, config, &mut report);
            }
            Ok(None) => {}
            Err(e) => summary.ballast_error = Some(e.to_string()),
        }
    }
    summary.ballast_held = watch.holds_ballast();
    report(DiskReport::Ballast(match &summary.ballast_error {
        Some(e) => Err(e.clone()),
        None => Ok(summary.ballast_held),
    }));
    let mut reported_full = false;
    loop {
        if !reported_full && let Some(full) = watch.full() {
            reported_full = true;
            report(DiskReport::Full(full));
        }
        // Woken at once by a full disk not yet reported, or a stop. The
        // stop is read after the wait, whyever it returned, so a wait that
        // returns early never keeps a stopped monitor running.
        watch.wait(config.interval, !reported_full);
        if watch.stopping() {
            break;
        }
        if !reported_full && watch.full().is_some() {
            continue;
        }
        checks.run(watch, config, &mut report);
    }
    summary.full = watch.full();
    summary.low = checks.low.is_some();
    summary.unchecked = checks.first_failure;
    summary
}

/// What the checks have found so far, so a warning is raised or cleared
/// only when it changes.
#[derive(Debug, Default)]
struct Checks {
    /// Whether the warning holds now; `None` if it never has.
    low: Option<bool>,
    unchecked: bool,
    /// A check found at least [`FULL_FLOOR`] free: from then on, one that
    /// finds less marks the disk full.
    seen_room: bool,
    first_failure: Option<String>,
}

impl Checks {
    /// One check, reported; marks the disk full if it's as good as full.
    fn run<S: Fs + Clone>(
        &mut self,
        watch: &DiskWatch<S>,
        config: &MonitorConfig,
        report: &mut impl FnMut(DiskReport),
    ) {
        match check(&watch.fs, &config.audio_dir, config.usage) {
            Ok(disk) => {
                self.unchecked = false;
                report(DiskReport::Space(disk));
                let low = disk.left.is_some_and(|left| left < LOW_WARNING);
                if low != self.low.unwrap_or(false) {
                    self.low = Some(low);
                    report(DiskReport::Low(if low {
                        WarningState::Raised
                    } else {
                        WarningState::Cleared
                    }));
                }
                if disk.free_bytes >= FULL_FLOOR {
                    self.seen_room = true;
                } else if self.seen_room {
                    watch.note_full(None);
                }
            }
            Err(e) => {
                if !self.unchecked {
                    self.unchecked = true;
                    let e = e.to_string();
                    self.first_failure.get_or_insert_with(|| e.clone());
                    report(DiskReport::Unchecked(e));
                }
            }
        }
    }
}

/// A segment store whose commits a [`DiskWatch`] watches, as a
/// [`WatchedFs`] watches the files: SQLite writes the library database
/// itself, so its `SQLITE_FULL` never passes through an [`Fs`]. A commit
/// that fails for want of space ([`SegmentStore::is_disk_full`]) frees the
/// ballast before its error is returned, so the publisher's next try finds
/// room.
#[derive(Debug)]
pub struct WatchedStore<T, S> {
    store: T,
    watch: Arc<DiskWatch<S>>,
    /// The database's file, named in [`Full::path`].
    path: PathBuf,
}

impl<T, S> WatchedStore<T, S> {
    /// `store`, whose database is at `path`, watched by `watch`.
    #[must_use]
    pub fn new(store: T, watch: Arc<DiskWatch<S>>, path: &Path) -> Self {
        Self {
            store,
            watch,
            path: path.to_path_buf(),
        }
    }

    /// Notes `result`'s error on the watch if it's for want of space.
    fn seen<R>(&self, result: Result<R, T::Error>) -> Result<R, T::Error>
    where
        T: SegmentStore,
        S: Fs + Clone,
    {
        if let Err(e) = &result
            && T::is_disk_full(e)
        {
            self.watch.note_full(Some(&self.path));
        }
        result
    }
}

impl<T: SegmentStore, S: Fs + Clone> SegmentStore for WatchedStore<T, S> {
    type Error = T::Error;

    fn rows(
        &mut self,
        session: nota_core::SessionId,
    ) -> Result<Vec<nota_store::SegmentRow>, T::Error> {
        let rows = self.store.rows(session);
        self.seen(rows)
    }

    fn insert(
        &mut self,
        session: nota_core::SessionId,
        segment: &DurableSegment,
    ) -> Result<(), T::Error> {
        let inserted = self.store.insert(session, segment);
        self.seen(inserted)
    }

    fn is_disk_full(error: &T::Error) -> bool {
        T::is_disk_full(error)
    }

    fn unparsable_row(error: &T::Error) -> Option<nota_store::RowKey> {
        T::unparsable_row(error)
    }
}

#[cfg(test)]
mod tests;
