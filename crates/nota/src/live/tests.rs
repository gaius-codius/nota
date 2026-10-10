use nota_core::recorder::{Cause, DeviceChange, EngineState, Warning, WarningState};
use nota_core::{Drift, SampleIndex, SampleRange, SampleRate};
use nota_recorder::capture::{CaptureError, CaptureNotice};
use nota_recorder::detect::Condition;
use nota_recorder::engine::OfflineReason;

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
    assert_eq!(recorded(failed), Vec::<u64>::new());
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
        t.retime(SampleIndex::new(1_000), Drift::from_ppb(-100_000).unwrap())
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

/// Each detector's condition reaches the screen as the warning of its own
/// cause, for its own track and at the time the recorder gave.
#[test]
fn a_detected_condition_is_a_warning_for_its_track() {
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
    let mut warnings = Vec::new();
    for (track, condition, state, at) in [
        (MIC, Condition::Stalled, WarningState::Raised, ms(10)),
        (
            SYSTEM,
            Condition::DigitalZeros,
            WarningState::Raised,
            ms(20),
        ),
        (MIC, Condition::Quiet, WarningState::Cleared, ms(30)),
    ] {
        let detected = RecorderEvent::Detected {
            condition,
            state,
            at,
        };
        warnings.extend(live.recorder(Some(track), detected).updates);
    }
    let warning = |cause, track, at, state| {
        Event::Warning(Warning {
            cause,
            track: Some(track),
            at,
            state,
        })
    };
    assert_eq!(
        warnings,
        [
            warning(Cause::Stalled, MIC, ms(10), WarningState::Raised),
            warning(Cause::DigitalZeros, SYSTEM, ms(20), WarningState::Raised),
            warning(Cause::Quiet, MIC, ms(30), WarningState::Cleared),
        ]
    );
}

/// A device change reaches the screen with the track it's about and the
/// time it was noticed.
#[test]
fn a_device_change_is_a_device_event_for_its_track() {
    let mut live = Live::new(&[opened(SYSTEM, 0)]);
    let lost = RecorderEvent::Device {
        change: DeviceChange::Lost,
        at: ms(40),
    };
    assert_eq!(
        live.recorder(Some(SYSTEM), lost).updates,
        [Event::Device {
            track: SYSTEM,
            change: DeviceChange::Lost,
            at: ms(40)
        }]
    );
}

#[test]
fn a_failed_stream_flushes_its_track() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    let failed = RecorderEvent::CaptureFailed(CaptureError::Backend("gone".into()));
    assert_eq!(live.recorder(Some(MIC), failed).flush, Some(MIC));
}

/// The warning `Live` raises for a report that carries no time.
fn raised(cause: Cause, track: TrackId, at: SessionTime) -> Event {
    Event::Warning(Warning {
        cause,
        track: Some(track),
        at,
        state: WarningState::Raised,
    })
}

/// A failed stream is a warning with the reason, dated where the track's
/// audio ended, and the flush is still asked for.
#[test]
fn a_failed_stream_warns_with_its_reason_where_the_audio_ended() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.recorder(Some(MIC), RecorderEvent::Audio(chunk(MIC, 0, vec![1; 40])));
    let error = CaptureError::Backend("gone".into());
    let cause = Cause::StreamFailed(error.to_string());
    let actions = live.recorder(Some(MIC), RecorderEvent::CaptureFailed(error));
    assert_eq!(
        actions,
        Actions {
            flush: Some(MIC),
            updates: vec![raised(cause, MIC, ms(40))],
            ..Actions::default()
        }
    );
}

/// Before any audio, a report is dated at the start of the track's current
/// epoch, which here isn't the session's start.
#[test]
fn a_failed_stream_before_any_audio_is_dated_at_its_epochs_start() {
    let mut live = Live::new(&[opened(MIC, 250)]);
    let error = CaptureError::Backend("gone".into());
    let cause = Cause::StreamFailed(error.to_string());
    let actions = live.recorder(Some(MIC), RecorderEvent::CaptureFailed(error));
    assert_eq!(actions.updates, [raised(cause, MIC, ms(250))]);
}

