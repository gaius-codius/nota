//! A pass over published audio ([`nota_recorder::engine::pass`]) against
//! `nota-fake-engine` in its `echo` mode, which answers every frame with
//! its range as text: the text tiles every published sample once, on both
//! tracks, however the engine dies or the pass is stopped on the way.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use nota_core::messages::Transcript;
use nota_core::{Clock, EpochId, SampleIndex, SampleRange, SystemClock, TrackId};
use nota_recorder::engine::EngineCommand;
use nota_recorder::engine::pass::{PassConfig, PassEnd, PassSink, TrackAudio, transcribe};
use nota_recorder::segment::ReadSegmentError;
use nota_store::{SegmentRow, Sha256Digest};

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);
/// A second at 16 kHz.
const S: u64 = 16_000;

fn config(args: &[&str]) -> PassConfig {
    let mut config = PassConfig::final_pass(EngineCommand {
        program: PathBuf::from(env!("CARGO_BIN_EXE_nota-fake-engine")),
        args: args.iter().map(OsString::from).collect(),
    });
    config.engine.initial_backoff = Duration::from_millis(20);
    config.engine.max_backoff = Duration::from_millis(200);
    config.engine.start_timeout = Duration::from_secs(10);
    config.engine.request_timeout = Duration::from_secs(10);
    config
}

fn row(track: TrackId, epoch: u32, from: u64, to: u64) -> SegmentRow {
    SegmentRow::new(
        track,
        EpochId::new(epoch),
        SampleRange::new(SampleIndex::new(from), SampleIndex::new(to)).unwrap(),
        Sha256Digest::new([0; 32]),
    )
    .unwrap()
}

/// The mic: 12.5 s in two segments of one run, a gap, then 3.3 s in a
/// new epoch. The system audio: 7 s, then 2 s in a new epoch that follows
/// on without a gap.
fn published() -> Vec<Vec<SegmentRow>> {
    vec![
        vec![
            row(MIC, 0, 0, 5 * S),
            row(MIC, 0, 5 * S, 12 * S + S / 2),
            row(MIC, 1, 20 * S, 23 * S + S * 3 / 10),
        ],
        vec![row(SYSTEM, 0, 0, 7 * S), row(SYSTEM, 1, 7 * S, 9 * S)],
    ]
}

fn tracks(from: &BTreeMap<TrackId, u64>) -> Vec<TrackAudio> {
    published()
        .into_iter()
        .map(|segments| {
            let track = segments[0].track();
            TrackAudio {
                track,
                segments,
                from: SampleIndex::new(from.get(&track).copied().unwrap_or(0)),
            }
        })
        .collect()
}

/// A segment's audio: values under 1000, which the fake never dies on.
fn audio(row: &SegmentRow) -> Vec<i16> {
    (row.range().start().get()..row.range().end().get())
        .map(|i| i16::try_from(i % 997).unwrap())
        .collect()
}

/// What the pass committed, as a store would hold it.
#[derive(Debug, Default)]
struct Committed {
    /// Track, start, end, text.
    texts: Vec<(TrackId, u64, u64, String)>,
    skipped: Vec<(TrackId, u64, u64)>,
    progress: BTreeMap<TrackId, u64>,
    calls: Rc<Cell<usize>>,
}

impl PassSink for Committed {
    type Error = String;

    fn confirmed(
        &mut self,
        track: TrackId,
        up_to: SampleIndex,
        texts: Vec<Transcript>,
        skipped: Vec<SampleRange>,
    ) -> Result<(), String> {
        for text in texts {
            assert_eq!(text.track(), track);
            assert!(text.range().end() <= up_to);
            self.texts.push((
                track,
                text.range().start().get(),
                text.range().end().get(),
                text.text().to_owned(),
            ));
        }
        for range in skipped {
            self.skipped
                .push((track, range.start().get(), range.end().get()));
        }
        let progress = self.progress.entry(track).or_default();
        assert!(up_to.get() >= *progress, "progress goes forward");
        *progress = up_to.get();
        self.calls.set(self.calls.get() + 1);
        Ok(())
    }
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::start().unwrap())
}

fn run(config: &PassConfig, sink: &mut Committed, stop: &dyn Fn() -> bool) -> PassEnd<String> {
    let tracks = tracks(&sink.progress);
    transcribe(&tracks, |row| Ok(audio(row)), config, &clock(), sink, stop)
}

fn uninterrupted() -> Committed {
    let mut sink = Committed::default();
    let end = run(&config(&["echo"]), &mut sink, &|| false);
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    sink
}

