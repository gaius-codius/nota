use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use nota_core::FakeClock;

use super::*;

/// A logind that counts the locks it has given and how many are still
/// held, or refuses to give any.
struct FakeLogind {
    refuses: bool,
    given: AtomicUsize,
    held: Arc<AtomicUsize>,
}

impl FakeLogind {
    fn new(refuses: bool) -> Self {
        Self {
            refuses,
            given: AtomicUsize::new(0),
            held: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn given(&self) -> usize {
        self.given.load(Ordering::SeqCst)
    }

    fn held(&self) -> usize {
        self.held.load(Ordering::SeqCst)
    }
}

/// What a [`FakeLogind`] lock holds: dropping it gives the lock back.
struct Returned(Arc<AtomicUsize>);

impl Drop for Returned {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Logind for FakeLogind {
    fn inhibit_sleep(&self) -> Result<SleepLock, SleepNotHeld> {
        if self.refuses {
            return Err(SleepNotHeld::new("access denied"));
        }
        self.given.fetch_add(1, Ordering::SeqCst);
        self.held.fetch_add(1, Ordering::SeqCst);
        Ok(SleepLock::new(Returned(Arc::clone(&self.held))))
    }
}

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

/// A stretch with no audio from `from` to `to` milliseconds.
fn quiet(from: u64, to: u64) -> Unrecorded {
    Unrecorded {
        from: ms(from),
        to: ms(to),
    }
}

/// A sleep that ended with audio at `resumed` ms, after `unrecorded`.
fn slept(resumed: u64, unrecorded: Option<Unrecorded>) -> Slept {
    Slept {
        resumed: ms(resumed),
        unrecorded,
    }
}

/// The lock is held from the moment it's taken until it's dropped, and
/// logind is asked once.
#[test]
fn the_lock_is_held_until_it_is_dropped() {
    let logind = FakeLogind::new(false);
    let clock = FakeClock::new(SessionTime::ZERO);
    let (ui, screen) = mpsc::channel();
    let mut notes = Vec::new();
    assert_eq!(logind.held(), 0);
    let sleep = hold(&logind);
    // The lock is taken now, before anything else is said of it.
    assert_eq!((logind.given(), logind.held()), (1, 1));
    sleep.report(&clock, &ui, &mut notes);
    drop(sleep);
    assert_eq!((logind.given(), logind.held()), (1, 0));
    // A lock given says nothing to the screen or the summary.
    assert_eq!(screen.try_iter().count(), 0);
    assert_eq!(notes, Vec::<String>::new());
}

/// A refusal is a warning and a note, said when the recording asks for
/// them and not before; nothing is held, and nothing stops.
#[test]
fn a_refusal_warns_and_holds_nothing() {
    let logind = FakeLogind::new(true);
    let clock = FakeClock::new(ms(1_500));
    let (ui, screen) = mpsc::channel();
    let mut notes = Vec::new();
    let sleep = hold(&logind);
    assert_eq!(logind.held(), 0);
    // Nothing is said until the recording has started.
    assert_eq!(screen.try_iter().count(), 0);
    assert_eq!(notes, Vec::<String>::new());
    sleep.report(&clock, &ui, &mut notes);
    assert_eq!(
        screen.try_iter().collect::<Vec<_>>(),
        [Event::Recorder(recorder::Event::Warning(Warning {
            cause: Cause::SleepNotHeld,
            track: None,
            at: ms(1_500),
            state: WarningState::Raised,
        }))]
    );
    assert_eq!(
        notes,
        [
            "sleep couldn't be held off: access denied; if the machine sleeps, \
          the recording has a gap there"
        ]
    );
}

/// A screen that has closed doesn't stop the recording from starting.
#[test]
fn a_refusal_with_the_screen_closed_still_notes() {
    let logind = FakeLogind::new(true);
    let clock = FakeClock::new(SessionTime::ZERO);
    let (ui, screen) = mpsc::channel();
    drop(screen);
    let mut notes = Vec::new();
    hold(&logind).report(&clock, &ui, &mut notes);
    assert_eq!(notes.len(), 1);
}

/// D-Bus's error name is kept with its message, whichever is missing.
#[cfg(target_os = "linux")]
#[test]
fn a_dbus_error_keeps_its_name_and_message() {
    let name = Some("org.freedesktop.DBus.Error.AccessDenied");
    assert_eq!(
        dbus_reason(name, Some("not allowed")),
        "org.freedesktop.DBus.Error.AccessDenied: not allowed"
    );
    assert_eq!(
        dbus_reason(name, None),
        "org.freedesktop.DBus.Error.AccessDenied"
    );
    assert_eq!(dbus_reason(None, Some("not allowed")), "not allowed");
    assert_eq!(dbus_reason(None, None), "no reason given");
}

/// A sleep's warning is raised at the resume, about no track in
/// particular.
#[test]
fn a_sleep_warns_at_the_resume() {
    let slept = slept(5_000, Some(quiet(1_000, 5_000)));
    assert_eq!(
        slept.warning(),
        recorder::Event::Warning(Warning {
            cause: Cause::Slept,
            track: None,
            at: ms(5_000),
            state: WarningState::Raised,
        })
    );
}

/// The summary says when the machine slept, from the end of the audio
/// before it, and for how long.
#[test]
fn the_note_for_a_sleep_with_a_gap_says_when_and_how_long() {
    let slept = slept(4_452_000, Some(quiet(4_360_000, 4_452_000)));
    assert_eq!(
        slept.note(),
        "the machine slept at 1:12:40 for 1m 32s; nothing was recorded then"
    );
}

/// Without a gap (drift took the stretch, or a new epoch was refused), the
/// note can only say the machine slept before the resume.
#[test]
fn the_note_for_a_sleep_without_a_gap_says_where_audio_resumed() {
    let slept = slept(61_000, None);
    assert_eq!(
        slept.note(),
        "the machine slept before 0:01:01; the recording may have a gap there"
    );
}

/// Lengths show their two largest units.
#[test]
fn lengths_show_their_two_largest_units() {
    let secs = Duration::from_secs;
    assert_eq!(lasted(Duration::from_millis(999)), "less than a second");
    assert_eq!(lasted(secs(1)), "1s");
    assert_eq!(lasted(secs(59)), "59s");
    assert_eq!(lasted(secs(60)), "1m 0s");
    assert_eq!(lasted(secs(92)), "1m 32s");
    assert_eq!(lasted(secs(3_599)), "59m 59s");
    assert_eq!(lasted(secs(3_600)), "1h 0m");
    assert_eq!(lasted(secs(7_500 + 59)), "2h 5m");
}

/// Tracks whose stretches overlap woke from one sleep; ones that only touch
/// or are apart didn't.
#[test]
fn sleeps_with_overlapping_stretches_are_the_same_sleep() {
    let first = slept(5_000, Some(quiet(1_000, 5_000)));
    assert!(first.same_sleep_as(&slept(9_000, Some(quiet(1_000, 9_000)))));
    assert!(first.same_sleep_as(&slept(5_040, Some(quiet(1_040, 5_040)))));
    // Touching isn't overlapping: the second began as the first ended.
    assert!(!first.same_sleep_as(&slept(9_000, Some(quiet(5_000, 9_000)))));
    assert!(!first.same_sleep_as(&slept(9_000, Some(quiet(6_000, 9_000)))));
}

/// A track with no stretch to compare is the same sleep if it woke in the
/// other's stretch or within two seconds after it, whichever came first.
#[test]
fn a_sleep_with_no_stretch_is_judged_by_when_it_woke() {
    let known = slept(5_000, Some(quiet(1_000, 5_000)));
    assert!(known.same_sleep_as(&slept(5_500, None)));
    assert!(known.same_sleep_as(&slept(7_000, None)));
    assert!(!known.same_sleep_as(&slept(7_001, None)));
    assert!(!known.same_sleep_as(&slept(999, None)));
    // The same from the other side.
    assert!(slept(7_000, None).same_sleep_as(&known));
    assert!(!slept(7_001, None).same_sleep_as(&known));
    // Neither has one: two seconds apart, in either order.
    assert!(slept(1_000, None).same_sleep_as(&slept(3_000, None)));
    assert!(slept(3_000, None).same_sleep_as(&slept(1_000, None)));
    assert!(!slept(1_000, None).same_sleep_as(&slept(3_001, None)));
    assert!(!slept(3_001, None).same_sleep_as(&slept(1_000, None)));
}

/// Merging the sleep one track saw with another's keeps what no track
/// recorded: the stretch they have in common, from the first to wake.
#[test]
fn merging_sleeps_keeps_the_stretch_no_track_recorded() {
    let mic = slept(5_000, Some(quiet(1_000, 5_000)));
    let system = slept(9_000, Some(quiet(1_000, 9_000)));
    let both = slept(5_000, Some(quiet(1_000, 5_000)));
    assert_eq!(mic.merged_with(&system), both);
    assert_eq!(system.merged_with(&mic), both);
    // With a stretch on one side only, that's the one kept.
    let unknown = slept(5_500, None);
    assert_eq!(mic.merged_with(&unknown), mic);
    assert_eq!(unknown.merged_with(&mic), mic);
}
