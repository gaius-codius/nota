//! A filesystem in memory that records every operation and can crash.
//!
//! # The crash model
//!
//! The fake keeps two views of the disk: what the running system sees, and
//! what is durable.
//! - **File data** is append-only, so the durable part of a file is always a
//!   prefix: what the last [`FsFile::sync`] covered.
//! - **Names** (creating a file or directory, renaming, removing) are durable
//!   once [`Fs::sync_dir`] runs on the directory holding them. Until then
//!   they wait, in order, as pending operations. A new directory's own entry
//!   lives in its parent, so a file in it survives a crash only if both the
//!   file's directory and the new directory's parent were synced.
//!
//! A crash ([`FakeFs::crash`]) makes a new fake holding only what survived.
//! What survives beyond the durable view is the [`CrashOutcome`]'s choice:
//! - [`CrashOutcome::LoseUnsynced`]: nothing. Every unsynced byte and pending
//!   name is gone.
//! - [`CrashOutcome::KeepAll`]: everything, as if the kernel flushed it all
//!   just before the power went.
//! - [`CrashOutcome::Partial`]: chosen by a seed. Each file's size keeps some
//!   of its unsynced growth, and within that each 512-byte block of unsynced
//!   data independently reads back as written or as zeros (written back, or
//!   not, in any order). The pending name changes survive either as a prefix
//!   of their order (as ext4's journal commits them) or as any subset (all
//!   POSIX promises), so a write path can't lean on one filesystem's
//!   ordering.
//!
//! Files and directories left without a surviving parent directory are gone
//! too.
//!
//! A crash also kills the "process": every later operation on the old fake,
//! or on files it opened, fails.
//!
//! # Locks
//!
//! [`Fs::lock_dir`] works as `flock` does: each guard is its own holder, so
//! a second lock on a directory is refused even through the same fake. A
//! lock isn't an operation on the disk: it isn't logged, doesn't count
//! towards [`FakeFs::crash_after`], still works after the crash (every disk
//! operation fails anyway), and is gone from the filesystem the crash
//! returns, as a dead process's locks are.
//!
//! # Failing without crashing
//!
//! [`FakeFs::fail_after`] fails one operation with an I/O error and lets the
//! process carry on, for testing what code does after `ENOSPC` or `EIO`.
//! The fake fails as harshly as Linux may:
//! - A failed [`FsFile::write_all`] appends the first half of its bytes, as
//!   a short write before the error would.
//! - A failed [`FsFile::sync`] leaves the unsynced bytes readable, but no
//!   later sync covers them: Linux marks their pages clean after a
//!   write-back error. After a crash they read back as zeros, even if a
//!   later sync succeeded, unless they were written back before the error:
//!   [`CrashOutcome::LoseUnsynced`] zeroes them, [`CrashOutcome::KeepAll`]
//!   keeps them, and [`CrashOutcome::Partial`] picks for each failed sync.
//!
//! Directory errors match Linux's too: reading, removing or renaming onto a
//! directory is `IsADirectory`, and listing or syncing a file is
//! `NotADirectory`. Renaming a directory is refused (`IsADirectory`), as
//! [`Fs::rename`] says; Linux would move it, but nothing needs that.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{Fs, FsFile, Synced, is_a_directory, same_directory, valid_dir, valid_path};

/// One operation on a [`FakeFs`], as recorded in its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// [`Fs::create`].
    Create(PathBuf),
    /// [`Fs::create_dir`].
    CreateDir(PathBuf),
    /// [`FsFile::write_all`] of `len` bytes to the file created at `path`.
    Write {
        /// Where the file was created.
        path: PathBuf,
        /// How many bytes were written.
        len: usize,
    },
    /// [`FsFile::sync`] of the file created at the path.
    Sync(PathBuf),
    /// [`Fs::rename`].
    Rename {
        /// The old name.
        from: PathBuf,
        /// The new name.
        to: PathBuf,
    },
    /// [`Fs::sync_dir`].
    SyncDir(PathBuf),
    /// [`Fs::remove`].
    Remove(PathBuf),
    /// [`Fs::read`].
    Read(PathBuf),
    /// [`Fs::list`].
    List(PathBuf),
}

/// What survives a crash beyond the durable view (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashOutcome {
    /// Only what was fsync'd survives.
    LoseUnsynced,
    /// Everything survives.
    KeepAll,
    /// Some of what wasn't fsync'd survives, chosen by the seed.
    Partial {
        /// Picks how much survives. The same seed on the same state gives
        /// the same result.
        seed: u64,
    },
}

impl CrashOutcome {
    /// The outcomes a crash test tries by default: the two extremes and
    /// eight partial ones.
    #[must_use]
    pub fn standard() -> Vec<Self> {
        let mut outcomes = vec![Self::LoseUnsynced, Self::KeepAll];
        outcomes.extend((0..8).map(|seed| Self::Partial { seed }));
        outcomes
    }
}

/// The granularity of partial writeback: each block of unsynced data
/// survives or reads back as zeros on its own.
const BLOCK: usize = 512;

/// An in-memory filesystem with a crash model. Cloning shares the same
/// disk, like two handles in one process.
#[derive(Debug, Clone)]
pub struct FakeFs {
    state: Arc<Mutex<State>>,
}

impl Default for FakeFs {
    fn default() -> Self {
        Self::new()
    }
}

/// A directory locked on a [`FakeFs`]; dropping it unlocks.
#[derive(Debug)]
pub struct FakeLock {
    state: Arc<Mutex<State>>,
    dir: PathBuf,
}

impl Drop for FakeLock {
    fn drop(&mut self) {
        lock(&self.state).locked.remove(&self.dir);
    }
}

