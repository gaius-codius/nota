use std::path::PathBuf;
use std::sync::Arc;

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRate, SessionId, SessionTime,
    TrackId,
};

use super::*;
use crate::fs::fake::FakeFs;
use crate::segment::{DurableSegment, FakeStore, needs_salvage};
use crate::session::{SessionDir, SessionLock, SessionWriter};

const MIC: TrackId = TrackId::new(0);

fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

fn length() -> SegmentLength {
    SegmentLength::new(SampleCount::new(1_000)).unwrap()
}

fn session() -> PathBuf {
    PathBuf::from("/session")
}

fn db() -> PathBuf {
    PathBuf::from("/db")
}

/// A session with `windows` finished journals of `MIC`, one per window.
fn recorded(windows: u64) -> (FakeFs, SessionLock<FakeFs>, Vec<FinishedJournal>) {
    let fs = FakeFs::with_dirs([session(), db()]);
    let lock = SessionDir::new(SessionId::new(1), fs.clone(), &session())
        .lock()
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    let audio: Vec<i16> = (0..1_000 * windows).map(|i| (i % 1_000) as i16).collect();
    writer.append(MIC, &audio).unwrap();
    let journals = writer.finish().unwrap();
    assert_eq!(journals.len() as u64, windows);
    (fs, lock, journals)
}

fn publisher<T: SegmentStore + Send + 'static>(lock: &SessionLock<FakeFs>, store: T) -> Publisher {
    Publisher::spawn(SessionStore::new(lock.clone(), store), length()).unwrap()
}

fn starts(rows: &[SegmentRow]) -> Vec<u64> {
    rows.iter().map(|r| r.range().start().get()).collect()
}

#[test]
fn batches_are_published_in_order() {
    let (fs, lock, mut journals) = recorded(3);
    let publisher = publisher(&lock, FakeStore::new(&fs, &db()));
    let queue = publisher.queue();
    let last = journals.split_off(1);
    assert!(queue.send(journals));
    assert!(queue.send(Vec::new()));
    assert!(publisher.queue().send(last));
    drop(queue);
    let report = publisher.finish().unwrap();
    assert_eq!(starts(report.rows()), vec![0, 1_000, 2_000]);
    assert!(report.errors().is_empty());
    assert!(report.is_complete());
    assert!(!needs_salvage(lock.session()).unwrap());
}

#[test]
fn a_journal_that_failed_to_read_is_tried_again_with_the_next_batch() {
    let (fs, lock, mut journals) = recorded(2);
    let publisher = publisher(&lock, FakeStore::new(&fs, &db()));
    // The first journal fails to read once.
    fs.fail_after(0, io::ErrorKind::Other);
    let second = journals.split_off(1);
    assert!(publisher.queue().send(journals));
    assert!(publisher.queue().send(second));
    let report = publisher.finish().unwrap();
    assert!(report.errors().is_empty());
    assert_eq!(starts(report.rows()), vec![0, 1_000]);
    assert!(report.left().is_empty());
    assert!(report.is_complete());
    assert!(!needs_salvage(lock.session()).unwrap());
}

#[test]
fn a_journal_that_failed_to_read_is_tried_again_at_the_finish() {
    let (fs, lock, journals) = recorded(1);
    let publisher = publisher(&lock, FakeStore::new(&fs, &db()));
    fs.fail_after(0, io::ErrorKind::Other);
    assert!(publisher.queue().send(journals));
    let report = publisher.finish().unwrap();
    assert!(report.errors().is_empty());
    assert_eq!(starts(report.rows()), vec![0]);
    assert!(report.is_complete());
}

/// A store whose commits always fail.
#[derive(Debug)]
struct Broken;

#[derive(Debug)]
struct BrokenError;

impl fmt::Display for BrokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("broken")
    }
}

impl std::error::Error for BrokenError {}

impl SegmentStore for Broken {
    type Error = BrokenError;

    fn rows(&mut self) -> Result<Vec<SegmentRow>, BrokenError> {
        Ok(Vec::new())
    }

    fn insert(&mut self, _: &DurableSegment) -> Result<(), BrokenError> {
        Err(BrokenError)
    }
}

#[test]
fn journals_that_never_publish_are_left_for_salvage() {
    let (_fs, lock, journals) = recorded(2);
    let ids: Vec<_> = journals.iter().map(FinishedJournal::id).collect();
    let publisher = publisher(&lock, Broken);
    assert!(publisher.queue().send(journals));
    let report = publisher.finish().unwrap();
    // The batch, then the last try.
    assert_eq!(report.errors().len(), 2);
    assert!(report.rows().is_empty());
    let left: Vec<_> = report.left().iter().map(FinishedJournal::id).collect();
    assert_eq!(left, ids);
    assert!(!report.is_complete());
    assert!(needs_salvage(lock.session()).unwrap());
}

