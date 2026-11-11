//! Every table: a row written and read back, the session it belongs to
//! checked by its foreign key, and, for per-track tables, each row's track
//! kept in a two-track session.

use nota_core::SessionId;
use rusqlite::types::Value;

use crate::Store;
use crate::test_dir::TestDir;
use crate::tests::new_session;

const ONE: SessionId = SessionId::new(1);

fn store(name: &str) -> (TestDir, Store) {
    let dir = TestDir::new(name);
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(ONE)).unwrap();
    (dir, store)
}

/// Every row of `table`, every column, in rowid order.
fn read(store: &Store, table: &str) -> Vec<Vec<Value>> {
    let mut stmt = store
        .conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |r| (0..columns).map(|i| r.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn int(i: i64) -> Value {
    Value::Integer(i)
}

fn text(t: &str) -> Value {
    Value::Text(t.to_owned())
}

/// Inserts `values` into `table`.
fn write(store: &Store, table: &str, columns: &str, values: &[Value]) {
    let marks = vec!["?"; values.len()].join(", ");
    let sql = format!("INSERT INTO {table} ({columns}) VALUES ({marks})");
    store
        .conn
        .execute(&sql, rusqlite::params_from_iter(values))
        .unwrap_or_else(|e| panic!("{table}: {e}"));
}

/// Checks `table` refuses `values`.
fn refused(store: &Store, table: &str, columns: &str, values: &[Value]) {
    let marks = vec!["?"; values.len()].join(", ");
    let sql = format!("INSERT INTO {table} ({columns}) VALUES ({marks})");
    assert!(
        store
            .conn
            .execute(&sql, rusqlite::params_from_iter(values))
            .is_err(),
        "{table} took {values:?}"
    );
}

#[test]
fn the_schema_has_exactly_these_tables() {
    let (_dir, store) = store("tables");
    let mut stmt = store
        .conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        names,
        [
            "epoch",
            "event",
            "final_progress",
            "final_text",
            "final_word",
            "finding",
            "heard_proposal",
            "heard_revision",
            "heard_revision_text",
            "job",
            "learned_pair",
            "mark",
            "note",
            "proposal",
            "proposal_v8",
            "revision",
            "revision_text",
            "segment",
            "session",
            "session_pack",
            "session_packs_chosen",
            "track",
            "utterance",
            "word"
        ]
    );
}

#[test]
fn epochs_keep_their_track() {
    let (_dir, store) = store("epoch");
    let columns = "session_id, track, epoch, first_sample, rate, anchor_ns, drift_ppb";
    let rows = [
        vec![int(1), int(0), int(0), int(0), int(16_000), int(5), int(0)],
        vec![
            int(1),
            int(1),
            int(0),
            int(0),
            int(16_000),
            Value::Null,
            int(-1_000_000),
        ],
        vec![
            int(1),
            int(1),
            int(1),
            int(9_600),
            int(16_000),
            int(900_000_000),
            int(1_000_000),
        ],
    ];
    for row in &rows {
        write(&store, "epoch", columns, row);
    }
    assert_eq!(read(&store, "epoch"), rows);
    // The same track and epoch twice, a rate of zero, another session, a
    // drift past the limit either way.
    refused(&store, "epoch", columns, &rows[0]);
    refused(
        &store,
        "epoch",
        columns,
        &[int(1), int(0), int(4), int(0), int(0), Value::Null, int(0)],
    );
    refused(
        &store,
        "epoch",
        columns,
        &[
            int(2),
            int(0),
            int(0),
            int(0),
            int(16_000),
            Value::Null,
            int(0),
        ],
    );
    for drift in [1_000_001, -1_000_001] {
        refused(
            &store,
            "epoch",
            columns,
            &[
                int(1),
                int(0),
                int(5),
                int(0),
                int(16_000),
                int(0),
                int(drift),
            ],
        );
    }
}

#[test]
fn utterances_and_their_words_keep_their_track() {
    let (_dir, store) = store("utterance");
    let columns = "id, session_id, track, start_ns, end_ns, text, engine, model";
    let rows = [
        vec![
            int(1),
            int(1),
            int(0),
            int(100),
            int(900),
            text("good morning"),
            text("sherpa-onnx"),
            text("parakeet-tdt-0.6b-v3"),
        ],
        vec![
            int(2),
            int(1),
            int(1),
            int(150),
            int(700),
            text("hello"),
            text("sherpa-onnx"),
            text("parakeet-tdt-0.6b-v3"),
        ],
    ];
    for row in &rows {
        write(&store, "utterance", columns, row);
    }
    assert_eq!(read(&store, "utterance"), rows);
    refused(
        &store,
        "utterance",
        columns,
        &[
            int(3),
            int(1),
            int(0),
            int(900),
            int(100),
            text("x"),
            text("e"),
            text("m"),
        ],
    );
    refused(
        &store,
        "utterance",
        columns,
        &[
            int(3),
            int(2),
            int(0),
            int(0),
            int(1),
            text("x"),
            text("e"),
            text("m"),
        ],
    );

    let columns = "utterance_id, position, text, start_ns, end_ns, log_prob";
    // The newest utterance's: its words go in with it (V4), one with a
    // log-probability and one unknown (V11).
    let words = [
        vec![
            int(2),
            int(0),
            text("hello"),
            int(150),
            int(400),
            Value::Real(-0.5),
        ],
        vec![
            int(2),
            int(1),
            text("there"),
            int(450),
            int(700),
            Value::Null,
        ],
    ];
    for word in &words {
        write(&store, "word", columns, word);
    }
    assert_eq!(read(&store, "word"), words);
    refused(
        &store,
        "word",
        columns,
        &[int(9), int(0), text("x"), int(0), int(1), Value::Null],
    );
    // A log-probability is never above zero (V11).
    refused(
        &store,
        "word",
        columns,
        &[
            int(2),
            int(2),
            text("x"),
            int(700),
            int(800),
            Value::Real(0.5),
        ],
    );
}

/// Session 1's final-text chunk at track 0, sample 0, with words 0 and 1,
/// and session 2's at the same place, with word 0; and each session's
/// revision 0.
fn two_sessions_chunks(store: &mut Store) {
    store
        .create_session(&new_session(SessionId::new(2)))
        .unwrap();
    let chunk = "session_id, track, start_sample, end_sample, text, engine, model";
    let word = "session_id, track, start_sample, position, text, word_start, word_end";
    for (session, words) in [(1, 2), (2, 1)] {
        write(
            store,
            "final_text",
            chunk,
            &[
                int(session),
                int(0),
                int(0),
                int(10),
                text("hyper profen"),
                text("e"),
                text("m"),
            ],
        );
        for position in 0..words {
            write(
                store,
                "final_word",
                word,
                &[
                    int(session),
                    int(0),
                    int(0),
                    int(position),
                    text("w"),
                    int(position),
                    int(position + 1),
                ],
            );
        }
        write(
            store,
            "revision",
            "session_id, number, parent",
            &[int(session), int(0), Value::Null],
        );
    }
}

#[test]
fn revisions_chain_and_name_their_final_words() {
    let (_dir, mut store) = store("revision");
    two_sessions_chunks(&mut store);
    let columns = "session_id, number, parent";
    write(&store, "revision", columns, &[int(1), int(1), int(0)]);
    assert_eq!(
        read(&store, "revision"),
        [
            vec![int(1), int(0), Value::Null],
            vec![int(2), int(0), Value::Null],
            vec![int(1), int(1), int(0)]
        ]
    );
    // A parent that isn't earlier, or isn't there.
    refused(&store, "revision", columns, &[int(1), int(2), int(2)]);
    refused(&store, "revision", columns, &[int(1), int(3), int(2)]);
    refused(&store, "revision", columns, &[int(9), int(0), Value::Null]);

    let columns = "session_id, revision, track, start_sample, first_word, last_word, text";
    let change = |session, revision, start, first, last| {
        vec![
            int(session),
            int(revision),
            int(0),
            int(start),
            int(first),
            int(last),
            text("ibuprofen"),
        ]
    };
    write(&store, "revision_text", columns, &change(1, 1, 0, 0, 1));
    assert_eq!(read(&store, "revision_text"), [change(1, 1, 0, 0, 1)]);
    // No such revision; no such chunk; a last word the chunk hasn't; a
    // run that ends before it starts.
    refused(&store, "revision_text", columns, &change(1, 5, 0, 0, 0));
    refused(&store, "revision_text", columns, &change(1, 1, 7, 0, 0));
    write(
        &store,
        "revision",
        "session_id, number, parent",
        &[int(2), int(1), int(0)],
    );
    refused(&store, "revision_text", columns, &change(2, 1, 0, 0, 1));
    // Both of session 1's words are there, so only the order refuses it.
    refused(&store, "revision_text", columns, &change(1, 1, 0, 1, 0));
    // Session 2's chunk at the same place is its own: its one word is
    // there to change, though session 1's chunk has a second.
    write(&store, "revision_text", columns, &change(2, 1, 0, 0, 0));
}

/// A proposal names words its own session's chunk has, where its outcome
/// is and where the lookup offered it, and a revision the session has;
/// its columns hold only the values clean-up writes.
#[test]
fn proposals_name_their_revision_and_final_words() {
    let (_dir, mut store) = store("proposal");
    two_sessions_chunks(&mut store);
    let columns = "id, session_id, revision, track, start_sample, first_word, last_word, \
                   offered_first, offered_last, heard, term, replacement, source, matched, \
                   decider, decider_model, decider_place, answer, outcome, rule, thresholds, \
                   log_version";
    let proposal = |id, session, revision, last| {
        vec![
            int(id),
            int(session),
            int(revision),
            int(0),
            int(0),
            int(0),
            int(last),
            int(0),
            int(last),
            text("hyper profen"),
            text("ibuprofen"),
            text("ibuprofen"),
            text("lookup"),
            text("alias"),
            text("llama-server"),
            text("qwen"),
            text("local"),
            text("no"),
            text("rejected"),
            text("no"),
            text("similarity=0.6"),
            int(1),
        ]
    };
    write(&store, "proposal", columns, &proposal(1, 1, 0, 1));
    let stored = read(&store, "proposal");
    assert_eq!(stored.len(), 1);
    // Words its chunk hasn't: session 2's chunk has only word 0.
    refused(&store, "proposal", columns, &proposal(2, 2, 0, 1));
    // A revision the session hasn't.
    refused(&store, "proposal", columns, &proposal(3, 1, 4, 1));
    // A run that ends before it starts, though both its words are there.
    let mut backwards = proposal(4, 1, 0, 0);
    backwards[5] = int(1);
    refused(&store, "proposal", columns, &backwards);
    // An offered run its chunk hasn't, though the outcome's words are there.
    let mut offered = proposal(5, 2, 0, 0);
    offered[8] = int(1);
    refused(&store, "proposal", columns, &offered);
    // A value clean-up never writes: an unknown outcome, or a rejection
    // with no rule.
    let mut outcome = proposal(6, 1, 0, 1);
    outcome[18] = text("pending");
    refused(&store, "proposal", columns, &outcome);
    let mut rule = proposal(7, 1, 0, 1);
    rule[19] = Value::Null;
    refused(&store, "proposal", columns, &rule);
}

#[test]
fn marks_and_notes_read_back() {
    let (_dir, store) = store("marks");
    let marks = [
        vec![int(1), int(1), int(5_000)],
        vec![int(2), int(1), int(6_000)],
    ];
    for mark in &marks {
        write(&store, "mark", "id, session_id, at_ns", mark);
    }
    assert_eq!(read(&store, "mark"), marks);
    refused(
        &store,
        "mark",
        "id, session_id, at_ns",
        &[int(3), int(2), int(0)],
    );
    refused(
        &store,
        "mark",
        "id, session_id, at_ns",
        &[int(3), int(1), int(-1)],
    );

    let notes = [vec![int(1), int(1), int(7_000), text("ask about the dose")]];
    write(&store, "note", "id, session_id, at_ns, text", &notes[0]);
    assert_eq!(read(&store, "note"), notes);
    refused(
        &store,
        "note",
        "id, session_id, at_ns, text",
        &[int(2), int(2), int(0), text("x")],
    );
}

#[test]
fn jobs_read_back() {
    let (_dir, store) = store("job");
    let columns = "id, session_id, kind, state, attempts, detail, progress, total, waits_for";
    let job = |id, session, kind, attempts, progress, total| {
        vec![
            int(id),
            int(session),
            text(kind),
            text("waiting"),
            int(attempts),
            Value::Null,
            int(progress),
            int(total),
            text("space"),
        ]
    };
    let jobs = [job(1, 1, "final-pass", 0, 5, 10)];
    write(&store, "job", columns, &jobs[0]);
    assert_eq!(read(&store, "job"), jobs);
    for bad in [
        job(2, 1, "k", -1, 0, 0),
        job(2, 2, "k", 0, 0, 0),
        job(2, 1, "k", 0, -1, 0),
        job(2, 1, "k", 0, 0, -1),
        // One job of each kind a session.
        job(2, 1, "final-pass", 0, 0, 0),
    ] {
        refused(&store, "job", columns, &bad);
    }
}

#[test]
fn final_text_keeps_its_track_and_samples() {
    let (_dir, store) = store("final");
    let columns = "session_id, track, start_sample, end_sample, text, engine, model";
    let row = |track, start, end| {
        vec![
            int(1),
            int(track),
            int(start),
            int(end),
            Value::Null,
            text("e"),
            text("m"),
        ]
    };
    write(&store, "final_text", columns, &row(0, 0, 10));
    // A chunk's words go in with it, before the next chunk (V8).
    let words = "session_id, track, start_sample, position, text, word_start, word_end";
    write(
        &store,
        "final_word",
        words,
        &[int(1), int(0), int(0), int(0), text("w"), int(2), int(4)],
    );
    write(&store, "final_text", columns, &row(1, 0, 10));
    for bad in [row(0, 0, 20), row(0, 5, 5), row(0, -1, 2), row(-1, 0, 1)] {
        refused(&store, "final_text", columns, &bad);
    }
    // A log-probability is never above zero (V11).
    refused(
        &store,
        "final_word",
        &format!("{words}, log_prob"),
        &[
            int(1),
            int(0),
            int(0),
            int(1),
            text("w"),
            int(4),
            int(6),
            Value::Real(0.5),
        ],
    );
    for bad in [
        // No such text.
        [int(1), int(0), int(3), int(0), text("w"), int(2), int(4)],
        [int(1), int(0), int(0), int(1), text("w"), int(4), int(2)],
        [int(1), int(0), int(0), int(-1), text("w"), int(2), int(4)],
    ] {
        refused(&store, "final_word", words, &bad);
    }
    let progress = "session_id, track, up_to";
    write(
        &store,
        "final_progress",
        progress,
        &[int(1), int(0), int(10)],
    );
    for bad in [
        [int(1), int(0), int(20)],
        [int(1), int(1), int(-1)],
        [int(9), int(1), int(1)],
    ] {
        refused(&store, "final_progress", progress, &bad);
    }
}

#[test]
fn events_keep_their_track_or_none() {
    let (_dir, store) = store("event");
    let columns = "id, session_id, track, at_ns, kind, detail";
    let events = [
        vec![
            int(1),
            int(1),
            int(0),
            int(10),
            text("device changed"),
            text("USB mic"),
        ],
        vec![int(2), int(1), int(1), int(20), text("gap"), Value::Null],
        vec![
            int(3),
            int(1),
            Value::Null,
            int(30),
            text("disk low"),
            Value::Null,
        ],
    ];
    for event in &events {
        write(&store, "event", columns, event);
    }
    assert_eq!(read(&store, "event"), events);
    refused(
        &store,
        "event",
        columns,
        &[int(4), int(2), int(0), int(0), text("k"), Value::Null],
    );
}

#[test]
fn a_two_track_sessions_segments_keep_their_track() {
    use crate::tests::row;
    let (_dir, mut store) = store("twotrack");
    let rows = [
        row(0, 0, 0, 480, 1),
        row(1, 0, 0, 480, 2),
        row(1, 0, 480, 960, 3),
    ];
    for r in &rows {
        store.insert_segment(ONE, r).unwrap();
    }
    let tracks: Vec<u32> = store
        .segments(ONE)
        .unwrap()
        .iter()
        .map(|r| r.track().get())
        .collect();
    assert_eq!(tracks, [0, 1, 1]);
    assert_eq!(store.segments(ONE).unwrap(), rows);
}
