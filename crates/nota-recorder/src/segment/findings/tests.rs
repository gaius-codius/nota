//! The findings file: its format against untrusted bytes, and its atomic,
//! merging write under a crash at every operation.

use std::path::{Path, PathBuf};

use nota_core::{EpochId, SampleIndex, SampleRange, SessionId, TrackId};
use nota_store::{SegmentRow, Sha256Digest};
use proptest::prelude::*;

use super::*;
use crate::fs::fake::{CrashOutcome, FakeFs, Op};
use crate::session::SessionDir;

fn dir() -> PathBuf {
    PathBuf::from("/session")
}

fn row(track: u32, start: u64, end: u64, hash: u8) -> SegmentRow {
    SegmentRow::new(
        TrackId::new(track),
        EpochId::new(track + 1),
        SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap(),
        Sha256Digest::new([hash; 32]),
    )
    .unwrap()
}

fn found(fs: &FakeFs) -> Result<Findings, FindingsError> {
    read_findings(&SessionDir::new(SessionId::new(1), fs.clone(), &dir()))
}

fn problem() -> impl Strategy<Value = Problem> {
    prop_oneof![
        Just(Problem::Missing),
        Just(Problem::HashMismatch),
        Just(Problem::LengthMismatch),
        Just(Problem::Unreadable(ReadFailure::PermissionDenied)),
        Just(Problem::Unreadable(ReadFailure::IsADirectory)),
        Just(Problem::Unreadable(ReadFailure::Other)),
    ]
}

fn finding() -> impl Strategy<Value = Finding> {
    (
        any::<u32>(),
        any::<u32>(),
        any::<u64>(),
        1..=u64::MAX,
        any::<[u8; 32]>(),
        problem(),
    )
        .prop_map(|(track, epoch, start, len, hash, problem)| {
            let end = start.saturating_add(len).max(start.saturating_add(1));
            let start = start.min(end - 1);
            let row = SegmentRow::new(
                TrackId::new(track),
                EpochId::new(epoch),
                SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap(),
                Sha256Digest::new(hash),
            )
            .unwrap();
            Finding::new(row, problem)
        })
}

fn findings() -> impl Strategy<Value = Findings> {
    (
        prop::collection::vec(finding(), 0..6),
        prop_oneof![Just(Verification::Done), Just(Verification::Unavailable)],
    )
        .prop_map(|(found, verification)| Findings::from_parts(found, verification))
}

proptest! {
    #[test]
    fn findings_round_trip(f in findings()) {
        let bytes = f.encode();
        prop_assert_eq!(bytes.len(), HEADER_LEN + f.found().len() * ENTRY_LEN + CRC_LEN);
        prop_assert_eq!(Findings::decode(&bytes), Some(f));
    }

    #[test]
    fn a_torn_or_flipped_file_is_refused(f in findings(), cut in any::<prop::sample::Index>(), bit in any::<prop::sample::Index>()) {
        let bytes = f.encode();
        let at = cut.index(bytes.len());
        prop_assert_eq!(Findings::decode(&bytes[..at]), None);
        let mut flipped = bytes.clone();
        let bit = bit.index(bytes.len() * 8);
        flipped[bit / 8] ^= 1 << (bit % 8);
        prop_assert_eq!(Findings::decode(&flipped), None);
    }

    #[test]
    fn sealed_random_entries_parse_into_valid_findings_or_are_refused(
        verification in 0_u8..3,
        count in 0_u32..4,
        body in prop::collection::vec(any::<u8>(), 0..300),
        entries in prop::collection::vec((any::<u32>(), any::<u32>(), 0_u64..50, 0_u64..50, 0_u8..7, 0_u8..2), 0..4),
    ) {
        // Past the CRC and magic, so the field checks are what's tested.
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.push(verification);
        bytes.extend_from_slice(&count.to_le_bytes());
        let mut valid = verification < 2 && count as usize == entries.len();
        for &(track, epoch, start, end, problem, status) in &entries {
            bytes.extend_from_slice(&track.to_le_bytes());
            bytes.extend_from_slice(&epoch.to_le_bytes());
            bytes.extend_from_slice(&start.to_le_bytes());
            bytes.extend_from_slice(&end.to_le_bytes());
            bytes.extend_from_slice(&[u8::try_from(start).unwrap(); 32]);
            bytes.push(problem);
            bytes.push(status);
            valid &= start < end && problem < 6 && status == 0;
        }
        if body.len() % 2 == 1 {
            // Sometimes trailing junk too.
            bytes.extend_from_slice(&body);
            valid &= body.is_empty();
        }
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        let got = Findings::decode(&bytes);
        prop_assert_eq!(got.is_some(), valid);
        if let Some(f) = got {
            // Every entry, once each, as written.
            let mut want: Vec<_> = entries
                .iter()
                .map(|&(track, epoch, start, end, problem, _)| (track, epoch, start, end, problem))
                .collect();
            want.sort_unstable();
            want.dedup();
            let mut got: Vec<_> = f
                .found()
                .iter()
                .map(|f| {
                    let r = f.row();
                    prop_assert_eq!(r.sha256().as_bytes()[0], u8::try_from(r.range().start().get()).unwrap());
                    Ok((r.track().get(), r.epoch().get(), r.range().start().get(), r.range().end().get(), f.problem().code()))
                })
                .collect::<Result<_, TestCaseError>>()?;
            got.sort_unstable();
            prop_assert_eq!(got, want);
            prop_assert!(f.found().windows(2).all(|w| w[0].key() < w[1].key()));
            prop_assert!(f.found().iter().all(|f| !f.row().range().is_empty()));
            prop_assert_eq!(Findings::decode(&f.encode()), Some(f));
        }
    }

    #[test]
    fn any_bytes_parse_or_are_refused_without_panicking(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
        if let Some(f) = Findings::decode(&bytes) {
            // Anything accepted is sorted and without duplicates.
            prop_assert!(f.found().windows(2).all(|w| w[0].key() < w[1].key()));
        }
    }

    #[test]
    fn merging_is_a_set_union(a in findings(), b in prop::collection::vec(finding(), 0..6)) {
        let merged = a.merged(&b, Verification::Done);
        for f in a.found().iter().chain(&b) {
            prop_assert!(merged.found().contains(f));
        }
        prop_assert!(merged.found().iter().all(|f| a.found().contains(f) || b.contains(f)));
        prop_assert!(merged.found().windows(2).all(|w| w[0].key() < w[1].key()));
        prop_assert_eq!(merged.merged(&b, Verification::Done), merged.clone());
    }
}

