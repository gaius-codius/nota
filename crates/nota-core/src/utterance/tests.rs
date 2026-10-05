use std::time::Duration;

use proptest::prelude::*;

use super::*;
use crate::epoch::EpochError;
use crate::ids::EpochId;
use crate::time::{SampleIndex, SampleRange, SampleRate};

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

/// A millisecond per sample keeps the arithmetic exact.
fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

fn range(start: u64, end: u64) -> SampleRange {
    SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap()
}

fn heard(track: TrackId, start: u64, end: u64, text: &str) -> Transcript {
    Transcript::new(track, range(start, end), text.to_owned()).unwrap()
}

/// `track` opened at 0 ms from sample 0, then reopened at 5 s after 2 s of
/// audio: a 3 s gap from sample 2,000.
fn with_gap(track: TrackId) -> TrackTimeline {
    let mut timeline = TrackTimeline::new(track);
    timeline
        .open_epoch(ms(0), SampleIndex::ZERO, rate())
        .unwrap();
    timeline
        .open_epoch(ms(5_000), SampleIndex::new(2_000), rate())
        .unwrap();
    timeline
}

#[test]
fn a_transcript_is_placed_through_its_epoch() {
    let timeline = with_gap(MIC);
    let before = Utterance::place(heard(MIC, 500, 1_500, "before"), &timeline).unwrap();
    assert_eq!((before.start(), before.end()), (ms(500), ms(1_500)));
    assert_eq!(before.track(), MIC);
    assert_eq!(before.text(), "before");
    let after = Utterance::place(heard(MIC, 2_000, 2_250, "after"), &timeline).unwrap();
    assert_eq!((after.start(), after.end()), (ms(5_000), ms(5_250)));
    assert_eq!(after.into_text(), "after");
}

#[test]
fn a_transcript_ending_at_an_epoch_boundary_ends_with_its_audio() {
    let timeline = with_gap(MIC);
    // The last sample before the gap is 1,999: its audio ends at 2 s, not
    // where the next epoch starts.
    let u = Utterance::place(heard(MIC, 1_900, 2_000, "x"), &timeline).unwrap();
    assert_eq!((u.start(), u.end()), (ms(1_900), ms(2_000)));
}

#[test]
fn a_transcript_across_epochs_spans_the_gap() {
    let timeline = with_gap(MIC);
    let u = Utterance::place(heard(MIC, 1_900, 2_100, "x"), &timeline).unwrap();
    assert_eq!((u.start(), u.end()), (ms(1_900), ms(5_100)));
}

#[test]
fn a_transcript_is_placed_only_through_its_own_track() {
    let timeline = with_gap(MIC);
    assert_eq!(Utterance::place(heard(SYSTEM, 0, 10, "x"), &timeline), None);
}

#[test]
fn a_transcript_before_the_first_epoch_is_not_placed() {
    let mut timeline = TrackTimeline::new(MIC);
    assert_eq!(Utterance::place(heard(MIC, 0, 10, "x"), &timeline), None);
    timeline
        .open_epoch(ms(0), SampleIndex::new(100), rate())
        .unwrap();
    assert_eq!(Utterance::place(heard(MIC, 50, 150, "x"), &timeline), None);
    assert!(Utterance::place(heard(MIC, 100, 150, "x"), &timeline).is_some());
}

#[test]
fn span_of_an_empty_range_is_none() {
    let timeline = with_gap(MIC);
    assert_eq!(timeline.span_of(range(10, 10)), None);
    assert_eq!(timeline.span_of(range(10, 11)), Some((ms(10), ms(11))));
}

#[test]
fn utterances_order_by_start_then_end_then_track() {
    let mut timeline = TrackTimeline::new(MIC);
    timeline
        .open_epoch(ms(0), SampleIndex::ZERO, rate())
        .unwrap();
    let mut system = TrackTimeline::new(SYSTEM);
    system.open_epoch(ms(0), SampleIndex::ZERO, rate()).unwrap();
    let a = Utterance::place(heard(SYSTEM, 10, 20, "a"), &system).unwrap();
    let b = Utterance::place(heard(MIC, 10, 30, "b"), &timeline).unwrap();
    let c = Utterance::place(heard(MIC, 10, 20, "c"), &timeline).unwrap();
    let d = Utterance::place(heard(MIC, 5, 40, "d"), &timeline).unwrap();
    let mut all = vec![a.clone(), b.clone(), c.clone(), d.clone()];
    all.sort();
    assert_eq!(all, vec![d, c, a, b]);
}

