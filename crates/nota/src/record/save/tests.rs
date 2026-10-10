use std::sync::Mutex;

use nota_core::recorder::{DeviceChange, Mark};
use nota_core::{FakeClock, SessionTime, TrackId};
use nota_store::Happened;

use super::*;
use crate::record::health::LibraryHealth;

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

fn text(at: u64) -> ToSave {
    let heard = Utterance::new(TrackId::new(0), ms(at), ms(at + 1), format!("t{at}")).unwrap();
    ToSave::Heard(heard, Vec::new())
}

fn mark(at: u64) -> ToSave {
    ToSave::Annotation(Annotation::Mark(Mark { at: ms(at) }))
}

/// A device lost on track 1, at `at` ms, as the timeline keeps it.
fn event(at: u64) -> ToSave {
    ToSave::Event(TimelineEvent {
        at: ms(at),
        track: Some(TrackId::new(1)),
        happened: Happened::Device(DeviceChange::Lost),
    })
}

/// The timeline entry the saver makes of its own warning, at the clock's
/// 7 ms.
fn offline(state: WarningState) -> ToSave {
    let cause = Cause::LibraryUnavailable;
    ToSave::Event(TimelineEvent {
        at: ms(7),
        track: None,
        happened: match state {
            WarningState::Raised => Happened::Raised(cause),
            WarningState::Cleared => Happened::Cleared(cause),
        },
    })
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

/// The state of a warning sent to the screen, which must be the saver's.
fn state_of(event: Event) -> WarningState {
    match event {
        Event::Recorder(RecorderEvent::Warning(w)) => {
            assert_eq!(w.cause, Cause::LibraryUnavailable);
            assert_eq!(w.track, None);
            assert_eq!(w.at, ms(7));
            w.state
        }
        other => panic!("{other:?}"),
    }
}

/// The warnings sent to the screen so far.
fn warnings(ui: &Receiver<Event>) -> Vec<WarningState> {
    ui.try_iter().map(state_of).collect()
}

/// The next warning sent to the screen, waiting up to 5 s for it.
fn next_warning(ui: &Receiver<Event>) -> WarningState {
    state_of(ui.recv_timeout(Duration::from_secs(5)).unwrap())
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

/// Text, marks and timeline events are each stored, counted by kind, in
/// the order given.
#[test]
fn everything_given_is_stored_in_order() {
    let fake = Fake::default();
    let (ui, screen) = mpsc::channel();
    let saver = Saver::spawn(fake.write(), ui, clock(), LibraryHealth::default()).unwrap();
    let given = [text(1), mark(2), event(3), text(4)];
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
            events: 1,
            ..Saved::default()
        }
    );
    assert!(warnings(&screen).is_empty());
}