/// A file opened on a [`FakeFs`].
#[derive(Debug)]
pub struct FakeFile {
    state: Arc<Mutex<State>>,
    inode: InodeId,
    /// The name it was created under, for the log.
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct InodeId(u64);

#[derive(Debug, Clone, Default)]
struct Inode {
    data: Vec<u8>,
    /// How much of `data` the last fsync covered.
    synced: usize,
    /// Ranges of `data` a failed fsync dropped from write-back: readable,
    /// but zeros after a crash.
    lost: Vec<(usize, usize)>,
}

/// The names in a directory tree: files (to their inodes) and directories.
#[derive(Debug, Clone, Default)]
struct Names {
    files: BTreeMap<PathBuf, InodeId>,
    dirs: BTreeSet<PathBuf>,
}

impl Names {
    /// Just the root directory.
    fn root() -> Self {
        Self {
            files: BTreeMap::new(),
            dirs: BTreeSet::from([PathBuf::from("/")]),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.contains_key(path) || self.dirs.contains(path)
    }

    /// Drops everything whose parent directory is gone.
    fn prune(&mut self) {
        loop {
            let orphans: Vec<_> = self
                .dirs
                .iter()
                .filter(|d| d.parent().is_some_and(|p| !self.dirs.contains(p)))
                .cloned()
                .collect();
            if orphans.is_empty() {
                break;
            }
            for d in orphans {
                self.dirs.remove(&d);
            }
        }
        let dirs = &self.dirs;
        self.files
            .retain(|f, _| f.parent().is_some_and(|p| dirs.contains(p)));
    }
}

/// A change to the names, not yet durable.
#[derive(Debug, Clone)]
enum NameOp {
    Link(PathBuf, InodeId),
    MakeDir(PathBuf),
    Rename(PathBuf, PathBuf),
    Unlink(PathBuf),
}

impl NameOp {
    /// The directory whose entries it changes. Renames stay within one
    /// directory.
    fn dir(&self) -> Option<&Path> {
        match self {
            Self::Link(path, _)
            | Self::MakeDir(path)
            | Self::Rename(path, _)
            | Self::Unlink(path) => path.parent(),
        }
    }

    fn apply(&self, names: &mut Names) {
        match self {
            Self::Link(path, inode) => {
                names.files.insert(path.clone(), *inode);
            }
            Self::MakeDir(path) => {
                names.dirs.insert(path.clone());
            }
            Self::Rename(from, to) => {
                if let Some(inode) = names.files.remove(from) {
                    names.files.insert(to.clone(), inode);
                }
            }
            Self::Unlink(path) => {
                names.files.remove(path);
            }
        }
    }
}

#[derive(Debug)]
struct State {
    inodes: BTreeMap<InodeId, Inode>,
    next_inode: u64,
    /// The names the running system sees.
    names: Names,
    /// The names on disk.
    durable: Names,
    /// Name changes since their directory was last synced, oldest first.
    pending: Vec<NameOp>,
    /// Operations that succeeded, in order.
    log: Vec<Op>,
    /// Operations attempted, failed ones included.
    attempted: usize,
    /// How many more operations may run before the crash, if one is set.
    budget: Option<usize>,
    /// An injected failure: after this many more operations, fail the next
    /// one with this error.
    fail: Option<(usize, io::ErrorKind)>,
    crashed: bool,
    /// The directories locked by a live [`FakeLock`].
    locked: BTreeSet<PathBuf>,
}

impl State {
    fn new(inodes: BTreeMap<InodeId, Inode>, next_inode: u64, names: Names) -> Self {
        Self {
            inodes,
            next_inode,
            durable: names.clone(),
            names,
            pending: Vec::new(),
            log: Vec::new(),
            attempted: 0,
            budget: None,
            fail: None,
            crashed: false,
            locked: BTreeSet::new(),
        }
    }

    /// Admits one operation, or fails it: if the process has crashed, if its
    /// operation budget is spent (which crashes it), or if a failure was
    /// injected for it.
    fn admit(&mut self) -> io::Result<()> {
        if self.crashed {
            return Err(crashed());
        }
        if let Some(budget) = self.budget.as_mut() {
            if *budget == 0 {
                self.crashed = true;
                return Err(crashed());
            }
            *budget -= 1;
        }
        self.attempted += 1;
        if let Some((after, kind)) = self.fail.as_mut() {
            if *after == 0 {
                let kind = *kind;
                self.fail = None;
                return Err(io::Error::new(kind, "injected failure"));
            }
            *after -= 1;
        }
        Ok(())
    }

    fn inode(&mut self, id: InodeId) -> io::Result<&mut Inode> {
        self.inodes
            .get_mut(&id)
            .ok_or_else(|| io::Error::other("the fake lost an open file's inode"))
    }

    /// Fails unless `dir` is a directory: `NotADirectory` for a file,
    /// `NotFound` for nothing.
    fn dir_exists(&self, dir: &Path) -> io::Result<()> {
        if self.names.dirs.contains(dir) {
            Ok(())
        } else if self.names.files.contains_key(dir) {
            Err(not_a_directory())
        } else {
            Err(not_found())
        }
    }

    /// Fails unless `path`'s directory exists.
    fn parent_exists(&self, path: &Path) -> io::Result<()> {
        match path.parent() {
            Some(dir) if self.names.dirs.contains(dir) => Ok(()),
            _ => Err(not_found()),
        }
    }

