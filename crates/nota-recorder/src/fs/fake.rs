//! A filesystem in memory that records every operation and can crash.
//!
//! # The crash model
//!
//! The fake keeps two views of the disk: what the running system sees, and
//! what is durable.
//! - **File data** is append-only, so the durable part of a file is always a
//!   prefix: what the last [`FsFile::sync`] covered.
//! - **Names** (creates, renames, removes) are durable once
//!   [`Fs::sync_dir`] runs on their directory. Until then they wait, in
//!   order, as pending operations.
//!
//! A crash ([`FakeFs::crash`]) makes a new fake holding only what survived.
//! What survives beyond the durable view is the [`CrashOutcome`]'s choice:
//! - [`CrashOutcome::LoseUnsynced`]: nothing. Every unsynced byte and pending
//!   name is gone.
//! - [`CrashOutcome::KeepAll`]: everything, as if the kernel flushed it all
//!   just before the power went.
//! - [`CrashOutcome::Partial`]: chosen by a seed. Each file keeps some prefix
//!   of its unsynced bytes, which may read back as zeros (the size reached
//!   the disk, the data didn't), and the pending names keep some prefix of
//!   their order, as a journaling filesystem commits metadata in order.
//!
//! A crash also kills the "process": every later operation on the old fake,
//! or on files it opened, fails.
//!
//! Directories are implicit: every path with a parent and a file name is
//! valid, and syncing a directory that holds nothing succeeds.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{Fs, FsFile, Synced, same_directory};

/// One operation on a [`FakeFs`], as recorded in its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// [`Fs::create`].
    Create(PathBuf),
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

/// An in-memory filesystem with a crash model. Cloning shares the same
/// disk, like two handles in one process.
#[derive(Debug, Clone, Default)]
pub struct FakeFs {
    state: Arc<Mutex<State>>,
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
}

/// A change to the names, not yet durable.
#[derive(Debug, Clone)]
enum NameOp {
    Link(PathBuf, InodeId),
    Rename(PathBuf, PathBuf),
    Unlink(PathBuf),
}

impl NameOp {
    /// The directory it changes. Renames stay within one directory.
    fn dir(&self) -> Option<&Path> {
        match self {
            Self::Link(path, _) | Self::Rename(path, _) | Self::Unlink(path) => path.parent(),
        }
    }

    fn apply(&self, names: &mut BTreeMap<PathBuf, InodeId>) {
        match self {
            Self::Link(path, inode) => {
                names.insert(path.clone(), *inode);
            }
            Self::Rename(from, to) => {
                if let Some(inode) = names.remove(from) {
                    names.insert(to.clone(), inode);
                }
            }
            Self::Unlink(path) => {
                names.remove(path);
            }
        }
    }
}

#[derive(Debug, Default)]
struct State {
    inodes: BTreeMap<InodeId, Inode>,
    next_inode: u64,
    /// The names the running system sees.
    names: BTreeMap<PathBuf, InodeId>,
    /// The names on disk.
    durable_names: BTreeMap<PathBuf, InodeId>,
    /// Name changes since their directory was last synced, oldest first.
    pending: Vec<NameOp>,
    log: Vec<Op>,
    /// How many more operations may run before the crash, if one is set.
    budget: Option<usize>,
    crashed: bool,
}

impl State {
    /// Admits one operation, or fails it if the process has crashed or
    /// its operation budget is spent (which crashes it).
    fn admit(&mut self, op: Op) -> io::Result<()> {
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
        self.log.push(op);
        Ok(())
    }

    fn inode(&mut self, id: InodeId) -> io::Result<&mut Inode> {
        self.inodes
            .get_mut(&id)
            .ok_or_else(|| io::Error::other("the fake lost an open file's inode"))
    }
}

fn crashed() -> io::Error {
    io::Error::other("simulated crash")
}

fn not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "no such file")
}

fn valid_path(path: &Path) -> io::Result<()> {
    if path.parent().is_some() && path.file_name().is_some() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a file path needs a directory and a name",
        ))
    }
}

impl FakeFs {
    /// An empty filesystem that never crashes until told to.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty filesystem that runs `ops` operations, then crashes: the next
    /// operation and all after it fail. Reads count as operations.
    #[must_use]
    pub fn crashing_after(ops: usize) -> Self {
        let fs = Self::new();
        fs.lock().budget = Some(ops);
        fs
    }

