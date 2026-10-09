use nota_core::SessionId;

use super::*;
use crate::test_dir::TestDir;
use crate::tests::{new_session, row};

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);

fn open(dir: &TestDir) -> Store {
    let mut store = Store::open(&dir.db()).unwrap();
    for id in [S1, S2] {
        store.create_session(&new_session(id)).unwrap();
    }
    store
}

/// One of each kind of entry, with every problem and status.
fn every_kind() -> Vec<IndexedFinding> {
    let problems = [
        Problem::Missing,
        Problem::HashMismatch,
        Problem::LengthMismatch,
        Problem::Unreadable(ReadFailure::PermissionDenied),
        Problem::Unreadable(ReadFailure::IsADirectory),
        Problem::Unreadable(ReadFailure::Other),
    ];
    let statuses = [Status::Unresolved, Status::SinceVerified, Status::Repaired];
    let mut out: Vec<IndexedFinding> = problems
        .into_iter()
        .enumerate()
        .map(|(i, problem)| {
            let start = 100 * i as u64;
            let mut r = row(1, 2, start, start + 50, 3);
            if i % 2 == 0 {
                r = r.with_audio(AudioDigest::new([4; 32]));
            }
            IndexedFinding::Row {
                row: r,
                problem,
                status: statuses[i % 3],
            }
        })
        .collect();
    out.push(IndexedFinding::Unparsable {
        key: RowKey {
            track: -1,
            start: -7,
        },
        status: Status::Unresolved,
    });
    out.push(IndexedFinding::Unparsable {
        key: RowKey {
            track: 3,
            start: 1 << 40,
        },
        status: Status::SinceVerified,
    });
    out
}

fn sorted(mut findings: Vec<IndexedFinding>) -> Vec<IndexedFinding> {
    let key = |f: &IndexedFinding| match f {
        IndexedFinding::Row { row, problem, .. } => (
            i64::from(row.track().get()),
            i64::try_from(row.range().start().get()).unwrap(),
            Some(i64::try_from(row.range().end().get()).unwrap()),
            problem_text(*problem),
        ),
        IndexedFinding::Unparsable { key, .. } => (key.track, key.start, None, UNPARSABLE),
    };
    findings.sort_by_key(|f| key(f));
    findings
}

#[test]
fn a_sessions_findings_round_trip_and_replace_the_old() {
    let dir = TestDir::new("findings-round-trip");
    let mut store = open(&dir);
    let all = every_kind();
    store.index_findings(S1, &all).unwrap();
    assert_eq!(store.findings(S1).unwrap(), sorted(all.clone()));
    assert_eq!(store.findings(S2).unwrap(), vec![]);
    // Reopened, the same.
    drop(store);
    let mut store = Store::open(&dir.db()).unwrap();
    assert_eq!(store.findings(S1).unwrap(), sorted(all.clone()));

    // Indexing again replaces, never adds.
    let fewer = all[..2].to_vec();
    store.index_findings(S1, &fewer).unwrap();
    assert_eq!(store.findings(S1).unwrap(), sorted(fewer));
    store.index_findings(S1, &[]).unwrap();
    assert_eq!(store.findings(S1).unwrap(), vec![]);
}

#[test]
fn unresolved_findings_are_counted_per_session() {
    let dir = TestDir::new("findings-count");
    let mut store = open(&dir);
    assert!(store.unresolved_findings().unwrap().is_empty());
    let all = every_kind();
    let unresolved = all
        .iter()
        .filter(|f| f.status() == Status::Unresolved)
        .count();
    assert_eq!(unresolved, 3);
    store.index_findings(S1, &all).unwrap();
    store.index_findings(S2, &all[1..2]).unwrap();
    let counts = store.unresolved_findings().unwrap();
    // S2's only entry is since verified: not counted.
    assert_eq!(counts.into_iter().collect::<Vec<_>>(), vec![(S1, 3)]);
}

#[test]
fn a_session_not_in_the_library_is_refused_and_nothing_changes() {
    let dir = TestDir::new("findings-no-session");
    let mut store = open(&dir);
    store.index_findings(S1, &every_kind()).unwrap();
    assert!(matches!(
        store.index_findings(SessionId::new(9), &every_kind()),
        Err(StoreError::NoSession(id)) if id == SessionId::new(9)
    ));
    // A row past SQLite's integer fails the whole replacement.
    let huge = IndexedFinding::Row {
        row: row(1, 0, 0, u64::MAX, 1),
        problem: Problem::Missing,
        status: Status::Unresolved,
    };
    assert!(matches!(
        store.index_findings(S1, &[huge]),
        Err(StoreError::OutOfRange)
    ));
    assert_eq!(store.findings(S1).unwrap(), sorted(every_kind()));
}

#[test]
fn entries_that_dont_parse_are_corrupt() {
    for (i, set) in [
        "status = 'later'",
        "problem = 'gone'",
        "end_sample = NULL",
        "end_sample = 0",
        "epoch = -1",
        "sha256 = zeroblob(3)",
        "audio_digest = zeroblob(3)",
        "track = -1",
    ]
    .into_iter()
    .enumerate()
    {
        let dir = TestDir::new(&format!("findings-corrupt-{i}"));
        let mut store = open(&dir);
        store.index_findings(S1, &every_kind()[..1]).unwrap();
        store
            .conn
            .execute(&format!("UPDATE finding SET {set}"), [])
            .unwrap();
        assert!(
            matches!(store.findings(S1), Err(StoreError::Corrupt(_))),
            "{set}"
        );
    }
}
