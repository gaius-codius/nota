//! Proving rows by their decoded audio, repairing rows whose file is
//! missing or mismatched (crashed after every operation), the integrity
//! scan, and a row that doesn't parse.

use nota_store::{AudioDigest, RowKey, Status};

use super::*;
use crate::segment::repair::MISMATCHED;

/// Commits `row` on `disk` with no file behind it.
fn plant(disk: &FakeFs, row: &SegmentRow) {
    FakeStore::new(disk, &db()).plant(SESSION, row);
}

/// Audio that's no row's: what a mismatched file holds.
fn junk() -> Vec<u8> {
    flac::encode(rate(), &[&[3; 64]]).unwrap()
}

/// Where a file under `row`'s name is first kept aside.
fn aside_of(row: &SegmentRow) -> PathBuf {
    let mut name = durable_path(row.track(), row.range()).into_os_string();
    name.push(MISMATCHED);
    PathBuf::from(name)
}

/// The files in the session directory nota kept aside for mismatching.
fn kept_aside(fs: &FakeFs) -> Vec<(PathBuf, Vec<u8>)> {
    fs.paths()
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(MISMATCHED))
        })
        .map(|p| {
            let bytes = fs.read(&p).unwrap();
            (p, bytes)
        })
        .collect()
}

/// `row`'s findings, as (problem, status).
fn findings_of(fs: &FakeFs, row: &SegmentRow) -> Vec<(Problem, Status)> {
    read_findings(&session_dir(fs))
        .unwrap()
        .found()
        .iter()
        .filter(|f| f.row() == row)
        .map(|f| (f.problem(), f.status()))
        .collect()
}

fn journals_left(fs: &FakeFs) -> usize {
    fs.paths().iter().filter(|p| is_journal(p)).count()
}

/// A recording's journals with `row`, one of the rows salvaging them
/// commits, already committed, and what an uninterrupted salvage of the
/// recording alone leaves.
fn with_a_committed_row(pick: usize) -> (FakeFs, Promised, Vec<SegmentRow>, SegmentRow, Observed) {
    let (disk, promised, rows) = unsalvaged();
    let clean = disk.copy_disk();
    salvage(&mut session_store(&clean), length()).unwrap();
    let row = rows[pick];
    plant(&disk, &row);
    (disk, promised, rows, row, observe(&clean))
}

/// What's on a disk but its findings file, which records how it got there.
fn without_findings(mut seen: Observed) -> Observed {
    seen.files.remove(&session().join(FINDINGS_FILE_NAME));
    seen
}

#[test]
fn a_missing_rows_file_is_rebuilt_from_its_journals() {
    let (disk, _, rows, row, clean) = with_a_committed_row(1);
    let done = salvage(&mut session_store(&disk), length()).unwrap();
    assert_eq!(done.repaired(), [(row, None)]);
    assert!(done.findings().is_empty() && done.not_repaired().is_empty());
    // The rest published as ever; the row itself wasn't committed again.
    assert!(!done.segments().contains(&row));
    assert_eq!(done.segments().len(), rows.len() - 1);
    // The disk is as if nothing had been missing: the same file, byte for
    // byte (the encoder is deterministic), and no journals.
    assert_eq!(without_findings(observe(&disk)), without_findings(clean));
    assert_eq!(journals_left(&disk), 0);
    assert_eq!(
        findings_of(&disk, &row),
        [(Problem::Missing, Status::Repaired)]
    );
    assert_eq!(read_findings(&session_dir(&disk)).unwrap().unresolved(), 0);
    // Salvage again: nothing to do, nothing changes.
    let before = observe(&disk);
    let again = salvage(&mut session_store(&disk), length()).unwrap();
    assert!(again.repaired().is_empty() && again.segments().is_empty());
    assert!(
        observe(&disk) == before,
        "a second salvage changed the disk"
    );
}

