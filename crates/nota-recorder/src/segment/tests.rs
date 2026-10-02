//! The segment acceptance tests: recording with live publishing crashed
//! after every operation, salvage crashed after every operation, overlapping
//! journals, and the rotation bound.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRange, SampleRate, SessionId,
    SessionTime, TrackId,
};
use nota_store::SegmentRow;
use sha2::{Digest, Sha256};

use super::publish::TempSegment;
use super::*;
use crate::fs::crash::{CrashCase, CrashTest};
use crate::fs::fake::{CrashOutcome, FakeFs, Op};
use crate::fs::{Fs, FsFile, StdFs};
use crate::journal::format::{FRAME_HEADER_LEN, HEADER_LEN};
use crate::journal::{JournalId, read_journal};
use crate::session::{FinishedJournal, SessionDir, SessionStore, SessionWriter};
use crate::test_dir::TestDir;

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);
const SESSION: SessionId = SessionId::new(1);

/// A low rate keeps the crash tests fast: a second is 1,000 samples.
fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

/// One and a half seconds per window, so journals sync partway through.
fn length() -> SegmentLength {
    SegmentLength::new(1_500).unwrap()
}

fn session() -> PathBuf {
    PathBuf::from("/session")
}

fn db() -> PathBuf {
    PathBuf::from("/db")
}

fn session_dir(fs: &FakeFs) -> SessionDir<FakeFs> {
    SessionDir::new(SESSION, fs.clone(), &session())
}

/// The session's store, in the usual place.
fn session_store(fs: &FakeFs) -> SessionStore<FakeFs, FakeStore> {
    SessionStore::new(session_dir(fs), FakeStore::new(fs, &db()))
}

/// The sample a test track holds at `index`: distinct per track and
/// position, so misplaced or duplicated audio shows.
fn sample(track: TrackId, index: u64) -> i16 {
    let v = index
        .wrapping_mul(31)
        .wrapping_add(u64::from(track.get()) * 7_919);
    i16::from_le_bytes([v.to_le_bytes()[0], v.to_le_bytes()[1]])
}

fn samples(track: TrackId, from: u64, len: u64) -> Vec<i16> {
    (from..from + len).map(|i| sample(track, i)).collect()
}

/// The finished journals numbered `ids`.
fn finished(ids: &[u64]) -> Vec<FinishedJournal> {
    ids.iter()
        .map(|&n| FinishedJournal::new(JournalId::new(n)))
        .collect()
}

fn decode_flac(bytes: &[u8]) -> Result<(u32, Vec<i16>), String> {
    let mut reader =
        claxon::FlacReader::new(io::Cursor::new(bytes)).map_err(|e| format!("bad FLAC: {e}"))?;
    let hz = reader.streaminfo().sample_rate;
    let samples = reader
        .samples()
        .map(|s| {
            s.map_err(|e| e.to_string())
                .and_then(|s| i16::try_from(s).map_err(|e| e.to_string()))
        })
        .collect::<Result<_, _>>()?;
    Ok((hz, samples))
}

/// What the recording told its caller before it stopped.
#[derive(Debug, Default, Clone)]
struct Promised {
    /// Per track, where it started and the furthest durable position the
    /// writer reported.
    started: BTreeMap<TrackId, SampleIndex>,
    durable: BTreeMap<TrackId, SampleIndex>,
    /// Rows publishing reported committed.
    rows: Vec<SegmentRow>,
}

impl Promised {
    fn note<S: Fs>(&mut self, writer: &SessionWriter<S>, track: TrackId, ok: bool) {
        // Between journals, a successful call has ended the last one with a
        // sync: everything up to the next sample is durable.
        let end = match writer.durable(track) {
            Some(d) => Some(d.end()),
            None if ok => writer.next_sample(track),
            None => None,
        };
        if let Some(end) = end {
            let at = self.durable.entry(track).or_insert(end);
            *at = (*at).max(end);
        }
    }
}

/// How a test recording runs.
#[derive(Debug, Clone, Copy)]
struct Recording {
    /// Rounds of audio on both tracks.
    steps: usize,
    /// Publish finished journals as it goes.
    publish: bool,
    /// Fail this operation (counting from the start) without crashing.
    fail_at: Option<usize>,
}

fn fake_clock() -> (Arc<FakeClock>, Arc<dyn Clock>) {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let dyn_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    (clock, dyn_clock)
}

/// Records two tracks in real time, chunks of uneven size, some longer than
/// a window. Stops at the first error, as the recorder would.
fn record(fs: &FakeFs, how: Recording) -> Promised {
    let mut promised = Promised::default();
    if let Some(at) = how.fail_at {
        fs.fail_after(at, io::ErrorKind::Other);
    }
    let _ = record_into(fs, how, &mut promised);
    promised
}

