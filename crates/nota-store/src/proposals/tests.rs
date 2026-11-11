use nota_core::decide::{Place, Probability};
use nota_core::{LogProb, SampleIndex, SampleRange, SessionId, TrackId};

use super::*;
use crate::test_dir::TestDir;
use crate::tests::{new_session, raw, refused};
use crate::{FinalText, FinalWord, HeardBy};

const S1: SessionId = SessionId::new(1);
const S2: SessionId = SessionId::new(2);
const MIC: TrackId = TrackId::new(0);

/// A library with sessions 1 and 2, and session 1's final text: one chunk
/// on the microphone, "start amylo diping daily".
fn store(name: &str) -> (TestDir, Store, ChunkKey) {
    let dir = TestDir::new(name);
    let mut store = Store::open(&dir.db()).unwrap();
    store.create_session(&new_session(S1)).unwrap();
    store.create_session(&new_session(S2)).unwrap();
    let words = ["start", "amylo", "diping", "daily"];
    let text = FinalText {
        track: MIC,
        range: SampleRange::new(SampleIndex::new(0), SampleIndex::new(400)).unwrap(),
        text: Some(words.join(" ")),
        words: (0_u64..)
            .zip(words)
            .map(|(i, w)| FinalWord {
                text: w.to_owned(),
                range: SampleRange::new(SampleIndex::new(i * 100), SampleIndex::new(i * 100 + 100))
                    .unwrap(),
                log_prob: LogProb::new(-0.5),
            })
            .collect(),
        heard_by: HeardBy {
            engine: "sherpa-onnx".to_owned(),
            model: "parakeet".to_owned(),
        },
    };
    store
        .add_final_text(S1, MIC, SampleIndex::new(400), std::slice::from_ref(&text))
        .unwrap();
    (dir, store, text.key())
}

fn span(first: u32, last: u32) -> WordSpan {
    WordSpan::new(first, last).unwrap()
}

/// A proposal of `term` for "amylo diping" (words 1 to 2), from a pack,
/// scored, decided as `decided`.
fn proposal(chunk: ChunkKey, term: &str, decided: Decided) -> NewProposal {
    NewProposal {
        proposed: Proposed {
            chunk,
            words: span(1, 2),
            offered: span(1, 2),
            heard: "amylo diping".to_owned(),
            term: term.to_owned(),
            replacement: term.to_owned(),
            origin: Origin::Pack {
                name: "pharmacy".to_owned(),
                version: "1".to_owned(),
                digest: PackDigest::from_stored(&[7; 32]).unwrap(),
            },
            found: Found::Scored {
                score: 0.75,
                sounds: 1.0,
                trigrams: 0.25,
                jaro_winkler: 0.75,
                rare: false,
            },
            decided_by: DecidedBy {
                name: "llama-server".to_owned(),
                model: "qwen".to_owned(),
                place: Place::Local,
            },
            log_prob: LogProb::new(-0.5),
            thresholds: "similarity=0.6".to_owned(),
        },
        decided,
    }
}

fn yes(p: f64) -> Option<Probability> {
    Probability::new(p)
}

/// Every kind of decision, so each is stored and read back.
fn every_decision(chunk: ChunkKey) -> Vec<NewProposal> {
    let mut learned = proposal(chunk, "amlodipine", Decided::No { yes: None });
    learned.proposed.origin = Origin::Learned;
    learned.proposed.found = Found::Alias;
    learned.proposed.log_prob = None;
    vec![
        proposal(
            chunk,
            "amlodipine",
            Decided::Applied {
                yes: yes(0.9),
                similarity: 0.85,
            },
        ),
        proposal(
            chunk,
            "amiloride",
            Decided::Refused {
                yes: yes(0.6),
                rule: Refusal::Dissimilar,
                similarity: Some(0.4),
            },
        ),
        proposal(
            chunk,
            "amlodipin",
            Decided::Refused {
                yes: None,
                rule: Refusal::SpanNotFound,
                similarity: None,
            },
        ),
        proposal(
            chunk,
            "amylo diping",
            Decided::Unchanged {
                yes: None,
                similarity: 1.0,
            },
        ),
        proposal(
            chunk,
            "amylase",
            Decided::Covered {
                yes: None,
                similarity: 0.7,
            },
        ),
        proposal(chunk, "ampicillin", Decided::No { yes: yes(0.1) }),
        proposal(
            chunk,
            "amoxicillin",
            Decided::NoAnswer("cut short".to_owned()),
        ),
        learned,
    ]
}

