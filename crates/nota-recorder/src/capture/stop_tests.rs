//! Two tracks recorded together, stopped anywhere: as when SIGHUP or
//! SIGTERM arrives, the streams stop, the recorder records what they sent,
//! the writer finishes and the last journals are published. Then the next
//! start's salvage runs.
//!
//! The recorder handles a stop between events: a signal that arrives while
//! an operation is under way is acted on once the event it belongs to is
//! recorded. So stopping before each event covers a stop during each of
//! the recording's operations, and a stop is also tried right after each
//! operation fails (a failpoint at every operation).
//!
//! The events are handled on the test's thread, through the recorder's own
//! [`handle`] and [`settle`], with publishing in step, so every run is
//! deterministic. The stop is the app's own,
//! [`Publisher::finish_recording`], on a publisher started at the stop and
//! given what failed to publish before with the last journals, as the
//! app's would retry it: the writer finishes before the publisher starts
//! work, so that stays deterministic too.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRate, SessionId, SessionTime,
    TrackId, TrackTimeline,
};
use nota_store::SegmentRow;

use super::*;
use crate::fs::Fs;
use crate::fs::crash::{CrashCase, CrashTest};
use crate::fs::fake::{CrashOutcome, FakeFs};
use crate::segment::{
    FakeStore, Publisher, SegmentLength, SegmentStore, needs_salvage, publish_journals, salvage,
    segment_file_name,
};
use crate::session::{FinishedJournal, SessionDir, SessionStore};

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);
const SESSION: SessionId = SessionId::new(1);

/// A second is 1,000 samples.
fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

/// A second and a half per window, so journals rotate and publish live.
fn length() -> SegmentLength {
    SegmentLength::new(SampleCount::new(1_500)).unwrap()
}

fn session() -> PathBuf {
    PathBuf::from("/session")
}

fn db() -> PathBuf {
    PathBuf::from("/db")
}

/// The most audio a track may lose here: 1.1 s, the journal's 850 ms sync
/// interval with room for a chunk being written when the process dies.
/// Stricter than the bounded-loss rule (about 2 s on a quiet disk), which
/// also allows for a slow fsync; these tests' fsyncs are instant.
const LOSS_LIMIT: u64 = 1_100; // check-bound

/// The sample a test track holds at `index`: distinct per track and
/// position, so misplaced audio shows.
fn sample(track: TrackId, index: u64) -> i16 {
    let v = index
        .wrapping_mul(31)
        .wrapping_add(u64::from(track.get()) * 7_919);
    i16::from_le_bytes([v.to_le_bytes()[0], v.to_le_bytes()[1]])
}

/// One step of the script.
#[derive(Debug, Clone)]
enum Step {
    /// The next audio on a track, `len` samples.
    Audio(TrackId, u64),
    /// An overrun on a track, reported now.
    Overrun(TrackId),
    /// The session clock moves on.
    Advance(u64),
}

/// About eight seconds of both tracks, in chunks of uneven size, with an
/// overrun on each track (after a stall, so each leaves a gap) and the
/// tracks' chunks interleaved as two callbacks would be.
fn script() -> Vec<Step> {
    let sizes = [120, 250, 60, 200, 90];
    let mut steps = Vec::new();
    for round in 0..40 {
        let len = sizes[round % sizes.len()];
        steps.push(Step::Audio(MIC, len));
        if round == 12 {
            steps.push(Step::Advance(300));
            steps.push(Step::Overrun(MIC));
        }
        steps.push(Step::Audio(SYSTEM, len));
        if round == 25 {
            steps.push(Step::Advance(200));
            steps.push(Step::Overrun(SYSTEM));
        }
        steps.push(Step::Advance(len));
    }
    steps
}

/// What a recording promised before it stopped, and what it reported.
#[derive(Debug, Default, Clone)]
struct Promised {
    /// Per track, the end of the audio the recorder handled.
    captured: BTreeMap<TrackId, u64>,
    /// Per track, the furthest the writer said was durable, up to the
    /// first journal failure: after it, a gap may come before what a new
    /// journal holds, so the writer's word covers only that journal.
    durable: BTreeMap<TrackId, u64>,
    /// The timelines as the recorder left them.
    timelines: Vec<TrackTimeline>,
    /// Journal failures reported.
    failures: usize,
    /// Whether the writer finished and its last journals were published.
    finalised: bool,
}

impl Promised {
    fn note<S: Fs>(&mut self, writer: &SessionWriter<S>) {
        if self.failures > 0 {
            return;
        }
        for track in [MIC, SYSTEM] {
            if let Some(d) = writer.durable(track) {
                let at = self.durable.entry(track).or_default();
                *at = (*at).max(d.end().get());
            }
        }
    }
}