fn record_into(fs: &FakeFs, how: Recording, promised: &mut Promised) -> Result<(), Box<dyn Error>> {
    let (clock, dyn_clock) = fake_clock();
    let mut writer = SessionWriter::open(&session_dir(fs), rate(), length(), dyn_clock)?;
    let mut store = session_store(fs);
    for (track, at) in [(MIC, 0_u64), (SYSTEM, 700)] {
        writer.start_track(track, EpochId::new(0), SampleIndex::new(at))?;
        promised.started.insert(track, SampleIndex::new(at));
        promised.durable.insert(track, SampleIndex::new(at));
    }
    let sizes = [250_u64, 100, 400, 1_600, 50];
    let mut pending: Vec<FinishedJournal> = Vec::new();
    let mut publish = |writer: &mut SessionWriter<FakeFs>,
                       promised: &mut Promised|
     -> Result<(), Box<dyn Error>> {
        pending.extend(writer.take_finished());
        if how.publish && !pending.is_empty() {
            let done = publish_journals(&mut store, length(), &pending)?;
            promised.rows.extend_from_slice(done.segments());
            pending.clear();
        }
        Ok(())
    };
    for step in 0..how.steps {
        let len = sizes[step % sizes.len()];
        for track in [MIC, SYSTEM] {
            let from = writer.next_sample(track).unwrap().get();
            let appended = writer.append(track, &samples(track, from, len));
            promised.note(&writer, track, appended.is_ok());
            appended?;
        }
        clock.advance(SampleCount::new(len).duration_at(rate()).unwrap());
        let synced = writer.sync_if_due();
        for track in [MIC, SYSTEM] {
            promised.note(&writer, track, synced.is_ok());
        }
        synced?;
        publish(&mut writer, promised)?;
    }
    let ends: Vec<_> = [MIC, SYSTEM]
        .into_iter()
        .map(|t| (t, writer.next_sample(t).unwrap()))
        .collect();
    let finished = writer.finish()?;
    for (track, end) in ends {
        promised.durable.insert(track, end);
    }
    pending.extend(finished);
    if how.publish {
        let done = publish_journals(&mut store, length(), &pending)?;
        promised.rows.extend_from_slice(done.segments());
    }
    Ok(())
}

/// Everything on the disk that matters: every file's bytes, and the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    files: BTreeMap<PathBuf, Vec<u8>>,
    rows: Result<Vec<SegmentRow>, String>,
}

/// On a crashed fake (recovery crashed partway) the reads fail, and what
/// it returns doesn't matter: only the final recovery's result is checked.
fn observe(fs: &FakeFs) -> Observed {
    let files = fs
        .paths()
        .into_iter()
        .filter_map(|p| fs.read(&p).ok().map(|bytes| (p, bytes)))
        .collect();
    let rows = FakeStore::new(fs, &db()).rows().map_err(|e| e.to_string());
    Observed { files, rows }
}

/// What recovery saw: the disk before salvage, after it, and after a
/// second salvage.
#[derive(Debug, Clone)]
struct Recovered {
    before: Observed,
    after: Result<Observed, String>,
    again: Result<Observed, String>,
}

fn salvage_fake(fs: &FakeFs) -> Result<Observed, String> {
    let mut store = session_store(fs);
    salvage(&mut store, length()).map_err(|e| e.to_string())?;
    Ok(observe(fs))
}

fn recover(fs: &FakeFs) -> Recovered {
    let before = observe(fs);
    let after = salvage_fake(fs);
    let again = salvage_fake(fs);
    Recovered {
        before,
        after,
        again,
    }
}

fn is_journal(path: &Path) -> bool {
    path.file_name()
        .and_then(JournalId::from_file_name)
        .is_some()
}

/// Per track, the samples held by valid journal frames, each checked
/// against what was recorded.
fn journal_samples(seen: &Observed) -> Result<BTreeSet<(TrackId, u64)>, String> {
    let mut held = BTreeSet::new();
    for (path, bytes) in &seen.files {
        if !is_journal(path) {
            continue;
        }
        let read = read_journal(bytes);
        for frame in read.frames() {
            let r = frame.range();
            let want = samples(frame.track(), r.start().get(), r.len().get());
            if frame.samples() != want {
                return Err(format!("{} misread at {:?}", path.display(), r));
            }
            held.extend((r.start().get()..r.end().get()).map(|s| (frame.track(), s)));
        }
    }
    Ok(held)
}

/// Checks every row has its file, holding exactly the row's audio; returns
/// the samples the rows hold.
fn row_samples(seen: &Observed) -> Result<BTreeSet<(TrackId, u64)>, String> {
    let rows = seen.rows.clone()?;
    let mut held = BTreeSet::new();
    for row in &rows {
        let path = session().join(segment_file_name(row.track(), row.range()));
        let Some(bytes) = seen.files.get(&path) else {
            return Err(format!("a row without its file: {row:?}"));
        };
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if &digest != row.sha256().as_bytes() {
            return Err(format!("{} doesn't match its row's hash", path.display()));
        }
        let (hz, got) = decode_flac(bytes)?;
        let r = row.range();
        if hz != rate().hz() || got != samples(row.track(), r.start().get(), r.len().get()) {
            return Err(format!("{} holds the wrong audio", path.display()));
        }
        for s in r.start().get()..r.end().get() {
            if !held.insert((row.track(), s)) {
                return Err(format!(
                    "rows overlap at track {} sample {s}",
                    row.track().get()
                ));
            }
        }
    }
    Ok(held)
}

/// Every promised-durable sample is in `held`.
fn check_durable(promised: &Promised, held: &BTreeSet<(TrackId, u64)>) -> Result<(), String> {
    for (&track, &start) in &promised.started {
        let end = promised.durable[&track];
        if let Some(s) = (start.get()..end.get()).find(|&s| !held.contains(&(track, s))) {
            return Err(format!(
                "track {} lost sample {s} (durable to {})",
                track.get(),
                end.get()
            ));
        }
    }
    Ok(())
}

fn check_after(promised: &Promised, after: &Observed) -> Result<(), String> {
    if let Some(left) = after
        .files
        .keys()
        .find(|p| is_journal(p) || is_temp_segment(p))
    {
        return Err(format!("salvage left {}", left.display()));
    }
    let held = row_samples(after)?;
    check_durable(promised, &held)?;
    let rows = after.rows.clone()?;
    for row in &promised.rows {
        if !rows.contains(row) {
            return Err(format!("a committed row disappeared: {row:?}"));
        }
    }
    // No segment file without a row.
    let named: BTreeSet<PathBuf> = rows
        .iter()
        .map(|r| session().join(segment_file_name(r.track(), r.range())))
        .collect();
    if let Some(orphan) = after
        .files
        .keys()
        .find(|p| p.starts_with(session()) && !named.contains(*p))
    {
        return Err(format!("a file without a row: {}", orphan.display()));
    }
    Ok(())
}

