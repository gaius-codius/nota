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
use crate::session::{
    FinishedJournal, MARKS_FILE_NAME, SessionDir, SessionError, SessionLock, SessionStore,
    SessionWriter, Use,
};
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

/// The session, owned.
fn owned(fs: &FakeFs) -> SessionLock<FakeFs> {
    session_dir(fs).lock().unwrap()
}

/// The session's store, in the usual place, under its own ownership.
fn session_store(fs: &FakeFs) -> SessionStore<FakeFs, FakeStore> {
    store_on(&owned(fs))
}

/// The session's store, in the usual place, under the ownership `lock`
/// (a writer's, say).
fn store_on(lock: &SessionLock<FakeFs>) -> SessionStore<FakeFs, FakeStore> {
    let fs = lock.session().fs();
    SessionStore::new(lock.clone(), FakeStore::new(fs, &db()))
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
        .map(|&n| FinishedJournal::new(SESSION, JournalId::new(n)))
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
    /// Per track, the end of what the writer accepted: every call that
    /// returned without an error. A failed call's samples are a gap the
    /// writer reported, not a promise.
    captured: BTreeMap<TrackId, SampleIndex>,
    /// Rows publishing reported committed.
    rows: Vec<SegmentRow>,
}

impl Promised {
    fn note<S: Fs>(&mut self, writer: &SessionWriter<S>, track: TrackId, ok: bool) {
        if ok && let Some(next) = writer.next_sample(track) {
            self.captured.insert(track, next);
        }
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
    record_chunks(fs, how, &[250, 100, 400, 1_600, 50], promised)
}

/// Records as [`record_into`] does, in chunks of `sizes`, in turn.
fn record_chunks(
    fs: &FakeFs,
    how: Recording,
    sizes: &[u64],
    promised: &mut Promised,
) -> Result<(), Box<dyn Error>> {
    let (clock, dyn_clock) = fake_clock();
    let lock = owned(fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock)?;
    let mut store = store_on(&lock);
    for (track, at) in [(MIC, 0_u64), (SYSTEM, 700)] {
        writer.start_track(track, EpochId::new(0), SampleIndex::new(at))?;
        promised.started.insert(track, SampleIndex::new(at));
        promised.durable.insert(track, SampleIndex::new(at));
        promised.captured.insert(track, SampleIndex::new(at));
    }
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
        promised.captured.insert(track, end);
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

/// A set of samples, per track: sorted, merged ranges rather than a
/// node per sample, so checking a disk costs per range.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Held(BTreeMap<TrackId, Vec<(u64, u64)>>);

impl Held {
    /// The samples of `ranges`, which may overlap.
    fn of(ranges: impl IntoIterator<Item = (TrackId, u64, u64)>) -> Self {
        let mut by_track: BTreeMap<TrackId, Vec<(u64, u64)>> = BTreeMap::new();
        for (track, from, to) in ranges {
            if from < to {
                by_track.entry(track).or_default().push((from, to));
            }
        }
        for ranges in by_track.values_mut() {
            ranges.sort_unstable();
            let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
            for &(from, to) in ranges.iter() {
                match merged.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => merged.push((from, to)),
                }
            }
            *ranges = merged;
        }
        Self(by_track)
    }

    fn ranges(&self) -> impl Iterator<Item = (TrackId, u64, u64)> + '_ {
        self.0
            .iter()
            .flat_map(|(&t, ranges)| ranges.iter().map(move |&(a, b)| (t, a, b)))
    }

    fn union(&self, other: &Self) -> Self {
        Self::of(self.ranges().chain(other.ranges()))
    }

    fn contains(&self, &(track, s): &(TrackId, u64)) -> bool {
        let Some(ranges) = self.0.get(&track) else {
            return false;
        };
        // The last range starting at or before `s`.
        let i = ranges.partition_point(|&(from, _)| from <= s);
        i > 0 && s < ranges[i - 1].1
    }

    /// The first of `track`'s samples `from..to` not held.
    fn first_missing(&self, track: TrackId, from: u64, to: u64) -> Option<u64> {
        let mut at = from;
        for &(a, b) in self.0.get(&track).map_or(&[][..], Vec::as_slice) {
            if at >= to || a > at {
                break;
            }
            at = at.max(b);
        }
        (at < to).then_some(at)
    }
}

proptest::proptest! {
    /// `Held` answers as a set of every sample would.
    #[test]
    fn held_answers_like_a_set_of_samples(
        a in proptest::collection::vec((0_u32..2, 0_u64..60, 0_u64..20), 0..8),
        b in proptest::collection::vec((0_u32..2, 0_u64..60, 0_u64..20), 0..8),
        from in 0_u64..80,
        len in 0_u64..40,
    ) {
        let ranges = |v: &[(u32, u64, u64)]| -> Vec<(TrackId, u64, u64)> {
            v.iter().map(|&(t, at, n)| (TrackId::new(t), at, at + n)).collect()
        };
        let set = |v: &[(TrackId, u64, u64)]| -> BTreeSet<(TrackId, u64)> {
            v.iter().flat_map(|&(t, x, y)| (x..y).map(move |s| (t, s))).collect()
        };
        let (ra, rb) = (ranges(&a), ranges(&b));
        let (ha, hb) = (Held::of(ra.clone()), Held::of(rb.clone()));
        let (sa, sb) = (set(&ra), set(&rb));
        let union = ha.union(&hb);
        let su: BTreeSet<_> = sa.union(&sb).copied().collect();
        for track in [TrackId::new(0), TrackId::new(1)] {
            for s in 0..100 {
                proptest::prop_assert_eq!(ha.contains(&(track, s)), sa.contains(&(track, s)));
                proptest::prop_assert_eq!(union.contains(&(track, s)), su.contains(&(track, s)));
            }
            let want = (from..from + len).find(|&s| !su.contains(&(track, s)));
            proptest::prop_assert_eq!(union.first_missing(track, from, from + len), want);
        }
        // Merged: no two ranges of a track touch or overlap.
        for ranges in union.0.values() {
            proptest::prop_assert!(ranges.windows(2).all(|w| w[0].1 < w[1].0));
        }
    }
}

/// Per track, the samples held by valid journal frames, each checked
/// against what was recorded.
fn journal_samples(seen: &Observed) -> Result<Held, String> {
    let mut held = Vec::new();
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
            held.push((frame.track(), r.start().get(), r.end().get()));
        }
    }
    Ok(Held::of(held))
}

/// Checks every row has its file, holding exactly the row's audio, and no
/// two rows overlap; returns the samples the rows hold.
fn row_samples(seen: &Observed) -> Result<Held, String> {
    let rows = seen.rows.clone()?;
    let mut held = Vec::new();
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
        held.push((row.track(), r.start().get(), r.end().get()));
    }
    // Sorted by track then start, a row overlaps an earlier one if it
    // starts before the furthest end so far.
    held.sort_unstable();
    let mut furthest: Option<(TrackId, u64)> = None;
    for &(track, from, to) in &held {
        if let Some((t, end)) = furthest
            && t == track
            && from < end
        {
            return Err(format!(
                "rows overlap at track {} sample {from}",
                track.get()
            ));
        }
        let end = match furthest {
            Some((t, end)) if t == track => end.max(to),
            _ => to,
        };
        furthest = Some((track, end));
    }
    Ok(Held::of(held))
}

