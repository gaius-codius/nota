//! The journal's acceptance tests: crash after every operation, the lag
//! bound in a timed run, torn final frames, and the v2 header with its
//! journal id.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRange, SampleRate, SessionTime,
    TrackId,
};

use super::format::{FRAME_HEADER_LEN, HEADER_LEN, MAX_FRAME_SAMPLES, encode_frame, encode_header};
use super::*;
use crate::fs::crash::{CrashCase, CrashTest};
use crate::fs::fake::{CrashOutcome, FakeFs, Op};
use crate::fs::{Fs, FsFile, StdFs};
use crate::test_dir::TestDir;

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);
const EPOCH: EpochId = EpochId::new(0);

fn session() -> PathBuf {
    PathBuf::from("/session")
}

/// The header of journal `id`, holding `track`'s audio from [`EPOCH`] at
/// speech rate.
fn header(id: u64, track: TrackId) -> JournalHeader {
    JournalHeader::new(JournalId::new(id), track, EPOCH, SampleRate::SPEECH)
}

/// Where journal `id` lives in `/session`.
fn journal_path(id: u64) -> PathBuf {
    session().join(JournalId::new(id).file_name())
}

/// The sample a test track holds at `index`: distinct per track and
/// position, so misplaced or duplicated audio shows.
fn sample(track: TrackId, index: u64) -> i16 {
    let v = index
        .wrapping_mul(31)
        .wrapping_add(u64::from(track.get()) * 7_919);
    // Keep the low 16 bits, as a signed sample.
    i16::from_le_bytes([v.to_le_bytes()[0], v.to_le_bytes()[1]])
}

fn samples(track: TrackId, from: u64, len: u64) -> Vec<i16> {
    (from..from + len).map(|i| sample(track, i)).collect()
}

fn fake_clock() -> (Arc<FakeClock>, Arc<dyn Clock>) {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let dyn_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    (clock, dyn_clock)
}

/// A new journal in `/session` for `track` from `first`, numbered `id`.
fn create(
    fs: &FakeFs,
    id: u64,
    track: TrackId,
    first: u64,
    clock: Arc<dyn Clock>,
) -> Result<JournalWriter<<FakeFs as Fs>::File>, JournalError> {
    JournalWriter::create(
        fs,
        &session(),
        header(id, track),
        SampleIndex::new(first),
        clock,
    )
}

/// How far `journal` has been fsync'd, checking the position names the
/// journal and track it came from.
fn durable_end<F: FsFile>(journal: &JournalWriter<F>) -> SampleIndex {
    let d = journal.durable();
    assert_eq!(
        d.journal(),
        journal.header().id(),
        "position of another journal"
    );
    assert_eq!(
        d.track(),
        journal.header().track(),
        "position of another track"
    );
    d.end()
}

/// The two journals of the recording scenario: (id, track, first sample).
const RECORDED: [(u64, TrackId, u64); 2] = [(0, MIC, 0), (1, SYSTEM, 5_000)];

/// What the recording scenario told its caller before it stopped.
#[derive(Debug, Default)]
struct Promised {
    /// Per track: whether `JournalWriter::create` returned, and with it
    /// where the track started. The file and its header are then durable.
    started: BTreeMap<TrackId, SampleIndex>,
    /// Per track: the last durable position its journal reported.
    durable: BTreeMap<TrackId, SampleIndex>,
    /// Per track: the end of everything handed to `append` successfully.
    captured: BTreeMap<TrackId, SampleIndex>,
}

/// Records two tracks in real time with a fake clock, each into its own
/// journal: chunks of uneven size, one longer than a frame, about 3.5 s in
/// all. Stops at the first error, as the recorder would.
fn record(fs: &FakeFs) -> Promised {
    let mut promised = Promised::default();
    let _ = record_into(fs, &mut promised);
    promised
}

fn record_into(fs: &FakeFs, promised: &mut Promised) -> Result<(), JournalError> {
    let (clock, dyn_clock) = fake_clock();
    let mut journals = Vec::new();
    let mut next = BTreeMap::new();
    for (id, track, first) in RECORDED {
        let journal = create(fs, id, track, first, Arc::clone(&dyn_clock))?;
        let at = SampleIndex::new(first);
        promised.started.insert(track, at);
        promised.captured.insert(track, journal.captured());
        promised.durable.insert(track, durable_end(&journal));
        next.insert(track, first);
        journals.push((track, journal));
    }
    let sizes = [1_600_u64, 800, 2_400, 10_000, 1_600, 320];
    for step in 0..14 {
        let len = sizes[step % sizes.len()];
        for (track, journal) in &mut journals {
            let track = *track;
            let from = next[&track];
            let appended = journal.append(&samples(track, from, len));
            // The writer's own word, even after a failure: a long append may
            // have written some frames before it failed.
            promised.captured.insert(track, journal.captured());
            promised.durable.insert(track, durable_end(journal));
            appended?;
            next.insert(track, from + len);
        }
        // Real time: the clock moves as much as the audio did.
        clock.advance(
            SampleCount::new(len)
                .duration_at(SampleRate::SPEECH)
                .unwrap(),
        );
    }
    for (track, journal) in journals {
        let header = journal.header();
        let d = journal.finish()?;
        assert_eq!((d.journal(), d.track()), (header.id(), header.track()));
        promised.durable.insert(track, d.end());
    }
    Ok(())
}

/// What recovery found of each recorded journal, by track: `None` if the
/// file is missing.
type Recovered = BTreeMap<TrackId, Option<JournalRead>>;