#[test]
fn a_mismatched_file_is_kept_aside_then_its_row_rebuilt() {
    let (disk, _, _, row, clean) = with_a_committed_row(2);
    plant_file(&disk, &durable_path(row.track(), row.range()), &junk());
    // Something under the first aside name already: it's never replaced.
    let taken = aside_of(&row);
    plant_file(&disk, &taken, b"kept before");
    let done = salvage(&mut session_store(&disk), length()).unwrap();
    let mut second = aside_of(&row).into_os_string();
    second.push(".1");
    let second = PathBuf::from(second);
    assert_eq!(done.repaired(), [(row, Some(second.clone()))]);
    assert_eq!(
        kept_aside(&disk),
        [(taken, b"kept before".to_vec()), (second, junk())]
    );
    let mut want = without_findings(clean);
    want.files.extend(kept_aside(&disk));
    assert_eq!(without_findings(observe(&disk)), want);
    assert_eq!(
        findings_of(&disk, &row),
        [(Problem::HashMismatch, Status::Repaired)]
    );
}

#[test]
fn a_row_from_before_audio_digests_is_repaired_by_its_hash() {
    let (disk, _, rows) = unsalvaged();
    let row = rows[0];
    let legacy = SegmentRow::new(row.track(), row.epoch(), row.range(), row.sha256()).unwrap();
    assert_eq!(legacy.audio(), None);
    plant(&disk, &legacy);
    let done = salvage(&mut session_store(&disk), length()).unwrap();
    assert_eq!(done.repaired(), [(legacy, None)]);
    assert_eq!(journals_left(&disk), 0);
    // Its file is the one publishing writes, so its hash proves it.
    assert_eq!(
        Sha256::digest(disk.read(&durable_path(row.track(), row.range())).unwrap()).as_slice(),
        row.sha256().as_bytes()
    );
}

#[test]
fn without_proof_nothing_is_repaired_or_moved() {
    let (disk, _, rows) = unsalvaged();
    let row = rows[1];
    // The journals hold this range, but not this audio: another digest.
    let other = row.with_audio(AudioDigest::new([1; 32]));
    plant(&disk, &other);
    let path = durable_path(row.track(), row.range());
    plant_file(&disk, &path, &junk());
    // A legacy row whose hash isn't what the journals rebuild.
    let legacy_row = rows[2];
    let legacy = SegmentRow::new(
        legacy_row.track(),
        legacy_row.epoch(),
        legacy_row.range(),
        nota_store::Sha256Digest::new([2; 32]),
    )
    .unwrap();
    plant(&disk, &legacy);
    let journals = journals_left(&disk);
    let done = salvage(&mut session_store(&disk), length()).unwrap();
    assert!(done.repaired().is_empty());
    let mut got = as_found(done.findings());
    got.sort_by_key(|(r, _)| (r.track(), r.range().start()));
    let mut want = vec![(other, Problem::HashMismatch), (legacy, Problem::Missing)];
    want.sort_by_key(|(r, _)| (r.track(), r.range().start()));
    assert_eq!(got, want);
    assert_eq!(disk.read(&path).unwrap(), junk());
    assert!(kept_aside(&disk).is_empty());
    assert!(journals_left(&disk) > 0 && journals_left(&disk) <= journals);
    assert_eq!(
        findings_of(&disk, &other),
        [(Problem::HashMismatch, Status::Unresolved)]
    );
}

#[test]
fn an_unreadable_file_is_never_repaired_over() {
    let (disk, _, _, row, _) = with_a_committed_row(1);
    let path = durable_path(row.track(), row.range());
    plant_file(&disk, &path, &junk());
    disk.fail_on(&path, Fault::Read, io::ErrorKind::PermissionDenied);
    let done = salvage(&mut session_store(&disk), length()).unwrap();
    assert!(done.repaired().is_empty());
    assert_eq!(
        as_found(done.findings()),
        [(row, Problem::Unreadable(ReadFailure::PermissionDenied))]
    );
    assert!(kept_aside(&disk).is_empty());
    assert!(disk.paths().contains(&path));
}

#[test]
fn a_mismatched_file_that_cant_be_kept_aside_stays_and_is_reported() {
    let (disk, _, _, row, _) = with_a_committed_row(1);
    let path = durable_path(row.track(), row.range());
    plant_file(&disk, &path, &junk());
    disk.fail_on(&path, Fault::Rename, io::ErrorKind::PermissionDenied);
    let done = salvage(&mut session_store(&disk), length()).unwrap();
    assert!(done.repaired().is_empty());
    assert_eq!(
        done.not_repaired(),
        [(row, io::ErrorKind::PermissionDenied)]
    );
    assert_eq!(as_found(done.findings()), [(row, Problem::HashMismatch)]);
    assert_eq!(disk.read(&path).unwrap(), junk());
    assert!(kept_aside(&disk).is_empty());
}