    /// Lets this filesystem run `ops` more operations, then crash.
    pub fn crash_after(&self, ops: usize) {
        self.lock().budget = Some(ops);
    }

    /// Every operation that has run, in order. Failed ones aren't listed.
    #[must_use]
    pub fn ops(&self) -> Vec<Op> {
        self.lock().log.clone()
    }

    /// Whether the simulated process has crashed.
    #[must_use]
    pub fn has_crashed(&self) -> bool {
        self.lock().crashed
    }

    /// The names the running system sees, in order.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        self.lock().names.keys().cloned().collect()
    }

    /// Crashes now: this filesystem and its open files stop working, and the
    /// returned filesystem holds what survived, as `outcome` decides. Its
    /// log is empty, everything on it is durable, and it doesn't crash
    /// until told to.
    #[must_use]
    pub fn crash(&self, outcome: CrashOutcome) -> Self {
        let mut state = self.lock();
        state.crashed = true;
        let mut rng = SplitMix(match outcome {
            CrashOutcome::Partial { seed } => seed,
            CrashOutcome::LoseUnsynced | CrashOutcome::KeepAll => 0,
        });

        let mut names = state.durable_names.clone();
        let kept_ops = match outcome {
            CrashOutcome::LoseUnsynced => 0,
            CrashOutcome::KeepAll => state.pending.len(),
            CrashOutcome::Partial { .. } => rng.up_to(state.pending.len()),
        };
        for op in &state.pending[..kept_ops] {
            op.apply(&mut names);
        }

        let mut inodes = BTreeMap::new();
        // Every inode in id order, named or not, so the seed's choices don't
        // depend on which names survived.
        for (&id, inode) in &state.inodes {
            let unsynced = inode.data.len() - inode.synced;
            let (kept, zeroed) = match outcome {
                CrashOutcome::LoseUnsynced => (0, false),
                CrashOutcome::KeepAll => (unsynced, false),
                CrashOutcome::Partial { .. } => (rng.up_to(unsynced), rng.coin()),
            };
            let mut data = inode.data[..inode.synced + kept].to_vec();
            if zeroed {
                data[inode.synced..].fill(0);
            }
            let synced = data.len();
            inodes.insert(id, Inode { data, synced });
        }
        inodes.retain(|id, _| names.values().any(|named| named == id));

        Self {
            state: Arc::new(Mutex::new(State {
                inodes,
                next_inode: state.next_inode,
                durable_names: names.clone(),
                names,
                pending: Vec::new(),
                log: Vec::new(),
                budget: None,
                crashed: false,
            })),
        }
    }

    /// A separate copy of this filesystem as the running system sees it,
    /// with everything durable and an empty log. For re-running recovery
    /// from the same starting point.
    #[must_use]
    pub fn copy_disk(&self) -> Self {
        let state = self.lock();
        let inodes = state
            .inodes
            .iter()
            .filter(|(id, _)| state.names.values().any(|named| named == *id))
            .map(|(&id, inode)| {
                let data = inode.data.clone();
                let synced = data.len();
                (id, Inode { data, synced })
            })
            .collect();
        Self {
            state: Arc::new(Mutex::new(State {
                inodes,
                next_inode: state.next_inode,
                names: state.names.clone(),
                durable_names: state.names.clone(),
                pending: Vec::new(),
                log: Vec::new(),
                budget: None,
                crashed: false,
            })),
        }
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

    fn create(&self, path: &Path) -> io::Result<FakeFile> {
        valid_path(path)?;
        let mut state = self.lock();
        if state.names.contains_key(path) {
            // Still counts as an operation: a real create would have run.
            state.admit(Op::Create(path.to_path_buf()))?;
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the file exists",
            ));
        }
        state.admit(Op::Create(path.to_path_buf()))?;
        let id = InodeId(state.next_inode);
        state.next_inode += 1;
        state.inodes.insert(id, Inode::default());
        state.names.insert(path.to_path_buf(), id);
        state.pending.push(NameOp::Link(path.to_path_buf(), id));
        Ok(FakeFile {
            state: Arc::clone(&self.state),
            inode: id,
            path: path.to_path_buf(),
        })
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        same_directory(from, to)?;
        let mut state = self.lock();
        state.admit(Op::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        })?;
        let id = state.names.remove(from).ok_or_else(not_found)?;
        state.names.insert(to.to_path_buf(), id);
        state
            .pending
            .push(NameOp::Rename(from.to_path_buf(), to.to_path_buf()));
        Ok(())
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let mut state = self.lock();
        state.admit(Op::SyncDir(dir.to_path_buf()))?;
        let State {
            pending,
            durable_names,
            ..
        } = &mut *state;
        pending.retain(|op| {
            if op.dir() == Some(dir) {
                op.apply(durable_names);
                false
            } else {
                true
            }
        });
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        let mut state = self.lock();
        state.admit(Op::Remove(path.to_path_buf()))?;
        state.names.remove(path).ok_or_else(not_found)?;
        state.pending.push(NameOp::Unlink(path.to_path_buf()));
        Ok(())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let mut state = self.lock();
        state.admit(Op::Read(path.to_path_buf()))?;
        let id = *state.names.get(path).ok_or_else(not_found)?;
        Ok(state.inode(id)?.data.clone())
    }
}