/// Every proposal reads back as stored, numbered in order, with the
/// outcome its decision makes: applied, to rank, or rejected.
#[test]
fn proposals_read_back_as_stored() {
    let (_dir, mut store, chunk) = store("proposals-read-back");
    let stored = every_decision(chunk);
    store.add_proposals(S1, &stored).unwrap();
    let read = store.proposals(S1).unwrap();
    let back: Vec<NewProposal> = read
        .iter()
        .map(|p| NewProposal {
            proposed: p.proposed.clone(),
            decided: p.decided.clone(),
        })
        .collect();
    assert_eq!(back, stored);
    let outcomes: Vec<Outcome> = read.iter().map(|p| p.outcome).collect();
    assert_eq!(
        outcomes,
        [
            Outcome::Applied(None),
            Outcome::ToRank,
            Outcome::ToRank,
            Outcome::Rejected,
            Outcome::Rejected,
            Outcome::Rejected,
            Outcome::Rejected,
            Outcome::Rejected,
        ]
    );
    let ids: Vec<i64> = read.iter().map(|p| p.id.get()).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    assert!(read.iter().all(|p| p.review.is_none()));
    // Another session's proposals are its own.
    assert_eq!(store.proposals(S2).unwrap(), []);
}

/// A chunk's proposals are stored together or not at all: one naming a
/// word the chunk doesn't have stores none of them.
#[test]
fn a_proposal_naming_missing_words_stores_none() {
    let (_dir, mut store, chunk) = store("proposals-missing-words");
    let good = proposal(chunk, "amlodipine", Decided::No { yes: None });
    let mut bad = good.clone();
    bad.proposed.offered = span(3, 4);
    let err = store.add_proposals(S1, &[good.clone(), bad]).unwrap_err();
    assert!(
        matches!(err, StoreError::NoWords { words, .. } if words == span(3, 4)),
        "{err:?}"
    );
    // And the words the outcome names are checked too.
    let mut bad = good.clone();
    bad.proposed.words = span(0, 9);
    let err = store.add_proposals(S1, &[good, bad]).unwrap_err();
    assert!(
        matches!(err, StoreError::NoWords { words, .. } if words == span(0, 9)),
        "{err:?}"
    );
    assert_eq!(store.proposals(S1).unwrap(), []);
}

/// The run's end ranks the yeses the gate refused and writes the applied
/// changes as one new revision on the newest, which applies them.
#[test]
fn placing_ranks_cards_and_writes_one_revision() {
    let (_dir, mut store, chunk) = store("proposals-placing");
    store.add_proposals(S1, &every_decision(chunk)).unwrap();
    let read = store.proposals(S1).unwrap();
    let placing = Placing {
        cards: vec![read[1].id],
        over_cap: vec![read[2].id],
        changes: vec![Change {
            chunk,
            words: span(1, 2),
            text: "amlodipine".to_owned(),
        }],
        applied: vec![read[0].id],
    };
    let made = store.place_proposals(S1, &placing).unwrap();
    assert_eq!(made, Some(RevisionNumber::stored(1)));
    let read = store.proposals(S1).unwrap();
    let outcomes: Vec<Outcome> = read.iter().take(3).map(|p| p.outcome).collect();
    assert_eq!(
        outcomes,
        [
            Outcome::Applied(Some(RevisionNumber::stored(1))),
            Outcome::Card,
            Outcome::OverCap
        ]
    );
    let shown = store.revision(S1, RevisionNumber::stored(1)).unwrap();
    assert_eq!(
        shown[0].text.as_deref(),
        Some("start amlodipine daily"),
        "{shown:?}"
    );
}

