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
//! be safe to repeat.
//!
//! The tests below show the shape: a segment publish (temp file, fsync,
//! rename, directory sync) and a recovery that finishes it.

use std::fmt;
use std::path::{Path, PathBuf};

use super::fake::{CrashOutcome, FakeFs, Op};

/// Where one crash test case crashed, handed to the check.
#[derive(Debug)]
pub struct CrashCase {
    /// How many operations the scenario attempted before the crash.
    pub after_ops: usize,
    /// The scenario's operations that succeeded, in order.
    pub ops: Vec<Op>,
    /// What survived the crash.
    pub outcome: CrashOutcome,
    /// If recovery was crashed too, after how many of its operations.
    pub recovery_crashed_after: Option<usize>,
    /// What survived the scenario's crash, before any recovery ran: for a
    /// check that compares with an uninterrupted recovery. Use a
    /// [`FakeFs::copy_disk`] of it.
    pub survived: FakeFs,
    /// The filesystem after recovery finished.
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
        if let Some(m) = self.recovery_crashed_after {
            write!(f, ", recovery crashed after {m} ops")?;
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
}

/// A crash test: a scenario, a recovery, and the invariants to check.
///
/// - The **scenario** runs the write path on the filesystem it's given and
///   returns what it observed or promised before it stopped (say, the last
///   durable position it reported). It must be deterministic, and should stop
///   at the first error, as the real code would.
/// - **Recovery** runs on what survived the crash and returns what it
///   recovered.
/// - The **check** compares the two and says what's wrong, if anything.
pub struct CrashTest<S, R, C> {
    scenario: S,
    recover: R,
    check: C,
    outcomes: Vec<CrashOutcome>,
    crash_recovery: bool,
    dirs: Vec<PathBuf>,
}

impl<S, R, C> fmt::Debug for CrashTest<S, R, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrashTest")
            .field("outcomes", &self.outcomes)
            .field("crash_recovery", &self.crash_recovery)
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
            crash_recovery: false,
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
        self.crash_recovery = true;
        self
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

        let mut cases = 0;
        for after_ops in 0..=scenario_ops {
            for &outcome in &self.outcomes {
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
                let survived = fs.crash(outcome);

                let mut recovery_points = vec![None];
                if self.crash_recovery {
                    let probe = survived.copy_disk();
                    (self.recover)(&probe);
                    recovery_points.extend((0..=probe.attempted()).map(Some));
                }
                for recovery_crashed_after in recovery_points {
                    let mut disk = survived.copy_disk();
                    if let Some(m) = recovery_crashed_after {
                        disk.crash_after(m);
                        (self.recover)(&disk);
                        disk = disk.crash(outcome);
                    }
                    self.check_one(
                        &CrashCase {
                            after_ops,
                            ops: ops.clone(),
                            outcome,
                            recovery_crashed_after,
                            survived: survived.copy_disk(),
                            fs: disk,
                        },
                        &observed,
                    )?;
                    cases += 1;
                }
            }
        }
        Ok(CrashSummary {
            scenario_ops,
            cases,
        })
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
        assert!(format!("{summary:?}").contains("crash_recovery: true"));
        let summary = summary.run().unwrap();
        // More cases than one per crash point: recovery crashed too.
        assert!(summary.cases > summary.scenario_ops + 1, "{summary:?}");
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
}