/// The text's runs, joined where they meet, by track.
fn covered(texts: &[(TrackId, u64, u64, String)]) -> BTreeMap<TrackId, Vec<(u64, u64)>> {
    let mut sorted = texts.to_vec();
    sorted.sort();
    let mut out: BTreeMap<TrackId, Vec<(u64, u64)>> = BTreeMap::new();
    for (track, from, to, text) in sorted {
        assert_eq!(text, format!("{from}-{to}"), "text is its own range");
        let runs = out.entry(track).or_default();
        match runs.last_mut() {
            Some(last) if last.1 == from => last.1 = to,
            Some(last) => {
                assert!(last.1 < from, "{track:?} {last:?} overlaps {from}");
                runs.push((from, to));
            }
            None => runs.push((from, to)),
        }
    }
    out
}

/// Acceptance (GAI-317): the final pass covers every published sample
/// exactly once, on both tracks, and each run ends in a chunk of its own.
#[test]
fn every_published_sample_is_transcribed_once_on_both_tracks() {
    let sink = uninterrupted();
    assert_eq!(
        covered(&sink.texts),
        BTreeMap::from([
            (
                MIC,
                vec![(0, 12 * S + S / 2), (20 * S, 23 * S + S * 3 / 10)]
            ),
            (SYSTEM, vec![(0, 9 * S)]),
        ])
    );
    assert!(sink.skipped.is_empty());
    assert_eq!(
        sink.progress,
        BTreeMap::from([(MIC, 23 * S + S * 3 / 10), (SYSTEM, 9 * S)])
    );
    // No frame crosses a run's end (the epoch change at 7 s included):
    // runs are flushed apart.
    for (track, from, to, _) in &sink.texts {
        if *track == SYSTEM {
            assert!(*to <= 7 * S || *from >= 7 * S, "{from}..{to}");
        }
    }
}

/// Acceptance (GAI-317): an engine killed mid-pass, again and again,
/// loses nothing: the pass resumes and its text is the uninterrupted
/// pass's.
#[test]
fn an_engine_killed_mid_pass_loses_nothing_and_matches_an_uninterrupted_pass() {
    let reference = uninterrupted();
    let mut sink = Committed::default();
    let end = run(
        &config(&["crash-after", "--after", "7"]),
        &mut sink,
        &|| false,
    );
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    assert!(sink.skipped.is_empty());
    assert_eq!(sink.texts, reference.texts);
    assert_eq!(sink.progress, reference.progress);
}

/// A pass stopped partway (a recording starting, nota closing) carries on
/// from what was committed, and ends with the uninterrupted pass's text,
/// each sample once.
#[test]
fn a_pass_stopped_partway_resumes_to_the_same_result() {
    let reference = uninterrupted();
    let mut sink = Committed::default();
    for stop_after in [3, 9, 20] {
        let calls = Rc::clone(&sink.calls);
        let end = run(&config(&["echo"]), &mut sink, &move || {
            calls.get() >= stop_after
        });
        assert!(matches!(end, PassEnd::Stopped), "{end:?}");
    }
    let end = run(&config(&["echo"]), &mut sink, &|| false);
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    assert_eq!(sink.texts, reference.texts);
    assert_eq!(sink.progress, reference.progress);
}

/// A pass whose engine never answers gives up once nothing has been
/// confirmed for its stall time, saying why the engine was down.
#[test]
fn a_pass_that_gets_no_answers_gives_up() {
    let mut config = config(&["hang"]);
    config.engine.request_timeout = Duration::from_millis(300);
    config.stall = Duration::from_secs(2);
    let mut sink = Committed::default();
    match run(&config, &mut sink, &|| false) {
        PassEnd::Stalled(Some(why)) => assert_eq!(why, "stopped answering"),
        other => panic!("{other:?}"),
    }
    assert!(sink.texts.is_empty());
}

/// A segment that can't be read ends the pass, naming it; what came
/// before it was committed.
#[test]
fn a_segment_that_cant_be_read_ends_the_pass() {
    let tracks = tracks(&BTreeMap::new());
    let bad = tracks[0].segments[1];
    let mut sink = Committed::default();
    let end = transcribe(
        &tracks,
        |row| {
            if *row == bad {
                Err(ReadSegmentError::Hash)
            } else {
                Ok(audio(row))
            }
        },
        &config(&["echo"]),
        &clock(),
        &mut sink,
        &|| false,
    );
    match end {
        PassEnd::Segment(row, ReadSegmentError::Hash) => assert_eq!(row, bad),
        other => panic!("{other:?}"),
    }
    assert!(sink.progress.get(&MIC).is_none_or(|&p| p <= 5 * S));
}

/// A pass with nothing left to do is done without starting an engine.
#[test]
fn a_pass_with_nothing_left_starts_no_engine() {
    let mut config = config(&["echo"]);
    config.engine.command.program = PathBuf::from("/nonexistent/nota-engine");
    let mut sink = Committed {
        progress: BTreeMap::from([(MIC, 24 * S), (SYSTEM, 9 * S)]),
        ..Committed::default()
    };
    let end = run(&config, &mut sink, &|| false);
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    assert_eq!(sink.calls.get(), 0);
}