fn recover(fs: &FakeFs) -> Recovered {
    RECORDED
        .iter()
        .map(|&(id, track, _)| {
            let read = match fs.read(&journal_path(id)) {
                Ok(bytes) => Some(read_journal(&bytes)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                // The recovery fake never crashes in this test.
                Err(e) => panic!("recovery read failed: {e}"),
            };
            (track, read)
        })
        .collect()
}

/// The most audio that may be captured but not durable: the 850 ms sync
/// interval at 16 kHz. Kept below the 1.1 s bounded-loss rule so the
/// stream's buffering and a slow fsync still fit within it against the
/// audio delivered.
const LAG_LIMIT: SampleCount = SampleCount::new(13_600);

/// The journal's crash invariants. A closure, to match `CrashTest`'s check
/// signature without clippy's by-reference lints.
const CHECK_RECOVERED: fn(&CrashCase, &Promised, &Recovered) -> Result<(), String> =
    |case, promised, recovered| {
        check_lag(promised)?;
        for &(id, track, _) in &RECORDED {
            let read = recovered.get(&track).and_then(Option::as_ref);
            check_recovered(case, promised, header(id, track), read)?;
        }
        Ok(())
    };

/// The most any track had captured but not durable when the scenario
/// stopped.
fn worst_lag(promised: &Promised) -> SampleCount {
    promised
        .captured
        .iter()
        .map(|(track, &captured)| {
            // Every created journal has a durable position; a missing one
            // counts from zero, so it can only fail the check.
            let durable = promised
                .durable
                .get(track)
                .copied()
                .unwrap_or(SampleIndex::ZERO);
            captured
                .checked_count_since(durable)
                .unwrap_or(SampleCount::ZERO)
        })
        .max()
        .unwrap_or(SampleCount::ZERO)
}

/// At the crash, no track had more than [`LAG_LIMIT`] captured but not
/// durable.
fn check_lag(promised: &Promised) -> Result<(), String> {
    let lag = worst_lag(promised);
    if lag > LAG_LIMIT {
        return Err(format!(
            "{lag:?} captured but not durable: captured {:?}, durable {:?}",
            promised.captured, promised.durable
        ));
    }
    Ok(())
}

fn check_recovered(
    case: &CrashCase,
    promised: &Promised,
    want: JournalHeader,
    recovered: Option<&JournalRead>,
) -> Result<(), String> {
    let track = want.track();
    let Some(&start) = promised.started.get(&track) else {
        // Never created: no promise, so a missing file or a missing header
        // is fine. Whatever is there must still be this journal's, and its
        // audio never misread.
        return match recovered {
            None => Ok(()),
            Some(read) if read.header().is_none() => Ok(()),
            Some(read) if read.header() == Some(want) => recovered_range(track, read).map(drop),
            Some(read) => Err(format!(
                "track {track:?}: uncreated journal has header {:?}, ended {:?}",
                read.header(),
                read.end()
            )),
        };
    };
    let Some(read) = recovered else {
        return Err(format!(
            "track {track:?}: journal lost after promising {:?}",
            promised.durable.get(&track)
        ));
    };
    if read.header() != Some(want) {
        return Err(format!(
            "track {track:?}: header {:?}, wanted {want:?}, ended {:?}",
            read.header(),
            read.end()
        ));
    }
    let (start_found, end_found) = match recovered_range(track, read)? {
        Some(range) => (range.start(), range.end()),
        None => (start, start),
    };
    if start_found != start {
        return Err(format!(
            "track {track:?} recovered from {start_found:?}, started at {start:?}"
        ));
    }
    // Sample-continuous up to the last durable position...
    let durable = promised.durable[&track];
    if end_found < durable {
        return Err(format!(
            "track {track:?}: recovered to {end_found:?}, durable was {durable:?}"
        ));
    }
    // ...and never past what was captured.
    let captured = promised.captured[&track];
    if end_found > captured {
        return Err(format!("track {track:?}: recovered past the captured end"));
    }
    // Keeping everything recovers everything captured.
    if case.outcome == CrashOutcome::KeepAll && end_found != captured {
        return Err(format!(
            "track {track:?}: recovered to {end_found:?} with everything kept, captured {captured:?}"
        ));
    }
    // Losing everything unsynced loses exactly that.
    if case.outcome == CrashOutcome::LoseUnsynced && end_found != durable {
        return Err(format!(
            "track {track:?}: recovered to {end_found:?} with nothing unsynced kept, durable {durable:?}"
        ));
    }
    Ok(())
}

/// The samples `read` recovered for `track`, checked to be exactly the
/// ones recorded there: never misread.
fn recovered_range(track: TrackId, read: &JournalRead) -> Result<Option<SampleRange>, String> {
    let Some((range, got)) = read.audio() else {
        return Ok(None);
    };
    if got != samples(track, range.start().get(), range.len().get()) {
        return Err(format!("track {track:?}: recovered samples differ"));
    }
    Ok(Some(range))
}

#[test]
fn crash_after_every_operation_recovers_to_the_durable_position() {
    // The worst lag any crash point saw, to show the bound is approached.
    let worst = std::cell::Cell::new(SampleCount::ZERO);
    let summary = CrashTest::new(
        record,
        recover,
        |case: &CrashCase, promised: &Promised, recovered: &Recovered| {
            worst.set(worst.get().max(worst_lag(promised)));
            CHECK_RECOVERED(case, promised, recovered)
        },
    )
    .dirs(["/session"])
    .run()
    .unwrap_or_else(|failure| panic!("{failure}"));
    // Not vacuous: some crash came with most of the sync interval unsynced.
    assert!(worst.get() >= SampleCount::new(13_000), "{:?}", worst.get());
    // Not vacuous: dozens of writes and several syncs of each journal, each
    // crashed at.
    assert!(summary.scenario_ops > 40, "{summary:?}");
    let clean = FakeFs::with_dirs(["/session"]);
    let promised = record(&clean);
    for (id, track, _) in RECORDED {
        let syncs = clean
            .ops()
            .iter()
            .filter(|op| matches!(op, Op::Sync(p) if *p == journal_path(id)))
            .count();
        assert!(syncs >= 4, "only {syncs} syncs of track {track:?}");
    }
    assert_eq!(
        promised.started.keys().collect::<Vec<_>>(),
        [&MIC, &SYSTEM],
        "a clean run creates both journals"
    );
    assert_eq!(
        promised.durable, promised.captured,
        "a clean run ends fully synced"
    );
}

#[test]
fn a_journal_without_its_directory_sync_fails_the_crash_test() {
    // The crash test can tell: the same writer minus the directory sync
    // loses the whole journal while promising durability.
    let without_dir_sync = |fs: &FakeFs| {
        let mut promised = Promised::default();
        let run = |promised: &mut Promised| -> io::Result<()> {
            let mut file = fs.create(&journal_path(0))?;
            let mut bytes = encode_header(header(0, MIC)).to_vec();
            encode_frame(&mut bytes, 0, MIC, SampleIndex::ZERO, &samples(MIC, 0, 100));
            file.write_all(&bytes)?;
            file.sync()?;
            promised.started.insert(MIC, SampleIndex::ZERO);
            promised.captured.insert(MIC, SampleIndex::new(100));
            promised.durable.insert(MIC, SampleIndex::new(100));
            Ok(())
        };
        let _ = run(&mut promised);
        promised
    };
    let failure = CrashTest::new(without_dir_sync, recover, CHECK_RECOVERED)
        .dirs(["/session"])
        .run()
        .unwrap_err();
    assert!(failure.message.contains("journal lost"), "{failure}");
}

#[test]
fn durable_stays_within_the_sync_interval_of_captured_in_a_timed_run() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    let limit = LAG_LIMIT;
    let chunk = 160; // 10 ms, as a capture callback delivers it
    let mut worst = SampleCount::ZERO;
    for i in 0..6_000 {
        journal.append(&samples(MIC, i * chunk, chunk)).unwrap();
        clock.advance(Duration::from_millis(10));
        let lag = journal
            .captured()
            .checked_count_since(durable_end(&journal))
            .unwrap();
        worst = worst.max(lag);
        assert!(lag <= limit, "at chunk {i}: {lag:?} unsynced");
    }
    // A minute of audio, synced every 850 ms: about 70 times.
    let syncs = fs
        .ops()
        .iter()
        .filter(|op| matches!(op, Op::Sync(_)))
        .count();
    assert!((68..=73).contains(&syncs), "{syncs} syncs in 60 s");
    assert!(worst >= SampleCount::new(13_000), "never lagged: {worst:?}");
}

