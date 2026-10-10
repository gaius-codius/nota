use nota_core::recorder::{Disk, Level};
use nota_core::{SampleIndex, SampleRate, TrackTimeline};

use super::*;
use crate::test_dir::TestDir;
use crate::tests::new_session;

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);
const MIC: TrackId = TrackId::new(0);

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

fn event(at: u64, track: Option<TrackId>, happened: Happened) -> TimelineEvent {
    TimelineEvent {
        at: ms(at),
        track,
        happened,
    }
}

/// One event of every kind, each with the detail it carries.
fn every_kind() -> Vec<TimelineEvent> {
    let causes = [
        Cause::Stalled,
        Cause::DigitalZeros,
        Cause::Quiet,
        Cause::Drift,
        Cause::StreamFailed("the device went away".to_owned()),
        Cause::JournalFailed("no space left".to_owned()),
        Cause::DiskLow,
        Cause::DiskFull,
        Cause::SleepNotHeld,
        Cause::Slept,
        Cause::LibraryUnavailable,
    ];
    let mut events: Vec<TimelineEvent> = causes
        .iter()
        .flat_map(|cause| {
            [
                Happened::Raised(cause.clone()),
                Happened::Cleared(cause.clone()),
            ]
        })
        .map(|happened| event(10, Some(MIC), happened))
        .collect();
    for change in [
        DeviceChange::Lost,
        DeviceChange::Changed("Headphones".to_owned()),
        DeviceChange::Format,
        DeviceChange::PermissionDenied,
    ] {
        events.push(event(20, Some(MIC), Happened::Device(change)));
    }
    events.push(event(30, None, Happened::Engine(EngineState::Online)));
    events.push(event(
        30,
        None,
        Happened::Engine(EngineState::Offline("it exited".to_owned())),
    ));
    events.push(event(40, Some(MIC), Happened::Gap { until: ms(92_000) }));
    events
}

/// Every kind of event reads back as it was stored, with its track,
/// time and detail; another session's never show.
#[test]
fn every_kind_of_event_reads_back_as_stored() {
    let dir = TestDir::new("timeline");
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    store.create_session(&new_session(S2)).unwrap();
    let events = every_kind();
    for e in &events {
        store.add_event(S1, e).unwrap();
    }
    let other = event(5, None, Happened::Raised(Cause::DiskLow));
    store.add_event(S2, &other).unwrap();
    assert_eq!(store.timeline(S1).unwrap(), events);
    assert_eq!(store.timeline(S2).unwrap(), [other]);
}

/// The timeline reads in session order, whatever order the events were
/// added in; at one moment, in the order they were added.
#[test]
fn the_timeline_reads_in_session_order() {
    let dir = TestDir::new("timeline-order");
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    let late = event(9_000, Some(MIC), Happened::Raised(Cause::Quiet));
    let first = event(1_500, None, Happened::Raised(Cause::DiskLow));
    let second = event(1_500, Some(MIC), Happened::Device(DeviceChange::Lost));
    let early = event(400, None, Happened::Engine(EngineState::Online));
    for e in [&late, &first, &second, &early] {
        store.add_event(S1, e).unwrap();
    }
    assert_eq!(store.timeline(S1).unwrap(), [early, first, second, late]);
}

/// An event for a session not in the library, or at a time SQLite can't
/// hold, is refused and stores nothing.
#[test]
fn an_event_that_cannot_be_stored_is_refused() {
    let dir = TestDir::new("timeline-refused");
    let mut store = Store::open(&dir.db()).unwrap();
    let missing = SessionId::new(4);
    let quiet = event(1, Some(MIC), Happened::Raised(Cause::Quiet));
    assert!(matches!(
        store.add_event(missing, &quiet),
        Err(StoreError::NoSession(id)) if id == missing
    ));
    store.create_session(&new_session(S1)).unwrap();
    let far = TimelineEvent {
        at: SessionTime::from_nanos(u64::MAX),
        ..quiet.clone()
    };
    assert!(matches!(
        store.add_event(S1, &far),
        Err(StoreError::OutOfRange)
    ));
    let far_gap = event(
        1,
        Some(MIC),
        Happened::Gap {
            until: SessionTime::from_nanos(u64::MAX),
        },
    );
    assert!(matches!(
        store.add_event(S1, &far_gap),
        Err(StoreError::OutOfRange)
    ));
    assert!(store.timeline(S1).unwrap().is_empty());
}