/// The invariants, at any crash point:
/// - before salvage: no row without its file; every durable sample is in a
///   row's file or a journal; nothing misread;
/// - after salvage: only segments and rows are left, holding every durable
///   sample and every committed row;
/// - salvage again changes nothing.
fn check(_: &CrashCase, promised: &Promised, got: &Recovered) -> Result<(), String> {
    let in_rows = row_samples(&got.before).map_err(|e| format!("before salvage: {e}"))?;
    let in_journals = journal_samples(&got.before).map_err(|e| format!("before salvage: {e}"))?;
    let held = in_rows.union(&in_journals).copied().collect();
    check_durable(promised, &held).map_err(|e| format!("before salvage: {e}"))?;
    let after = got.after.clone()?;
    check_after(promised, &after).map_err(|e| format!("after salvage: {e}"))?;
    if got.again.as_ref() != Ok(&after) {
        return Err("a second salvage changed something".to_owned());
    }
    Ok(())
}

fn crash_test(how: Recording) -> CrashTest<impl Fn(&FakeFs) -> Promised, RecoverFn, CheckFn> {
    CrashTest::new(
        move |fs: &FakeFs| record(fs, how),
        recover as RecoverFn,
        check as CheckFn,
    )
    .dirs([session(), db()])
}

type RecoverFn = fn(&FakeFs) -> Recovered;
type CheckFn = fn(&CrashCase, &Promised, &Recovered) -> Result<(), String>;

/// A clean run, for checking a test isn't vacuous.
fn clean_run(how: Recording) -> (FakeFs, Promised) {
    let fs = FakeFs::with_dirs([session(), db()]);
    let promised = record(&fs, how);
    (fs, promised)
}

#[test]
fn recording_and_publishing_crashed_after_every_operation_loses_nothing() {
    let how = Recording {
        steps: 7,
        publish: true,
        fail_at: None,
    };
    let (_, promised) = clean_run(how);
    // Several segments per track, published live, so crashes land in every
    // publish step.
    assert!(promised.rows.len() >= 4, "{:?}", promised.rows);
    let summary = crash_test(how).run().unwrap();
    assert!(summary.scenario_ops > 100, "{summary:?}");
}

/// Crashes salvage of `disk` after each of its operations, under every
/// standard outcome, and runs it again: the end state must be byte for byte
/// what one uninterrupted salvage makes, and keep every promised sample.
/// Returns how many operations an uninterrupted salvage took.
fn salvage_crashed_everywhere(disk: &FakeFs, promised: &Promised) -> usize {
    let probe = disk.copy_disk();
    let uninterrupted = salvage_fake(&probe).unwrap();
    check_after(promised, &uninterrupted).unwrap();
    let ops = probe.attempted();
    for after in 0..=ops {
        for outcome in CrashOutcome::standard() {
            let run = disk.copy_disk();
            run.crash_after(after);
            let _ = salvage_fake(&run);
            let survived = run.crash(outcome);
            let rerun = salvage_fake(&survived)
                .unwrap_or_else(|e| panic!("after {after} ops, {outcome:?}: {e}"));
            assert!(
                rerun == uninterrupted,
                "salvage crashed after {after} ops, {outcome:?}, ended differently"
            );
        }
    }
    ops
}

#[test]
fn salvage_crashed_after_every_operation_ends_as_an_uninterrupted_run() {
    // Recording without publishing leaves salvage the most to do.
    let plain = Recording {
        steps: 4,
        publish: false,
        fail_at: None,
    };
    let (fs, promised) = clean_run(plain);
    for outcome in [
        CrashOutcome::LoseUnsynced,
        CrashOutcome::KeepAll,
        CrashOutcome::Partial { seed: 1 },
    ] {
        let ops = salvage_crashed_everywhere(&fs.crash(outcome), &promised);
        assert!(ops > 30, "{ops}");
    }

    // Overlapping journals, kept by the crash.
    let broken = Recording {
        fail_at: Some(a_write_after_unsynced_frames(plain)),
        ..plain
    };
    let (fs, promised) = clean_run(broken);
    for outcome in [
        CrashOutcome::KeepAll,
        CrashOutcome::Partial { seed: 2 },
        CrashOutcome::Partial { seed: 4 },
    ] {
        salvage_crashed_everywhere(&fs.crash(outcome), &promised);
    }

    // A live publish crashed between its rename and its row.
    let live = Recording {
        steps: 4,
        publish: true,
        fail_at: None,
    };
    let (fs, _) = clean_run(live);
    let rename = fs
        .ops()
        .iter()
        .position(
            |op| matches!(op, Op::Rename { to, .. } if to.extension().is_some_and(|e| e == "flac")),
        )
        .unwrap();
    let crashed = FakeFs::with_dirs([session(), db()]);
    crashed.crash_after(rename + 1);
    let promised = record(&crashed, live);
    salvage_crashed_everywhere(&crashed.crash(CrashOutcome::KeepAll), &promised);
}

#[test]
fn recovery_crashed_at_every_point_of_a_short_recording() {
    // The full product: every crash point of a recording with live
    // publishing, and salvage crashed after each of its operations too.
    let how = Recording {
        steps: 2,
        publish: true,
        fail_at: None,
    };
    let summary = crash_test(how)
        .outcomes(vec![
            CrashOutcome::KeepAll,
            CrashOutcome::Partial { seed: 3 },
        ])
        .crash_recovery()
        .run()
        .unwrap();
    assert!(
        summary.cases > 2 * (summary.scenario_ops + 1),
        "{summary:?}"
    );
}