#[test]
fn a_burst_faster_than_real_time_still_syncs_each_interval_of_audio() {
    // The clock stands still; the audio bound alone keeps durable close.
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    for i in 0..100 {
        journal.append(&samples(MIC, i * 1_000, 1_000)).unwrap();
        let lag = journal
            .captured()
            .checked_count_since(durable_end(&journal))
            .unwrap();
        assert!(lag < LAG_LIMIT, "at {i}: {lag:?}");
    }
}

#[test]
fn a_rate_too_low_for_one_sample_per_interval_still_records() {
    // At 1 Hz the sync interval holds no whole sample; the budget is still
    // one, so each sample is written and synced rather than the writer
    // syncing forever without writing.
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let slow = SampleRate::new(1).unwrap();
    let mut journal = JournalWriter::create(
        &fs,
        &session(),
        JournalHeader::new(JournalId::new(0), MIC, EPOCH, slow),
        SampleIndex::ZERO,
        dyn_clock,
    )
    .unwrap();
    journal.append(&samples(MIC, 0, 3)).unwrap();
    assert_eq!(journal.captured(), SampleIndex::new(3));
    assert_eq!(durable_end(&journal), SampleIndex::new(3));
    let read = read_journal(&fs.read(&journal_path(0)).unwrap());
    let lens: Vec<_> = read
        .frames()
        .iter()
        .map(|f| f.range().len().get())
        .collect();
    assert_eq!(lens, [1, 1, 1]);
}

#[test]
fn sync_if_due_syncs_a_stalled_track_after_the_interval() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    let at_create = journal.durable();
    journal.append(&samples(MIC, 0, 160)).unwrap();
    assert_eq!(
        journal.durable(),
        at_create,
        "an append alone isn't durable"
    );
    assert_eq!(at_create.end(), SampleIndex::ZERO);
    clock.advance(Duration::from_millis(849));
    assert!(!journal.sync_if_due().unwrap());
    clock.advance(Duration::from_millis(1));
    assert!(journal.sync_if_due().unwrap());
    assert_eq!(journal.durable().end(), SampleIndex::new(160));
    assert_eq!(journal.durable().track(), MIC);
    assert_eq!(journal.durable().journal(), JournalId::FIRST);
    // Nothing new: no pointless fsync.
    clock.advance(Duration::from_secs(5));
    assert!(!journal.sync_if_due().unwrap());
}

#[test]
fn a_failed_write_breaks_the_journal() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    journal.append(&samples(MIC, 0, 10)).unwrap();
    assert!(!journal.is_broken());
    fs.crash_after(0);
    assert!(matches!(
        journal.append(&samples(MIC, 10, 10)),
        Err(JournalError::Io(_))
    ));
    assert!(journal.is_broken());
    assert_eq!(journal.captured(), SampleIndex::new(10));
    assert!(matches!(journal.append(&[1]), Err(JournalError::Broken)));
    assert!(matches!(journal.sync(), Err(JournalError::Broken)));
    assert!(matches!(journal.sync_if_due(), Err(JournalError::Broken)));
    assert!(matches!(journal.finish(), Err(JournalError::Broken)));
}

#[test]
fn a_failed_fsync_breaks_the_journal_without_moving_durable() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, 0, Arc::clone(&dyn_clock)).unwrap();
    journal.append(&samples(MIC, 0, 10)).unwrap();
    journal.sync().unwrap();
    journal.append(&samples(MIC, 10, 10)).unwrap();
    // EIO from fsync, and the process lives on.
    fs.fail_after(0, io::ErrorKind::Other);
    assert!(matches!(journal.sync(), Err(JournalError::Io(_))));
    assert!(!fs.has_crashed());
    assert_eq!(journal.durable().end(), SampleIndex::new(10));
    // No retry: the kernel may have dropped the data, so another fsync
    // "succeeding" would claim audio that's gone.
    assert!(matches!(journal.sync(), Err(JournalError::Broken)));
    assert!(matches!(journal.append(&[1]), Err(JournalError::Broken)));
    // What was durable is still there, and a new journal can start. The
    // unsynced frame still reads back too, as on Linux, though it may
    // never reach the disk.
    let read = read_journal(&fs.read(&journal_path(0)).unwrap());
    assert_eq!(read.audio().unwrap().1, samples(MIC, 0, 20));
    let mut journal = create(&fs, 1, MIC, 10, dyn_clock).unwrap();
    journal.append(&samples(MIC, 10, 10)).unwrap();
    journal.sync().unwrap();
    assert_eq!(journal.durable().end(), SampleIndex::new(20));
    // After a crash that loses what wasn't written back, only the durable
    // frames of the broken journal are left.
    let after = fs.crash(CrashOutcome::LoseUnsynced);
    let read = read_journal(&after.read(&journal_path(0)).unwrap());
    assert_eq!(read.audio().unwrap().1, samples(MIC, 0, 10));
}

#[test]
fn a_full_disk_breaks_the_journal_but_not_the_process() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    fs.fail_after(0, io::ErrorKind::StorageFull);
    assert!(matches!(
        journal.append(&samples(MIC, 0, 10)),
        Err(JournalError::Io(e)) if e.kind() == io::ErrorKind::StorageFull
    ));
    assert_eq!(journal.captured(), SampleIndex::ZERO);
    assert!(matches!(journal.append(&[1]), Err(JournalError::Broken)));
    assert!(fs.read(&journal_path(0)).is_ok());
}

#[test]
fn a_long_append_never_leaves_more_than_the_sync_interval_unsynced() {
    // One append of ten seconds, crashed after every operation: whatever
    // the writer reports, captured is never more than the sync interval
    // ahead of durable, and the durable part survives.
    for crash_at in 0..60 {
        let fs = FakeFs::with_dirs(["/session"]);
        let (_clock, dyn_clock) = fake_clock();
        let mut journal = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
        fs.crash_after(crash_at);
        let done = journal.append(&samples(MIC, 0, 160_000)).is_ok();
        let captured = journal.captured();
        let durable = durable_end(&journal);
        let lag = captured.checked_count_since(durable).unwrap();
        assert!(lag <= LAG_LIMIT, "crash at {crash_at}: {lag:?} unsynced");
        let after = fs.crash(CrashOutcome::LoseUnsynced);
        let read = read_journal(&after.read(&journal_path(0)).unwrap());
        let end = read.range().map_or(SampleIndex::ZERO, SampleRange::end);
        assert_eq!(end, durable, "crash at {crash_at}");
        if done {
            assert_eq!(captured, SampleIndex::new(160_000));
        }
    }
}