impl FsFile for FakeFile {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.admit(Op::Write {
            path: self.path.clone(),
            len: bytes.len(),
        })?;
        state.inode(self.inode)?.data.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<Synced> {
        let mut state = lock(&self.state);
        state.admit(Op::Sync(self.path.clone()))?;
        let inode = state.inode(self.inode)?;
        inode.synced = inode.data.len();
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
    fn unsynced_write_is_lost() {
        let fs = FakeFs::new();
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
        let fs = FakeFs::new();
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
        let fs = FakeFs::new();
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
        let fs = FakeFs::new();
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
        let fs = FakeFs::new();
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
        let fs = FakeFs::new();
        let _f = fs.create(&p("/a/x")).unwrap();
        fs.sync_dir(&p("/b")).unwrap();
        assert_eq!(after_crash(&fs, CrashOutcome::LoseUnsynced, "/a/x"), None);
    }

    #[test]
    fn remove_without_a_directory_sync_can_be_undone() {
        let fs = FakeFs::new();
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
        let fs = FakeFs::new();
        let _f = fs.create(&p("/s/j")).unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        fs.remove(&p("/s/j")).unwrap();
        fs.sync_dir(&p("/s")).unwrap();
        assert_eq!(after_crash(&fs, CrashOutcome::KeepAll, "/s/j"), None);
    }

    #[test]
    fn partial_outcomes_keep_a_prefix_and_vary() {
        let fs = FakeFs::new();
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
        let fs = FakeFs::new();
        let mut f = fs.create(&p("/s/j")).unwrap();
        f.write_all(&[7; 100]).unwrap();
        let a = fs.crash(CrashOutcome::Partial { seed: 3 });
        let b = fs.crash(CrashOutcome::Partial { seed: 3 });
        assert_eq!(a.paths(), b.paths());
        assert_eq!(a.read(&p("/s/j")).ok(), b.read(&p("/s/j")).ok());
    }

    #[test]
    fn pending_names_survive_in_order() {
        // Create a then b; a partial crash may keep a alone, never b alone.
        for seed in 0..64 {
            let fs = FakeFs::new();
            let _a = fs.create(&p("/s/a")).unwrap();
            let _b = fs.create(&p("/s/b")).unwrap();
            let after = fs.crash(CrashOutcome::Partial { seed });
            let paths = after.paths();
            assert!(
                paths.is_empty() || paths == [p("/s/a")] || paths == [p("/s/a"), p("/s/b")],
                "seed {seed}: {paths:?}"
            );
        }
    }

    #[test]
    fn crashing_after_n_operations() {
        let fs = FakeFs::crashing_after(2);
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
        let fs = FakeFs::new();
        let mut f = fs.create(&p("/s/j")).unwrap();
        let after = fs.crash(CrashOutcome::KeepAll);
        assert!(f.write_all(b"late").is_err());
        assert!(fs.create(&p("/s/k")).is_err());
        assert_eq!(after.read(&p("/s/j")).unwrap(), b"");
        assert!(after.ops().len() == 1 && !after.has_crashed());
    }

    #[test]
    fn create_refuses_an_existing_name_and_rename_replaces() {
        let fs = FakeFs::new();
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
        let fs = FakeFs::new();
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
        let fs = FakeFs::new();
        assert_eq!(
            fs.create(&p("/s/..")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(fs.ops().is_empty());
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
        let fs = FakeFs::new();
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
    fn copy_disk_is_independent() {
        let fs = FakeFs::new();
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
