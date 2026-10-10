use nota_core::recorder::{Cause, Warning, WarningState};
use nota_core::{SampleIndex, SampleRange, SampleRate};
use nota_recorder::capture::CaptureNotice;

use super::*;

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

/// A millisecond a sample.
fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

fn opened(track: TrackId, at_ms: u64) -> TrackTimeline {
    let mut t = TrackTimeline::new(track);
    t.open_epoch(ms(at_ms), SampleIndex::ZERO, rate()).unwrap();
    t
}

fn chunk(track: TrackId, from: u64, samples: Vec<i16>) -> AudioChunk {
    AudioChunk::new(track, SampleIndex::new(from), rate(), samples).unwrap()
}

fn heard(track: TrackId, start: u64, end: u64, text: &str) -> EngineEvent {
    let range = SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap();
    EngineEvent::Transcript(Transcript::new(track, range, text.to_owned()).unwrap())
}

fn texts(updates: &[Event]) -> Vec<(SessionTime, SessionTime, String)> {
    updates
        .iter()
        .filter_map(|u| match u {
            Event::Text(t) => Some((t.start(), t.end(), t.text().to_owned())),
            _ => None,
        })
        .collect()
}

#[test]
fn audio_goes_to_the_engine_and_levels_to_the_screen() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    let audio = chunk(MIC, 0, vec![10, -300, 20]);
    let actions = live.recorder(Some(MIC), RecorderEvent::Audio(audio.clone()));
    assert_eq!(actions.transcribe, Some(audio));
    assert_eq!(actions.flush, None);
    assert_eq!(
        actions.updates,
        [
            Event::Level {
                track: MIC,
                at: ms(3),
                level: Level::from_peak(300)
            },
            Event::Recorded(6)
        ]
    );
}

#[test]
fn levels_are_sent_every_100_ms_with_the_peak_between() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    let mut levels = Vec::new();
    let mut recorded = Vec::new();
    // Twenty-one 10 ms chunks, one of them loud.
    for i in 0..21_u64 {
        let loud = if i == 4 { 9_000 } else { 100 };
        let actions = live.recorder(
            Some(MIC),
            RecorderEvent::Audio(chunk(MIC, i * 10, vec![loud; 10])),
        );
        for u in actions.updates {
            match u {
                Event::Level { at, level, .. } => levels.push((at, level.peak())),
                Event::Recorded(b) => recorded.push(b),
                other => panic!("{other:?}"),
            }
        }
    }
    assert_eq!(levels, [(ms(10), 100), (ms(110), 9_000), (ms(210), 100)]);
    assert_eq!(recorded, [20, 220, 420]);
}

#[test]
fn the_size_counts_what_was_captured_through_a_broken_journal() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    let recorded = |actions: Actions| {
        actions
            .updates
            .into_iter()
            .filter_map(|u| match u {
                Event::Recorded(bytes) => Some(bytes),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let first = live.recorder(Some(MIC), RecorderEvent::Audio(chunk(MIC, 0, vec![1; 100])));
    assert_eq!(recorded(first), [200]);
    let broken = nota_recorder::session::SessionError::Marks(std::io::ErrorKind::Other.into());
    let failed = live.recorder(Some(MIC), RecorderEvent::JournalFailed(broken));
    assert_eq!(failed, Actions::default());
    // Captured, not on disk: the stretch the journal dropped still counts.
    let next = live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 100, vec![1; 100])),
    );
    assert_eq!(recorded(next), [400]);
}

#[test]
fn each_track_keeps_its_own_meter() {
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
    let a = live.recorder(Some(MIC), RecorderEvent::Audio(chunk(MIC, 0, vec![5; 10])));
    let b = live.recorder(
        Some(SYSTEM),
        RecorderEvent::Audio(chunk(SYSTEM, 0, vec![7; 10])),
    );
    assert_eq!(
        a.updates[0],
        Event::Level {
            track: MIC,
            at: ms(10),
            level: Level::from_peak(5)
        }
    );
    assert_eq!(
        b.updates,
        [
            Event::Level {
                track: SYSTEM,
                at: ms(10),
                level: Level::from_peak(7)
            },
            Event::Recorded(40)
        ]
    );
}