/// A report about no track is no warning.
#[test]
fn a_failure_about_no_track_warns_of_nothing() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    let error = CaptureError::Backend("gone".into());
    let actions = live.recorder(None, RecorderEvent::CaptureFailed(error));
    assert_eq!(actions, Actions::default());
}

/// A broken journal is a warning with the reason, for its track.
#[test]
fn a_broken_journal_warns_with_its_reason_where_the_audio_ended() {
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
    live.recorder(Some(MIC), RecorderEvent::Audio(chunk(MIC, 0, vec![1; 70])));
    let error = nota_recorder::session::SessionError::Marks(std::io::ErrorKind::Other.into());
    let cause = Cause::JournalFailed(error.to_string());
    let actions = live.recorder(Some(MIC), RecorderEvent::JournalFailed(error));
    assert_eq!(
        actions,
        Actions {
            updates: vec![raised(cause, MIC, ms(70))],
            ..Actions::default()
        }
    );
}

/// With nothing heard on the track, the journal's warning is dated at its
/// epoch's start, not at another track's audio.
#[test]
fn a_broken_journal_with_nothing_heard_is_dated_at_the_epochs_start() {
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 300)]);
    live.recorder(Some(MIC), RecorderEvent::Audio(chunk(MIC, 0, vec![1; 70])));
    let error = nota_recorder::session::SessionError::Marks(std::io::ErrorKind::Other.into());
    let cause = Cause::JournalFailed(error.to_string());
    let actions = live.recorder(Some(SYSTEM), RecorderEvent::JournalFailed(error));
    assert_eq!(actions.updates, [raised(cause, SYSTEM, ms(300))]);
}

/// Drift is reported once, as a warning the screen can ignore and the
/// timeline stores.
#[test]
fn drift_is_a_warning_for_its_track() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.recorder(Some(MIC), RecorderEvent::Audio(chunk(MIC, 0, vec![1; 20])));
    let drift = Drift::from_ppb(900_000).unwrap();
    let actions = live.recorder(
        Some(MIC),
        RecorderEvent::Capture(CaptureNotice::Drifted(drift)),
    );
    assert_eq!(
        actions,
        Actions {
            updates: vec![raised(Cause::Drift, MIC, ms(20))],
            ..Actions::default()
        }
    );
}

fn online() -> EngineEvent {
    EngineEvent::Status(EngineStatus::Online { pid: 1 })
}

fn offline() -> EngineEvent {
    EngineEvent::Status(EngineStatus::Offline(OfflineReason::StartTimeout))
}

fn confirmed(track: TrackId, up_to: u64) -> EngineEvent {
    EngineEvent::Confirmed {
        track,
        up_to: SampleIndex::new(up_to),
    }
}

/// The transcribing marks among `updates`.
fn marks(updates: Vec<Event>) -> Vec<Event> {
    updates
        .into_iter()
        .filter(|u| matches!(u, Event::Transcribing(_)))
        .collect()
}

/// Hands the engine `samples` of `track` from sample `from` and returns the
/// transcribing marks that sent.
fn hand(live: &mut Live, track: TrackId, from: u64, samples: usize) -> Vec<Event> {
    let audio = chunk(track, from, vec![1; samples]);
    let actions = live.recorder(Some(track), RecorderEvent::Audio(audio));
    marks(actions.updates)
}

/// The engine coming up and going down reach the screen, the reason as the
/// text the reason displays.
#[test]
fn the_engines_status_reaches_the_screen() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    let text = OfflineReason::StartTimeout.to_string();
    let up = live.engine(online());
    let down = live.engine(offline());
    assert_eq!(up.updates, [Event::Engine(EngineState::Online)]);
    assert_eq!(down.updates, [Event::Engine(EngineState::Offline(text))]);
}