type Edit = dyn Fn(&mut Vec<u8>);

/// Refused with valid CRCs: what the CRC alone wouldn't catch.
#[test]
fn well_formed_but_invalid_fields_are_refused() {
    const ENTRY: usize = HEADER_LEN;
    let f = Findings::from_parts(
        vec![Finding::new(row(0, 0, 10, 1), Problem::Missing)],
        Verification::Done,
    );
    let good = f.encode();
    let resealed = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = good[..good.len() - CRC_LEN].to_vec();
        edit(&mut bytes);
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        Findings::decode(&bytes)
    };
    assert_eq!(resealed(&|_| {}), Some(f));
    let cases: [(&str, &Edit); 8] = [
        ("magic", &|b| b[0] = b'X'),
        ("version", &|b| b[8] = 2),
        ("verification", &|b| b[10] = 2),
        ("count too high", &|b| b[11] = 2),
        ("count too low", &|b| b[11] = 0),
        ("empty range", &|b| {
            b[ENTRY + 16..ENTRY + 24].copy_from_slice(&0_u64.to_le_bytes());
        }),
        ("problem", &|b| b[ENTRY + 56] = 6),
        ("status", &|b| b[ENTRY + 57] = 1),
    ];
    for (what, edit) in cases {
        assert_eq!(resealed(edit), None, "{what}");
    }
    // A count whose entries would overflow the length.
    assert_eq!(
        resealed(&|b| b[11..15].copy_from_slice(&u32::MAX.to_le_bytes())),
        None
    );
    // The fields are where the docs say.
    assert_eq!(&good[..8], b"NOTAFIND");
    assert_eq!(good[8..10], 1_u16.to_le_bytes());
    assert_eq!(good[10], 0);
    assert_eq!(good[11..15], 1_u32.to_le_bytes());
    assert_eq!(good[ENTRY..ENTRY + 4], 0_u32.to_le_bytes());
    assert_eq!(good[ENTRY + 4..ENTRY + 8], 1_u32.to_le_bytes());
    assert_eq!(good[ENTRY + 24..ENTRY + 56], [1; 32]);
}

/// The problem codes are the ones the module docs give, and an unreadable
/// file's error kind maps to the failure kept for it.
#[test]
fn problem_codes_and_read_failures() {
    let codes = [
        (Problem::Missing, 0),
        (Problem::HashMismatch, 1),
        (Problem::LengthMismatch, 2),
        (Problem::Unreadable(ReadFailure::PermissionDenied), 3),
        (Problem::Unreadable(ReadFailure::IsADirectory), 4),
        (Problem::Unreadable(ReadFailure::Other), 5),
    ];
    for (problem, code) in codes {
        assert_eq!(problem.code(), code, "{problem:?}");
        assert_eq!(Problem::from_code(code), Some(problem));
        let f = Findings::from_parts(
            vec![Finding::new(row(0, 0, 10, 1), problem)],
            Verification::Done,
        );
        assert_eq!(f.encode()[HEADER_LEN + 56], code);
    }
    assert_eq!(Problem::from_code(6), None);
    assert_eq!(
        ReadFailure::of(io::ErrorKind::PermissionDenied),
        ReadFailure::PermissionDenied
    );
    assert_eq!(
        ReadFailure::of(io::ErrorKind::IsADirectory),
        ReadFailure::IsADirectory
    );
    for kind in [
        io::ErrorKind::Other,
        io::ErrorKind::Interrupted,
        io::ErrorKind::InvalidData,
    ] {
        assert_eq!(ReadFailure::of(kind), ReadFailure::Other, "{kind:?}");
    }
}