#[test]
fn track_errors_write_nothing() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, u64::MAX - 1, dyn_clock).unwrap();
    assert!(matches!(
        journal.append(&[1, 2]),
        Err(JournalError::SampleOverflow)
    ));
    assert!(!journal.is_broken(), "an overflow writes nothing to break");
    journal.append(&[]).unwrap();
    journal.append(&[1]).unwrap();
    assert_eq!(journal.captured(), SampleIndex::new(u64::MAX));
    assert_eq!(journal.start(), SampleIndex::new(u64::MAX - 1));
    let written = fs.read(&journal_path(0)).unwrap();
    assert_eq!(written.len(), HEADER_LEN + FRAME_HEADER_LEN + 2);
    assert_eq!(journal.header(), header(0, MIC));
    // Only the good append made it in, and it reads back.
    let read = read_journal(&written);
    assert_eq!(read.frames().len(), 1);
    assert_eq!(read.end(), ReadEnd::Complete);
}

#[test]
fn create_refuses_an_existing_journal() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let _first = create(&fs, 0, MIC, 0, Arc::clone(&dyn_clock)).unwrap();
    // Same id, even for another track: the file name is the id's.
    let again = create(&fs, 0, SYSTEM, 0, dyn_clock);
    assert!(matches!(again, Err(JournalError::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists));
    assert_eq!(fs.paths(), [journal_path(0)]);
}

#[test]
fn long_appends_split_into_frames() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, 7, dyn_clock).unwrap();
    let max = u64::from(MAX_FRAME_SAMPLES);
    journal.append(&samples(MIC, 7, 2 * max + 1)).unwrap();
    let read = read_journal(&fs.read(&journal_path(0)).unwrap());
    let lens: Vec<_> = read
        .frames()
        .iter()
        .map(|f| f.range().len().get())
        .collect();
    // A full frame, then what's left of the first sync interval (13 600
    // samples), a sync, and the rest.
    let budget = LAG_LIMIT.get();
    assert_eq!(lens, [max, budget - max, 2 * max + 1 - budget]);
    let syncs = fs
        .ops()
        .iter()
        .filter(|op| matches!(op, Op::Sync(_)))
        .count();
    assert_eq!(
        syncs, 2,
        "the header's sync and the one at the sync interval's audio"
    );
    let (range, got) = read.audio().unwrap();
    assert_eq!(
        (range.start().get(), range.end().get()),
        (7, 7 + 2 * max + 1)
    );
    assert_eq!(got, samples(MIC, 7, 2 * max + 1));
    // Every frame is the header's track.
    assert_eq!(read.header().map(JournalHeader::track), Some(MIC));
    assert!(read.frames().iter().all(|f| f.track() == MIC));
}

/// A synced journal of three frames, then a fourth written but not synced.
fn journal_with_unsynced_frame() -> (FakeFs, usize, Vec<u8>) {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    for i in 0..3 {
        journal.append(&samples(MIC, i * 100, 100)).unwrap();
    }
    journal.sync().unwrap();
    let synced_len = fs.read(&journal_path(0)).unwrap().len();
    journal.append(&samples(MIC, 300, 100)).unwrap();
    let full = fs.read(&journal_path(0)).unwrap();
    (fs, synced_len, full)
}

#[test]
fn torn_final_frame_is_dropped_at_every_length() {
    let (_fs, synced_len, full) = journal_with_unsynced_frame();
    for cut in synced_len..full.len() {
        let read = read_journal(&full[..cut]);
        assert_eq!(read.frames().len(), 3, "cut at {cut}");
        assert_eq!(read.valid_len(), synced_len);
        let want = if cut == synced_len {
            ReadEnd::Complete
        } else {
            ReadEnd::Incomplete { offset: synced_len }
        };
        assert_eq!(read.end(), want, "cut at {cut}");
    }
    assert_eq!(read_journal(&full).frames().len(), 4);
}

#[test]
fn torn_final_frame_that_reads_back_as_zeros_is_dropped() {
    let (_fs, synced_len, full) = journal_with_unsynced_frame();
    for cut in synced_len + 1..=full.len() {
        let mut bytes = full[..cut].to_vec();
        bytes[synced_len..].fill(0);
        let read = read_journal(&bytes);
        assert_eq!(read.frames().len(), 3, "cut at {cut}");
        assert_ne!(read.end(), ReadEnd::Complete, "cut at {cut}");
    }
}

#[test]
fn torn_frame_with_a_corrupt_byte_fails_its_crc() {
    let (_fs, synced_len, full) = journal_with_unsynced_frame();
    // Every byte of the last frame's samples, damaged in turn.
    for at in synced_len + FRAME_HEADER_LEN..full.len() {
        let mut bytes = full.clone();
        bytes[at] ^= 0x40;
        let read = read_journal(&bytes);
        assert_eq!(read.frames().len(), 3);
        assert_eq!(
            read.end(),
            ReadEnd::Invalid {
                offset: synced_len,
                reason: Invalid::Crc
            }
        );
    }
}

#[test]
fn partial_crashes_of_the_unsynced_frame_never_misread() {
    let (fs, _synced_len, _full) = journal_with_unsynced_frame();
    for seed in 0..200 {
        let after = fs.crash(CrashOutcome::Partial { seed });
        let read = read_journal(&after.read(&journal_path(0)).unwrap());
        let n = read.frames().len();
        assert!(n == 3 || n == 4, "seed {seed}: {n} frames");
        let (_, got) = read.audio().unwrap();
        assert_eq!(got, samples(MIC, 0, 100 * n as u64), "seed {seed}");
    }
}

#[test]
fn header_problems_are_reported() {
    let bytes = encode_header(header(0, MIC));
    assert_eq!(read_journal(&[]).end(), ReadEnd::Incomplete { offset: 0 });
    assert_eq!(read_journal(&bytes[..HEADER_LEN - 1]).header(), None);
    let ok = read_journal(&bytes);
    assert_eq!(ok.header(), Some(header(0, MIC)));
    assert_eq!((ok.end(), ok.valid_len()), (ReadEnd::Complete, HEADER_LEN));
    for at in 0..HEADER_LEN {
        let mut bad = bytes;
        bad[at] ^= 1;
        let read = read_journal(&bad);
        assert_eq!(read.header(), None, "byte {at}");
        assert_eq!(
            read.end(),
            ReadEnd::Invalid {
                offset: 0,
                reason: Invalid::Header
            }
        );
    }
}