    /// The filesystem that survives a crash now, as `outcome` decides.
    fn survivor(&self, outcome: CrashOutcome) -> State {
        let mut rng = SplitMix(match outcome {
            CrashOutcome::Partial { seed } => seed,
            CrashOutcome::LoseUnsynced | CrashOutcome::KeepAll => 0,
        });

        let mut names = self.durable.clone();
        let kept: Vec<&NameOp> = match outcome {
            CrashOutcome::LoseUnsynced => Vec::new(),
            CrashOutcome::KeepAll => self.pending.iter().collect(),
            CrashOutcome::Partial { .. } => {
                if rng.coin() {
                    // In order, as ext4's journal commits them.
                    let n = rng.up_to(self.pending.len());
                    self.pending[..n].iter().collect()
                } else {
                    // Any subset: POSIX promises no order between name
                    // changes that no directory sync separates.
                    self.pending.iter().filter(|_| rng.coin()).collect()
                }
            }
        };
        for op in kept {
            op.apply(&mut names);
        }
        names.prune();

        let mut inodes = BTreeMap::new();
        // Every inode in id order, named or not, so the seed's choices don't
        // depend on which names survived.
        for (&id, inode) in &self.inodes {
            let unsynced = inode.data.len() - inode.synced;
            let mut data = match outcome {
                CrashOutcome::LoseUnsynced => inode.data[..inode.synced].to_vec(),
                CrashOutcome::KeepAll => inode.data.clone(),
                CrashOutcome::Partial { .. } => {
                    let mut data = inode.data[..inode.synced + rng.up_to(unsynced)].to_vec();
                    // 0: every block written back; 1: none; 2: each on its own.
                    let pattern = rng.up_to(2);
                    for block in data[inode.synced..].chunks_mut(BLOCK) {
                        let zeroed = match pattern {
                            0 => false,
                            1 => true,
                            _ => rng.coin(),
                        };
                        if zeroed {
                            block.fill(0);
                        }
                    }
                    data
                }
            };
            // What a failed fsync dropped from write-back: gone unless the
            // kernel had written it back before the error.
            for &(from, to) in &inode.lost {
                let zeroed = match outcome {
                    CrashOutcome::LoseUnsynced => true,
                    CrashOutcome::KeepAll => false,
                    CrashOutcome::Partial { .. } => rng.coin(),
                };
                let to = to.min(data.len());
                if let (true, Some(range)) = (zeroed, data.get_mut(from..to)) {
                    range.fill(0);
                }
            }
            data.shrink_to_fit();
            let synced = data.len();
            inodes.insert(
                id,
                Inode {
                    data,
                    synced,
                    lost: Vec::new(),
                },
            );
        }
        inodes.retain(|id, _| names.files.values().any(|named| named == id));
        State::new(inodes, self.next_inode, names)
    }
}

fn crashed() -> io::Error {
    io::Error::other("simulated crash")
}

fn not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "no such file or directory")
}

fn exists() -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, "the name exists")
}

/// As Linux's opendir(3) and fsync of a directory handle on a file:
/// `ENOTDIR`.
fn not_a_directory() -> io::Error {
    io::Error::new(io::ErrorKind::NotADirectory, "not a directory")
}

impl FakeFs {
    /// An empty filesystem holding only the root directory, `/`. It never
    /// crashes until told to.
    #[must_use]
    pub fn new() -> Self {
        Self::from_state(State::new(BTreeMap::new(), 0, Names::root()))
    }

    /// A filesystem where `dirs` and their ancestors already exist, durably.
    #[must_use]
    pub fn with_dirs<P: AsRef<Path>>(dirs: impl IntoIterator<Item = P>) -> Self {
        let mut names = Names::root();
        for dir in dirs {
            names
                .dirs
                .extend(dir.as_ref().ancestors().map(Path::to_path_buf));
        }
        names.dirs.remove(Path::new(""));
        Self::from_state(State::new(BTreeMap::new(), 0, names))
    }

    fn from_state(state: State) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// Lets this filesystem run `ops` more operations, then crash: the next
    /// operation and all after it fail. Every operation attempted counts,
    /// reads and failed ones included.
    pub fn crash_after(&self, ops: usize) {
        self.lock().budget = Some(ops);
    }

    /// Fails the operation after the next `ops` with an error of `kind`,
    /// without crashing. A failed write appends half its bytes; a failed
    /// fsync leaves the unsynced bytes readable but not durable (see the
    /// module docs).
    pub fn fail_after(&self, ops: usize, kind: io::ErrorKind) {
        self.lock().fail = Some((ops, kind));
    }

    /// Every operation that succeeded, in order.
    #[must_use]
    pub fn ops(&self) -> Vec<Op> {
        self.lock().log.clone()
    }

    /// How many operations were attempted, failed ones included: the count
    /// [`Self::crash_after`] counts.
    #[must_use]
    pub fn attempted(&self) -> usize {
        self.lock().attempted
    }

    /// Whether the simulated process has crashed.
    #[must_use]
    pub fn has_crashed(&self) -> bool {
        self.lock().crashed
    }

    /// The files the running system sees, in order.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        self.lock().names.files.keys().cloned().collect()
    }

    /// Crashes now: this filesystem and its open files stop working, and the
    /// returned filesystem holds what survived, as `outcome` decides. Its
    /// log is empty, everything on it is durable, and it doesn't crash
    /// until told to.
    #[must_use]
    pub fn crash(&self, outcome: CrashOutcome) -> Self {
        let mut state = self.lock();
        state.crashed = true;
        Self::from_state(state.survivor(outcome))
    }

    /// A separate copy of this filesystem as the running system sees it,
    /// with everything durable and an empty log. For re-running recovery
    /// from the same starting point. Bytes a failed fsync dropped read as
    /// zeros in the copy: they may never have reached the disk.
    #[must_use]
    pub fn copy_disk(&self) -> Self {
        let state = self.lock();
        let inodes = state
            .inodes
            .iter()
            .filter(|(id, _)| state.names.files.values().any(|named| named == *id))
            .map(|(&id, inode)| {
                let mut data = inode.data.clone();
                for &(from, to) in &inode.lost {
                    if let Some(range) = data.get_mut(from..to.min(inode.data.len())) {
                        range.fill(0);
                    }
                }
                let synced = data.len();
                let lost = Vec::new();
                (id, Inode { data, synced, lost })
            })
            .collect();
        Self::from_state(State::new(inodes, state.next_inode, state.names.clone()))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }
}

