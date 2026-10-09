use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;

use nota_core::{Clock, SessionId, SystemClock};
use nota_store::NewSession;

use super::*;

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

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
        db.with(|db| {
            db.create_session(&NewSession {
                id,
                title: None,
                language: None,
                started_at: None,
                tracks: Vec::new(),
            })?;
            db.finish_recording(id, waits)
        })
        .unwrap();
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
}

/// A worker that does as told per session, and notes each run.
#[derive(Clone, Default)]
struct Fake {
    script: Arc<Mutex<BTreeMap<SessionId, Vec<Then>>>>,
    ran: Arc<Mutex<Vec<SessionId>>>,
    no_engine: bool,
}

impl Fake {
    fn script(&self, session: SessionId, then: &[Then]) {
        self.script.lock().unwrap().insert(session, then.to_vec());
    }

    fn ran(&self) -> Vec<SessionId> {
        self.ran.lock().unwrap().clone()
    }
}

impl Worker for Fake {
    fn run(&mut self, job: &Job, running: &Running<'_>) -> JobEnd {
        self.ran.lock().unwrap().push(job.session);
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
struct Switch(Arc<AtomicBool>);

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
    )
    .unwrap()
}

/// Acceptance (GAI-316): three sessions' jobs run in order, and a failed
/// job doesn't block the next session's.
#[test]
fn three_sessions_run_in_order_and_a_failure_holds_nothing_up() {
    let (dir, db) = library("order", &[(S2, None), (S1, None), (S3, None)]);
    let worker = Fake::default();
    worker.script(S1, &[Then::Fail]);
    let _runner = spawn(&dir, &db, &worker, &Switch::default(), &Switch::on());
    until_states(
        &db,
        &[
            (S2, JobState::Done),
            (S1, JobState::Failed("it broke".into())),
            (S3, JobState::Done),
        ],
    );
    // In the order they were queued, each once.
    assert_eq!(worker.ran(), [S2, S1, S3]);
    let jobs = db.with(|db| db.jobs()).unwrap();
    assert!(
        jobs.iter()
            .all(|j| j.progress == Progress { done: 1, total: 2 })
    );
}

/// Acceptance (GAI-316): no job starts while a recording runs; once it
/// stops, they do.
#[test]
fn no_job_starts_while_a_recording_runs() {
    let (dir, db) = library("capture-first", &[(S1, None)]);
    let worker = Fake::default();
    let recording = Switch::on();
    let runner = spawn(&dir, &db, &worker, &recording, &Switch::on());
    pause(Duration::from_millis(1_500));
    assert!(worker.ran().is_empty());
    assert_eq!(states(&db), [(S1, JobState::Waiting(None))]);
    recording.set(false);
    runner.wake();
    until_states(&db, &[(S1, JobState::Done)]);
}

/// A job running when a recording starts stops at once and waits, keeping
/// its progress, then runs again once the recording has stopped.
#[test]
fn a_recording_that_starts_pauses_the_running_job() {
    let (dir, db) = library("pause", &[(S1, None)]);
    let worker = Fake::default();
    worker.script(S1, &[Then::UntilStopped]);
    let recording = Switch::default();
    let runner = spawn(&dir, &db, &worker, &recording, &Switch::on());
    until_states(&db, &[(S1, JobState::Running)]);
    recording.set(true);
    until_states(&db, &[(S1, JobState::Waiting(None))]);
    let paused = db.with(|db| db.jobs()).unwrap().remove(0);
    assert_eq!(paused.progress, Progress { done: 1, total: 2 });
    // A pause isn't a death: nothing is counted against it.
    assert_eq!(paused.attempts, 0);
    pause(Duration::from_millis(300));
    assert_eq!(worker.ran(), [S1]);
    recording.set(false);
    runner.wake();
    until_states(&db, &[(S1, JobState::Done)]);
    assert_eq!(worker.ran(), [S1, S1]);
}

/// Acceptance (GAI-316): a session stopped by a full disk has its jobs
/// wait, not fail, while space is short, and they run once it's back;
/// other sessions' jobs aren't held up, and one that runs out of space
/// itself waits for it too.
#[test]
fn a_full_disk_sessions_jobs_wait_for_space_without_holding_others_up() {
    let (dir, db) = library("space", &[(S1, Some(Wait::Space)), (S2, None), (S3, None)]);
    let worker = Fake::default();
    worker.script(S3, &[Then::NoSpace, Then::Done]);
    let room = Switch::default();
    let runner = spawn(&dir, &db, &worker, &Switch::default(), &room);
    until_states(
        &db,
        &[
            (S1, JobState::Waiting(Some(Wait::Space))),
            (S2, JobState::Done),
            (S3, JobState::Waiting(Some(Wait::Space))),
        ],
    );
    pause(Duration::from_millis(300));
    assert_eq!(worker.ran(), [S2, S3]);
    room.set(true);
    runner.wake();
    until_states(
        &db,
        &[
            (S1, JobState::Done),
            (S2, JobState::Done),
            (S3, JobState::Done),
        ],
    );
    assert_eq!(worker.ran(), [S2, S3, S1, S3]);
}

/// Acceptance (GAI-316): a job a runner died while running runs again,
/// with the death counted, from where it got to.
#[test]
fn a_job_left_running_by_a_dead_runner_runs_again() {
    let (dir, db) = library("recover", &[(S1, None)]);
    let id = db.with(|db| db.jobs()).unwrap()[0].id;
    db.with(|db| db.start_job(id)).unwrap();
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
    drop(first);
    until_states(&db, &[(S1, JobState::Done)]);
    assert_eq!(worker.ran(), [S1]);
}

/// Without a speech engine, a job that needs one waits for it, saying so,
/// and nothing runs it.
#[test]
fn a_job_without_an_engine_waits_for_one() {
    let (dir, db) = library("engine", &[(S1, None)]);
    let worker = Fake {
        no_engine: true,
        ..Fake::default()
    };
    let _runner = spawn(&dir, &db, &worker, &Switch::default(), &Switch::on());
    until_states(&db, &[(S1, JobState::Waiting(Some(Wait::Engine)))]);
    pause(Duration::from_millis(300));
    assert!(worker.ran().is_empty());
}

/// A job waiting for space keeps that wait while nota has no engine
/// either, so a nota with one later still checks for room first.
#[test]
fn a_wait_for_space_isnt_lost_to_a_missing_engine() {
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
    let worker = Fake::default();
    let room = Switch::default();
    let runner = spawn(&dir, &db, &worker, &Switch::default(), &room);
    pause(Duration::from_millis(500));
    assert!(worker.ran().is_empty());
    room.set(true);
    runner.wake();
    until_states(&db, &[(S1, JobState::Done)]);
}
