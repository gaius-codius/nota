use nota_core::{SessionId, SessionTime, TrackId, Utterance};
use rusqlite::Connection;

use super::*;
use crate::test_dir::TestDir;
use crate::tests::{new_session, raw};

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

fn store(name: &str) -> (TestDir, Store) {
    let dir = TestDir::new(name);
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    store.create_session(&new_session(S2)).unwrap();
    (dir, store)
}

/// `text` heard on `track` from `from` to `to` ms, one word per
/// whitespace-separated piece, spread evenly.
fn heard(track: TrackId, from: u64, to: u64, text: &str) -> Heard {
    let pieces: Vec<&str> = text.split_whitespace().collect();
    let n = u64::try_from(pieces.len()).unwrap().max(1);
    let step = (to - from) / n;
    let words = (0..)
        .zip(&pieces)
        .map(|(i, w)| {
            Word::new(
                (*w).to_owned(),
                ms(from + i * step),
                ms(from + (i + 1) * step),
            )
            .unwrap()
        })
        .collect();
    Heard {
        utterance: Utterance::new(track, ms(from), ms(to), text.to_owned()).unwrap(),
        engine: "sherpa-onnx".to_owned(),
        model: "parakeet-tdt-0.6b-v3-int8".to_owned(),
        words,
    }
}

fn texts(lines: &[Line]) -> Vec<(TrackId, &str)> {
    lines.iter().map(|l| (l.track, l.text.as_str())).collect()
}

/// Acceptance (GAI-310): a two-track session's utterances read back in
/// session order, each with its track, its words and what heard it,
/// whatever order they were stored in.
#[test]
fn a_two_track_session_reads_back_in_session_order() {
    let (_dir, mut store) = store("two-tracks");
    let stored = [
        heard(SYSTEM, 2_000, 4_000, "the second slide"),
        heard(MIC, 500, 2_500, "can you hear me"),
        heard(MIC, 4_500, 5_000, "yes"),
        heard(SYSTEM, 600, 1_800, "welcome back"),
    ];
    for h in &stored {
        store.add_utterance(S1, h).unwrap();
    }
    // Another session's text never shows.
    store
        .add_utterance(S2, &heard(MIC, 0, 100, "elsewhere"))
        .unwrap();
    let read = store.utterances(S1).unwrap();
    let order: Vec<_> = read.iter().map(|u| u.heard.clone()).collect();
    assert_eq!(
        order,
        [
            stored[1].clone(),
            stored[3].clone(),
            stored[0].clone(),
            stored[2].clone()
        ]
    );
    assert_eq!(read[0].heard.words.len(), 4);
    assert_eq!(read[0].heard.words[3].text(), "me");
    // Revision 0 is the heard text.
    let shown = store.revision(S1, RevisionNumber::HEARD).unwrap();
    assert_eq!(
        texts(&shown),
        [
            (MIC, "can you hear me"),
            (SYSTEM, "welcome back"),
            (SYSTEM, "the second slide"),
            (MIC, "yes")
        ]
    );
    assert_eq!(shown[1].start, ms(600));
    assert_eq!(shown[1].end, ms(1_800));
}

/// Two utterances at the same moment read by end, then track, then the
/// order they were stored in.
#[test]
fn ties_read_by_end_then_track_then_order_stored() {
    let (_dir, mut store) = store("ties");
    for h in [
        heard(SYSTEM, 0, 1_000, "c"),
        heard(MIC, 0, 1_000, "b"),
        heard(MIC, 0, 500, "a"),
        heard(SYSTEM, 0, 1_000, "d"),
    ] {
        store.add_utterance(S1, &h).unwrap();
    }
    let read: Vec<_> = store
        .utterances(S1)
        .unwrap()
        .into_iter()
        .map(|u| u.heard.utterance.into_text())
        .collect();
    assert_eq!(read, ["a", "b", "c", "d"]);
}

