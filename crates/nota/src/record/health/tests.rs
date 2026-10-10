use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};

use nota_core::{FakeClock, SessionTime};
use nota_store::{Happened, StoreError, TimelineEvent};

use nota_recorder::segment::FakeStore;

use super::*;

/// What `note` warned of, once per call.
fn noting(health: &LibraryHealth, writer: Writer, failed: bool) -> Option<WarningState> {
    let mut warned = None;
    health.note(writer, failed, |state| warned = Some(state));
    warned
}

/// One warning however many writers fail: raised when the first fails,
/// cleared only once neither is.
#[test]
fn the_library_warns_once_whichever_writers_fail() {
    let health = LibraryHealth::default();
    assert_eq!(noting(&health, Writer::Saver, false), None);
    assert_eq!(
        noting(&health, Writer::Saver, true),
        Some(WarningState::Raised)
    );
    // The publisher failing too, and the saver getting through, change
    // nothing: the publisher still fails.
    assert_eq!(noting(&health, Writer::Publisher, true), None);
    assert_eq!(noting(&health, Writer::Saver, false), None);
    assert_eq!(
        noting(&health, Writer::Publisher, false),
        Some(WarningState::Cleared)
    );
    // The publisher alone raises it as well.
    assert_eq!(
        noting(&health, Writer::Publisher, true),
        Some(WarningState::Raised)
    );
}

/// Why the test store failed: down, or a damaged row it read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Down {
    No,
    Unreachable,
    DamagedRow,
}

/// A segment store whose rows fail as `down` says.
struct Flaky {
    down: Down,
}

/// The test store's error: a store error, so a damaged row reads as one.
#[derive(Debug)]
struct FlakyError(StoreError);

impl fmt::Display for FlakyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for FlakyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl SegmentStore for Flaky {
    type Error = FlakyError;

    fn rows(&mut self, _: SessionId) -> Result<Vec<SegmentRow>, Self::Error> {
        match self.down {
            Down::No => Ok(Vec::new()),
            Down::Unreachable => Err(FlakyError(StoreError::Corrupt("gone".to_owned()))),
            Down::DamagedRow => Err(FlakyError(StoreError::CorruptRow {
                session: SessionId::new(1),
                key: RowKey { track: 0, start: 0 },
                why: "a negative range".to_owned(),
            })),
        }
    }

    fn insert(&mut self, _: SessionId, _: &DurableSegment) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A reporting store over [`Flaky`], with what it sends to the screen and
/// the saver.
fn reporting() -> (ReportingStore<Flaky>, Receiver<Event>, Receiver<ToSave>) {
    let (ui, screen) = mpsc::channel();
    let (save, saved) = mpsc::channel();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::from_nanos(9)));
    let store = ReportingStore::new(
        Flaky { down: Down::No },
        LibraryHealth::default(),
        ui,
        save,
        clock,
    );
    (store, screen, saved)
}

/// The library warning's states sent to the screen so far.
fn warned(screen: &Receiver<Event>) -> Vec<WarningState> {
    screen
        .try_iter()
        .map(|event| match event {
            Event::Recorder(RecorderEvent::Warning(w)) if w.cause == Cause::LibraryUnavailable => {
                w.state
            }
            other => panic!("{other:?}"),
        })
        .collect()
}

/// The timeline entries handed to the saver so far.
fn kept(saved: &Receiver<ToSave>) -> Vec<Happened> {
    saved
        .try_iter()
        .map(|item| match item {
            ToSave::Event(TimelineEvent { happened, .. }) => happened,
            other => panic!("{other:?}"),
        })
        .collect()
}

/// A segment-row read that fails raises the warning, and one that works
/// doesn't clear it: only a committed row says writes go through.
#[test]
fn a_read_raises_the_warning_but_never_clears_it() {
    let (mut store, screen, saved) = reporting();
    let session = SessionId::new(1);
    assert!(store.rows(session).is_ok());
    store.inner.down = Down::Unreachable;
    assert!(store.rows(session).is_err());
    assert!(store.rows(session).is_err());
    store.inner.down = Down::No;
    assert!(store.rows(session).is_ok());
    assert_eq!(warned(&screen), [WarningState::Raised]);
    assert_eq!(kept(&saved), [Happened::Raised(Cause::LibraryUnavailable)]);
}

