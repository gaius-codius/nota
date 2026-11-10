use std::cell::RefCell;
use std::path::PathBuf;

use nota_core::{EpochId, SystemClock};
use nota_recorder::engine::OfflineReason;
use nota_recorder::fs::Fs as _;
use nota_store::{JobState, NewSession, Sha256Digest};

use super::*;

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("nota-final-pass-{name}-{}", std::process::id()));
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

const SESSION: SessionId = SessionId::new(1);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

fn s(n: u64) -> SampleIndex {
    SampleIndex::new(n)
}

fn range(a: u64, b: u64) -> SampleRange {
    SampleRange::new(s(a), s(b)).unwrap()
}

fn row(track: TrackId, a: u64, b: u64) -> SegmentRow {
    SegmentRow::new(
        track,
        EpochId::new(0),
        range(a, b),
        Sha256Digest::new([0; 32]),
    )
    .unwrap()
}

fn heard_by() -> HeardBy {
    HeardBy {
        engine: "fake".into(),
        model: "fake".into(),
    }
}

fn db(dir: &TestDir) -> Writer {
    let db = Writer::new(&dir.0.join("library.db"));
    db.with(|db| {
        db.create_session(&NewSession {
            id: SESSION,
            title: None,
            language: None,
            started_at: None,
            tracks: Vec::new(),
        })
    })
    .unwrap();
    db
}

/// Each track's segments, in sample order, from where the pass got to.
#[test]
fn the_pass_starts_each_track_where_it_got_to() {
    let tracks = tracks(
        vec![
            row(SYSTEM, 0, 10),
            row(MIC, 50, 60),
            row(MIC, 0, 50),
            row(SYSTEM, 10, 20),
        ],
        &BTreeMap::from([(MIC, s(30))]),
    );
    assert_eq!(
        tracks,
        [
            TrackAudio {
                track: MIC,
                segments: vec![row(MIC, 0, 50), row(MIC, 50, 60)],
                from: s(30),
            },
            TrackAudio {
                track: SYSTEM,
                segments: vec![row(SYSTEM, 0, 10), row(SYSTEM, 10, 20)],
                from: s(0),
            },
        ]
    );
}

/// What the engine confirms is stored as the final pass's text, skipped
/// audio as text-less, with each track's progress, and the job's progress
/// counts the published samples done.
#[test]
fn confirmed_text_is_stored_with_its_progress() {
    let dir = TestDir::new("stored");
    let db = db(&dir);
    let tracks = tracks(
        vec![row(MIC, 0, 100), row(MIC, 200, 300), row(SYSTEM, 0, 100)],
        &BTreeMap::new(),
    );
    let reported = RefCell::new(Vec::new());
    let report = |p: Progress| reported.borrow_mut().push((p.done, p.total));
    let stop = || false;
    let running = Running {
        stop: &stop,
        progress: &report,
        answered: &|| {},
        engine_down: &|_| {},
    };
    let mut sink = Stored::new(db.clone(), SESSION, heard_by(), &tracks, &running);
    assert_eq!(
        sink.progress(),
        Progress {
            done: 0,
            total: 300
        }
    );
    let word = |text: &str, a, b| {
        nota_core::messages::HeardWord::new(text.to_owned(), range(a, b)).unwrap()
    };
    let text = Transcript::new(MIC, range(0, 80), "hello there".into())
        .unwrap()
        .with_words(vec![word("hello", 0, 40), word("there", 40, 80)])
        .unwrap();
    sink.confirmed(MIC, s(100), vec![text], vec![]).unwrap();
    sink.confirmed(MIC, s(250), vec![], vec![range(200, 250)])
        .unwrap();
    assert_eq!(*reported.borrow(), [(100, 300), (150, 300)]);
    let stored = db.with(|db| db.final_texts(SESSION)).unwrap();
    assert_eq!(
        stored
            .iter()
            .map(|t| (t.track, t.range, t.text.clone()))
            .collect::<Vec<_>>(),
        [
            (MIC, range(0, 80), Some("hello there".to_owned())),
            (MIC, range(200, 250), None),
        ]
    );
    assert_eq!(stored[0].heard_by, heard_by());
    // The words, by sample, as the engine gave them.
    let word = |text: &str, a, b| FinalWord {
        text: text.to_owned(),
        range: range(a, b),
    };
    assert_eq!(
        stored[0].words,
        [word("hello", 0, 40), word("there", 40, 80)]
    );
    assert!(stored[1].words.is_empty());
    assert_eq!(
        db.with(|db| db.final_progress(SESSION)).unwrap()[&MIC],
        s(250)
    );
}

