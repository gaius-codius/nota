//! Tests for the chunker: one per cutting rule, and properties over random
//! speech, pauses, detector lag and push sizes.
//!
//! The properties, on every input:
//! - chunks tile the stream exactly and give back the audio pushed;
//! - no chunk is empty or longer than the cap;
//! - every chunk that overlaps speech is marked `has_speech`;
//! - a cut lands inside speech only as the fallback, near the cap; and
//!   when speech always pauses in time, never.

use nota_core::{SampleCount, SampleIndex, SampleRange};
use proptest::prelude::*;

use super::{Chunk, Chunker, ChunkerConfig, Labels};

fn config(min_pause: u64, target: u64, cap: u64, window: u64, frame: u64) -> ChunkerConfig {
    ChunkerConfig::new(
        SampleCount::new(min_pause),
        SampleCount::new(target),
        SampleCount::new(cap),
        SampleCount::new(window),
        SampleCount::new(frame),
    )
    .unwrap()
}

fn range(from: u64, to: u64) -> SampleRange {
    SampleRange::new(SampleIndex::new(from), SampleIndex::new(to)).unwrap()
}

/// A detector that knows the truth but reports it as the real one does:
/// a segment only once `min_pause` of silence follows it, and silence only
/// `lag` samples late. Segments closer than `min_pause` are one segment, as
/// the real detector would see them.
#[derive(Debug)]
struct FakeDetector {
    speech: Vec<(u64, u64)>,
    min_pause: u64,
    lag: u64,
    first: u64,
    processed: u64,
    reported: usize,
}

impl FakeDetector {
    fn new(first: u64, mut truth: Vec<(u64, u64)>, min_pause: u64, lag: u64) -> Self {
        truth.sort_unstable();
        let mut speech: Vec<(u64, u64)> = Vec::new();
        for (from, to) in truth.into_iter().filter(|(f, t)| t > f) {
            match speech.last_mut() {
                Some(last) if from < last.1 + min_pause => last.1 = last.1.max(to),
                _ => speech.push((from, to)),
            }
        }
        Self {
            speech,
            min_pause,
            lag,
            first,
            processed: first,
            reported: 0,
        }
    }

    fn accept(&mut self, n: u64) -> Labels {
        self.processed += n;
        let mut segments = Vec::new();
        while let Some(&(from, to)) = self.speech.get(self.reported) {
            if to + self.min_pause > self.processed {
                break;
            }
            segments.push(range(from, to));
            self.reported += 1;
        }
        let pending = self
            .speech
            .get(self.reported)
            .map_or(u64::MAX, |&(from, _)| from);
        let silent_until = self
            .processed
            .saturating_sub(self.lag)
            .max(self.first)
            .min(pending);
        Labels {
            segments,
            silent_until: SampleIndex::new(silent_until),
        }
    }

    fn flush(&mut self) -> Labels {
        let segments = self.speech[self.reported..]
            .iter()
            .map(|&(from, to)| range(from, to.min(self.processed)))
            .collect();
        self.reported = self.speech.len();
        Labels {
            segments,
            silent_until: SampleIndex::new(self.processed),
        }
    }

    fn in_speech(&self, sample: u64) -> bool {
        self.speech
            .iter()
            .any(|&(from, to)| from <= sample && sample < to)
    }
}

/// Audio that's loud in speech and near-silent otherwise.
fn audio(first: u64, len: u64, detector: &FakeDetector) -> Vec<f32> {
    (first..first + len)
        .map(|i| {
            let wobble = f32::from(u8::try_from((i * 7919) % 13).unwrap()) / 13.0;
            if detector.in_speech(i) {
                0.3 + 0.5 * wobble
            } else {
                0.001 * wobble
            }
        })
        .collect()
}

/// Runs the chunker over `audio` in pushes of the given sizes, then
/// finishes.
fn run(
    config: ChunkerConfig,
    first: u64,
    samples: &[f32],
    detector: &mut FakeDetector,
    pushes: &[usize],
) -> Vec<Chunk> {
    let mut chunker = Chunker::new(config, SampleIndex::new(first));
    let mut chunks = Vec::new();
    let mut at = 0;
    let mut sizes = pushes.iter().cycle();
    while at < samples.len() {
        let n = (*sizes.next().unwrap()).clamp(1, samples.len() - at);
        assert_eq!(chunker.next_sample().get(), first + at as u64);
        let labels = detector.accept(n as u64);
        chunks.extend(chunker.push(&samples[at..at + n], &labels));
        at += n;
    }
    chunks.extend(chunker.finish(&detector.flush()));
    chunks
}