#[test]
fn an_unreadable_journal_leaves_the_report_incomplete() {
    let (fs, lock, journals) = recorded(1);
    // A directory where the journal was: there, but it can't be read.
    let path = session().join(journals[0].id().file_name());
    fs.remove(&path).unwrap();
    fs.create_dir(&path).unwrap();
    let publisher = publisher(&lock, FakeStore::new(&fs, &db()));
    assert!(publisher.queue().send(journals));
    let report = publisher.finish().unwrap();
    assert!(report.errors().is_empty());
    assert_eq!(report.left().len(), 1);
    assert!(!report.is_complete());
}

#[test]
fn a_queue_outliving_the_thread_reports_it_gone() {
    let (batches, received) = mpsc::channel::<Vec<FinishedJournal>>();
    drop(received);
    let queue = PublishQueue { batches };
    assert!(queue.send(Vec::new()));
    let (_fs, _lock, journals) = recorded(1);
    assert!(!queue.send(journals));
    assert_eq!(
        PublisherPanicked.to_string(),
        "the segment publisher stopped unexpectedly"
    );
}

/// A store whose `fail_at`th commit (counting from 0) fails, once.
#[derive(Debug)]
struct FailsOnce {
    store: FakeStore,
    commits: usize,
    fail_at: usize,
}

impl SegmentStore for FailsOnce {
    type Error = BrokenError;

    fn rows(&mut self) -> Result<Vec<SegmentRow>, BrokenError> {
        self.store.rows().map_err(|_| BrokenError)
    }

    fn insert(&mut self, segment: &DurableSegment) -> Result<(), BrokenError> {
        let n = self.commits;
        self.commits += 1;
        if n == self.fail_at {
            return Err(BrokenError);
        }
        self.store.insert(segment).map_err(|_| BrokenError)
    }
}

#[test]
fn rows_committed_by_a_run_that_failed_are_counted() {
    let (fs, lock, journals) = recorded(2);
    let store = FailsOnce {
        store: FakeStore::new(&fs, &db()),
        commits: 0,
        fail_at: 1,
    };
    let publisher = publisher(&lock, store);
    assert!(publisher.queue().send(journals));
    let report = publisher.finish().unwrap();
    // The first run committed window 0, then failed at window 1; the last
    // try committed window 1.
    assert_eq!(report.errors().len(), 1);
    assert_eq!(starts(report.rows()), vec![0, 1_000]);
    assert!(report.is_complete());
    assert!(!needs_salvage(lock.session()).unwrap());
}

/// The end of a recording whose last journal can't be finished (the disk
/// is gone): the journals that did finish are still handed to the
/// publisher, which reports them left for salvage, and the error is
/// reported. `meanwhile` runs before the wait.
#[test]
fn finishing_a_recording_hands_on_what_finished_even_if_finishing_failed() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let lock = SessionDir::new(SessionId::new(1), fs.clone(), &session())
        .lock()
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    // The first window's journal finishes; the second's is still open.
    let audio: Vec<i16> = (0..1_500).map(|i| (i % 1_000) as i16).collect();
    writer.append(MIC, &audio).unwrap();
    let first = fs
        .paths()
        .into_iter()
        .filter(|p| p.starts_with(session()))
        .min()
        .unwrap();
    assert!(first.ends_with("journal-000000"), "{first:?}");
    let publisher = publisher(&lock, FakeStore::new(&fs, &db()));
    fs.crash_after(0);
    let mut ran = false;
    let stopped = publisher.finish_recording(writer, || ran = true);
    assert!(ran);
    assert!(stopped.finishing.is_some());
    let report = stopped.published.unwrap();
    assert!(report.rows().is_empty());
    let left: Vec<PathBuf> = report
        .left()
        .iter()
        .map(|j| session().join(j.id().file_name()))
        .collect();
    assert!(left.contains(&first), "{left:?}, {first:?}");
}

/// A clean end publishes every journal, the last included.
#[test]
fn finishing_a_recording_publishes_every_journal() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let lock = SessionDir::new(SessionId::new(1), fs.clone(), &session())
        .lock()
        .unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    let audio: Vec<i16> = (0..1_500).map(|i| (i % 1_000) as i16).collect();
    writer.append(MIC, &audio).unwrap();
    let publisher = publisher(&lock, FakeStore::new(&fs, &db()));
    let stopped = publisher.finish_recording(writer, || {});
    assert!(stopped.finishing.is_none());
    let report = stopped.published.unwrap();
    assert_eq!(starts(report.rows()), vec![0, 1_000]);
    assert!(report.is_complete());
    assert!(!needs_salvage(lock.session()).unwrap());
}