#[test]
fn a_new_epoch_flushes_the_engine_and_places_later_text_after_the_gap() {
    let recorder_mic = {
        let mut t = opened(MIC, 0);
        t.open_epoch(ms(5_000), SampleIndex::new(1_000), rate())
            .unwrap();
        t
    };
    let mut live = Live::new(&[opened(MIC, 0)]);
    let epoch = recorder_mic.epochs()[1];
    let actions = live.recorder(Some(MIC), RecorderEvent::Epoch(epoch));
    assert_eq!(actions.flush, Some(MIC));
    assert!(actions.updates.is_empty());
    let before = live.engine(heard(MIC, 500, 900, "before"));
    let after = live.engine(heard(MIC, 1_000, 1_200, "after"));
    assert_eq!(
        texts(&before.updates),
        [(ms(500), ms(900), "before".to_owned())]
    );
    assert_eq!(
        texts(&after.updates),
        [(ms(5_000), ms(5_200), "after".to_owned())]
    );
}

/// A retimed epoch follows straight on, with no gap: the engine keeps what
/// it's hearing, and later text is placed by the new epoch's drift.
#[test]
fn a_retimed_epoch_flushes_nothing() {
    let recorder_mic = {
        let mut t = opened(MIC, 0);
        t.retime(
            SampleIndex::new(1_000),
            nota_core::Drift::from_ppb(-100_000).unwrap(),
        )
        .unwrap();
        t
    };
    let mut live = Live::new(&[opened(MIC, 0)]);
    let epoch = recorder_mic.epochs()[1];
    let actions = live.recorder(Some(MIC), RecorderEvent::Epoch(epoch));
    assert_eq!(actions.flush, None);
    assert!(actions.updates.is_empty());
    // 10 s of samples 100 ppm slow after the retime take 10.001 s.
    let after = live.engine(heard(MIC, 1_000, 11_000, "after"));
    assert_eq!(
        texts(&after.updates),
        [(
            ms(1_000),
            SessionTime::from_nanos(11_001_000_101),
            "after".to_owned()
        )]
    );
}

#[test]
fn a_failed_stream_flushes_its_track() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    let failed =
        RecorderEvent::CaptureFailed(nota_recorder::capture::CaptureError::Backend("gone".into()));
    assert_eq!(live.recorder(Some(MIC), failed).flush, Some(MIC));
}

#[test]
fn two_tracks_text_is_placed_by_each_tracks_own_epochs() {
    // The system audio started 2 s after the mic, and the mic reopened at
    // 10 s after 3 s of audio: their sample numbers alone would misorder
    // the text.
    let mut mic = opened(MIC, 0);
    mic.open_epoch(ms(10_000), SampleIndex::new(3_000), rate())
        .unwrap();
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 2_000)]);
    live.recorder(Some(MIC), RecorderEvent::Epoch(mic.epochs()[1]));
    let mut shown = Vec::new();
    for event in [
        heard(SYSTEM, 0, 500, "s1"),
        heard(MIC, 3_000, 3_500, "m2"),
        heard(MIC, 0, 400, "m1"),
        heard(SYSTEM, 4_000, 4_500, "s2"),
    ] {
        shown.extend(texts(&live.engine(event).updates));
    }
    shown.sort();
    let order: Vec<_> = shown.iter().map(|(.., t)| t.as_str()).collect();
    assert_eq!(order, ["m1", "s1", "s2", "m2"]);
    assert_eq!(shown[2].0, ms(6_000));
    assert_eq!(shown[3].0, ms(10_000));
}