#[test]
fn cuts_in_the_first_pause_after_the_target() {
    // Speech 0..30 and 40..70, then 80..100; pauses at 30..40 and 70..80.
    let config = config(5, 20, 100, 50, 2);
    let mut detector = FakeDetector::new(0, vec![(0, 30), (40, 70), (80, 100)], 5, 0);
    let samples = audio(0, 100, &detector);
    let chunks = run(config, 0, &samples, &mut detector, &[1]);
    let cuts: Vec<_> = chunks.iter().map(|c| c.range().end().get()).collect();
    // Each pause is cut as soon as it's settled and `min_pause` long: in the
    // middle of what's known of it (30..35, 70..75), not of its final extent.
    assert_eq!(cuts, [32, 72, 100]);
}

#[test]
fn at_the_cap_cuts_in_the_widest_pause() {
    // Pauses inside the cap at 10..14 (4 wide) and 20..27 (7 wide), both
    // before the target, then continuous speech.
    let config = config(3, 30, 40, 20, 2);
    let mut detector = FakeDetector::new(0, vec![(0, 10), (14, 20), (27, 200)], 3, 0);
    let samples = audio(0, 60, &detector);
    let chunks = run(config, 0, &samples, &mut detector, &[1]);
    assert_eq!(chunks[0].range(), range(0, 23));
    assert!(chunks[0].has_speech());
}

#[test]
fn at_the_cap_the_widest_pause_wins_even_if_earlier() {
    // Pauses at 8..16 (8 wide) and 22..26 (4 wide), both before the target.
    let config = config(3, 30, 40, 20, 2);
    let mut detector = FakeDetector::new(0, vec![(0, 8), (16, 22), (26, 200)], 3, 0);
    let samples = audio(0, 60, &detector);
    let chunks = run(config, 0, &samples, &mut detector, &[1]);
    assert_eq!(chunks[0].range(), range(0, 12));
}

#[test]
fn a_chunk_is_cut_as_soon_as_it_reaches_the_cap() {
    let config = config(3, 10, 40, 20, 2);
    let mut chunker = Chunker::new(config, SampleIndex::ZERO);
    let labels = Labels {
        segments: Vec::new(),
        silent_until: SampleIndex::ZERO,
    };
    assert!(chunker.push(&[0.5; 39], &labels).is_empty());
    let chunks = chunker.push(&[0.5], &labels);
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].range().end().get() <= 40);
}

#[test]
fn the_quietest_frame_is_by_energy_not_by_sum() {
    // Frames of 4 from 20: 20..24 swings hard but sums to zero; 28..32 is
    // steady and quiet. The quiet one wins.
    let config = config(3, 10, 40, 20, 4);
    let mut chunker = Chunker::new(config, SampleIndex::ZERO);
    let mut samples = vec![0.5_f32; 40];
    for (i, s) in samples[20..24].iter_mut().enumerate() {
        *s = if i % 2 == 0 { 0.9 } else { -0.9 };
    }
    for s in &mut samples[28..32] {
        *s = 0.1;
    }
    let chunks = chunker.push(&samples, &Labels::default());
    assert_eq!(chunks[0].range(), range(0, 30));
}

#[test]
fn settled_silence_at_the_end_is_not_speech() {
    let config = config(3, 10, 40, 20, 2);
    let mut chunker = Chunker::new(config, SampleIndex::ZERO);
    assert!(chunker.push(&[0.0; 5], &Labels::default()).is_empty());
    let settled = Labels {
        segments: Vec::new(),
        silent_until: SampleIndex::new(5),
    };
    let chunks = chunker.finish(&settled);
    assert_eq!(chunks.len(), 1);
    assert!(!chunks[0].has_speech());
    // Unsettled, the same audio might be speech.
    let mut chunker = Chunker::new(config, SampleIndex::ZERO);
    chunker.push(&[0.0; 5], &Labels::default());
    let unsettled = Labels {
        segments: Vec::new(),
        silent_until: SampleIndex::new(4),
    };
    assert!(chunker.finish(&unsettled)[0].has_speech());
}

