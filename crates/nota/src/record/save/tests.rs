use std::sync::Mutex;

use nota_core::recorder::Mark;
use nota_core::{FakeClock, SessionTime, TrackId};

use super::*;

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

fn text(at: u64) -> ToSave {
    ToSave::Heard(Utterance::new(TrackId::new(0), ms(at), ms(at + 1), format!("t{at}")).unwrap())
}

fn mark(at: u64) -> ToSave {
    ToSave::Annotation(Annotation::Mark(Mark { at: ms(at) }))
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(FakeClock::new(ms(7)))
}

/// A store that keeps what it's given, and fails while `failing` is set.
#[derive(Clone, Default)]
struct Fake {
    stored: Arc<Mutex<Vec<ToSave>>>,
    failing: Arc<Mutex<bool>>,
    tries: Arc<Mutex<usize>>,
}

impl Fake {
    fn write(&self) -> impl FnMut(&ToSave) -> Result<(), StoreError> + Send + 'static {
        let fake = self.clone();
        move |item| {
            *fake.tries.lock().unwrap() += 1;
            if *fake.failing.lock().unwrap() {
                return Err(StoreError::Corrupt("the disk is gone".to_owned()));
            }
            fake.stored.lock().unwrap().push(item.clone());
            Ok(())
        }
    }

    fn fail(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
    }

    fn stored(&self) -> Vec<ToSave> {
        self.stored.lock().unwrap().clone()
    }

    fn tries(&self) -> usize {
        *self.tries.lock().unwrap()
    }
}

/// The warnings sent to the screen, as (raised?, at).
fn warnings(ui: &Receiver<Event>) -> Vec<WarningState> {
    ui.try_iter()
        .map(|e| match e {
            Event::Recorder(RecorderEvent::Warning(w)) => {
                assert_eq!(w.cause, Cause::LibraryUnavailable);
                assert_eq!(w.track, None);
                assert_eq!(w.at, ms(7));
                w.state
            }
            other => panic!("{other:?}"),
        })
        .collect()
}

/// Waits until `done`, for up to 5 s.
fn eventually(mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        let (_keep, never) = mpsc::channel::<()>();
        let _ = never.recv_timeout(Duration::from_millis(10));
    }
    assert!(done(), "timed out");
}

#[test]
fn everything_given_is_stored_in_order() {
    let fake = Fake::default();
    let (ui, screen) = mpsc::channel();
    let saver = Saver::spawn(fake.write(), ui, clock()).unwrap();
    let given = [text(1), mark(2), text(3)];
    let sender = saver.sender();
    for item in &given {
        sender.send(item.clone()).unwrap();
    }
    drop(sender);
    let report = saver.finish();
    assert_eq!(fake.stored(), given);
    assert_eq!(
        report,
        Saved {
            text: 2,
            annotations: 1,
            ..Saved::default()
        }
    );
    assert!(warnings(&screen).is_empty());
}

/// A store that fails warns the screen once, keeps what it couldn't
/// store, and stores it, in order, once it can; the warning is cleared.
#[test]
fn what_fails_is_kept_tried_again_and_stored_once_it_can_be() {
    let fake = Fake::default();
    let (ui, screen) = mpsc::channel();
    let saver = Saver::spawn(fake.write(), ui, clock()).unwrap();
    let sender = saver.sender();
    sender.send(text(1)).unwrap();
    eventually(|| fake.stored().len() == 1);
    fake.fail(true);
    sender.send(mark(2)).unwrap();
    sender.send(text(3)).unwrap();
    // Tried, refused, and tried again with nothing new given.
    eventually(|| fake.tries() >= 4);
    assert_eq!(fake.stored(), [text(1)]);
    assert_eq!(warnings(&screen), [WarningState::Raised]);
    fake.fail(false);
    eventually(|| fake.stored().len() == 3);
    assert_eq!(fake.stored(), [text(1), mark(2), text(3)]);
    assert_eq!(warnings(&screen), [WarningState::Cleared]);
    drop(sender);
    let report = saver.finish();
    assert_eq!((report.text, report.annotations), (2, 1));
    assert_eq!((report.lost_text, report.lost_annotations), (0, 0));
    assert_eq!(
        report.error.as_deref(),
        Some("corrupt row: the disk is gone")
    );
}

/// A store down for good: giving never waits on it, and what was never
/// stored is counted as lost at the end, by kind.
#[test]
fn a_store_down_for_good_loses_what_it_was_given_and_says_so() {
    let fake = Fake::default();
    fake.fail(true);
    let (ui, screen) = mpsc::channel();
    let saver = Saver::spawn(fake.write(), ui, clock()).unwrap();
    let sender = saver.sender();
    for at in 0..3 {
        sender.send(text(at)).unwrap();
    }
    sender.send(mark(9)).unwrap();
    drop(sender);
    let report = saver.finish();
    assert!(fake.stored().is_empty());
    assert_eq!(
        report,
        Saved {
            lost_text: 3,
            lost_annotations: 1,
            error: Some("corrupt row: the disk is gone".to_owned()),
            ..Saved::default()
        }
    );
    assert_eq!(warnings(&screen), [WarningState::Raised]);
}

/// What waits is bounded: past it, the oldest are dropped and counted.
#[test]
fn the_oldest_are_dropped_past_the_bound() {
    let fake = Fake::default();
    fake.fail(true);
    // The first two writes wait for the test: the first while everything
    // is sent, the second (all of it taken off the channel by then) while
    // the store comes back.
    let (gate, gated) = mpsc::channel::<()>();
    let (entered, entering) = mpsc::channel::<()>();
    let gated_write = {
        let mut write = fake.write();
        let mut calls = 0;
        move |item: &ToSave| {
            calls += 1;
            if calls <= 2 {
                entered.send(()).unwrap();
                gated.recv().unwrap();
            }
            write(item)
        }
    };
    let (ui, _screen) = mpsc::channel();
    let saver = Saver::spawn(gated_write, ui, clock()).unwrap();
    let sender = saver.sender();
    sender.send(mark(0)).unwrap();
    entering.recv().unwrap();
    for at in 1..=MAX_PENDING {
        sender.send(text(u64::try_from(at).unwrap())).unwrap();
    }
    drop(sender);
    gate.send(()).unwrap();
    entering.recv().unwrap();
    fake.fail(false);
    gate.send(()).unwrap();
    let report = saver.finish();
    assert_eq!(report.lost_annotations, 1, "the oldest is the one dropped");
    assert_eq!(report.lost_text, 0);
    assert_eq!(report.text, MAX_PENDING);
}

/// A store that fails slowly (a lock held elsewhere waits out the busy
/// timeout) is tried once for everything waiting, not once per item, so
/// the stop doesn't wait on it item by item.
#[test]
fn a_slow_failing_store_is_tried_once_for_everything_waiting() {
    let fake = Fake::default();
    fake.fail(true);
    let slow = {
        let mut write = fake.write();
        move |item: &ToSave| {
            let (_keep, never) = mpsc::channel::<()>();
            let _ = never.recv_timeout(Duration::from_millis(20));
            write(item)
        }
    };
    let (ui, _screen) = mpsc::channel();
    let saver = Saver::spawn(slow, ui, clock()).unwrap();
    let sender = saver.sender();
    for at in 0..500 {
        sender.send(text(at)).unwrap();
    }
    drop(sender);
    let report = saver.finish();
    assert_eq!(report.lost_text, 500);
    assert!(fake.tries() <= 4, "{} tries", fake.tries());
}
