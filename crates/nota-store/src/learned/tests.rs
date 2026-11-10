use proptest::prelude::*;

use super::*;
use crate::test_dir::TestDir;
use crate::tests::{new_session, raw, refused};

const ONE: SessionId = SessionId::new(1);
const TWO: SessionId = SessionId::new(2);

/// 2026-11-10 09:00 UTC, plus `n` seconds.
fn at(n: i64) -> WallTime {
    WallTime::from_unix_seconds(1_762_765_200 + n).unwrap()
}

/// A library with sessions 1 and 2.
fn library(dir: &TestDir) -> Store {
    let mut store = Store::open(&dir.db()).unwrap();
    for id in [ONE, TWO] {
        store.create_session(&new_session(id)).unwrap();
    }
    store
}

/// Acceptance: learned pairs survive a restart, and a rejected pair is
/// never offered again for that heard text, in any later session.
#[test]
fn learned_pairs_survive_a_restart() {
    let dir = TestDir::new("learned-restart");
    {
        let mut store = library(&dir);
        store
            .learn(ONE, "a dlp one", "GLP-1", Verdict::Accepted, at(0))
            .unwrap();
        store
            .learn(ONE, "endside", "inside", Verdict::Rejected, at(1))
            .unwrap();
    }
    // A new process opens the library again.
    let store = Store::open(&dir.db()).unwrap();
    let learned = store.learned().unwrap();
    assert_eq!(
        learned.history(),
        [
            LearnedPair {
                heard: "a dlp one".to_owned(),
                term: "GLP-1".to_owned(),
                verdict: Verdict::Accepted,
                session: ONE,
                at: at(0),
            },
            LearnedPair {
                heard: "endside".to_owned(),
                term: "inside".to_owned(),
                verdict: Verdict::Rejected,
                session: ONE,
                at: at(1),
            },
        ]
    );
    assert!(!learned.allows("endside", "inside"));
    // Only that term, for that span.
    assert!(learned.allows("endside", "NSAID"));
    assert!(learned.allows("inside", "inside"));
    assert_eq!(learned.accepted_for("a dlp one"), ["GLP-1"]);
    assert_eq!(learned.accepted(), [("a dlp one", "GLP-1")]);
}

/// A span is matched however its case, punctuation or spacing came out,
/// so a rejection made on "Endside," holds for "endside".
#[test]
fn a_span_matches_whatever_its_case_and_punctuation() {
    let dir = TestDir::new("learned-keys");
    let mut store = library(&dir);
    store
        .learn(ONE, "Endside,", "Inside", Verdict::Rejected, at(0))
        .unwrap();
    store
        .learn(ONE, "A  DLP-one.", "GLP-1", Verdict::Accepted, at(1))
        .unwrap();
    let learned = store.learned().unwrap();
    assert!(!learned.allows("endside", "inside"));
    assert!(!learned.allows("  ENDSIDE ", "inside!"));
    // A typographic apostrophe is a plain one.
    assert_eq!(match_key("Crohn\u{2019}s"), match_key("crohn's"));
    assert_eq!(learned.accepted_for("a dlp one"), ["GLP-1"]);
}

/// The newest choice on a span and term decides: a term typed in by hand
/// after it was rejected is accepted, and one rejected after it was
/// accepted is offered no more. Both rows stay in the history.
#[test]
fn the_newest_choice_decides() {
    let dir = TestDir::new("learned-newest");
    let mut store = library(&dir);
    store
        .learn(ONE, "endside", "NSAID", Verdict::Rejected, at(0))
        .unwrap();
    store
        .learn(TWO, "endside", "NSAID", Verdict::Accepted, at(1))
        .unwrap();
    store
        .learn(ONE, "amylo", "amlodipine", Verdict::Accepted, at(2))
        .unwrap();
    store
        .learn(TWO, "amylo", "amlodipine", Verdict::Rejected, at(3))
        .unwrap();
    let learned = store.learned().unwrap();
    assert!(learned.allows("endside", "NSAID"));
    assert_eq!(learned.accepted_for("endside"), ["NSAID"]);
    assert!(!learned.allows("amylo", "amlodipine"));
    assert!(learned.accepted_for("amylo").is_empty());
    assert_eq!(learned.history().len(), 4);
}

/// Several terms accepted for one span all come back, and none for
/// another span.
#[test]
fn every_term_accepted_for_a_span_comes_back() {
    let dir = TestDir::new("learned-many");
    let mut store = library(&dir);
    for (heard, term) in [
        ("baricus", "varicose"),
        ("baricus", "Baricus"),
        ("barica", "x"),
    ] {
        store
            .learn(ONE, heard, term, Verdict::Accepted, at(0))
            .unwrap();
    }
    let learned = store.learned().unwrap();
    assert_eq!(learned.accepted_for("baricus"), ["Baricus", "varicose"]);
    assert_eq!(learned.accepted_for("barica"), ["x"]);
    assert!(learned.accepted_for("baric").is_empty());
}

/// A pair with nothing to match by, or from a session the library lacks,
/// is refused, and nothing is stored.
#[test]
fn a_pair_needs_text_and_its_session() {
    let dir = TestDir::new("learned-refused");
    let mut store = library(&dir);
    for (heard, term) in [
        ("", "GLP-1"),
        ("a dlp one", " - "),
        ("...", "x"),
        ("'", "x"),
    ] {
        assert!(matches!(
            store.learn(ONE, heard, term, Verdict::Accepted, at(0)),
            Err(StoreError::NoText)
        ));
    }
    let three = SessionId::new(3);
    assert!(matches!(
        store.learn(three, "a", "b", Verdict::Accepted, at(0)),
        Err(StoreError::NoSession(id)) if id == three
    ));
    assert!(store.learned().unwrap().history().is_empty());
}

/// A learned pair is a record: changing, deleting or replacing one is
/// refused by the schema.
#[test]
fn a_learned_pair_is_never_changed() {
    let dir = TestDir::new("learned-record");
    let mut store = library(&dir);
    store
        .learn(ONE, "endside", "inside", Verdict::Rejected, at(0))
        .unwrap();
    drop(store);
    let conn = raw(&dir.db());
    for sql in [
        "UPDATE learned_pair SET verdict = 'accepted'",
        "DELETE FROM learned_pair",
        "INSERT OR REPLACE INTO learned_pair VALUES \
         (1, 1, 'endside', 'endside', 'inside', 'inside', 'accepted', 0)",
    ] {
        assert!(refused(&conn, sql), "{sql} was allowed");
    }
}

proptest! {
    /// A match key has nothing but letters, digits, apostrophes and single
    /// spaces, no space at either end and no word without a letter or
    /// digit, is the same key again when keyed, and doesn't depend on ASCII
    /// case.
    #[test]
    fn a_match_key_is_plain_and_stable(text in "\\PC{0,30}") {
        let key = match_key(&text);
        prop_assert!(!key.starts_with(' ') && !key.ends_with(' '), "{key:?}");
        prop_assert!(!key.contains("  "), "{key:?}");
        prop_assert!(
            key.split(' ').all(|w| key.is_empty() || w.chars().any(char::is_alphanumeric)),
            "{key:?}"
        );
        prop_assert!(key.chars().all(|c| c == ' ' || c == '\'' || c.is_alphanumeric()), "{key:?}");
        prop_assert_eq!(match_key(&key), key);
        // ASCII case only: some letters' capitals aren't one letter.
        prop_assert_eq!(
            match_key(&text.to_ascii_uppercase()),
            match_key(&text.to_ascii_lowercase())
        );
    }
}
