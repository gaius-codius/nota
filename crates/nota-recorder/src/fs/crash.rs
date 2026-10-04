//! Crash after every operation: the exhaustive crash test.
//!
//! [`CrashTest`] runs a write scenario on a [`FakeFs`] once to count its
//! operations, then again for every crash point: after 0 operations, after
//! 1, and so on to the end. At each point it crashes with every
//! [`CrashOutcome`] it was given, runs recovery on what survived, and asks
//! the check whether the invariants hold.
//!
//! With [`CrashTest::crash_recovery`], recovery itself is crashed after each
//! of its operations too, and run again on what survived that: salvage must
//! be safe to repeat. [`CrashTest::recovery_outcomes`] crashes recovery with
//! other outcomes than the scenario's, [`CrashTest::sample_recovery`] keeps
//! only some of those crashes when there are too many to run, and
//! [`CrashTest::crash_rerun`] crashes the re-run as well.
//!
//! The tests below show the shape: a segment publish (temp file, fsync,
//! rename, directory sync) and a recovery that finishes it.

use std::fmt;
use std::path::{Path, PathBuf};

use super::fake::{CrashOutcome, FakeFs, Op};

/// One crash of a recovery run: after how many of its operations, and what
/// survived it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryCrash {
    /// How many operations recovery attempted before the crash.
    pub after_ops: usize,
    /// What survived it.
    pub outcome: CrashOutcome,
}

/// Where one crash test case crashed, handed to the check.
#[derive(Debug)]
pub struct CrashCase {
    /// How many operations the scenario attempted before the crash.
    pub after_ops: usize,
    /// The scenario's operations that succeeded, in order.
    pub ops: Vec<Op>,
    /// What survived the crash.
    pub outcome: CrashOutcome,
    /// The recovery runs crashed before the one that finished, in order:
    /// none, recovery's first run, or that and its re-run.
    pub recovery_crashes: Vec<RecoveryCrash>,
    /// What survived the scenario's crash, before any recovery ran: for a
    /// check that compares with an uninterrupted recovery. Use a
    /// [`FakeFs::copy_disk`] of it.
    pub survived: FakeFs,
    /// The filesystem the final recovery runs on: what survived the last
    /// crash.
    pub fs: FakeFs,
}

impl fmt::Display for CrashCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "crash after {} ops (last: {:?}), {:?}",
            self.after_ops,
            self.ops.last(),
            self.outcome
        )?;
        for (i, crash) in self.recovery_crashes.iter().enumerate() {
            let run = if i == 0 { "recovery" } else { "its re-run" };
            write!(
                f,
                ", {run} crashed after {} ops, {:?}",
                crash.after_ops, crash.outcome
            )?;
        }
        Ok(())
    }
}

/// The first case whose check failed.
#[derive(Debug)]
pub struct CrashFailure {
    /// Where it crashed.
    pub case: String,
    /// What the check said.
    pub message: String,
}

impl fmt::Display for CrashFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.case, self.message)
    }
}

impl std::error::Error for CrashFailure {}

/// What a passing crash test covered, so a test can check it wasn't vacuous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrashSummary {
    /// Operations attempted in an uncrashed run of the scenario.
    pub scenario_ops: usize,
    /// Cases checked.
    pub cases: usize,
    /// Of those, the cases whose recovery was crashed once.
    pub recovery_crashed: usize,
    /// Of those, the cases whose recovery's re-run was crashed too.
    pub rerun_crashed: usize,
}

/// How recovery is crashed.
#[derive(Debug, Clone)]
struct RecoveryCrashes {
    /// The outcomes to crash it with; `None` for the scenario's own.
    outcomes: Option<Vec<CrashOutcome>>,
    /// Keep every this many recovery crashes.
    every: usize,
    /// Crash the re-run of every this many kept recovery crashes.
    rerun_every: Option<usize>,
}