/// The mark turns on once a second of audio waits, and a later chunk
/// doesn't send it again.
#[test]
fn transcribing_turns_on_once_with_a_second_unanswered() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.engine(online());
    assert_eq!(hand(&mut live, MIC, 0, 1_500), [Event::Transcribing(true)]);
    assert_eq!(hand(&mut live, MIC, 1_500, 500), []);
}

/// The mark turns off when the engine has answered all the audio it was
/// handed.
#[test]
fn transcribing_turns_off_when_the_engine_confirms_the_audio() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.engine(online());
    hand(&mut live, MIC, 0, 1_500);
    let actions = live.engine(confirmed(MIC, 1_500));
    assert_eq!(actions.updates, [Event::Transcribing(false)]);
}

/// Under a second unanswered the mark stays off, even with more audio
/// handed over in all: it would flicker with every chunk.
#[test]
fn transcribing_stays_off_under_a_second_unanswered() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.engine(online());
    assert_eq!(hand(&mut live, MIC, 0, 999), []);
    live.engine(confirmed(MIC, 500));
    assert_eq!(hand(&mut live, MIC, 999, 500), []);
}

/// The engine going down while the mark is on turns it off, and its coming
/// back with the audio still unanswered turns it on again.
#[test]
fn going_offline_turns_transcribing_off() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.engine(online());
    hand(&mut live, MIC, 0, 1_500);
    let down = live.engine(offline());
    let reason = OfflineReason::StartTimeout.to_string();
    assert_eq!(
        down.updates,
        [
            Event::Engine(EngineState::Offline(reason)),
            Event::Transcribing(false)
        ]
    );
    let up = live.engine(online());
    assert_eq!(
        up.updates,
        [
            Event::Engine(EngineState::Online),
            Event::Transcribing(true)
        ]
    );
}

/// With no engine ever online, nothing is with it.
#[test]
fn transcribing_never_turns_on_without_an_engine() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    assert_eq!(hand(&mut live, MIC, 0, 5_000), []);
}

/// Audio the engine skipped is answered as far as the skip reaches.
#[test]
fn skipped_audio_counts_as_confirmed() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.engine(online());
    hand(&mut live, MIC, 0, 2_000);
    let range = SampleRange::new(SampleIndex::ZERO, SampleIndex::new(2_000)).unwrap();
    let actions = live.engine(EngineEvent::Skipped { track: MIC, range });
    assert_eq!(actions.updates, [Event::Transcribing(false)]);
}

/// A second's wait is judged at its own track's rate, not a fixed one.
#[test]
fn the_wait_is_measured_at_the_tracks_own_rate() {
    let fast = SampleRate::new(48_000).unwrap();
    let mut timeline = TrackTimeline::new(SYSTEM);
    timeline.open_epoch(ms(0), SampleIndex::ZERO, fast).unwrap();
    let mut live = Live::new(&[timeline]);
    live.engine(online());
    let short = AudioChunk::new(SYSTEM, SampleIndex::ZERO, fast, vec![1; 47_999]).unwrap();
    let long = AudioChunk::new(SYSTEM, SampleIndex::new(47_999), fast, vec![1; 1]).unwrap();
    let first = live.recorder(Some(SYSTEM), RecorderEvent::Audio(short));
    let second = live.recorder(Some(SYSTEM), RecorderEvent::Audio(long));
    assert_eq!(marks(first.updates), []);
    assert_eq!(marks(second.updates), [Event::Transcribing(true)]);
}

/// A confirmation that arrives late, naming an older position, doesn't
/// take the engine's progress back and turn the mark on again.
#[test]
fn an_older_confirmation_arriving_late_changes_nothing() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.engine(online());
    hand(&mut live, MIC, 0, 1_500);
    live.engine(confirmed(MIC, 1_500));
    let late = live.engine(confirmed(MIC, 200));
    assert_eq!(late.updates, []);
    assert_eq!(hand(&mut live, MIC, 1_500, 500), []);
}