#[test]
fn headers_with_a_valid_crc_but_bad_fields_are_refused() {
    // Another version (an older or a future one) in the v2 layout, or a zero
    // rate, with a correct CRC.
    let good = encode_header(header(0, MIC));
    let mut old = good;
    old[8..10].copy_from_slice(&1_u16.to_le_bytes());
    let mut future = good;
    future[8..10].copy_from_slice(&3_u16.to_le_bytes());
    let mut zero_rate = good;
    zero_rate[10..14].fill(0);
    for mut bad in [old, future, zero_rate] {
        let crc = crc32fast::hash(&bad[..30]);
        bad[30..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(read_journal(&bad).header(), None, "{bad:?}");
    }
}

#[test]
fn a_version_1_journal_is_refused() {
    // The old 18-byte layout: magic, version 1, rate, CRC of bytes 0..14.
    let mut v1 = b"NOTAJRNL".to_vec();
    v1.extend_from_slice(&1_u16.to_le_bytes());
    v1.extend_from_slice(&SampleRate::SPEECH.hz().to_le_bytes());
    let crc = crc32fast::hash(&v1);
    v1.extend_from_slice(&crc.to_le_bytes());
    assert_eq!(v1.len(), 18);
    // On its own it's shorter than a v2 header: no header, nothing read.
    let read = read_journal(&v1);
    assert_eq!(read.header(), None);
    assert_eq!(read.valid_len(), 0);
    // With a frame after it, as a v1 journal had: refused, not misread.
    encode_frame(&mut v1, 0, MIC, SampleIndex::ZERO, &samples(MIC, 0, 100));
    let read = read_journal(&v1);
    assert_eq!(read.header(), None);
    assert!(read.frames().is_empty());
    assert_eq!(read.valid_len(), 0);
    assert_eq!(
        read.end(),
        ReadEnd::Invalid {
            offset: 0,
            reason: Invalid::Header
        }
    );
}

#[test]
fn the_v2_header_round_trips_and_names_the_file() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let rate = SampleRate::new(48_000).unwrap();
    let wanted = [
        JournalHeader::new(JournalId::FIRST, MIC, EPOCH, SampleRate::SPEECH),
        JournalHeader::new(JournalId::new(42), TrackId::new(3), EpochId::new(7), rate),
    ];
    for (want, name) in wanted.into_iter().zip(["journal-000000", "journal-000042"]) {
        let mut journal = JournalWriter::create(
            &fs,
            &session(),
            want,
            SampleIndex::new(9),
            Arc::clone(&dyn_clock),
        )
        .unwrap();
        assert_eq!(journal.path(), session().join(name));
        assert_eq!(journal.header(), want);
        journal.append(&samples(want.track(), 9, 10)).unwrap();
        journal.finish().unwrap();
        let bytes = fs.read(&session().join(name)).unwrap();
        assert_eq!(&bytes[..HEADER_LEN], encode_header(want));
        assert_eq!(&bytes[8..10], 2_u16.to_le_bytes(), "format version 2");
        let read = read_journal(&bytes);
        let got = read.header().unwrap();
        assert_eq!(
            (got.id(), got.track(), got.epoch(), got.rate()),
            (want.id(), want.track(), want.epoch(), want.rate())
        );
        assert_eq!(read.audio().unwrap().1, samples(want.track(), 9, 10));
        assert_eq!(read.end(), ReadEnd::Complete);
    }
    assert_eq!(
        fs.paths(),
        [
            session().join("journal-000000"),
            session().join("journal-000042")
        ]
    );
}

#[test]
fn durable_starts_at_the_first_sample_and_a_crash_after_create_keeps_the_header() {
    for outcome in CrashOutcome::standard() {
        let fs = FakeFs::with_dirs(["/session"]);
        let (_clock, dyn_clock) = fake_clock();
        let journal = create(&fs, 3, SYSTEM, 5_000, dyn_clock).unwrap();
        let durable = journal.durable();
        assert_eq!(durable.end(), SampleIndex::new(5_000));
        assert_eq!(durable.journal(), JournalId::new(3));
        assert_eq!(durable.track(), SYSTEM);
        assert_eq!(journal.start(), SampleIndex::new(5_000));
        assert_eq!(journal.captured(), SampleIndex::new(5_000));
        // A crash right after create returns: the file and its header are
        // there, with no audio.
        let after = fs.crash(outcome);
        let read = read_journal(&after.read(&journal_path(3)).unwrap());
        assert_eq!(read.header(), Some(header(3, SYSTEM)), "{outcome:?}");
        assert!(read.frames().is_empty(), "{outcome:?}");
        assert_eq!(read.end(), ReadEnd::Complete, "{outcome:?}");
    }
}

#[test]
fn positions_in_two_journals_of_one_track_never_compare_equal() {
    // Journal 0 breaks; journal 1 replaces it, carrying on the same track's
    // sample numbers from where 0's durable position ended.
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut broken = create(&fs, 0, MIC, 0, Arc::clone(&dyn_clock)).unwrap();
    broken.append(&samples(MIC, 0, 100)).unwrap();
    broken.sync().unwrap();
    broken.append(&samples(MIC, 100, 50)).unwrap();
    fs.fail_after(0, io::ErrorKind::Other);
    assert!(broken.sync().is_err());
    let old = broken.durable();
    assert_eq!(old.end(), SampleIndex::new(100));

    let replacement = create(&fs, 1, MIC, old.end().get(), dyn_clock).unwrap();
    let new = replacement.durable();
    // The same track and end...
    assert_eq!((new.track(), new.end()), (old.track(), old.end()));
    // ...but not the same position.
    assert_ne!(new, old);
    assert_ne!(new.journal(), old.journal());
    assert_eq!(
        (old.journal(), new.journal()),
        (JournalId::FIRST, JournalId::new(1))
    );
    // A position equals itself: equality isn't simply always false.
    assert_eq!(broken.durable(), old);
    assert_eq!(replacement.durable(), new);
}

/// A header for `MIC`, then the frames given as (seq, track, first,
/// samples).
fn build(frames: &[(u64, TrackId, u64, Vec<i16>)]) -> Vec<u8> {
    let mut bytes = encode_header(header(0, MIC)).to_vec();
    for (seq, track, first, s) in frames {
        encode_frame(&mut bytes, *seq, *track, SampleIndex::new(*first), s);
    }
    bytes
}

