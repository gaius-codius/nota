use std::cell::RefCell;
use std::path::PathBuf;

use nota_core::{EpochId, SystemClock};
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
    };
    let mut sink = Stored::new(db.clone(), SESSION, heard_by(), &tracks, &running);
    assert_eq!(
        sink.progress(),
        Progress {
            done: 0,
            total: 300
        }
    );
    let text = Transcript::new(MIC, range(0, 80), "hello".into()).unwrap();
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
            (MIC, range(0, 80), Some("hello".to_owned())),
            (MIC, range(200, 250), None),
        ]
    );
    assert_eq!(stored[0].heard_by, heard_by());
    assert_eq!(
        db.with(|db| db.final_progress(SESSION)).unwrap()[&MIC],
        s(250)
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