/// A track whose samples start late holds nothing before its first chunk:
/// 1.5 s from sample 160 000 turns the mark on once.
#[test]
fn a_track_that_starts_late_is_not_counted_from_sample_zero() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.engine(online());
    let first = hand(&mut live, MIC, 160_000, 1_500);
    assert_eq!(first, [Event::Transcribing(true)]);
    assert_eq!(hand(&mut live, MIC, 161_500, 100), []);
}

/// The same late start with under a second of audio leaves the mark off.
#[test]
fn a_short_first_chunk_that_starts_late_stays_off() {
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.engine(online());
    assert_eq!(hand(&mut live, MIC, 160_000, 900), []);
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
    let mut live = Live::new(&[opened(MIC, 0)]);
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
            unrecorded: Some(Unrecorded {
                from: gap.from(),
                to: gap.to()
            })
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
            unrecorded: None
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
            unrecorded: None
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

/// An overrun reported on the way back from a sleep opens an epoch with no
/// audio, and a stall after it another: the sleep is the whole stretch
/// from the last audio to the first, and the screen gets each epoch and gap.
#[test]
fn a_sleep_followed_by_an_overrun_is_one_stretch() {
    let mut timeline = reopened_at(5_000, 1_000);
    timeline
        .open_epoch(ms(6_000), SampleIndex::new(1_000), rate())
        .unwrap();
    let (first, second) = (timeline.epochs()[1], timeline.epochs()[2]);
    let gaps: Vec<_> = timeline.gaps().collect();
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 0, vec![1; 1_000])),
    );
    live.recorder(Some(MIC), suspended());
    live.recorder(Some(MIC), RecorderEvent::Epoch(first));
    live.recorder(Some(MIC), RecorderEvent::Epoch(second));
    let woke = live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 1_000, vec![1; 100])),
    );
    assert_eq!(
        told(woke.updates),
        [
            slept_warning(ms(6_000)),
            Event::Epoch {
                track: MIC,
                epoch: first
            },
            Event::Gap {
                track: MIC,
                gap: gaps[0]
            },
            Event::Epoch {
                track: MIC,
                epoch: second
            },
            Event::Gap {
                track: MIC,
                gap: gaps[1]
            },
        ]
    );
    assert_eq!(
        live.slept(),
        [Slept {
            resumed: ms(6_000),
            unrecorded: Some(Unrecorded {
                from: ms(1_000),
                to: ms(6_000)
            })
        }]
    );
}

/// What the machine slept through is what no track recorded: a track that
/// came back late doesn't stretch the sleep.
#[test]
fn a_late_track_does_not_stretch_the_sleep() {
    let mic = reopened_at(5_000, 1_000);
    let mut system = opened(SYSTEM, 0);
    system
        .open_epoch(ms(9_000), SampleIndex::new(1_000), rate())
        .unwrap();
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
    // The late track is handled first, so its longer stretch is the first
    // one seen.
    for (track, timeline) in [(SYSTEM, &system), (MIC, &mic)] {
        live.recorder(
            Some(track),
            RecorderEvent::Audio(chunk(track, 0, vec![1; 1_000])),
        );
        live.recorder(Some(track), suspended());
        live.recorder(Some(track), RecorderEvent::Epoch(timeline.epochs()[1]));
        live.recorder(
            Some(track),
            RecorderEvent::Audio(chunk(track, 1_000, vec![1; 10])),
        );
    }
    assert_eq!(
        live.slept(),
        [Slept {
            resumed: ms(5_000),
            unrecorded: Some(Unrecorded {
                from: ms(1_000),
                to: ms(5_000)
            })
        }]
    );
}

/// Wakes `track` from a sleep, with `epoch` the epoch it was reopened in
/// (`None` if the timeline refused one), and `first` its first sample.
fn wake(live: &mut Live, track: TrackId, epoch: Option<Epoch>, first: u64) {
    live.recorder(Some(track), suspended());
    let reopened = match epoch {
        Some(epoch) => RecorderEvent::Epoch(epoch),
        None => RecorderEvent::EpochRefused(nota_core::EpochError::TimeOverflow),
    };
    live.recorder(Some(track), reopened);
    live.recorder(
        Some(track),
        RecorderEvent::Audio(chunk(track, first, vec![1; 10])),
    );
}