#[test]
fn frames_out_of_sequence_or_discontinuous_are_invalid() {
    let a = samples(MIC, 0, 4);
    let b = samples(MIC, 4, 4);
    let skip = build(&[(0, MIC, 0, a.clone()), (2, MIC, 4, b.clone())]);
    assert_eq!(
        read_journal(&skip).end(),
        ReadEnd::Invalid {
            offset: HEADER_LEN + FRAME_HEADER_LEN + 8,
            reason: Invalid::Sequence {
                expected: 1,
                found: 2
            }
        }
    );
    let gap = build(&[(0, MIC, 0, a.clone()), (1, MIC, 5, b.clone())]);
    let read = read_journal(&gap);
    assert_eq!(read.frames().len(), 1);
    assert!(matches!(
        read.end(),
        ReadEnd::Invalid {
            reason: Invalid::Discontinuous { track, expected, found },
            ..
        } if track == MIC && expected.get() == 4 && found.get() == 5
    ));
    // The first frame may start anywhere; each one after continues it.
    let late = build(&[
        (0, MIC, 900, a.clone()),
        (1, MIC, 904, b.clone()),
        (2, MIC, 908, a),
    ]);
    let read = read_journal(&late);
    assert_eq!((read.frames().len(), read.end()), (3, ReadEnd::Complete));
    assert_eq!(read.frames()[1].seq(), 1);
    assert_eq!(
        read.range().map(|r| (r.start().get(), r.end().get())),
        Some((900, 912))
    );
}

#[test]
fn a_frame_of_another_track_is_invalid() {
    // In sequence, continuous and with a good CRC, but not the header's
    // track: the reader stops before it.
    let a = samples(MIC, 0, 4);
    let b = samples(SYSTEM, 4, 4);
    let mixed = build(&[
        (0, MIC, 0, a.clone()),
        (1, SYSTEM, 4, b),
        (2, MIC, 8, a.clone()),
    ]);
    let read = read_journal(&mixed);
    assert_eq!(read.frames().len(), 1);
    assert_eq!(read.valid_len(), HEADER_LEN + FRAME_HEADER_LEN + 8);
    assert_eq!(
        read.end(),
        ReadEnd::Invalid {
            offset: HEADER_LEN + FRAME_HEADER_LEN + 8,
            reason: Invalid::Track {
                expected: MIC,
                found: SYSTEM
            }
        }
    );
    // Even as the first frame.
    let first = build(&[(0, SYSTEM, 0, a)]);
    let read = read_journal(&first);
    assert!(read.frames().is_empty());
    assert_eq!(
        read.end(),
        ReadEnd::Invalid {
            offset: HEADER_LEN,
            reason: Invalid::Track {
                expected: MIC,
                found: SYSTEM
            }
        }
    );
}

#[test]
fn bad_lengths_and_magic_are_invalid() {
    let mut zero = build(&[]);
    encode_frame(&mut zero, 0, MIC, SampleIndex::ZERO, &[]);
    assert!(matches!(
        read_journal(&zero).end(),
        ReadEnd::Invalid {
            reason: Invalid::Length,
            ..
        }
    ));

    let too_long = vec![0_i16; MAX_FRAME_SAMPLES as usize + 1];
    let mut long = build(&[]);
    encode_frame(&mut long, 0, MIC, SampleIndex::ZERO, &too_long);
    assert!(matches!(
        read_journal(&long).end(),
        ReadEnd::Invalid {
            reason: Invalid::Length,
            ..
        }
    ));

    let overflow = build(&[(0, MIC, u64::MAX, vec![1, 2])]);
    assert!(matches!(
        read_journal(&overflow).end(),
        ReadEnd::Invalid {
            reason: Invalid::Length,
            ..
        }
    ));

    let mut magic = build(&[(0, MIC, 0, vec![1])]);
    magic[HEADER_LEN] = b'X';
    assert!(matches!(
        read_journal(&magic).end(),
        ReadEnd::Invalid {
            reason: Invalid::Magic,
            ..
        }
    ));
    // A short tail that already isn't a frame is invalid, not torn.
    let mut tail = build(&[]);
    tail.extend_from_slice(b"JUNK");
    assert!(matches!(
        read_journal(&tail).end(),
        ReadEnd::Invalid {
            reason: Invalid::Magic,
            ..
        }
    ));
    // Every reason describes itself.
    for reason in [
        Invalid::Header,
        Invalid::Track {
            expected: MIC,
            found: SYSTEM,
        },
        Invalid::Magic,
        Invalid::Length,
        Invalid::Crc,
        Invalid::Sequence {
            expected: 1,
            found: 2,
        },
        Invalid::Discontinuous {
            track: MIC,
            expected: SampleIndex::ZERO,
            found: SampleIndex::new(3),
        },
    ] {
        assert!(!reason.to_string().is_empty());
    }
}

#[test]
fn journal_on_the_real_filesystem() {
    let dir = TestDir::new("journal");
    let (_clock, dyn_clock) = fake_clock();
    let mut journal =
        JournalWriter::create(&StdFs, &dir.0, header(0, MIC), SampleIndex::ZERO, dyn_clock)
            .unwrap();
    let path = dir.0.join("journal-000000");
    assert_eq!(journal.path(), path);
    journal.append(&samples(MIC, 0, 20_000)).unwrap();
    journal.sync().unwrap();
    let read = read_journal(&StdFs.read(&path).unwrap());
    assert_eq!(read.header(), Some(header(0, MIC)));
    assert_eq!(read.audio().unwrap().1, samples(MIC, 0, 20_000));
    assert_eq!(read.end(), ReadEnd::Complete);
}

#[test]
fn errors_describe_themselves() {
    let errors = [
        JournalError::Io(io::Error::other("disk")),
        JournalError::SampleOverflow,
        JournalError::Broken,
    ];
    for e in &errors {
        assert!(!e.to_string().is_empty());
    }
    assert!(std::error::Error::source(&errors[0]).is_some());
    assert!(std::error::Error::source(&errors[1]).is_none());
    assert!(std::error::Error::source(&errors[2]).is_none());
}

#[test]
fn create_needs_a_directory() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let missing = JournalWriter::create(
        &fs,
        Path::new("/missing"),
        header(0, MIC),
        SampleIndex::ZERO,
        dyn_clock,
    );
    assert!(matches!(missing, Err(JournalError::Io(_))));
    assert_eq!(fs.paths(), Vec::<PathBuf>::new());
}

#[test]
fn a_failed_create_removes_its_file_so_a_retry_works() {
    // Fail each step after the file exists: the header write, its fsync,
    // the directory fsync.
    for step in 1..=3 {
        let fs = FakeFs::with_dirs(["/session"]);
        let (_clock, dyn_clock) = fake_clock();
        fs.fail_after(step, io::ErrorKind::StorageFull);
        let first = create(&fs, 0, MIC, 0, Arc::clone(&dyn_clock));
        assert!(matches!(first, Err(JournalError::Io(_))), "step {step}");
        assert_eq!(fs.paths(), Vec::<PathBuf>::new(), "step {step}");
        create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    }
}

