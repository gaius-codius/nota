//! The findings file: its format against untrusted bytes, and its atomic,
//! merging write under a crash at every operation.

use std::path::{Path, PathBuf};

use nota_core::{EpochId, SampleIndex, SampleRange, SessionId, TrackId};
use nota_store::{AudioDigest, SegmentRow, Sha256Digest};
use proptest::prelude::*;

use super::*;
use crate::fs::fake::{CrashOutcome, FakeFs, Op};
use crate::fs::sweep::Sweep;
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

/// Records a run that found `new`, with `verification`.
fn rec(fs: &FakeFs, new: &[Finding], verification: Verification) -> io::Result<Findings> {
    record(
        fs,
        &dir(),
        &Run {
            found: new,
            verification,
            ..Run::default()
        },
    )
}

fn status() -> impl Strategy<Value = Status> {
    prop_oneof![
        Just(Status::Unresolved),
        Just(Status::SinceVerified),
        Just(Status::Repaired),
    ]
}

fn unparsable() -> impl Strategy<Value = UnparsableRow> {
    (any::<i64>(), any::<i64>(), status()).prop_map(|(track, start, status)| UnparsableRow {
        key: RowKey { track, start },
        status,
    })
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
        proptest::option::of(any::<[u8; 32]>()),
        problem(),
        status(),
    )
        .prop_map(|(track, epoch, start, len, hash, audio, problem, status)| {
            let end = start.saturating_add(len).max(start.saturating_add(1));
            let start = start.min(end - 1);
            let row = SegmentRow::new(
                TrackId::new(track),
                EpochId::new(epoch),
                SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap(),
                Sha256Digest::new(hash),
            )
            .unwrap();
            let row = match audio {
                Some(a) => row.with_audio(AudioDigest::new(a)),
                None => row,
            };
            Finding {
                row,
                problem,
                status,
            }
        })
}

fn findings() -> impl Strategy<Value = Findings> {
    (
        prop::collection::vec(finding(), 0..6),
        prop::collection::vec(unparsable(), 0..3),
        prop_oneof![Just(Verification::Done), Just(Verification::Unavailable)],
    )
        .prop_map(|(found, unparsable, verification)| {
            Findings::from_parts(found, unparsable, verification)
        })
}

