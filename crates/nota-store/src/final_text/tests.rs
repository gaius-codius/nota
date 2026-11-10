use nota_core::{SampleIndex, SampleRange, SessionId, TrackId};

use super::*;
use crate::test_dir::TestDir;
use crate::tests::{new_session, refused};

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

fn store(name: &str) -> (TestDir, Store) {
    let dir = TestDir::new(name);
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    store.create_session(&new_session(S2)).unwrap();
    (dir, store)
}

fn s(n: u64) -> SampleIndex {
    SampleIndex::new(n)
}

fn range(a: u64, b: u64) -> SampleRange {
    SampleRange::new(s(a), s(b)).unwrap()
}

fn text(track: TrackId, a: u64, b: u64, heard: Option<&str>) -> FinalText {
    FinalText {
        track,
        range: range(a, b),
        text: heard.map(str::to_owned),
        words: Vec::new(),
        heard_by: HeardBy {
            engine: "sherpa-onnx".to_owned(),
            model: "parakeet".to_owned(),
        },
    }
}

/// Text and progress go in together and read back by track and sample;
/// another session's are its own.
#[test]
fn final_text_reads_back_by_track_and_sample_with_its_progress() {
    let (_dir, mut store) = store("read-back");
    let mut worded = text(MIC, 0, 400, Some("good morning"));
    worded.words = vec![
        FinalWord {
            text: "good".into(),
            range: range(10, 100),
        },
        FinalWord {
            text: "morning".into(),
            range: range(120, 380),
        },
    ];
    store
        .add_final_text(S1, SYSTEM, s(300), &[text(SYSTEM, 0, 300, None)])
        .unwrap();
    store
        .add_final_text(
            S1,
            MIC,
            s(900),
            &[worded.clone(), text(MIC, 500, 900, Some("today"))],
        )
        .unwrap();
    store.add_final_text(S2, MIC, s(50), &[]).unwrap();
    assert_eq!(
        store.final_texts(S1).unwrap(),
        [
            worded,
            text(MIC, 500, 900, Some("today")),
            text(SYSTEM, 0, 300, None)
        ]
    );
    assert_eq!(
        store
            .final_progress(S1)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [(MIC, s(900)), (SYSTEM, s(300))]
    );
    assert!(store.final_texts(S2).unwrap().is_empty());
    assert_eq!(
        store
            .final_progress(S2)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [(MIC, s(50))]
    );
}

/// The same text stored twice (an answer lost, the write retried) is
/// stored once; progress never goes back.
#[test]
fn a_retried_write_stores_nothing_twice_and_progress_never_goes_back() {
    let (_dir, mut store) = store("retry");
    let first = [text(MIC, 0, 100, Some("one"))];
    store.add_final_text(S1, MIC, s(100), &first).unwrap();
    store.add_final_text(S1, MIC, s(100), &first).unwrap();
    store.add_final_text(S1, MIC, s(40), &[]).unwrap();
    assert_eq!(store.final_texts(S1).unwrap(), first);
    assert_eq!(store.final_progress(S1).unwrap()[&MIC], s(100));
}

/// Text that would cover a sample twice, sits on another track, or ends
/// past its progress is refused, and nothing of its call is stored.
#[test]
fn text_that_overlaps_or_runs_past_its_progress_is_refused() {
    let (_dir, mut store) = store("overlap");
    store
        .add_final_text(S1, MIC, s(100), &[text(MIC, 0, 100, Some("one"))])
        .unwrap();
    for (up_to, given) in [
        // The same samples, other text.
        (100, vec![text(MIC, 0, 100, Some("won"))]),
        // Overlapping its end.
        (200, vec![text(MIC, 50, 200, Some("two"))]),
        // Inside it.
        (100, vec![text(MIC, 10, 20, None)]),
        // Two of its own that overlap, after one that's fine.
        (
            400,
            vec![
                text(MIC, 100, 300, Some("a")),
                text(MIC, 250, 400, Some("b")),
            ],
        ),
        // Past the progress given with it.
        (150, vec![text(MIC, 100, 200, Some("c"))]),
        // On another track.
        (300, vec![text(SYSTEM, 100, 200, Some("d"))]),
    ] {
        assert!(
            matches!(
                store.add_final_text(S1, MIC, s(up_to), &given),
                Err(StoreError::FinalOverlap { track: MIC, .. })
            ),
            "{given:?}"
        );
    }
    assert_eq!(
        store.final_texts(S1).unwrap(),
        [text(MIC, 0, 100, Some("one"))]
    );
    assert_eq!(store.final_progress(S1).unwrap()[&MIC], s(100));
    assert!(matches!(
        store.add_final_text(SessionId::new(7), MIC, s(1), &[]),
        Err(StoreError::NoSession(_))
    ));
}