/// A track whose epoch was refused is the same sleep if its audio came back
/// in the other track's stretch or just after it, and another sleep if it
/// came back long after.
#[test]
fn a_track_with_no_gap_is_judged_against_the_other_tracks_stretch() {
    let mic = reopened_at(5_000, 1_000);
    for (first, sleeps) in [(1_000, 1), (6_999, 1), (7_500, 2)] {
        let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
        wake(&mut live, MIC, Some(mic.epochs()[1]), 1_000);
        wake(&mut live, SYSTEM, None, first);
        assert_eq!(live.slept().len(), sleeps, "system back at {first} ms");
    }
}

/// Two tracks whose gaps don't overlap (the system audio had stopped well
/// after the mic's gap ended) didn't sleep together.
#[test]
fn tracks_with_gaps_that_do_not_overlap_are_separate_sleeps() {
    let mic = reopened_at(5_000, 1_000);
    let mut system = opened(SYSTEM, 0);
    system
        .open_epoch(ms(9_000), SampleIndex::new(6_000), rate())
        .unwrap();
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
    live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 0, vec![1; 1_000])),
    );
    live.recorder(
        Some(SYSTEM),
        RecorderEvent::Audio(chunk(SYSTEM, 0, vec![1; 6_000])),
    );
    wake(&mut live, MIC, Some(mic.epochs()[1]), 1_000);
    wake(&mut live, SYSTEM, Some(system.epochs()[1]), 6_000);
    let resumed: Vec<_> = live.slept().iter().map(|s| s.resumed).collect();
    assert_eq!(resumed, [ms(5_000), ms(9_000)]);
}

/// A track that woke with the others and then sleeps again, soon enough
/// that its audio comes back inside the first sleep's reach, is another
/// sleep: it already woke from that one.
#[test]
fn a_track_that_woke_from_a_sleep_does_not_join_it_again() {
    let mic = reopened_at(5_000, 1_000);
    let mut system = opened(SYSTEM, 0);
    system
        .open_epoch(ms(5_040), SampleIndex::new(1_000), rate())
        .unwrap();
    let mut live = Live::new(&[opened(MIC, 0), opened(SYSTEM, 0)]);
    wake(&mut live, MIC, Some(mic.epochs()[1]), 1_000);
    wake(&mut live, SYSTEM, Some(system.epochs()[1]), 1_000);
    assert_eq!(live.slept().len(), 1);
    // The system audio sleeps again at once; its epoch is refused, so its
    // audio is timed 100 ms after its last, inside the first sleep's reach.
    wake(&mut live, SYSTEM, None, 1_010);
    assert_eq!(live.slept().len(), 2);
}

/// An epoch that follows its predecessor with no gap (the timeline took
/// the stretch as drift) isn't the end of an older gap: with no audio of
/// the track heard yet to say where this stretch began, the warning comes
/// without a gap rather than with the older one.
#[test]
fn an_epoch_with_no_gap_before_it_does_not_report_an_older_one() {
    let mut timeline = reopened_at(5_000, 1_000);
    // The second epoch starts where the first's audio ends: no gap.
    timeline
        .open_epoch(ms(6_000), SampleIndex::new(2_000), rate())
        .unwrap();
    assert_eq!(timeline.gaps().count(), 1);
    let mut live = Live::new(&[opened(MIC, 0)]);
    live.recorder(Some(MIC), RecorderEvent::Epoch(timeline.epochs()[1]));
    live.recorder(Some(MIC), RecorderEvent::Epoch(timeline.epochs()[2]));
    live.recorder(Some(MIC), suspended());
    let woke = live.recorder(
        Some(MIC),
        RecorderEvent::Audio(chunk(MIC, 2_000, vec![1; 100])),
    );
    assert_eq!(told(woke.updates), [slept_warning(ms(6_000))]);
    assert_eq!(live.slept()[0].unrecorded, None);
}