/// How a run goes.
#[derive(Debug, Clone, Copy, Default)]
struct Run {
    /// Stop before the first event that starts after this many operations.
    stop_after_ops: Option<usize>,
    /// Fail this operation (counting from the start), without crashing.
    fail_at: Option<usize>,
}

/// Records the script until the stop, then finishes as the app does on a
/// signal: the streams stop, and [`Publisher::finish_recording`] finishes
/// the writer and publishes every finished journal. Stops at the first
/// error the app would stop at.
fn record(fs: &FakeFs, run: Run) -> Promised {
    let mut promised = Promised::default();
    if let Some(at) = run.fail_at {
        fs.fail_after(at, io::ErrorKind::Other);
    }
    let _ = record_into(fs, run, &mut promised);
    promised
}

fn record_into(fs: &FakeFs, run: Run, promised: &mut Promised) -> Result<(), String> {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let dyn_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    let lock = SessionDir::new(SESSION, fs.clone(), &session())
        .lock()
        .map_err(|e| e.to_string())?;
    let mut writer =
        SessionWriter::open(&lock, rate(), length(), dyn_clock).map_err(|e| e.to_string())?;
    let mut store = SessionStore::new(lock, FakeStore::new(fs, &db()));
    let mut timelines = Vec::new();
    for track in [MIC, SYSTEM] {
        writer
            .start_track(
                track,
                &writer.test_epoch(track, EpochId::new(0), SampleIndex::ZERO),
            )
            .map_err(|e| e.to_string())?;
        let mut timeline = TrackTimeline::new(track);
        timeline
            .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
            .map_err(|e| e.to_string())?;
        timelines.push(timeline);
        promised.captured.insert(track, 0);
        promised.durable.insert(track, 0);
    }
    promised.timelines.clone_from(&timelines);
    let mut pending: Vec<FinishedJournal> = Vec::new();
    for step in script() {
        if run.stop_after_ops.is_some_and(|k| fs.attempted() > k) {
            break;
        }
        let (track, event) = match step {
            Step::Advance(ms) => {
                clock.advance(Duration::from_millis(ms));
                continue;
            }
            Step::Audio(track, len) => {
                let from = promised.captured[&track];
                let audio = (from..from + len).map(|i| sample(track, i)).collect();
                promised.captured.insert(track, from + len);
                (track, CaptureEvent::Audio(audio))
            }
            Step::Overrun(track) => (
                track,
                CaptureEvent::Notice {
                    notice: CaptureNotice::Overrun,
                    at: clock.now(),
                },
            ),
        };
        let mut finished = Vec::new();
        let mut failures = 0;
        let mut report = |_: Option<TrackId>, e: RecorderEvent| match e {
            RecorderEvent::Finished(j) => finished.extend(j),
            RecorderEvent::JournalFailed(_) => failures += 1,
            _ => {}
        };
        let mut given = Timelines {
            given: &mut timelines,
            joined: Vec::new(),
            stamps: BTreeMap::new(),
        };
        let handled = handle(&mut writer, &mut given, track, event, &mut report)
            .map_err(|e| e.to_string())?;
        if let Handled::Recorded(outcome, _) = handled {
            settle(&mut writer, outcome, &mut report).map_err(|e| e.to_string())?;
        }
        promised.note(&writer);
        promised.failures += failures;
        promised.timelines.clone_from(&timelines);
        if fs.has_crashed() {
            // The process is dead: nothing more happens.
            return Err("crashed".into());
        }
        pending.extend(finished);
        if !pending.is_empty() {
            publish(&mut store, &mut pending);
        }
    }

    // The stop. The app's publisher would hold what failed to publish so
    // far, and try it again with the last journals.
    let ends: Vec<_> = [MIC, SYSTEM]
        .into_iter()
        .map(|t| (t, writer.next_sample(t)))
        .collect();
    let publisher = Publisher::spawn(store, length()).map_err(|e| e.to_string())?;
    let stopped = publisher.finish_recording_after(pending, writer, || {});
    match stopped.finishing {
        None if promised.failures == 0 => {
            for (track, end) in ends {
                if let Some(end) = end {
                    promised.durable.insert(track, end.get());
                }
            }
        }
        None => {}
        Some(_) => promised.failures += 1,
    }
    promised.finalised = stopped.published.is_ok_and(|report| report.is_complete());
    Ok(())
}