/// A store that fails warns the screen once, keeps what it couldn't
/// store, and stores it, in order, once it can; the warning is cleared.
/// The warning is stored on the timeline too, behind what was waiting when
/// it was raised, and its clear behind what was waiting when the store
/// came back.
#[test]
fn what_fails_is_kept_tried_again_and_stored_once_it_can_be() {
    let fake = Fake::default();
    let (ui, screen) = mpsc::channel();
    let saver = Saver::spawn(fake.write(), ui, clock(), LibraryHealth::default()).unwrap();
    let sender = saver.sender();
    sender.send(text(1)).unwrap();
    eventually(|| fake.stored().len() == 1);
    fake.fail(true);
    sender.send(mark(2)).unwrap();
    // Text 3 is given once the raise is queued, so the order stored is
    // the same on every run.
    assert_eq!(next_warning(&screen), WarningState::Raised);
    sender.send(text(3)).unwrap();
    // Tried, refused, and tried again with nothing new given.
    eventually(|| fake.tries() >= 4);
    assert_eq!(fake.stored(), [text(1)]);
    fake.fail(false);
    eventually(|| fake.stored().len() == 5);
    assert_eq!(
        fake.stored(),
        [
            text(1),
            mark(2),
            offline(WarningState::Raised),
            text(3),
            offline(WarningState::Cleared)
        ]
    );
    assert_eq!(warnings(&screen), [WarningState::Cleared]);
    drop(sender);
    let report = saver.finish();
    assert_eq!((report.text, report.annotations, report.events), (2, 1, 2));
    assert_eq!(
        (
            report.lost_text,
            report.lost_annotations,
            report.lost_events
        ),
        (0, 0, 0)
    );
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
    let saver = Saver::spawn(fake.write(), ui, clock(), LibraryHealth::default()).unwrap();
    let sender = saver.sender();
    for at in 0..3 {
        sender.send(text(at)).unwrap();
    }
    sender.send(mark(9)).unwrap();
    sender.send(event(8)).unwrap();
    drop(sender);
    let report = saver.finish();
    assert!(fake.stored().is_empty());
    // The events lost are the one given and the raise the saver queued.
    assert_eq!(
        report,
        Saved {
            lost_text: 3,
            lost_annotations: 1,
            lost_events: 2,
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
    let saver = Saver::spawn(gated_write, ui, clock(), LibraryHealth::default()).unwrap();
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
    let saver = Saver::spawn(slow, ui, clock(), LibraryHealth::default()).unwrap();
    let sender = saver.sender();
    for at in 0..500 {
        sender.send(text(at)).unwrap();
    }
    drop(sender);
    let report = saver.finish();
    assert_eq!(report.lost_text, 500);
    assert!(fake.tries() <= 4, "{} tries", fake.tries());
}

/// A store that fails every other write can't grow what waits: a raise
/// that comes while its own clear still waits takes the clear back, so
/// the queue never holds more than what was given and one warning.
#[test]
fn a_store_that_fails_every_other_write_does_not_grow_the_queue() {
    let (ui, _screen) = mpsc::channel();
    let mut writes = 0_u32;
    let mut saving = Saving {
        // Fails the first write, then every other one.
        write: move |_: &ToSave| {
            writes += 1;
            if writes % 2 == 1 {
                Err(StoreError::Corrupt("busy".to_owned()))
            } else {
                Ok(())
            }
        },
        ui,
        clock: clock(),
        health: LibraryHealth::default(),
        pending: VecDeque::new(),
        failing: false,
        saved: Saved::default(),
    };
    for at in 1..=5 {
        saving.queue(text(at));
    }
    let mut longest = 0;
    for _ in 0..20 {
        saving.write_pending();
        longest = longest.max(saving.pending.len());
    }
    // The five given and the raise, at most.
    assert_eq!(longest, 6);
    assert_eq!(saving.saved.text, 5);
}

/// While the publisher's rows fail, the saver's own failure and recovery
/// say nothing: the library's one warning is already up, and stays up
/// until the publisher gets through too.
#[test]
fn the_saver_getting_through_leaves_the_publisher_s_warning_up() {
    use crate::record::health::Writer;

    let health = LibraryHealth::default();
    health.note(Writer::Publisher, true, drop);
    let fake = Fake::default();
    let (ui, screen) = mpsc::channel();
    let saver = Saver::spawn(fake.write(), ui, clock(), health.clone()).unwrap();
    let sender = saver.sender();
    fake.fail(true);
    sender.send(text(1)).unwrap();
    // Tried and refused, then tried again once it works.
    eventually(|| fake.tries() >= 1);
    fake.fail(false);
    eventually(|| fake.stored().len() == 1);
    drop(sender);
    let report = saver.finish();
    assert_eq!(warnings(&screen), []);
    assert_eq!(fake.stored(), [text(1)]);
    assert_eq!(report.events, 0);
    // The publisher getting through is what clears it.
    let mut cleared = None;
    health.note(Writer::Publisher, false, |state| cleared = Some(state));
    assert_eq!(cleared, Some(WarningState::Cleared));
}