#[test]
fn text_for_a_track_nobody_follows_is_dropped() {
    let live = Live::new(&[opened(MIC, 0)]);
    assert!(live.engine(heard(SYSTEM, 0, 10, "x")).updates.is_empty());
    assert!(
        live.engine(EngineEvent::Confirmed {
            track: MIC,
            up_to: SampleIndex::new(10)
        })
        .updates
        .is_empty()
    );
}

/// A track that joins the recorder is followed from its first epoch,
/// which flushes nothing: its text is placed from when its audio began.
#[test]
fn a_joining_track_is_followed_from_its_first_epoch() {
    let mut live = Live::new(&[]);
    assert!(live.engine(heard(SYSTEM, 0, 10, "x")).updates.is_empty());
    let first = opened(SYSTEM, 2_000).epochs()[0];
    let actions = live.recorder(Some(SYSTEM), RecorderEvent::Epoch(first));
    assert_eq!(actions.flush, None);
    assert_eq!(
        texts(&live.engine(heard(SYSTEM, 500, 900, "welcome")).updates),
        [(ms(2_500), ms(2_900), "welcome".to_owned())]
    );
}

/// A track that joins a resumed session starts in an epoch above zero,
/// from the sample its earlier audio ended at, and is followed from it all
/// the same.
#[test]
fn a_joining_track_of_a_resumed_session_is_followed_from_its_first_epoch() {
    let mut live = Live::new(&[]);
    let mut resumed = TrackTimeline::starting_after(SYSTEM, nota_core::EpochId::new(4)).unwrap();
    resumed
        .open_epoch(ms(60_000), SampleIndex::new(1_000), rate())
        .unwrap();
    let first = resumed.epochs()[0];
    live.recorder(Some(SYSTEM), RecorderEvent::Epoch(first));
    assert_eq!(
        texts(&live.engine(heard(SYSTEM, 1_500, 1_900, "again")).updates),
        [(ms(60_500), ms(60_900), "again".to_owned())]
    );
}

/// A mic timeline that was suspended after `audio` samples (a millisecond
/// each) and reopened at `resumed` ms, as the capture does for the first
/// audio after a sleep.
fn reopened_at(resumed: u64, audio: u64) -> TrackTimeline {
    let mut timeline = opened(MIC, 0);
    timeline
        .open_epoch(ms(resumed), SampleIndex::new(audio), rate())
        .unwrap();
    timeline
}

fn suspended() -> RecorderEvent {
    RecorderEvent::Capture(CaptureNotice::Suspended)
}

/// What the screen is told apart from levels and sizes.
fn told(updates: Vec<Event>) -> Vec<Event> {
    updates
        .into_iter()
        .filter(|u| !matches!(u, Event::Level { .. } | Event::Recorded(_)))
        .collect()
}

fn slept_warning(at: SessionTime) -> Event {
    Event::Warning(Warning {
        cause: Cause::Slept,
        track: None,
        at,
        state: WarningState::Raised,
    })
}

/// The first audio after a suspend brings the warning, and the epoch and
/// gap the capture opened for it. Nothing is said before that audio.
#[test]
fn the_first_audio_after_a_suspend_warns_and_reports_the_gap() {
    let timeline = reopened_at(5_000, 1_000);
    let (epoch, gap) = (timeline.epochs()[1], timeline.gaps().next().unwrap());
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 0, vec![1; 1_000])),
    );
    let noticed = live.recorder(Some(MIC), suspended());
    assert_eq!(noticed.updates, []);
    live.recorder(Some(MIC), RecorderEvent::Epoch(epoch));
    let woke = live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 1_000, vec![1; 100])),
    );
    assert_eq!(
        told(woke.updates),
        [
            slept_warning(ms(5_000)),
            Event::Epoch { track: MIC, epoch },
            Event::Gap { track: MIC, gap },
        ]
    );
    assert_eq!(
        live.slept(),
        [Slept {
            resumed: ms(5_000),
            gap: Some(gap)
        }]
    );
}