#[test]
fn finish_syncs_what_was_captured() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    journal.append(&samples(MIC, 0, 100)).unwrap();
    let done = journal.finish().unwrap();
    assert_eq!(
        (done.journal(), done.track(), done.end()),
        (JournalId::FIRST, MIC, SampleIndex::new(100))
    );
    let after = fs.crash(CrashOutcome::LoseUnsynced);
    let read = read_journal(&after.read(&journal_path(0)).unwrap());
    assert_eq!(read.audio().unwrap().1, samples(MIC, 0, 100));

    // Nothing new: no fsync. Broken: the error.
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let journal = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    let before = fs.ops().len();
    let done = journal.finish().unwrap();
    assert_eq!(done.end(), SampleIndex::ZERO);
    assert_eq!(fs.ops().len(), before);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = create(&fs, 1, MIC, 0, dyn_clock).unwrap();
    fs.fail_after(0, io::ErrorKind::StorageFull);
    assert!(journal.append(&[1]).is_err());
    assert!(matches!(journal.finish(), Err(JournalError::Broken)));
}

#[test]
fn the_scan_skips_frames_out_of_range_and_of_other_tracks() {
    let track = TrackId::new(3);
    let frame = |track, first: u64, len: u32| {
        let mut out = Vec::new();
        let samples = vec![7; usize::try_from(len).unwrap()];
        encode_frame(&mut out, 0, track, SampleIndex::new(first), &samples);
        out
    };
    let mut bytes = Vec::new();
    // The largest frame counts; one sample more, none at all, or another
    // track's, each with a valid CRC, don't.
    bytes.extend(frame(track, 0, MAX_FRAME_SAMPLES));
    bytes.extend(frame(track, 100_000, MAX_FRAME_SAMPLES + 1));
    bytes.extend(frame(track, 200_000, 0));
    bytes.extend(frame(TrackId::new(4), 300_000, 5));
    bytes.extend(frame(track, 400_000, 5));
    let found: Vec<_> = format::frames_after(&bytes, 0, track)
        .iter()
        .map(|r| (r.start().get(), r.len().get()))
        .collect();
    assert_eq!(found, [(0, u64::from(MAX_FRAME_SAMPLES)), (400_000, 5)]);
}

// ---------------------------------------------------------------------------
// Syncs started here and run elsewhere.

/// The sync budget at speech rate: 850 ms, 13,600 samples.
const BUDGET: u64 = 13_600;

fn durable_at(journal: &JournalWriter<<FakeFs as Fs>::File>) -> u64 {
    journal.durable().end().get()
}

#[test]
fn a_sync_completed_after_more_appends_covers_only_what_came_before_it() {
    let fs = FakeFs::with_dirs([session()]);
    let (_, clock) = fake_clock();
    let mut j = create(&fs, 0, MIC, 0, clock).unwrap();
    j.append_within(&samples(MIC, 0, 100)).unwrap();
    let job = j.begin_sync().unwrap();
    assert_eq!(job.journal(), JournalId::new(0));
    assert!(j.sync_in_flight());
    j.append_within(&samples(MIC, 100, 50)).unwrap();
    let done = job.run();
    assert_eq!(done.journal(), JournalId::new(0));
    j.complete_sync(done).unwrap();
    assert!(!j.sync_in_flight());
    assert_eq!((durable_at(&j), j.captured().get()), (100, 150));
    assert!(j.needs_sync());
    assert!(!j.is_settled());
    // The file itself got the whole fsync: all 150 survive a crash, though
    // only 100 are promised.
    let after = fs.crash(CrashOutcome::LoseUnsynced);
    let read = read_journal(&after.read(&journal_path(0)).unwrap());
    assert_eq!(read.range().unwrap().end().get(), 150);
}

#[test]
fn append_within_stops_at_the_budget_and_never_syncs() {
    let fs = FakeFs::with_dirs([session()]);
    let (_, clock) = fake_clock();
    let mut j = create(&fs, 0, MIC, 0, clock).unwrap();
    let syncs = |fs: &FakeFs| {
        fs.ops()
            .iter()
            .filter(|op| matches!(op, Op::Sync(_)))
            .count()
    };
    let before = syncs(&fs);
    assert_eq!(j.room(), BUDGET);
    assert_eq!(
        j.append_within(&samples(MIC, 0, BUDGET + 500)).unwrap(),
        usize::try_from(BUDGET).unwrap()
    );
    assert_eq!(syncs(&fs), before);
    assert_eq!(j.room(), 0);
    assert!(j.sync_due(), "a full budget is due");
    assert_eq!(j.append_within(&samples(MIC, BUDGET, 10)).unwrap(), 0);
    let job = j.begin_sync().unwrap();
    assert!(!j.sync_due(), "not while one is in flight");
    assert_eq!(j.append_within(&samples(MIC, BUDGET, 10)).unwrap(), 0);
    j.complete_sync(job.run()).unwrap();
    assert_eq!(j.room(), BUDGET);
    assert!(j.is_settled());
    assert_eq!(j.append_within(&samples(MIC, BUDGET, 10)).unwrap(), 10);
    assert_eq!(j.captured().get(), BUDGET + 10);
}

#[test]
fn a_sync_is_due_once_the_interval_has_passed_since_the_last_one_started() {
    let fs = FakeFs::with_dirs([session()]);
    let (clock, dyn_clock) = fake_clock();
    let mut j = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    // Nothing written: never due.
    clock.advance(SYNC_INTERVAL);
    assert!(!j.sync_due());
    assert!(!j.needs_sync());
    j.append_within(&samples(MIC, 0, 1)).unwrap();
    assert!(j.sync_due());
    let job = j.begin_sync().unwrap();
    clock.advance(SYNC_INTERVAL);
    j.append_within(&samples(MIC, 1, 1)).unwrap();
    assert!(!j.sync_due(), "one is in flight");
    j.complete_sync(job.run()).unwrap();
    // The interval since that one started has passed.
    assert!(j.sync_due());
    let job = j.begin_sync().unwrap();
    j.complete_sync(job.run()).unwrap();
    j.append_within(&samples(MIC, 2, 1)).unwrap();
    clock.advance(SYNC_INTERVAL.checked_sub(Duration::from_millis(1)).unwrap());
    assert!(!j.sync_due());
    clock.advance(Duration::from_millis(1));
    assert!(j.sync_due());
}

#[test]
fn a_failed_sync_breaks_the_journal_and_a_later_success_moves_nothing() {
    let fs = FakeFs::with_dirs([session()]);
    let (_, clock) = fake_clock();
    let mut j = create(&fs, 0, MIC, 0, clock).unwrap();
    j.append_within(&samples(MIC, 0, 100)).unwrap();
    let first = j.begin_sync().unwrap();
    j.append_within(&samples(MIC, 100, 100)).unwrap();
    let second = j.begin_sync().unwrap();
    fs.fail_after(0, io::ErrorKind::Other);
    let failed = first.run();
    let succeeded = second.run();
    assert!(matches!(j.complete_sync(failed), Err(JournalError::Io(_))));
    assert!(j.is_broken());
    assert!(!j.sync_in_flight());
    j.complete_sync(succeeded).unwrap();
    assert_eq!(durable_at(&j), 0);
    assert!(matches!(j.begin_sync(), Err(JournalError::Broken)));
    assert!(matches!(j.append_within(&[1]), Err(JournalError::Broken)));
    assert!(!j.sync_due());
}