#[test]
fn a_follower_maps_like_the_timeline_it_follows() {
    let mut timeline = TrackTimeline::new(MIC);
    let mut follower = TrackTimeline::new(MIC);
    timeline
        .open_epoch(ms(0), SampleIndex::ZERO, rate())
        .unwrap();
    follower.follow(&timeline.epochs()[0]).unwrap();
    // The device ran fast: the reopening is pushed to where its audio ended.
    let opened = timeline
        .open_epoch(ms(999), SampleIndex::new(1_000), rate())
        .unwrap();
    assert_eq!(opened.start, ms(1_000));
    follower.follow(&timeline.epochs()[1]).unwrap();
    assert_eq!(follower, timeline);
}

#[test]
fn a_follower_refuses_an_epoch_out_of_turn() {
    let timeline = with_gap(MIC);
    let [first, second] = timeline.epochs() else {
        panic!("two epochs")
    };
    let mut follower = TrackTimeline::new(MIC);
    assert_eq!(
        follower.follow(second),
        Err(EpochError::NotNext {
            expected: EpochId::new(0),
            got: EpochId::new(1)
        })
    );
    follower.follow(first).unwrap();
    assert_eq!(
        follower.follow(first),
        Err(EpochError::NotNext {
            expected: EpochId::new(1),
            got: EpochId::new(0)
        })
    );
    follower.follow(second).unwrap();
    assert_eq!(follower, timeline);
}

#[test]
fn a_follower_refuses_an_epoch_that_goes_back() {
    // Two timelines that disagree about epoch 0: epoch 1 of one doesn't fit
    // the other's epoch 0.
    let mut late = TrackTimeline::new(MIC);
    late.open_epoch(ms(0), SampleIndex::new(5_000), rate())
        .unwrap();
    let mut early = TrackTimeline::new(MIC);
    early.open_epoch(ms(0), SampleIndex::ZERO, rate()).unwrap();
    early
        .open_epoch(ms(1_000), SampleIndex::new(1_000), rate())
        .unwrap();
    let next = early.epochs()[1];
    let before = late.clone();
    assert_eq!(
        late.follow(&next),
        Err(EpochError::SampleWentBack {
            previous: SampleIndex::new(5_000),
            first_sample: SampleIndex::new(1_000)
        })
    );
    assert_eq!(late, before);

    // Epoch 0 from 10 s on: epoch 1 of `early` starts at 1 s, before it.
    let mut later = TrackTimeline::new(MIC);
    later
        .open_epoch(ms(10_000), SampleIndex::ZERO, rate())
        .unwrap();
    assert_eq!(
        later.follow(&next),
        Err(EpochError::ImplausibleOverrun {
            previous_end: ms(11_000),
            start: ms(1_000)
        })
    );
    assert_eq!(later.epochs().len(), 1);
}

#[test]
fn a_follower_accepts_an_epoch_that_starts_as_the_last_audio_ends() {
    let mut timeline = TrackTimeline::new(MIC);
    timeline
        .open_epoch(ms(0), SampleIndex::ZERO, rate())
        .unwrap();
    timeline
        .open_epoch(ms(1_000), SampleIndex::new(1_000), rate())
        .unwrap();
    let mut follower = TrackTimeline::new(MIC);
    follower.follow(&timeline.epochs()[0]).unwrap();
    follower.follow(&timeline.epochs()[1]).unwrap();
    assert_eq!(follower, timeline);
}

#[test]
fn not_next_reads_plainly() {
    let e = EpochError::NotNext {
        expected: EpochId::new(2),
        got: EpochId::new(4),
    };
    assert_eq!(e.to_string(), "epoch 4 came where epoch 2 was due");
}

/// One epoch of a generated track: the gap before it (after the previous
/// epoch's audio), how many samples it holds, and where speech was heard
/// in it, as offsets from its first sample.
#[derive(Debug, Clone)]
struct GenEpoch {
    gap_ms: u64,
    len: u64,
    speech: Vec<(u64, u64)>,
}

fn gen_epoch() -> impl Strategy<Value = GenEpoch> {
    (
        0_u64..3_000,
        1_u64..4_000,
        prop::collection::vec((0_u64..4_000, 1_u64..800), 0..4),
    )
        .prop_map(|(gap_ms, len, cuts)| {
            // Speech within the epoch, in order and apart.
            let mut speech = Vec::new();
            let mut from = 0;
            let mut cuts: Vec<_> = cuts.into_iter().map(|(at, l)| (at % len, l)).collect();
            cuts.sort_unstable();
            for (at, l) in cuts {
                let start = at.max(from);
                let end = (start + l).min(len);
                if start < end {
                    speech.push((start, end));
                    from = end;
                }
            }
            GenEpoch {
                gap_ms,
                len,
                speech,
            }
        })
}