/// Every promised-durable sample is in `held`.
fn check_durable(promised: &Promised, held: &Held) -> Result<(), String> {
    for (&track, &start) in &promised.started {
        let end = promised.durable[&track];
        if let Some(s) = held.first_missing(track, start.get(), end.get()) {
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
    if let Some(orphan) = after.files.keys().find(|p| {
        p.starts_with(session())
            && !named.contains(*p)
            && p.file_name() != Some(FINDINGS_FILE_NAME.as_ref())
            && p.file_name() != Some(MARKS_FILE_NAME.as_ref())
    }) {
        return Err(format!("a file without a row: {}", orphan.display()));
    }
    Ok(())
}

/// The most audio that may be captured but not durable: the journal's
/// 850 ms sync interval at the test's rate, which keeps durable within the
/// 1.1 s bounded-loss rule of the audio delivered.
const LAG_LIMIT: u64 = 850;

/// The most any track had captured but not durable when the recording
/// stopped, in samples.
fn worst_lag(promised: &Promised) -> u64 {
    promised
        .captured
        .iter()
        .map(|(track, captured)| {
            // Every started track has a durable position; a missing one
            // counts from zero, so it can only fail the check.
            let durable = promised.durable.get(track).map_or(0, |d| d.get());
            captured.get().saturating_sub(durable)
        })
        .max()
        .unwrap_or(0)
}

/// No track's journals hold more than [`LAG_LIMIT`] written past what was
/// fsync'd, measured on the disk the crash left when it kept everything
/// (`seen`), from the scenario's operations: for frames a call wrote before
/// it failed, which the writer's word doesn't cover.
fn check_written_lag(case: &CrashCase, promised: &Promised, seen: &Observed) -> Result<(), String> {
    // Per journal, how many of its bytes an fsync covered.
    let mut written: BTreeMap<&Path, usize> = BTreeMap::new();
    let mut synced: BTreeMap<&Path, usize> = BTreeMap::new();
    for op in &case.ops {
        match op {
            Op::Write { path, len } if is_journal(path) => {
                *written.entry(path).or_default() += len;
            }
            Op::Sync(path) if is_journal(path) => {
                synced.insert(path, written.get(path.as_path()).copied().unwrap_or(0));
            }
            _ => {}
        }
    }
    // Per track, the furthest sample written, and the furthest durable:
    // in a row (its journal synced before it was published), or in an
    // fsync'd part of a journal.
    let mut ends: BTreeMap<TrackId, (u64, u64)> = promised
        .durable
        .iter()
        .map(|(&t, d)| (t, (d.get(), d.get())))
        .collect();
    let mut note = |track: TrackId, end: u64, durable: bool| {
        let e = ends.entry(track).or_insert((0, 0));
        e.0 = e.0.max(end);
        if durable {
            e.1 = e.1.max(end);
        }
    };
    for row in seen.rows.as_ref().map_or(&[][..], Vec::as_slice) {
        note(row.track(), row.range().end().get(), true);
    }
    for (path, bytes) in &seen.files {
        if !is_journal(path) {
            continue;
        }
        let read = read_journal(bytes);
        let Some(track) = read.header().map(crate::journal::JournalHeader::track) else {
            continue;
        };
        if let Some(r) = read.range() {
            note(track, r.end().get(), false);
        }
        let len = synced
            .get(path.as_path())
            .copied()
            .unwrap_or(0)
            .min(bytes.len());
        if let Some(r) = read_journal(&bytes[..len]).range() {
            note(track, r.end().get(), true);
        }
    }
    for (track, (end, durable)) in ends {
        let lag = end.saturating_sub(durable);
        if lag > LAG_LIMIT {
            return Err(format!(
                "track {}: {lag} samples written but not durable (written to {end}, durable to {durable})",
                track.get()
            ));
        }
    }
    Ok(())
}

#[test]
fn the_written_lag_counts_frames_past_the_last_fsync() {
    // A journal holding `to` samples of MIC, fsync'd only up to 600: the
    // writer's word (nothing captured) hides the rest, the disk doesn't.
    let lag_of = |to: u64| {
        let fs = FakeFs::with_dirs([session(), db()]);
        let path = session().join(JournalId::new(0).file_name());
        let header =
            crate::journal::JournalHeader::new(JournalId::new(0), MIC, EpochId::new(0), rate());
        let mut file = fs.create(&path).unwrap();
        file.write_all(&crate::journal::format::encode_header(header))
            .unwrap();
        let mut frames = Vec::new();
        crate::journal::format::encode_frame(
            &mut frames,
            0,
            MIC,
            SampleIndex::ZERO,
            &samples(MIC, 0, 600),
        );
        file.write_all(&frames).unwrap();
        file.sync().unwrap();
        frames.clear();
        crate::journal::format::encode_frame(
            &mut frames,
            1,
            MIC,
            SampleIndex::new(600),
            &samples(MIC, 600, to - 600),
        );
        file.write_all(&frames).unwrap();
        let case = CrashCase {
            after_ops: fs.attempted(),
            ops: fs.ops(),
            outcome: CrashOutcome::KeepAll,
            recovery_crashes: Vec::new(),
            survived: fs.copy_disk(),
            fs: fs.copy_disk(),
        };
        let promised = Promised {
            started: BTreeMap::from([(MIC, SampleIndex::ZERO)]),
            durable: BTreeMap::from([(MIC, SampleIndex::ZERO)]),
            captured: BTreeMap::from([(MIC, SampleIndex::ZERO)]),
            rows: Vec::new(),
        };
        assert_eq!(worst_lag(&promised), 0);
        check_written_lag(&case, &promised, &observe(&fs))
    };
    // 850 past the fsync: within the limit.
    lag_of(1_450).unwrap();
    // 851: past it.
    let err = lag_of(1_451).unwrap_err();
    assert!(err.contains("851 samples written but not durable"), "{err}");
}

/// The invariants, at any crash point:
/// - at the crash: no track had more than [`LAG_LIMIT`] captured but not
///   durable, counting what the writer accepted and, when the crash kept
///   everything, every journal frame written, even by a call that failed;
/// - before salvage: no row without its file; every durable sample is in a
///   row's file or a journal; nothing misread;
/// - after salvage: only segments and rows are left, holding every durable
///   sample and every committed row;
/// - salvage again changes nothing;
/// - if salvage was crashed, it ended byte for byte as an uninterrupted
///   salvage of what the recording's crash left.
fn check(case: &CrashCase, promised: &Promised, got: &Recovered) -> Result<(), String> {
    let lag = worst_lag(promised);
    if lag > LAG_LIMIT {
        return Err(format!(
            "{lag} samples captured but not durable: captured {:?}, durable {:?}",
            promised.captured, promised.durable
        ));
    }
    let in_rows = row_samples(&got.before).map_err(|e| format!("before salvage: {e}"))?;
    let in_journals = journal_samples(&got.before).map_err(|e| format!("before salvage: {e}"))?;
    if case.outcome == CrashOutcome::KeepAll && case.recovery_crashes.is_empty() {
        // The disk as the crash found it: frames written mid-call count.
        check_written_lag(case, promised, &got.before)?;
    }
    let held = in_rows.union(&in_journals);
    check_durable(promised, &held).map_err(|e| format!("before salvage: {e}"))?;
    let after = got.after.clone()?;
    check_after(promised, &after).map_err(|e| format!("after salvage: {e}"))?;
    if got.again.as_ref() != Ok(&after) {
        return Err("a second salvage changed something".to_owned());
    }
    // Uncrashed, salvage ran on that very disk: nothing to compare.
    if !case.recovery_crashes.is_empty() {
        let uninterrupted = uninterrupted_salvage(&case.survived)?;
        if after != uninterrupted {
            return Err(format!(
                "ended differently from an uninterrupted salvage: {}",
                differences(&after, &uninterrupted)
            ));
        }
    }
    Ok(())
}

/// What an uninterrupted salvage of `disk` ends with. A case's recovery
/// crashes all start from the same disk, so the last answer is kept, keyed
/// by the whole disk.
fn uninterrupted_salvage(disk: &FakeFs) -> Result<Observed, String> {
    thread_local! {
        static LAST: std::cell::RefCell<Option<(Observed, Observed)>> =
            const { std::cell::RefCell::new(None) };
    }
    let before = observe(disk);
    if let Some(after) = LAST.with_borrow(|last| {
        last.as_ref()
            .filter(|(seen, _)| *seen == before)
            .map(|(_, after)| after.clone())
    }) {
        return Ok(after);
    }
    let after = salvage_fake(&disk.copy_disk())?;
    LAST.set(Some((before, after.clone())));
    Ok(after)
}

/// What differs between two disks, briefly: the paths whose bytes differ,
/// and whether the rows do.
fn differences(a: &Observed, b: &Observed) -> String {
    let paths: BTreeSet<&PathBuf> = a.files.keys().chain(b.files.keys()).collect();
    let files: Vec<String> = paths
        .into_iter()
        .filter(|p| a.files.get(*p) != b.files.get(*p))
        .map(|p| p.display().to_string())
        .collect();
    format!("files {files:?}, rows differ: {}", a.rows != b.rows)
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
fn recording_and_salvage_crashed_anywhere_end_as_an_uninterrupted_salvage() {
    let how = Recording {
        steps: 7,
        publish: true,
        fail_at: None,
    };
    let (clean, promised) = clean_run(how);
    // Several segments per track, published live, so crashes land in every
    // publish step: a segment is published before the last audio is
    // journaled.
    assert!(promised.rows.len() >= 4, "{:?}", promised.rows);
    let ops = clean.ops();
    let first_row = ops
        .iter()
        .position(
            |op| matches!(op, Op::Rename { to, .. } if to.extension().is_some_and(|e| e == "flac")),
        )
        .unwrap();
    let last_journal = ops
        .iter()
        .rposition(|op| matches!(op, Op::Write { path, .. } if is_journal(path)))
        .unwrap();
    assert!(first_row < last_journal, "nothing published live");

    // Every crash point and outcome of the recording. Salvage is crashed at
    // a sample of its points per case, the sample moving along from case to
    // case, each with three outcomes; the re-runs of some of those are
    // crashed at a sample of their points too.
    let worst = std::cell::Cell::new(0);
    let summary = CrashTest::new(
        move |fs: &FakeFs| record(fs, how),
        recover,
        |case: &CrashCase, promised: &Promised, got: &Recovered| {
            worst.set(worst.get().max(worst_lag(promised)));
            check(case, promised, got)
        },
    )
    .dirs([session(), db()])
    .recovery_outcomes(vec![
        CrashOutcome::LoseUnsynced,
        CrashOutcome::KeepAll,
        CrashOutcome::Partial { seed: 5 },
    ])
    .sample_recovery(SAMPLE)
    .crash_rerun(RERUN)
    .run()
    .unwrap_or_else(|failure| panic!("{failure}"));
    assert!(summary.scenario_ops > 100, "{summary:?}");
    assert!(summary.recovery_crashed > 1_000, "{summary:?}");
    assert!(summary.rerun_crashed > 100, "{summary:?}");
    // Not vacuous: some crash came with most of the sync interval unsynced.
    assert!(worst.get() >= 700, "{}", worst.get());
}

#[test]
fn a_recording_in_small_chunks_never_lags_past_the_limit_at_any_crash() {
    // 25 ms chunks, as a capture callback might deliver them, over two
    // windows with live publishing: the lag climbs to the sync budget
    // before every sync, so a budget past the limit shows.
    let how = Recording {
        steps: 120,
        publish: true,
        fail_at: None,
    };
    let scenario = move |fs: &FakeFs| {
        let mut promised = Promised::default();
        let _ = record_chunks(fs, how, &[25], &mut promised);
        promised
    };
    let worst = std::cell::Cell::new(0);
    let summary = CrashTest::new(
        scenario,
        recover,
        |case: &CrashCase, promised: &Promised, got: &Recovered| {
            worst.set(worst.get().max(worst_lag(promised)));
            check(case, promised, got)
        },
    )
    .dirs([session(), db()])
    .outcomes(vec![CrashOutcome::LoseUnsynced, CrashOutcome::KeepAll])
    .run()
    .unwrap_or_else(|failure| panic!("{failure}"));
    assert!(summary.scenario_ops > 240, "{summary:?}");
    // Within one chunk of the limit: the budget is what bounds the lag.
    assert!(worst.get() >= LAG_LIMIT - 25, "{}", worst.get());
}

/// Salvage takes fewer than this many operations here, so each case crashes
/// it at one point, with each of three outcomes.
const SAMPLE: usize = 200;
/// Coprime with the three recovery outcomes and the ten scenario outcomes,
/// so the crashed re-runs fall on every combination of them, not a fixed
/// few.
const RERUN: usize = 23;

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
    // The full product on a short recording with live publishing: every
    // crash point of the recording with three outcomes, and salvage crashed
    // after each of its operations with the same outcome. Mixed outcomes and
    // crashed re-runs are in the long test above.
    let how = Recording {
        steps: 2,
        publish: true,
        fail_at: None,
    };
    let summary = crash_test(how)
        .outcomes(vec![
            CrashOutcome::LoseUnsynced,
            CrashOutcome::KeepAll,
            CrashOutcome::Partial { seed: 3 },
        ])
        .crash_recovery()
        .run()
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert!(
        summary.recovery_crashed > 3 * (summary.scenario_ops + 1),
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
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock).unwrap();
    // The store's directory doesn't exist: every publish fails.
    let mut store = SessionStore::new(lock.clone(), FakeStore::new(&fs, Path::new("/missing")));
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
    // Every failed run said the rows couldn't be checked.
    let findings = read_findings(&session_dir(&fs)).unwrap();
    assert_eq!(findings.verification(), Verification::Unavailable);
    assert!(findings.found().is_empty());

    let mut covered = 100;
    let mut journals = 0;
    for path in fs.paths().into_iter().filter(|p| is_journal(p)) {
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
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    writer.append(MIC, &samples(MIC, 0, 400)).unwrap();
    writer.new_epoch(MIC, EpochId::new(1)).unwrap();
    writer.append(MIC, &samples(MIC, 400, 200)).unwrap();
    let ids = writer.finish().unwrap();
    assert_eq!(ids, finished(&[0, 1]));
    let mut store = store_on(&lock);
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
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
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
    let mut missing = SessionStore::new(owned(&fs), FakeStore::new(&fs, Path::new("/missing")));
    let err = salvage(&mut missing, length()).unwrap_err();
    drop(missing);
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
    let owner = ours.lock().unwrap();
    let mut writer = SessionWriter::open(&owner, rate(), length(), dyn_clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(2), SampleIndex::new(10))
        .unwrap();
    writer.append(MIC, &samples(MIC, 10, 3_000)).unwrap();
    clock.advance(std::time::Duration::from_secs(3));
    writer.sync_if_due().unwrap();
    // Stop without finishing, as a crash would.
    drop(writer);

    let mut store = nota_store::Store::open(&dir.0.join("nota.db")).unwrap();
    let done = salvage(&mut SessionStore::new(owner.clone(), &mut store), length()).unwrap();
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
        salvage(&mut SessionStore::new(owner, &mut store), length()).unwrap(),
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
    let other =
        PublishError::OtherSession(FinishedJournal::new(SessionId::new(9), JournalId::new(4)));
    assert!(other.to_string().contains("journal-000004"));
    assert!(other.to_string().contains('9'));
    assert!(other.source().is_none());
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
    // Salvage publishes nothing over the row's samples (the store would
    // refuse it anyway), keeps the journal holding them, records the row,
    // and publishes the rest.
    let done = salvage(&mut store, length()).unwrap();
    assert_eq!(
        done.findings()
            .iter()
            .map(|f| (*f.row(), f.problem()))
            .collect::<Vec<_>>(),
        [(row[0], Problem::Missing)]
    );
    assert_eq!(done.findings_unsaved(), None);
    // The mic's audio is all in the row's window; the system track's isn't.
    assert!(!done.segments().is_empty());
    assert!(done.segments().iter().all(|r| r.track() == SYSTEM));
    let journals_after: Vec<_> = fs.paths().into_iter().filter(|p| is_journal(p)).collect();
    assert!(journals_after.contains(&session().join(JournalId::new(0).file_name())));
    assert!(
        !fs.paths()
            .contains(&session().join("seg-t0-000000000000.flac"))
    );
    let recorded = read_findings(&session_dir(&fs)).unwrap();
    assert_eq!(recorded.found(), done.findings());
    assert_eq!(recorded.verification(), Verification::Done);
}

#[test]
fn a_journal_corrupt_before_its_end_is_published_then_set_aside() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let (clock, dyn_clock) = fake_clock();
    let long = SegmentLength::new(1_000_000).unwrap();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), long, dyn_clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    // Half a sync interval at a time, so every frame is 425 samples.
    for k in 0..120 {
        writer.append(MIC, &samples(MIC, k * 425, 425)).unwrap();
        clock.advance(std::time::Duration::from_millis(425));
        writer.sync_if_due().unwrap();
    }
    writer.finish().unwrap();
    let path = session().join(JournalId::FIRST.file_name());
    let mut bytes = fs.read(&path).unwrap();
    // Flip a sample byte in the third frame: two frames read, then 117
    // frames of audio that can't be.
    let frame = FRAME_HEADER_LEN + 850;
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
    assert_eq!(ranges, [(0, 850)]);
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

/// A finished 16 kHz journal of `seconds`, a second per append, and the
/// offsets where its frames start.
fn journal_at_16_khz(seconds: u64) -> (Vec<u8>, Vec<(usize, u64)>) {
    let fs = FakeFs::with_dirs([session(), db()]);
    let (clock, dyn_clock) = fake_clock();
    let hz = SampleRate::new(16_000).unwrap();
    let long = SegmentLength::new(1_000_000_000).unwrap();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, hz, long, dyn_clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    for k in 0..seconds {
        writer
            .append(MIC, &samples(MIC, k * 16_000, 16_000))
            .unwrap();
        clock.advance(std::time::Duration::from_secs(1));
        writer.sync_if_due().unwrap();
    }
    writer.finish().unwrap();
    let bytes = fs
        .read(&session().join(JournalId::FIRST.file_name()))
        .unwrap();
    let mut starts = Vec::new();
    let mut at = HEADER_LEN;
    for frame in read_journal(&bytes).frames() {
        starts.push((at, frame.range().start().get()));
        at += FRAME_HEADER_LEN + 2 * frame.samples().len();
    }
    (bytes, starts)
}

/// Salvages `bytes` as the session's only journal.
fn salvage_bytes(bytes: &[u8]) -> Published {
    let disk = FakeFs::with_dirs([session(), db()]);
    let mut file = disk
        .create(&session().join(JournalId::FIRST.file_name()))
        .unwrap();
    file.write_all(bytes).unwrap();
    salvage(
        &mut session_store(&disk),
        SegmentLength::new(1_000_000_000).unwrap(),
    )
    .unwrap()
}

#[test]
fn a_journal_corrupt_well_before_its_end_is_kept_whatever_its_frame_size() {
    // At 16 kHz with real frame sizes, 15 s of frames are fewer bytes than
    // a second of one-sample frames: the byte count alone can't see it.
    let (bytes, starts) = journal_at_16_khz(20);
    let (at, first) = *starts
        .iter()
        .find(|&&(_, first)| first >= 5 * 16_000)
        .unwrap();
    let mut corrupt = bytes.clone();
    corrupt[at + FRAME_HEADER_LEN + 3] ^= 0x10;
    let done = salvage_bytes(&corrupt);
    let ranges: Vec<_> = done
        .segments()
        .iter()
        .map(|r| (r.range().start().get(), r.range().end().get()))
        .collect();
    assert_eq!(ranges, [(0, first)]);
    assert!(done.deleted().is_empty());
    assert_eq!(done.quarantined().len(), 1);

    // Damage within the last sync interval (13 600 samples) can't be told
    // from a crash's torn tail, whose frames may be written back out of
    // order: deleted.
    let (at, first) = starts[starts.len() - 2];
    let last = starts.last().unwrap().1;
    assert!(last > first, "a frame after the damage");
    assert!(last < first + 13_600, "{first}..{last}");
    let mut torn = bytes;
    torn[at + FRAME_HEADER_LEN + 3] ^= 0x10;
    let done = salvage_bytes(&torn);
    assert_eq!(
        done.segments()
            .iter()
            .map(|r| r.range().end().get())
            .collect::<Vec<_>>(),
        [first]
    );
    assert_eq!(done.deleted(), [JournalId::FIRST]);
    assert!(done.quarantined().is_empty());
}

#[test]
fn a_journal_corrupt_from_its_first_frame_is_kept_if_audio_after_was_synced() {
    // No valid frame to measure from: the frames found after the damage
    // are measured against each other. Over three seconds they span more
    // than a crash can leave unsynced.
    let corrupt_first = |seconds| {
        let (mut bytes, _) = journal_at_16_khz(seconds);
        bytes[HEADER_LEN + FRAME_HEADER_LEN + 3] ^= 0x10;
        salvage_bytes(&bytes)
    };
    let done = corrupt_first(3);
    assert!(done.segments().is_empty());
    assert!(done.deleted().is_empty());
    assert_eq!(done.quarantined().len(), 1);
    // A second of audio, in frames starting 5 408 samples apart after the
    // damage: within the sync interval, so they could all be a torn tail:
    // deleted.
    let done = corrupt_first(1);
    assert!(done.segments().is_empty());
    assert_eq!(done.deleted(), [JournalId::FIRST]);
    assert!(done.quarantined().is_empty());
}

#[test]
fn frames_spanning_the_sync_interval_after_damage_keep_the_journal() {
    // At 1 kHz the writer syncs every 850 samples. Damage in the first of
    // `count` 50-sample frames: the frames after it start at 50, 100, ...;
    // once they reach 50 + 850, one of them must have been fsync'd, so the
    // damage is corruption and the journal is kept. Short of that, it could
    // all be a torn tail, and the journal is deleted.
    let corrupt_first = |count: u64| {
        let header =
            crate::journal::JournalHeader::new(JournalId::FIRST, MIC, EpochId::new(0), rate());
        let mut bytes = crate::journal::format::encode_header(header).to_vec();
        for k in 0..count {
            crate::journal::format::encode_frame(
                &mut bytes,
                k,
                MIC,
                SampleIndex::new(k * 50),
                &samples(MIC, k * 50, 50),
            );
        }
        bytes[HEADER_LEN + FRAME_HEADER_LEN + 3] ^= 0x10;
        salvage_bytes(&bytes)
    };
    // Frames start up to 900: kept.
    let done = corrupt_first(19);
    assert!(done.segments().is_empty());
    assert!(done.deleted().is_empty());
    assert_eq!(done.quarantined().len(), 1);
    // Up to 850: within the interval, so deleted.
    let done = corrupt_first(18);
    assert!(done.segments().is_empty());
    assert_eq!(done.deleted(), [JournalId::FIRST]);
    assert!(done.quarantined().is_empty());
}

#[test]
fn unreadable_bytes_past_what_a_crash_can_leave_keep_the_journal() {
    // No frame after the damage: only the byte count tells. The most a
    // crash can leave at 16 kHz: the sync interval's 13 600 samples as
    // one-sample frames, and one largest frame.
    let (bytes, _) = journal_at_16_khz(2);
    let most = 13_600 * (FRAME_HEADER_LEN + 2) + FRAME_HEADER_LEN + 2 * 8_192;
    let with_tail = |n: usize| {
        let mut out = bytes.clone();
        out.extend(std::iter::repeat_n(0xA5, n));
        out
    };
    let done = salvage_bytes(&with_tail(most));
    assert_eq!(done.deleted(), [JournalId::FIRST]);
    assert!(done.quarantined().is_empty());
    let done = salvage_bytes(&with_tail(most + 1));
    assert!(done.deleted().is_empty());
    assert_eq!(done.quarantined().len(), 1);
    assert_eq!(
        done.segments()
            .iter()
            .map(|r| r.range().end().get())
            .collect::<Vec<_>>(),
        [32_000]
    );
}

#[test]
fn a_corrupt_journal_in_a_bad_window_keeps_its_name_until_it_publishes() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let (_, clock) = fake_clock();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    writer.append(MIC, &samples(MIC, 0, 1_000)).unwrap();
    let finished = writer.finish().unwrap();
    // Garbage well past what a crash can leave: corruption.
    let path = session().join(JournalId::FIRST.file_name());
    let mut bytes = fs.read(&path).unwrap();
    bytes.extend(std::iter::repeat_n(0xA5, 200_000));
    fs.remove(&path).unwrap();
    let mut file = fs.create(&path).unwrap();
    file.write_all(&bytes).unwrap();
    file.sync().unwrap();
    let range = SampleRange::new(SampleIndex::new(100), SampleIndex::new(200)).unwrap();
    let flac = flac_of(MIC, 100, 100);
    let missing = plant_row(&fs, MIC, range, &flac, &flac);
    let row_path = durable_path(MIC, range);
    fs.remove(&row_path).unwrap();
    fs.sync_dir(&session()).unwrap();

    for _ in 0..2 {
        let done = publish_journals(&mut store_on(&lock), length(), &finished).unwrap();
        assert_eq!(as_found(done.findings()), [(missing, Problem::Missing)]);
        assert!(done.segments().is_empty());
        assert!(done.quarantined().is_empty());
        assert_eq!(fs.read(&path).unwrap(), bytes);
    }

    // The row's file is back: the rest of the window publishes around it,
    // and only then is the journal set aside.
    let mut file = fs.create(&row_path).unwrap();
    file.write_all(&flac).unwrap();
    file.sync().unwrap();
    fs.sync_dir(&session()).unwrap();
    let done = publish_journals(&mut store_on(&lock), length(), &finished).unwrap();
    assert!(done.findings().is_empty());
    assert_eq!(
        done.segments()
            .iter()
            .map(|r| (r.range().start().get(), r.range().end().get()))
            .collect::<Vec<_>>(),
        [(0, 100), (200, 1_000)]
    );
    let aside = PathBuf::from("/session/journal-000000.unreadable");
    assert_eq!(done.quarantined(), std::slice::from_ref(&aside));
    assert_eq!(fs.read(&aside).unwrap(), bytes);
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

/// Plants a committed row on `fs` for `range` of `track`, with no file
/// under its name.
fn plant_missing_row(fs: &FakeFs, track: TrackId, range: SampleRange) -> SegmentRow {
    let row = plant_row(fs, track, range, b"gone", b"gone");
    fs.remove(&durable_path(track, range)).unwrap();
    fs.sync_dir(&session()).unwrap();
    row
}

#[test]
fn rows_that_claim_nothing_never_let_a_journal_go_at_any_crash() {
    let (fs, promised) = clean_run(Recording {
        steps: 7,
        publish: false,
        fail_at: None,
    });
    let range = |a, b| SampleRange::new(SampleIndex::new(a), SampleIndex::new(b)).unwrap();
    // Mic 0..2,750 (windows 0 and 1), system 700..3,450 (windows 0 to 2).
    assert_eq!(promised.durable[&MIC].get(), 2_750);
    assert_eq!(promised.durable[&SYSTEM].get(), 3_450);
    for outcome in [
        CrashOutcome::LoseUnsynced,
        CrashOutcome::KeepAll,
        CrashOutcome::Partial { seed: 5 },
    ] {
        let disk = fs.crash(outcome);
        // A wrong hash: the file under the row's name holds other audio
        // (another session's, or a store restored out of step). It ends
        // where the next window's segment starts.
        let wrong_hash = plant_row(
            &disk,
            MIC,
            range(0, 1_500),
            &flac_of(MIC, 0, 1_500),
            &flac::encode(rate(), &[&[0; 1_500]]).unwrap(),
        );
        // The same name with a different range: the file's hash is the
        // row's, but it holds 300 samples where the row claims 800. A check
        // of the hash alone would let the row claim 1,500..2,300 and the
        // journals holding 1,800..2,300 go. It starts where the window
        // before ends.
        let short = flac_of(SYSTEM, 1_500, 300);
        let wrong_range = plant_row(&disk, SYSTEM, range(1_500, 2_300), &short, &short);
        // No file at all, partway through a window: the store refuses a
        // segment over it, which used to stop every later segment.
        let missing = plant_missing_row(&disk, SYSTEM, range(3_100, 3_300));
        let bad = [wrong_hash, wrong_range, missing];
        let expected = [
            (wrong_hash, Problem::HashMismatch),
            (wrong_range, Problem::LengthMismatch),
            (missing, Problem::Missing),
        ];
        // Rows overlapping no journal of their own track are never read,
        // so their files, which don't match either, aren't reported: one
        // right after the mic's last sample, and one on the system track
        // before its first, over samples the mic's journals hold.
        let mic_end = promised.durable[&MIC].get();
        let unread = [
            plant_row(&disk, MIC, range(mic_end, mic_end + 10), b"a", b"b"),
            plant_row(&disk, SYSTEM, range(0, 700), b"c", b"d"),
        ];
        let planted: BTreeMap<PathBuf, Option<Vec<u8>>> = bad
            .iter()
            .chain(&unread)
            .map(|r| {
                let path = durable_path(r.track(), r.range());
                let bytes = disk.read(&path).ok();
                (path, bytes)
            })
            .collect();
        let ignored: Vec<SegmentRow> = bad.iter().chain(&unread).copied().collect();
        let as_found = |f: &[Finding]| -> Vec<(SegmentRow, Problem)> {
            f.iter().map(|f| (*f.row(), f.problem())).collect()
        };

        // Uninterrupted: the bad rows are reported, claim nothing, and keep
        // their journals and files; the rest is published on both tracks.
        let probe = disk.copy_disk();
        let done = salvage(&mut session_store(&probe), length()).unwrap();
        let ops = probe.attempted();
        assert_eq!(as_found(done.findings()), expected, "{outcome:?}");
        assert_eq!(done.findings_unsaved(), None);
        for track in [MIC, SYSTEM] {
            assert!(done.segments().iter().any(|r| r.track() == track));
        }
        assert!(!done.deleted().is_empty());
        let recorded = read_findings(&session_dir(&probe)).unwrap();
        assert_eq!(as_found(recorded.found()), expected);
        assert!(
            recorded
                .found()
                .iter()
                .all(|f| f.status() == Status::Unresolved)
        );
        assert_eq!(recorded.verification(), Verification::Done);
        let uninterrupted = observe(&probe);
        check_mismatch_kept(&promised, &bad, &ignored, &planted, &uninterrupted)
            .unwrap_or_else(|e| panic!("{outcome:?}: {e}"));
        // Any number of salvages after it change nothing.
        for _ in 0..2 {
            let again = salvage(&mut session_store(&probe), length()).unwrap();
            assert_eq!(as_found(again.findings()), expected);
            assert!(again.segments().is_empty() && again.deleted().is_empty());
            assert!(observe(&probe) == uninterrupted, "{outcome:?}");
        }

        // Crashed after every operation, under every outcome, then run
        // again: the same end state, findings file included.
        for after in 0..=ops {
            for crash in CrashOutcome::standard() {
                let run = disk.copy_disk();
                run.crash_after(after);
                let _ = salvage(&mut session_store(&run), length());
                let survived = run.crash(crash);
                let mut again = session_store(&survived);
                let rerun = salvage(&mut again, length())
                    .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
                assert_eq!(as_found(rerun.findings()), expected);
                let seen = observe(&survived);
                check_mismatch_kept(&promised, &bad, &ignored, &planted, &seen)
                    .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
                assert!(
                    seen == uninterrupted,
                    "salvage crashed after {after} ops, {crash:?}, ended differently"
                );
            }
        }
    }
}

/// After salvage with bad rows `bad` (missing or mismatched files), and
/// `ignored` rows (`bad` and those never read) whose files were `planted`
/// (`None`: no file): those files are as they were, every sample in a
/// window a bad row overlaps is still in a journal and in no row, and
/// every other promised sample is in a good row's file, with no journal
/// left holding it.
fn check_mismatch_kept(
    promised: &Promised,
    bad: &[SegmentRow],
    ignored: &[SegmentRow],
    planted: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    seen: &Observed,
) -> Result<(), String> {
    for (path, bytes) in planted {
        if seen.files.get(path) != bytes.as_ref() {
            return Err(format!("{} changed", path.display()));
        }
    }
    let good = Observed {
        files: seen.files.clone(),
        rows: Ok(seen
            .rows
            .clone()?
            .into_iter()
            .filter(|r| !ignored.contains(r))
            .collect()),
    };
    let in_rows = row_samples(&good)?;
    let in_journals = journal_samples(seen)?;
    // A segment is one window here (no gaps), so a window that shares a
    // sample with a bad row isn't published, and stays in its journals.
    let in_bad = |track: TrackId, s: u64| {
        let window = length().window_of(SampleIndex::new(s));
        let (from, to) = (
            window * length().samples(),
            (window + 1) * length().samples(),
        );
        bad.iter().any(|r| {
            r.track() == track && r.range().start().get() < to && from < r.range().end().get()
        })
    };
    for (&track, &start) in &promised.started {
        let end = promised.durable[&track].get();
        for s in start.get()..end {
            let ok = if in_bad(track, s) {
                in_journals.contains(&(track, s)) && !in_rows.contains(&(track, s))
            } else {
                in_rows.contains(&(track, s)) && !in_journals.contains(&(track, s))
            };
            if !ok {
                return Err(format!(
                    "track {} sample {s}: in a row {}, in a journal {}, under a mismatched row {}",
                    track.get(),
                    in_rows.contains(&(track, s)),
                    in_journals.contains(&(track, s)),
                    in_bad(track, s)
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn recording_with_the_store_down_loses_nothing_once_it_is_back() {
    // The store's directory doesn't exist yet: every publish fails.
    let fs = FakeFs::with_dirs([session()]);
    let (clock, dyn_clock) = fake_clock();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock).unwrap();
    let mut store = store_on(&lock);
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

#[test]
fn another_sessions_journals_are_refused_before_anything_is_done() {
    // Both sessions number their journals from 0: another session's
    // finished journal 0 names this session's journal 0, still recording.
    let fs = FakeFs::with_dirs([session(), db()]);
    let (_, clock) = fake_clock();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    writer.append(MIC, &samples(MIC, 0, 100)).unwrap();
    let theirs = || FinishedJournal::new(SessionId::new(2), JournalId::FIRST);
    let before = observe(&fs);
    let ops = fs.ops().len();
    let err = publish_journals(&mut store_on(&lock), length(), &[theirs()]).unwrap_err();
    assert!(
        matches!(&err, PublishError::OtherSession(j) if *j == theirs()),
        "{err}"
    );
    // Mixed with this session's own, still refused.
    let ours = finished(&[0]);
    let mixed = [finished(&[0]).remove(0), theirs()];
    let err = publish_journals(&mut store_on(&lock), length(), &mixed).unwrap_err();
    assert!(matches!(err, PublishError::OtherSession(_)), "{err}");
    assert_eq!(fs.ops().len(), ops);
    assert_eq!(observe(&fs), before);
    // The journal goes on recording, and is published once it's finished.
    writer.append(MIC, &samples(MIC, 100, 100)).unwrap();
    let finished = writer.finish().unwrap();
    assert_eq!(finished, ours);
    assert_eq!(finished[0].session(), SESSION);
    let done = publish_journals(&mut store_on(&lock), length(), &finished).unwrap();
    assert_eq!(done.deleted(), [JournalId::FIRST]);
    assert_eq!(done.segments().len(), 1);
    assert_eq!(done.segments()[0].range().len().get(), 200);
}

fn as_found(f: &[Finding]) -> Vec<(SegmentRow, Problem)> {
    f.iter().map(|f| (*f.row(), f.problem())).collect()
}

/// A recording with a row whose file is missing, on the system track's
/// third window, and the findings salvage makes of it.
fn recording_with_a_missing_row() -> (FakeFs, Vec<(SegmentRow, Problem)>) {
    let (fs, _) = clean_run(Recording {
        steps: 7,
        publish: false,
        fail_at: None,
    });
    let range = SampleRange::new(SampleIndex::new(3_100), SampleIndex::new(3_300)).unwrap();
    let missing = plant_missing_row(&fs, SYSTEM, range);
    (fs, vec![(missing, Problem::Missing)])
}

#[test]
fn findings_survive_any_later_failure_and_their_own_never_stops_publishing() {
    let (fs, expected) = recording_with_a_missing_row();
    let findings_path = session().join(FINDINGS_FILE_NAME);
    let probe = fs.copy_disk();
    salvage(&mut session_store(&probe), length()).unwrap();
    let ops = probe.attempted();
    let uninterrupted = observe(&probe);
    assert!(
        probe
            .ops()
            .iter()
            .any(|op| matches!(op, Op::Rename { to, .. } if *to == findings_path))
    );

    // Fail each operation in turn. Before some point the findings aren't
    // durable yet; from it on, they are, and every later failure is a
    // publish error that leaves them.
    let mut durable_from = None;
    let mut unsaved = 0;
    let mut row_reads = 0;
    let mut journal_reads = 0;
    let unreadable = [(expected[0].0, Problem::Unreadable(ReadFailure::Other))];
    for at in 0..ops {
        let run = fs.copy_disk();
        run.fail_after(at, io::ErrorKind::Other);
        let result = salvage(&mut session_store(&run), length());
        let on_disk = read_findings(&session_dir(&run)).unwrap();
        if let Ok(done) = &result
            && as_found(done.findings()) == unreadable
        {
            // The failed operation was the read of the row's file: that's
            // a finding too, recorded, and publishing carried on.
            assert_eq!(as_found(on_disk.found()), unreadable);
            assert!(!done.segments().is_empty());
            assert!(!done.deleted().is_empty());
            // The next run finds the file missing, and keeps both.
            let again = salvage(&mut session_store(&run), length()).unwrap();
            assert_eq!(as_found(again.findings()), expected);
            let on_disk = read_findings(&session_dir(&run)).unwrap();
            assert_eq!(
                as_found(on_disk.found()),
                [expected[0], unreadable[0]],
                "failing op {at}"
            );
            row_reads += 1;
            continue;
        }
        if let Ok(done) = &result
            && let [(_, kind)] = done.unread()
        {
            // The failed operation was a journal's first read: it's kept for
            // the next run, and publishing carried on. The row is checked
            // only if another journal holds its samples, so it's recorded
            // or not yet; the next run reads the journal and records it.
            assert_eq!(*kind, io::ErrorKind::Other);
            assert!(!done.segments().is_empty(), "failing op {at}");
            let found = as_found(on_disk.found());
            assert!(found.is_empty() || found == expected, "failing op {at}");
            salvage(&mut session_store(&run), length()).unwrap();
            assert!(observe(&run) == uninterrupted, "failing op {at}");
            journal_reads += 1;
            continue;
        }
        let present = as_found(on_disk.found()) == expected;
        match durable_from {
            None if present => durable_from = Some(at),
            None => assert!(on_disk.found().is_empty(), "failing op {at}"),
            Some(_) => {
                assert!(present, "failing op {at} lost the findings");
                assert!(result.is_err(), "failing op {at} went unnoticed");
            }
        }
        if let Ok(done) = &result
            && done.findings_unsaved().is_some()
        {
            // The findings write failed: publishing carried on regardless.
            assert_eq!(done.findings_unsaved(), Some(io::ErrorKind::Other));
            assert_eq!(as_found(done.findings()), expected);
            assert!(!done.segments().is_empty());
            assert!(!done.deleted().is_empty());
            unsaved += 1;
        }
        // Whatever failed, the next run ends where an uninterrupted one
        // does, and the findings are there.
        salvage(&mut session_store(&run), length()).unwrap();
        assert!(observe(&run) == uninterrupted, "failing op {at}");
    }
    let durable_from = durable_from.unwrap();
    // Publishing comes after the findings, so failures there were tried.
    assert!(ops - durable_from > 20, "{durable_from} of {ops}");
    assert!(unsaved >= 4, "{unsaved}");
    assert_eq!(row_reads, 1);
    let journals = fs.paths().into_iter().filter(|p| is_journal(p)).count();
    assert!(journals >= 4, "{journals}");
    assert_eq!(journal_reads, journals);
}

#[test]
fn an_unreadable_store_is_recorded_and_clears_no_finding() {
    let (fs, expected) = recording_with_a_missing_row();
    salvage(&mut session_store(&fs), length()).unwrap();
    let after_first = observe(&fs);

    // A row file the store can't read: nothing can be checked.
    let bad_row = db().join("t9-00000000000000000000.row");
    let mut file = fs.create(&bad_row).unwrap();
    file.write_all(b"torn").unwrap();
    let err = salvage(&mut session_store(&fs), length()).unwrap_err();
    assert!(matches!(err, PublishError::Store(_)), "{err}");
    let findings = read_findings(&session_dir(&fs)).unwrap();
    assert_eq!(findings.verification(), Verification::Unavailable);
    assert_eq!(as_found(findings.found()), expected);
    // Nothing else changed: no journal deleted, no segment published.
    let now = observe(&fs);
    for (path, bytes) in &after_first.files {
        if path.file_name() != Some(FINDINGS_FILE_NAME.as_ref()) {
            assert_eq!(now.files.get(path), Some(bytes), "{}", path.display());
        }
    }

    // The store readable again: checked, and the finding still stands.
    fs.remove(&bad_row).unwrap();
    let done = salvage(&mut session_store(&fs), length()).unwrap();
    assert_eq!(as_found(done.findings()), expected);
    let findings = read_findings(&session_dir(&fs)).unwrap();
    assert_eq!(findings.verification(), Verification::Done);
    assert_eq!(as_found(findings.found()), expected);
    assert_eq!(observe(&fs), after_first);
}

#[test]
fn recording_starts_and_rotates_with_the_store_down_and_the_findings_unwritable() {
    // No store directory, and a directory where the findings' temp file
    // goes, so the findings can't be written either.
    let fs = FakeFs::with_dirs([session()]);
    fs.create_dir(&session().join("salvage-findings.tmp"))
        .unwrap();
    let err = salvage(&mut session_store(&fs), length()).unwrap_err();
    assert!(matches!(err, PublishError::Store(_)), "{err}");

    let (clock, dyn_clock) = fake_clock();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    let mut store = store_on(&lock);
    let mut pending = Vec::new();
    for _ in 0..5 {
        let from = writer.next_sample(MIC).unwrap().get();
        writer.append(MIC, &samples(MIC, from, 1_000)).unwrap();
        clock.advance(SampleCount::new(1_000).duration_at(rate()).unwrap());
        writer.sync_if_due().unwrap();
        pending.extend(writer.take_finished());
        let err = publish_journals(&mut store, length(), &pending).unwrap_err();
        assert!(matches!(err, PublishError::Store(_)), "{err}");
    }
    pending.extend(writer.finish().unwrap());
    // 5,000 samples in windows of 1,500: four journals.
    assert_eq!(pending.len(), 4);
    assert_eq!(fs.paths().into_iter().filter(|p| is_journal(p)).count(), 4);
    assert_eq!(
        read_findings(&session_dir(&fs)).unwrap(),
        Findings::default()
    );

    // The store back, with a row whose file is missing in the first
    // window: the rest publishes, and the findings failure is reported,
    // not fatal.
    fs.create_dir(&db()).unwrap();
    let range = SampleRange::new(SampleIndex::new(100), SampleIndex::new(200)).unwrap();
    let missing = plant_missing_row(&fs, MIC, range);
    let done = publish_journals(&mut store, length(), &pending).unwrap();
    assert_eq!(as_found(done.findings()), [(missing, Problem::Missing)]);
    assert_eq!(done.findings_unsaved(), Some(io::ErrorKind::IsADirectory));
    assert_eq!(done.deleted().len(), 3);
    assert_eq!(
        done.segments()
            .iter()
            .map(|r| r.range().len().get())
            .sum::<u64>(),
        3_500
    );
}

#[test]
fn nothing_in_a_bad_rows_window_is_published_even_in_another_segment() {
    // One window, two epochs: two segments, only the first under the row.
    let fs = FakeFs::with_dirs([session(), db()]);
    let (_, clock) = fake_clock();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    writer.append(MIC, &samples(MIC, 0, 500)).unwrap();
    writer.new_epoch(MIC, EpochId::new(1)).unwrap();
    writer.append(MIC, &samples(MIC, 500, 500)).unwrap();
    // The next window, which publishes.
    writer.append(MIC, &samples(MIC, 1_000, 1_000)).unwrap();
    let finished = writer.finish().unwrap();
    assert_eq!(finished.len(), 3);
    let range = SampleRange::new(SampleIndex::new(100), SampleIndex::new(200)).unwrap();
    let missing = plant_missing_row(&fs, MIC, range);

    let done = publish_journals(&mut store_on(&lock), length(), &finished).unwrap();
    assert_eq!(as_found(done.findings()), [(missing, Problem::Missing)]);
    assert_eq!(
        done.segments()
            .iter()
            .map(|r| (r.range().start().get(), r.range().end().get()))
            .collect::<Vec<_>>(),
        [(1_500, 2_000)]
    );
    // Both journals of the first window are kept (the second runs on to
    // the window's end); the next window's is published and deleted.
    assert_eq!(done.deleted(), [JournalId::new(2)]);
    assert_eq!(fs.paths().into_iter().filter(|p| is_journal(p)).count(), 2);
}

#[test]
fn salvage_removes_a_findings_temp_a_crash_left() {
    let (fs, expected) = recording_with_a_missing_row();
    salvage(&mut session_store(&fs), length()).unwrap();
    let clean = observe(&fs);
    // A crash during a findings write that, rerun, has nothing to change.
    let temp = session().join("salvage-findings.tmp");
    let mut file = fs.create(&temp).unwrap();
    file.write_all(b"half").unwrap();
    let done = salvage(&mut session_store(&fs), length()).unwrap();
    assert_eq!(as_found(done.findings()), expected);
    assert!(!fs.paths().contains(&temp));
    assert_eq!(observe(&fs), clean);
}

/// A recording with a committed row over the mic's first window whose file
/// is there and matches, and that file's path and bytes.
fn recording_with_a_good_row() -> (FakeFs, Promised, SegmentRow, PathBuf, Vec<u8>) {
    let (fs, promised) = clean_run(Recording {
        steps: 7,
        publish: false,
        fail_at: None,
    });
    let range = SampleRange::new(SampleIndex::ZERO, SampleIndex::new(1_500)).unwrap();
    let flac = flac_of(MIC, 0, 1_500);
    let row = plant_row(&fs, MIC, range, &flac, &flac);
    (fs, promised, row, durable_path(MIC, range), flac)
}

#[test]
fn an_unreadable_segment_file_claims_nothing_until_it_reads_and_matches() {
    let (fs, promised, row, path, flac) = recording_with_a_good_row();
    // Where salvage reads the row's file: every operation before it
    // succeeds, so its place in the log is its place in the count.
    let probe = fs.copy_disk();
    salvage(&mut session_store(&probe), length()).unwrap();
    let at = probe
        .ops()
        .iter()
        .position(|op| *op == Op::Read(path.clone()))
        .unwrap();
    let planted = BTreeMap::from([(path.clone(), Some(flac))]);
    for kind in [
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::IsADirectory,
        io::ErrorKind::Other,
    ] {
        let run = fs.copy_disk();
        run.fail_after(at, kind);
        // The read fails: the row claims nothing and is recorded, and
        // every segment outside its window is published.
        let done = salvage(&mut session_store(&run), length()).unwrap();
        let expected = [(row, Problem::Unreadable(ReadFailure::of(kind)))];
        assert_eq!(as_found(done.findings()), expected, "{kind:?}");
        assert_eq!(done.findings_unsaved(), None);
        for track in [MIC, SYSTEM] {
            assert!(done.segments().iter().any(|r| r.track() == track));
        }
        check_mismatch_kept(&promised, &[row], &[row], &planted, &observe(&run))
            .unwrap_or_else(|e| panic!("{kind:?}: {e}"));

        // The finding survives a restart.
        let restarted = run.crash(CrashOutcome::LoseUnsynced);
        let recorded = read_findings(&session_dir(&restarted)).unwrap();
        assert_eq!(as_found(recorded.found()), expected);

        // Read now, and matching: the row claims its samples again, so the
        // journals holding them go and nothing is published over it. The
        // finding stays recorded; nothing new is found.
        let again = salvage(&mut session_store(&restarted), length()).unwrap();
        assert!(again.findings().is_empty(), "{kind:?}");
        assert!(again.segments().is_empty());
        assert!(!again.deleted().is_empty());
        let seen = observe(&restarted);
        check_mismatch_kept(&promised, &[], &[], &planted, &seen)
            .unwrap_or_else(|e| panic!("{kind:?}, read again: {e}"));
        assert!(seen.rows.clone().unwrap().contains(&row));
        let recorded = read_findings(&session_dir(&restarted)).unwrap();
        assert_eq!(as_found(recorded.found()), expected);
        // And it stays that way.
        assert_eq!(
            salvage(&mut session_store(&restarted), length()).unwrap(),
            Published::default()
        );
        assert!(
            observe(&restarted) == seen,
            "{kind:?}: a third salvage changed the disk"
        );
    }
}

#[test]
fn a_directory_under_a_rows_name_never_lets_its_journals_go_at_any_crash() {
    // A file that can never be read: a directory where the row's file
    // should be.
    let (disk, promised, row, path, _) = recording_with_a_good_row();
    disk.remove(&path).unwrap();
    disk.create_dir(&path).unwrap();
    disk.sync_dir(&session()).unwrap();
    let expected = [(row, Problem::Unreadable(ReadFailure::IsADirectory))];
    let planted = BTreeMap::from([(path.clone(), None)]);
    let is_dir = |fs: &FakeFs| {
        fs.read(&path)
            .is_err_and(|e| e.kind() == io::ErrorKind::IsADirectory)
    };

    let probe = disk.copy_disk();
    let done = salvage(&mut session_store(&probe), length()).unwrap();
    let ops = probe.attempted();
    assert_eq!(as_found(done.findings()), expected);
    for track in [MIC, SYSTEM] {
        assert!(done.segments().iter().any(|r| r.track() == track));
    }
    let uninterrupted = observe(&probe);
    check_mismatch_kept(&promised, &[row], &[row], &planted, &uninterrupted).unwrap();
    assert!(is_dir(&probe));
    let again = salvage(&mut session_store(&probe), length()).unwrap();
    assert_eq!(as_found(again.findings()), expected);
    assert!(again.segments().is_empty() && again.deleted().is_empty());
    assert!(
        observe(&probe) == uninterrupted,
        "a second salvage changed the disk"
    );

    for after in 0..=ops {
        for crash in CrashOutcome::standard() {
            let run = disk.copy_disk();
            run.crash_after(after);
            let _ = salvage(&mut session_store(&run), length());
            let survived = run.crash(crash);
            let rerun = salvage(&mut session_store(&survived), length())
                .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
            assert_eq!(as_found(rerun.findings()), expected);
            let seen = observe(&survived);
            check_mismatch_kept(&promised, &[row], &[row], &planted, &seen)
                .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
            assert!(is_dir(&survived));
            assert!(
                seen == uninterrupted,
                "salvage crashed after {after} ops, {crash:?}, ended differently"
            );
        }
    }
}

#[test]
fn a_transient_read_error_then_a_crash_anywhere_loses_nothing() {
    let (fs, promised, row, path, flac) = recording_with_a_good_row();
    let probe = fs.copy_disk();
    salvage(&mut session_store(&probe), length()).unwrap();
    let at = probe
        .ops()
        .iter()
        .position(|op| *op == Op::Read(path.clone()))
        .unwrap();
    let planted = BTreeMap::from([(path, Some(flac))]);
    let unreadable = [(row, Problem::Unreadable(ReadFailure::Other))];
    let findings_path = session().join(FINDINGS_FILE_NAME);
    let without_findings = |seen: &Observed| {
        let mut seen = seen.clone();
        seen.files.remove(&findings_path);
        seen
    };

    // Uninterrupted: the read fails once, then a restart reads it.
    let failed = fs.copy_disk();
    failed.fail_after(at, io::ErrorKind::Other);
    salvage(&mut session_store(&failed), length()).unwrap();
    let ops = failed.attempted();
    let restarted = failed.crash(CrashOutcome::LoseUnsynced);
    salvage(&mut session_store(&restarted), length()).unwrap();
    let settled = without_findings(&observe(&restarted));

    // The same, crashed after every operation of the failing run: the run
    // after the restart reads the file, and ends where the uninterrupted
    // one does, with the finding recorded or not yet, never anything else.
    let mut kept = 0;
    for after in 0..=ops {
        for crash in CrashOutcome::standard() {
            let run = fs.copy_disk();
            run.fail_after(at, io::ErrorKind::Other);
            run.crash_after(after);
            let _ = salvage(&mut session_store(&run), length());
            let survived = run.crash(crash);
            let rerun = salvage(&mut session_store(&survived), length())
                .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
            assert!(rerun.findings().is_empty(), "after {after} ops, {crash:?}");
            let seen = observe(&survived);
            check_mismatch_kept(&promised, &[], &[], &planted, &seen)
                .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
            let recorded = as_found(read_findings(&session_dir(&survived)).unwrap().found());
            assert!(
                recorded.is_empty() || recorded == unreadable,
                "after {after} ops, {crash:?}: {recorded:?}"
            );
            kept += usize::from(!recorded.is_empty());
            assert!(
                without_findings(&seen) == settled,
                "after {after} ops, {crash:?}, ended differently"
            );
        }
    }
    // Crashes landed both before the finding was durable and after.
    let runs = (ops + 1) * CrashOutcome::standard().len();
    assert!(kept > 0 && kept < runs, "{kept} of {runs}");
}

/// Where salvage of `disk` first reads each journal, in order: its pass
/// over the journals. Every operation before it succeeds, so its place in
/// the log is its place in the count.
fn first_journal_reads(disk: &FakeFs) -> Vec<(usize, PathBuf)> {
    let probe = disk.copy_disk();
    salvage(&mut session_store(&probe), length()).unwrap();
    let mut seen = BTreeSet::new();
    probe
        .ops()
        .into_iter()
        .enumerate()
        .filter_map(|(at, op)| match op {
            Op::Read(path) if is_journal(&path) && seen.insert(path.clone()) => Some((at, path)),
            _ => None,
        })
        .collect()
}

/// The journals on `disk` that overlap a newer one of their track, each
/// with that newer one: (older, newer).
fn overlapping_pairs(disk: &FakeFs) -> Vec<(JournalId, JournalId)> {
    let mut ranges: Vec<(JournalId, TrackId, SampleRange)> = Vec::new();
    for path in disk.paths().into_iter().filter(|p| is_journal(p)) {
        let read = read_journal(&disk.read(&path).unwrap());
        if let (Some(h), Some(r)) = (read.header(), read.range()) {
            ranges.push((h.id(), h.track(), r));
        }
    }
    let mut pairs = Vec::new();
    for a in &ranges {
        for b in &ranges {
            if a.0 < b.0 && a.1 == b.1 && a.2.start() < b.2.end() && b.2.start() < a.2.end() {
                pairs.push((a.0, b.0));
            }
        }
    }
    pairs
}

fn journal_id(path: &Path) -> JournalId {
    path.file_name()
        .and_then(JournalId::from_file_name)
        .unwrap()
}

#[test]
fn a_journal_that_cant_be_read_is_kept_and_everything_else_published() {
    let plain = Recording {
        steps: 4,
        publish: false,
        fail_at: None,
    };
    // Overlapping journals, so the one that can't be read is sometimes the
    // newer of a pair: the older one's copy of the overlap is published
    // meanwhile, and when the newer one reads, only the rest of it is.
    let broken = Recording {
        fail_at: Some(a_write_after_unsynced_frames(plain)),
        ..plain
    };
    let mut tried = 0;
    for (how, overlapping) in [(plain, false), (broken, true)] {
        let (fs, promised) = clean_run(how);
        let disk = fs.crash(CrashOutcome::KeepAll);
        assert_eq!(!overlapping_pairs(&disk).is_empty(), overlapping);
        let reads = first_journal_reads(&disk);
        assert!(reads.len() >= 4, "{reads:?}");
        for (at, path) in reads {
            let id = journal_id(&path);
            let bytes = disk.read(&path).unwrap();
            let only_it = Observed {
                files: BTreeMap::from([(path.clone(), bytes.clone())]),
                rows: Ok(Vec::new()),
            };
            let in_it = journal_samples(&only_it).unwrap();
            for kind in [
                io::ErrorKind::PermissionDenied,
                io::ErrorKind::IsADirectory,
                io::ErrorKind::Other,
            ] {
                let run = disk.copy_disk();
                run.fail_after(at, kind);
                let done = salvage(&mut session_store(&run), length())
                    .unwrap_or_else(|e| panic!("{}, {kind:?}: {e}", path.display()));
                assert_eq!(
                    done.unread(),
                    [(FinishedJournal::new(SESSION, id), kind)],
                    "{}",
                    path.display()
                );
                assert!(!done.deleted().contains(&id));
                assert!(done.quarantined().is_empty());
                // Kept as it was, and the only journal left: every other
                // durable sample is in a row.
                let seen = observe(&run);
                let left: Vec<_> = seen.files.keys().filter(|p| is_journal(p)).collect();
                assert_eq!(left, [&path]);
                assert_eq!(seen.files[&path], bytes);
                let in_rows = row_samples(&seen).unwrap();
                check_durable(&promised, &in_rows.union(&in_it))
                    .unwrap_or_else(|e| panic!("{}, {kind:?}: {e}", path.display()));

                // Read now: its samples are published and it goes.
                let restarted = run.crash(CrashOutcome::LoseUnsynced);
                let again = salvage(&mut session_store(&restarted), length()).unwrap();
                assert!(again.unread().is_empty());
                assert!(again.deleted().contains(&id));
                check_after(&promised, &observe(&restarted))
                    .unwrap_or_else(|e| panic!("{}, {kind:?}, read: {e}", path.display()));
                tried += 1;
            }
        }
    }
    assert!(tried >= 24, "{tried}");
}

#[test]
fn a_directory_under_a_journals_name_never_stops_publishing_at_any_crash() {
    let (fs, promised) = clean_run(Recording {
        steps: 4,
        publish: false,
        fail_at: None,
    });
    let disk = fs.crash(CrashOutcome::KeepAll);
    let dir = session().join(JournalId::new(90).file_name());
    disk.create_dir(&dir).unwrap();
    disk.sync_dir(&session()).unwrap();
    let is_dir = |fs: &FakeFs| {
        fs.read(&dir)
            .is_err_and(|e| e.kind() == io::ErrorKind::IsADirectory)
    };
    let unread = [(
        FinishedJournal::new(SESSION, JournalId::new(90)),
        io::ErrorKind::IsADirectory,
    )];

    let probe = disk.copy_disk();
    let done = salvage(&mut session_store(&probe), length()).unwrap();
    let ops = probe.attempted();
    assert_eq!(done.unread(), unread);
    assert!(done.quarantined().is_empty());
    for track in [MIC, SYSTEM] {
        assert!(done.segments().iter().any(|r| r.track() == track));
    }
    assert!(is_dir(&probe));
    // It's still there, so every run reports it, and changes nothing else.
    let settled = observe(&probe);
    let again = salvage(&mut session_store(&probe), length()).unwrap();
    assert_eq!(again.unread(), unread);
    assert!(again.segments().is_empty() && again.deleted().is_empty());
    assert!(
        observe(&probe) == settled,
        "a second salvage changed the disk"
    );

    // Crashed after every operation, under every outcome: the next run
    // ends where the uninterrupted one did, with every durable sample in a
    // row and the directory as it was.
    check_after(&promised, &settled).unwrap();
    assert!(ops > 30, "{ops}");
    for after in 0..=ops {
        for crash in CrashOutcome::standard() {
            let run = disk.copy_disk();
            run.crash_after(after);
            let _ = salvage(&mut session_store(&run), length());
            let survived = run.crash(crash);
            let rerun = salvage(&mut session_store(&survived), length())
                .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
            assert_eq!(rerun.unread(), unread, "after {after} ops, {crash:?}");
            assert!(is_dir(&survived), "after {after} ops, {crash:?}");
            assert!(
                observe(&survived) == settled,
                "salvage crashed after {after} ops, {crash:?}, ended differently"
            );
        }
    }
}

#[test]
fn a_journal_read_failing_once_then_a_crash_anywhere_loses_nothing() {
    let (fs, promised) = clean_run(Recording {
        steps: 4,
        publish: false,
        fail_at: None,
    });
    let disk = fs.crash(CrashOutcome::KeepAll);
    // The uninterrupted end state, with every read working.
    let settled = salvage_fake(&disk.copy_disk()).unwrap();
    check_after(&promised, &settled).unwrap();
    // No journal overlaps another, so none of this one's samples is in a
    // row until it reads.
    assert!(overlapping_pairs(&disk).is_empty());
    // A journal read after another, and before the rest.
    let (at, path) = first_journal_reads(&disk)[1].clone();
    let bytes = disk.read(&path).unwrap();

    let failed = disk.copy_disk();
    failed.fail_after(at, io::ErrorKind::Other);
    let done = salvage(&mut session_store(&failed), length()).unwrap();
    assert_eq!(done.unread().len(), 1);
    assert!(!done.segments().is_empty());
    let ops = failed.attempted();

    // Crashed after every operation of the failing run: the journal is as
    // it was (none of its samples is in a row yet), and the run after the
    // restart, reading it, ends as the uninterrupted one did.
    for after in 0..=ops {
        for crash in CrashOutcome::standard() {
            let run = disk.copy_disk();
            run.fail_after(at, io::ErrorKind::Other);
            run.crash_after(after);
            let _ = salvage(&mut session_store(&run), length());
            let survived = run.crash(crash);
            assert_eq!(
                survived.read(&path).unwrap(),
                bytes,
                "after {after} ops, {crash:?}"
            );
            let rerun = salvage_fake(&survived)
                .unwrap_or_else(|e| panic!("after {after} ops, {crash:?}: {e}"));
            assert!(
                rerun == settled,
                "after {after} ops, {crash:?}, ended differently"
            );
        }
    }
}

#[test]
fn a_journal_that_is_gone_is_skipped_not_reported() {
    let (fs, promised) = clean_run(Recording {
        steps: 4,
        publish: false,
        fail_at: None,
    });
    let disk = fs.crash(CrashOutcome::KeepAll);
    let mut store = session_store(&disk);
    // Published already, by an earlier run that deleted it.
    let done = publish_journals(&mut store, length(), &finished(&[99])).unwrap();
    assert_eq!(done, Published::default());
    let done = salvage(&mut store, length()).unwrap();
    assert!(done.unread().is_empty());
    check_after(&promised, &observe(&disk)).unwrap();
}

#[test]
fn an_unread_journal_of_an_overlapping_pair_then_a_crash_anywhere_loses_nothing() {
    let plain = Recording {
        steps: 4,
        publish: false,
        fail_at: None,
    };
    let how = Recording {
        fail_at: Some(a_write_after_unsynced_frames(plain)),
        ..plain
    };
    let (fs, promised) = clean_run(how);
    let disk = fs.crash(CrashOutcome::KeepAll);
    let pairs = overlapping_pairs(&disk);
    let &(older, newer) = pairs.first().unwrap();
    let reads = first_journal_reads(&disk);

    // Either of the pair fails its read once; the other is published, its
    // window split from the unread one's samples, which the run after the
    // restart publishes. Crashed after every operation of the failing run,
    // under every outcome: that run ends with every durable sample in
    // exactly one row, no journal left, and a second run changes nothing.
    for id in [older, newer] {
        let (at, _) = reads.iter().find(|(_, p)| journal_id(p) == id).unwrap();
        let failed = disk.copy_disk();
        failed.fail_after(*at, io::ErrorKind::Other);
        let done = salvage(&mut session_store(&failed), length()).unwrap();
        assert_eq!(
            done.unread(),
            [(FinishedJournal::new(SESSION, id), io::ErrorKind::Other)]
        );
        assert!(!done.segments().is_empty());
        let ops = failed.attempted();
        for after in 0..=ops {
            for crash in CrashOutcome::standard() {
                let run = disk.copy_disk();
                run.fail_after(*at, io::ErrorKind::Other);
                run.crash_after(after);
                let _ = salvage(&mut session_store(&run), length());
                let survived = run.crash(crash);
                let rerun = salvage_fake(&survived).unwrap_or_else(|e| {
                    panic!("journal {}, after {after} ops, {crash:?}: {e}", id.get())
                });
                check_after(&promised, &rerun).unwrap_or_else(|e| {
                    panic!("journal {}, after {after} ops, {crash:?}: {e}", id.get())
                });
                assert!(
                    salvage_fake(&survived) == Ok(rerun),
                    "journal {}, after {after} ops, {crash:?}: a second salvage changed something",
                    id.get()
                );
            }
        }
    }
}

// One owner per session: the lock, journal ids that never repeat, and a
// resumed track's first sample and epoch.

#[test]
fn salvage_while_a_writer_records_is_refused_and_loses_nothing() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let (clock, dyn_clock) = fake_clock();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    writer.append(MIC, &samples(MIC, 0, 1_000)).unwrap();
    clock.advance(std::time::Duration::from_secs(1));
    writer.sync_if_due().unwrap();
    let live = session().join(JournalId::FIRST.file_name());
    let before = observe(&fs);

    // The owner itself, through a store bound under the writer's lock.
    let err = salvage(&mut store_on(&lock), length()).unwrap_err();
    assert!(matches!(err, PublishError::InUse(Use::Recording)), "{err}");
    // Another owner, as a second process would be.
    assert_eq!(
        session_dir(&fs).lock().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    // And a second writer for the same owner.
    let (_, other_clock) = fake_clock();
    assert!(matches!(
        SessionWriter::open(&lock, rate(), length(), other_clock),
        Err(SessionError::InUse(Use::Recording))
    ));
    assert_eq!(observe(&fs), before);
    assert!(fs.read(&live).is_ok());

    // Recording goes on into the same journal, and once it's finished
    // salvage may run: every sample is there.
    writer.append(MIC, &samples(MIC, 1_000, 500)).unwrap();
    let finished = writer.finish().unwrap();
    let mut store = store_on(&lock);
    publish_journals(&mut store, length(), &finished).unwrap();
    salvage(&mut store, length()).unwrap();
    drop(store);
    drop(lock);
    let held = row_samples(&observe(&fs)).unwrap();
    assert_eq!(held.first_missing(MIC, 0, 1_500), None);
    // The lock went with its last holder.
    let _again = owned(&fs);
}

#[test]
fn a_writer_cant_open_while_its_owner_salvages() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let lock = owned(&fs);
    let salvaging = lock.begin(Use::Salvaging).unwrap();
    let (_, clock) = fake_clock();
    assert!(matches!(
        SessionWriter::open(&lock, rate(), length(), clock),
        Err(SessionError::InUse(Use::Salvaging))
    ));
    drop(salvaging);
    let (_, clock) = fake_clock();
    assert!(SessionWriter::open(&lock, rate(), length(), clock).is_ok());
}

/// The first recording of the session that [`resume`] continues: 1,200
/// samples on the mic in epoch 0, finished and published. Returns its
/// finished journals, kept as a caller would for a retry.
fn first_recording(fs: &FakeFs) -> Vec<FinishedJournal> {
    let (_, clock) = fake_clock();
    let lock = owned(fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    writer.append(MIC, &samples(MIC, 0, 1_200)).unwrap();
    let finished = writer.finish().unwrap();
    let done = publish_journals(&mut store_on(&lock), length(), &finished).unwrap();
    assert_eq!(done.deleted(), [JournalId::FIRST]);
    finished
}

/// Resumes the session on the mic where it may, in a new epoch, and
/// records 2,400 samples with live publishing, 200 at a time so the first
/// new journal (1,200 to 1,500) is still being written at the first step.
/// At every step the first recording's `stale` finished journals are
/// published again, as a retry would. Notes how far the mic is durable in
/// `durable`.
fn resume(fs: &FakeFs, stale: &[FinishedJournal], durable: &mut u64) -> Result<(), Box<dyn Error>> {
    let (clock, dyn_clock) = fake_clock();
    let lock = session_dir(fs).lock()?;
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock)?;
    let from = writer.first_free_sample(MIC);
    let epoch = writer
        .highest_epoch(MIC)
        .map_or(EpochId::new(0), |e| EpochId::new(e.get() + 1));
    writer.start_track(MIC, epoch, from)?;
    let mut store = store_on(&lock);
    for _ in 0..12 {
        let at = writer.next_sample(MIC).ok_or("no track")?.get();
        writer.append(MIC, &samples(MIC, at, 200))?;
        clock.advance(SampleCount::new(200).duration_at(rate()).ok_or("time")?);
        writer.sync_if_due()?;
        if let Some(d) = writer.durable(MIC) {
            *durable = (*durable).max(d.end().get());
        }
        publish_journals(&mut store, length(), stale)?;
        let done = writer.take_finished();
        publish_journals(&mut store, length(), &done)?;
    }
    let end = writer.next_sample(MIC).ok_or("no track")?.get();
    let finished = writer.finish()?;
    *durable = end;
    publish_journals(&mut store, length(), &finished)?;
    publish_journals(&mut store, length(), stale)?;
    Ok(())
}

/// The ids of the journals created on `fs` (its log).
fn created_journals(fs: &FakeFs) -> BTreeSet<JournalId> {
    fs.ops()
        .iter()
        .filter_map(|op| match op {
            Op::Create(p) => p.file_name().and_then(JournalId::from_file_name),
            _ => None,
        })
        .collect()
}

/// After salvage: every row holds the audio recorded at its samples, the
/// first recording is all there, and so is the resumed one up to
/// `durable`. Then a new writer's first journal takes an id above all of
/// `used`.
fn check_resumed(disk: &FakeFs, durable: u64, used: &BTreeSet<JournalId>) -> Result<(), String> {
    salvage(&mut session_store(disk), length()).map_err(|e| e.to_string())?;
    let seen = observe(disk);
    let held = row_samples(&seen)?;
    if let Some(s) = held.first_missing(MIC, 0, 1_200) {
        return Err(format!("the first recording lost sample {s}"));
    }
    if let Some(s) = held.first_missing(MIC, 1_200, durable) {
        return Err(format!("the resumed recording lost durable sample {s}"));
    }
    if let Some(left) = seen.files.keys().find(|p| is_journal(p)) {
        return Err(format!("salvage left {}", left.display()));
    }
    let (_, clock) = fake_clock();
    let lock = owned(disk);
    let mut writer =
        SessionWriter::open(&lock, rate(), length(), clock).map_err(|e| e.to_string())?;
    let from = writer.first_free_sample(MIC);
    let epoch = writer
        .highest_epoch(MIC)
        .map_or(EpochId::new(0), |e| EpochId::new(e.get() + 1));
    writer
        .start_track(MIC, epoch, from)
        .map_err(|e| e.to_string())?;
    writer.append(MIC, &[1]).map_err(|e| e.to_string())?;
    let next = writer.durable(MIC).ok_or("no journal")?.journal();
    match used.last() {
        Some(&highest) if next <= highest => Err(format!(
            "a new journal took id {}, but {} was used",
            next.get(),
            highest.get()
        )),
        _ => Ok(()),
    }
}

#[test]
fn a_stale_finished_journal_never_touches_a_resumed_sessions_journal() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let stale = first_recording(&fs);
    let mut durable = 0;
    resume(&fs, &stale, &mut durable).unwrap();
    // Every sample was published: the retried stale journal 0 named
    // nothing, because the resumed recording's journals took new ids.
    let held = row_samples(&observe(&fs)).unwrap();
    assert_eq!(held.first_missing(MIC, 0, 3_600), None);
    let created: Vec<JournalId> = fs
        .ops()
        .iter()
        .filter_map(|op| match op {
            Op::Create(p) => p.file_name().and_then(JournalId::from_file_name),
            _ => None,
        })
        .collect();
    assert_eq!(
        created.iter().filter(|&&id| id == JournalId::FIRST).count(),
        1
    );
    assert!(created.len() > 2, "{created:?}");
    // The resumed rows are in a new epoch.
    let epochs: BTreeSet<u32> = FakeStore::new(&fs, &db())
        .rows()
        .unwrap()
        .iter()
        .map(|r| r.epoch().get())
        .collect();
    assert_eq!(epochs, BTreeSet::from([0, 1]));
}

#[test]
fn a_resumed_session_crashed_anywhere_loses_nothing_and_never_reuses_an_id() {
    let base = FakeFs::with_dirs([session(), db()]);
    let stale = first_recording(&base);
    let base = base.copy_disk();
    let total = {
        let fs = base.copy_disk();
        resume(&fs, &stale, &mut 0).unwrap();
        fs.attempted()
    };
    let mut cases = 0;
    for k in 0..total {
        for outcome in CrashOutcome::standard() {
            let fs = base.copy_disk();
            fs.crash_after(k);
            let mut durable = 1_200;
            let _ = resume(&fs, &stale, &mut durable);
            let mut used = created_journals(&fs);
            used.insert(JournalId::FIRST);
            let survived = fs.crash(outcome);
            check_resumed(&survived, durable, &used)
                .unwrap_or_else(|e| panic!("crash after {k} ops, {outcome:?}: {e}"));
            cases += 1;
        }
    }
    assert!(cases > 100, "{cases}");
}

#[test]
fn a_resumed_track_cant_start_inside_what_it_holds_or_reuse_an_epoch() {
    let fs = FakeFs::with_dirs([session(), db()]);
    first_recording(&fs);
    let (_, clock) = fake_clock();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    // The published rows end at 1,200, in epoch 0.
    assert_eq!(writer.first_free_sample(MIC), SampleIndex::new(1_200));
    assert_eq!(writer.highest_epoch(MIC), Some(EpochId::new(0)));
    assert!(matches!(
        writer.start_track(MIC, EpochId::new(1), SampleIndex::new(600)),
        Err(SessionError::Covered { track: MIC, first_free }) if first_free == SampleIndex::new(1_200)
    ));
    assert!(matches!(
        writer.start_track(MIC, EpochId::new(0), SampleIndex::new(1_200)),
        Err(SessionError::EpochUsed { track: MIC, .. })
    ));
    // Another track is untouched by the mic's history.
    assert_eq!(writer.first_free_sample(SYSTEM), SampleIndex::ZERO);
    assert_eq!(writer.highest_epoch(SYSTEM), None);
    writer
        .start_track(MIC, EpochId::new(1), SampleIndex::new(1_200))
        .unwrap();
    // Within a recording, epochs only go up.
    assert!(matches!(
        writer.new_epoch(MIC, EpochId::new(1)),
        Err(SessionError::EpochUsed { track: MIC, .. })
    ));
    writer.new_epoch(MIC, EpochId::new(2)).unwrap();
}

#[test]
fn a_track_cant_start_inside_journals_left_unsalvaged() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let (clock, dyn_clock) = fake_clock();
    let lock = owned(&fs);
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock).unwrap();
    writer
        .start_track(SYSTEM, EpochId::new(3), SampleIndex::new(100))
        .unwrap();
    writer.append(SYSTEM, &samples(SYSTEM, 100, 2_000)).unwrap();
    clock.advance(std::time::Duration::from_secs(3));
    writer.sync_if_due().unwrap();
    // Stopped as a crash would, with nothing published.
    drop(writer);
    let (_, clock) = fake_clock();
    let writer = SessionWriter::open(&lock, rate(), length(), clock).unwrap();
    assert_eq!(writer.first_free_sample(SYSTEM), SampleIndex::new(2_100));
    assert_eq!(writer.highest_epoch(SYSTEM), Some(EpochId::new(3)));
}

#[test]
fn a_damaged_marks_file_stops_a_writer_opening() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let mut file = fs.create(&session().join(MARKS_FILE_NAME)).unwrap();
    file.write_all(b"not marks\n").unwrap();
    let (_, clock) = fake_clock();
    let err = SessionWriter::open(&owned(&fs), rate(), length(), clock).unwrap_err();
    assert!(
        matches!(&err, SessionError::Io(e) if e.kind() == io::ErrorKind::InvalidData),
        "{err}"
    );
}
