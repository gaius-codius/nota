use nota_core::{LogProb, SessionId, SessionTime, TrackId, Utterance};

use super::*;
use crate::test_dir::TestDir;
use crate::tests::{new_session, raw, refused};

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
    // Since version 8 the heard text is never revised: revisions are of
    // the final pass's text, and this session has none.
    assert!(store.heard_revisions(S1).unwrap().is_empty());
    assert!(store.revisions(S1).unwrap().is_empty());
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

/// Acceptance (GAI-310): SQLite itself refuses to change or delete heard
/// text, from any connection, including an INSERT OR REPLACE, which
/// deletes without firing delete triggers.
#[test]
fn sqlite_refuses_to_change_or_delete_heard_text() {
    let (dir, mut store) = store("append-only");
    let a = store
        .add_utterance(S1, &heard(MIC, 0, 1_000, "as heard"))
        .unwrap();
    let before = store.utterances(S1).unwrap();
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
        // A log-probability goes in with its word and stays as it was.
        format!("UPDATE word SET log_prob = -1.0 WHERE utterance_id = {id}"),
        format!("DELETE FROM word WHERE utterance_id = {id}"),
        format!(
            "INSERT OR REPLACE INTO word (utterance_id, position, text, start_ns, end_ns) \
             VALUES ({id}, 0, 'replaced', 0, 1)"
        ),
        // A stored rowid with a new key (V8): a replace that would delete
        // the word it takes without firing the delete trigger.
        format!(
            "INSERT OR REPLACE INTO word (rowid, utterance_id, position, text, start_ns, end_ns) \
             VALUES (1, {id}, 5, 'replaced', 0, 1)"
        ),
    ] {
        assert!(refused(&conn, &sql), "{sql} was allowed");
    }
    // Nothing is added to what was heard once a later utterance is stored.
    let mut store = Store::open(&dir.db()).unwrap();
    store
        .add_utterance(S1, &heard(MIC, 2_000, 3_000, "later"))
        .unwrap();
    drop(store);
    let added = format!(
        "INSERT INTO word (utterance_id, position, text, start_ns, end_ns) \
         VALUES ({id}, 7, 'added', 0, 1)"
    );
    assert!(refused(&conn, &added), "{added} was allowed");
    // Recursive triggers on make no difference.
    conn.execute_batch("PRAGMA recursive_triggers = ON")
        .unwrap();
    assert!(refused(
        &conn,
        &format!("DELETE FROM utterance WHERE id = {id}")
    ));
    drop(conn);

    let store = Store::open(&dir.db()).unwrap();
    let mut after = store.utterances(S1).unwrap();
    after.truncate(1);
    assert_eq!(before, after);
}

/// Acceptance (#157): the heard text's words read back with the
/// log-probabilities they were heard with, and a word heard without one
/// reads back unknown, not as zero.
#[test]
fn heard_words_read_back_with_their_log_probabilities() {
    let (_dir, mut store) = store("log-probs");
    let mut h = heard(MIC, 0, 3_000, "amylodian is certain");
    let given = [LogProb::new(-1.954), None, Some(LogProb::CERTAIN)];
    h.words = h
        .words
        .into_iter()
        .zip(given)
        .map(|(word, log_prob)| word.with_log_prob(log_prob))
        .collect();
    store.add_utterance(S1, &h).unwrap();
    let read = store.utterances(S1).unwrap();
    let scores: Vec<_> = read[0].heard.words.iter().map(Word::log_prob).collect();
    assert_eq!(scores, given);
    assert_eq!(read[0].heard, h);
}

/// The same words with another log-probability are another utterance,
/// not a retry: the value is part of what was heard.
#[test]
fn the_same_words_with_another_log_probability_are_stored_again() {
    let (_dir, mut store) = store("retry-log-prob");
    let h = heard(MIC, 0, 1_000, "one");
    let first = store.add_utterance(S1, &h).unwrap();
    let mut other = h.clone();
    other.words[0] = other.words[0].clone().with_log_prob(LogProb::new(-0.5));
    assert_ne!(store.add_utterance(S1, &other).unwrap(), first);
    assert_eq!(store.add_utterance(S1, &h).unwrap(), first);
}