/// The operation index of a journal frame write that comes straight after
/// another frame write to the same journal: failing it leaves unsynced
/// frames behind it.
fn a_write_after_unsynced_frames(how: Recording) -> usize {
    let (fs, _) = clean_run(how);
    let ops = fs.ops();
    // `fail_after` counts attempted operations, and a clean run attempts
    // exactly the ones it logs.
    assert_eq!(fs.attempted(), ops.len());
    // Per journal, whether its last operation was a frame write.
    let mut unsynced: BTreeMap<PathBuf, bool> = BTreeMap::new();
    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::Write { path, len } if is_journal(path) && *len != HEADER_LEN => {
                if unsynced.get(path) == Some(&true) {
                    return i;
                }
                unsynced.insert(path.clone(), true);
            }
            Op::Sync(path) => {
                unsynced.insert(path.clone(), false);
            }
            _ => {}
        }
    }
    panic!("no frame write after an unsynced one in {ops:?}");
}

#[test]
fn overlapping_journals_resolve_to_the_newer_one() {
    let base = Recording {
        steps: 5,
        publish: false,
        fail_at: None,
    };
    let how = Recording {
        fail_at: Some(a_write_after_unsynced_frames(base)),
        ..base
    };

    // Not vacuous: with everything kept, the broken journal's unsynced tail
    // and its replacement hold the same samples.
    let (fs, promised) = clean_run(how);
    let disk = fs.crash(CrashOutcome::KeepAll);
    let mut ranges: Vec<(JournalId, TrackId, SampleRange)> = Vec::new();
    for path in disk.paths().into_iter().filter(|p| is_journal(p)) {
        let read = read_journal(&disk.read(&path).unwrap());
        if let (Some(h), Some(r)) = (read.header(), read.range()) {
            ranges.push((h.id(), h.track(), r));
        }
    }
    let overlaps = ranges.iter().any(|a| {
        ranges
            .iter()
            .any(|b| a.0 < b.0 && a.1 == b.1 && a.2.start() < b.2.end() && b.2.start() < a.2.end())
    });
    assert!(overlaps, "{ranges:?}");
    let after = salvage_fake(&disk).unwrap();
    check_after(&promised, &after).unwrap();

    // And at every crash point, under the outcomes that keep unsynced data.
    crash_test(how)
        .outcomes(vec![
            CrashOutcome::KeepAll,
            CrashOutcome::Partial { seed: 0 },
            CrashOutcome::Partial { seed: 1 },
            CrashOutcome::Partial { seed: 2 },
            CrashOutcome::Partial { seed: 6 },
        ])
        .run()
        .unwrap();
}

#[test]
fn journals_rotate_at_every_window_even_when_publishing_fails() {
    let fs = FakeFs::with_dirs([session()]);
    let (clock, dyn_clock) = fake_clock();
    let mut writer = SessionWriter::open(&session_dir(&fs), rate(), length(), dyn_clock).unwrap();
    // The store's directory doesn't exist: every publish fails.
    let mut store = SessionStore::new(session_dir(&fs), FakeStore::new(&fs, Path::new("/missing")));
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::new(100))
        .unwrap();
    let mut pending = Vec::new();
    let mut failures = 0;
    let mut from = 100;
    for len in [700_u64, 3_100, 20, 1_480, 5_000, 1, 2_999] {
        writer.append(MIC, &samples(MIC, from, len)).unwrap();
        from += len;
        clock.advance(SampleCount::new(len).duration_at(rate()).unwrap());
        writer.sync_if_due().unwrap();
        pending.extend(writer.take_finished());
        if publish_journals(&mut store, length(), &pending).is_err() {
            failures += 1;
        }
    }
    writer.finish().unwrap();
    assert_eq!(failures, 7);

    let mut covered = 100;
    let mut journals = 0;
    for path in fs.paths() {
        let bytes = fs.read(&path).unwrap();
        let read = read_journal(&bytes);
        let range = read.range().unwrap();
        // Within one window...
        assert_eq!(
            length().window_of(range.start()),
            length().window_of(SampleIndex::new(range.end().get() - 1)),
            "{} holds {range:?}",
            path.display()
        );
        // ...so never more than one window's worth of bytes.
        let frames = u64::try_from(read.frames().len()).unwrap();
        assert!(range.len().get() <= length().samples());
        assert_eq!(
            bytes.len() as u64,
            HEADER_LEN as u64 + frames * FRAME_HEADER_LEN as u64 + 2 * range.len().get()
        );
        // Journals are in order and continuous, one per window here.
        assert_eq!(range.start().get(), covered);
        covered = range.end().get();
        journals += 1;
    }
    assert_eq!(covered, from);
    assert_eq!(u64::try_from(journals).unwrap(), (from - 1) / 1_500 + 1);
}

#[test]
fn a_journal_break_is_replaced_without_losing_samples() {
    // Fail one frame write mid-recording: the writer replays the unsynced
    // samples into a new journal, so salvage loses nothing even when the
    // broken journal's tail is gone.
    let base = Recording {
        steps: 5,
        publish: false,
        fail_at: None,
    };
    let how = Recording {
        fail_at: Some(a_write_after_unsynced_frames(base)),
        ..base
    };
    let (fs, promised) = clean_run(how);
    let (_, unbroken) = clean_run(base);
    assert_eq!(promised.durable, unbroken.durable);
    let after = salvage_fake(&fs.crash(CrashOutcome::LoseUnsynced)).unwrap();
    check_after(&promised, &after).unwrap();
}

