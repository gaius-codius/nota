use nota_core::{SampleIndex, SampleRange, SampleRate};

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