/// A crash test: a scenario, a recovery, and the invariants to check.
///
/// - The **scenario** runs the write path on the filesystem it's given and
///   returns what it observed or promised before it stopped (say, the last
///   durable position it reported). It must be deterministic, and should stop
///   at the first error, as the real code would.
/// - **Recovery** runs on what survived the crash and returns what it
///   recovered. It must be deterministic too.
/// - The **check** compares the two and says what's wrong, if anything.
pub struct CrashTest<S, R, C> {
    scenario: S,
    recover: R,
    check: C,
    outcomes: Vec<CrashOutcome>,
    recovery: Option<RecoveryCrashes>,
    dirs: Vec<PathBuf>,
}

impl<S, R, C> fmt::Debug for CrashTest<S, R, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrashTest")
            .field("outcomes", &self.outcomes)
            .field("recovery", &self.recovery)
            .field("dirs", &self.dirs)
            .finish_non_exhaustive()
    }
}

impl<S, R, C, O, T> CrashTest<S, R, C>
where
    S: Fn(&FakeFs) -> O,
    R: Fn(&FakeFs) -> T,
    C: Fn(&CrashCase, &O, &T) -> Result<(), String>,
{
    /// A crash test with [`CrashOutcome::standard`] outcomes, not crashing
    /// recovery.
    pub fn new(scenario: S, recover: R, check: C) -> Self {
        Self {
            scenario,
            recover,
            check,
            outcomes: CrashOutcome::standard(),
            recovery: None,
            dirs: Vec::new(),
        }
    }

    /// Starts each run on a filesystem where these directories already
    /// exist durably (see [`FakeFs::with_dirs`]). Without it, only `/` does.
    #[must_use]
    pub fn dirs<P: AsRef<Path>>(mut self, dirs: impl IntoIterator<Item = P>) -> Self {
        self.dirs = dirs.into_iter().map(|d| d.as_ref().to_path_buf()).collect();
        self
    }

    /// Crashes with these outcomes instead.
    #[must_use]
    pub fn outcomes(mut self, outcomes: Vec<CrashOutcome>) -> Self {
        self.outcomes = outcomes;
        self
    }

    /// Also crashes recovery after each of its operations (and before the
    /// first), with the same outcome, and runs it again on what survived.
    #[must_use]
    pub fn crash_recovery(mut self) -> Self {
        self.recovery_crashes();
        self
    }

    /// Crashes recovery as [`Self::crash_recovery`] does, but with each of
    /// these outcomes at each point, whatever the scenario's crash left:
    /// recovery from a crash that kept everything may itself lose what it
    /// didn't sync, and the other way round.
    #[must_use]
    pub fn recovery_outcomes(mut self, outcomes: Vec<CrashOutcome>) -> Self {
        self.recovery_crashes().outcomes = Some(outcomes);
        self
    }

    /// Of each case's recovery crashes, numbered by point then outcome,
    /// keeps only every `every`th, starting one further along in each case:
    /// over any `every` cases in a row, each number is kept once. Every case
    /// still runs recovery uncrashed too. For a test with too many recovery
    /// crashes to run them all.
    #[must_use]
    pub fn sample_recovery(mut self, every: usize) -> Self {
        self.recovery_crashes().every = every.max(1);
        self
    }

    /// For every `every`th recovery crash kept, also crashes the re-run,
    /// after each of its operations (and before the first), with each
    /// recovery outcome, and runs recovery a third time on what survived.
    /// With several outcomes they mix: a first run that lost what it didn't
    /// sync, then a re-run that kept it, and so on. With
    /// [`Self::sample_recovery`], the re-run's crashes are sampled the same
    /// way.
    #[must_use]
    pub fn crash_rerun(mut self, every: usize) -> Self {
        self.recovery_crashes().rerun_every = Some(every.max(1));
        self
    }

    fn recovery_crashes(&mut self) -> &mut RecoveryCrashes {
        self.recovery.get_or_insert(RecoveryCrashes {
            outcomes: None,
            every: 1,
            rerun_every: None,
        })
    }

    /// Runs every case.
    ///
    /// # Errors
    ///
    /// The first case whose check fails.
    pub fn run(&self) -> Result<CrashSummary, CrashFailure> {
        let clean = FakeFs::with_dirs(&self.dirs);
        (self.scenario)(&clean);
        let scenario_ops = clean.attempted();
        let clean_ops = clean.ops();

        let mut summary = CrashSummary {
            scenario_ops,
            cases: 0,
            recovery_crashed: 0,
            rerun_crashed: 0,
        };
        // Cases so far, and recovery crashes kept.
        let mut case_index = 0;
        let mut kept = 0;
        for after_ops in 0..=scenario_ops {
            // One run per crash point: what survives depends only on the
            // filesystem's state at the crash, and each outcome is taken
            // from that.
            let fs = FakeFs::with_dirs(&self.dirs);
            fs.crash_after(after_ops);
            let observed = (self.scenario)(&fs);
            let ops = fs.ops();
            if after_ops == scenario_ops && (fs.has_crashed() || ops != clean_ops) {
                // With room for every operation it must do what the
                // clean run did, or the crash points don't line up.
                return Err(CrashFailure {
                    case: format!("rerun with room for {scenario_ops} ops"),
                    message: "the scenario isn't deterministic".to_owned(),
                });
            }
            for &outcome in &self.outcomes {
                let survived = fs.crash(outcome);
                let case = |recovery_crashes, disk| CrashCase {
                    after_ops,
                    ops: ops.clone(),
                    outcome,
                    recovery_crashes,
                    survived: survived.copy_disk(),
                    fs: disk,
                };
                self.check_one(&case(Vec::new(), survived.copy_disk()), &observed)?;
                summary.cases += 1;

                let Some(plan) = &self.recovery else {
                    continue;
                };
                let own = [outcome];
                let recovery_outcomes = plan.outcomes.as_deref().unwrap_or(&own);
                let offset = case_index % plan.every;
                case_index += 1;
                let points = self.recovery_points(&survived, recovery_outcomes);
                for first in points.into_iter().skip(offset).step_by(plan.every) {
                    kept += 1;
                    let disk = self.crash_one(&survived, first);
                    self.check_one(&case(vec![first], disk.copy_disk()), &observed)?;
                    summary.cases += 1;
                    summary.recovery_crashed += 1;

                    if plan.rerun_every.is_none_or(|n| (kept - 1) % n != 0) {
                        continue;
                    }
                    let rerun = self.recovery_points(&disk, recovery_outcomes);
                    let offset = kept % plan.every;
                    for second in rerun.into_iter().skip(offset).step_by(plan.every) {
                        let last = self.crash_one(&disk, second);
                        self.check_one(&case(vec![first, second], last), &observed)?;
                        summary.cases += 1;
                        summary.rerun_crashed += 1;
                    }
                }
            }
        }
        Ok(summary)
    }

    /// Every point recovery of `disk` can crash at, with an outcome each:
    /// with one outcome, each point with it; with several, each point with
    /// each of them.
    fn recovery_points(&self, disk: &FakeFs, outcomes: &[CrashOutcome]) -> Vec<RecoveryCrash> {
        let probe = disk.copy_disk();
        (self.recover)(&probe);
        (0..=probe.attempted())
            .flat_map(|after_ops| {
                outcomes
                    .iter()
                    .map(move |&outcome| RecoveryCrash { after_ops, outcome })
            })
            .collect()
    }

    /// Runs recovery on a copy of `disk`, crashed as `crash` says, and
    /// returns what survived.
    fn crash_one(&self, disk: &FakeFs, crash: RecoveryCrash) -> FakeFs {
        let run = disk.copy_disk();
        run.crash_after(crash.after_ops);
        (self.recover)(&run);
        run.crash(crash.outcome)
    }

    /// Runs recovery to the end on `case.fs` and checks the result.
    fn check_one(&self, case: &CrashCase, observed: &O) -> Result<(), CrashFailure> {
        let recovered = (self.recover)(&case.fs);
        (self.check)(case, observed, &recovered).map_err(|message| CrashFailure {
            case: case.to_string(),
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::fs::{Fs, FsFile};

    fn publish(fs: &FakeFs, sync_dir: bool) -> io::Result<()> {
        let dir = Path::new("/s");
        let mut f = fs.create(&dir.join("seg.tmp"))?;
        f.write_all(b"flac")?;
        f.sync()?;
        fs.rename(&dir.join("seg.tmp"), &dir.join("seg"))?;
        if sync_dir {
            fs.sync_dir(dir)?;
        }
        Ok(())
    }

    fn published(fs: &FakeFs) -> Option<Vec<u8>> {
        fs.read(&PathBuf::from("/s/seg")).ok()
    }

    /// A check on a publish: the scenario says whether it finished, recovery
    /// what it found.
    type Check = fn(&CrashCase, &bool, &Option<Vec<u8>>) -> Result<(), String>;

    const PROMISED_IS_KEPT: Check = |_, &done, got| match (done, got) {
        (true, Some(bytes)) if bytes == b"flac" => Ok(()),
        (true, other) => Err(format!("reported published, found {other:?}")),
        (false, Some(bytes)) if bytes != b"flac" => Err(format!("half a file: {bytes:?}")),
        (false, _) => Ok(()),
    };

    #[test]
    fn correct_publish_passes_every_case() {
        let summary = CrashTest::new(|fs| publish(fs, true).is_ok(), published, PROMISED_IS_KEPT)
            .dirs(["/s"])
            .run()
            .unwrap();
        assert_eq!(summary.scenario_ops, 5);
        assert_eq!(summary.cases, 6 * CrashOutcome::standard().len());
    }

    #[test]
    fn missing_directory_sync_is_caught() {
        let failure = CrashTest::new(|fs| publish(fs, false).is_ok(), published, PROMISED_IS_KEPT)
            .dirs(["/s"])
            .run()
            .unwrap_err();
        assert!(failure.case.contains("after 4 ops"), "{failure}");
        assert!(failure.message.contains("reported published"), "{failure}");
        let shown = failure.to_string();
        assert!(shown.starts_with(&failure.case) && shown.ends_with(&failure.message));
    }

    #[test]
    fn unsynced_publish_is_caught() {
        // Renaming before the data's fsync can publish a short file.
        let failure = CrashTest::new(
            |fs: &FakeFs| {
                let run = || -> io::Result<()> {
                    let mut f = fs.create(Path::new("/s/seg.tmp"))?;
                    f.write_all(b"flac")?;
                    fs.rename(Path::new("/s/seg.tmp"), Path::new("/s/seg"))?;
                    fs.sync_dir(Path::new("/s"))?;
                    f.sync()?;
                    Ok(())
                };
                run().is_ok()
            },
            published,
            PROMISED_IS_KEPT,
        )
        .dirs(["/s"])
        .run()
        .unwrap_err();
        assert!(failure.message.contains("half a file"), "{failure}");
    }

    /// Recovery that finishes a publish: if the temp file survived, rename it.
    fn finish_publish(fs: &FakeFs, sync_dir: bool) -> Option<Vec<u8>> {
        let dir = Path::new("/s");
        if fs.read(&dir.join("seg.tmp")).is_ok() {
            fs.rename(&dir.join("seg.tmp"), &dir.join("seg")).ok()?;
            if sync_dir {
                fs.sync_dir(dir).ok()?;
            }
        }
        published(fs)
    }

    const ALWAYS_PUBLISHED: Check = |_, _, got| match got {
        Some(bytes) if bytes == b"flac" => Ok(()),
        // Before the temp file's data was synced there's nothing to save.
        None => Ok(()),
        Some(other) => Err(format!("bad file {other:?}")),
    };

    #[test]
    fn crash_recovery_runs_recovery_crashes() {
        let summary = CrashTest::new(
            |fs| publish(fs, true).is_ok(),
            |fs| finish_publish(fs, true),
            ALWAYS_PUBLISHED,
        )
        .dirs(["/s"])
        .outcomes(vec![CrashOutcome::LoseUnsynced])
        .crash_recovery();
        assert!(
            format!("{summary:?}").contains("outcomes: None, every: 1, rerun_every: None"),
            "{summary:?}"
        );
        let summary = summary.run().unwrap();
        // More cases than one per crash point: recovery crashed too.
        assert!(summary.cases > summary.scenario_ops + 1, "{summary:?}");
        assert_eq!(
            summary.cases,
            summary.scenario_ops + 1 + summary.recovery_crashed,
            "{summary:?}"
        );
        assert_eq!(summary.rerun_crashed, 0);
    }

    #[test]
    fn crash_recovery_catches_unrepeatable_recovery() {
        // Recovery that deletes the temp file before publishing it loses the
        // segment if it crashes in between.
        let lossy = |fs: &FakeFs| {
            let dir = Path::new("/s");
            if let Ok(bytes) = fs.read(&dir.join("seg.tmp")) {
                fs.remove(&dir.join("seg.tmp")).ok()?;
                fs.sync_dir(dir).ok()?;
                let mut f = fs.create(&dir.join("seg")).ok()?;
                f.write_all(&bytes).ok()?;
                f.sync().ok()?;
                fs.sync_dir(dir).ok()?;
            }
            published(fs)
        };
        let must_publish = |case: &CrashCase, _: &bool, got: &Option<Vec<u8>>| {
            // Once the temp file's data and name were durable (create, write,
            // sync, directory sync), the segment must survive.
            let synced = case.ops.iter().any(|op| matches!(op, Op::SyncDir(_)));
            match got {
                Some(bytes) if bytes == b"flac" => Ok(()),
                _ if !synced => Ok(()),
                other => Err(format!("lost the segment: {other:?}")),
            }
        };
        // Without recovery crashes the lossy recovery passes...
        CrashTest::new(
            |fs| {
                let dir = Path::new("/s");
                let run = || -> io::Result<()> {
                    let mut f = fs.create(&dir.join("seg.tmp"))?;
                    f.write_all(b"flac")?;
                    f.sync()?;
                    fs.sync_dir(dir)
                };
                run().is_ok()
            },
            lossy,
            must_publish,
        )
        .dirs(["/s"])
        .outcomes(vec![CrashOutcome::LoseUnsynced])
        .run()
        .unwrap();
        // ...and crashing it shows the loss.
        let failure = CrashTest::new(
            |fs| {
                let dir = Path::new("/s");
                let run = || -> io::Result<()> {
                    let mut f = fs.create(&dir.join("seg.tmp"))?;
                    f.write_all(b"flac")?;
                    f.sync()?;
                    fs.sync_dir(dir)
                };
                run().is_ok()
            },
            lossy,
            must_publish,
        )
        .dirs(["/s"])
        .outcomes(vec![CrashOutcome::LoseUnsynced])
        .crash_recovery()
        .run()
        .unwrap_err();
        assert!(failure.case.contains("recovery crashed"), "{failure}");
    }

    #[test]
    fn a_nondeterministic_scenario_is_refused() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runs = AtomicUsize::new(0);
        let failure = CrashTest::new(
            |fs: &FakeFs| {
                // One more read on every run after the first.
                if runs.fetch_add(1, Ordering::SeqCst) > 0 {
                    let _ = fs.read(Path::new("/s/x"));
                }
                publish(fs, true).is_ok()
            },
            published,
            PROMISED_IS_KEPT,
        )
        .dirs(["/s"])
        .outcomes(vec![CrashOutcome::KeepAll])
        .run()
        .unwrap_err();
        assert!(failure.message.contains("deterministic"), "{failure}");
    }

    #[test]
    fn a_scenario_doing_different_operations_is_refused() {
        // Same number of operations, different ones: no crash, but the crash
        // points wouldn't line up with the clean run's.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runs = AtomicUsize::new(0);
        let failure = CrashTest::new(
            |fs: &FakeFs| {
                let name = format!("/s/x{}", runs.fetch_add(1, Ordering::SeqCst).min(1));
                let _ = fs.create(Path::new(&name));
                true
            },
            published,
            |_, _, _| Ok(()),
        )
        .dirs(["/s"])
        .outcomes(vec![CrashOutcome::KeepAll])
        .run()
        .unwrap_err();
        assert!(failure.message.contains("deterministic"), "{failure}");
    }

    #[test]
    fn recovery_is_crashed_after_its_last_operation_too() {
        // Recovery whose last step destroys its input: crashing right after
        // it, before anything replaces the file, loses the segment.
        let destructive = |fs: &FakeFs| {
            let dir = Path::new("/s");
            let bytes = fs.read(&dir.join("seg.tmp")).ok();
            if bytes.is_some() {
                fs.remove(&dir.join("seg.tmp")).ok()?;
            }
            bytes.or_else(|| published(fs))
        };
        let kept = |_: &CrashCase, done: &bool, got: &Option<Vec<u8>>| match (done, got) {
            (true, None) => Err("lost the segment".to_owned()),
            _ => Ok(()),
        };
        let failure = CrashTest::new(
            |fs: &FakeFs| {
                let dir = Path::new("/s");
                let run = || -> io::Result<()> {
                    let mut f = fs.create(&dir.join("seg.tmp"))?;
                    f.write_all(b"flac")?;
                    f.sync()?;
                    fs.sync_dir(dir)
                };
                run().is_ok()
            },
            destructive,
            kept,
        )
        .dirs(["/s"])
        .outcomes(vec![CrashOutcome::KeepAll])
        .crash_recovery()
        .run()
        .unwrap_err();
        assert!(
            failure.case.contains("recovery crashed after 2 ops"),
            "{failure}"
        );
    }

    /// The scenario of the recovery tests below: a temp segment made
    /// durable, for recovery to publish.
    fn durable_temp(fs: &FakeFs) -> bool {
        let dir = Path::new("/s");
        let run = || -> io::Result<()> {
            let mut f = fs.create(&dir.join("seg.tmp"))?;
            f.write_all(b"flac")?;
            f.sync()?;
            fs.sync_dir(dir)
        };
        run().is_ok()
    }

    /// Once the temp file's data and name were durable, the segment must
    /// survive.
    const MUST_PUBLISH: Check = |case, _, got| {
        let synced = case.ops.iter().any(|op| matches!(op, Op::SyncDir(_)));
        match got {
            Some(bytes) if bytes == b"flac" => Ok(()),
            _ if !synced => Ok(()),
            other => Err(format!("lost the segment: {other:?}")),
        }
    };

    #[test]
    fn mixed_recovery_outcomes_catch_a_recovery_that_trusts_unsynced_data() {
        // Recovery that copies the segment without an fsync, then removes
        // the temp file: fine if the crash keeps everything, a loss if it
        // keeps only what was synced.
        let unsynced_copy = |fs: &FakeFs| {
            let dir = Path::new("/s");
            if let Ok(bytes) = fs.read(&dir.join("seg.tmp")) {
                // A crashed run may have left a copy.
                let _ = fs.remove(&dir.join("seg"));
                let mut f = fs.create(&dir.join("seg")).ok()?;
                f.write_all(&bytes).ok()?;
                fs.remove(&dir.join("seg.tmp")).ok()?;
                fs.sync_dir(dir).ok()?;
            }
            published(fs)
        };
        let test = || {
            CrashTest::new(durable_temp, unsynced_copy, MUST_PUBLISH)
                .dirs(["/s"])
                .outcomes(vec![CrashOutcome::KeepAll])
        };
        // Recovery crashed with the scenario's own outcome passes...
        test().crash_recovery().run().unwrap();
        // ...and with another, it doesn't.
        let failure = test()
            .recovery_outcomes(vec![CrashOutcome::KeepAll, CrashOutcome::LoseUnsynced])
            .run()
            .unwrap_err();
        assert!(
            failure
                .case
                .contains("KeepAll, recovery crashed after 6 ops, LoseUnsynced"),
            "{failure}"
        );
        assert!(failure.message.contains("lost the segment"), "{failure}");
    }

    #[test]
    fn crash_rerun_catches_a_recovery_unsafe_to_crash_twice() {
        // Recovery that moves the temp file aside, then into place: safe to
        // crash. But when it finds the file aside, from a crashed run, it
        // copies it the unsafe way: the source is gone before the copy is
        // durable.
        let twice_unsafe = |fs: &FakeFs| {
            let dir = Path::new("/s");
            let (tmp, aside, seg) = (dir.join("seg.tmp"), dir.join("seg.bak"), dir.join("seg"));
            if let Ok(bytes) = fs.read(&aside) {
                fs.remove(&aside).ok()?;
                fs.sync_dir(dir).ok()?;
                let mut f = fs.create(&seg).ok()?;
                f.write_all(&bytes).ok()?;
                f.sync().ok()?;
                fs.sync_dir(dir).ok()?;
            } else if fs.read(&tmp).is_ok() {
                fs.rename(&tmp, &aside).ok()?;
                fs.sync_dir(dir).ok()?;
                fs.rename(&aside, &seg).ok()?;
                fs.sync_dir(dir).ok()?;
            }
            published(fs)
        };
        let test = || {
            CrashTest::new(durable_temp, twice_unsafe, MUST_PUBLISH)
                .dirs(["/s"])
                .outcomes(vec![CrashOutcome::KeepAll])
                .crash_recovery()
        };
        let once = test().run().unwrap();
        assert!(once.recovery_crashed > 0, "{once:?}");
        let failure = test().crash_rerun(1).run().unwrap_err();
        assert!(
            failure.case.contains("its re-run crashed after"),
            "{failure}"
        );
        assert!(failure.message.contains("lost the segment"), "{failure}");
    }

    #[test]
    fn crash_rerun_crashes_the_rerun_of_every_nth_recovery_crash() {
        let test = || {
            CrashTest::new(
                |fs| publish(fs, true).is_ok(),
                |fs| finish_publish(fs, true),
                ALWAYS_PUBLISHED,
            )
            .dirs(["/s"])
            .outcomes(vec![CrashOutcome::LoseUnsynced])
            .recovery_outcomes(vec![CrashOutcome::KeepAll, CrashOutcome::LoseUnsynced])
        };
        let all = test().crash_rerun(1).run().unwrap();
        let half = test().crash_rerun(2).run().unwrap();
        let none = test().run().unwrap();
        assert_eq!(none.rerun_crashed, 0);
        assert_eq!(all.recovery_crashed, none.recovery_crashed);
        assert_eq!(half.recovery_crashed, none.recovery_crashed);
        // Each re-run crashed at every point with both outcomes: more than
        // one re-run crash per recovery crash.
        assert!(all.rerun_crashed > all.recovery_crashed, "{all:?}");
        assert!(
            0 < half.rerun_crashed && half.rerun_crashed < all.rerun_crashed,
            "{half:?} {all:?}"
        );
        assert_eq!(
            all.cases,
            all.scenario_ops + 1 + all.recovery_crashed + all.rerun_crashed
        );
    }

    #[test]
    fn sample_recovery_keeps_every_nth_and_still_reaches_every_point() {
        use std::cell::RefCell;
        use std::collections::BTreeSet;
        let run = |every: usize| {
            let seen = RefCell::new(BTreeSet::new());
            let summary = CrashTest::new(
                |fs| publish(fs, true).is_ok(),
                |fs| finish_publish(fs, true),
                |case: &CrashCase, done: &bool, got: &Option<Vec<u8>>| {
                    if let Some(first) = case.recovery_crashes.first() {
                        seen.borrow_mut()
                            .insert((first.after_ops, format!("{:?}", first.outcome)));
                    }
                    ALWAYS_PUBLISHED(case, done, got)
                },
            )
            .dirs(["/s"])
            .outcomes(vec![CrashOutcome::LoseUnsynced])
            .recovery_outcomes(vec![CrashOutcome::KeepAll, CrashOutcome::LoseUnsynced])
            .sample_recovery(every)
            .run()
            .unwrap();
            (summary, seen.into_inner())
        };
        let (all, every_crash) = run(1);
        let (sampled, sampled_crashes) = run(3);
        assert_eq!(sampled.scenario_ops, all.scenario_ops);
        // About a third of the recovery crashes...
        assert!(
            3 * sampled.recovery_crashed <= all.recovery_crashed + 3 * (all.scenario_ops + 1),
            "{sampled:?} {all:?}"
        );
        assert!(
            sampled.recovery_crashed < all.recovery_crashed,
            "{sampled:?}"
        );
        // ...yet every point and outcome is crashed in some case.
        assert!(every_crash.len() > 4, "{every_crash:?}");
        assert_eq!(sampled_crashes, every_crash);
        // A stride of zero is taken as one.
        assert_eq!(run(0).0, all);
    }
}