/// Salvage of `disk` crashed after every operation, under every standard
/// outcome, then run again: no file is ever lost (`junk`, if given, is
/// always somewhere in the session directory, and every sample promised
/// durable is in a row or a journal), and the rerun ends as an
/// uninterrupted salvage does, but for how the findings file says the row
/// was put right.
fn sweep_repair(disk: &FakeFs, promised: &Promised, row: &SegmentRow, junk: Option<&[u8]>) {
    let path = durable_path(row.track(), row.range());
    let probe = disk.copy_disk();
    let done = salvage(&mut session_store(&probe), length()).unwrap();
    assert_eq!(done.repaired().len(), 1);
    // Salvage's own operations: reading the result back adds more, which
    // the sweep would crash after salvage had finished.
    let ops = probe.attempted();
    let settled = without_findings(observe(&probe));
    assert!(ops > 30, "{ops}"); // check-bound
    let mut repaired = 0;
    let mut sweep = Sweep::new();
    for after in 0..=ops {
        for crash in CrashOutcome::standard() {
            let case = format!("after {after} ops, {crash:?}");
            let run = disk.copy_disk();
            run.crash_after(after);
            let _ = salvage(&mut session_store(&run), length());
            sweep.crash_point(&run);
            let survived = run.crash(crash);
            if let Some(junk) = junk {
                let there = survived
                    .paths()
                    .iter()
                    .any(|p| survived.read(p).is_ok_and(|b| b == junk));
                assert!(there, "{case}: the mismatched file is gone");
            }
            // The row holds its samples only once its file proves it.
            let mut seen = observe(&survived);
            if seen.files.get(&path) != settled.files.get(&path) {
                seen.rows = seen
                    .rows
                    .map(|rows| rows.into_iter().filter(|r| r != row).collect());
            }
            let held = row_samples(&seen)
                .and_then(|rows| Ok(rows.union(&journal_samples(&seen)?)))
                .unwrap_or_else(|e| panic!("{case}: {e}"));
            check_durable(promised, &held).unwrap_or_else(|e| panic!("{case}: {e}"));
            salvage(&mut session_store(&survived), length())
                .unwrap_or_else(|e| panic!("{case}: {e}"));
            assert!(
                without_findings(observe(&survived)) == settled,
                "{case}: ended differently: {}",
                differences(&without_findings(observe(&survived)), &settled)
            );
            let statuses: Vec<Status> = findings_of(&survived, row)
                .into_iter()
                .map(|(_, s)| s)
                .collect();
            // Repaired, unless a crash came after the repair and before it
            // was recorded: then the next run finds the row proven by its
            // file, since verified. A crash after the file was kept aside
            // leaves the row's file missing, which the next run finds and
            // repairs too: two findings. None at all if the crash came
            // before the first was recorded and the repair after it.
            if statuses.contains(&Status::Repaired) {
                repaired += 1;
            }
            let fine = !statuses.contains(&Status::Unresolved);
            assert!(fine, "{case}: {statuses:?}");
        }
    }
    assert!(repaired > 0); // check-bound
    // Not vacuous: the crash cut salvage short at every point but the last,
    // under every outcome.
    sweep.interrupted_at_least(ops * CrashOutcome::standard().len()); // check-bound
}

#[test]
fn a_missing_files_repair_crashed_at_every_operation_loses_nothing() {
    let (disk, promised, _, row, _) = with_a_committed_row(1);
    sweep_repair(&disk, &promised, &row, None);
}

#[test]
fn a_mismatched_files_repair_crashed_at_every_operation_never_loses_the_file() {
    let (disk, promised, _, row, _) = with_a_committed_row(2);
    plant_file(&disk, &durable_path(row.track(), row.range()), &junk());
    sweep_repair(&disk, &promised, &row, Some(&junk()));
}