#[test]
fn in_continuous_speech_cuts_at_the_quietest_frame_before_the_cap() {
    let config = config(3, 10, 40, 20, 4);
    let mut detector = FakeDetector::new(0, vec![(0, 1_000)], 3, 0);
    let mut samples = vec![0.5_f32; 100];
    // A dip at 26..30, inside the last 20 samples before the cap at 40.
    for s in &mut samples[26..30] {
        *s = 0.01;
    }
    // A deeper dip at 5..9 is before the window, so it's ignored.
    for s in &mut samples[5..9] {
        *s = 0.0;
    }
    let chunks = run(config, 0, &samples, &mut detector, &[100]);
    // Frames from 20: 20..24, 24..28, 28..32; the quietest is 24..28 (two
    // dipped samples) or 28..32 (two); the earlier wins, cut at its middle.
    assert_eq!(chunks[0].range(), range(0, 26));
}

#[test]
fn silence_is_cut_off_and_marked() {
    let config = config(3, 10, 40, 20, 2);
    let mut detector = FakeDetector::new(1_000, vec![(1_050, 1_060)], 3, 2);
    let samples = audio(1_000, 100, &detector);
    let chunks = run(config, 1_000, &samples, &mut detector, &[7]);
    assert!(!chunks[0].has_speech(), "{chunks:?}");
    assert!(chunks[0].range().end().get() <= 1_050);
    assert!(chunks.iter().any(Chunk::has_speech));
}

#[test]
fn unsettled_audio_counts_as_speech() {
    // The detector settles nothing (all of it is "maybe speech"); at the cap
    // the fallback cut must still be marked as speech.
    let config = config(3, 10, 40, 20, 2);
    let mut chunker = Chunker::new(config, SampleIndex::ZERO);
    let chunks = chunker.push(&[0.0; 45], &Labels::default());
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].has_speech());
}

#[test]
fn live_config_is_valid_at_any_rate() {
    for hz in [1, 2, 7, 8_000, 16_000, 48_000, 1_000_000] {
        let rate = nota_core::SampleRate::new(hz).unwrap();
        let live = ChunkerConfig::live(rate);
        let again = ChunkerConfig::new(
            live.min_pause(),
            live.target(),
            live.cap(),
            live.fallback_window(),
            live.frame(),
        )
        .unwrap();
        assert_eq!(live, again, "{hz} Hz");
    }
    let live = ChunkerConfig::live(nota_core::SampleRate::SPEECH);
    assert_eq!(live.min_pause().get(), 2_400);
    assert_eq!(live.target().get(), 48_000);
    assert_eq!(live.cap().get(), 160_000);
    assert_eq!(live.fallback_window().get(), 128_000);
    assert_eq!(live.frame().get(), 480);
}

#[test]
fn config_refuses_out_of_range_values() {
    let c = |p, t, cap, w, f| {
        ChunkerConfig::new(
            SampleCount::new(p),
            SampleCount::new(t),
            SampleCount::new(cap),
            SampleCount::new(w),
            SampleCount::new(f),
        )
    };
    assert!(c(2, 2, 2, 2, 1).is_some());
    assert!(c(1, 2, 2, 2, 1).is_none(), "pause under 2");
    assert!(c(3, 2, 4, 2, 1).is_none(), "pause over target");
    assert!(c(2, 5, 4, 2, 1).is_none(), "target over cap");
    assert!(c(2, 2, 4, 5, 1).is_none(), "window over cap");
    assert!(c(2, 2, 4, 2, 3).is_none(), "frame over window");
    assert!(c(2, 2, 4, 2, 0).is_none(), "empty frame");
}

/// A config, the first sample, the length, the speech, the push sizes and
/// the detector's lag.
type Scenario = (ChunkerConfig, u64, u64, Vec<(u64, u64)>, Vec<usize>, u64);

fn scenario() -> impl Strategy<Value = Scenario> {
    (3_u64..12, 0_u64..30, 20_u64..120, 1_u64..6)
        .prop_flat_map(|(min_pause, extra_target, extra_cap, frame)| {
            let target = min_pause + extra_target;
            let cap = target + extra_cap;
            (
                Just((min_pause, target, cap, frame)),
                frame..=cap,
                0_u64..1_000_000,
                0_u64..600,
                prop::collection::vec((0_u64..600, 1_u64..150), 0..12),
                prop::collection::vec(1_usize..90, 1..8),
                0_u64..25,
            )
        })
        .prop_map(
            |((min_pause, target, cap, frame), window, first, len, raw, pushes, lag)| {
                let config = config(min_pause, target, cap, window, frame);
                let truth = raw
                    .into_iter()
                    .map(|(at, n)| (first + at, (first + at + n).min(first + len)))
                    .collect();
                (config, first, len, truth, pushes, lag)
            },
        )
}

