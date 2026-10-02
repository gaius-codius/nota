//! The journal's acceptance tests: crash after every operation, the lag
//! bound in a timed run, and torn final frames.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nota_core::{Clock, FakeClock, SampleCount, SampleIndex, SampleRate, SessionTime, TrackId};

use super::format::{FRAME_HEADER_LEN, HEADER_LEN, MAX_FRAME_SAMPLES, encode_frame, encode_header};
use super::*;
use crate::fs::crash::{CrashCase, CrashTest};
use crate::fs::fake::{CrashOutcome, FakeFs, Op};
use crate::fs::{Fs, FsFile, StdFs};
use crate::test_dir::TestDir;

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

fn journal_path() -> PathBuf {
    PathBuf::from("/session/journal")
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

/// What the recording scenario told its caller before it stopped.
#[derive(Debug, Default)]
struct Promised {
    /// Whether `JournalWriter::create` returned: the file and its header
    /// are then durable.
    created: bool,
    /// Per track: where the track started, and the last durable position
    /// the writer reported.
    started: BTreeMap<TrackId, SampleIndex>,
    durable: BTreeMap<TrackId, SampleIndex>,
    /// Per track: the end of everything handed to `append` successfully.
    captured: BTreeMap<TrackId, SampleIndex>,
}

/// Records two tracks in real time with a fake clock: chunks of uneven
/// size, one longer than a frame, about 3.5 s in all. Stops at the first
/// error, as the recorder would.
fn record(fs: &FakeFs) -> Promised {
    let mut promised = Promised::default();
    let _ = record_into(fs, &mut promised);
    promised
}

fn record_into(fs: &FakeFs, promised: &mut Promised) -> Result<(), JournalError> {
    let (clock, dyn_clock) = fake_clock();
    let mut journal = JournalWriter::create(fs, &journal_path(), SampleRate::SPEECH, dyn_clock)?;
    promised.created = true;
    let starts = [(MIC, 0_u64), (SYSTEM, 5_000)];
    for (track, at) in starts {
        journal.start_track(track, SampleIndex::new(at))?;
        promised.started.insert(track, SampleIndex::new(at));
        promised.captured.insert(track, SampleIndex::new(at));
    }
    let sizes = [1_600_u64, 800, 2_400, 10_000, 1_600, 320];
    let mut next = BTreeMap::from(starts);
    for step in 0..14 {
        let len = sizes[step % sizes.len()];
        for track in [MIC, SYSTEM] {
            let from = next[&track];
            let appended = journal.append(track, &samples(track, from, len));
            // The writer's own word, even after a failure: a long append may
            // have written some frames before it failed.
            if let Some(captured) = journal.captured(track) {
                promised.captured.insert(track, captured);
            }
            // A sync moves every track's durable position.
            for t in [MIC, SYSTEM] {
                if let Some(d) = journal.durable(t) {
                    promised.durable.insert(t, d.end());
                }
            }
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
    journal.sync()?;
    for track in [MIC, SYSTEM] {
        if let Some(d) = journal.durable(track) {
            promised.durable.insert(track, d.end());
        }
    }
    Ok(())
}

fn recover(fs: &FakeFs) -> Option<JournalRead> {
    match fs.read(&journal_path()) {
        Ok(bytes) => Some(read_journal(&bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        // The recovery fake never crashes in this test.
        Err(e) => panic!("recovery read failed: {e}"),
    }
}

/// The journal's crash invariants. A closure, to match `CrashTest`'s check
/// signature without clippy's by-reference lints.
const CHECK_RECOVERED: fn(&CrashCase, &Promised, &Option<JournalRead>) -> Result<(), String> =
    |case, promised, recovered| check_recovered(case, promised, recovered.as_ref());

fn check_recovered(
    case: &CrashCase,
    promised: &Promised,
    recovered: Option<&JournalRead>,
) -> Result<(), String> {
    let Some(read) = recovered else {
        // No file: fine only if nothing was promised.
        return if promised.durable.is_empty() && !promised.created {
            Ok(())
        } else {
            Err(format!(
                "journal lost after promising {:?}",
                promised.durable
            ))
        };
    };
    if !promised.created && read.header().is_none() {
        // The crash came before the header was synced.
        return Ok(());
    }
    if read.header().map(JournalHeader::rate) != Some(SampleRate::SPEECH) {
        return Err(format!("bad header, ended {:?}", read.end()));
    }
    for (&track, &start) in &promised.started {
        let audio = read.track_audio(track);
        let (start_found, end_found) = match &audio {
            Some((range, got)) => {
                let want = samples(track, range.start().get(), range.len().get());
                if *got != want {
                    return Err(format!("track {track:?}: recovered samples differ"));
                }
                (range.start(), range.end())
            }
            None => (start, start),
        };
        if start_found != start {
            return Err(format!(
                "track {track:?} recovered from {start_found:?}, started at {start:?}"
            ));
        }
        // Sample-continuous up to the last durable position...
        if let Some(&durable) = promised.durable.get(&track)
            && end_found < durable
        {
            return Err(format!(
                "track {track:?}: recovered to {end_found:?}, durable was {durable:?}"
            ));
        }
        // ...and never past what was captured.
        if end_found > promised.captured[&track] {
            return Err(format!("track {track:?}: recovered past the captured end"));
        }
        // Keeping everything recovers everything captured.
        if case.outcome == CrashOutcome::KeepAll && end_found != promised.captured[&track] {
            return Err(format!(
                "track {track:?}: recovered to {end_found:?} with everything kept, captured {:?}",
                promised.captured[&track]
            ));
        }
        // Losing everything unsynced loses exactly that.
        if case.outcome == CrashOutcome::LoseUnsynced {
            let durable = promised.durable.get(&track).copied().unwrap_or(start);
            if end_found != durable {
                return Err(format!(
                    "track {track:?}: recovered to {end_found:?} with nothing unsynced kept, durable {durable:?}"
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn crash_after_every_operation_recovers_to_the_durable_position() {
    let summary = CrashTest::new(record, recover, CHECK_RECOVERED)
        .dirs(["/session"])
        .run()
        .unwrap_or_else(|failure| panic!("{failure}"));
    // Not vacuous: dozens of writes and several syncs, each crashed at.
    assert!(summary.scenario_ops > 30, "{summary:?}");
    let clean = FakeFs::with_dirs(["/session"]);
    let promised = record(&clean);
    let syncs = clean
        .ops()
        .iter()
        .filter(|op| matches!(op, Op::Sync(_)))
        .count();
    assert!(syncs >= 4, "only {syncs} syncs");
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
            let mut file = fs.create(&journal_path())?;
            let mut bytes = encode_header(SampleRate::SPEECH).to_vec();
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
fn durable_stays_within_a_second_of_captured_in_a_timed_run() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (clock, dyn_clock) = fake_clock();
    let mut journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    let limit = SampleCount::new(17_600); // 1.1 s at 16 kHz
    let chunk = 160; // 10 ms, as a capture callback delivers it
    let mut worst = SampleCount::ZERO;
    for i in 0..6_000 {
        journal
            .append(MIC, &samples(MIC, i * chunk, chunk))
            .unwrap();
        clock.advance(Duration::from_millis(10));
        let captured = journal.captured(MIC).unwrap();
        let durable = journal
            .durable(MIC)
            .map_or(SampleIndex::ZERO, DurablePosition::end);
        let lag = captured.checked_count_since(durable).unwrap();
        worst = worst.max(lag);
        assert!(lag <= limit, "at chunk {i}: {lag:?} unsynced");
    }
    // A minute of audio, synced about once a second.
    let syncs = fs
        .ops()
        .iter()
        .filter(|op| matches!(op, Op::Sync(_)))
        .count();
    assert!((58..=62).contains(&syncs), "{syncs} syncs in 60 s");
    assert!(worst >= SampleCount::new(15_000), "never lagged: {worst:?}");
}

#[test]
fn a_burst_faster_than_real_time_still_syncs_each_second_of_audio() {
    // The clock stands still; the audio bound alone keeps durable close.
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    for i in 0..100 {
        journal
            .append(MIC, &samples(MIC, i * 1_000, 1_000))
            .unwrap();
        let lag = journal
            .captured(MIC)
            .unwrap()
            .checked_count_since(
                journal
                    .durable(MIC)
                    .map_or(SampleIndex::ZERO, DurablePosition::end),
            )
            .unwrap();
        assert!(lag < SampleCount::new(16_000), "at {i}: {lag:?}");
    }
}

#[test]
fn sync_if_due_syncs_a_stalled_track_after_the_interval() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (clock, dyn_clock) = fake_clock();
    let mut journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    journal.append(MIC, &samples(MIC, 0, 160)).unwrap();
    assert_eq!(journal.durable(MIC), None);
    clock.advance(Duration::from_millis(999));
    assert!(!journal.sync_if_due().unwrap());
    clock.advance(Duration::from_millis(1));
    assert!(journal.sync_if_due().unwrap());
    assert_eq!(
        journal.durable(MIC).map(DurablePosition::end),
        Some(SampleIndex::new(160))
    );
    assert_eq!(journal.durable(MIC).map(DurablePosition::track), Some(MIC));
    // Nothing new: no pointless fsync.
    clock.advance(Duration::from_secs(5));
    assert!(!journal.sync_if_due().unwrap());
}

#[test]
fn a_failed_write_breaks_the_journal() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    journal.append(MIC, &samples(MIC, 0, 10)).unwrap();
    fs.crash_after(0);
    assert!(matches!(
        journal.append(MIC, &samples(MIC, 10, 10)),
        Err(JournalError::Io(_))
    ));
    assert_eq!(journal.captured(MIC), Some(SampleIndex::new(10)));
    assert!(matches!(
        journal.append(MIC, &[1]),
        Err(JournalError::Broken)
    ));
    assert!(matches!(journal.sync(), Err(JournalError::Broken)));
    assert!(matches!(journal.sync_if_due(), Err(JournalError::Broken)));
    assert!(matches!(
        journal.start_track(SYSTEM, SampleIndex::ZERO),
        Err(JournalError::Broken)
    ));
}

#[test]
fn a_failed_fsync_breaks_the_journal_without_moving_durable() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = JournalWriter::create(
        &fs,
        &journal_path(),
        SampleRate::SPEECH,
        Arc::clone(&dyn_clock),
    )
    .unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    journal.append(MIC, &samples(MIC, 0, 10)).unwrap();
    journal.sync().unwrap();
    journal.append(MIC, &samples(MIC, 10, 10)).unwrap();
    // EIO from fsync, and the process lives on.
    fs.fail_after(0, io::ErrorKind::Other);
    assert!(matches!(journal.sync(), Err(JournalError::Io(_))));
    assert!(!fs.has_crashed());
    assert_eq!(
        journal.durable(MIC).map(DurablePosition::end),
        Some(SampleIndex::new(10))
    );
    // No retry: the kernel may have dropped the data, so another fsync
    // "succeeding" would claim audio that's gone.
    assert!(matches!(journal.sync(), Err(JournalError::Broken)));
    assert!(matches!(
        journal.append(MIC, &[1]),
        Err(JournalError::Broken)
    ));
    // What was durable is still there, and a new journal can start.
    let read = read_journal(&fs.read(&journal_path()).unwrap());
    assert_eq!(read.track_audio(MIC).unwrap().1, samples(MIC, 0, 10));
    let next = PathBuf::from("/session/journal-2");
    let mut journal = JournalWriter::create(&fs, &next, SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::new(10)).unwrap();
    journal.append(MIC, &samples(MIC, 10, 10)).unwrap();
    journal.sync().unwrap();
    assert_eq!(
        journal.durable(MIC).map(DurablePosition::end),
        Some(SampleIndex::new(20))
    );
}

#[test]
fn a_full_disk_breaks_the_journal_but_not_the_process() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    fs.fail_after(0, io::ErrorKind::StorageFull);
    assert!(matches!(
        journal.append(MIC, &samples(MIC, 0, 10)),
        Err(JournalError::Io(e)) if e.kind() == io::ErrorKind::StorageFull
    ));
    assert_eq!(journal.captured(MIC), Some(SampleIndex::ZERO));
    assert!(matches!(
        journal.append(MIC, &[1]),
        Err(JournalError::Broken)
    ));
    assert!(fs.read(&journal_path()).is_ok());
}

#[test]
fn a_long_append_never_leaves_more_than_a_second_unsynced() {
    // One append of ten seconds, crashed after every operation: whatever
    // the writer reports, captured is never more than a second ahead of
    // durable, and the durable part survives.
    let second = SampleCount::new(16_000);
    for crash_at in 0..60 {
        let fs = FakeFs::with_dirs(["/session"]);
        let (_clock, dyn_clock) = fake_clock();
        let mut journal =
            JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
        journal.start_track(MIC, SampleIndex::ZERO).unwrap();
        fs.crash_after(crash_at);
        let done = journal.append(MIC, &samples(MIC, 0, 160_000)).is_ok();
        let captured = journal.captured(MIC).unwrap();
        let durable = journal
            .durable(MIC)
            .map_or(SampleIndex::ZERO, DurablePosition::end);
        let lag = captured.checked_count_since(durable).unwrap();
        assert!(lag <= second, "crash at {crash_at}: {lag:?} unsynced");
        let after = fs.crash(CrashOutcome::LoseUnsynced);
        let read = read_journal(&after.read(&journal_path()).unwrap());
        let end = read
            .track_audio(MIC)
            .map_or(SampleIndex::ZERO, |(range, _)| range.end());
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
    let mut journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    assert!(matches!(journal.append(MIC, &[1]), Err(JournalError::UnknownTrack(t)) if t == MIC));
    journal
        .start_track(MIC, SampleIndex::new(u64::MAX - 1))
        .unwrap();
    assert!(matches!(
        journal.start_track(MIC, SampleIndex::ZERO),
        Err(JournalError::TrackExists(t)) if t == MIC
    ));
    assert!(
        matches!(journal.append(MIC, &[1, 2]), Err(JournalError::SampleOverflow(t)) if t == MIC)
    );
    journal.append(MIC, &[]).unwrap();
    journal.append(MIC, &[1]).unwrap();
    assert_eq!(journal.captured(MIC), Some(SampleIndex::new(u64::MAX)));
    let written = fs.read(&journal_path()).unwrap();
    assert_eq!(written.len(), HEADER_LEN + FRAME_HEADER_LEN + 2);
    assert_eq!(journal.rate(), SampleRate::SPEECH);
    // Only the good append made it in, and it reads back.
    let read = read_journal(&written);
    assert_eq!(read.frames().len(), 1);
    assert_eq!(read.end(), ReadEnd::Complete);
}

#[test]
fn create_refuses_an_existing_journal() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let _first = JournalWriter::create(
        &fs,
        &journal_path(),
        SampleRate::SPEECH,
        Arc::clone(&dyn_clock),
    )
    .unwrap();
    let again = JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock);
    assert!(matches!(again, Err(JournalError::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists));
}

#[test]
fn long_appends_split_into_frames() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::new(7)).unwrap();
    let max = u64::from(MAX_FRAME_SAMPLES);
    journal.append(MIC, &samples(MIC, 7, 2 * max + 1)).unwrap();
    let read = read_journal(&fs.read(&journal_path()).unwrap());
    let lens: Vec<_> = read
        .frames()
        .iter()
        .map(|f| f.range().len().get())
        .collect();
    // A full frame, then what's left of the first second (16 000 samples),
    // a sync, and the rest.
    assert_eq!(lens, [max, 16_000 - max, 2 * max + 1 - 16_000]);
    let syncs = fs
        .ops()
        .iter()
        .filter(|op| matches!(op, Op::Sync(_)))
        .count();
    assert_eq!(
        syncs, 2,
        "the header's sync and the one at a second of audio"
    );
    let (range, got) = read.track_audio(MIC).unwrap();
    assert_eq!(
        (range.start().get(), range.end().get()),
        (7, 7 + 2 * max + 1)
    );
    assert_eq!(got, samples(MIC, 7, 2 * max + 1));
    assert_eq!(read.track_audio(SYSTEM), None);
}

/// A synced journal of three frames, then a fourth written but not synced.
fn journal_with_unsynced_frame() -> (FakeFs, usize, Vec<u8>) {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    for i in 0..3 {
        journal.append(MIC, &samples(MIC, i * 100, 100)).unwrap();
    }
    journal.sync().unwrap();
    let synced_len = fs.read(&journal_path()).unwrap().len();
    journal.append(MIC, &samples(MIC, 300, 100)).unwrap();
    let full = fs.read(&journal_path()).unwrap();
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
        let read = read_journal(&after.read(&journal_path()).unwrap());
        let n = read.frames().len();
        assert!(n == 3 || n == 4, "seed {seed}: {n} frames");
        let (_, got) = read.track_audio(MIC).unwrap();
        assert_eq!(got, samples(MIC, 0, 100 * n as u64), "seed {seed}");
    }
}

#[test]
fn header_problems_are_reported() {
    let header = encode_header(SampleRate::SPEECH);
    assert_eq!(read_journal(&[]).end(), ReadEnd::Incomplete { offset: 0 });
    assert_eq!(read_journal(&header[..HEADER_LEN - 1]).header(), None);
    let ok = read_journal(&header);
    assert_eq!(
        ok.header().map(JournalHeader::rate),
        Some(SampleRate::SPEECH)
    );
    assert_eq!((ok.end(), ok.valid_len()), (ReadEnd::Complete, HEADER_LEN));
    for at in 0..HEADER_LEN {
        let mut bad = header;
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
    // A future version, or a zero rate, with a correct CRC.
    let mut version = encode_header(SampleRate::SPEECH);
    version[8] = 2;
    let mut zero_rate = encode_header(SampleRate::SPEECH);
    zero_rate[10..14].fill(0);
    for mut bad in [version, zero_rate] {
        let crc = crc32fast::hash(&bad[..14]);
        bad[14..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(read_journal(&bad).header(), None, "{bad:?}");
    }
}

/// Builds a header, then the frames given as (seq, track, first, samples).
fn build(frames: &[(u64, TrackId, u64, Vec<i16>)]) -> Vec<u8> {
    let mut bytes = encode_header(SampleRate::SPEECH).to_vec();
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
    // A second track may start anywhere; each track continues on its own.
    let two = build(&[(0, MIC, 0, a), (1, SYSTEM, 900, b.clone()), (2, MIC, 4, b)]);
    let read = read_journal(&two);
    assert_eq!((read.frames().len(), read.end()), (3, ReadEnd::Complete));
    assert_eq!(read.frames()[1].seq(), 1);
    assert_eq!(read.frames()[1].track(), SYSTEM);
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
    let path = dir.0.join("journal");
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = JournalWriter::create(&StdFs, &path, SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    journal.append(MIC, &samples(MIC, 0, 20_000)).unwrap();
    journal.sync().unwrap();
    let read = read_journal(&StdFs.read(&path).unwrap());
    assert_eq!(read.track_audio(MIC).unwrap().1, samples(MIC, 0, 20_000));
    assert_eq!(read.end(), ReadEnd::Complete);
}

#[test]
fn errors_describe_themselves() {
    let errors = [
        JournalError::Io(io::Error::other("disk")),
        JournalError::UnknownTrack(MIC),
        JournalError::TrackExists(MIC),
        JournalError::SampleOverflow(MIC),
        JournalError::Broken,
    ];
    for e in &errors {
        assert!(!e.to_string().is_empty());
    }
    assert!(std::error::Error::source(&errors[0]).is_some());
    assert!(std::error::Error::source(&errors[1]).is_none());
}

#[test]
fn create_needs_a_directory() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    assert!(JournalWriter::create(&fs, Path::new("/"), SampleRate::SPEECH, dyn_clock).is_err());
}

#[test]
fn a_failed_create_removes_its_file_so_a_retry_works() {
    // Fail each step after the file exists: the header write, its fsync,
    // the directory fsync.
    for step in 1..=3 {
        let fs = FakeFs::with_dirs(["/session"]);
        let (_clock, dyn_clock) = fake_clock();
        fs.fail_after(step, io::ErrorKind::StorageFull);
        let first = JournalWriter::create(
            &fs,
            &journal_path(),
            SampleRate::SPEECH,
            Arc::clone(&dyn_clock),
        );
        assert!(matches!(first, Err(JournalError::Io(_))), "step {step}");
        assert_eq!(fs.paths(), Vec::<PathBuf>::new(), "step {step}");
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    }
}

#[test]
fn finish_syncs_what_was_captured() {
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    journal.append(MIC, &samples(MIC, 0, 100)).unwrap();
    journal.finish().unwrap();
    let after = fs.crash(CrashOutcome::LoseUnsynced);
    let read = read_journal(&after.read(&journal_path()).unwrap());
    assert_eq!(read.track_audio(MIC).unwrap().1, samples(MIC, 0, 100));

    // Nothing new: no fsync. Broken: the error.
    let fs = FakeFs::with_dirs(["/session"]);
    let (_clock, dyn_clock) = fake_clock();
    let journal =
        JournalWriter::create(&fs, &journal_path(), SampleRate::SPEECH, dyn_clock).unwrap();
    let before = fs.ops().len();
    journal.finish().unwrap();
    assert_eq!(fs.ops().len(), before);
    let (_clock, dyn_clock) = fake_clock();
    let mut journal = JournalWriter::create(
        &fs,
        Path::new("/session/journal-2"),
        SampleRate::SPEECH,
        dyn_clock,
    )
    .unwrap();
    journal.start_track(MIC, SampleIndex::ZERO).unwrap();
    fs.fail_after(0, io::ErrorKind::StorageFull);
    assert!(journal.append(MIC, &[1]).is_err());
    assert!(matches!(journal.finish(), Err(JournalError::Broken)));
}