/// A test that panicked while holding the lock shouldn't hide the state from
/// the next one: the fake's state stays consistent between operations.
fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Fs for FakeFs {
    type File = FakeFile;
    type Lock = FakeLock;

    fn create(&self, path: &Path) -> io::Result<FakeFile> {
        valid_path(path)?;
        let mut state = self.lock();
        state.admit()?;
        state.parent_exists(path)?;
        if state.names.exists(path) {
            return Err(exists());
        }
        let id = InodeId(state.next_inode);
        state.next_inode += 1;
        state.inodes.insert(id, Inode::default());
        state.names.files.insert(path.to_path_buf(), id);
        state.pending.push(NameOp::Link(path.to_path_buf(), id));
        state.log.push(Op::Create(path.to_path_buf()));
        Ok(FakeFile {
            state: Arc::clone(&self.state),
            inode: id,
            path: path.to_path_buf(),
        })
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        valid_path(path)?;
        let mut state = self.lock();
        state.admit()?;
        state.parent_exists(path)?;
        if state.names.exists(path) {
            return Err(exists());
        }
        state.names.dirs.insert(path.to_path_buf());
        state.pending.push(NameOp::MakeDir(path.to_path_buf()));
        state.log.push(Op::CreateDir(path.to_path_buf()));
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        same_directory(from, to)?;
        let mut state = self.lock();
        state.admit()?;
        if state.names.dirs.contains(from) {
            return Err(is_a_directory());
        }
        let id = *state.names.files.get(from).ok_or_else(not_found)?;
        if state.names.dirs.contains(to) {
            return Err(is_a_directory());
        }
        state.names.files.remove(from);
        state.names.files.insert(to.to_path_buf(), id);
        state
            .pending
            .push(NameOp::Rename(from.to_path_buf(), to.to_path_buf()));
        state.log.push(Op::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        Ok(())
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        valid_dir(dir)?;
        let mut state = self.lock();
        state.admit()?;
        state.dir_exists(dir)?;
        let State {
            pending, durable, ..
        } = &mut *state;
        pending.retain(|op| {
            if op.dir() == Some(dir) {
                op.apply(durable);
                false
            } else {
                true
            }
        });
        state.log.push(Op::SyncDir(dir.to_path_buf()));
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        valid_path(path)?;
        let mut state = self.lock();
        state.admit()?;
        if state.names.dirs.contains(path) {
            return Err(is_a_directory());
        }
        state.names.files.remove(path).ok_or_else(not_found)?;
        state.pending.push(NameOp::Unlink(path.to_path_buf()));
        state.log.push(Op::Remove(path.to_path_buf()));
        Ok(())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        valid_path(path)?;
        let mut state = self.lock();
        state.admit()?;
        if state.names.dirs.contains(path) {
            return Err(is_a_directory());
        }
        let id = *state.names.files.get(path).ok_or_else(not_found)?;
        let data = state.inode(id)?.data.clone();
        state.log.push(Op::Read(path.to_path_buf()));
        Ok(data)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        valid_dir(dir)?;
        let mut state = self.lock();
        state.admit()?;
        state.dir_exists(dir)?;
        let in_dir = |p: &&PathBuf| p.parent() == Some(dir);
        let mut entries: Vec<PathBuf> = state.names.files.keys().filter(in_dir).cloned().collect();
        entries.extend(state.names.dirs.iter().filter(in_dir).cloned());
        entries.sort();
        state.log.push(Op::List(dir.to_path_buf()));
        Ok(entries)
    }

    fn lock_dir(&self, dir: &Path) -> io::Result<FakeLock> {
        valid_path(dir)?;
        let mut state = self.lock();
        state.dir_exists(dir)?;
        if !state.locked.insert(dir.to_path_buf()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the directory is locked",
            ));
        }
        Ok(FakeLock {
            state: Arc::clone(&self.state),
            dir: dir.to_path_buf(),
        })
    }
}

impl FsFile for FakeFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = lock(&self.state);
        if let Err(e) = state.admit() {
            if !state.crashed {
                // A short write, then the error.
                let half = &bytes[..bytes.len() / 2];
                state.inode(self.inode)?.data.extend_from_slice(half);
            }
            return Err(e);
        }
        state.inode(self.inode)?.data.extend_from_slice(bytes);
        state.log.push(Op::Write {
            path: self.path.clone(),
            len: bytes.len(),
        });
        Ok(())
    }

    fn sync(&mut self) -> io::Result<Synced> {
        let mut state = lock(&self.state);
        if let Err(e) = state.admit() {
            if !state.crashed {
                // Linux after a failed fsync: the unsynced pages are marked
                // clean, so they read back but never reach the disk.
                let inode = state.inode(self.inode)?;
                let range = (inode.synced, inode.data.len());
                inode.lost.push(range);
            }
            return Err(e);
        }
        let inode = state.inode(self.inode)?;
        inode.synced = inode.data.len();
        state.log.push(Op::Sync(self.path.clone()));
        Ok(Synced::after_fsync())
    }
}