#[test]
fn no_file_reads_as_no_findings_and_nothing_to_say_writes_nothing() {
    let fs = FakeFs::with_dirs([dir()]);
    assert_eq!(found(&fs).unwrap(), Findings::default());
    let got = record(&fs, &dir(), &[], Verification::Done).unwrap();
    assert_eq!(got, Findings::default());
    assert!(fs.paths().is_empty());
    assert!(
        fs.ops()
            .iter()
            .all(|op| matches!(op, Op::Read(_) | Op::List(_)))
    );
}

#[test]
fn findings_merge_and_are_rewritten_only_when_they_change() {
    let fs = FakeFs::with_dirs([dir()]);
    let a = Finding::new(row(1, 50, 60, 2), Problem::HashMismatch);
    let b = Finding::new(row(0, 0, 10, 1), Problem::Missing);
    let got = record(&fs, &dir(), &[a, a], Verification::Done).unwrap();
    assert_eq!(got.found(), [a]);
    assert_eq!(found(&fs).unwrap(), got);

    // The same again: no write, only a directory sync.
    let ops = fs.ops().len();
    record(&fs, &dir(), &[a], Verification::Done).unwrap();
    assert_eq!(
        fs.ops()[ops..],
        [Op::Read(dir().join(FILE_NAME)), Op::SyncDir(dir())]
    );

    // Nothing new found this time: the old finding stays.
    record(&fs, &dir(), &[], Verification::Done).unwrap();
    assert_eq!(found(&fs).unwrap().found(), [a]);

    // The store unreadable: said so, and nothing is cleared.
    let got = record(&fs, &dir(), &[], Verification::Unavailable).unwrap();
    assert_eq!(got.verification(), Verification::Unavailable);
    assert_eq!(found(&fs).unwrap(), got);
    assert_eq!(got.found(), [a]);

    // Back, with a new finding: sorted by track, both kept.
    let got = record(&fs, &dir(), &[a, b], Verification::Done).unwrap();
    assert_eq!(got.found(), [b, a]);
    assert_eq!(got.verification(), Verification::Done);
    assert_eq!(found(&fs).unwrap(), got);
    assert_eq!(fs.paths(), [dir().join(FILE_NAME)]);
}

#[test]
fn a_corrupt_file_is_set_aside_and_findings_start_again() {
    let fs = FakeFs::with_dirs([dir()]);
    let mut file = fs.create(&dir().join(FILE_NAME)).unwrap();
    file.write_all(b"not findings").unwrap();
    file.sync().unwrap();
    assert!(matches!(found(&fs), Err(FindingsError::Corrupt)));

    let a = Finding::new(row(0, 0, 10, 1), Problem::LengthMismatch);
    let got = record(&fs, &dir(), &[a], Verification::Done).unwrap();
    assert_eq!(got.found(), [a]);
    assert_eq!(found(&fs).unwrap(), got);
    assert_eq!(
        fs.read(&dir().join(ASIDE_NAME)).unwrap(),
        b"not findings".to_vec()
    );

    // Corrupt again, with nothing to say: set aside all the same, beside
    // the first, and no new file.
    for junk in [b"junk 1", b"junk 2"] {
        let _ = fs.remove(&dir().join(FILE_NAME));
        let mut file = fs.create(&dir().join(FILE_NAME)).unwrap();
        file.write_all(junk).unwrap();
        record(&fs, &dir(), &[], Verification::Done).unwrap();
    }
    let aside = |n: &str| dir().join(format!("{ASIDE_NAME}{n}"));
    assert_eq!(fs.paths(), [aside(""), aside(".1"), aside(".2")]);
    assert_eq!(fs.read(&aside("")).unwrap(), b"not findings".to_vec());
    assert_eq!(fs.read(&aside(".1")).unwrap(), b"junk 1".to_vec());
    assert_eq!(fs.read(&aside(".2")).unwrap(), b"junk 2".to_vec());
    assert_eq!(found(&fs).unwrap(), Findings::default());
}