/// A write retried after its answer was lost stores the utterance once.
#[test]
fn the_same_utterance_twice_is_stored_once() {
    let (_dir, mut store) = store("retry");
    let h = heard(MIC, 0, 1_000, "once");
    let first = store.add_utterance(S1, &h).unwrap();
    assert_eq!(store.add_utterance(S1, &h).unwrap(), first);
    assert_eq!(store.utterances(S1).unwrap().len(), 1);
    // The same words by another model are another utterance.
    let mut other = h.clone();
    other.model = "whisper".to_owned();
    assert_ne!(store.add_utterance(S1, &other).unwrap(), first);
    // So is the same text in another session.
    assert_ne!(store.add_utterance(S2, &h).unwrap(), first);
    assert_eq!(store.utterances(S1).unwrap().len(), 2);
}

#[test]
fn text_for_a_session_not_in_the_library_is_refused() {
    let (_dir, mut store) = store("no-session");
    let missing = SessionId::new(9);
    assert!(matches!(
        store.add_utterance(missing, &heard(MIC, 0, 10, "x")),
        Err(StoreError::NoSession(id)) if id == missing
    ));
    let rows: i64 = store
        .conn
        .query_row("SELECT count(*) FROM utterance", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn a_time_past_sqlite_s_integer_is_refused() {
    let (_dir, mut store) = store("range");
    let far = SessionTime::from_nanos(u64::MAX);
    let mut h = heard(MIC, 0, 10, "x");
    h.utterance = Utterance::new(MIC, ms(0), far, "x".to_owned()).unwrap();
    assert!(matches!(
        store.add_utterance(S1, &h),
        Err(StoreError::OutOfRange)
    ));
    let mut h = heard(MIC, 0, 10, "x");
    h.words = vec![Word::new("x".to_owned(), ms(0), far).unwrap()];
    assert!(matches!(
        store.add_utterance(S1, &h),
        Err(StoreError::OutOfRange)
    ));
    assert!(store.utterances(S1).unwrap().is_empty());
}

#[test]
fn a_word_never_ends_before_it_starts() {
    assert!(Word::new("x".to_owned(), ms(5), ms(4)).is_none());
    let w = Word::new("x".to_owned(), ms(5), ms(5)).unwrap();
    assert_eq!((w.text(), w.start(), w.end()), ("x", ms(5), ms(5)));
}

/// Each revision is its parent with its own changes; the heard text and
/// every earlier revision read as they did.
#[test]
fn revisions_build_on_their_parents_and_never_change_them() {
    let (_dir, mut store) = store("revisions");
    let a = store
        .add_utterance(S1, &heard(MIC, 0, 1_000, "the clam"))
        .unwrap();
    let b = store
        .add_utterance(S1, &heard(MIC, 1_000, 2_000, "holds it"))
        .unwrap();
    let one = store
        .add_revision(S1, RevisionNumber::HEARD, &[(a, "the clamp".to_owned())])
        .unwrap();
    assert_eq!(one.get(), 1);
    let two = store
        .add_revision(S1, one, &[(b, "holds them".to_owned())])
        .unwrap();
    assert_eq!(two.get(), 2);
    // From the heard text again: revision 1's change isn't in it.
    let three = store
        .add_revision(S1, RevisionNumber::HEARD, &[(b, "hold it".to_owned())])
        .unwrap();
    assert_eq!(three.get(), 3);
    // A revision that changes a change.
    let four = store
        .add_revision(S1, two, &[(a, "a clamp".to_owned())])
        .unwrap();

    let read = |store: &Store, n| {
        store
            .revision(S1, n)
            .unwrap()
            .into_iter()
            .map(|l| l.text)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        read(&store, RevisionNumber::HEARD),
        ["the clam", "holds it"]
    );
    assert_eq!(read(&store, one), ["the clamp", "holds it"]);
    assert_eq!(read(&store, two), ["the clamp", "holds them"]);
    assert_eq!(read(&store, three), ["the clam", "hold it"]);
    assert_eq!(read(&store, four), ["a clamp", "holds them"]);
    // The heard text is as it was.
    let heard_text: Vec<_> = store
        .utterances(S1)
        .unwrap()
        .into_iter()
        .map(|u| u.heard.utterance.into_text())
        .collect();
    assert_eq!(heard_text, ["the clam", "holds it"]);
    // Text heard after a revision was made shows in it as heard.
    store
        .add_utterance(S1, &heard(MIC, 2_000, 3_000, "later"))
        .unwrap();
    assert_eq!(read(&store, one), ["the clamp", "holds it", "later"]);
}

#[test]
fn a_revision_needs_its_parent_and_its_own_session_s_utterances() {
    let (_dir, mut store) = store("revision-refused");
    // No text yet, so no revision 0.
    assert!(matches!(
        store.revision(S1, RevisionNumber::HEARD),
        Err(StoreError::NoRevision(S1, 0))
    ));
    let ours = store.add_utterance(S1, &heard(MIC, 0, 10, "x")).unwrap();
    let theirs = store.add_utterance(S2, &heard(MIC, 0, 10, "y")).unwrap();
    assert!(matches!(
        store.add_revision(S1, RevisionNumber(5), &[(ours, "z".to_owned())]),
        Err(StoreError::NoRevision(S1, 5))
    ));
    assert!(matches!(
        store.add_revision(S1, RevisionNumber::HEARD, &[(theirs, "z".to_owned())]),
        Err(StoreError::NoUtterance(S1, id)) if id == theirs.get()
    ));
    assert!(matches!(
        store.revision(S1, RevisionNumber(1)),
        Err(StoreError::NoRevision(S1, 1))
    ));
    // Nothing was added by the refusals: the next is still 1.
    let next = store
        .add_revision(S1, RevisionNumber::HEARD, &[(ours, "z".to_owned())])
        .unwrap();
    assert_eq!(next.get(), 1);
    // Two changes to one utterance roll back together.
    assert!(
        store
            .add_revision(
                S1,
                RevisionNumber::HEARD,
                &[(ours, "p".to_owned()), (ours, "q".to_owned())]
            )
            .is_err()
    );
    assert!(matches!(
        store.revision(S1, RevisionNumber(2)),
        Err(StoreError::NoRevision(S1, 2))
    ));
}

/// Whether `sql` is refused on `conn`.
fn refused(conn: &Connection, sql: &str) -> bool {
    match conn.execute_batch(sql) {
        Err(e) => {
            let text = e.to_string();
            assert!(
                text.contains("never"),
                "{sql}: refused, but not by the triggers: {text}"
            );
            true
        }
        Ok(()) => false,
    }
}

/// Acceptance (GAI-310): SQLite itself refuses to change or delete heard
/// text or a revision, from any connection, including an INSERT OR
/// REPLACE, which deletes without firing delete triggers.
#[test]
fn sqlite_refuses_to_change_or_delete_heard_text_or_a_revision() {
    let (dir, mut store) = store("append-only");
    let a = store
        .add_utterance(S1, &heard(MIC, 0, 1_000, "as heard"))
        .unwrap();
    store
        .add_revision(S1, RevisionNumber::HEARD, &[(a, "as shown".to_owned())])
        .unwrap();
    let before = (
        store.utterances(S1).unwrap(),
        store.revision(S1, RevisionNumber(1)).unwrap(),
    );
    drop(store);

    // A plain connection, with none of the store's settings.
    let conn = raw(&dir.db());
    let id = a.get();
    for sql in [
        format!("UPDATE utterance SET text = 'changed' WHERE id = {id}"),
        format!("UPDATE utterance SET start_ns = 7 WHERE id = {id}"),
        format!("DELETE FROM utterance WHERE id = {id}"),
        format!(
            "INSERT OR REPLACE INTO utterance (id, session_id, track, start_ns, end_ns, text, engine, model) \
             VALUES ({id}, 1, 0, 0, 1000000000, 'replaced', 'e', 'm')"
        ),
        format!("UPDATE word SET text = 'changed' WHERE utterance_id = {id}"),
        format!("DELETE FROM word WHERE utterance_id = {id}"),
        format!(
            "INSERT OR REPLACE INTO word (utterance_id, position, text, start_ns, end_ns) \
             VALUES ({id}, 0, 'replaced', 0, 1)"
        ),
        "UPDATE revision SET parent = NULL WHERE number = 1".to_owned(),
        "DELETE FROM revision WHERE number = 1".to_owned(),
        "INSERT OR REPLACE INTO revision (session_id, number, parent) VALUES (1, 1, NULL)"
            .to_owned(),
        "UPDATE revision_text SET text = 'changed'".to_owned(),
        "DELETE FROM revision_text".to_owned(),
        format!(
            "INSERT OR REPLACE INTO revision_text (session_id, revision, utterance_id, text) \
             VALUES (1, 1, {id}, 'replaced')"
        ),
    ] {
        assert!(refused(&conn, &sql), "{sql} was allowed");
    }
    // Nothing is added to what was heard, or to a revision once made.
    store_more(&dir);
    for sql in [
        format!(
            "INSERT INTO word (utterance_id, position, text, start_ns, end_ns) \
             VALUES ({id}, 7, 'added', 0, 1)"
        ),
        format!(
            "INSERT INTO revision_text (session_id, revision, utterance_id, text) \
             VALUES (1, 0, {id}, 'added')"
        ),
        format!(
            "INSERT INTO revision_text (session_id, revision, utterance_id, text) \
             VALUES (1, 1, {id}, 'added')"
        ),
    ] {
        assert!(refused(&conn, &sql), "{sql} was allowed");
    }
    // Recursive triggers on make no difference.
    conn.execute_batch("PRAGMA recursive_triggers = ON")
        .unwrap();
    assert!(refused(
        &conn,
        &format!("DELETE FROM utterance WHERE id = {id}")
    ));
    drop(conn);

    let store = Store::open(&dir.db()).unwrap();
    let mut after_utterances = store.utterances(S1).unwrap();
    after_utterances.truncate(1);
    assert_eq!(before.0, after_utterances);
    let shown = store.revision(S1, RevisionNumber(1)).unwrap();
    assert_eq!(before.1[0], shown[0]);
    assert_eq!(
        store.revision(S1, RevisionNumber(2)).unwrap()[1].text,
        "fixed"
    );
}

/// A later utterance and a revision 2 after it, so revision 1 is no longer
/// the newest and utterance 1 not the newest utterance.
fn store_more(dir: &TestDir) {
    let mut store = Store::open(&dir.db()).unwrap();
    let later = store
        .add_utterance(S1, &heard(MIC, 2_000, 3_000, "later"))
        .unwrap();
    store
        .add_revision(S1, RevisionNumber(1), &[(later, "fixed".to_owned())])
        .unwrap();
}

/// The same utterance with other words is another utterance, not a retry.
#[test]
fn the_same_utterance_with_other_words_is_stored_again() {
    let (_dir, mut store) = store("retry-words");
    let h = heard(MIC, 0, 1_000, "two words");
    let first = store.add_utterance(S1, &h).unwrap();
    let mut other = h.clone();
    other.words.pop();
    let second = store.add_utterance(S1, &other).unwrap();
    assert_ne!(second, first);
    let read = store.utterances(S1).unwrap();
    assert_eq!(read[0].heard, h);
    assert_eq!(read[1].heard, other);
    // An exact retry of either is still the same one.
    assert_eq!(store.add_utterance(S1, &other).unwrap(), second);
}

#[test]
fn a_session_s_revisions_are_listed_in_order() {
    let (_dir, mut store) = store("list-revisions");
    assert!(store.revisions(S1).unwrap().is_empty());
    let a = store.add_utterance(S1, &heard(MIC, 0, 10, "x")).unwrap();
    assert_eq!(store.revisions(S1).unwrap(), [RevisionNumber::HEARD]);
    let one = store
        .add_revision(S1, RevisionNumber::HEARD, &[(a, "y".to_owned())])
        .unwrap();
    store.add_utterance(S2, &heard(MIC, 0, 10, "z")).unwrap();
    assert_eq!(store.revisions(S1).unwrap(), [RevisionNumber::HEARD, one]);
    assert_eq!(store.revisions(S2).unwrap(), [RevisionNumber::HEARD]);
}