#[test]
fn a_re_encoded_file_with_another_hash_still_proves_its_row() {
    let (disk, _, rows) = unsalvaged();
    salvage(&mut session_store(&disk), length()).unwrap();
    let row = rows[0];
    let path = durable_path(row.track(), row.range());
    let (hz, audio) = decode_flac(&disk.read(&path).unwrap()).unwrap();
    assert_eq!(hz, rate().hz());
    let other = flac::encode_otherwise(rate(), &audio, 256);
    assert_ne!(Sha256::digest(&other).as_slice(), row.sha256().as_bytes());
    disk.remove(&path).unwrap();
    plant_file(&disk, &path, &other);
    let found = scan(&mut session_store(&disk), Depth::Contents).unwrap();
    assert_eq!(found, Integrity::default());
    // The same, with one sample changed: it doesn't.
    let mut changed = audio;
    changed[0] = changed[0].wrapping_add(1);
    disk.remove(&path).unwrap();
    plant_file(&disk, &path, &flac::encode_otherwise(rate(), &changed, 256));
    let found = scan(&mut session_store(&disk), Depth::Contents).unwrap();
    assert_eq!(
        as_found(found.needs_attention()),
        [(row, Problem::HashMismatch)]
    );
}

#[test]
fn the_integrity_scan_tells_rows_to_publish_from_rows_that_need_the_user() {
    // Fully salvaged: no journals left.
    let (disk, _, rows) = unsalvaged();
    salvage(&mut session_store(&disk), length()).unwrap();
    assert_eq!(journals_left(&disk), 0);
    let clean = scan(&mut session_store(&disk), Depth::Contents).unwrap();
    assert_eq!(clean, Integrity::default());

    // One row's file gone, another's replaced: no journal holds either.
    let gone = rows[0];
    let gone_path = durable_path(gone.track(), gone.range());
    let kept = disk.read(&gone_path).unwrap();
    disk.remove(&gone_path).unwrap();
    disk.sync_dir(&session()).unwrap();
    let swapped = rows[1];
    let swapped_path = durable_path(swapped.track(), swapped.range());
    disk.remove(&swapped_path).unwrap();
    plant_file(&disk, &swapped_path, &junk());

    // By names, only the missing file shows, and needs the user.
    let names = scan(&mut session_store(&disk), Depth::Names).unwrap();
    assert_eq!(
        as_found(names.needs_attention()),
        [(gone, Problem::Missing)]
    );
    assert!(names.needs_publishing().is_empty());
    // By contents, the replaced one too.
    let contents = scan(&mut session_store(&disk), Depth::Contents).unwrap();
    let mut want = vec![(gone, Problem::Missing), (swapped, Problem::HashMismatch)];
    want.sort_by_key(|(r, _)| (r.track(), r.range().start()));
    let mut got = as_found(contents.needs_attention());
    got.sort_by_key(|(r, _)| (r.track(), r.range().start()));
    assert_eq!(got, want);
    // Recorded for the app, unresolved; the scan changed no file.
    assert_eq!(read_findings(&session_dir(&disk)).unwrap().unresolved(), 2);
    assert_eq!(disk.read(&swapped_path).unwrap(), junk());

    // The missing file put back: a scan by names reads the rows with
    // unresolved findings, and finds it since verified.
    plant_file(&disk, &gone_path, &kept);
    let back = scan(&mut session_store(&disk), Depth::Names).unwrap();
    assert_eq!(
        as_found(back.needs_attention()),
        [(swapped, Problem::HashMismatch)]
    );
    assert_eq!(
        findings_of(&disk, &gone),
        [(Problem::Missing, Status::SinceVerified)]
    );
    assert_eq!(
        findings_of(&disk, &swapped),
        [(Problem::HashMismatch, Status::Unresolved)]
    );
}

#[test]
fn the_integrity_scan_says_a_row_journals_still_hold_needs_publishing() {
    let (disk, _, _, row, _) = with_a_committed_row(1);
    let found = scan(&mut session_store(&disk), Depth::Names).unwrap();
    assert_eq!(
        as_found(found.needs_publishing()),
        [(row, Problem::Missing)]
    );
    assert!(found.needs_attention().is_empty());
    // Scanning publishes nothing and deletes nothing.
    assert!(journals_left(&disk) > 0);
    // Salvage then repairs it.
    let done = salvage(&mut session_store(&disk), length()).unwrap();
    assert_eq!(done.repaired(), [(row, None)]);
    assert_eq!(
        findings_of(&disk, &row),
        [(Problem::Missing, Status::Repaired)]
    );
}