/// Text stored on the live path and the final pass's never meet: the
/// heard text shows none of the final pass's, and the final pass's makes
/// a revision of its own, not a heard one.
#[test]
fn the_final_pass_never_shows_in_the_heard_text() {
    let (_dir, mut store) = store("apart");
    store
        .add_final_text(S1, MIC, s(100), &[text(MIC, 0, 100, Some("final"))])
        .unwrap();
    assert!(store.utterances(S1).unwrap().is_empty());
    assert!(store.heard_revisions(S1).unwrap().is_empty());
    assert_eq!(store.revisions(S1).unwrap(), [crate::RevisionNumber::FINAL]);
}

/// Acceptance: SQLite itself refuses to change, delete or replace final
/// text or its words, from any connection, including an INSERT OR
/// REPLACE, which deletes without firing delete triggers; and to add a
/// word to a chunk stored before the newest.
#[test]
fn sqlite_refuses_to_change_or_delete_final_text() {
    let (dir, mut store) = store("final-append-only");
    let mut worded = text(MIC, 0, 100, Some("good morning"));
    worded.words = vec![
        FinalWord {
            text: "good".into(),
            range: range(0, 40),
        },
        FinalWord {
            text: "morning".into(),
            range: range(40, 100),
        },
    ];
    let later = text(SYSTEM, 0, 50, Some("later"));
    store
        .add_final_text(S1, MIC, s(100), std::slice::from_ref(&worded))
        .unwrap();
    store
        .add_final_text(S1, SYSTEM, s(50), std::slice::from_ref(&later))
        .unwrap();
    drop(store);

    // A plain connection, with none of the store's settings.
    let conn = crate::tests::raw(&dir.db());
    for sql in [
        "UPDATE final_text SET text = 'changed' WHERE track = 0",
        "UPDATE final_text SET end_sample = 90 WHERE track = 0",
        "DELETE FROM final_text WHERE track = 0",
        "INSERT OR REPLACE INTO final_text \
         (session_id, track, start_sample, end_sample, text, engine, model) \
         VALUES (1, 0, 0, 100, 'replaced', 'e', 'm')",
        "UPDATE final_word SET text = 'changed' WHERE track = 0",
        "DELETE FROM final_word WHERE track = 0",
        "INSERT OR REPLACE INTO final_word \
         (session_id, track, start_sample, position, text, word_start, word_end) \
         VALUES (1, 0, 0, 0, 'replaced', 0, 40)",
        // A word added to the microphone's chunk, which isn't the newest.
        "INSERT INTO final_word \
         (session_id, track, start_sample, position, text, word_start, word_end) \
         VALUES (1, 0, 0, 2, 'added', 90, 100)",
        // A word added to the newest chunk, which its progress has passed.
        "INSERT INTO final_word \
         (session_id, track, start_sample, position, text, word_start, word_end) \
         VALUES (1, 1, 0, 0, 'added', 0, 10)",
        // A stored rowid with a new key: a replace that would delete the
        // row it takes without firing the delete triggers.
        "INSERT OR REPLACE INTO final_text \
         (rowid, session_id, track, start_sample, end_sample, text, engine, model) \
         VALUES (1, 1, 0, 500, 600, 'replaced', 'e', 'm')",
        "INSERT OR REPLACE INTO final_word \
         (rowid, session_id, track, start_sample, position, text, word_start, word_end) \
         VALUES (1, 1, 1, 0, 0, 'replaced', 0, 10)",
    ] {
        assert!(refused(&conn, sql), "{sql} was allowed");
    }
    // Recursive triggers on make no difference.
    conn.execute_batch("PRAGMA recursive_triggers = ON")
        .unwrap();
    assert!(refused(&conn, "DELETE FROM final_text"));
    drop(conn);

    let store = Store::open(&dir.db()).unwrap();
    assert_eq!(store.final_texts(S1).unwrap(), [worded, later]);
}