proptest! {
    #[test]
    fn chunks_tile_the_stream(
        (config, first, len, truth, pushes, lag) in scenario()
    ) {
        let mut detector = FakeDetector::new(first, truth, config.min_pause().get(), lag);
        let samples = audio(first, len, &detector);
        let chunks = run(config, first, &samples, &mut detector, &pushes);

        let mut next = first;
        let mut joined = Vec::new();
        for chunk in &chunks {
            prop_assert_eq!(chunk.range().start().get(), next);
            prop_assert!(!chunk.range().is_empty());
            prop_assert!(chunk.range().len() <= config.cap());
            prop_assert_eq!(chunk.audio().len() as u64, chunk.range().len().get());
            joined.extend_from_slice(chunk.audio());
            next = chunk.range().end().get();
        }
        prop_assert_eq!(next, first + len);
        prop_assert_eq!(joined, samples);
    }

    #[test]
    fn speech_is_never_marked_silent(
        (config, first, len, truth, pushes, lag) in scenario()
    ) {
        let mut detector = FakeDetector::new(first, truth, config.min_pause().get(), lag);
        let samples = audio(first, len, &detector);
        let chunks = run(config, first, &samples, &mut detector, &pushes);
        for chunk in chunks {
            let (from, to) = (chunk.range().start().get(), chunk.range().end().get());
            if (from..to).any(|i| detector.in_speech(i)) {
                prop_assert!(chunk.has_speech(), "{:?}", chunk.range());
            }
        }
    }

    #[test]
    fn cuts_inside_speech_are_only_fallbacks(
        (config, first, len, truth, pushes, lag) in scenario()
    ) {
        let mut detector = FakeDetector::new(first, truth, config.min_pause().get(), lag);
        let samples = audio(first, len, &detector);
        let chunks = run(config, first, &samples, &mut detector, &pushes);
        let last = chunks.len().saturating_sub(1);
        for (k, chunk) in chunks.iter().enumerate() {
            let cut = chunk.range().end().get();
            // The last chunk ends where the stream does, which isn't a cut.
            if k == last || !detector.in_speech(cut) || !detector.in_speech(cut - 1) {
                continue;
            }
            prop_assert!(
                chunk.range().len().get() >= config.cap().get() - config.fallback_window().get(),
                "{:?} cut inside speech before the fallback window", chunk.range()
            );
        }
    }

    /// When every stretch of speech is short enough that a settled pause
    /// always follows before the cap, no cut lands in speech.
    #[test]
    fn short_speech_is_never_cut(
        (config, first, len, truth, pushes, lag) in short_speech()
    ) {
        let mut detector = FakeDetector::new(first, truth, config.min_pause().get(), lag);
        let samples = audio(first, len, &detector);
        let chunks = run(config, first, &samples, &mut detector, &pushes);
        let last = chunks.len().saturating_sub(1);
        for (k, chunk) in chunks.iter().enumerate() {
            let cut = chunk.range().end().get();
            if k < last {
                prop_assert!(
                    !(detector.in_speech(cut) && detector.in_speech(cut - 1)),
                    "cut at {cut} inside speech {:?}", detector.speech
                );
            }
        }
    }
}

/// Speech in segments of at most `longest` samples with pauses of at least
/// `min_pause` between them, and a cap with room for a segment that starts
/// just after the target, its pause, the detector's lag and one push.
fn short_speech() -> impl Strategy<Value = Scenario> {
    (
        3_u64..12,
        0_u64..30,
        1_u64..60,
        1_u64..6,
        0_u64..25,
        prop::collection::vec(1_usize..90, 1..8),
        0_u64..30,
    )
        .prop_flat_map(
            |(min_pause, extra_target, longest, frame, lag, pushes, slack)| {
                let target = min_pause + extra_target;
                let biggest_push = pushes.iter().copied().max().unwrap_or(1) as u64;
                let cap = target + longest + 2 * min_pause + lag + biggest_push + slack;
                (
                    Just((min_pause, target, cap, frame, lag, pushes)),
                    frame..=cap,
                    0_u64..1_000_000,
                    prop::collection::vec((min_pause..80, 1..=longest), 0..12),
                )
            },
        )
        .prop_map(
            |((min_pause, target, cap, frame, lag, pushes), window, first, runs)| {
                let mut truth = Vec::new();
                let mut at = first;
                for (gap, len) in runs {
                    at += gap;
                    truth.push((at, at + len));
                    at += len;
                }
                let len = at + min_pause - first;
                (
                    config(min_pause, target, cap, window, frame),
                    first,
                    len,
                    truth,
                    pushes,
                    lag,
                )
            },
        )
}