/// Both tracks wake from one sleep: the screen gets one warning, and each
/// track's own epoch and gap.
#[test]
fn two_tracks_waking_from_one_sleep_warn_once() {
    let mic = reopened_at(5_000, 1_000);
    let mut system = opened(SYSTEM, 0);
    system
        .open_epoch(ms(5_040), SampleIndex::new(1_000), rate())
        .unwrap();
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
    for (track, timeline) in [(MIC, &mic), (SYSTEM, &system)] {
        live.recorder(Some(track), suspended());
        live.recorder(Some(track), RecorderEvent::Epoch(timeline.epochs()[1]));
    }
    let mic_woke = live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 1_000, vec![1; 10])),
    );
    let system_woke = live.recorder(
        Some(SYSTEM),
        RecorderEvent::Audio(chunk(SYSTEM, 1_000, vec![1; 10])),
    );
    let warnings = |updates: Vec<Event>| {
        told(updates)
            .into_iter()
            .filter(|u| matches!(u, Event::Warning(_)))
            .count()
    };
    assert_eq!(warnings(mic_woke.updates), 1);
    assert_eq!(warnings(system_woke.updates.clone()), 0);
    // The second track still gets its own epoch and gap.
    assert_eq!(
        told(system_woke.updates),
        [
            Event::Epoch {
                track: SYSTEM,
                epoch: system.epochs()[1]
            },
            Event::Gap {
                track: SYSTEM,
                gap: system.gaps().next().unwrap()
            },
        ]
    );
    assert_eq!(live.slept().len(), 1);
}

/// A sleep a minute after another is a sleep of its own.
#[test]
fn a_later_sleep_warns_again() {
    let mut timeline = reopened_at(5_000, 1_000);
    timeline
        .open_epoch(ms(65_000), SampleIndex::new(1_100), rate())
        .unwrap();
    let mut live = Live::new(&[opened(MIC, 0)]);
    for (i, first) in [(1, 1_000), (2, 1_100)] {
        live.recorder(Some(MIC), suspended());
        live.recorder(Some(MIC), RecorderEvent::Epoch(timeline.epochs()[i]));
        live.recorder(
            Some(MIC),
            RecorderEvent::Audio(chunk(MIC, first, vec![1; 100])),
        );
    }
    let resumed: Vec<_> = live.slept().iter().map(|s| s.resumed).collect();
    assert_eq!(resumed, [ms(5_000), ms(65_000)]);
}

/// If the timeline refused an epoch for the sleep, the machine slept all
/// the same: the warning is raised, without a gap.
#[test]
fn a_suspend_the_timeline_refused_still_warns_without_a_gap() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.recorder(Some(MIC), suspended());
    live.recorder(
        Some(MIC),
        RecorderEvent::EpochRefused(nota_core::EpochError::TimeOverflow),
    );
    let woke = live.recorder(Some(MIC), RecorderEvent::Audio(chunk(MIC, 0, vec![1; 100])));
    assert_eq!(told(woke.updates), [slept_warning(ms(0))]);
    assert_eq!(
        live.slept(),
        [Slept {
            resumed: ms(0),
            gap: None
        }]
    );
}

/// A route change reopens the track too, but nothing slept: only a
/// suspend notice makes the audio after it a sleep.
#[test]
fn a_reopened_epoch_with_no_suspend_is_not_a_sleep() {
    let timeline = reopened_at(5_000, 1_000);
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.recorder(Some(MIC), RecorderEvent::Epoch(timeline.epochs()[1]));
    let woke = live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 1_000, vec![1; 100])),
    );
    assert_eq!(told(woke.updates), []);
    assert_eq!(live.slept(), []);
}

