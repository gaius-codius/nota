//! A full disk during a recording: the disk fills (the fake's capacity)
//! while two tracks record with live publishing, so `ENOSPC` lands in a
//! journal write, a FLAC publish or a row commit. The first such failure
//! frees the ballast, the recording stops, its open segments finish, and
//! the audio salvages; crashed after every operation, the loss bound holds
//! up to the failure.

use super::*;
use crate::disk::{Ballast, DiskWatch, Freed, Full, WatchedFs};

/// Where the ballast is kept: the data directory.
fn data() -> PathBuf {
    PathBuf::from("/data")
}

/// The ballast's size: more than finishing both tracks' open segments
/// needs at the test's rate (two journals and a FLAC window a track, about
/// 18 KB; see [`crate::disk::Usage::reserve`]).
const BALLAST: u64 = 32 * 1024;

/// Where an operation failed for want of space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Where {
    /// Writing a journal (or its replacement).
    Journal,
    /// Writing a segment's FLAC temp file.
    Flac,
    /// Committing a segment row (the fake store's temp file).
    Row,
    /// Writing the session's marks.
    Marks,
}

fn classify(path: &Path) -> Option<Where> {
    if is_journal(path) {
        Some(Where::Journal)
    } else if is_temp_segment(path) {
        Some(Where::Flac)
    } else if path.starts_with(db()) {
        Some(Where::Row)
    } else if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(MARKS_FILE_NAME))
    {
        Some(Where::Marks)
    } else {
        None
    }
}

/// How a recording onto a disk that fills went.
#[derive(Debug, Clone, Default)]
struct FullRun {
    promised: Promised,
    /// The disk filled, and how.
    full: Option<Full>,
    /// The first error the recording itself met: an append or a sync
    /// whose journal broke and couldn't be replaced, or finishing. None
    /// means no audio was lost to the full disk.
    error: Option<String>,
    /// Journals left unpublished once the recording stopped and its last
    /// segments were published (two tries, as the publisher makes).
    left: usize,
}

/// Records two tracks with live publishing onto a disk of `capacity` bytes
/// of file data, with a ballast of `ballast` bytes if it's `Some`, until
/// the disk fills (or [`ROUNDS`] rounds run out); then stops as `nota
/// record` does on a full disk: finishes the writer and publishes the last
/// journals.
fn record_until_full(fs: &FakeFs, capacity: u64, ballast: Option<u64>) -> FullRun {
    record_rounds(fs, capacity, ballast, ROUNDS)
}

/// More rounds of audio than any capacity here lasts.
const ROUNDS: usize = 200;

/// [`record_until_full`], for at most `rounds` rounds.
fn record_rounds(fs: &FakeFs, capacity: u64, ballast: Option<u64>, rounds: usize) -> FullRun {
    let mut run = FullRun::default();
    let watch = DiskWatch::new(fs.clone());
    if let Err(e) = record_until_full_into(fs, &watch, capacity, ballast, rounds, &mut run) {
        run.error.get_or_insert(e.to_string());
    }
    run.full = watch.full();
    run
}