proptest! {
    #[test]
    fn findings_round_trip(f in findings()) {
        let bytes = f.encode();
        prop_assert_eq!(
            bytes.len(),
            HEADER_LEN + f.found().len() * ENTRY_LEN + COUNT_LEN
                + f.unparsable().len() * UNPARSABLE_LEN + CRC_LEN
        );
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
        entries in prop::collection::vec(
            (any::<u32>(), any::<u32>(), 0_u64..50, 0_u64..50, 0_u8..3, 0_u8..7, 0_u8..4),
            0..4,
        ),
        unparsable in prop::collection::vec((any::<i64>(), any::<i64>(), 0_u8..4), 0..3),
        listed in 0_u32..4,
    ) {
        // Past the CRC and magic, so the field checks are what's tested.
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.push(verification);
        bytes.extend_from_slice(&count.to_le_bytes());
        let mut valid = verification < 2 && count as usize == entries.len();
        for &(track, epoch, start, end, flag, problem, status) in &entries {
            bytes.extend_from_slice(&track.to_le_bytes());
            bytes.extend_from_slice(&epoch.to_le_bytes());
            bytes.extend_from_slice(&start.to_le_bytes());
            bytes.extend_from_slice(&end.to_le_bytes());
            bytes.extend_from_slice(&[u8::try_from(start).unwrap(); 32]);
            bytes.push(flag);
            // A flag of 0 needs a zero digest; 2 is no flag at all.
            bytes.extend_from_slice(&[if flag == 1 { 9 } else { 0 }; 32]);
            bytes.push(problem);
            bytes.push(status);
            valid &= start < end && flag < 2 && problem < 6 && status < 3;
        }
        bytes.extend_from_slice(&listed.to_le_bytes());
        valid &= listed as usize == unparsable.len();
        for &(track, start, status) in &unparsable {
            bytes.extend_from_slice(&track.to_le_bytes());
            bytes.extend_from_slice(&start.to_le_bytes());
            bytes.push(status);
            valid &= status < 3;
        }
        if body.len() % 2 == 1 {
            // Sometimes trailing junk too.
            bytes.extend_from_slice(&body);
            valid &= body.is_empty();
        }
        // One entry per key.
        let mut keys: Vec<_> = entries.iter().map(|e| (e.0, e.1, e.2, e.3, e.4, e.5)).collect();
        keys.sort_unstable();
        keys.dedup();
        valid &= keys.len() == entries.len();
        let mut rows: Vec<_> = unparsable.iter().map(|u| (u.0, u.1)).collect();
        rows.sort_unstable();
        rows.dedup();
        valid &= rows.len() == unparsable.len();
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        let got = Findings::decode(&bytes);
        prop_assert_eq!(got.is_some(), valid);
        if let Some(f) = got {
            // Every entry, once each, as written.
            let mut want: Vec<_> = entries
                .iter()
                .map(|&(track, epoch, start, end, flag, problem, status)| {
                    (track, epoch, start, end, flag == 1, problem, status)
                })
                .collect();
            want.sort_unstable();
            let mut got: Vec<_> = f
                .found()
                .iter()
                .map(|f| {
                    let r = f.row();
                    prop_assert_eq!(r.sha256().as_bytes()[0], u8::try_from(r.range().start().get()).unwrap());
                    prop_assert!(r.audio().is_none_or(|a| *a.as_bytes() == [9; 32]));
                    Ok((
                        r.track().get(),
                        r.epoch().get(),
                        r.range().start().get(),
                        r.range().end().get(),
                        r.audio().is_some(),
                        problem_code(f.problem()),
                        status_code(f.status()),
                    ))
                })
                .collect::<Result<_, TestCaseError>>()?;
            got.sort_unstable();
            prop_assert_eq!(got, want);
            let mut want: Vec<_> = unparsable;
            want.sort_unstable_by_key(|u| (u.0, u.1));
            let got: Vec<_> = f
                .unparsable()
                .iter()
                .map(|u| (u.key().track, u.key().start, status_code(u.status())))
                .collect();
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
    fn merging_keeps_every_finding_once(
        a in findings(),
        b in prop::collection::vec(finding(), 0..6),
        verified in prop::collection::vec(finding(), 0..3),
        repaired in prop::collection::vec(finding(), 0..3),
        key in proptest::option::of((any::<i64>(), any::<i64>())),
        done in any::<bool>(),
    ) {
        // Findings a run makes are new: unresolved.
        let b: Vec<Finding> = b.into_iter().map(|f| Finding::new(f.row, f.problem)).collect();
        let verified: Vec<SegmentRow> = verified.iter().map(|f| f.row).collect();
        let repaired: Vec<SegmentRow> = repaired.iter().map(|f| f.row).collect();
        let run = Run {
            found: &b,
            verified: &verified,
            repaired: &repaired,
            unparsable: key.map(|(track, start)| RowKey { track, start }),
            verification: if done { Verification::Done } else { Verification::Unavailable },
        };
        let merged = a.merged(&run);
        let failing = |row: &SegmentRow| b.iter().any(|f| f.row == *row);
        // Every finding, old or new, is there once, by key.
        for f in a.found().iter().chain(&b) {
            prop_assert_eq!(merged.found().iter().filter(|m| m.key() == f.key()).count(), 1);
        }
        prop_assert_eq!(
            merged.found().len(),
            a.found().len() + b.iter().filter(|f| !a.found().iter().any(|o| o.key() == f.key())).count()
        );
        prop_assert!(merged.found().windows(2).all(|w| w[0].key() < w[1].key()));
        for m in merged.found() {
            let old = a.found().iter().find(|o| o.key() == m.key());
            let want = if b.iter().any(|f| f.key() == m.key()) {
                // Found again: unresolved again.
                Status::Unresolved
            } else {
                let was = old.map_or(Status::Unresolved, Finding::status);
                match was {
                    Status::Unresolved if failing(&m.row) => Status::Unresolved,
                    Status::Unresolved if verified.contains(&m.row) => Status::SinceVerified,
                    Status::Unresolved if repaired.contains(&m.row) => Status::Repaired,
                    other => other,
                }
            };
            // A row found failing this run isn't verified or repaired in it,
            // even if a run says both.
            prop_assert_eq!(m.status(), want);
        }
        for u in merged.unparsable() {
            let want = if Some((u.key().track, u.key().start)) == key {
                Status::Unresolved
            } else {
                let was = a.unparsable().iter().find(|o| o.key() == u.key()).unwrap().status();
                if done && was == Status::Unresolved { Status::SinceVerified } else { was }
            };
            prop_assert_eq!(u.status(), want);
        }
        prop_assert_eq!(merged.verification(), run.verification);
        // Merging the same run again changes nothing.
        prop_assert_eq!(Findings::decode(&merged.encode()), Some(merged.clone()));
        prop_assert_eq!(merged.merged(&run), merged);
    }
}

type Edit = dyn Fn(&mut Vec<u8>);

/// Refused with valid CRCs: what the CRC alone wouldn't catch.
#[test]
fn well_formed_but_invalid_fields_are_refused() {
    const ENTRY: usize = HEADER_LEN;
    const AFTER: usize = HEADER_LEN + ENTRY_LEN;
    let f = Findings::from_parts(
        vec![Finding::new(row(0, 0, 10, 1), Problem::Missing)],
        vec![UnparsableRow {
            key: RowKey {
                track: -1,
                start: 5,
            },
            status: Status::Repaired,
        }],
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
    let cases: [(&str, &Edit); 14] = [
        ("magic", &|b| b[0] = b'X'),
        ("version", &|b| b[8] = 3),
        ("version 0", &|b| b[8] = 0),
        ("verification", &|b| b[10] = 2),
        ("count too high", &|b| b[11] = 2),
        ("count too low", &|b| b[11] = 0),
        ("empty range", &|b| {
            b[ENTRY + 16..ENTRY + 24].copy_from_slice(&0_u64.to_le_bytes());
        }),
        ("audio flag", &|b| b[ENTRY + 56] = 2),
        ("no audio but a digest", &|b| b[ENTRY + 57] = 1),
        ("problem", &|b| b[ENTRY + 89] = 6),
        ("status", &|b| b[ENTRY + 90] = 3),
        ("unparsable count too high", &|b| b[AFTER] = 2),
        ("unparsable count too low", &|b| b[AFTER] = 0),
        ("unparsable status", &|b| b[AFTER + 4 + 16] = 3),
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
    assert_eq!(good[8..10], 2_u16.to_le_bytes());
    assert_eq!(good[10], 0);
    assert_eq!(good[11..15], 1_u32.to_le_bytes());
    assert_eq!(good[ENTRY..ENTRY + 4], 0_u32.to_le_bytes());
    assert_eq!(good[ENTRY + 4..ENTRY + 8], 1_u32.to_le_bytes());
    assert_eq!(good[ENTRY + 24..ENTRY + 56], [1; 32]);
    assert_eq!(good[ENTRY + 56..ENTRY + 89], [0; 33]);
    assert_eq!(good[AFTER..AFTER + 4], 1_u32.to_le_bytes());
    assert_eq!(good[AFTER + 4..AFTER + 12], (-1_i64).to_le_bytes());
    assert_eq!(good[AFTER + 12..AFTER + 20], 5_i64.to_le_bytes());
    assert_eq!(good[AFTER + 20], 2);
    // With an audio digest: the flag, then the digest.
    let audio = Findings::from_parts(
        vec![Finding::new(
            row(0, 0, 10, 1).with_audio(AudioDigest::new([7; 32])),
            Problem::Missing,
        )],
        vec![],
        Verification::Done,
    )
    .encode();
    assert_eq!(audio[ENTRY + 56], 1);
    assert_eq!(audio[ENTRY + 57..ENTRY + 89], [7; 32]);
}

/// A file with the same finding, or the same unparsable row, twice isn't
/// one the writer makes: refused.
#[test]
fn duplicate_entries_are_refused() {
    let finding = Finding::new(row(0, 0, 10, 1), Problem::Missing);
    let unparsable = UnparsableRow {
        key: RowKey { track: 0, start: 1 },
        status: Status::Unresolved,
    };
    let unique = Findings {
        found: vec![finding],
        unparsable: vec![unparsable],
        verification: Verification::Done,
    };
    assert_eq!(Findings::decode(&unique.encode()), Some(unique.clone()));
    for twice in [
        Findings {
            found: vec![finding, finding],
            ..unique.clone()
        },
        Findings {
            unparsable: vec![unparsable, unparsable],
            ..unique
        },
    ] {
        assert_eq!(Findings::decode(&twice.encode()), None, "{twice:?}");
    }
}

/// A version 1 file, as M1 wrote it, still reads: no audio digests, every
/// finding unresolved, and no unparsable rows. It's rewritten as version 2
/// when it changes.
#[test]
fn a_version_1_file_still_reads() {
    let mut v1 = MAGIC.to_vec();
    v1.extend_from_slice(&1_u16.to_le_bytes());
    v1.push(1);
    v1.extend_from_slice(&1_u32.to_le_bytes());
    v1.extend_from_slice(&3_u32.to_le_bytes());
    v1.extend_from_slice(&4_u32.to_le_bytes());
    v1.extend_from_slice(&10_u64.to_le_bytes());
    v1.extend_from_slice(&20_u64.to_le_bytes());
    v1.extend_from_slice(&[6; 32]);
    v1.push(1);
    v1.push(0);
    let sealed = |mut bytes: Vec<u8>| {
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes
    };
    let want_row = SegmentRow::new(
        TrackId::new(3),
        EpochId::new(4),
        SampleRange::new(SampleIndex::new(10), SampleIndex::new(20)).unwrap(),
        Sha256Digest::new([6; 32]),
    )
    .unwrap();
    let got = Findings::decode(&sealed(v1.clone())).unwrap();
    assert_eq!(got.verification(), Verification::Unavailable);
    assert_eq!(got.found(), [Finding::new(want_row, Problem::HashMismatch)]);
    assert!(got.unparsable().is_empty());
    // In version 1 only status 0 exists, and nothing follows the entries.
    let mut status = v1.clone();
    *status.last_mut().unwrap() = 1;
    assert_eq!(Findings::decode(&sealed(status)), None);
    let mut more = v1.clone();
    more.extend_from_slice(&0_u32.to_le_bytes());
    assert_eq!(Findings::decode(&sealed(more)), None);

    let fs = FakeFs::with_dirs([dir()]);
    let mut file = fs.create(&dir().join(FILE_NAME)).unwrap();
    file.write_all(&sealed(v1)).unwrap();
    file.sync().unwrap();
    assert_eq!(found(&fs).unwrap(), got);
    let verified = [want_row];
    let now = record(
        &fs,
        &dir(),
        &Run {
            verified: &verified,
            ..Run::default()
        },
    )
    .unwrap();
    assert_eq!(now.found()[0].status(), Status::SinceVerified);
    let bytes = fs.read(&dir().join(FILE_NAME)).unwrap();
    assert_eq!(bytes[8..10], 2_u16.to_le_bytes());
    assert_eq!(found(&fs).unwrap(), now);
}

/// A finding is never dropped: it's marked since verified or repaired, and
/// unresolved again if found again. An unparsable row is named, and since
/// verified once the rows read.
#[test]
fn statuses_follow_what_runs_find() {
    let fs = FakeFs::with_dirs([dir()]);
    let r = row(0, 0, 10, 1);
    let other = row(1, 0, 10, 2);
    let missing = Finding::new(r, Problem::Missing);
    let mismatch = Finding::new(other, Problem::HashMismatch);
    let got = rec(&fs, &[missing, mismatch], Verification::Done).unwrap();
    assert_eq!(got.unresolved(), 2);

    let run = |verified: &[SegmentRow], repaired: &[SegmentRow]| {
        record(
            &fs,
            &dir(),
            &Run {
                verified,
                repaired,
                ..Run::default()
            },
        )
        .unwrap()
    };
    let got = run(&[r], &[other]);
    assert_eq!(
        got.found()
            .iter()
            .map(|f| (*f.row(), f.status()))
            .collect::<Vec<_>>(),
        [(r, Status::SinceVerified), (other, Status::Repaired)]
    );
    assert_eq!(got.unresolved(), 0);
    // A status, once given, isn't changed by later verifications.
    assert_eq!(run(&[other], &[r]), got);
    // Found again: unresolved again.
    let again = rec(&fs, &[missing], Verification::Done).unwrap();
    assert_eq!(again.found()[0].status(), Status::Unresolved);
    assert_eq!(again.unresolved(), 1);
    assert_eq!(found(&fs).unwrap(), again);

    // A row that doesn't parse stops the run, and is named.
    let key = RowKey {
        track: 0,
        start: -4,
    };
    let stopped = record(
        &fs,
        &dir(),
        &Run {
            unparsable: Some(key),
            verification: Verification::Unavailable,
            ..Run::default()
        },
    )
    .unwrap();
    assert_eq!(
        stopped.unparsable(),
        [UnparsableRow {
            key,
            status: Status::Unresolved
        }]
    );
    assert_eq!(stopped.unresolved(), 2);
    // Still stopped: still named, once.
    let still = record(
        &fs,
        &dir(),
        &Run {
            unparsable: Some(key),
            verification: Verification::Unavailable,
            ..Run::default()
        },
    )
    .unwrap();
    assert_eq!(still, stopped);
    // Another failure to read the store says nothing about the row.
    let unavailable = rec(&fs, &[], Verification::Unavailable).unwrap();
    assert_eq!(unavailable.unparsable()[0].status(), Status::Unresolved);
    // Read again: since verified.
    let read = rec(&fs, &[], Verification::Done).unwrap();
    assert_eq!(read.unparsable()[0].status(), Status::SinceVerified);
    assert_eq!(read.unresolved(), 1);
    assert_eq!(found(&fs).unwrap(), read);

    // The index has every entry.
    let indexed = read.indexed();
    assert_eq!(indexed.len(), 3);
    assert!(indexed.contains(&IndexedFinding::Unparsable {
        key,
        status: Status::SinceVerified
    }));
    assert!(indexed.contains(&IndexedFinding::Row {
        row: other,
        problem: Problem::HashMismatch,
        status: Status::Repaired
    }));
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
        assert_eq!(problem_code(problem), code, "{problem:?}");
        assert_eq!(problem_from_code(code), Some(problem));
        let f = Findings::from_parts(
            vec![Finding::new(row(0, 0, 10, 1), problem)],
            vec![],
            Verification::Done,
        );
        assert_eq!(f.encode()[HEADER_LEN + 89], code);
    }
    assert_eq!(problem_from_code(6), None);
    for (status, code) in [
        (Status::Unresolved, 0),
        (Status::SinceVerified, 1),
        (Status::Repaired, 2),
    ] {
        assert_eq!(status_code(status), code);
        assert_eq!(status_from_code(code), Some(status));
    }
    assert_eq!(status_from_code(3), None);
    assert_eq!(
        read_failure(io::ErrorKind::PermissionDenied),
        ReadFailure::PermissionDenied
    );
    assert_eq!(
        read_failure(io::ErrorKind::IsADirectory),
        ReadFailure::IsADirectory
    );
    for kind in [
        io::ErrorKind::Other,
        io::ErrorKind::Interrupted,
        io::ErrorKind::InvalidData,
    ] {
        assert_eq!(read_failure(kind), ReadFailure::Other, "{kind:?}");
    }
}

#[test]
fn no_file_reads_as_no_findings_and_nothing_to_say_writes_nothing() {
    let fs = FakeFs::with_dirs([dir()]);
    assert_eq!(found(&fs).unwrap(), Findings::default());
    let got = rec(&fs, &[], Verification::Done).unwrap();
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
    let got = rec(&fs, &[a, a], Verification::Done).unwrap();
    assert_eq!(got.found(), [a]);
    assert_eq!(found(&fs).unwrap(), got);

    // The same again: no write, only a directory sync.
    let ops = fs.ops().len();
    rec(&fs, &[a], Verification::Done).unwrap();
    assert_eq!(
        fs.ops()[ops..],
        [Op::Read(dir().join(FILE_NAME)), Op::SyncDir(dir())]
    );

    // Nothing new found this time: the old finding stays.
    rec(&fs, &[], Verification::Done).unwrap();
    assert_eq!(found(&fs).unwrap().found(), [a]);

    // The store unreadable: said so, and nothing is cleared.
    let got = rec(&fs, &[], Verification::Unavailable).unwrap();
    assert_eq!(got.verification(), Verification::Unavailable);
    assert_eq!(found(&fs).unwrap(), got);
    assert_eq!(got.found(), [a]);

    // Back, with a new finding: sorted by track, both kept.
    let got = rec(&fs, &[a, b], Verification::Done).unwrap();
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
    let got = rec(&fs, &[a], Verification::Done).unwrap();
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
        rec(&fs, &[], Verification::Done).unwrap();
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
    let err = rec(&fs, &[], Verification::Done).unwrap_err();
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
    let old = rec(&base, &[a], Verification::Done).unwrap();
    let probe = base.copy_disk();
    let new = rec(&probe, &[b], Verification::Unavailable).unwrap();
    assert_ne!(old, new);
    let ops = probe.attempted();
    assert!(ops >= 7, "{ops}"); // check-bound
    // Which the first crash left: the new findings or not.
    let mut sweep = Sweep::with_outcomes();
    let mut retries = Sweep::new();
    // More partial outcomes than usual for the retry: the case that
    // matters is a reused temp name whose unlink is lost but whose rename
    // survives, which few seeds pick.
    let seconds: Vec<CrashOutcome> = CrashOutcome::standard()
        .into_iter()
        .chain((8..64).map(|seed| CrashOutcome::Partial { seed }))
        .collect();
    // Every retry's crash point but its last, under every outcome, worked
    // out from each retry's operations before its sweep runs.
    let mut retry_floor = 0;
    for after in 0..=ops {
        for outcome in CrashOutcome::standard() {
            let run = base.copy_disk();
            run.crash_after(after);
            let _ = rec(&run, &[b], Verification::Unavailable);
            sweep.crash_point(&run);
            let survived = run.crash(outcome);
            let case = format!("after {after}, {outcome:?}");
            sweep.saw(old_or_new(&survived, &old, &new, &case));
            // The retry crashed too, anywhere: still old or new.
            let retry_ops = {
                let probe = survived.copy_disk();
                rec(&probe, &[b], Verification::Unavailable).unwrap();
                probe.attempted()
            };
            retry_floor += retry_ops * seconds.len();
            for again in 0..=retry_ops {
                for &second in &seconds {
                    let run = survived.copy_disk();
                    run.crash_after(again);
                    let _ = rec(&run, &[b], Verification::Unavailable);
                    retries.crash_point(&run);
                    let twice = run.crash(second);
                    let case = format!("{case}, then after {again}, {second:?}");
                    old_or_new(&twice, &old, &new, &case);
                    // An uninterrupted retry finishes the job.
                    let done = rec(&twice, &[b], Verification::Unavailable).unwrap();
                    assert_eq!(done, new, "{case}");
                    assert_eq!(found(&twice).unwrap(), new, "{case}");
                }
            }
        }
    }
    // Not vacuous: the crash cut the write short at every point but the
    // last, under every outcome, and left the old findings and the new.
    sweep.interrupted_at_least(ops * CrashOutcome::standard().len()); // check-bound
    sweep.saw_each([false, true]); // check-bound
    // And so did the second crash, of the retry.
    retries.interrupted_at_least(retry_floor); // check-bound
}

#[test]
fn a_retry_after_any_failed_operation_is_durable_when_it_returns() {
    let a = Finding::new(row(0, 0, 10, 1), Problem::Missing);
    let b = Finding::new(row(1, 20, 30, 2), Problem::HashMismatch);
    let base = FakeFs::with_dirs([dir()]);
    let old = rec(&base, &[a], Verification::Done).unwrap();
    let probe = base.copy_disk();
    let new = rec(&probe, &[b], Verification::Done).unwrap();
    let ops = probe.attempted();
    let mut sweep = Sweep::new();
    for at in 0..ops {
        for outcome in CrashOutcome::standard() {
            let run = base.copy_disk();
            run.fail_after(at, io::ErrorKind::Other);
            let first = rec(&run, &[b], Verification::Done);
            sweep.failure_point(&run);
            assert!(first.is_err(), "failing op {at} went unnoticed");
            // The retry succeeds: then the new findings survive any crash.
            assert_eq!(rec(&run, &[b], Verification::Done).unwrap(), new);
            let survived = run.crash(outcome);
            assert_eq!(
                found(&survived).unwrap(),
                new,
                "failing op {at}, {outcome:?}"
            );
        }
    }
    // Not vacuous: the failure fired at every operation of the write.
    sweep.interrupted_at_least(ops * CrashOutcome::standard().len()); // check-bound
    assert_ne!(old, new);
}