/// The runner hears once that the engine answered, at the first audio the
/// engine itself confirms; audio skipped without it says nothing.
#[test]
fn the_first_engine_confirmation_says_the_engine_answered() {
    let dir = TestDir::new("answered");
    let db = db(&dir);
    let tracks = tracks(vec![row(MIC, 0, 300)], &BTreeMap::new());
    let told = RefCell::new(0);
    let answered = || *told.borrow_mut() += 1;
    let stop = || false;
    let running = Running {
        stop: &stop,
        progress: &|_| {},
        answered: &answered,
        engine_down: &|_| {},
    };
    let mut sink = Stored::new(db, SESSION, heard_by(), &tracks, &running);
    // Skipped audio (unreadable, or dropped while the engine was down)
    // proves nothing about the engine.
    sink.confirmed(MIC, s(100), vec![], vec![range(0, 100)])
        .unwrap();
    assert_eq!(*told.borrow(), 0);
    sink.confirmed(MIC, s(200), vec![], vec![]).unwrap();
    sink.confirmed(MIC, s(300), vec![], vec![]).unwrap();
    assert_eq!(*told.borrow(), 1);
}

/// The runner hears why the engine is down as the pass does, and that
/// it's back, in order, while the pass runs.
#[test]
fn the_runner_hears_why_the_engine_is_down_while_the_pass_runs() {
    let dir = TestDir::new("engine-down");
    let db = db(&dir);
    let tracks = tracks(vec![row(MIC, 0, 300)], &BTreeMap::new());
    let heard = RefCell::new(Vec::new());
    let engine_down = |why: Option<&str>| heard.borrow_mut().push(why.map(str::to_owned));
    let stop = || false;
    let running = Running {
        stop: &stop,
        progress: &|_| {},
        answered: &|| {},
        engine_down: &engine_down,
    };
    let mut sink = Stored::new(db, SESSION, heard_by(), &tracks, &running);
    sink.engine_down(Some("the transcriber stopped working: couldn't start"));
    sink.engine_down(None);
    assert_eq!(
        *heard.borrow(),
        [
            Some("the transcriber stopped working: couldn't start".to_owned()),
            None
        ]
    );
}

/// Acceptance: a final pass run again over a session's stored chunks (its
/// progress lost, so from the start) leaves them, and the session's one
/// revision, unchanged: the same text again stores nothing, and other text
/// for a stored chunk is refused.
#[test]
fn a_rerun_over_stored_chunks_leaves_them_unchanged() {
    let dir = TestDir::new("rerun");
    let db = db(&dir);
    let tracks = tracks(vec![row(MIC, 0, 100)], &BTreeMap::new());
    let report = |_: Progress| {};
    let stop = || false;
    let running = Running {
        stop: &stop,
        progress: &report,
        answered: &|| {},
        engine_down: &|_| {},
    };
    let heard = |text: &str| Transcript::new(MIC, range(0, 80), text.into()).unwrap();
    Stored::new(db.clone(), SESSION, heard_by(), &tracks, &running)
        .confirmed(MIC, s(100), vec![heard("hello there")], vec![])
        .unwrap();
    let before = db.with(|db| db.final_texts(SESSION)).unwrap();

    let mut rerun = Stored::new(db.clone(), SESSION, heard_by(), &tracks, &running);
    rerun
        .confirmed(MIC, s(100), vec![heard("hello there")], vec![])
        .unwrap();
    // An engine that hears the chunk differently this time.
    assert!(matches!(
        rerun.confirmed(MIC, s(100), vec![heard("yellow hair")], vec![]),
        Err(StoreError::FinalOverlap { track: MIC, .. })
    ));
    assert_eq!(db.with(|db| db.final_texts(SESSION)).unwrap(), before);
    assert_eq!(
        db.with(|db| db.revisions(SESSION)).unwrap(),
        [nota_store::RevisionNumber::FINAL]
    );
}

/// Without an engine, the final pass waits for one rather than failing.
#[test]
fn without_an_engine_the_final_pass_waits_for_one() {
    let dir = TestDir::new("no-engine");
    let library = Library::open(&dir.0).unwrap();
    let job = db(&dir)
        .with(|db| {
            db.finish_recording(SESSION, None)?;
            db.jobs()
        })
        .unwrap()
        .remove(0);
    assert_eq!(
        (job.kind, job.state.clone()),
        (JobKind::FinalPass, JobState::Waiting(None))
    );
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::start().unwrap());
    let mut jobs = Jobs::new(library, None, clock);
    assert_eq!(jobs.lacks(&job), Some(Wait::Engine));
    let stop = || false;
    let progress = |_| {};
    let running = Running {
        stop: &stop,
        progress: &progress,
        answered: &|| {},
        engine_down: &|_| {},
    };
    assert_eq!(
        jobs.run(&job, &running),
        JobEnd::Waiting(Some(Wait::Engine))
    );
}

/// A store error ends the job waiting for space if the disk is full, and
/// failed otherwise.
#[test]
fn a_full_disk_waits_and_other_store_errors_fail() {
    let full = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
        None,
    ));
    assert_eq!(stored(&full), JobEnd::Waiting(Some(Wait::Space)));
    assert!(matches!(
        stored(&StoreError::NoSession(SESSION)),
        JobEnd::Failed(why) if why.contains("is not in the library")
    ));
}