/// Placing with nothing applied makes no revision.
#[test]
fn placing_with_no_changes_makes_no_revision() {
    let (_dir, mut store, chunk) = store("proposals-no-changes");
    store.add_proposals(S1, &every_decision(chunk)).unwrap();
    let read = store.proposals(S1).unwrap();
    let placing = Placing {
        cards: vec![read[1].id, read[2].id],
        ..Placing::default()
    };
    assert_eq!(store.place_proposals(S1, &placing).unwrap(), None);
    assert_eq!(
        store.revisions(S1).unwrap(),
        [RevisionNumber::FINAL],
        "no revision past the final text"
    );
}

/// A placing that can't all be written writes none of it: no outcome
/// moves and no revision is made.
#[test]
fn a_placing_that_fails_changes_nothing() {
    let (_dir, mut store, chunk) = store("proposals-placing-fails");
    store.add_proposals(S1, &every_decision(chunk)).unwrap();
    let before = store.proposals(S1).unwrap();
    let change = Change {
        chunk,
        words: span(1, 2),
        text: "amlodipine".to_owned(),
    };
    let failing = [
        // A rejected proposal can't be a card.
        Placing {
            cards: vec![before[1].id, before[5].id],
            changes: vec![change.clone()],
            applied: vec![before[0].id],
            ..Placing::default()
        },
        // A proposal that isn't applied can't be applied by the revision.
        Placing {
            cards: vec![before[1].id],
            changes: vec![change.clone()],
            applied: vec![before[0].id, before[1].id],
            ..Placing::default()
        },
        // Another session's proposal isn't this session's to rank.
        Placing {
            over_cap: vec![before[2].id],
            ..Placing::default()
        },
        // Changes that share a word.
        Placing {
            changes: vec![change.clone(), change],
            applied: vec![before[0].id],
            ..Placing::default()
        },
    ];
    let sessions = [S1, S1, S2, S1];
    for (placing, session) in failing.iter().zip(sessions) {
        assert!(
            store.place_proposals(session, placing).is_err(),
            "{placing:?}"
        );
        assert_eq!(store.proposals(S1).unwrap(), before, "{placing:?}");
        assert_eq!(store.revisions(S1).unwrap(), [RevisionNumber::FINAL]);
    }
    // Applied proposals with no change to write them aren't applied.
    let none = Placing {
        applied: vec![before[0].id],
        ..Placing::default()
    };
    assert!(matches!(
        store.place_proposals(S1, &none),
        Err(StoreError::NoProposal(_))
    ));
}

/// A review is recorded once, and only on a proposal that exists.
#[test]
fn a_proposal_is_reviewed_once() {
    let (_dir, mut store, chunk) = store("proposals-review");
    store.add_proposals(S1, &every_decision(chunk)).unwrap();
    let id = store.proposals(S1).unwrap()[0].id;
    store.review_proposal(id, Reviewed::Rejected).unwrap();
    assert_eq!(
        store.proposals(S1).unwrap()[0].review,
        Some(Reviewed::Rejected)
    );
    assert!(store.review_proposal(id, Reviewed::Accepted).is_err());
    // Not even with the same verdict.
    assert!(store.review_proposal(id, Reviewed::Rejected).is_err());
    assert!(matches!(
        store.review_proposal(ProposalId(9_999), Reviewed::Accepted),
        Err(StoreError::NoProposal(9_999))
    ));
    assert_eq!(
        store.proposals(S1).unwrap()[0].review,
        Some(Reviewed::Rejected)
    );
}