#[test]
fn the_scan_needs_the_session_to_itself() {
    let fs = FakeFs::with_dirs([session(), db()]);
    let lock = owned(&fs);
    let (_clock, dyn_clock) = fake_clock();
    let _writer = SessionWriter::open(&lock, rate(), length(), dyn_clock).unwrap();
    let mut store = store_on(&lock);
    assert!(matches!(
        scan(&mut store, Depth::Names),
        Err(PublishError::InUse(Use::Recording))
    ));
}

/// A session recorded on the fake disk, with its rows in a real library
/// database holding one row that doesn't parse.
#[expect(
    clippy::disallowed_methods,
    reason = "plants a row the schema's checks refuse, past nota-store, as a damaged database could hold"
)]
fn with_an_unparsable_row(dir: &TestDir) -> (FakeFs, nota_store::Store) {
    let (fs, _) = clean_run(Recording {
        steps: 4,
        publish: false,
        fail_at: None,
    });
    let path = dir.0.join("library.db");
    let mut store = nota_store::Store::open(&path).unwrap();
    store
        .create_session(&nota_store::NewSession::bare(SESSION))
        .unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    raw.execute(
        "INSERT INTO segment (session_id, track, epoch, start_sample, end_sample, sha256) \
         VALUES (1, 0, 0, 4800, 4900, zeroblob(31))",
        [],
    )
    .unwrap();
    (fs, store)
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "plants a row the schema's checks refuse, past nota-store, as a damaged database could hold"
)]
fn an_unparsable_row_stops_its_session_and_is_named() {
    let dir = TestDir::new("unparsable-row");
    let (fs, store) = with_an_unparsable_row(&dir);
    let journals = journals_left(&fs);
    let mut bound = SessionStore::new(owned(&fs), store);
    let err = salvage(&mut bound, length()).unwrap_err();
    assert!(matches!(err, PublishError::Store(_)), "{err}");
    let shown = err.to_string();
    assert!(
        shown.contains("session 1's segment row at track 0, sample 4800"),
        "{shown}"
    );
    // Nothing published or deleted, and the row named for the app.
    assert_eq!(journals_left(&fs), journals);
    let findings = read_findings(&session_dir(&fs)).unwrap();
    assert_eq!(findings.verification(), Verification::Unavailable);
    assert_eq!(findings.unparsable().len(), 1);
    let named = findings.unparsable()[0];
    assert_eq!(
        (named.key(), named.status()),
        (
            RowKey {
                track: 0,
                start: 4800
            },
            Status::Unresolved
        )
    );
    assert_eq!(findings.unresolved(), 1);
    // The scan stops on it too.
    assert!(scan(&mut bound, Depth::Names).is_err());

    // Once the row is gone (the user's fix), the session publishes, and
    // the row is since verified.
    rusqlite::Connection::open(dir.0.join("library.db"))
        .unwrap()
        .execute("DELETE FROM segment WHERE start_sample = 4800", [])
        .unwrap();
    let done = salvage(&mut bound, length()).unwrap();
    assert!(!done.segments().is_empty());
    assert_eq!(journals_left(&fs), 0);
    let findings = read_findings(&session_dir(&fs)).unwrap();
    assert_eq!(findings.unparsable()[0].status(), Status::SinceVerified);
    assert_eq!(findings.unresolved(), 0);
}

#[test]
fn a_findings_file_that_doesnt_parse_makes_the_scan_read_every_row() {
    let (disk, _, rows) = unsalvaged();
    salvage(&mut session_store(&disk), length()).unwrap();
    let swapped = rows[1];
    let path = durable_path(swapped.track(), swapped.range());
    disk.remove(&path).unwrap();
    plant_file(&disk, &path, &junk());
    let found = scan(&mut session_store(&disk), Depth::Contents).unwrap();
    assert_eq!(
        as_found(found.needs_attention()),
        [(swapped, Problem::HashMismatch)]
    );
    // The findings file damaged: which rows had findings isn't known, so
    // a scan by names reads every row, and finds the mismatch again.
    let findings = session().join(FINDINGS_FILE_NAME);
    disk.remove(&findings).unwrap();
    plant_file(&disk, &findings, b"not findings");
    let names = scan(&mut session_store(&disk), Depth::Names).unwrap();
    assert_eq!(
        as_found(names.needs_attention()),
        [(swapped, Problem::HashMismatch)]
    );
    let recorded = read_findings(&session_dir(&disk)).unwrap();
    assert_eq!(recorded.unresolved(), 1);
    // The damaged file is kept aside.
    assert!(
        disk.paths()
            .iter()
            .any(|p| disk.read(p).is_ok_and(|b| b == b"not findings"))
    );
}