fn record_until_full_into(
    fs: &FakeFs,
    watch: &Arc<DiskWatch<FakeFs>>,
    capacity: u64,
    ballast: Option<u64>,
    rounds: usize,
    run: &mut FullRun,
) -> Result<(), Box<dyn Error>> {
    if let Some(len) = ballast
        && let Some(ballast) = Ballast::keep(fs, &data(), len, || false)?
    {
        watch.hold(ballast);
    }
    fs.set_capacity(Some(capacity));
    let disk: WatchedFs<FakeFs> = watch.fs();
    let (clock, dyn_clock) = fake_clock();
    let lock = SessionDir::new(SESSION, disk.clone(), &session()).lock()?;
    let mut writer = SessionWriter::open(&lock, rate(), length(), dyn_clock)?;
    let mut store = SessionStore::new(lock, FakeStore::new(&disk, &db()));
    let promised = &mut run.promised;
    for (track, at) in [(MIC, 0_u64), (SYSTEM, 700)] {
        writer.start_track(track, EpochId::new(0), SampleIndex::new(at))?;
        promised.started.insert(track, SampleIndex::new(at));
        promised.durable.insert(track, SampleIndex::new(at));
        promised.captured.insert(track, SampleIndex::new(at));
    }
    let sizes = [250_u64, 100, 400, 1_600, 50];
    let mut pending: Vec<FinishedJournal> = Vec::new();
    // As the publisher does: a failed run keeps the journals still on disk
    // for the next try.
    let mut publish = |pending: &mut Vec<FinishedJournal>, promised: &mut Promised| {
        if pending.is_empty() {
            return;
        }
        if let Ok(done) = publish_journals(&mut store, length(), pending) {
            promised.rows.extend_from_slice(done.segments());
        }
        if let Ok(there) = fs.list(&session()) {
            pending.retain(|j| there.contains(&session().join(j.id().file_name())));
        }
    };
    for step in 0..rounds {
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
        pending.extend(writer.take_finished());
        publish(&mut pending, promised);
        // The monitor reports the full disk, and `nota record` stops.
        if watch.full().is_some() {
            break;
        }
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
    publish(&mut pending, promised);
    publish(&mut pending, promised);
    run.left = pending.len();
    Ok(())
}

/// What the first failure for want of space hit, in a run.
fn hit(run: &FullRun) -> Option<Where> {
    run.full.as_ref()?.path.as_deref().and_then(classify)
}

/// Where a scan found the disk first filling in each kind of write, past
/// the ballast: tried first, so each test needn't scan.
const HINTS: [(Where, u64); 3] = [
    (Where::Journal, 20_195),
    (Where::Flac, 20_000),
    (Where::Row, 20_094),
];

/// A capacity at which the disk first fills in a `kind` write: its hint.
/// If the hint no longer does (the write path changed), this scans for one
/// and fails, naming it, so the hint is updated rather than scanned for on
/// every run.
fn capacity_for(kind: Where) -> u64 {
    let fills_in = |capacity: u64| {
        let fs = FakeFs::with_dirs([session(), db(), data()]);
        hit(&record_until_full(&fs, capacity, Some(BALLAST))) == Some(kind)
    };
    let hint = HINTS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or(0, |&(_, past)| BALLAST + past);
    if fills_in(hint) {
        return hint;
    }
    match (BALLAST + 20_000..BALLAST + 30_000).find(|&capacity| fills_in(capacity)) {
        Some(found) => panic!(
            "the hint for {kind:?} is stale: update it in HINTS to {}",
            found - BALLAST
        ),
        None => panic!("no capacity fills the disk in a {kind:?} write"),
    }
}

/// Recovery once there's room again: the user freed some, or the ballast's
/// room is there.
fn recover_with_room(fs: &FakeFs) -> Recovered {
    fs.set_capacity(None);
    recover(fs)
}

/// [`check`], on a full run's promises.
fn check_full(case: &CrashCase, run: &FullRun, got: &Recovered) -> Result<(), String> {
    check(case, &run.promised, got)
}

/// A clean run onto a disk that fills in a `kind` write: the warning (the
/// full disk, with the ballast freed), no audio lost, every segment
/// finished and published, nothing left for salvage.
fn clean_full_run(kind: Where) -> (FakeFs, FullRun) {
    let fs = FakeFs::with_dirs([session(), db(), data()]);
    let run = record_until_full(&fs, capacity_for(kind), Some(BALLAST));
    assert_eq!(hit(&run), Some(kind), "{run:?}");
    assert_eq!(run.full.as_ref().map(|f| f.ballast), Some(Freed::Freed));
    assert_eq!(run.error, None, "audio was lost: {run:?}");
    assert_eq!(run.left, 0, "a segment wasn't finished: {run:?}");
    assert!(
        !fs.paths()
            .contains(&data().join(crate::disk::ballast_file_name(BALLAST)))
    );
    assert!(
        !fs.paths().iter().any(|p| is_journal(p)),
        "{:?}",
        fs.paths()
    );
    // Everything captured is in a published segment, uncrashed.
    let case = CrashCase {
        after_ops: fs.attempted(),
        ops: fs.ops(),
        outcome: CrashOutcome::KeepAll,
        recovery_crashes: Vec::new(),
        survived: fs.copy_disk(),
        fs: fs.copy_disk(),
    };
    let after = observe(&fs);
    check_after(&run.promised, &after).unwrap_or_else(|e| panic!("{kind:?}: {e}"));
    check_full(&case, &run, &recover_with_room(&fs.copy_disk())).unwrap();
    assert!(run.promised.rows.len() >= 4, "{:?}", run.promised.rows);
    (fs, run)
}

#[test]
fn a_full_disk_in_a_journal_write_frees_the_ballast_and_finishes_the_segment() {
    clean_full_run(Where::Journal);
}

#[test]
fn a_full_disk_in_a_flac_publish_frees_the_ballast_and_finishes_the_segment() {
    clean_full_run(Where::Flac);
}

#[test]
fn a_full_disk_in_a_row_commit_frees_the_ballast_and_finishes_the_segment() {
    clean_full_run(Where::Row);
}

/// Without the ballast, the same disk loses audio or leaves a segment
/// unfinished: the ballast is what finishes it.
#[test]
fn without_a_ballast_the_same_full_disk_leaves_the_recording_unfinished() {
    for kind in [Where::Journal, Where::Flac, Where::Row] {
        let capacity = capacity_for(kind) - BALLAST;
        let fs = FakeFs::with_dirs([session(), db(), data()]);
        let run = record_until_full(&fs, capacity, None);
        assert_eq!(run.full.as_ref().map(|f| f.ballast), Some(Freed::None));
        assert!(
            run.error.is_some() || run.left > 0,
            "{kind:?}: the ballast made no difference: {run:?}"
        );
        // Still, nothing promised is lost: what wasn't published waits in
        // its journals for a start with room.
        let survivor = fs.crash(CrashOutcome::LoseUnsynced);
        let case = CrashCase {
            after_ops: fs.attempted(),
            ops: fs.ops(),
            outcome: CrashOutcome::LoseUnsynced,
            recovery_crashes: Vec::new(),
            survived: survivor.copy_disk(),
            fs: survivor.clone(),
        };
        check_full(&case, &run, &recover_with_room(&survivor))
            .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
    }
}

/// A full disk in each kind of write, crashed after every operation of the
/// recording (making the ballast included), losing everything unsynced,
/// keeping it all, and with `partial`, keeping some of it, which tears the
/// short write that met the full disk: the loss bound holds up to the
/// failure, and salvage with room ends with every durable sample in a row.
fn crash_swept(kind: Where, partial: bool) {
    let capacity = capacity_for(kind);
    clean_full_run(kind);
    let summary = CrashTest::new(
        move |fs: &FakeFs| record_until_full(fs, capacity, Some(BALLAST)),
        recover_with_room,
        check_full,
    )
    .dirs([session(), db(), data()])
    .outcomes(if partial {
        vec![
            CrashOutcome::LoseUnsynced,
            CrashOutcome::KeepAll,
            CrashOutcome::Partial { seed: 5 },
        ]
    } else {
        vec![CrashOutcome::LoseUnsynced, CrashOutcome::KeepAll]
    })
    .run()
    .unwrap_or_else(|failure| panic!("{kind:?}: {failure}"));
    assert!(summary.scenario_ops > 100, "{summary:?}");
}

#[test]
fn a_full_disk_in_a_journal_write_crashed_anywhere_loses_nothing_promised() {
    // The write that meets the full disk is a journal's: torn too.
    crash_swept(Where::Journal, true);
}

#[test]
fn a_full_disk_in_a_flac_publish_crashed_anywhere_loses_nothing_promised() {
    crash_swept(Where::Flac, false);
}

#[test]
fn a_full_disk_in_a_row_commit_crashed_anywhere_loses_nothing_promised() {
    crash_swept(Where::Row, false);
}

/// `ENOSPC` injected once at every operation of a recording, with the
/// ballast held: the first such failure frees it and the recording stops
/// and finishes, whatever the operation, and nothing promised is lost.
#[test]
fn enospc_at_any_operation_frees_the_ballast_and_loses_nothing_promised() {
    let capacity = u64::MAX / 2;
    // Enough rounds for a few segments a track, published live.
    let rounds = 9;
    let clean = FakeFs::with_dirs([session(), db(), data()]);
    let whole = record_rounds(&clean, capacity, Some(BALLAST), rounds);
    assert!(whole.promised.rows.len() >= 4, "{:?}", whole.promised.rows);
    assert_eq!(whole.full, None);
    let ops = clean.attempted();
    let mut kinds = BTreeSet::new();
    // After the ballast is made: its own failures are another test's.
    let made = {
        let probe = FakeFs::with_dirs([session(), db(), data()]);
        Ballast::keep(&probe, &data(), BALLAST, || false).unwrap();
        probe.attempted()
    };
    for at in made..ops {
        let fs = FakeFs::with_dirs([session(), db(), data()]);
        fs.fail_after(at, io::ErrorKind::StorageFull);
        let run = record_rounds(&fs, capacity, Some(BALLAST), rounds);
        if let Some(full) = &run.full {
            assert_eq!(full.ballast, Freed::Freed, "op {at}");
            kinds.extend(hit(&run));
            assert_eq!(run.left, 0, "op {at}: {run:?}");
            // The retry found the ballast's room: no audio lost.
            assert_eq!(run.error, None, "op {at}: {run:?}");
        }
        for outcome in [CrashOutcome::LoseUnsynced, CrashOutcome::KeepAll] {
            let crashed = fs.crash(outcome);
            let case = CrashCase {
                after_ops: fs.attempted(),
                ops: fs.ops(),
                outcome,
                recovery_crashes: Vec::new(),
                survived: crashed.copy_disk(),
                fs: crashed.clone(),
            };
            check_full(&case, &run, &recover_with_room(&crashed))
                .unwrap_or_else(|e| panic!("op {at}, {outcome:?}: {e}"));
        }
    }
    for kind in [Where::Journal, Where::Flac, Where::Row, Where::Marks] {
        assert!(kinds.contains(&kind), "{kinds:?}");
    }
}

/// Whether an old stopped session has a reserve to free at startup.
#[derive(Debug, Clone, Copy)]
enum StartupBallast {
    /// A reserve left by the previous recording.
    Existing,
    /// No reserve was available to the previous recording.
    Missing,
}

/// An old stopped recording on a disk with no free bytes.
struct StartupDisk {
    /// Synced journals and, when requested, the existing reserve.
    fs: FakeFs,
    /// The audio the stopped recording had synced.
    promised: Promised,
}

/// Finishes a recording without publishing, then fills the remaining space.
fn stopped_full_disk(ballast: StartupBallast) -> StartupDisk {
    let fs = FakeFs::with_dirs([session(), db(), data()]);
    let promised = record(
        &fs,
        Recording {
            steps: 4,
            publish: false,
            fail_at: None,
        },
    );
    if let StartupBallast::Existing = ballast {
        assert!(
            Ballast::keep(&fs, &data(), BALLAST, || false)
                .unwrap()
                .is_some()
        );
    }
    // Copy what reached the disk before trying each recovery point.
    let fs = fs.copy_disk();
    let used = fs
        .paths()
        .iter()
        .map(|p| fs.read(p).unwrap().len() as u64)
        .sum();
    fs.set_capacity(Some(used));
    assert_eq!(fs.free_space(&data()).unwrap(), 0);
    StartupDisk {
        fs: fs.copy_disk(),
        promised,
    }
}

/// The outcome of a startup attempt, including a caught full disk.
struct StartupRun {
    /// Published rows, or the error that stopped the attempt.
    published: Result<Published, Box<dyn Error>>,
    /// The watch's first full-disk report.
    full: Option<Full>,
}

/// Uses only the reserve surviving on disk, as a fresh startup does.
fn startup_salvage(fs: &FakeFs) -> StartupRun {
    let watch = DiskWatch::new(fs.clone());
    let published = startup_salvage_into(fs, &watch);
    StartupRun {
        published,
        full: watch.full(),
    }
}

/// Salvages through the same watched filesystem for audio and row writes.
fn startup_salvage_into(
    fs: &FakeFs,
    watch: &Arc<DiskWatch<FakeFs>>,
) -> Result<Published, Box<dyn Error>> {
    if let Some(ballast) = Ballast::find(fs, &data(), BALLAST)? {
        watch.hold(ballast);
    }
    let disk = watch.fs();
    let lock = SessionDir::new(SESSION, disk.clone(), &session()).lock()?;
    let mut store = SessionStore::new(lock, FakeStore::new(&disk, &db()));
    Ok(salvage_start(&mut store, length(), watch)?)
}

/// Startup uses the old reserve's room to publish every stopped journal.
#[test]
fn startup_salvage_on_a_full_disk_frees_existing_ballast_and_publishes_all_audio() {
    let old = stopped_full_disk(StartupBallast::Existing);
    // The initial write meets the actual capacity limit, freeing the reserve.
    let run = startup_salvage(&old.fs);
    let done = run.published.unwrap();
    assert_eq!(
        run.full.as_ref().map(|full| full.ballast),
        Some(Freed::Freed)
    );
    assert!(!done.segments().is_empty());
    assert!(
        !old.fs
            .paths()
            .contains(&data().join(crate::disk::ballast_file_name(BALLAST)))
    );
    // Verify decoded samples, row hashes and removal of every source journal.
    check_after(&old.promised, &observe(&old.fs)).unwrap();
}

/// With no reserve, startup preserves the stopped recording's journals.
#[test]
fn startup_salvage_on_a_full_disk_without_ballast_keeps_journals() {
    let old = stopped_full_disk(StartupBallast::Missing);
    let before = observe(&old.fs);
    let journals: BTreeMap<_, _> = before
        .files
        .into_iter()
        .filter(|(p, _)| is_journal(p))
        .collect();
    assert!(!journals.is_empty());
    // A failed publish must keep the journals holding the stopped audio.
    let run = startup_salvage(&old.fs);
    assert!(run.published.is_err());
    assert_eq!(
        run.full.as_ref().map(|full| full.ballast),
        Some(Freed::None)
    );
    let after = observe(&old.fs);
    for (path, bytes) in journals {
        assert_eq!(after.files.get(&path), Some(&bytes));
    }
    check_durable(&old.promised, &journal_samples(&after).unwrap()).unwrap();
}

/// Every startup crash can recover with the same remaining capacity.
#[test]
fn startup_salvage_on_a_full_disk_crashed_anywhere_keeps_all_durable_audio() {
    let old = stopped_full_disk(StartupBallast::Existing);
    let probe = old.fs.copy_disk();
    startup_salvage(&probe).published.unwrap();
    let operations = probe.attempted();
    assert!(operations > 30, "{operations}");
    let settled = observe(&probe);
    check_after(&old.promised, &settled).unwrap();
    // Crash both the first attempt and its reserve-backed retry at each operation.
    for after in 0..=operations {
        for outcome in CrashOutcome::standard() {
            check_startup_crash(&old, &settled, after, outcome);
        }
    }
}

/// Recovers a particular crash without raising the disk's capacity.
fn check_startup_crash(old: &StartupDisk, settled: &Observed, after: usize, outcome: CrashOutcome) {
    let run = old.fs.copy_disk();
    run.crash_after(after);
    let _ = startup_salvage(&run);
    let survived = run.crash(outcome);
    // A fresh watch holds any reserve whose unlink was lost in the crash.
    startup_salvage(&survived)
        .published
        .unwrap_or_else(|error| panic!("after {after} operations, {outcome:?}: {error}"));
    let recovered = observe(&survived);
    check_after(&old.promised, &recovered)
        .unwrap_or_else(|error| panic!("after {after} operations, {outcome:?}: {error}"));
    assert_eq!(&recovered, settled, "after {after} operations, {outcome:?}");
    // Repeated startup is stable once the journal recovery is complete.
    startup_salvage(&survived).published.unwrap();
    assert_eq!(observe(&survived), recovered);
}

/// A later session's successful salvage keeps its report after an earlier
/// session already freed the startup ballast.
#[test]
fn successful_startup_salvage_after_freeing_ballast_keeps_its_rows() {
    let old = stopped_full_disk(StartupBallast::Existing);
    let watch = DiskWatch::new(old.fs.clone());
    let ballast = Ballast::find(&old.fs, &data(), BALLAST).unwrap().unwrap();
    watch.hold(ballast);
    // Another session's recovery already met the full disk.
    watch.note_full(None);
    let done = startup_salvage_into(&old.fs, &watch).unwrap();
    assert!(!done.segments().is_empty());
    check_after(&old.promised, &observe(&old.fs)).unwrap();
}