#[test]
fn a_newer_sync_counts_only_once_every_older_one_has_succeeded() {
    let fs = FakeFs::with_dirs([session()]);
    let (_, clock) = fake_clock();
    let mut j = create(&fs, 0, MIC, 0, clock).unwrap();
    j.append_within(&samples(MIC, 0, 100)).unwrap();
    let first = j.begin_sync().unwrap();
    j.append_within(&samples(MIC, 100, 100)).unwrap();
    let second = j.begin_sync().unwrap();
    let (first, second) = (first.run(), second.run());
    j.complete_sync(second).unwrap();
    // The older one might yet fail, losing what the newer one covers.
    assert_eq!(durable_at(&j), 0);
    assert!(j.sync_in_flight());
    j.complete_sync(first).unwrap();
    assert_eq!(durable_at(&j), 200);
    assert!(!j.sync_in_flight());
    assert!(j.is_settled());

    // In order, each counts at once.
    j.append_within(&samples(MIC, 200, 1)).unwrap();
    let third = j.begin_sync().unwrap();
    j.append_within(&samples(MIC, 201, 1)).unwrap();
    let fourth = j.begin_sync().unwrap();
    j.complete_sync(third.run()).unwrap();
    assert_eq!(durable_at(&j), 201);
    assert!(j.sync_in_flight(), "the newer sync is still out");
    j.complete_sync(fourth.run()).unwrap();
    assert!(!j.sync_in_flight());
    assert_eq!(durable_at(&j), 202);
}

#[test]
fn an_older_sync_failing_after_a_newer_one_succeeded_proves_nothing() {
    let fs = FakeFs::with_dirs([session()]);
    let (_, clock) = fake_clock();
    let mut j = create(&fs, 0, MIC, 0, clock).unwrap();
    j.append_within(&samples(MIC, 0, 100)).unwrap();
    let first = j.begin_sync().unwrap();
    j.append_within(&samples(MIC, 100, 100)).unwrap();
    let second = j.begin_sync().unwrap();
    fs.fail_after(0, io::ErrorKind::Other);
    let (first, second) = (first.run(), second.run());
    j.complete_sync(second).unwrap();
    assert!(matches!(j.complete_sync(first), Err(JournalError::Io(_))));
    assert_eq!(durable_at(&j), 0);
    assert!(j.is_broken());
}

#[test]
fn a_result_from_another_writer_of_the_same_journal_id_is_ignored() {
    let (_, clock) = fake_clock();
    let here = FakeFs::with_dirs([session()]);
    let there = FakeFs::with_dirs([session()]);
    let mut mine = create(&here, 0, MIC, 0, Arc::clone(&clock)).unwrap();
    let mut theirs = create(&there, 0, MIC, 0, clock).unwrap();
    mine.append_within(&samples(MIC, 0, 100)).unwrap();
    theirs.append_within(&samples(MIC, 0, 100)).unwrap();
    let _pending = mine.begin_sync().unwrap();
    let done = theirs.begin_sync().unwrap().run();
    mine.complete_sync(done).unwrap();
    assert_eq!(durable_at(&mine), 0);
    assert!(mine.sync_in_flight());
}

#[test]
fn a_result_for_another_journal_is_ignored() {
    let fs = FakeFs::with_dirs([session()]);
    let (_, clock) = fake_clock();
    let mut mic = create(&fs, 0, MIC, 0, Arc::clone(&clock)).unwrap();
    let mut system = create(&fs, 1, SYSTEM, 0, clock).unwrap();
    mic.append_within(&samples(MIC, 0, 100)).unwrap();
    system.append_within(&samples(SYSTEM, 0, 100)).unwrap();
    let mic_job = mic.begin_sync().unwrap();
    let system_job = system.begin_sync().unwrap();
    mic.complete_sync(system_job.run()).unwrap();
    assert_eq!(durable_at(&mic), 0);
    assert!(mic.sync_in_flight());
    let lost = mic_job.lost();
    assert_eq!(lost.journal(), JournalId::new(0));
    system.complete_sync(lost).unwrap();
    assert!(!system.is_broken());
    // A lost sync breaks its own journal.
    let lost = mic.begin_sync().unwrap().lost();
    assert!(matches!(mic.complete_sync(lost), Err(JournalError::Io(_))));
    assert!(mic.is_broken());
}

#[test]
fn finishing_a_settled_journal_doesnt_fsync_again() {
    let fs = FakeFs::with_dirs([session()]);
    let (_, clock) = fake_clock();
    let mut j = create(&fs, 0, MIC, 0, clock).unwrap();
    assert!(j.is_settled());
    j.append_within(&samples(MIC, 0, 10)).unwrap();
    assert!(!j.is_settled());
    let job = j.begin_sync().unwrap();
    assert!(!j.is_settled(), "in flight");
    j.complete_sync(job.run()).unwrap();
    assert!(j.is_settled());
    let ops = fs.ops().len();
    assert_eq!(j.finish().unwrap().end().get(), 10);
    assert_eq!(fs.ops().len(), ops);
}

#[test]
fn an_inline_sync_or_finish_waits_its_turn_behind_a_started_one() {
    let fs = FakeFs::with_dirs([session()]);
    let (_, clock) = fake_clock();
    let mut j = create(&fs, 0, MIC, 0, clock).unwrap();
    j.append_within(&samples(MIC, 0, 100)).unwrap();
    let started = j.begin_sync().unwrap();
    j.append_within(&samples(MIC, 100, 100)).unwrap();
    assert!(matches!(j.sync(), Err(JournalError::SyncPending)));
    assert_eq!(durable_at(&j), 0);
    assert!(!j.is_broken());
    assert_eq!(
        JournalError::SyncPending.to_string(),
        "an earlier sync of the journal hasn't completed"
    );
    j.complete_sync(started.run()).unwrap();
    j.sync().unwrap();
    assert_eq!(durable_at(&j), 200);
    j.append_within(&samples(MIC, 200, 1)).unwrap();
    let _held = j.begin_sync().unwrap();
    assert!(matches!(j.finish(), Err(JournalError::SyncPending)));
}

#[test]
fn append_syncs_once_the_interval_has_passed_though_the_budget_has_room() {
    let fs = FakeFs::with_dirs([session()]);
    let (clock, dyn_clock) = fake_clock();
    let mut j = create(&fs, 0, MIC, 0, dyn_clock).unwrap();
    j.append(&samples(MIC, 0, 10)).unwrap();
    assert_eq!(durable_at(&j), 0);
    clock.advance(SYNC_INTERVAL);
    j.append(&samples(MIC, 10, 10)).unwrap();
    assert_eq!(durable_at(&j), 20);
}