/// The schema keeps what a proposal records, whatever connection tries:
/// it's never deleted or replaced, its record never changes, its outcome
/// moves only from to-rank to a card or over the cap, and its revision
/// and review are set once.
#[test]
fn the_schema_keeps_proposals_as_recorded() {
    let (dir, mut store, chunk) = store("proposals-schema");
    store.add_proposals(S1, &every_decision(chunk)).unwrap();
    let read = store.proposals(S1).unwrap();
    let (applied, ranked) = (read[0].id.get(), read[1].id.get());
    drop(store);
    let conn = raw(&dir.db());
    for sql in [
        format!("DELETE FROM proposal WHERE id = {applied}"),
        format!("UPDATE proposal SET term = 'other' WHERE id = {applied}"),
        format!("UPDATE proposal SET first_word = 0 WHERE id = {applied}"),
        format!("UPDATE proposal SET offered_first = 0 WHERE id = {applied}"),
        format!("UPDATE proposal SET similarity = 0.1 WHERE id = {applied}"),
        format!("UPDATE proposal SET outcome = 'card' WHERE id = {applied}"),
        format!("INSERT OR REPLACE INTO proposal SELECT * FROM proposal WHERE id = {applied}"),
    ] {
        assert!(refused(&conn, &sql), "{sql}");
    }
    // To-rank may move to a card, once.
    conn.execute_batch(&format!(
        "UPDATE proposal SET outcome = 'card' WHERE id = {ranked}"
    ))
    .unwrap();
    assert!(refused(
        &conn,
        &format!("UPDATE proposal SET outcome = 'over-cap' WHERE id = {ranked}")
    ));
    // A review is set once.
    conn.execute_batch(&format!(
        "UPDATE proposal SET review = 'accepted' WHERE id = {ranked}"
    ))
    .unwrap();
    assert!(refused(
        &conn,
        &format!("UPDATE proposal SET review = 'rejected' WHERE id = {ranked}")
    ));
    // The revision that applied a proposal is set once.
    conn.execute_batch(
        "INSERT INTO revision (session_id, number, parent) VALUES (1, 1, 0), (1, 2, 1)",
    )
    .unwrap();
    conn.execute_batch(&format!(
        "UPDATE proposal SET applied_in = 1 WHERE id = {applied}"
    ))
    .unwrap();
    assert!(refused(
        &conn,
        &format!("UPDATE proposal SET applied_in = 2 WHERE id = {applied}")
    ));
    // Only an applied proposal names the revision that applied it.
    assert!(
        conn.execute_batch(&format!(
            "UPDATE proposal SET applied_in = 1 WHERE id = {ranked}"
        ))
        .is_err()
    );
}

/// A row's outcome agrees with its decision: one written by hand that
/// doesn't is refused, so a read never meets it. Each is a stored row
/// copied with one decision changed; the copy unchanged goes in.
#[test]
fn the_schema_ties_each_outcome_to_its_decision() {
    let (dir, mut store, chunk) = store("proposals-outcomes");
    store.add_proposals(S1, &every_decision(chunk)).unwrap();
    drop(store);
    let conn = raw(&dir.db());
    let copy = |outcome: &str, change: &str| -> bool {
        let sql = format!(
            "CREATE TEMP TABLE copied AS SELECT * FROM proposal WHERE outcome = '{outcome}' LIMIT 1;
             UPDATE copied SET id = id + 1000 {change};
             INSERT INTO proposal SELECT * FROM copied;"
        );
        let refused = conn.execute_batch(&sql).is_err();
        conn.execute_batch("DROP TABLE IF EXISTS temp.copied")
            .unwrap();
        refused
    };
    for (outcome, change) in [
        ("applied", ", answer = 'no', gate = NULL"),
        ("applied", ", gate = 'dissimilar'"),
        ("to-rank", ", gate = 'applied'"),
        ("to-rank", ", answer = 'no', gate = NULL"),
        ("rejected", ", rule = 'no-answer'"),
    ] {
        assert!(copy(outcome, change), "{outcome}{change}");
    }
    assert!(!copy("applied", ""), "an unchanged copy goes in");
}
