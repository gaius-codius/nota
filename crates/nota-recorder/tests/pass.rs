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
use nota_core::{Clock, EpochId, SampleCount, SampleIndex, SampleRange, SystemClock, TrackId};
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
    // A pass that stops making progress fails its test quickly.
    config.stall = Duration::from_secs(5);
    config
}

/// The fake answering every two frames a track, and at each flush.
const ECHO: &[&str] = &["echo", "--every", "2"];

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
    /// The last point confirmed, on any track.
    latest: Rc<Cell<u64>>,
    /// What the pass said of the engine being down, in order.
    downs: Rc<std::cell::RefCell<Vec<Option<String>>>>,
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
        self.latest.set(up_to.get());
        self.calls.set(self.calls.get() + 1);
        Ok(())
    }

    fn engine_down(&mut self, why: Option<&str>) {
        self.downs.borrow_mut().push(why.map(str::to_owned));
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
    let end = run(&config(ECHO), &mut sink, &|| false);
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
    // Frames end on whole seconds and at segments' ends; each run is
    // flushed at its end (the gap, the epoch change at 7 s, the last
    // sample), and only there, so the fake's answers to every two frames
    // run on across the segment boundary at 5 s.
    let ranges: Vec<(TrackId, u64, u64)> = sink.texts.iter().map(|t| (t.0, t.1, t.2)).collect();
    let mic = [
        (0, 2 * S),
        (2 * S, 4 * S),
        (4 * S, 6 * S),
        (6 * S, 8 * S),
        (8 * S, 10 * S),
        (10 * S, 12 * S),
        (12 * S, 12 * S + S / 2),
        (20 * S, 22 * S),
        (22 * S, 23 * S + S * 3 / 10),
    ]
    .map(|(a, b)| (MIC, a, b));
    let system = [
        (0, 2 * S),
        (2 * S, 4 * S),
        (4 * S, 6 * S),
        (6 * S, 7 * S),
        (7 * S, 9 * S),
    ]
    .map(|(a, b)| (SYSTEM, a, b));
    assert_eq!(ranges, [&mic[..], &system[..]].concat());
}

/// The final pass's config: 1 s frames, a minute ahead at most, 25 s
/// skipped on poison, a minute per request, no idle flush, a short stop.
#[test]
fn the_final_pass_config() {
    let config = PassConfig::final_pass(EngineCommand {
        program: PathBuf::from("nota"),
        args: Vec::new(),
    });
    assert_eq!(config.frame.get(), S);
    assert_eq!(config.in_flight.get(), 60 * S);
    assert_eq!(config.stall, Duration::from_mins(10));
    assert_eq!(config.engine.poison_skip.get(), 25 * S);
    assert_eq!(config.engine.request_timeout, Duration::from_mins(1));
    assert_eq!(config.engine.idle_flush, Duration::from_hours(24));
    assert_eq!(config.engine.shutdown_wait, Duration::ZERO);
}

/// Audio goes no further ahead of what's confirmed than the config says:
/// with one second in flight, each one-second segment is read only once
/// the one before it is confirmed.
#[test]
fn the_pass_stays_within_its_window() {
    let segments: Vec<SegmentRow> = (0..6).map(|k| row(MIC, 0, k * S, (k + 1) * S)).collect();
    let tracks = [TrackAudio {
        track: MIC,
        segments,
        from: SampleIndex::ZERO,
    }];
    let mut config = config(&["echo"]);
    config.in_flight = SampleCount::new(S);
    let mut sink = Committed::default();
    let seen = Rc::clone(&sink.latest);
    let reads = std::cell::RefCell::new(Vec::new());
    let end = transcribe(
        &tracks,
        |row| {
            reads
                .borrow_mut()
                .push((row.range().start().get(), seen.get()));
            Ok(audio(row))
        },
        &config,
        &clock(),
        &mut sink,
        &|| false,
    );
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    // Segment k is read when exactly k seconds are confirmed.
    let want: Vec<(u64, u64)> = (0..6).map(|k| (k * S, k * S)).collect();
    assert_eq!(*reads.borrow(), want);
}