#[test]
fn a_journal_that_cant_be_read_again_for_a_repair_stops_only_that_repair() {
    let (disk, _, rows, row, _) = with_a_committed_row(1);
    // Fail each operation in turn until one fails the repair's re-read of
    // the row's journals (after the first pass read them).
    let ops = {
        let probe = disk.copy_disk();
        salvage(&mut session_store(&probe), length()).unwrap();
        probe.attempted()
    };
    let (run, done) = (0..ops)
        .find_map(|at| {
            let run = disk.copy_disk();
            run.fail_after(at, io::ErrorKind::Other);
            let done = salvage(&mut session_store(&run), length()).ok()?;
            (!done.not_repaired().is_empty()).then_some((run, done))
        })
        .expect("no failing operation stopped only the repair");
    assert_eq!(done.not_repaired(), [(row, io::ErrorKind::Other)]);
    assert!(done.repaired().is_empty());
    assert_eq!(as_found(done.findings()), [(row, Problem::Missing)]);
    // Every other row's segment was published.
    assert_eq!(done.segments().len(), rows.len() - 1);
    // The next run repairs it.
    let again = salvage(&mut session_store(&run), length()).unwrap();
    assert_eq!(again.repaired(), [(row, None)]);
    assert_eq!(journals_left(&run), 0);
}

#[test]
fn a_row_with_two_findings_counts_once() {
    let (disk, _, rows) = unsalvaged();
    salvage(&mut session_store(&disk), length()).unwrap();
    let row = rows[1];
    let path = durable_path(row.track(), row.range());
    disk.remove(&path).unwrap();
    plant_file(&disk, &path, &junk());
    scan(&mut session_store(&disk), Depth::Contents).unwrap();
    disk.remove(&path).unwrap();
    disk.sync_dir(&session()).unwrap();
    scan(&mut session_store(&disk), Depth::Names).unwrap();
    let recorded = read_findings(&session_dir(&disk)).unwrap();
    assert_eq!(
        findings_of(&disk, &row),
        [
            (Problem::Missing, Status::Unresolved),
            (Problem::HashMismatch, Status::Unresolved)
        ]
    );
    assert_eq!(recorded.unresolved(), 1);
}

#[test]
fn the_scan_says_publishing_only_for_a_journal_of_the_rows_own_track_and_samples() {
    let (disk, promised, _) = unsalvaged();
    let (mic, system) = (
        promised.durable[&MIC].get(),
        promised.durable[&SYSTEM].get(),
    );
    assert!(system > mic + 10, "{mic} {system}");
    // On the mic, past its journals but where the system's go on.
    let rate_row = |start: u64| {
        SegmentRow::new(
            MIC,
            EpochId::new(0),
            SampleRange::new(SampleIndex::new(start), SampleIndex::new(start + 10)).unwrap(),
            nota_store::Sha256Digest::new([9; 32]),
        )
        .unwrap()
    };
    let past = rate_row(mic);
    let inside = rate_row(0);
    plant(&disk, &past);
    plant(&disk, &inside);
    let found = scan(&mut session_store(&disk), Depth::Names).unwrap();
    assert_eq!(
        as_found(found.needs_publishing()),
        [(inside, Problem::Missing)]
    );
    assert_eq!(
        as_found(found.needs_attention()),
        [(past, Problem::Missing)]
    );
}

#[test]
fn a_scan_whose_findings_cant_be_saved_says_so() {
    let (disk, _, _, row, _) = with_a_committed_row(1);
    disk.fail_on(
        &session().join("salvage-findings.tmp"),
        Fault::Create,
        io::ErrorKind::PermissionDenied,
    );
    let found = scan(&mut session_store(&disk), Depth::Names).unwrap();
    assert_eq!(
        found.findings_unsaved(),
        Some(io::ErrorKind::PermissionDenied)
    );
    assert_eq!(
        as_found(found.needs_publishing()),
        [(row, Problem::Missing)]
    );
}
