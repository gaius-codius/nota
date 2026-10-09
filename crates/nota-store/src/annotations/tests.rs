use nota_core::recorder::{Mark, Note};
use nota_core::{SessionId, SessionTime};

use super::*;
use crate::test_dir::TestDir;
use crate::tests::new_session;

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

fn mark(at: u64) -> Annotation {
    Annotation::Mark(Mark { at: ms(at) })
}

fn note(at: u64, text: &str) -> Annotation {
    Annotation::Note(Note::new(ms(at), text).unwrap())
}

/// Marks and notes read back in session order, each with its moment,
/// whatever order they were stored in; a mark and a note at one moment
/// read mark first; another session's never show.
#[test]
fn marks_and_notes_read_back_in_session_order() {
    let dir = TestDir::new("annotations");
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    store.create_session(&new_session(S2)).unwrap();
    let made = [
        note(9_000, "ask about the dose"),
        mark(1_500),
        note(1_500, "same moment"),
        mark(1_500),
        mark(400),
    ];
    for a in &made {
        store.add_annotation(S1, a).unwrap();
    }
    store.add_annotation(S2, &mark(1)).unwrap();
    assert_eq!(
        store.annotations(S1).unwrap(),
        [
            mark(400),
            mark(1_500),
            mark(1_500),
            note(1_500, "same moment"),
            note(9_000, "ask about the dose"),
        ]
    );
    assert_eq!(store.annotations(S2).unwrap(), [mark(1)]);
    assert_eq!(made[0].at(), ms(9_000));
}

#[test]
fn an_annotation_for_a_session_not_in_the_library_is_refused() {
    let dir = TestDir::new("annotations-refused");
    let mut store = Store::open(&dir.db()).unwrap();
    let missing = SessionId::new(4);
    assert!(matches!(
        store.add_annotation(missing, &mark(1)),
        Err(StoreError::NoSession(id)) if id == missing
    ));
    store.create_session(&new_session(S1)).unwrap();
    let far = Annotation::Mark(Mark {
        at: SessionTime::from_nanos(u64::MAX),
    });
    assert!(matches!(
        store.add_annotation(S1, &far),
        Err(StoreError::OutOfRange)
    ));
    assert!(store.annotations(S1).unwrap().is_empty());
}

/// A stored row that can't be a mark or a note is reported, not misread.
#[test]
fn a_bad_stored_annotation_is_corrupt() {
    let dir = TestDir::new("annotations-corrupt");
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO note (session_id, at_ns, text) VALUES (1, 5, '  ')",
            [],
        )
        .unwrap();
    assert!(matches!(store.annotations(S1), Err(StoreError::Corrupt(_))));
}