/// Rows that can't be a timeline event (an unknown kind or cause, a kind
/// without the detail it needs, a bad gap end, a negative time, a track
/// out of range) are reported, not misread.
#[test]
fn a_bad_stored_event_is_corrupt() {
    for (at, track, kind, detail) in [
        (5, "NULL", "'raised:sneezed'", "NULL"),
        (5, "NULL", "'unplugged'", "NULL"),
        (5, "0", "'raised:stream-failed'", "NULL"),
        (5, "0", "'device:changed'", "NULL"),
        (5, "NULL", "'engine:offline'", "NULL"),
        (5, "0", "'gap'", "NULL"),
        (5, "0", "'gap'", "'soon'"),
        (5, "0", "'gap'", "'-4'"),
        (-1, "NULL", "'raised:quiet'", "NULL"),
    ] {
        let dir = TestDir::new("timeline-corrupt");
        let mut store = Store::open(&dir.db()).unwrap();
        store.create_session(&new_session(S1)).unwrap();
        // The schema's checks refuse a negative time; only a damaged
        // database or an outside edit makes one, so they're set aside here.
        store
            .conn
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "INSERT INTO event (session_id, track, at_ns, kind, detail) \
                     VALUES (1, {track}, {at}, {kind}, {detail})"
                ),
                [],
            )
            .unwrap();
        let read = store.timeline(S1);
        assert!(
            matches!(read, Err(StoreError::Corrupt(_))),
            "{kind} {detail}: {read:?}"
        );
    }
}

/// Warnings, device changes, the transcriber's state and gaps are the
/// timeline's, each at its own time and track; the transcriber's state,
/// which carries no time, is at the time given.
#[test]
fn changes_are_picked_out_of_the_screen_s_events() {
    let warning = Event::Warning(Warning {
        cause: Cause::Stalled,
        track: Some(MIC),
        at: ms(700),
        state: WarningState::Cleared,
    });
    assert_eq!(
        TimelineEvent::of(&warning, ms(9_999)),
        Some(event(700, Some(MIC), Happened::Cleared(Cause::Stalled)))
    );
    let raised = Event::Warning(Warning {
        cause: Cause::DiskLow,
        track: None,
        at: ms(800),
        state: WarningState::Raised,
    });
    assert_eq!(
        TimelineEvent::of(&raised, ms(9_999)),
        Some(event(800, None, Happened::Raised(Cause::DiskLow)))
    );
    let device = Event::Device {
        track: MIC,
        change: DeviceChange::Lost,
        at: ms(900),
    };
    assert_eq!(
        TimelineEvent::of(&device, ms(9_999)),
        Some(event(900, Some(MIC), Happened::Device(DeviceChange::Lost)))
    );
    let engine = Event::Engine(EngineState::Offline("it exited".to_owned()));
    assert_eq!(
        TimelineEvent::of(&engine, ms(9_999)),
        Some(event(
            9_999,
            None,
            Happened::Engine(EngineState::Offline("it exited".to_owned()))
        ))
    );
    let mut timeline = TrackTimeline::new(MIC);
    let rate = SampleRate::new(1_000).unwrap();
    timeline.open_epoch(ms(0), SampleIndex::ZERO, rate).unwrap();
    timeline
        .open_epoch(ms(5_000), SampleIndex::new(1_000), rate)
        .unwrap();
    let gap = timeline.gaps().next().unwrap();
    assert_eq!(
        TimelineEvent::of(&Event::Gap { track: MIC, gap }, ms(9_999)),
        Some(event(1_000, Some(MIC), Happened::Gap { until: ms(5_000) }))
    );
}

/// What isn't a change has no place on the timeline.
#[test]
fn levels_text_and_progress_are_not_on_the_timeline() {
    let now = ms(1);
    for event in [
        Event::Level {
            track: MIC,
            at: now,
            level: Level::SILENT,
        },
        Event::Recorded(12),
        Event::Transcribing(true),
        Event::Disk(Disk {
            free_bytes: 1,
            left: None,
        }),
        Event::Durable {
            track: MIC,
            up_to: now,
        },
        Event::Stopping,
    ] {
        assert_eq!(TimelineEvent::of(&event, now), None, "{event:?}");
    }
}