#[test]
fn salvage_twice_changes_nothing_and_leaves_no_journals() {
    let (fs, promised) = clean_run(Recording {
        steps: 6,
        publish: false,
        fail_at: None,
    });
    let disk = fs.crash(CrashOutcome::KeepAll);
    assert!(needs_salvage(&session_dir(&disk)).unwrap());
    let mut store = session_store(&disk);
    let first = salvage(&mut store, length()).unwrap();
    assert!(!first.segments().is_empty());
    assert!(!first.deleted().is_empty());
    assert!(!needs_salvage(&session_dir(&disk)).unwrap());
    let seen = observe(&disk);
    check_after(&promised, &seen).unwrap();
    let second = salvage(&mut store, length()).unwrap();
    assert_eq!(second, Published::default());
    assert_eq!(observe(&disk), seen);
}

#[test]
fn rows_carry_the_epoch_and_a_new_epoch_starts_a_new_segment() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let (_, clock) = fake_clock();
    let mut writer = SessionWriter::open(&session_dir(&fs), rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    writer.append(MIC, &samples(MIC, 0, 400)).unwrap();
    writer.new_epoch(MIC, EpochId::new(1)).unwrap();
    writer.append(MIC, &samples(MIC, 400, 200)).unwrap();
    let ids = writer.finish().unwrap();
    assert_eq!(ids, finished(&[0, 1]));
    let mut store = session_store(&fs);
    let done = publish_journals(&mut store, length(), &ids).unwrap();
    let got: Vec<_> = done
        .segments()
        .iter()
        .map(|r| {
            (
                r.epoch().get(),
                r.range().start().get(),
                r.range().end().get(),
            )
        })
        .collect();
    assert_eq!(got, [(0, 0, 400), (1, 400, 600)]);
    assert_eq!(done.deleted(), [JournalId::new(0), JournalId::new(1)]);
}

#[test]
fn journal_ids_continue_after_a_restart() {
    let fs = FakeFs::with_dirs([session()]);
    let (_, clock) = fake_clock();
    // A journal left by an earlier run, numbered 7.
    let mut file = fs
        .create(&session().join(JournalId::new(7).file_name()))
        .unwrap();
    file.write_all(b"x").unwrap();
    let mut writer = SessionWriter::open(&session_dir(&fs), rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    writer.append(MIC, &[1, 2, 3]).unwrap();
    assert_eq!(writer.finish().unwrap(), finished(&[8]));
}

#[test]
fn an_unreadable_journal_is_set_aside_and_the_rest_salvaged() {
    let (fs, promised) = clean_run(Recording {
        steps: 2,
        publish: false,
        fail_at: None,
    });
    let disk = fs.crash(CrashOutcome::KeepAll);
    let bad = session().join(JournalId::new(90).file_name());
    let mut file = disk.create(&bad).unwrap();
    file.write_all(&[0xAB; 40]).unwrap();
    // A good header naming another id than the file's name is refused too.
    let renamed = session().join(JournalId::new(91).file_name());
    let first = session().join(JournalId::new(0).file_name());
    let mut copy = disk.create(&renamed).unwrap();
    copy.write_all(&disk.read(&first).unwrap()).unwrap();

    let mut store = session_store(&disk);
    let done = salvage(&mut store, length()).unwrap();
    let mut aside: Vec<_> = done.quarantined().to_vec();
    aside.sort();
    assert_eq!(
        aside,
        [
            PathBuf::from("/session/journal-000090.unreadable"),
            PathBuf::from("/session/journal-000091.unreadable"),
        ]
    );
    assert_eq!(disk.read(&aside[0]).unwrap(), [0xAB; 40]);
    let mut seen = observe(&disk);
    seen.files
        .retain(|p, _| p.extension().is_none_or(|e| e != "unreadable"));
    check_after(&promised, &seen).unwrap();
}

#[test]
fn a_failing_store_keeps_the_journals_for_the_next_try() {
    let (fs, promised) = clean_run(Recording {
        steps: 3,
        publish: false,
        fail_at: None,
    });
    let mut missing =
        SessionStore::new(session_dir(&fs), FakeStore::new(&fs, Path::new("/missing")));
    let err = salvage(&mut missing, length()).unwrap_err();
    assert!(matches!(err, PublishError::Store(_)), "{err}");
    assert!(needs_salvage(&session_dir(&fs)).unwrap());
    let mut store = session_store(&fs);
    salvage(&mut store, length()).unwrap();
    check_after(&promised, &observe(&fs)).unwrap();
}

#[test]
fn salvage_on_the_real_filesystem_with_sqlite() {
    let dir = TestDir::new("salvage-sqlite");
    let session = dir.0.join("session");
    StdFs.create_dir(&session).unwrap();
    let (clock, dyn_clock) = fake_clock();
    let ours = SessionDir::new(SESSION, StdFs, &session);
    let mut writer = SessionWriter::open(&ours, rate(), length(), dyn_clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(2), SampleIndex::new(10))
        .unwrap();
    writer.append(MIC, &samples(MIC, 10, 3_000)).unwrap();
    clock.advance(std::time::Duration::from_secs(3));
    writer.sync_if_due().unwrap();
    // Stop without finishing, as a crash would.
    drop(writer);

    let mut store = nota_store::Store::open(&dir.0.join("nota.db")).unwrap();
    let done = salvage(&mut SessionStore::new(ours.clone(), &mut store), length()).unwrap();
    let rows = store.segments().unwrap();
    assert_eq!(rows, done.segments());
    let ranges: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r.epoch().get(),
                r.range().start().get(),
                r.range().end().get(),
            )
        })
        .collect();
    assert_eq!(
        ranges,
        [(2, 10, 1_500), (2, 1_500, 3_000), (2, 3_000, 3_010)]
    );
    for row in &rows {
        let bytes = StdFs
            .read(&session.join(segment_file_name(row.track(), row.range())))
            .unwrap();
        let (_, got) = decode_flac(&bytes).unwrap();
        let r = row.range();
        assert_eq!(got, samples(MIC, r.start().get(), r.len().get()));
    }
    assert!(!needs_salvage(&ours).unwrap());
    // Reopened, the rows are still there, and salvage has nothing to do.
    drop(store);
    let mut store = nota_store::Store::open(&dir.0.join("nota.db")).unwrap();
    assert_eq!(store.segments().unwrap(), rows);
    assert_eq!(
        salvage(&mut SessionStore::new(ours, &mut store), length()).unwrap(),
        Published::default()
    );
}