/// A stored log-probability no [`LogProb`] could have written is a
/// corrupt row, not unknown; SQLite refuses one above zero outright.
#[test]
fn a_stored_log_probability_that_isnt_one_is_corrupt() {
    let (dir, mut store) = store("bad-log-prob");
    let id = store
        .add_utterance(S1, &heard(MIC, 0, 1_000, "one"))
        .unwrap()
        .get();
    drop(store);
    let conn = raw(&dir.db());
    let above = conn.execute(
        &format!(
            "INSERT INTO word (utterance_id, position, text, start_ns, end_ns, log_prob) \
             VALUES ({id}, 1, 'x', 0, 1, 0.5)"
        ),
        [],
    );
    assert!(
        matches!(&above, Err(e) if e.to_string().contains("CHECK constraint failed")),
        "{above:?}"
    );
    // Not an f32's value: 0.1 has no exact f32.
    conn.execute(
        &format!(
            "INSERT INTO word (utterance_id, position, text, start_ns, end_ns, log_prob) \
             VALUES ({id}, 1, 'x', 0, 1, -0.1)"
        ),
        [],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&dir.db()).unwrap();
    let err = store.utterances(S1).unwrap_err();
    assert!(
        matches!(&err, StoreError::Corrupt(why) if why == "log-probability -0.1 isn't one"),
        "{err:?}"
    );
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

/// A version 7 library holding session 1's heard text, "the clam" then
/// "holds it", with heard revisions 1 to 4 of it, and session 2's,
/// "elsewhere", with only revision 0, as `add_utterance` and
/// `add_revision` made them before version 8.
fn version_7_with_heard_revisions(dir: &TestDir) {
    let conn = crate::tests::library_at(&dir.db(), 7);
    conn.execute_batch(
        "INSERT INTO session (id, state) VALUES (1, 'stopped'), (2, 'stopped');
         INSERT INTO utterance (id, session_id, track, start_ns, end_ns, text, engine, model)
         VALUES (1, 1, 0, 0, 1000000000, 'the clam', 'e', 'm'),
                (2, 1, 1, 1000000000, 2000000000, 'holds it', 'e', 'm'),
                (3, 2, 0, 0, 10, 'elsewhere', 'e', 'm');
         INSERT INTO revision VALUES (1, 0, NULL), (2, 0, NULL), (1, 1, 0);
         INSERT INTO revision_text VALUES (1, 1, 1, 'the clamp');
         INSERT INTO revision VALUES (1, 2, 1);
         INSERT INTO revision_text VALUES (1, 2, 2, 'holds them');
         INSERT INTO revision VALUES (1, 3, 0);
         INSERT INTO revision_text VALUES (1, 3, 2, 'hold it');
         INSERT INTO revision VALUES (1, 4, 2);
         INSERT INTO revision_text VALUES (1, 4, 1, 'a clamp');",
    )
    .unwrap();
}

/// Heard revisions made before version 8 read as they did: each is its
/// parent with its own changes, back to the heard text, which is as heard.
#[test]
fn heard_revisions_from_before_version_8_read_as_they_did() {
    let dir = TestDir::new("heard-revisions");
    version_7_with_heard_revisions(&dir);
    let store = Store::open(&dir.db()).unwrap();
    let read = |n| {
        let lines = store.heard_revision(S1, HeardRevision(n)).unwrap();
        texts(&lines)
            .into_iter()
            .map(|(_, text)| text.to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(read(0), ["the clam", "holds it"]);
    assert_eq!(read(1), ["the clamp", "holds it"]);
    assert_eq!(read(2), ["the clamp", "holds them"]);
    // From the heard text again: revision 1's change isn't in it.
    assert_eq!(read(3), ["the clam", "hold it"]);
    // A change to a change: the nearest revision's text wins.
    assert_eq!(read(4), ["a clamp", "holds them"]);
    let lines = store.heard_revision(S1, HeardRevision(4)).unwrap();
    assert_eq!(
        (lines[1].track, lines[1].start, lines[1].end),
        (SYSTEM, ms(1_000), ms(2_000))
    );
    assert_eq!(
        store.heard_revisions(S1).unwrap(),
        (0..=4).map(HeardRevision).collect::<Vec<_>>()
    );
    assert_eq!(store.heard_revisions(S2).unwrap(), [HeardRevision::HEARD]);
    assert!(matches!(
        store.heard_revision(S1, HeardRevision(5)),
        Err(StoreError::NoRevision(S1, 5))
    ));
    // None of them is a revision of the final text.
    assert!(store.revisions(S1).unwrap().is_empty());
}
