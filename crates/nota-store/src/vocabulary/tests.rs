use nota_core::WallTime;

use super::*;
use crate::learned::Verdict;
use crate::packs::{AliasRow, Manifest, PackKind, Review, Source, TermRow};
use crate::test_dir::TestDir;
use crate::tests::{new_session, raw, refused};

const ONE: SessionId = SessionId::new(1);
const TWO: SessionId = SessionId::new(2);

/// A pack named `name` at `version`, of `kind`, with one source and these
/// terms, each reviewed, and an alias for the first.
fn pack(name: &str, version: &str, kind: PackKind, terms: &[&str]) -> Pack {
    let source = Source {
        id: "list".to_owned(),
        name: "A list".to_owned(),
        licence: "CC0-1.0".to_owned(),
        attribution: "From a list.".to_owned(),
        url: None,
    };
    let rows = terms
        .iter()
        .map(|t| TermRow {
            term: (*t).to_owned(),
            source: "list".to_owned(),
            licence: "CC0-1.0".to_owned(),
            review: Review::Reviewed,
        })
        .collect();
    let aliases = terms
        .first()
        .map(|t| AliasRow {
            term: (*t).to_owned(),
            alias: format!("{t} spoken"),
            source: "list".to_owned(),
            licence: "CC0-1.0".to_owned(),
            review: Review::Reviewed,
        })
        .into_iter()
        .collect();
    let manifest = Manifest {
        name: name.to_owned(),
        version: version.to_owned(),
        language: "en".to_owned(),
        kind,
        sources: vec![source],
    };
    Pack::new(manifest, rows, aliases).unwrap()
}

/// A library with sessions 1 and 2.
fn library(dir: &TestDir) -> Store {
    let mut store = Store::open(&dir.db()).unwrap();
    for id in [ONE, TWO] {
        store.create_session(&new_session(id)).unwrap();
    }
    store
}

/// `name` at `version`, as a session records it.
fn named(name: &str, version: &str) -> PackRef {
    PackRef {
        name: name.to_owned(),
        version: version.to_owned(),
    }
}

/// Acceptance: a session's packs and their versions read back, in the
/// order it chose them, after a restart; another session's are its own.
#[test]
fn a_sessions_packs_and_versions_read_back() {
    let dir = TestDir::new("session-packs");
    let chosen = [named("pharmacy", "2026.11"), named("my glossary", "1")];
    {
        let mut store = library(&dir);
        store.record_session_packs(ONE, &chosen).unwrap();
        store
            .record_session_packs(TWO, &[named("pharmacy", "2026.10")])
            .unwrap();
    }
    let store = Store::open(&dir.db()).unwrap();
    assert_eq!(store.session_packs(ONE).unwrap(), chosen);
    assert_eq!(
        store.session_packs(TWO).unwrap(),
        [named("pharmacy", "2026.10")]
    );
    assert!(store.session_packs(SessionId::new(3)).unwrap().is_empty());
}

/// A session's packs are recorded once: a second call is refused and
/// changes nothing, and so is a session the library lacks.
#[test]
fn a_sessions_packs_are_recorded_once() {
    let dir = TestDir::new("session-packs-once");
    let mut store = library(&dir);
    store
        .record_session_packs(ONE, &[named("pharmacy", "1")])
        .unwrap();
    assert!(matches!(
        store.record_session_packs(ONE, &[named("other", "1")]),
        Err(StoreError::PacksRecorded(id)) if id == ONE
    ));
    assert_eq!(store.session_packs(ONE).unwrap(), [named("pharmacy", "1")]);
    let three = SessionId::new(3);
    assert!(matches!(
        store.record_session_packs(three, &[named("pharmacy", "1")]),
        Err(StoreError::NoSession(id)) if id == three
    ));
    // One pack named twice is refused whole.
    assert!(matches!(
        store.record_session_packs(TWO, &[named("a", "1"), named("a", "2")]),
        Err(StoreError::Sqlite(_))
    ));
    assert!(store.session_packs(TWO).unwrap().is_empty());
}

/// The record of a session's packs is never changed by SQL either.
#[test]
fn a_sessions_packs_are_never_changed() {
    let dir = TestDir::new("session-packs-record");
    let mut store = library(&dir);
    store
        .record_session_packs(ONE, &[named("pharmacy", "1")])
        .unwrap();
    drop(store);
    let conn = raw(&dir.db());
    for sql in [
        "UPDATE session_pack SET version = '2'",
        "DELETE FROM session_pack",
        "INSERT OR REPLACE INTO session_pack VALUES (1, 0, 'pharmacy', '2')",
        "INSERT INTO session_pack VALUES (1, 0, 'other', '1')",
    ] {
        assert!(refused(&conn, sql), "{sql} was allowed");
    }
}

/// A session's vocabulary is the terms of the packs it chose, in its
/// order and taken from the installed packs, and every learned pair; a
/// pack it didn't choose gives nothing.
#[test]
fn a_session_gets_the_terms_of_the_packs_it_chose() {
    let dir = TestDir::new("session-terms");
    let mut store = library(&dir);
    let pharmacy = pack("pharmacy", "2", PackKind::Shipped, &["amlodipine"]);
    let glossary = pack("glossary", "1", PackKind::Own, &["Naranjo"]);
    let other = pack("law", "1", PackKind::Shipped, &["estoppel"]);
    store
        .record_session_packs(ONE, &[named("glossary", "1"), named("pharmacy", "2")])
        .unwrap();
    store
        .learn(
            TWO,
            "endside",
            "inside",
            Verdict::Rejected,
            WallTime::from_unix_seconds(0).unwrap(),
        )
        .unwrap();
    let installed = [other, pharmacy.clone(), glossary.clone()];
    let terms = store.session_terms(ONE, &installed).unwrap();
    let mut want = glossary.offered();
    want.extend(pharmacy.offered());
    assert_eq!(terms.terms, want);
    // A pair learned in another session holds here too.
    assert!(!terms.allows("endside", "inside"));
    assert!(terms.allows("endside", "NSAID"));
}

/// A session that chose no packs gets no pack terms, but still the
/// learned pairs.
#[test]
fn a_session_with_no_packs_gets_only_the_learned_pairs() {
    let dir = TestDir::new("session-terms-none");
    let mut store = library(&dir);
    store
        .learn(
            ONE,
            "a dlp one",
            "GLP-1",
            Verdict::Accepted,
            WallTime::from_unix_seconds(0).unwrap(),
        )
        .unwrap();
    let installed = [pack("pharmacy", "2", PackKind::Shipped, &["amlodipine"])];
    let terms = store.session_terms(TWO, &installed).unwrap();
    assert!(terms.terms.is_empty());
    assert_eq!(terms.learned.accepted(), [("a dlp one", "GLP-1")]);
}

/// A pack the session chose that isn't installed, or is installed at
/// another version, is refused by name: its terms would differ from the
/// ones the session's proposals came from.
#[test]
fn a_chosen_pack_missing_or_at_another_version_is_refused() {
    let dir = TestDir::new("session-terms-missing");
    let mut store = library(&dir);
    store
        .record_session_packs(ONE, &[named("pharmacy", "2")])
        .unwrap();
    for installed in [
        vec![],
        vec![pack("pharmacy", "3", PackKind::Shipped, &["amlodipine"])],
    ] {
        let err = store.session_terms(ONE, &installed).unwrap_err();
        assert!(
            matches!(&err, StoreError::PackMissing { session, pack }
                if *session == ONE && *pack == named("pharmacy", "2")),
            "{err:?}"
        );
    }
}