/// One publish run as the [`Publisher`](crate::segment::Publisher) makes
/// it: every journal still on disk afterwards (its run failed, or it
/// couldn't be read or deleted) stays pending for the next.
fn publish(store: &mut SessionStore<FakeFs, FakeStore>, pending: &mut Vec<FinishedJournal>) {
    let _ = publish_journals(store, length(), pending);
    let dir = store.session();
    if let Ok(there) = dir.fs().list(dir.dir()) {
        pending.retain(|j| there.contains(&dir.dir().join(j.id().file_name())));
    }
}

/// Every file's bytes, and the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Disk {
    files: BTreeMap<PathBuf, Vec<u8>>,
    rows: Vec<SegmentRow>,
}

fn disk(fs: &FakeFs) -> Result<Disk, String> {
    let mut files = BTreeMap::new();
    for path in fs.paths() {
        files.insert(path.clone(), fs.read(&path).map_err(|e| e.to_string())?);
    }
    let rows = FakeStore::new(fs, &db())
        .rows(SESSION)
        .map_err(|e| e.to_string())?;
    Ok(Disk { files, rows })
}

/// The next start's salvage, twice: what it found to do, the disk after
/// it, and after the second.
#[derive(Debug)]
struct Salvaged {
    needed: bool,
    after: Result<Disk, String>,
    again: Result<Disk, String>,
}

fn salvage_once(fs: &FakeFs) -> Result<Disk, String> {
    let lock = SessionDir::new(SESSION, fs.clone(), &session())
        .lock()
        .map_err(|e| e.to_string())?;
    let mut store = SessionStore::new(lock, FakeStore::new(fs, &db()));
    salvage(&mut store, length()).map_err(|e| e.to_string())?;
    disk(fs)
}

fn next_start(fs: &FakeFs) -> Salvaged {
    let needed = needs_salvage(&SessionDir::new(SESSION, fs.clone(), &session())).unwrap_or(true);
    let after = salvage_once(fs);
    let again = salvage_once(fs);
    Salvaged {
        needed,
        after,
        again,
    }
}

fn decode(bytes: &[u8]) -> Result<Vec<i16>, String> {
    let mut reader =
        claxon::FlacReader::new(io::Cursor::new(bytes)).map_err(|e| format!("bad FLAC: {e}"))?;
    reader
        .samples()
        .map(|s| {
            s.map_err(|e| e.to_string())
                .and_then(|s| i16::try_from(s).map_err(|e| e.to_string()))
        })
        .collect()
}

/// After salvage: only segments and rows, every row's file holding the
/// track's own audio at its place; per track, everything durable is there,
/// and no more than [`LOSS_LIMIT`] of what was captured is missing.
fn check_salvaged(promised: &Promised, after: &Disk) -> Result<(), String> {
    let mut held: BTreeMap<TrackId, Vec<bool>> = BTreeMap::new();
    for row in &after.rows {
        let path = session().join(segment_file_name(row.track(), row.range()));
        let bytes = after
            .files
            .get(&path)
            .ok_or_else(|| format!("row without its file: {}", path.display()))?;
        let audio = decode(bytes)?;
        let start = row.range().start().get();
        let have = held.entry(row.track()).or_default();
        for (i, s) in audio.iter().enumerate() {
            let index = start + i as u64;
            if *s != sample(row.track(), index) {
                return Err(format!(
                    "track {} sample {index} is wrong",
                    row.track().get()
                ));
            }
            let at = usize::try_from(index).map_err(|e| e.to_string())?;
            if have.len() <= at {
                have.resize(at + 1, false);
            }
            have[at] = true;
        }
        // Each segment is in the epoch its first sample was recorded in.
        let timeline = promised
            .timelines
            .iter()
            .find(|t| t.track() == row.track())
            .ok_or("a row of an unknown track")?;
        let epoch = timeline
            .epoch_of(row.range().start())
            .ok_or("a row before the first epoch")?;
        if epoch.id() != row.epoch() {
            return Err(format!(
                "track {} row from {start} is in epoch {}, recorded in {}",
                row.track().get(),
                row.epoch().get(),
                epoch.id().get()
            ));
        }
    }
    for path in after.files.keys() {
        let segment = path.extension().is_some_and(|e| e == "flac");
        let kept = path.ends_with(crate::session::MARKS_FILE_NAME)
            || path.ends_with(crate::segment::FINDINGS_FILE_NAME);
        if path.starts_with(session()) && !segment && !kept {
            return Err(format!("left after salvage: {}", path.display()));
        }
    }
    for (&track, &captured) in &promised.captured {
        let have = held.get(&track).map_or(&[][..], Vec::as_slice);
        let has = |i: u64| {
            usize::try_from(i)
                .ok()
                .and_then(|i| have.get(i).copied())
                .unwrap_or(false)
        };
        let missing = (0..captured).filter(|&i| !has(i)).count() as u64;
        let durable = promised.durable[&track];
        if let Some(lost) = (0..durable).find(|&i| !has(i)) {
            return Err(format!(
                "track {}: durable sample {lost} is missing (durable to {durable})",
                track.get()
            ));
        }
        if missing > LOSS_LIMIT {
            return Err(format!(
                "track {}: {missing} of {captured} captured samples lost",
                track.get()
            ));
        }
    }
    Ok(())
}

