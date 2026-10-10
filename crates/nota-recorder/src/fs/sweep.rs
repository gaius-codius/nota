//! Proof that a crash or seed sweep wasn't vacuous.
//!
//! A sweep runs one operation many times, crashed or failed after each of
//! its operations in turn, or crashed with each of many seeds, and checks
//! what survived each time. Its checks hold at every point it reaches, so a
//! sweep that never interrupts the operation (it finished before the first
//! crash point) or whose seeds all leave the same thing passes while
//! proving nothing.
//!
//! Each sweep reports into a [`Sweep`]: every point it ran, whether the
//! crash or failure interrupted the operation there, and what the point led
//! to. It then states its floor: at least so many interrupted points, or
//! every outcome it must reach. The line stating a floor ends with the
//! check-bound tag, so `scripts/check-weakened.sh` reports any change to
//! it. A sweep dropped without a floor fails the test, and
//! `scripts/check-weakened.test.sh` fails on a sweep without a floor in any
//! file with tests that uses [`FakeFs`], but for the fake's, the crash
//! test's and this module's own.
//!
//! A [`CrashTest`](super::crash::CrashTest)'s sweeps don't report point by
//! point: [`CrashSummary::scenario`], [`CrashSummary::recovery`] and
//! [`CrashSummary::reruns`] build them from its counts.

use std::fmt;
use std::thread;

use super::crash::CrashSummary;
use super::fake::FakeFs;

/// What a sweep reached, checked against its floor before it's dropped.
///
/// `K` is what a point can lead to, for a sweep that must reach several
/// outcomes: the frames a partial crash kept, where a full disk hit.
#[must_use = "a sweep proves nothing until its floor is checked"]
pub struct Sweep<K = ()> {
    /// Points run.
    points: usize,
    /// Of those, the points where the crash or failure interrupted the
    /// operation.
    interrupted: usize,
    /// Each outcome seen, once, in the order first seen.
    seen: Vec<K>,
    /// Whether a floor was checked.
    floored: bool,
}

impl Sweep {
    /// A sweep that has run no points, with a floor on its interruptions.
    pub fn new() -> Self {
        Self::counted(0, 0)
    }
}

impl<K> Sweep<K> {
    /// A sweep that has run no points, with a floor on the outcomes it
    /// sees.
    pub fn with_outcomes() -> Self {
        Self::counted(0, 0)
    }

    /// A sweep of `points` points, `interrupted` of them interrupted.
    fn counted(points: usize, interrupted: usize) -> Self {
        Self {
            points,
            interrupted,
            seen: Vec::new(),
            floored: false,
        }
    }

    /// Records a point where the crash or failure interrupted the
    /// operation.
    pub fn interrupted(&mut self) {
        self.points += 1;
        self.interrupted += 1;
    }

    /// Records a point where the operation ran to its end: the crash or
    /// failure came too late, or not at all.
    pub fn finished(&mut self) {
        self.points += 1;
    }

    /// Records a point of a crash sweep, once the operation has run on
    /// `fs`: interrupted if `fs` crashed before it finished. Call it before
    /// crashing `fs` yourself ([`FakeFs::crash`]), which counts as a crash
    /// too.
    pub fn crash_point(&mut self, fs: &FakeFs) {
        if fs.has_crashed() {
            self.interrupted();
        } else {
            self.finished();
        }
    }

    /// Records a point of a failure sweep, once the operation has run on
    /// `fs`: interrupted if the failure [`FakeFs::fail_after`] set fired.
    pub fn failure_point(&mut self, fs: &FakeFs) {
        if fs.has_failed() {
            self.interrupted();
        } else {
            self.finished();
        }
    }

    /// Records whether the point interrupted the operation, from its
    /// result: an error is an interruption.
    pub fn result<T, E>(&mut self, result: &Result<T, E>) {
        if result.is_ok() {
            self.finished();
        } else {
            self.interrupted();
        }
    }
}