/// The final pass waits while its session still has journals to publish
/// (salvage does that at the next start), so it covers their audio.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding: a journal-named file"
)]
fn the_final_pass_waits_for_journals_to_be_published() {
    let dir = TestDir::new("journals");
    let library = Library::open(&dir.0).unwrap();
    let job = db(&dir)
        .with(|db| {
            db.finish_recording(SESSION, None)?;
            db.jobs()
        })
        .unwrap()
        .remove(0);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::start().unwrap());
    let paths = library.session(SESSION);
    let audio = paths.audio();
    StdFs.create_dir(&paths.dir).unwrap();
    StdFs.create_dir(&audio).unwrap();
    let engine = Engine {
        command: EngineCommand {
            program: PathBuf::from("nota"),
            args: Vec::new(),
        },
        heard_by: heard_by(),
    };
    let jobs = Jobs::new(library, Some(engine), clock);
    assert_eq!(jobs.lacks(&job), None);
    drop(StdFs.create(&audio.join("journal-000003")).unwrap());
    assert_eq!(jobs.lacks(&job), Some(Wait::Audio));
    // A journal set aside as damaged isn't one.
    std::fs::rename(
        audio.join("journal-000003"),
        audio.join("journal-000003.unreadable"),
    )
    .unwrap();
    assert_eq!(jobs.lacks(&job), None);
}

/// The engine's spawn failure as the supervisor reports it when nota's
/// executable is gone.
fn missing_executable() -> OfflineReason {
    OfflineReason::SpawnFailed(std::io::Error::from_raw_os_error(2).to_string())
}

/// The engine's exit as the supervisor reports it when its model doesn't
/// load: the last line of its stderr.
fn broken_model() -> OfflineReason {
    OfflineReason::Exited {
        status: None,
        stderr: "loading\nnota engine asr: the model at /m is not a model".into(),
    }
}

/// Acceptance: an engine that can't start or answer leaves the pass
/// waiting for a working one, with a reason that tells a missing
/// executable from a broken model.
#[test]
fn an_engine_that_cant_start_or_answer_waits_with_its_reason() {
    let missing = pass_end(PassEnd::Stalled(Some(missing_executable().to_string())));
    let broken = pass_end(PassEnd::Stalled(Some(broken_model().to_string())));
    assert_eq!(
        missing,
        JobEnd::EngineDown(format!(
            "the transcriber stopped working: couldn't start: {}",
            std::io::Error::from_raw_os_error(2)
        ))
    );
    assert_eq!(
        broken,
        JobEnd::EngineDown(
            "the transcriber stopped working: exited: \
             nota engine asr: the model at /m is not a model"
                .into()
        )
    );
    assert_eq!(
        pass_end(PassEnd::Stalled(None)),
        JobEnd::EngineDown("the transcriber stopped answering".into())
    );
    // The supervisor's own thread failing to start has its own wording.
    let failed = PassEnd::Engine(std::io::Error::new(
        std::io::ErrorKind::OutOfMemory,
        "no threads",
    ));
    assert_eq!(
        pass_end(failed),
        JobEnd::EngineDown("the transcriber couldn't be started: no threads".into())
    );
    assert_eq!(pass_end(PassEnd::Done), JobEnd::Done);
    assert_eq!(pass_end(PassEnd::Stopped), JobEnd::Waiting(None));
}

/// Acceptance: the two reasons stay apart once stored, while the job
/// waits to run again.
#[test]
fn a_missing_executable_and_a_broken_model_are_stored_apart() {
    let dir = TestDir::new("engine-reasons");
    let library = Library::open(&dir.0).unwrap();
    let db = library.db();
    for reason in [missing_executable(), broken_model()] {
        let paths = library.create().unwrap();
        db.with(|db| {
            db.create_session(&NewSession::bare(paths.id))?;
            db.finish_recording(paths.id, None)
        })
        .unwrap();
        let job = db.with(|db| db.session_jobs(paths.id)).unwrap().remove(0);
        let end = pass_end(PassEnd::Stalled(Some(reason.to_string())));
        db.with(|db| {
            db.start_job(job.id)?;
            db.end_job(job.id, &end)
        })
        .unwrap();
    }
    let states: Vec<JobState> = db
        .with(|db| db.jobs())
        .unwrap()
        .into_iter()
        .map(|job| job.state)
        .collect();
    assert_eq!(
        states,
        [
            JobState::EngineDown(format!(
                "the transcriber stopped working: couldn't start: {}",
                std::io::Error::from_raw_os_error(2)
            )),
            JobState::EngineDown(
                "the transcriber stopped working: exited: \
                 nota engine asr: the model at /m is not a model"
                    .into()
            ),
        ]
    );
}