#[test]
fn segment_lengths_and_names() {
    assert_eq!(SegmentLength::new(0), None);
    let five = SegmentLength::default_at(SampleRate::SPEECH);
    assert_eq!(five.samples(), 4_800_000);
    let l = SegmentLength::new(10).unwrap();
    assert_eq!(l.window_of(SampleIndex::new(9)), 0);
    assert_eq!(l.window_of(SampleIndex::new(10)), 1);
    assert_eq!(
        l.window_end(SampleIndex::new(10)),
        Some(SampleIndex::new(20))
    );
    assert_eq!(
        SegmentLength::new(u64::MAX)
            .unwrap()
            .window_end(SampleIndex::new(5)),
        Some(SampleIndex::new(u64::MAX))
    );
    assert_eq!(
        SegmentLength::new(1)
            .unwrap()
            .window_end(SampleIndex::new(u64::MAX)),
        None
    );
    let r = SampleRange::new(SampleIndex::new(4_800_000), SampleIndex::new(4_800_001)).unwrap();
    assert_eq!(
        segment_file_name(TrackId::new(1), r),
        "seg-t1-000004800000.flac"
    );
    assert!(is_temp_segment(Path::new(
        "/s/seg-t1-000004800000.flac.tmp"
    )));
    assert!(!is_temp_segment(Path::new("/s/seg-t1-000004800000.flac")));
    assert!(!is_temp_segment(Path::new("/s/t1.row.tmp")));
}

#[test]
fn errors_describe_themselves() {
    let errors = [
        PublishError::Io(io::Error::other("disk")),
        PublishError::Store(Box::new(io::Error::other("db"))),
        PublishError::Flac(FlacError::Empty),
        PublishError::Changed(JournalId::new(4)),
    ];
    for e in &errors {
        assert!(!e.to_string().is_empty());
    }
    assert!(errors[3].to_string().contains("journal-000004"));
    assert!(errors[0].source().is_some());
    assert!(errors[3].source().is_none());
}