impl Default for Sweep {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: PartialEq + fmt::Debug> Sweep<K> {
    /// Records that a point led to `outcome`. It doesn't count the point:
    /// a sweep with a floor on its interruptions also calls
    /// [`Self::interrupted`] or [`Self::finished`].
    pub fn saw(&mut self, outcome: K) {
        if !self.seen.contains(&outcome) {
            self.seen.push(outcome);
        }
    }

    /// Checks that more than `floor` points interrupted the operation.
    ///
    /// # Panics
    ///
    /// If `floor` or fewer did.
    #[track_caller]
    pub fn interrupted_more_than(&mut self, floor: usize) {
        self.floored = true;
        assert!(
            self.interrupted > floor,
            "want more than {floor} interrupted: {self:?}"
        );
    }

    /// Checks that at least `floor` points interrupted the operation.
    ///
    /// # Panics
    ///
    /// If fewer did.
    #[track_caller]
    pub fn interrupted_at_least(&mut self, floor: usize) {
        self.floored = true;
        assert!(
            self.interrupted >= floor,
            "want at least {floor} interrupted: {self:?}"
        );
    }

    /// Checks that the points led to more than `floor` different outcomes.
    ///
    /// # Panics
    ///
    /// If they led to `floor` or fewer.
    #[track_caller]
    pub fn saw_more_than(&mut self, floor: usize) {
        self.floored = true;
        assert!(
            self.seen.len() > floor,
            "want more than {floor} outcomes: {self:?}"
        );
    }

    /// Checks that every one of `outcomes` was seen.
    ///
    /// # Panics
    ///
    /// If one wasn't.
    #[track_caller]
    pub fn saw_each(&mut self, outcomes: impl IntoIterator<Item = K>) {
        self.floored = true;
        for outcome in outcomes {
            assert!(
                self.seen.contains(&outcome),
                "never saw {outcome:?}: {self:?}"
            );
        }
    }
}

impl<K: fmt::Debug> fmt::Debug for Sweep<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sweep")
            .field("points", &self.points)
            .field("interrupted", &self.interrupted)
            .field("seen", &self.seen)
            .finish_non_exhaustive()
    }
}

impl<K> Drop for Sweep<K> {
    fn drop(&mut self) {
        // A test already failing has said why; a second panic would abort.
        assert!(
            self.floored || thread::panicking(),
            "a sweep of {} points was dropped without a floor",
            self.points
        );
    }
}

impl CrashSummary {
    /// The sweep of the scenario's crash points: one after each number of
    /// operations from none to all of them, every one but the last
    /// interrupting it.
    pub fn scenario(&self) -> Sweep {
        Sweep::counted(self.scenario_ops + 1, self.scenario_ops)
    }

    /// The sweep of recovery's crashes: each counted as interrupting it,
    /// though one after its last operation leaves it whole.
    pub fn recovery(&self) -> Sweep {
        Sweep::counted(self.recovery_crashed, self.recovery_crashed)
    }

