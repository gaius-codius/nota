use std::fmt;
use std::sync::mpsc::{self, Receiver};

use nota_core::{FakeClock, SessionTime};
use nota_store::{Happened, StoreError, TimelineEvent};

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

/// The publisher's rows failing raises the warning, on the screen and the
/// timeline, once however many fail; a call that goes through clears it.
/// The result is passed on unchanged, so publishing tries again as before.
#[test]
fn the_publisher_s_failures_raise_and_clear_the_warning() {
    let (mut store, screen, saved) = reporting();
    let session = SessionId::new(1);
    assert!(store.rows(session).is_ok());
    assert_eq!(warned(&screen), []);
    store.inner.down = Down::Unreachable;
    assert!(store.rows(session).is_err());
    assert!(store.rows(session).is_err());
    assert_eq!(warned(&screen), [WarningState::Raised]);
    store.inner.down = Down::No;
    assert!(store.rows(session).is_ok());
    assert_eq!(warned(&screen), [WarningState::Cleared]);
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