/// How a pass ended, in words.
#[test]
fn a_pass_says_how_it_ended() {
    let said = |end: PassEnd<String>| end.to_string();
    assert_eq!(said(PassEnd::Done), "done");
    assert_eq!(said(PassEnd::Stopped), "stopped");
    assert_eq!(
        said(PassEnd::Stalled(Some("exited".into()))),
        "the transcriber stopped working: exited"
    );
    assert_eq!(
        said(PassEnd::CantStart("couldn't start: not found".into())),
        "the transcriber stopped working: couldn't start: not found"
    );
    assert_eq!(
        said(PassEnd::Stalled(None)),
        "the transcriber stopped answering"
    );
    assert_eq!(
        said(PassEnd::Sink("the disk is full".into())),
        "the disk is full"
    );
    assert_eq!(
        said(PassEnd::Engine(std::io::Error::other("no such file"))),
        "the transcriber couldn't be started: no such file"
    );
}

/// Acceptance (GAI-317): an engine killed mid-pass, again and again,
/// loses nothing: the pass resumes and its text is the uninterrupted
/// pass's.
#[test]
fn an_engine_killed_mid_pass_loses_nothing_and_matches_an_uninterrupted_pass() {
    let reference = uninterrupted();
    let mut sink = Committed::default();
    let end = run(
        &config(&["crash-after", "--after", "7", "--every", "2"]),
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
    for stop_after in [2, 5, 9] {
        let calls = Rc::clone(&sink.calls);
        let end = run(&config(ECHO), &mut sink, &move || calls.get() >= stop_after);
        assert!(matches!(end, PassEnd::Stopped), "{end:?}");
    }
    let end = run(&config(ECHO), &mut sink, &|| false);
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    assert_eq!(sink.texts, reference.texts);
    assert_eq!(sink.progress, reference.progress);
}

/// A pass whose engine never answers gives up once nothing has been
/// confirmed for its stall time, saying why the engine was down: it stopped
/// answering, or (if the stall comes while a restarted engine is up) no
/// reason, which reads the same.
#[test]
fn a_pass_that_gets_no_answers_gives_up() {
    let mut config = config(&["hang"]);
    config.engine.request_timeout = Duration::from_millis(300);
    config.stall = Duration::from_secs(2);
    let mut sink = Committed::default();
    match run(&config, &mut sink, &|| false) {
        PassEnd::Stalled(Some(why)) => assert_eq!(why, "stopped answering"),
        PassEnd::Stalled(None) => {}
        other => panic!("{other:?}"),
    }
    // It was down, each time, for not answering.
    let downs = sink.downs.borrow();
    assert!(downs.contains(&Some(
        "the transcriber stopped working: stopped answering".to_owned()
    )));
    assert!(
        downs
            .iter()
            .flatten()
            .all(|why| why == "the transcriber stopped working: stopped answering")
    );
    assert!(sink.texts.is_empty());
}

/// Acceptance (#237): an engine program that's gone ends the pass at its
/// first failed spawn, with the reason the spawn gave, not after the
/// stall.
#[test]
fn a_missing_engine_program_ends_the_pass_at_once_with_why() {
    let mut config = config(&["echo"]);
    config.engine.command.program = PathBuf::from("/nonexistent/nota-engine");
    // Far beyond the test's run: only the early end can end it in time.
    config.stall = Duration::from_mins(10);
    let mut sink = Committed::default();
    let why = format!("couldn't start: {}", std::io::Error::from_raw_os_error(2));
    match run(&config, &mut sink, &|| false) {
        PassEnd::CantStart(said) => assert_eq!(said, why),
        other => panic!("{other:?}"),
    }
    // One spawn, one failure: no restart was waited for.
    assert_eq!(
        *sink.downs.borrow(),
        [Some(format!("the transcriber stopped working: {why}"))]
    );
    assert!(sink.texts.is_empty());
}

/// An engine that exits as it starts, with what a model that doesn't load
/// writes to stderr.
fn exits_at_start(config: &mut PassConfig) {
    config.engine.command.program = PathBuf::from("/bin/sh");
    config.engine.command.args = [
        "-c",
        "echo loading >&2; echo 'nota engine asr: no model at /m' >&2; exit 1",
    ]
    .iter()
    .map(OsString::from)
    .collect();
}

/// Acceptance (#237): engines that exit before hello (a model that doesn't
/// load) end the pass at the third in a row, with the last line the last
/// one wrote to stderr, not after the stall.
#[test]
fn an_engine_that_exits_at_start_three_times_ends_the_pass_with_its_last_error() {
    let mut config = config(&[]);
    exits_at_start(&mut config);
    // Far beyond the test's run: only the early end can end it in time.
    config.stall = Duration::from_mins(10);
    let mut sink = Committed::default();
    let why = "exited (exit status: 1): nota engine asr: no model at /m";
    match run(&config, &mut sink, &|| false) {
        PassEnd::CantStart(said) => assert_eq!(said, why),
        other => panic!("{other:?}"),
    }
    // Three engines were tried, and the pass ended at the third.
    let down = Some(format!("the transcriber stopped working: {why}"));
    assert_eq!(*sink.downs.borrow(), [down.clone(), down.clone(), down]);
    assert!(sink.texts.is_empty());
}

/// Acceptance (#237): an engine that says hello and then dies, every time,
/// isn't given up on early: it started, so its audio is struck and skipped
/// as before and the pass goes on.
#[test]
fn an_engine_that_dies_after_each_hello_does_not_end_the_pass_early() {
    // Every engine dies on the first audio it's sent.
    let mut sink = Committed::default();
    let end = run(
        &config(&["crash-after", "--after", "0"]),
        &mut sink,
        &|| false,
    );
    assert!(!matches!(end, PassEnd::CantStart(_)), "{end:?}");
    // More engines died than the early end allows before hello.
    let deaths = sink
        .downs
        .borrow()
        .iter()
        .flatten()
        .filter(|why| why.starts_with("the transcriber stopped working: exited"))
        .count();
    assert!(deaths > 3, "{:?}", sink.downs.borrow());
}

/// Acceptance (#224): the sink hears why the engine is down while the
/// pass runs, in the words its stall would give, not only once it ends.
#[test]
fn a_pass_says_why_its_engine_is_down_before_it_ends() {
    let mut config = config(&[]);
    // One exit at start leaves the pass running, so it's still going when
    // the sink first hears why.
    exits_at_start(&mut config);
    let mut sink = Committed::default();
    let downs = Rc::clone(&sink.downs);
    let end = run(&config, &mut sink, &move || !downs.borrow().is_empty());
    assert!(matches!(end, PassEnd::Stopped), "{end:?}");
    assert_eq!(
        sink.downs.borrow()[0],
        Some(
            "the transcriber stopped working: exited (exit status: 1): \
             nota engine asr: no model at /m"
                .to_owned()
        )
    );
}

/// Acceptance (#224): an engine that comes back up after being down
/// takes its reason with it: the sink hears it's back, and a stall after
/// that says only that it stopped answering.
#[test]
fn an_engine_back_up_no_longer_gives_its_old_reason() {
    // The first engine exits at start; the second answers hello and then
    // never confirms anything.
    let marker = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pass-back-up-{}", std::process::id()));
    // One left by a run killed between the two starts would skip the
    // first failure.
    let cleared = std::process::Command::new("/bin/rm")
        .arg("-f")
        .arg(&marker)
        .status()
        .unwrap();
    assert!(cleared.success());
    let mut config = config(&[]);
    config.engine.command.program = PathBuf::from("/bin/sh");
    config.engine.command.args = [
        "-c",
        r#"if [ -e "$1" ]; then rm -f "$1"; exec "$2" hang; fi
           : > "$1"; echo 'first try fails' >&2; exit 1"#,
        "sh",
    ]
    .iter()
    .map(OsString::from)
    .chain([
        marker.into_os_string(),
        OsString::from(env!("CARGO_BIN_EXE_nota-fake-engine")),
    ])
    .collect();
    // The stall must outlast both starts, and come long before the second
    // engine's request times out.
    config.engine.request_timeout = Duration::from_secs(60);
    config.stall = Duration::from_secs(5);
    let mut sink = Committed::default();
    match run(&config, &mut sink, &|| false) {
        PassEnd::Stalled(None) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(
        *sink.downs.borrow(),
        [
            Some(
                "the transcriber stopped working: exited (exit status: 1): first try fails"
                    .to_owned()
            ),
            None
        ]
    );
}

/// An engine that's never down says nothing of it, so whatever the sink
/// kept from an earlier pass stays until the engine confirms audio.
#[test]
fn an_engine_never_down_says_nothing_of_it() {
    let sink = uninterrupted();
    assert!(sink.downs.borrow().is_empty());
}

/// A segment that can't be read is skipped, once everything before it is
/// confirmed, and the rest of the session is transcribed as before.
#[test]
fn a_segment_that_cant_be_read_is_skipped_and_the_rest_done() {
    let reference = uninterrupted();
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
        &config(ECHO),
        &clock(),
        &mut sink,
        &|| false,
    );
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    assert_eq!(sink.skipped, [(MIC, 5 * S, 12 * S + S / 2)]);
    // Everything else as the uninterrupted pass has it, but the run ends
    // where the unreadable segment starts.
    let kept: Vec<_> = reference
        .texts
        .iter()
        .filter(|t| t.0 != MIC || t.2 <= 4 * S || t.1 >= 20 * S)
        .cloned()
        .collect();
    let mut want = kept;
    want.insert(2, (MIC, 4 * S, 5 * S, format!("{}-{}", 4 * S, 5 * S)));
    assert_eq!(sink.texts, want);
    assert_eq!(sink.progress, reference.progress);
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

/// Rows that overlap (the store refuses them, but the pass takes any)
/// don't hang or panic the pass: each sample is sent once.
#[test]
fn overlapping_rows_send_each_sample_once() {
    let tracks = [TrackAudio {
        track: MIC,
        segments: vec![row(MIC, 0, 0, 3 * S), row(MIC, 0, 2 * S, 4 * S)],
        from: SampleIndex::ZERO,
    }];
    let mut sink = Committed::default();
    let end = transcribe(
        &tracks,
        |row| Ok(audio(row)),
        &config(&["echo"]),
        &clock(),
        &mut sink,
        &|| false,
    );
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    assert_eq!(
        covered(&sink.texts),
        BTreeMap::from([(MIC, vec![(0, 4 * S)])])
    );
}

/// Audio that kills every engine is skipped (25 s at most, here what's
/// left of its run), stored as skipped, and the pass goes on to the end.
#[test]
fn audio_that_keeps_killing_the_engine_is_skipped_and_the_rest_done() {
    let reference = uninterrupted();
    let tracks = tracks(&BTreeMap::new());
    // The system track's first second holds the sample value the fake
    // dies on.
    let mut sink = Committed::default();
    let end = transcribe(
        &tracks,
        |row| {
            let mut samples = audio(row);
            if row.track() == SYSTEM && row.range().start().get() == 0 {
                samples[100] = 1_002;
            }
            Ok(samples)
        },
        &config(&["echo", "--every", "2", "--poison", "1002"]),
        &clock(),
        &mut sink,
        &|| false,
    );
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    // One 25 s skip takes everything queued from the poison on: the
    // system track's whole 9 s, across its epoch change.
    assert_eq!(sink.skipped, [(SYSTEM, 0, 9 * S)]);
    let rest: Vec<_> = reference
        .texts
        .iter()
        .filter(|t| t.0 == MIC)
        .cloned()
        .collect();
    assert_eq!(sink.texts, rest);
    assert_eq!(sink.progress, reference.progress);
}

/// An engine that hangs partway is killed at the request timeout and
/// restarted, and the pass resumes to the uninterrupted pass's text.
#[test]
fn an_engine_that_hangs_mid_pass_is_restarted_and_matches_an_uninterrupted_pass() {
    let reference = uninterrupted();
    let mut config = config(&["hang-after", "--after", "9", "--every", "2"]);
    config.engine.request_timeout = Duration::from_millis(300);
    let mut sink = Committed::default();
    let end = run(&config, &mut sink, &|| false);
    assert!(matches!(end, PassEnd::Done), "{end:?}");
    assert!(sink.skipped.is_empty());
    assert_eq!(sink.texts, reference.texts);
    assert_eq!(sink.progress, reference.progress);
}