    /// The sweep of the re-runs' crashes, counted as
    /// [`recovery`](Self::recovery)'s are.
    pub fn reruns(&self) -> Sweep {
        Sweep::counted(self.rerun_crashed, self.rerun_crashed)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::Path;

    use super::*;
    use crate::fs::Fs;

    /// A sweep of `interrupted` interrupted points and `finished` others.
    fn swept(interrupted: usize, finished: usize) -> Sweep {
        let mut sweep = Sweep::new();
        for _ in 0..interrupted {
            sweep.interrupted();
        }
        for _ in 0..finished {
            sweep.finished();
        }
        sweep
    }

    #[test]
    fn a_sweep_counts_its_points_and_interruptions() {
        let mut sweep = swept(3, 2);
        assert_eq!((sweep.points, sweep.interrupted), (5, 3));
        sweep.result(&Err::<(), ()>(()));
        assert_eq!((sweep.points, sweep.interrupted), (6, 4));
        sweep.result(&Ok::<(), ()>(()));
        assert_eq!((sweep.points, sweep.interrupted), (7, 4));
        sweep.interrupted_at_least(4);
    }

    #[test]
    fn a_crash_point_is_interrupted_only_if_the_crash_came() {
        let mut sweep = Sweep::new();
        for budget in [0, 1, 2] {
            let fs = FakeFs::new();
            fs.crash_after(budget);
            let _ = fs.create_dir(Path::new("/a"));
            sweep.crash_point(&fs);
        }
        // One operation: only a budget of none stops it.
        assert_eq!((sweep.points, sweep.interrupted), (3, 1));
        sweep.interrupted_at_least(1);
    }

    #[test]
    fn a_failure_point_is_interrupted_only_if_the_failure_fired() {
        let mut sweep = Sweep::new();
        for at in [0, 1, 2] {
            let fs = FakeFs::new();
            // An operation before the failure is set doesn't count towards
            // it.
            fs.create_dir(Path::new("/a")).unwrap();
            fs.fail_after(at, io::ErrorKind::Other);
            let _ = fs.create_dir(Path::new("/b"));
            sweep.failure_point(&fs);
        }
        // One operation after it: only failing the first one hits it.
        assert_eq!((sweep.points, sweep.interrupted), (3, 1));
        sweep.interrupted_at_least(1);
    }

    #[test]
    fn interrupted_floors_pass_at_their_bound() {
        swept(3, 9).interrupted_at_least(3);
        swept(3, 9).interrupted_more_than(2);
    }

    #[test]
    #[should_panic(expected = "want at least 4 interrupted")]
    fn too_few_interruptions_fail_an_at_least_floor() {
        swept(3, 9).interrupted_at_least(4);
    }

    #[test]
    #[should_panic(
        expected = "want more than 3 interrupted: Sweep { points: 12, interrupted: 3, seen: [], .. }"
    )]
    fn too_few_interruptions_fail_a_more_than_floor() {
        // The finished points don't count towards it.
        swept(3, 9).interrupted_more_than(3);
    }

    #[test]
    fn outcomes_are_counted_once_each() {
        let mut sweep = Sweep::with_outcomes();
        for n in [3, 4, 3, 3, 4] {
            sweep.saw(n);
        }
        assert_eq!(sweep.seen, [3, 4]);
        sweep.saw_more_than(1);
        sweep.saw_each([4, 3]);
    }

    #[test]
    #[should_panic(expected = "want more than 2 outcomes")]
    fn too_few_outcomes_fail_their_floor() {
        let mut sweep = Sweep::with_outcomes();
        for n in [3, 4, 3] {
            sweep.saw(n);
        }
        sweep.saw_more_than(2);
    }

    #[test]
    #[should_panic(expected = "never saw 4")]
    fn an_outcome_never_seen_fails_its_floor() {
        let mut sweep = Sweep::with_outcomes();
        sweep.saw(3);
        sweep.saw_each([3, 4]);
    }

    #[test]
    #[should_panic(expected = "a sweep of 2 points was dropped without a floor")]
    fn a_sweep_without_a_floor_fails() {
        drop(swept(1, 1));
    }

    #[test]
    fn a_crash_summary_s_sweeps_count_its_crash_points() {
        let summary = CrashSummary {
            scenario_ops: 5,
            cases: 60,
            recovery_crashed: 40,
            rerun_crashed: 12,
        };
        let (scenario, recovery, reruns) =
            (summary.scenario(), summary.recovery(), summary.reruns());
        let counts = |s: &Sweep| (s.points, s.interrupted);
        assert_eq!(counts(&scenario), (6, 5));
        assert_eq!(counts(&recovery), (40, 40));
        assert_eq!(counts(&reruns), (12, 12));
        for mut sweep in [scenario, recovery, reruns] {
            sweep.interrupted_at_least(0);
        }
    }
}
