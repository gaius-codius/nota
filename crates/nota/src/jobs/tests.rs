use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;

use nota_core::{Clock, SessionId, SystemClock};
use nota_store::NewSession;

use super::*;

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(
    /// The directory removed when the test ends.
    PathBuf,
);

impl TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-jobs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Waits on a channel nothing sends on: a pause that isn't a sleep.
fn pause(d: Duration) {
    let (_keep, never) = mpsc::channel::<()>();
    let _ = never.recv_timeout(d);
}

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);
const S3: SessionId = SessionId::new(3);

/// A library with `sessions`, each stopped with its jobs queued, in order,
/// waiting for what each says.
fn library(name: &str, sessions: &[(SessionId, Option<Wait>)]) -> (TestDir, Writer) {
    let dir = TestDir::new(name);
    let db = Writer::new(&dir.0.join("library.db"));
    for &(id, waits) in sessions {
        queue(&db, id, waits);
    }
    (dir, db)
}

/// What the fake worker does with a session's job, run after run.
#[derive(Debug, Clone)]
enum Then {
    Done,
    Fail,
    /// Runs until told to stop, then waits to run again.
    UntilStopped,
    /// Runs out of space.
    NoSpace,
    /// Finds it has no engine.
    NoEngine,
    /// Its engine couldn't start or stopped working, for this reason.
    EngineDown(&'static str),
    /// Its engine goes down while it runs, for this reason, and the run
    /// ends with it down.
    DownNow(&'static str),
    /// Its engine goes down while it runs, for this reason, comes back,
    /// and the run finishes.
    DownAndBack(&'static str),
    /// Its engine confirms audio, and the run finishes.
    Answered,
    /// Its engine confirms audio, then stops working, for this reason.
    AnsweredThenDown(&'static str),
}

/// A worker that does as told per session, and notes each run.
#[derive(Clone, Default)]
struct Fake {
    /// What each session does on successive runs.
    script: Arc<Mutex<BTreeMap<SessionId, Vec<Then>>>>,
    /// The sessions run, in order.
    ran: Arc<Mutex<Vec<SessionId>>>,
    /// Whether jobs must wait for an engine.
    no_engine: bool,
    /// The library, to note each job's state as it runs.
    db: Option<Writer>,
    /// The states noted while running, in order.
    seen: Arc<Mutex<Vec<JobState>>>,
    /// Moved on by `takes` in every run, so runs take time.
    clock: Option<Arc<nota_core::FakeClock>>,
    /// How long each run takes on `clock`.
    takes: Duration,
}

impl Fake {
    fn script(&self, session: SessionId, then: &[Then]) {
        self.script.lock().unwrap().insert(session, then.to_vec());
    }

    fn ran(&self) -> Vec<SessionId> {
        self.ran.lock().unwrap().clone()
    }

    fn seen(&self) -> Vec<JobState> {
        self.seen.lock().unwrap().clone()
    }

    /// Notes the job's state as the store has it now, mid-run.
    fn note(&self, job: &Job) {
        let db = self.db.as_ref().unwrap();
        let state = db.with(|db| db.jobs()).unwrap();
        let state = state.into_iter().find(|j| j.id == job.id).unwrap().state;
        self.seen.lock().unwrap().push(state);
    }
}

impl Worker for Fake {
    fn run(&mut self, job: &Job, running: &Running<'_>) -> JobEnd {
        self.ran.lock().unwrap().push(job.session);
        if let Some(clock) = &self.clock {
            clock.advance(self.takes);
        }
        let then = {
            let mut script = self.script.lock().unwrap();
            let steps = script.entry(job.session).or_default();
            if steps.is_empty() {
                Then::Done
            } else {
                steps.remove(0)
            }
        };
        (running.progress)(Progress { done: 1, total: 2 });
        match then {
            Then::Done => JobEnd::Done,
            Then::Fail => JobEnd::Failed("it broke".into()),
            Then::NoEngine => JobEnd::Waiting(Some(Wait::Engine)),
            Then::EngineDown(why) => JobEnd::EngineDown(why.into()),
            Then::DownNow(why) => {
                (running.engine_down)(Some(why));
                self.note(job);
                JobEnd::EngineDown(why.into())
            }
            Then::DownAndBack(why) => {
                (running.engine_down)(Some(why));
                self.note(job);
                (running.engine_down)(None);
                self.note(job);
                JobEnd::Done
            }
            Then::Answered => {
                (running.answered)();
                JobEnd::Done
            }
            Then::AnsweredThenDown(why) => {
                (running.answered)();
                JobEnd::EngineDown(why.into())
            }
            Then::NoSpace => JobEnd::Waiting(Some(Wait::Space)),
            Then::UntilStopped => {
                while !(running.stop)() {
                    pause(Duration::from_millis(10));
                }
                JobEnd::Waiting(None)
            }
        }
    }

    fn lacks(&self, _job: &Job) -> Option<Wait> {
        self.no_engine.then_some(Wait::Engine)
    }
}

/// A switch the test flips: a recording going, or room on the disk.
#[derive(Clone, Default)]
struct Switch(
    /// Whether recording or free space is present.
    Arc<AtomicBool>,
);

impl Switch {
    fn on() -> Self {
        let switch = Self::default();
        switch.set(true);
        switch
    }

    fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }
}

impl Capture for Switch {
    fn recording(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Room for Switch {
    fn room(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn states(db: &Writer) -> Vec<(SessionId, JobState)> {
    db.with(|db| db.jobs())
        .unwrap()
        .into_iter()
        .map(|job| (job.session, job.state))
        .collect()
}

/// Waits up to 10 s for the jobs' states to be `want`.
fn until_states(db: &Writer, want: &[(SessionId, JobState)]) {
    let clock = SystemClock::start().unwrap();
    while states(db) != want {
        assert!(
            clock.now().elapsed() < Duration::from_secs(10),
            "{:?} never became {want:?}",
            states(db)
        );
        pause(Duration::from_millis(20));
    }
}

fn spawn(dir: &TestDir, db: &Writer, worker: &Fake, capture: &Switch, room: &Switch) -> Runner {
    Runner::spawn(
        &dir.0,
        db.clone(),
        worker.clone(),
        capture.clone(),
        room.clone(),
        Arc::new(SystemClock::start().unwrap()),
    )
    .unwrap()
}

/// Jobs run in the order their sessions were stopped.
#[test]
fn three_sessions_run_in_order() {
    // Queue out of id order so the test checks queue order.
    let (dir, db) = library("order", &[(S2, None), (S1, None), (S3, None)]);
    let worker = Fake::default();
    let _runner = spawn(&dir, &db, &worker, &Switch::default(), &Switch::on());
    // Wait for every result before checking the runs.
    until_states(
        &db,
        &[
            (S2, JobState::Done),
            (S1, JobState::Done),
            (S3, JobState::Done),
        ],
    );
    assert_eq!(worker.ran(), [S2, S1, S3]);
}

/// A failed job leaves the queue free for the next session.
#[test]
fn a_failed_job_does_not_hold_up_the_next_session() {
    // Put the failing session first so it could hold the queue up.
    let (dir, db) = library("failure", &[(S1, None), (S2, None)]);
    let worker = Fake::default();
    worker.script(S1, &[Then::Fail]);
    let _runner = spawn(&dir, &db, &worker, &Switch::default(), &Switch::on());
    // Both results must be noted, including the reason for the failure.
    until_states(
        &db,
        &[
            (S1, JobState::Failed("it broke".into())),
            (S2, JobState::Done),
        ],
    );
    assert_eq!(worker.ran(), [S1, S2]);
}

/// The worker's progress is kept when its job finishes.
#[test]
fn a_finished_job_keeps_its_progress() {
    // The fake worker reports progress before returning its result.
    let (dir, db) = library("progress", &[(S1, None)]);
    let worker = Fake::default();
    let _runner = spawn(&dir, &db, &worker, &Switch::default(), &Switch::on());
    // Read it after the job ends so the callback has run.
    until_states(&db, &[(S1, JobState::Done)]);
    let job = db.with(|db| db.jobs()).unwrap().remove(0);
    assert_eq!(job.progress, Progress { done: 1, total: 2 });
}

/// Acceptance (GAI-316): no job starts while a recording runs; once it
/// stops, they do.
#[test]
fn no_job_starts_while_a_recording_runs() {
    // Start while a recording is already going, so no job can run.
    let (dir, db) = library("capture-first", &[(S1, None)]);
    let worker = Fake::default();
    let recording = Switch::on();
    let runner = spawn(&dir, &db, &worker, &recording, &Switch::on());
    pause(Duration::from_millis(1_500));
    assert!(worker.ran().is_empty());
    assert_eq!(states(&db), [(S1, JobState::Waiting(None))]);
    // Stopping the recording frees the queue to run again.
    recording.set(false);
    runner.wake();
    until_states(&db, &[(S1, JobState::Done)]);
}

/// A job running when a recording starts stops at once and waits, keeping
/// its progress, then runs again once the recording has stopped.
#[test]
fn a_recording_that_starts_pauses_the_running_job() {
    // Keep the worker running until the recording asks it to stop.
    let (dir, db) = library("pause", &[(S1, None)]);
    let worker = Fake::default();
    worker.script(S1, &[Then::UntilStopped]);
    let recording = Switch::default();
    let runner = spawn(&dir, &db, &worker, &recording, &Switch::on());
    until_states(&db, &[(S1, JobState::Running)]);
    // A new recording pauses it without counting a failed attempt.
    recording.set(true);
    until_states(&db, &[(S1, JobState::Waiting(None))]);
    let paused = db.with(|db| db.jobs()).unwrap().remove(0);
    assert_eq!(paused.progress, Progress { done: 1, total: 2 });
    // A pause isn't a death: nothing is counted against it.
    assert_eq!(paused.attempts, 0);
    pause(Duration::from_millis(300));
    assert_eq!(worker.ran(), [S1]);
    // Stopping the recording frees the queue to run again.
    recording.set(false);
    runner.wake();
    until_states(&db, &[(S1, JobState::Done)]);
    assert_eq!(worker.ran(), [S1, S1]);
}

/// A session waiting for space does not hold up another session.
#[test]
fn a_wait_for_space_does_not_hold_up_other_sessions() {
    // Queue the waiting session first, with no room for it to run.
    let (dir, db) = library("space-next", &[(S1, Some(Wait::Space)), (S2, None)]);
    let worker = Fake::default();
    let _runner = spawn(&dir, &db, &worker, &Switch::default(), &Switch::default());
    // The next session finishes while the first keeps waiting.
    until_states(
        &db,
        &[
            (S1, JobState::Waiting(Some(Wait::Space))),
            (S2, JobState::Done),
        ],
    );
    assert_eq!(worker.ran(), [S2]);
}

/// A session waiting for space runs once there is room again.
#[test]
fn a_wait_for_space_ends_when_room_returns() {
    // Start with a session stopped for lack of space.
    let (dir, db) = library("space-return", &[(S1, Some(Wait::Space))]);
    let worker = Fake::default();
    let room = Switch::default();
    let runner = spawn(&dir, &db, &worker, &Switch::default(), &room);
    pause(Duration::from_millis(300));
    assert!(worker.ran().is_empty());
    // Space returning is enough for the queued job to start.
    room.set(true);
    runner.wake();
    until_states(&db, &[(S1, JobState::Done)]);
    assert_eq!(worker.ran(), [S1]);
}

/// A job that runs out of space waits, then runs again when room returns.
#[test]
fn a_job_that_runs_out_of_space_waits_for_room() {
    // This session starts normally; its worker finds the disk full.
    let (dir, db) = library("space-run", &[(S1, None)]);
    let worker = Fake::default();
    worker.script(S1, &[Then::NoSpace, Then::Done]);
    let room = Switch::default();
    let runner = spawn(&dir, &db, &worker, &Switch::default(), &room);
    until_states(&db, &[(S1, JobState::Waiting(Some(Wait::Space)))]);
    pause(Duration::from_millis(300));
    assert_eq!(worker.ran(), [S1]);
    // The worker's wait follows the same space check as a stopped session.
    room.set(true);
    runner.wake();
    until_states(&db, &[(S1, JobState::Done)]);
    assert_eq!(worker.ran(), [S1, S1]);
}

/// Acceptance (GAI-316): a job a runner died while running runs again,
/// with the death counted, from where it got to.
#[test]
fn a_job_left_running_by_a_dead_runner_runs_again() {
    // Leave the job running in the queue, as a dead runner would.
    let (dir, db) = library("recover", &[(S1, None)]);
    let id = db.with(|db| db.jobs()).unwrap()[0].id;
    db.with(|db| db.start_job(id)).unwrap();
    // The new runner takes it back and counts the earlier death.
    let worker = Fake::default();
    let _runner = spawn(&dir, &db, &worker, &Switch::default(), &Switch::on());
    until_states(&db, &[(S1, JobState::Done)]);
    assert_eq!(worker.ran(), [S1]);
    assert_eq!(db.with(|db| db.jobs()).unwrap()[0].attempts, 1);
}

/// Only one runner runs a library's jobs; a second waits, and takes the
/// queue over once the first stops.
#[test]
fn a_second_runner_takes_over_when_the_first_stops() {
    // The first runner holds the lock while its job waits for an engine.
    let (dir, db) = library("take-over", &[(S1, None)]);
    let idle = Fake {
        no_engine: true,
        ..Fake::default()
    };
    let first = spawn(&dir, &db, &idle, &Switch::default(), &Switch::on());
    until_states(&db, &[(S1, JobState::Waiting(Some(Wait::Engine)))]);
    let worker = Fake::default();
    let _second = spawn(&dir, &db, &worker, &Switch::default(), &Switch::on());
    pause(Duration::from_millis(1_500));
    assert!(worker.ran().is_empty());
    // Closing it releases the lock for the second runner.
    drop(first);
    until_states(&db, &[(S1, JobState::Done)]);
    assert_eq!(worker.ran(), [S1]);
}

/// Without a speech engine, a job that needs one waits for it, saying so,
/// and nothing runs it.
#[test]
fn a_job_without_an_engine_waits_for_one() {
    // A queued job cannot run while its worker has no engine.
    let (dir, db) = library("engine", &[(S1, None)]);
    let worker = Fake {
        no_engine: true,
        ..Fake::default()
    };
    let _runner = spawn(&dir, &db, &worker, &Switch::default(), &Switch::on());
    until_states(&db, &[(S1, JobState::Waiting(Some(Wait::Engine)))]);
    // Wait beyond the queue update to catch an unwanted start.
    pause(Duration::from_millis(300));
    assert!(worker.ran().is_empty());
}

/// A job whose engine failed, found by a nota with no engine at all,
/// waits for one without the old reason, which no longer says why.
#[test]
fn a_missing_engine_replaces_an_old_engine_failure() {
    let (dir, db) = library("engine-stale", &[(S1, None)]);
    // An earlier run's engine failed.
    let id = db.with(|db| db.jobs()).unwrap()[0].id;
    db.with(|db| {
        db.start_job(id)?;
        db.end_job(id, &JobEnd::EngineDown("exited: no model".into()))
    })
    .unwrap();
    let worker = Fake {
        no_engine: true,
        ..Fake::default()
    };
    let _runner = spawn(&dir, &db, &worker, &Switch::default(), &Switch::on());
    until_states(&db, &[(S1, JobState::Waiting(Some(Wait::Engine)))]);
    assert!(worker.ran().is_empty());
}

/// A job waiting for space keeps that wait while nota has no engine
/// either, so a nota with one later still checks for room first.
#[test]
fn a_wait_for_space_isnt_lost_to_a_missing_engine() {
    // The space wait must survive both disk states without an engine.
    let (dir, db) = library("space-engine", &[(S1, Some(Wait::Space))]);
    let idle = Fake {
        no_engine: true,
        ..Fake::default()
    };
    for room in [Switch::default(), Switch::on()] {
        let runner = spawn(&dir, &db, &idle, &Switch::default(), &room);
        pause(Duration::from_millis(500));
        drop(runner);
        assert_eq!(states(&db), [(S1, JobState::Waiting(Some(Wait::Space)))]);
    }
    // Once an engine returns, the job must still wait for space.
    let worker = Fake::default();
    let room = Switch::default();
    let runner = spawn(&dir, &db, &worker, &Switch::default(), &room);
    pause(Duration::from_millis(500));
    assert!(worker.ran().is_empty());
    room.set(true);
    runner.wake();
    until_states(&db, &[(S1, JobState::Done)]);
}

/// A failed engine start leaves later sessions free to run.
#[test]
fn an_engine_startup_failure_does_not_hold_up_other_sessions() {
    // The first session waits for an engine that could not start.
    let (_dir, db) = library("engine-next", &[(S1, None), (S2, None)]);
    let worker = Fake::default();
    worker.script(S1, &[Then::EngineDown("couldn't start")]);
    let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
    let mut runner = loop_with_clock(&db, &worker, &clock);
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    // The queue must skip that session while it waits.
    let next = runner.next().unwrap().unwrap();
    assert_eq!(next.session, S2);
    runner.run_one(&next);
    assert_eq!(
        states(&db),
        [
            (S1, JobState::EngineDown("couldn't start".into())),
            (S2, JobState::Done)
        ],
    );
    assert_eq!(worker.ran(), [S1, S2]);
}

/// Wakes cannot retry an engine until the supplied clock reaches the limit,
/// whether the run found its engine down or found it had none.
#[test]
fn engine_startup_retries_wait_one_idle_interval_and_then_finish() {
    for first in [Then::EngineDown("couldn't start"), Then::NoEngine] {
        // Keep time still so waking the runner cannot shorten the wait.
        let (_dir, db) = library("engine-retry", &[(S1, None)]);
        let worker = Fake::default();
        worker.script(S1, &[first, Then::Done]);
        let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
        let mut runner = loop_with_clock(&db, &worker, &clock);
        let job = runner.next().unwrap().unwrap();
        runner.run_one(&job);
        for _ in 0..10 {
            runner.shared.wake();
            assert!(runner.next().unwrap().is_none());
        }
        // Check both sides of the limit, to the nanosecond.
        clock.advance(Duration::from_nanos(4_999_999_999));
        assert!(runner.next().unwrap().is_none());
        clock.advance(Duration::from_nanos(1));
        let job = runner.next().unwrap().unwrap();
        runner.run_one(&job);
        assert_eq!(states(&db), [(S1, JobState::Done)]);
        assert_eq!(worker.ran(), [S1, S1]);
    }
}

/// Acceptance (#224): while a run goes on, the job already says why its
/// engine is down, and the run's end keeps that reason; the retry is still
/// one idle interval after the run's end, and the next session isn't held
/// up meanwhile.
#[test]
fn a_run_says_why_its_engine_is_down_before_it_ends_and_keeps_the_retry_spacing() {
    let (_dir, db) = library("engine-down-live", &[(S1, None), (S2, None)]);
    let why = "the transcriber stopped working: couldn't start: gone";
    let worker = Fake {
        db: Some(db.clone()),
        ..Fake::default()
    };
    worker.script(S1, &[Then::DownNow(why), Then::Done]);
    let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
    let mut runner = loop_with_clock(&db, &worker, &clock);
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    assert_eq!(worker.seen(), [JobState::Retrying(why.into())]);
    assert_eq!(states(&db)[0], (S1, JobState::EngineDown(why.into())));
    // S2 runs meanwhile; S1 waits out the idle interval from its end.
    let job = runner.next().unwrap().unwrap();
    assert_eq!(job.session, S2);
    runner.run_one(&job);
    clock.advance(Duration::from_nanos(4_999_999_999));
    assert!(runner.next().unwrap().is_none());
    clock.advance(Duration::from_nanos(1));
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    assert_eq!(states(&db), [(S1, JobState::Done), (S2, JobState::Done)]);
    assert_eq!(worker.ran(), [S1, S2, S1]);
}

/// Acceptance (#224): once the engine is back, the running job no longer
/// says why it was down.
#[test]
fn an_engine_back_up_clears_the_reason_while_the_run_goes_on() {
    let (_dir, db) = library("engine-down-back", &[(S1, None)]);
    let why = "the transcriber stopped working: exited: no model";
    let worker = Fake {
        db: Some(db.clone()),
        ..Fake::default()
    };
    worker.script(S1, &[Then::DownAndBack(why)]);
    let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
    let mut runner = loop_with_clock(&db, &worker, &clock);
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    assert_eq!(
        worker.seen(),
        [JobState::Retrying(why.into()), JobState::Running]
    );
    assert_eq!(states(&db), [(S1, JobState::Done)]);
}

/// Runs the jobs as they come due, moving `clock` on a second at a time
/// while none is, until every job is done.
fn run_until_done(runner: &mut Loop<Fake, Switch, Switch>, clock: &nota_core::FakeClock) {
    for _ in 0..10_000 {
        match runner.next().unwrap() {
            Some(job) => runner.run_one(&job),
            None if states(&runner.db)
                .iter()
                .all(|(_, state)| *state == JobState::Done) =>
            {
                return;
            }
            None => clock.advance(Duration::from_secs(1)),
        }
    }
    panic!("never done: {:?}", states(&runner.db));
}

/// The job that comes due exactly `wait` from now: nothing is due a
/// nanosecond sooner. Leaves `clock` at `wait` from now.
fn due_after(
    runner: &Loop<Fake, Switch, Switch>,
    clock: &nota_core::FakeClock,
    wait: Duration,
) -> Job {
    clock.advance(wait.checked_sub(Duration::from_nanos(1)).unwrap());
    assert!(runner.next().unwrap().is_none(), "due before {wait:?}");
    clock.advance(Duration::from_nanos(1));
    runner.next().unwrap().unwrap()
}

/// Fails `S1`'s engine once per wait in `waits`, checking each retry
/// comes due after exactly that wait, then lets it finish.
fn check_retry_waits(name: &str, waits: &[u64]) {
    let (_dir, db) = library(name, &[(S1, None)]);
    let worker = Fake::default();
    let mut script = vec![Then::EngineDown("exited: no model"); waits.len()];
    script.push(Then::Done);
    worker.script(S1, &script);
    let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
    let mut runner = loop_with_clock(&db, &worker, &clock);
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    for &wait in waits {
        let job = due_after(&runner, &clock, Duration::from_secs(wait));
        runner.run_one(&job);
    }
    assert_eq!(states(&db), [(S1, JobState::Done)]);
    assert_eq!(worker.ran().len(), waits.len() + 1);
}

/// Acceptance (#247): sessions whose engine keeps failing are each tried
/// before any is tried again, even when every run outlasts the idle
/// interval.
#[test]
fn engine_down_sessions_each_get_a_turn_however_long_the_runs_take() {
    let (_dir, db) = library("engine-rotate", &[(S1, None), (S2, None), (S3, None)]);
    let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
    // Six seconds a run: longer than the idle interval, so the first
    // sessions come due again before the third has had a turn.
    let worker = Fake {
        clock: Some(Arc::clone(&clock)),
        takes: Duration::from_secs(6),
        ..Fake::default()
    };
    let down = Then::EngineDown("exited: no model");
    for session in [S1, S2, S3] {
        worker.script(session, &[down.clone(), down.clone(), Then::Done]);
    }
    let mut runner = loop_with_clock(&db, &worker, &clock);
    run_until_done(&mut runner, &clock);
    assert_eq!(worker.ran(), [S1, S2, S3, S1, S2, S3, S1, S2, S3]);
}

/// Acceptance (#247): each further engine failure doubles the wait before
/// the job is tried again.
#[test]
fn a_job_whose_engine_keeps_failing_waits_twice_as_long_each_time() {
    check_retry_waits("engine-backoff", &[5, 10, 20, 40]);
}

/// Acceptance (#247): the wait between retries stops growing at five
/// minutes.
#[test]
fn engine_retries_wait_at_most_five_minutes() {
    // 5 s doubled six times is 320 s, past the cap.
    check_retry_waits("engine-cap", &[5, 10, 20, 40, 80, 160, 300, 300]);
}

/// Acceptance (#247): a run whose engine confirms audio and then fails
/// is retried after the shortest wait again, and its next failure doubles
/// from there.
#[test]
fn an_engine_that_answers_starts_its_retry_spacing_again() {
    let (_dir, db) = library("engine-reset", &[(S1, None)]);
    let worker = Fake::default();
    let down = Then::EngineDown("exited: no model");
    worker.script(
        S1,
        &[
            down.clone(),
            down.clone(),
            Then::AnsweredThenDown("the transcriber stopped working"),
            down,
            Then::Done,
        ],
    );
    let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
    let mut runner = loop_with_clock(&db, &worker, &clock);
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    // Two failures: the spacing has grown to 10 s when the engine answers.
    for wait in [5, 10, 5, 10] {
        let job = due_after(&runner, &clock, Duration::from_secs(wait));
        runner.run_one(&job);
    }
    assert_eq!(states(&db), [(S1, JobState::Done)]);
    assert_eq!(worker.ran(), [S1; 5]);
}

/// Acceptance (#247): an engine confirming audio for one session puts the
/// other sessions' retries back to the shortest wait too.
#[test]
fn an_engine_that_answers_for_one_session_resets_the_others() {
    let (_dir, db) = library("engine-reset-all", &[(S1, None), (S2, None)]);
    let worker = Fake::default();
    let down = Then::EngineDown("exited: no model");
    worker.script(S1, &[down.clone(), down.clone(), Then::Done]);
    worker.script(S2, &[down, Then::Answered]);
    let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
    let mut runner = loop_with_clock(&db, &worker, &clock);
    for _ in 0..2 {
        let job = runner.next().unwrap().unwrap();
        runner.run_one(&job);
    }
    // At 5 s both come due: S1 fails again (its spacing now 10 s), then
    // S2's engine answers.
    let job = due_after(&runner, &clock, Duration::from_secs(5));
    runner.run_one(&job);
    let job = runner.next().unwrap().unwrap();
    assert_eq!(job.session, S2);
    runner.run_one(&job);
    let job = due_after(&runner, &clock, Duration::from_secs(5));
    runner.run_one(&job);
    assert_eq!(states(&db), [(S1, JobState::Done), (S2, JobState::Done)]);
    assert_eq!(worker.ran(), [S1, S2, S1, S2, S1]);
}

/// Acceptance (#247): a session queued while another has backed off
/// waits for that one's turn before it's retried, rather than being
/// retried on its shorter spacing meanwhile.
#[test]
fn a_session_queued_later_waits_its_turn_behind_one_that_backed_off() {
    let (_dir, db) = library("engine-turns", &[(S1, None)]);
    let worker = Fake::default();
    let down = Then::EngineDown("exited: no model");
    worker.script(S1, &[down.clone(), down.clone(), Then::Done]);
    worker.script(S2, &[down, Then::Done]);
    let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
    let mut runner = loop_with_clock(&db, &worker, &clock);
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    let job = due_after(&runner, &clock, Duration::from_secs(5));
    runner.run_one(&job);
    // S1 now waits 10 s, to 15 s. S2, stopped at 6 s, fails at once.
    clock.advance(Duration::from_secs(1));
    queue(&db, S2, None);
    let job = runner.next().unwrap().unwrap();
    assert_eq!(job.session, S2);
    runner.run_one(&job);
    // At 11 s S2's own 5 s are up, but S1 ended first and isn't due.
    let job = due_after(&runner, &clock, Duration::from_secs(9));
    assert_eq!(job.session, S1);
    runner.run_one(&job);
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    assert_eq!(states(&db), [(S1, JobState::Done), (S2, JobState::Done)]);
    assert_eq!(worker.ran(), [S1, S1, S2, S1, S2]);
}

/// A retry stopped by a recording runs again once it has stopped, and
/// keeps the spacing it had grown to.
#[test]
fn a_retry_stopped_by_a_recording_keeps_its_spacing() {
    let (_dir, db) = library("engine-paused", &[(S1, None)]);
    let worker = Fake::default();
    let down = Then::EngineDown("exited: no model");
    worker.script(
        S1,
        &[
            down.clone(),
            down.clone(),
            Then::UntilStopped,
            down,
            Then::Done,
        ],
    );
    let clock = Arc::new(nota_core::FakeClock::new(SessionTime::ZERO));
    let mut runner = loop_with_clock(&db, &worker, &clock);
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    let job = due_after(&runner, &clock, Duration::from_secs(5));
    runner.run_one(&job);
    // A recording stops the third run as soon as it starts.
    let job = due_after(&runner, &clock, Duration::from_secs(10));
    runner.capture.set(true);
    runner.run_one(&job);
    runner.capture.set(false);
    let job = runner.next().unwrap().unwrap();
    runner.run_one(&job);
    // The failure after the pause doubles from 10 s, not from 5 s.
    let job = due_after(&runner, &clock, Duration::from_secs(20));
    runner.run_one(&job);
    assert_eq!(states(&db), [(S1, JobState::Done)]);
    assert_eq!(worker.ran(), [S1; 5]);
}

/// Queues `session`'s jobs as a stop would, waiting for what `waits` says.
fn queue(db: &Writer, session: SessionId, waits: Option<Wait>) {
    db.with(|db| {
        db.create_session(&NewSession {
            id: session,
            title: None,
            language: None,
            started_at: None,
            tracks: Vec::new(),
        })?;
        db.finish_recording(session, waits)
    })
    .unwrap();
}

/// A runner loop using time the test can advance.
fn loop_with_clock(
    db: &Writer,
    worker: &Fake,
    clock: &Arc<nota_core::FakeClock>,
) -> Loop<Fake, Switch, Switch> {
    Loop {
        db: db.clone(),
        worker: worker.clone(),
        capture: Switch::default(),
        room: Switch::on(),
        shared: Arc::new(Shared::default()),
        clock: Arc::clone(clock) as Arc<dyn Clock>,
        retries: BTreeMap::new(),
    }
}

/// A clock that reports when the runner reads it.
#[derive(Debug)]
struct ReadClock {
    /// Receives each reading so the test can wait for the runner.
    readings: mpsc::Sender<SessionTime>,
}

impl Clock for ReadClock {
    fn now(&self) -> SessionTime {
        let now = SessionTime::from_nanos(123);
        self.readings.send(now).unwrap();
        now
    }

    fn suspended(&self) -> Duration {
        Duration::ZERO
    }

    fn awake_to_session(&self, _: Duration) -> Option<SessionTime> {
        None
    }
}

/// A spawned runner uses the supplied clock when an engine cannot start.
#[test]
fn a_spawned_runner_reads_the_supplied_clock() {
    // An engine failure makes the runner note when it can retry.
    let (dir, db) = library("supplied-clock", &[(S1, None)]);
    let worker = Fake::default();
    worker.script(S1, &[Then::NoEngine]);
    let (readings, read) = mpsc::channel();
    let runner = Runner::spawn(
        &dir.0,
        db.clone(),
        worker,
        Switch::default(),
        Switch::on(),
        Arc::new(ReadClock { readings }),
    )
    .unwrap();
    // Wait for the read rather than guessing when the thread has run.
    assert_eq!(
        read.recv_timeout(Duration::from_secs(10)).unwrap(),
        SessionTime::from_nanos(123),
    );
    assert_eq!(states(&db), [(S1, JobState::Waiting(Some(Wait::Engine)))]);
    // Join before closing the receiver, so later reads still have a home.
    drop(runner);
}