/// A track that wakes from a sleep a few seconds after the others, as a
/// Bluetooth mic can, is the same sleep: its gap overlaps theirs, though
/// its audio comes back more than a buffer later.
#[test]
fn a_track_slow_to_wake_is_still_the_same_sleep() {
    let mic = reopened_at(5_000, 1_000);
    let mut system = opened(SYSTEM, 0);
    system
        .open_epoch(ms(9_000), SampleIndex::new(1_000), rate())
        .unwrap();
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
    // The slow track's audio is handled first, and the quick one's after.
    for (track, timeline) in [(SYSTEM, &system), (MIC, &mic)] {
        live.recorder(Some(track), suspended());
        live.recorder(Some(track), RecorderEvent::Epoch(timeline.epochs()[1]));
        live.recorder(
            Some(track),
            RecorderEvent::Audio(chunk(track, 1_000, vec![1; 10])),
        );
    }
    assert_eq!(live.slept().len(), 1);
}

/// One track that sleeps again just after waking is another sleep, though
/// with no gap to compare (the timeline refused the second one's epoch) its
/// audio comes back within two seconds of the first sleep's.
#[test]
fn a_second_sleep_on_one_track_just_after_the_first_warns_again() {
    let timeline = reopened_at(5_000, 1_000);
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.recorder(Some(MIC), suspended());
    live.recorder(Some(MIC), RecorderEvent::Epoch(timeline.epochs()[1]));
    live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 1_000, vec![1; 100])),
    );
    live.recorder(Some(MIC), suspended());
    live.recorder(
        Some(MIC),
        RecorderEvent::EpochRefused(nota_core::EpochError::TimeOverflow),
    );
    live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 1_100, vec![1; 100])),
    );
    let resumed: Vec<_> = live.slept().iter().map(|s| s.resumed).collect();
    assert_eq!(resumed, [ms(5_000), ms(5_100)]);
}

/// When the timeline refuses the epoch for a sleep, the track's older gap
/// isn't taken for the sleep's: the warning comes without a gap, and the
/// old epoch and gap aren't sent again.
#[test]
fn a_refused_epoch_does_not_borrow_an_earlier_gap() {
    let timeline = reopened_at(5_000, 1_000);
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 0, vec![1; 1_000])),
    );
    live.recorder(Some(MIC), RecorderEvent::Epoch(timeline.epochs()[1]));
    // A route change's gap, and three seconds of audio after it.
    live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 1_000, vec![1; 3_000])),
    );
    live.recorder(Some(MIC), suspended());
    live.recorder(
        Some(MIC),
        RecorderEvent::EpochRefused(nota_core::EpochError::TimeOverflow),
    );
    let woke = live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 4_000, vec![1; 100])),
    );
    assert_eq!(told(woke.updates), [slept_warning(ms(8_000))]);
    assert_eq!(
        live.slept(),
        [Slept {
            resumed: ms(8_000),
            gap: None
        }]
    );
}

/// With no gaps to compare (the timelines refused both epochs), tracks
/// whose audio comes back within two seconds of each other, whichever
/// first, woke from one sleep; three seconds apart, two.
#[test]
fn tracks_without_gaps_wake_from_one_sleep_if_they_resume_within_two_seconds() {
    // A sample is a millisecond, so `at` is when the audio came back.
    let wake = |first: (TrackId, u64), second: (TrackId, u64)| {
        let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
        for (track, at) in [first, second] {
            live.recorder(Some(track), suspended());
            live.recorder(
                Some(track),
                RecorderEvent::EpochRefused(nota_core::EpochError::TimeOverflow),
            );
            live.recorder(
                Some(track),
                RecorderEvent::Audio(chunk(track, at, vec![1; 10])),
            );
        }
        live.slept().len()
    };
    assert_eq!(wake((MIC, 1_000), (SYSTEM, 1_500)), 1);
    assert_eq!(wake((SYSTEM, 1_500), (MIC, 1_000)), 1);
    assert_eq!(wake((MIC, 1_000), (SYSTEM, 4_000)), 2);
}