#[test]
fn a_row_without_its_file_here_claims_nothing() {
    // A row naming samples of this track, but whose file isn't in this
    // session's directory (another session's, in a shared store): the
    // journals holding those samples must not be deleted on its word.
    let (fs, _) = clean_run(Recording {
        steps: 2,
        publish: false,
        fail_at: None,
    });
    let elsewhere = FakeFs::with_dirs([PathBuf::from("/other"), db()]);
    let mut foreign = FakeStore::new(&elsewhere, &db());
    let mut file = elsewhere
        .create(Path::new("/other/seg-t0-000000000000.flac"))
        .unwrap();
    file.write_all(b"x").unwrap();
    let durable = TempSegment::write(
        &elsewhere,
        Path::new("/other"),
        MIC,
        EpochId::new(0),
        SampleRange::new(SampleIndex::ZERO, SampleIndex::new(1_000)).unwrap(),
        b"not this session's",
    )
    .unwrap()
    .sync()
    .unwrap()
    .rename(&elsewhere)
    .unwrap()
    .sync_dir(&elsewhere)
    .unwrap();
    durable.commit(&mut foreign).unwrap();
    let row = foreign.rows().unwrap();

    // The same row in this session's store.
    let mut file = fs
        .create(&db().join("t0-00000000000000000000.row"))
        .unwrap();
    file.write_all(
        &elsewhere
            .read(&db().join("t0-00000000000000000000.row"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(FakeStore::new(&fs, &db()).rows().unwrap(), row);
    let mut store = session_store(&fs);
    let journals_before: Vec<_> = fs.paths().into_iter().filter(|p| is_journal(p)).collect();
    // Salvage plans a segment over the row's samples, and the store refuses
    // it: nothing is deleted.
    let err = salvage(&mut store, length()).unwrap_err();
    assert!(matches!(err, PublishError::Store(_)), "{err}");
    let journals_after: Vec<_> = fs.paths().into_iter().filter(|p| is_journal(p)).collect();
    assert!(journals_after.contains(&session().join(JournalId::new(0).file_name())));
    assert!(!journals_before.is_empty());
}

#[test]
fn a_journal_corrupt_before_its_end_is_published_then_set_aside() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let (clock, dyn_clock) = fake_clock();
    let long = SegmentLength::new(1_000_000).unwrap();
    let mut writer = SessionWriter::open(&session_dir(&fs), rate(), long, dyn_clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    for k in 0..60 {
        writer.append(MIC, &samples(MIC, k * 1_000, 1_000)).unwrap();
        clock.advance(std::time::Duration::from_secs(1));
        writer.sync_if_due().unwrap();
    }
    writer.finish().unwrap();
    let path = session().join(JournalId::FIRST.file_name());
    let mut bytes = fs.read(&path).unwrap();
    // Flip a sample byte in the third frame: two frames read, then 57
    // seconds of audio that can't be.
    let frame = FRAME_HEADER_LEN + 2_000;
    bytes[HEADER_LEN + 2 * frame + FRAME_HEADER_LEN + 7] ^= 0x40;
    let disk = FakeFs::with_dirs([session(), db()]);
    let mut file = disk.create(&path).unwrap();
    file.write_all(&bytes).unwrap();

    let mut store = session_store(&disk);
    let done = salvage(&mut store, long).unwrap();
    let ranges: Vec<_> = done
        .segments()
        .iter()
        .map(|r| (r.range().start().get(), r.range().end().get()))
        .collect();
    assert_eq!(ranges, [(0, 2_000)]);
    assert!(done.deleted().is_empty());
    let aside = PathBuf::from("/session/journal-000000.unreadable");
    assert_eq!(done.quarantined(), std::slice::from_ref(&aside));
    assert_eq!(disk.read(&aside).unwrap(), bytes);

    // A torn tail no longer than a crash can leave is deleted as usual.
    let torn = &bytes[..HEADER_LEN + 3 * frame + 100];
    let disk = FakeFs::with_dirs([session(), db()]);
    let mut file = disk.create(&path).unwrap();
    file.write_all(torn).unwrap();
    let mut store = session_store(&disk);
    let done = salvage(&mut store, long).unwrap();
    assert_eq!(done.deleted(), [JournalId::FIRST]);
    assert!(done.quarantined().is_empty());
}

#[test]
fn a_row_whose_commit_failed_isnt_reported() {
    // Fail the store's directory sync: the commit fails, and the row must
    // not show up as committed for a retry to trust.
    let how = Recording {
        steps: 7,
        publish: true,
        fail_at: None,
    };
    let (fs, _) = clean_run(how);
    let at = fs
        .ops()
        .iter()
        .position(|op| matches!(op, Op::SyncDir(d) if *d == db()))
        .unwrap();
    let fs = FakeFs::with_dirs([session(), db()]);
    let promised = record(
        &fs,
        Recording {
            fail_at: Some(at),
            ..how
        },
    );
    assert!(promised.rows.is_empty());
    assert!(FakeStore::new(&fs, &db()).rows().unwrap().is_empty());
    // Salvage on the running system, then a crash that drops what wasn't
    // made durable: nothing promised is lost.
    let mut store = session_store(&fs);
    salvage(&mut store, length()).unwrap();
    let after = observe(&fs.crash(CrashOutcome::LoseUnsynced));
    check_after(&promised, &after).unwrap();
}

#[test]
fn a_journal_break_while_publishing_live_loses_nothing_at_any_crash() {
    // The broken journal is held back until its replacement ends, so live
    // publishing plans both together; crash everywhere around that.
    let base = Recording {
        steps: 5,
        publish: true,
        fail_at: None,
    };
    // The first failure point that breaks a journal mid-recording: the run
    // makes one more journal than a clean one, and still finishes.
    let journals_made = |fs: &FakeFs| {
        fs.ops()
            .iter()
            .filter(|op| matches!(op, Op::Create(p) if is_journal(p)))
            .count()
    };
    let (clean, _) = clean_run(base);
    let normal = journals_made(&clean);
    let at = (20..400)
        .find(|&at| {
            let fs = FakeFs::with_dirs([session(), db()]);
            let promised = record(
                &fs,
                Recording {
                    fail_at: Some(at),
                    ..base
                },
            );
            let finished = fs
                .ops()
                .iter()
                .filter(|op| matches!(op, Op::Rename { .. }))
                .count()
                > 4;
            journals_made(&fs) > normal && finished && !promised.rows.is_empty()
        })
        .unwrap();
    let how = Recording {
        fail_at: Some(at),
        ..base
    };
    let (_, promised) = clean_run(how);
    assert!(!promised.rows.is_empty());
    crash_test(how)
        .outcomes(vec![
            CrashOutcome::LoseUnsynced,
            CrashOutcome::KeepAll,
            CrashOutcome::Partial { seed: 7 },
        ])
        .run()
        .unwrap();
}

/// Plants a committed row on `fs` for `range` of `track`, hashed over
/// `hashed`, and leaves `file` (durably) under the row's name.
fn plant_row(
    fs: &FakeFs,
    track: TrackId,
    range: SampleRange,
    hashed: &[u8],
    file: &[u8],
) -> SegmentRow {
    let durable = TempSegment::write(fs, &session(), track, EpochId::new(0), range, hashed)
        .unwrap()
        .sync()
        .unwrap()
        .rename(fs)
        .unwrap()
        .sync_dir(fs)
        .unwrap();
    let row = *durable.row();
    durable.commit(&mut FakeStore::new(fs, &db())).unwrap();
    if file != hashed {
        let path = durable_path(track, range);
        fs.remove(&path).unwrap();
        let mut out = fs.create(&path).unwrap();
        out.write_all(file).unwrap();
        out.sync().unwrap();
        fs.sync_dir(&session()).unwrap();
    }
    row
}

fn durable_path(track: TrackId, range: SampleRange) -> PathBuf {
    session().join(segment_file_name(track, range))
}

fn flac_of(track: TrackId, from: u64, len: u64) -> Vec<u8> {
    flac::encode(rate(), &[&samples(track, from, len)]).unwrap()
}

#[test]
fn a_row_whose_file_doesnt_match_never_lets_a_journal_go_at_any_crash() {
    let (fs, promised) = clean_run(Recording {
        steps: 4,
        publish: false,
        fail_at: None,
    });
    let range = |a, b| SampleRange::new(SampleIndex::new(a), SampleIndex::new(b)).unwrap();
    for outcome in [
        CrashOutcome::LoseUnsynced,
        CrashOutcome::KeepAll,
        CrashOutcome::Partial { seed: 5 },
    ] {
        let disk = fs.crash(outcome);
        // A wrong hash: the file under the row's name holds other audio
        // (another session's, or a store restored out of step).
        let wrong_hash = plant_row(
            &disk,
            MIC,
            range(0, 1_000),
            &flac_of(MIC, 0, 1_000),
            &flac::encode(rate(), &[&[0; 1_000]]).unwrap(),
        );
        // The same name with a different range: the file's hash is the
        // row's, but it holds 300 samples where the row claims 800. A check
        // of the hash alone would let the row claim 700..1,500 and the
        // journals holding 1,000..1,500 go.
        let short = flac_of(SYSTEM, 700, 300);
        let wrong_range = plant_row(&disk, SYSTEM, range(700, 1_500), &short, &short);
        let bad = [wrong_hash, wrong_range];
        let planted: BTreeMap<PathBuf, Vec<u8>> = bad
            .iter()
            .map(|r| {
                let path = durable_path(r.track(), r.range());
                let bytes = disk.read(&path).unwrap();
                (path, bytes)
            })
            .collect();

        // Uninterrupted: both rows are reported, claim nothing, and keep
        // their journals and files; the rest is published.
        let probe = disk.copy_disk();
        let done = salvage(&mut session_store(&probe), length()).unwrap();
        assert_eq!(done.mismatched(), bad, "{outcome:?}");
        assert!(!done.segments().is_empty());
        assert!(!done.deleted().is_empty());
        let uninterrupted = observe(&probe);
        check_mismatch_kept(&promised, &bad, &planted, &uninterrupted)
            .unwrap_or_else(|e| panic!("{outcome:?}: {e}"));

        // Crashed after every operation, under every outcome, then run
        // again: the same end state.
        let ops = probe.attempted();
        for after in 0..=ops {
            for crash in CrashOutcome::standard() {
                let run = disk.copy_disk();
                run.crash_after(after);
                let _ = salvage(&mut session_store(&run), length());
                let survived = run.crash(crash);
                let mut again = session_store(&survived);
                let rerun = salvage(&mut again, length())
                    .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
                assert_eq!(rerun.mismatched(), bad);
                let seen = observe(&survived);
                check_mismatch_kept(&promised, &bad, &planted, &seen)
                    .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
                assert!(
                    seen == uninterrupted,
                    "salvage crashed after {after} ops, {crash:?}, ended differently"
                );
            }
        }
    }
}

/// After salvage with mismatched rows `bad`: their files are as they were,
/// every promised sample is in a good row's file or a journal, and every
/// sample in a bad row's range is still in a journal.
fn check_mismatch_kept(
    promised: &Promised,
    bad: &[SegmentRow],
    planted: &BTreeMap<PathBuf, Vec<u8>>,
    seen: &Observed,
) -> Result<(), String> {
    for (path, bytes) in planted {
        if seen.files.get(path) != Some(bytes) {
            return Err(format!("{} changed", path.display()));
        }
    }
    let good = Observed {
        files: seen.files.clone(),
        rows: Ok(seen
            .rows
            .clone()?
            .into_iter()
            .filter(|r| !bad.contains(r))
            .collect()),
    };
    let in_rows = row_samples(&good)?;
    let in_journals = journal_samples(seen)?;
    let held = in_rows.union(&in_journals).copied().collect();
    check_durable(promised, &held)?;
    for row in bad {
        let r = row.range();
        let end = r.end().min(promised.durable[&row.track()]);
        if let Some(s) =
            (r.start().get()..end.get()).find(|&s| !in_journals.contains(&(row.track(), s)))
        {
            return Err(format!(
                "track {} sample {s}, claimed by a mismatched row, is in no journal",
                row.track().get()
            ));
        }
    }
    Ok(())
}

#[test]
fn recording_with_the_store_down_loses_nothing_once_it_is_back() {
    // The store's directory doesn't exist yet: every publish fails.
    let fs = FakeFs::with_dirs([session()]);
    let (clock, dyn_clock) = fake_clock();
    let mut writer = SessionWriter::open(&session_dir(&fs), rate(), length(), dyn_clock).unwrap();
    let mut store = session_store(&fs);
    let mut promised = Promised::default();
    for (track, at) in [(MIC, 100_u64), (SYSTEM, 0)] {
        writer
            .start_track(track, EpochId::new(0), SampleIndex::new(at))
            .unwrap();
        promised.started.insert(track, SampleIndex::new(at));
    }
    let mut pending = Vec::new();
    let mut failures = 0;
    for len in [700_u64, 3_100, 20, 1_480, 2_000] {
        for track in [MIC, SYSTEM] {
            let from = writer.next_sample(track).unwrap().get();
            writer.append(track, &samples(track, from, len)).unwrap();
        }
        clock.advance(SampleCount::new(len).duration_at(rate()).unwrap());
        writer.sync_if_due().unwrap();
        pending.extend(writer.take_finished());
        let err = publish_journals(&mut store, length(), &pending).unwrap_err();
        assert!(matches!(err, PublishError::Store(_)), "{err}");
        failures += 1;
    }
    for track in [MIC, SYSTEM] {
        promised
            .durable
            .insert(track, writer.next_sample(track).unwrap());
    }
    pending.extend(writer.finish().unwrap());
    assert_eq!(failures, 5);
    // Journals rotated at every window while the store was down.
    let journals = fs.paths().into_iter().filter(|p| is_journal(p)).count();
    assert_eq!(journals, pending.len());
    assert!(journals >= 10, "{journals}");

    // The store comes back.
    fs.create_dir(&db()).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    // Salvage of a copy, crashed anywhere, loses nothing...
    salvage_crashed_everywhere(&fs.copy_disk(), &promised);
    // ...and the same handle publishes everything that waited.
    let done = publish_journals(&mut store, length(), &pending).unwrap();
    assert_eq!(done.deleted().len(), journals);
    promised.rows = done.segments().to_vec();
    check_after(&promised, &observe(&fs)).unwrap();
}