/// A run stopped and finalised without a crash: if nothing failed, the
/// next start finds nothing to salvage and no audio was lost; either way
/// salvage keeps the loss bound and a second salvage changes nothing.
fn check_stopped(promised: &Promised, fs: &FakeFs) -> Result<(), String> {
    let before = disk(fs)?;
    let next = next_start(fs);
    let after = next.after?;
    if next.again? != after {
        return Err("a second salvage changed something".into());
    }
    if promised.failures == 0 && promised.finalised {
        if next.needed {
            return Err("salvage found journals after a clean stop".into());
        }
        if after != before {
            return Err("salvage changed a cleanly stopped session".into());
        }
        for (&track, &captured) in &promised.captured {
            if promised.durable[&track] != captured {
                return Err(format!(
                    "track {}: only {} of {captured} samples durable after the stop",
                    track.get(),
                    promised.durable[&track]
                ));
            }
        }
    }
    check_salvaged(promised, &after)
}

fn clean_ops() -> usize {
    let fs = FakeFs::with_dirs([session(), db()]);
    let promised = record(&fs, Run::default());
    assert!(promised.finalised);
    assert_eq!(promised.failures, 0);
    // Not vacuous: both tracks moved to a second epoch, and audio was
    // published live.
    for timeline in &promised.timelines {
        assert_eq!(timeline.epochs().len(), 2, "{timeline:?}");
    }
    fs.attempted()
}

#[test]
fn a_stop_between_any_two_events_finalises_both_tracks() {
    let ops = clean_ops();
    assert!(ops > 150, "{ops}");
    let mut stops = BTreeSet::new();
    for k in 0..=ops {
        let fs = FakeFs::with_dirs([session(), db()]);
        let promised = record(
            &fs,
            Run {
                stop_after_ops: Some(k),
                fail_at: None,
            },
        );
        stops.insert(promised.captured.clone().into_iter().collect::<Vec<_>>());
        check_stopped(&promised, &fs).unwrap_or_else(|e| panic!("stopped after op {k}: {e}"));
    }
    // Stops landed at many places in the script, not a few.
    assert!(stops.len() > 40, "{}", stops.len());
}

#[test]
fn a_stop_at_each_failpoint_keeps_the_loss_bound() {
    let ops = clean_ops();
    let mut failed = 0;
    for k in 0..ops {
        let fs = FakeFs::with_dirs([session(), db()]);
        let promised = record(
            &fs,
            Run {
                stop_after_ops: Some(k),
                fail_at: Some(k),
            },
        );
        failed += usize::from(promised.failures > 0 || !promised.finalised);
        check_stopped(&promised, &fs)
            .unwrap_or_else(|e| panic!("op {k} failed and the stop came: {e}"));
    }
    // Not vacuous: dozens of the failures broke a journal or left the
    // finalisation incomplete. (Most fall in publishing, which tries
    // again.)
    assert!(failed >= 50, "{failed} of {ops}");
}

/// What a crashed run promised, checked against salvage: durable audio
/// kept, the loss bound held per track, and salvage repeatable.
fn check_crashed(_case: &CrashCase, promised: &Promised, got: &Salvaged) -> Result<(), String> {
    let after = got.after.clone()?;
    if got.again.as_ref() != Ok(&after) {
        return Err("a second salvage changed something".into());
    }
    check_salvaged(promised, &after)
}

#[test]
fn killed_while_finalising_salvage_completes_it_repeatably() {
    // Stopped two thirds of the way through, then crashed at every
    // operation, the finalisation's included: the process killed after the
    // signal, before it finished.
    let ops = clean_ops();
    let stop = Run {
        stop_after_ops: Some(ops * 2 / 3),
        fail_at: None,
    };
    let summary = CrashTest::new(
        move |fs: &FakeFs| record(fs, stop),
        next_start,
        check_crashed,
    )
    .dirs([session(), db()])
    .outcomes(vec![
        CrashOutcome::LoseUnsynced,
        CrashOutcome::KeepAll,
        CrashOutcome::Partial { seed: 3 },
    ])
    .run()
    .unwrap_or_else(|failure| panic!("{failure}"));
    assert!(summary.scenario_ops > 100, "{summary:?}");
}