/// `SplitMix64`: a tiny, well-mixed generator, so crash outcomes need no
/// random-number dependency.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number from 0 to `max`, both included.
    fn up_to(&mut self, max: usize) -> usize {
        let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
        // Fits: the result is at most `max`.
        usize::try_from(self.next() % span).unwrap_or(max)
    }

    fn coin(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Whether `path` reads back as `want` after crashing with `outcome`.
    fn after_crash(fs: &FakeFs, outcome: CrashOutcome, path: &str) -> Option<Vec<u8>> {
        fs.crash(outcome).read(&p(path)).ok()
    }

    #[test]
    fn a_lock_is_exclusive_until_dropped_and_gone_after_a_crash() {
        let fs = FakeFs::with_dirs(["/s"]);
        let held = fs.lock_dir(&p("/s")).unwrap();
        assert_eq!(
            fs.clone().lock_dir(&p("/s")).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(held);
        let held = fs.lock_dir(&p("/s")).unwrap();
        // Not a disk operation: nothing logged or counted.
        assert!(fs.ops().is_empty());
        assert_eq!(fs.attempted(), 0);
        let after = fs.crash(CrashOutcome::KeepAll);
        let _new = after.lock_dir(&p("/s")).unwrap();
        assert_eq!(
            fs.lock_dir(&p("/s")).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(held);
    }

    #[test]
    fn lock_dir_needs_an_existing_directory() {
        let fs = FakeFs::with_dirs(["/s"]);
        assert_eq!(
            fs.lock_dir(&p("s")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            fs.lock_dir(&p("/nope")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let _f = fs.create(&p("/s/journal")).unwrap();
        assert_eq!(
            fs.lock_dir(&p("/s/journal")).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
    }

    #[test]
    fn unsynced_write_is_lost() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/journal")).unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        f.write_all(b"abc").unwrap();
        assert_eq!(fs.read(&p("/s/journal")).unwrap(), b"abc");
        assert_eq!(
            after_crash(&fs, CrashOutcome::LoseUnsynced, "/s/journal"),
            Some(Vec::new())
        );
    }

    #[test]
    fn synced_data_survives_every_outcome() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/journal")).unwrap();
        f.write_all(b"durable").unwrap();
        f.sync().unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        f.write_all(b" maybe").unwrap();
        for outcome in CrashOutcome::standard() {
            let got = after_crash(&fs, outcome, "/s/journal").unwrap();
            assert!(got.starts_with(b"durable"), "{outcome:?}: {got:?}");
            assert!(got.len() <= b"durable maybe".len(), "{outcome:?}");
        }
        assert_eq!(
            after_crash(&fs, CrashOutcome::KeepAll, "/s/journal").unwrap(),
            b"durable maybe"
        );
    }

    #[test]
    fn synced_file_without_a_directory_sync_can_vanish() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/journal")).unwrap();
        f.write_all(b"abc").unwrap();
        f.sync().unwrap();
        assert_eq!(
            after_crash(&fs, CrashOutcome::LoseUnsynced, "/s/journal"),
            None
        );
        assert_eq!(
            after_crash(&fs, CrashOutcome::KeepAll, "/s/journal"),
            Some(b"abc".to_vec())
        );
    }

    #[test]
    fn rename_without_a_directory_sync_can_be_lost() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/seg.tmp")).unwrap();
        f.write_all(b"flac").unwrap();
        f.sync().unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        fs.rename(&p("/s/seg.tmp"), &p("/s/seg")).unwrap();

        let lost = fs.crash(CrashOutcome::LoseUnsynced);
        assert_eq!(lost.read(&p("/s/seg.tmp")).unwrap(), b"flac");
        assert!(lost.read(&p("/s/seg")).is_err());

        let kept = fs.crash(CrashOutcome::KeepAll);
        assert!(kept.read(&p("/s/seg.tmp")).is_err());
        assert_eq!(kept.read(&p("/s/seg")).unwrap(), b"flac");
    }

    #[test]
    fn rename_then_directory_sync_is_durable() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/seg.tmp")).unwrap();
        f.write_all(b"flac").unwrap();
        f.sync().unwrap();
        fs.rename(&p("/s/seg.tmp"), &p("/s/seg")).unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        for outcome in CrashOutcome::standard() {
            let after = fs.crash(outcome);
            assert_eq!(after.read(&p("/s/seg")).unwrap(), b"flac", "{outcome:?}");
            assert!(after.read(&p("/s/seg.tmp")).is_err(), "{outcome:?}");
        }
    }

    #[test]
    fn syncing_another_directory_doesnt_make_names_durable() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let _f = fs.create(&p("/a/x")).unwrap();
        fs.sync_dir(&p("/b")).unwrap();
        assert_eq!(after_crash(&fs, CrashOutcome::LoseUnsynced, "/a/x"), None);
    }

    #[test]
    fn remove_without_a_directory_sync_can_be_undone() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/j")).unwrap();
        f.write_all(b"pcm").unwrap();
        f.sync().unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        fs.remove(&p("/s/j")).unwrap();
        assert!(fs.read(&p("/s/j")).is_err());
        assert_eq!(
            after_crash(&fs, CrashOutcome::LoseUnsynced, "/s/j"),
            Some(b"pcm".to_vec())
        );
        fs.sync_dir(&p("/s")).unwrap_err(); // the crash above killed `fs`
    }

    #[test]
    fn remove_then_directory_sync_is_durable() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let _f = fs.create(&p("/s/j")).unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        fs.remove(&p("/s/j")).unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        assert_eq!(after_crash(&fs, CrashOutcome::KeepAll, "/s/j"), None);
    }

    #[test]
    fn partial_outcomes_keep_a_prefix_and_vary() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/j")).unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        f.write_all(b"head").unwrap();
        f.sync().unwrap();
        f.write_all(b"0123456789").unwrap();
        let mut lengths = std::collections::BTreeSet::new();
        let mut zeroed = false;
        for seed in 0..64 {
            let got = after_crash(&fs, CrashOutcome::Partial { seed }, "/s/j").unwrap();
            assert!(got.starts_with(b"head"));
            let tail = &got[4..];
            assert!(tail.len() <= 10);
            if tail.iter().all(|&b| b == 0) {
                zeroed |= !tail.is_empty();
            } else {
                assert_eq!(tail, &b"0123456789"[..tail.len()]);
            }
            lengths.insert(got.len());
        }
        assert!(
            lengths.len() > 5,
            "partial crashes barely vary: {lengths:?}"
        );
        assert!(zeroed, "no partial crash zeroed the tail");
    }

    #[test]
    fn same_seed_same_crash() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/j")).unwrap();
        f.write_all(&[7; 100]).unwrap();
        let a = fs.crash(CrashOutcome::Partial { seed: 3 });
        let b = fs.crash(CrashOutcome::Partial { seed: 3 });
        assert_eq!(a.paths(), b.paths());
        assert_eq!(a.read(&p("/s/j")).ok(), b.read(&p("/s/j")).ok());
    }

    #[test]
    fn pending_names_survive_in_any_combination() {
        // Create a then b, no directory sync: a partial crash can keep
        // neither, either one alone, or both.
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..64 {
            let fs = FakeFs::with_dirs(["/s"]);
            let _a = fs.create(&p("/s/a")).unwrap();
            let _b = fs.create(&p("/s/b")).unwrap();
            seen.insert(fs.crash(CrashOutcome::Partial { seed }).paths());
        }
        let all: std::collections::BTreeSet<Vec<PathBuf>> = [
            vec![],
            vec![p("/s/a")],
            vec![p("/s/b")],
            vec![p("/s/a"), p("/s/b")],
        ]
        .into();
        assert_eq!(seen, all);
    }

    #[test]
    fn crashing_after_n_operations() {
        let fs = FakeFs::with_dirs(["/s"]);
        fs.crash_after(2);
        let mut f = fs.create(&p("/s/j")).unwrap();
        f.write_all(b"a").unwrap();
        assert!(!fs.has_crashed());
        assert!(f.write_all(b"b").is_err());
        assert!(fs.has_crashed());
        assert!(f.sync().is_err());
        assert!(fs.read(&p("/s/j")).is_err());
        assert_eq!(
            fs.ops(),
            [
                Op::Create(p("/s/j")),
                Op::Write {
                    path: p("/s/j"),
                    len: 1
                }
            ]
        );
    }

    #[test]
    fn crash_kills_open_files() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/j")).unwrap();
        let after = fs.crash(CrashOutcome::KeepAll);
        assert!(f.write_all(b"late").is_err());
        assert!(fs.create(&p("/s/k")).is_err());
        assert_eq!(after.read(&p("/s/j")).unwrap(), b"");
        assert!(after.ops().len() == 1 && !after.has_crashed());
    }

    #[test]
    fn create_refuses_an_existing_name_and_rename_replaces() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut a = fs.create(&p("/s/a")).unwrap();
        a.write_all(b"A").unwrap();
        assert_eq!(
            fs.create(&p("/s/a")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let mut b = fs.create(&p("/s/b")).unwrap();
        b.write_all(b"B").unwrap();
        fs.rename(&p("/s/b"), &p("/s/a")).unwrap();
        assert_eq!(fs.read(&p("/s/a")).unwrap(), b"B");
        assert_eq!(fs.paths(), [p("/s/a")]);
        assert_eq!(
            fs.rename(&p("/s/b"), &p("/s/c")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            fs.rename(&p("/s/a"), &p("/t/a")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            fs.create(&p("/")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn files_keep_their_own_contents() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut a = fs.create(&p("/s/a")).unwrap();
        a.write_all(b"A").unwrap();
        let mut b = fs.create(&p("/s/b")).unwrap();
        b.write_all(b"B").unwrap();
        a.write_all(b"a").unwrap();
        assert_eq!(fs.read(&p("/s/a")).unwrap(), b"Aa");
        assert_eq!(fs.read(&p("/s/b")).unwrap(), b"B");
    }

    #[test]
    fn a_path_needs_a_file_name() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        assert_eq!(
            fs.create(&p("/s/..")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let bare = p("j");
        let kinds = [
            fs.remove(&bare).unwrap_err().kind(),
            fs.read(&bare).unwrap_err().kind(),
            fs.list(&bare).unwrap_err().kind(),
            fs.sync_dir(&bare).unwrap_err().kind(),
        ];
        assert_eq!(kinds, [io::ErrorKind::InvalidInput; 4]);
        // An absolute path still needs a name, unless it's the root.
        assert_eq!(
            fs.list(&p("/s/..")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            fs.sync_dir(&p("/s/..")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            fs.remove(&p("/")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(fs.ops().is_empty());
        // Refused before counting as an operation.
        assert_eq!(fs.attempted(), 0);
    }

    #[test]
    fn splitmix_matches_the_reference() {
        // The first outputs of SplitMix64 seeded with 0 and 1, from the
        // reference implementation.
        let mut zero = SplitMix(0);
        assert_eq!(zero.next(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(zero.next(), 0x6E78_9E6A_A1B9_65F4);
        let mut one = SplitMix(1);
        assert_eq!(one.next(), 0x910A_2DEC_8902_5CC1);
    }

    #[test]
    fn partial_crashes_sometimes_keep_the_real_bytes() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/j")).unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        f.write_all(b"0123456789").unwrap();
        let kept_real = (0..64).any(|seed| {
            let got = after_crash(&fs, CrashOutcome::Partial { seed }, "/s/j").unwrap();
            !got.is_empty() && got == b"0123456789"[..got.len()]
        });
        assert!(kept_real);
    }

    #[test]
    fn a_new_directory_needs_its_parent_synced() {
        let fs = FakeFs::new();
        fs.create_dir(&p("/rec")).unwrap();
        let mut f = fs.create(&p("/rec/j")).unwrap();
        f.write_all(b"pcm").unwrap();
        f.sync().unwrap();
        fs.sync_dir(&p("/rec")).unwrap();
        // The file's own entry is durable, its directory's isn't.
        assert_eq!(after_crash(&fs, CrashOutcome::LoseUnsynced, "/rec/j"), None);
        let fs = FakeFs::new();
        fs.create_dir(&p("/rec")).unwrap();
        fs.sync_dir(&p("/")).unwrap();
        let mut f = fs.create(&p("/rec/j")).unwrap();
        f.write_all(b"pcm").unwrap();
        f.sync().unwrap();
        fs.sync_dir(&p("/rec")).unwrap();
        for outcome in CrashOutcome::standard() {
            assert_eq!(
                after_crash(&fs, outcome, "/rec/j"),
                Some(b"pcm".to_vec()),
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn directories_must_exist() {
        let fs = FakeFs::new();
        let kind = |r: io::Result<()>| r.unwrap_err().kind();
        assert_eq!(
            fs.create(&p("/rec/j")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(kind(fs.create_dir(&p("/rec/sub"))), io::ErrorKind::NotFound);
        assert_eq!(kind(fs.sync_dir(&p("/rec"))), io::ErrorKind::NotFound);
        assert_eq!(
            fs.list(&p("/rec")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs.create_dir(&p("/rec")).unwrap();
        assert_eq!(
            kind(fs.create_dir(&p("/rec"))),
            io::ErrorKind::AlreadyExists
        );
        let _f = fs.create(&p("/rec/j")).unwrap();
        assert_eq!(
            kind(fs.create_dir(&p("/rec/j"))),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            fs.create(&p("/rec")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        fs.create_dir(&p("/rec/sub")).unwrap();
        assert_eq!(
            fs.rename(&p("/rec/j"), &p("/rec/sub")).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(fs.list(&p("/rec")).unwrap(), [p("/rec/j"), p("/rec/sub")]);
        assert_eq!(fs.list(&p("/")).unwrap(), [p("/rec")]);
        // Eight of them failed.
        assert_eq!(fs.attempted(), fs.ops().len() + 8);
    }

    #[test]
    fn with_dirs_makes_ancestors_durable() {
        let fs = FakeFs::with_dirs(["/home/u/rec"]);
        let after = fs.crash(CrashOutcome::LoseUnsynced);
        assert_eq!(after.list(&p("/home")).unwrap(), [p("/home/u")]);
        assert_eq!(
            after.list(&p("/home/u/rec")).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn an_injected_failure_doesnt_crash() {
        let fs = FakeFs::with_dirs(["/s"]);
        let mut f = fs.create(&p("/s/j")).unwrap();
        fs.fail_after(1, io::ErrorKind::StorageFull);
        f.write_all(b"ok").unwrap();
        assert_eq!(
            f.write_all(b"full").unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert!(!fs.has_crashed());
        f.write_all(b"!").unwrap();
        // The failed write left half its bytes, as a short write can.
        assert_eq!(fs.read(&p("/s/j")).unwrap(), b"okfu!");
        // Only the successes are in the log.
        assert_eq!(fs.ops().len(), 4);
        assert_eq!(fs.attempted(), 5);
    }

    #[test]
    fn a_failed_fsync_keeps_the_data_readable_but_never_durable() {
        let failed_sync = || {
            let fs = FakeFs::with_dirs(["/s"]);
            let mut f = fs.create(&p("/s/j")).unwrap();
            fs.sync_dir(&p("/s")).unwrap();
            f.write_all(b"safe").unwrap();
            f.sync().unwrap();
            f.write_all(b"lost").unwrap();
            fs.fail_after(0, io::ErrorKind::Other);
            assert!(f.sync().is_err());
            assert!(!fs.has_crashed());
            assert_eq!(fs.read(&p("/s/j")).unwrap(), b"safelost");
            (fs, f)
        };
        // Before another sync, the dropped bytes are zeros, kept or gone.
        for outcome in CrashOutcome::standard() {
            let (fs, _f) = failed_sync();
            let got = after_crash(&fs, outcome, "/s/j").unwrap();
            let zeroed = b"safe\0\0\0\0".starts_with(&got);
            assert!(
                zeroed || b"safelost".starts_with(&got),
                "{outcome:?}: {got:?}"
            );
            assert!(got.len() >= 4, "{outcome:?}");
        }
        // A later sync makes what follows durable, but not them: they're
        // zeros unless written back before the error, and both happen.
        let mut seen = BTreeSet::new();
        for outcome in CrashOutcome::standard() {
            let (fs, mut f) = failed_sync();
            f.write_all(b"next").unwrap();
            f.sync().unwrap();
            assert_eq!(fs.read(&p("/s/j")).unwrap(), b"safelostnext");
            let got = after_crash(&fs, outcome, "/s/j").unwrap();
            let want: &[u8] = match outcome {
                CrashOutcome::LoseUnsynced => b"safe\0\0\0\0next",
                CrashOutcome::KeepAll => b"safelostnext",
                CrashOutcome::Partial { .. } => &got,
            };
            assert_eq!(got, want, "{outcome:?}");
            assert!(
                [&b"safe\0\0\0\0next"[..], b"safelostnext"].contains(&&got[..]),
                "{outcome:?}: {got:?}"
            );
            seen.insert(got);
        }
        assert_eq!(seen.len(), 2, "partial crashes try both");
        // A copy of the running disk doesn't promote them to durable.
        let (fs, _f) = failed_sync();
        assert_eq!(fs.copy_disk().read(&p("/s/j")).unwrap(), b"safe\0\0\0\0");
    }

    #[test]
    fn a_failed_write_leaves_a_prefix() {
        let fs = FakeFs::with_dirs(["/s"]);
        let mut f = fs.create(&p("/s/j")).unwrap();
        fs.fail_after(0, io::ErrorKind::StorageFull);
        assert_eq!(
            f.write_all(b"abcdefg").unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(fs.read(&p("/s/j")).unwrap(), b"abc");
        // Not in the log, which holds successes only.
        assert_eq!(fs.ops(), [Op::Create(p("/s/j")), Op::Read(p("/s/j"))]);
        // A crash, unlike a failure, writes nothing.
        fs.crash_after(0);
        assert!(f.write_all(b"xy").is_err());
        assert_eq!(
            fs.crash(CrashOutcome::KeepAll).read(&p("/s/j")).unwrap(),
            b"abc"
        );
    }

    #[test]
    fn directory_errors_match_linux() {
        let fs = FakeFs::with_dirs(["/s", "/s/d"]);
        let _file = fs.create(&p("/s/f")).unwrap();
        assert_eq!(
            fs.sync_dir(&p("/s/f")).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(
            fs.list(&p("/s/f")).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(
            fs.lock_dir(&p("/s/f")).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(
            fs.rename(&p("/s/d"), &p("/s/e")).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(fs.list(&p("/s")).unwrap(), [p("/s/d"), p("/s/f")]);
        // StdFs agrees.
        let dir = crate::test_dir::TestDir::new("fake-dir-errors");
        let (sub, file) = (dir.0.join("d"), dir.0.join("f"));
        crate::fs::StdFs.create_dir(&sub).unwrap();
        let _file = crate::fs::StdFs.create(&file).unwrap();
        assert_eq!(
            crate::fs::StdFs.sync_dir(&file).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(
            crate::fs::StdFs.list(&file).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(
            crate::fs::StdFs
                .rename(&sub, &dir.0.join("e"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::IsADirectory
        );
        assert!(sub.is_dir());
    }

    #[test]
    fn partial_crashes_can_lose_a_block_and_keep_a_later_one() {
        let fs = FakeFs::with_dirs(["/s"]);
        let mut f = fs.create(&p("/s/j")).unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        let data: Vec<u8> = (1..=u8::MAX).cycle().take(4 * BLOCK).collect();
        f.write_all(&data).unwrap();
        let mut hole_then_data = false;
        for seed in 0..200 {
            let got = after_crash(&fs, CrashOutcome::Partial { seed }, "/s/j").unwrap();
            let blocks: Vec<bool> = got
                .chunks(BLOCK)
                .map(|b| b.iter().all(|&x| x == 0))
                .collect();
            for (i, block) in got.chunks(BLOCK).enumerate() {
                let want = &data[i * BLOCK..i * BLOCK + block.len()];
                assert!(blocks[i] || block == want, "seed {seed} block {i}");
            }
            hole_then_data |= blocks.windows(2).any(|w| w[0] && !w[1]);
        }
        assert!(hole_then_data, "no crash kept data after a lost block");
    }

    #[test]
    fn reading_a_directory_fails_as_on_linux() {
        let fs = FakeFs::with_dirs(["/s", "/s/d"]);
        assert_eq!(
            fs.read(&p("/s/d")).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(
            fs.read(&p("/s/none")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        // StdFs agrees.
        let dir = crate::test_dir::TestDir::new("fake-read-dir");
        assert_eq!(
            crate::fs::StdFs.read(&dir.0).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
    }

    #[test]
    fn removing_or_renaming_onto_a_directory_fails_as_on_linux() {
        let fs = FakeFs::with_dirs(["/s", "/s/d"]);
        let _file = fs.create(&p("/s/f")).unwrap();
        assert_eq!(
            fs.remove(&p("/s/d")).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(
            fs.rename(&p("/s/f"), &p("/s/d")).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(fs.paths(), [p("/s/f")]);
        assert_eq!(fs.list(&p("/s")).unwrap(), [p("/s/d"), p("/s/f")]);
        // A missing source is reported first.
        assert_eq!(
            fs.rename(&p("/s/none"), &p("/s/d")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        // StdFs agrees.
        let dir = crate::test_dir::TestDir::new("fake-remove-dir");
        let sub = dir.0.join("d");
        let file = dir.0.join("f");
        crate::fs::StdFs.create_dir(&sub).unwrap();
        let _file = crate::fs::StdFs.create(&file).unwrap();
        assert_eq!(
            crate::fs::StdFs.remove(&sub).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(
            crate::fs::StdFs.rename(&file, &sub).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert!(sub.is_dir() && file.is_file());
        assert_eq!(
            crate::fs::StdFs
                .rename(&dir.0.join("none"), &sub)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn copy_disk_is_independent() {
        let fs = FakeFs::with_dirs(["/s", "/a", "/b"]);
        let mut f = fs.create(&p("/s/j")).unwrap();
        f.write_all(b"x").unwrap();
        let copy = fs.copy_disk();
        f.write_all(b"y").unwrap();
        assert_eq!(copy.read(&p("/s/j")).unwrap(), b"x");
        assert_eq!(
            copy.crash(CrashOutcome::LoseUnsynced)
                .read(&p("/s/j"))
                .unwrap(),
            b"x",
            "a copy's contents are all durable"
        );
    }
}
