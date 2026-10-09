use nota_core::SessionId;

use super::*;
use crate::test_dir::TestDir;
use crate::tests::new_session;

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);

fn store(name: &str) -> (TestDir, Store) {
    let dir = TestDir::new(name);
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    store.create_session(&new_session(S2)).unwrap();
    (dir, store)
}

fn states(store: &Store) -> Vec<(SessionId, JobState)> {
    store
        .jobs()
        .unwrap()
        .into_iter()
        .map(|job| (job.session, job.state))
        .collect()
}

/// Stopping a recording marks it stopped and queues its final pass, in
/// order of the stops; stopping again queues nothing more.
#[test]
fn a_stop_queues_the_sessions_jobs_once() {
    let (_dir, mut store) = store("queue");
    assert!(store.jobs().unwrap().is_empty());
    store.finish_recording(S2, None).unwrap();
    store.finish_recording(S1, Some(Wait::Space)).unwrap();
    store.finish_recording(S2, Some(Wait::Space)).unwrap();
    let jobs = store.jobs().unwrap();
    assert_eq!(
        jobs.iter()
            .map(|j| (j.session, j.kind, j.state.clone(), j.attempts, j.progress))
            .collect::<Vec<_>>(),
        [
            (
                S2,
                JobKind::FinalPass,
                JobState::Waiting(None),
                0,
                Progress::default()
            ),
            (
                S1,
                JobKind::FinalPass,
                JobState::Waiting(Some(Wait::Space)),
                0,
                Progress::default()
            ),
        ]
    );
    assert!(jobs[0].id < jobs[1].id);
    for session in [S1, S2] {
        assert_eq!(
            store.session(session).unwrap().unwrap().state,
            SessionState::Stopped
        );
    }
    assert_eq!(store.session_jobs(S1).unwrap(), [jobs[1].clone()]);
    assert!(matches!(
        store.finish_recording(SessionId::new(9), None),
        Err(StoreError::NoSession(_))
    ));
}

/// Salvage's stop of a recording that never finished queues its jobs
/// too; a session that wasn't recording gets none from it.
#[test]
fn a_recording_stopped_by_salvage_gets_its_jobs() {
    let (_dir, mut store) = store("salvage");
    assert!(store.stop_recording(S1).unwrap());
    assert!(!store.stop_recording(S1).unwrap());
    store.set_state(S2, SessionState::Stopped).unwrap();
    assert!(!store.stop_recording(S2).unwrap());
    assert_eq!(states(&store), [(S1, JobState::Waiting(None))]);
}

/// A job runs from waiting, notes its progress, and ends done, failed or
/// waiting again; only a waiting job can be started.
#[test]
fn a_job_runs_and_ends() {
    let (_dir, mut store) = store("run");
    store.finish_recording(S1, Some(Wait::Engine)).unwrap();
    let id = store.jobs().unwrap()[0].id;
    // Progress is noted only while running.
    let half = Progress {
        done: 50,
        total: 100,
    };
    store.job_progress(id, half).unwrap();
    assert_eq!(store.jobs().unwrap()[0].progress, Progress::default());
    assert!(store.start_job(id).unwrap());
    assert!(!store.start_job(id).unwrap());
    store.job_progress(id, half).unwrap();
    let job = &store.jobs().unwrap()[0];
    assert_eq!((job.state.clone(), job.progress), (JobState::Running, half));

    store.end_job(id, &JobEnd::Waiting(None)).unwrap();
    assert_eq!(states(&store), [(S1, JobState::Waiting(None))]);
    // Paused, it keeps its progress.
    assert_eq!(store.jobs().unwrap()[0].progress, half);
    assert!(store.start_job(id).unwrap());
    store
        .end_job(id, &JobEnd::Failed("no audio".into()))
        .unwrap();
    assert_eq!(
        states(&store),
        [(S1, JobState::Failed("no audio".to_owned()))]
    );
    assert!(!store.start_job(id).unwrap());
    store.end_job(id, &JobEnd::Done).unwrap();
    assert_eq!(states(&store), [(S1, JobState::Done)]);
    assert!(matches!(
        store.end_job(JobId(99), &JobEnd::Done),
        Err(StoreError::NoJob(99))
    ));
}

/// Jobs a dead runner left running wait to run again, with the attempt
/// counted; once `limit` attempts have died, the job fails instead.
#[test]
fn jobs_left_running_are_taken_back_until_they_have_died_too_often() {
    let (_dir, mut store) = store("recover");
    store.finish_recording(S1, None).unwrap();
    store.finish_recording(S2, None).unwrap();
    let ids: Vec<JobId> = store.jobs().unwrap().iter().map(|j| j.id).collect();
    assert_eq!(store.recover_jobs(3).unwrap(), 0);
    for attempt in 1..=3 {
        assert!(store.start_job(ids[0]).unwrap());
        assert_eq!(store.recover_jobs(3).unwrap(), 1);
        let job = &store.jobs().unwrap()[0];
        assert_eq!(job.attempts, attempt);
        if attempt < 3 {
            assert_eq!(job.state, JobState::Waiting(None));
        }
    }
    assert_eq!(
        states(&store),
        [
            (
                S1,
                JobState::Failed("nota stopped while running it 3 times".to_owned())
            ),
            (S2, JobState::Waiting(None)),
        ]
    );
}

/// A stored job that doesn't parse is an error, not a guess.
#[test]
fn a_bad_job_row_is_corrupt() {
    let (_dir, store) = store("corrupt");
    for (kind, state, waits) in [
        ("summary", "waiting", None),
        ("final-pass", "paused", None),
        ("final-pass", "waiting", Some("time")),
        ("final-pass", "done", Some("space")),
    ] {
        store
            .conn
            .execute(
                "INSERT INTO job (session_id, kind, state, waits_for) VALUES (1, ?1, ?2, ?3)",
                rusqlite::params![kind, state, waits],
            )
            .unwrap();
        assert!(
            matches!(store.jobs(), Err(StoreError::Corrupt(_))),
            "{kind} {state} {waits:?}"
        );
        store.conn.execute("DELETE FROM job", []).unwrap();
    }
}

/// A session adopted from disk stopped while the library couldn't take
/// it, so its stop queued nothing: adopting it queues its jobs.
#[test]
fn an_adopted_session_gets_its_jobs() {
    let (_dir, mut store) = store("adopt");
    let found = SessionId::new(7);
    assert_eq!(
        store
            .adopt_session(&crate::NewSession::bare(found), None)
            .unwrap(),
        crate::Adopted::Added
    );
    assert_eq!(states(&store), [(found, JobState::Waiting(None))]);
    // Adopting it again changes nothing.
    store
        .adopt_session(&crate::NewSession::bare(found), None)
        .unwrap();
    assert_eq!(store.jobs().unwrap().len(), 1);
}