#[test]
fn errors_describe_themselves() {
    let io = FindingsError::Io(io::Error::other("disk"));
    assert!(io.to_string().contains("disk"));
    assert!(io.source().is_some());
    assert!(FindingsError::Corrupt.to_string().contains("corrupt"));
    assert!(FindingsError::Corrupt.source().is_none());
    let fs = FakeFs::with_dirs([dir()]);
    fs.fail_after(0, io::ErrorKind::PermissionDenied);
    assert!(
        matches!(found(&fs), Err(FindingsError::Io(e)) if e.kind() == io::ErrorKind::PermissionDenied)
    );
    fs.fail_after(0, io::ErrorKind::PermissionDenied);
    let err = record(&fs, &dir(), &[], Verification::Done).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn a_temp_name_is_only_the_findings_temp() {
    assert!(is_temp(&dir().join("salvage-findings.tmp")));
    assert!(!is_temp(&dir().join(FILE_NAME)));
    assert!(!is_temp(&dir().join(ASIDE_NAME)));
    assert!(!is_temp(Path::new("/x/seg-t0-000000000000.flac.tmp")));
}

/// What a findings file read after a crash may hold: the old findings or
/// the new, never a torn or missing file.
fn old_or_new(fs: &FakeFs, old: &Findings, new: &Findings, case: &str) -> bool {
    let got = found(fs).unwrap_or_else(|e| panic!("{case}: {e}"));
    assert!(got == *old || got == *new, "{case}: {got:?}");
    got == *new
}

#[test]
fn a_write_crashed_at_every_operation_leaves_the_old_findings_or_the_new() {
    let a = Finding::new(row(0, 0, 10, 1), Problem::Missing);
    let b = Finding::new(row(1, 20, 30, 2), Problem::HashMismatch);
    let base = FakeFs::with_dirs([dir()]);
    let old = record(&base, &dir(), &[a], Verification::Done).unwrap();
    let probe = base.copy_disk();
    let new = record(&probe, &dir(), &[b], Verification::Unavailable).unwrap();
    assert_ne!(old, new);
    let ops = probe.attempted();
    assert!(ops >= 7, "{ops}");
    let mut saw = (0, 0);
    for after in 0..=ops {
        for outcome in CrashOutcome::standard() {
            let run = base.copy_disk();
            run.crash_after(after);
            let _ = record(&run, &dir(), &[b], Verification::Unavailable);
            let survived = run.crash(outcome);
            let case = format!("after {after}, {outcome:?}");
            if old_or_new(&survived, &old, &new, &case) {
                saw.1 += 1;
            } else {
                saw.0 += 1;
            }
            // The retry crashed too, anywhere: still old or new.
            let retry_ops = {
                let probe = survived.copy_disk();
                record(&probe, &dir(), &[b], Verification::Unavailable).unwrap();
                probe.attempted()
            };
            for again in 0..=retry_ops {
                // More partial outcomes than usual: the case that matters is
                // a reused temp name whose unlink is lost but whose rename
                // survives, which few seeds pick.
                let seconds = (8..64).map(|seed| CrashOutcome::Partial { seed });
                for second in CrashOutcome::standard().into_iter().chain(seconds) {
                    let run = survived.copy_disk();
                    run.crash_after(again);
                    let _ = record(&run, &dir(), &[b], Verification::Unavailable);
                    let twice = run.crash(second);
                    let case = format!("{case}, then after {again}, {second:?}");
                    old_or_new(&twice, &old, &new, &case);
                    // An uninterrupted retry finishes the job.
                    let done = record(&twice, &dir(), &[b], Verification::Unavailable).unwrap();
                    assert_eq!(done, new, "{case}");
                    assert_eq!(found(&twice).unwrap(), new, "{case}");
                }
            }
        }
    }
    assert!(saw.0 > 0 && saw.1 > 0, "{saw:?}");
}

#[test]
fn a_retry_after_any_failed_operation_is_durable_when_it_returns() {
    let a = Finding::new(row(0, 0, 10, 1), Problem::Missing);
    let b = Finding::new(row(1, 20, 30, 2), Problem::HashMismatch);
    let base = FakeFs::with_dirs([dir()]);
    let old = record(&base, &dir(), &[a], Verification::Done).unwrap();
    let probe = base.copy_disk();
    let new = record(&probe, &dir(), &[b], Verification::Done).unwrap();
    let ops = probe.attempted();
    for at in 0..ops {
        for outcome in CrashOutcome::standard() {
            let run = base.copy_disk();
            run.fail_after(at, io::ErrorKind::Other);
            let first = record(&run, &dir(), &[b], Verification::Done);
            assert!(first.is_err(), "failing op {at} went unnoticed");
            // The retry succeeds: then the new findings survive any crash.
            assert_eq!(record(&run, &dir(), &[b], Verification::Done).unwrap(), new);
            let survived = run.crash(outcome);
            assert_eq!(
                found(&survived).unwrap(),
                new,
                "failing op {at}, {outcome:?}"
            );
        }
    }
    assert_ne!(old, new);
}