/// A segment store whose inserts fail while `failing` is set, counting
/// the inserts tried; reads always work.
struct Gate {
    inner: FakeStore,
    failing: Arc<AtomicBool>,
    inserts: Arc<AtomicUsize>,
}

impl SegmentStore for Gate {
    type Error = std::io::Error;

    fn rows(&mut self, session: SessionId) -> Result<Vec<SegmentRow>, Self::Error> {
        self.inner.rows(session)
    }

    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), Self::Error> {
        self.inserts.fetch_add(1, Ordering::SeqCst);
        if self.failing.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("the database is locked"));
        }
        self.inner.insert(session, segment)
    }
}

/// Waits until `done`, for up to 10 s.
fn eventually(mut done: impl FnMut() -> bool) {
    for _ in 0..1_000 {
        if done() {
            return;
        }
        let (_keep, never) = mpsc::channel::<()>();
        let _ = never.recv_timeout(std::time::Duration::from_millis(10));
    }
    assert!(done(), "timed out");
}

/// Through the real publisher, with only segment rows failing: the first
/// failed insert raises the warning, later publish runs that still fail
/// (whose reads work) leave it up, and the first row committed clears it,
/// on the screen and for the timeline, once each.
#[test]
fn only_a_committed_row_clears_the_publisher_s_warning() {
    use nota_core::{SampleCount, SampleRate, TrackId};
    use nota_recorder::fs::fake::FakeFs;
    use nota_recorder::segment::{Publisher, SegmentLength};
    use nota_recorder::session::{SessionDir, SessionStore, SessionWriter};

    let dir = std::path::PathBuf::from("/session");
    let db = std::path::PathBuf::from("/db");
    let fs = FakeFs::with_dirs([dir.clone(), db.clone()]);
    let lock = SessionDir::new(SessionId::new(1), fs.clone(), &dir)
        .lock()
        .unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let mut writer = SessionWriter::open(
        &lock,
        SampleRate::new(1_000).unwrap(),
        length,
        Arc::clone(&clock),
    )
    .unwrap();
    // Three tracks, so three journals to publish in three runs.
    for track in (0..3).map(TrackId::new) {
        let (_, epoch) = writer.open_first_epoch(track, SessionTime::ZERO).unwrap();
        writer.start_track(track, &epoch).unwrap();
        writer.append(track, &[7; 500]).unwrap();
    }
    let journals = writer.finish().unwrap();
    let failing = Arc::new(AtomicBool::new(true));
    let inserts = Arc::new(AtomicUsize::new(0));
    let gate = Gate {
        inner: FakeStore::new(&fs, &db),
        failing: Arc::clone(&failing),
        inserts: Arc::clone(&inserts),
    };
    let (ui, screen) = mpsc::channel();
    let (save, saved) = mpsc::channel();
    let store = ReportingStore::new(gate, LibraryHealth::default(), ui, save, clock);
    let publisher = Publisher::spawn(SessionStore::new(lock, store), length).unwrap();
    let queue = publisher.queue();
    let mut batches = journals.into_iter().map(|journal| vec![journal]);
    // Two runs that fail: the second reads its rows, then fails again.
    for tried in [1, 2] {
        assert!(queue.send(batches.next().unwrap()));
        eventually(|| inserts.load(Ordering::SeqCst) >= tried);
    }
    failing.store(false, Ordering::SeqCst);
    assert!(queue.send(batches.next().unwrap()));
    drop(queue);
    let report = publisher.finish().unwrap();
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(
        warned(&screen),
        [WarningState::Raised, WarningState::Cleared]
    );
    assert_eq!(
        kept(&saved),
        [
            Happened::Raised(Cause::LibraryUnavailable),
            Happened::Cleared(Cause::LibraryUnavailable)
        ]
    );
}

/// A damaged row came from a database that answered: publishing names it,
/// and the library isn't said to be down.
#[test]
fn a_damaged_row_is_not_the_library_down() {
    let (mut store, screen, saved) = reporting();
    store.inner.down = Down::DamagedRow;
    let error = store.rows(SessionId::new(1)).unwrap_err();
    assert!(ReportingStore::<Flaky>::unparsable_row(&error).is_some());
    assert_eq!(warned(&screen), []);
    assert_eq!(kept(&saved), []);
}