/// A recorded track: the recorder's timeline, its transcripts, and when
/// each was really spoken, worked out from the generated epochs alone.
fn record(
    track: TrackId,
    first_start_ms: u64,
    epochs: &[GenEpoch],
) -> (TrackTimeline, Vec<(Transcript, SessionTime, SessionTime)>) {
    let mut timeline = TrackTimeline::new(track);
    let mut said = Vec::new();
    let mut next_sample = 0;
    let mut audio_end_ms = first_start_ms;
    for (i, epoch) in epochs.iter().enumerate() {
        let start_ms = if i == 0 {
            first_start_ms
        } else {
            audio_end_ms + epoch.gap_ms
        };
        timeline
            .open_epoch(ms(start_ms), SampleIndex::new(next_sample), rate())
            .unwrap();
        for &(from, to) in &epoch.speech {
            let t = heard(
                track,
                next_sample + from,
                next_sample + to,
                &format!("{}:{}", track.get(), said.len()),
            );
            said.push((t, ms(start_ms + from), ms(start_ms + to)));
        }
        next_sample += epoch.len;
        audio_end_ms = start_ms + epoch.len;
    }
    (timeline, said)
}

proptest! {
    /// Two tracks, each reopened with gaps; their text arrives in any order,
    /// each transcript placed through a timeline following its track's
    /// epochs as they were opened. Sorted, it reads in the order it was
    /// spoken, each utterance at the moment it was spoken.
    #[test]
    fn two_tracks_merge_in_spoken_order_across_epochs(
        mic in prop::collection::vec(gen_epoch(), 1..5),
        system in prop::collection::vec(gen_epoch(), 1..5),
        system_starts_ms in 0_u64..2_000,
        arrival in prop::collection::vec(any::<prop::sample::Index>(), 40),
    ) {
        let (mic_timeline, mic_said) = record(MIC, 0, &mic);
        let (system_timeline, system_said) = record(SYSTEM, system_starts_ms, &system);
        let mut followers = [TrackTimeline::new(MIC), TrackTimeline::new(SYSTEM)];
        for (follower, timeline) in followers.iter_mut().zip([&mic_timeline, &system_timeline]) {
            for epoch in timeline.epochs() {
                follower.follow(epoch).unwrap();
            }
            prop_assert_eq!(&*follower, timeline);
        }

        // Every transcript, in an arbitrary arrival order.
        let mut pending: Vec<_> = mic_said.iter().chain(&system_said).cloned().collect();
        let mut placed = Vec::new();
        for pick in &arrival {
            if pending.is_empty() {
                break;
            }
            let (t, ..) = pending.remove(pick.index(pending.len()));
            let follower = &followers[usize::from(t.track() == SYSTEM)];
            placed.push(Utterance::place(t, follower).unwrap());
        }
        for (t, ..) in pending {
            let follower = &followers[usize::from(t.track() == SYSTEM)];
            placed.push(Utterance::place(t, follower).unwrap());
        }
        placed.sort();

        let mut expected: Vec<_> = mic_said
            .iter()
            .chain(&system_said)
            .map(|(t, start, end)| (*start, *end, t.track(), t.text().to_owned()))
            .collect();
        expected.sort();
        let got: Vec<_> = placed
            .iter()
            .map(|u| (u.start(), u.end(), u.track(), u.text().to_owned()))
            .collect();
        prop_assert_eq!(got, expected);
        for pair in placed.windows(2) {
            prop_assert!(pair[0].start() <= pair[1].start());
        }
    }
}

#[test]
fn spans_are_exact_at_speech_rate() {
    // 16 kHz: a sample every 62.5 µs, from an epoch at 1 s.
    let mut timeline = TrackTimeline::new(MIC);
    timeline
        .open_epoch(ms(1_000), SampleIndex::ZERO, SampleRate::SPEECH)
        .unwrap();
    assert_eq!(
        timeline.span_of(range(16_000, 16_001)),
        Some((
            ms(2_000),
            ms(2_000).checked_add(Duration::from_nanos(62_500)).unwrap()
        ))
    );
}
