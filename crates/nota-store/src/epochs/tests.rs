use nota_core::{
    Drift, EpochAnchor, EpochId, SampleIndex, SampleRate, SessionId, SessionTime, TrackId,
};
use rusqlite::params;

use crate::test_dir::TestDir;
use crate::tests::new_session;
use crate::{Inserted, Store, StoreError};

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

/// A store with sessions 1 and 2 in it.
fn open(dir: &TestDir) -> Store {
    let mut store = Store::open(&dir.db()).unwrap();
    for id in [S1, S2] {
        if store.session(id).unwrap().is_none() {
            store.create_session(&new_session(id)).unwrap();
        }
    }
    store
}

/// Epoch `id` from sample `first` at 16 kHz, starting `start` ns in.
fn anchor(id: u32, first: u64, start: u64) -> EpochAnchor {
    EpochAnchor {
        id: EpochId::new(id),
        start: SessionTime::from_nanos(start),
        first_sample: SampleIndex::new(first),
        rate: SampleRate::SPEECH,
        drift: Drift::ZERO,
    }
}

/// Anchors read back by track then epoch, each session its own, and they
/// last across reopening.
#[test]
fn anchors_read_back_in_order_and_by_session() {
    let dir = TestDir::new("epochs-order");
    {
        let mut store = open(&dir);
        for (session, track, a) in [
            (S1, SYSTEM, anchor(0, 0, 5)),
            (S1, MIC, anchor(2, 900, 3_000)),
            (S1, MIC, anchor(0, 0, 0)),
            (S2, MIC, anchor(0, 7, 1)),
        ] {
            assert_eq!(
                store.insert_epoch(session, track, &a).unwrap(),
                Inserted::New
            );
        }
    }
    let store = open(&dir);
    assert_eq!(
        store.epochs(S1).unwrap(),
        [
            (MIC, anchor(0, 0, 0)),
            (MIC, anchor(2, 900, 3_000)),
            (SYSTEM, anchor(0, 0, 5))
        ]
    );
    assert_eq!(store.epochs(S2).unwrap(), [(MIC, anchor(0, 7, 1))]);
    assert_eq!(store.epochs(SessionId::new(9)).unwrap(), []);
}

/// The same anchor again is already there; another for the same epoch is
/// refused, and the first stands.
#[test]
fn an_epoch_s_anchor_never_changes() {
    let dir = TestDir::new("epochs-conflict");
    let mut store = open(&dir);
    store.insert_epoch(S1, MIC, &anchor(1, 10, 20)).unwrap();
    assert_eq!(
        store.insert_epoch(S1, MIC, &anchor(1, 10, 20)).unwrap(),
        Inserted::AlreadyPresent
    );
    for other in [anchor(1, 11, 20), anchor(1, 10, 21)] {
        assert!(matches!(
            store.insert_epoch(S1, MIC, &other),
            Err(StoreError::EpochConflict { track, epoch }) if track == MIC && epoch == EpochId::new(1)
        ));
    }
    // Another rate is another anchor too.
    let faster = EpochAnchor {
        rate: SampleRate::new(48_000).unwrap(),
        drift: Drift::ZERO,
        ..anchor(1, 10, 20)
    };
    assert!(store.insert_epoch(S1, MIC, &faster).is_err());
    assert_eq!(store.epochs(S1).unwrap(), [(MIC, anchor(1, 10, 20))]);
}

/// An epoch of a session the library doesn't hold is refused.
#[test]
fn an_epoch_of_a_session_not_in_the_library_is_refused() {
    let dir = TestDir::new("epochs-nosession");
    let mut store = open(&dir);
    let missing = SessionId::new(9);
    assert!(matches!(
        store.insert_epoch(missing, MIC, &anchor(0, 0, 0)),
        Err(StoreError::NoSession(id)) if id == missing
    ));
}

/// Numbers SQLite's integer can't hold are refused before anything is
/// written.
#[test]
fn anchors_beyond_i64_are_out_of_range() {
    let dir = TestDir::new("epochs-range");
    let mut store = open(&dir);
    for a in [anchor(0, u64::MAX, 0), anchor(0, 0, u64::MAX)] {
        assert!(matches!(
            store.insert_epoch(S1, MIC, &a),
            Err(StoreError::OutOfRange)
        ));
    }
    assert!(matches!(
        store.epochs(SessionId::new(u64::MAX)),
        Err(StoreError::OutOfRange)
    ));
    assert_eq!(store.epochs(S1).unwrap(), []);
}

/// A row with no anchor isn't a timed epoch, and a row that doesn't parse
/// is reported, not skipped.
#[test]
fn untimed_rows_are_left_out_and_bad_ones_reported() {
    let dir = TestDir::new("epochs-raw");
    let mut store = open(&dir);
    store.insert_epoch(S1, MIC, &anchor(1, 10, 20)).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO epoch (session_id, track, epoch, first_sample, rate, anchor_ns) \
             VALUES (1, 0, 0, 0, 16000, NULL)",
            [],
        )
        .unwrap();
    assert_eq!(store.epochs(S1).unwrap(), [(MIC, anchor(1, 10, 20))]);
    let cases: [(i64, i64, i64, i64, i64, &str); 5] = [
        (-1, 5, 0, 16_000, 0, "track"),
        (0, -1, 0, 16_000, 0, "epoch"),
        (0, 5, -1, 16_000, 0, "first sample"),
        (0, 5, 0, 0, 0, "rate"),
        (0, 5, 0, 16_000, -1, "anchor"),
    ];
    for (i, (track, epoch, first, rate, start, what)) in cases.into_iter().enumerate() {
        let dir = TestDir::new(&format!("epochs-bad{i}"));
        let store = open(&dir);
        // Past the table's checks, as a damaged file could be.
        store
            .conn
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO epoch (session_id, track, epoch, first_sample, rate, anchor_ns) \
                 VALUES (2, ?1, ?2, ?3, ?4, ?5)",
                params![track, epoch, first, rate, start],
            )
            .unwrap();
        let read = store.epochs(S2);
        assert!(
            matches!(&read, Err(StoreError::Corrupt(why)) if why.contains(what)),
            "{what}: {read:?}"
        );
    }
    store
        .conn
        .execute_batch("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    // A stored row that doesn't parse can't be compared, so an insert
    // reports it too.
    store
        .conn
        .execute(
            "INSERT INTO epoch (session_id, track, epoch, first_sample, rate, anchor_ns) \
             VALUES (2, 0, 3, -1, 16000, 0)",
            [],
        )
        .unwrap();
    assert!(matches!(
        store.insert_epoch(S2, MIC, &anchor(3, 0, 0)),
        Err(StoreError::Corrupt(_))
    ));
}

/// The conflict says which epoch of which track.
#[test]
fn the_conflict_names_its_epoch() {
    let text = StoreError::EpochConflict {
        track: SYSTEM,
        epoch: EpochId::new(4),
    }
    .to_string();
    assert_eq!(text, "epoch 4 of track 1 is stored with another anchor");
}
