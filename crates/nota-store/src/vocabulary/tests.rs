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

/// A shipped pack `name` at `version` with one term, as a session
/// records it.
fn named(name: &str, version: &str) -> PackRef {
    pack(name, version, PackKind::Shipped, &["amlodipine"]).pack_ref()
}

/// `packs` as the session's record reads them back.
fn recorded(packs: &[PackRef]) -> Vec<RecordedPack> {
    packs.iter().map(RecordedPack::from).collect()
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
    assert_eq!(store.session_packs(ONE).unwrap(), recorded(&chosen));
    assert_eq!(
        store.session_packs(TWO).unwrap(),
        recorded(&[named("pharmacy", "2026.10")])
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
    assert_eq!(
        store.session_packs(ONE).unwrap(),
        recorded(&[named("pharmacy", "1")])
    );
    let three = SessionId::new(3);
    assert!(matches!(
        store.record_session_packs(three, &[named("pharmacy", "1")]),
        Err(StoreError::NoSession(id)) if id == three
    ));
    // A choice of no packs is a choice: it's recorded once too.
    store.record_session_packs(TWO, &[]).unwrap();
    assert!(matches!(
        store.record_session_packs(TWO, &[named("pharmacy", "1")]),
        Err(StoreError::PacksRecorded(id)) if id == TWO
    ));
    assert!(store.session_packs(TWO).unwrap().is_empty());
    store
        .create_session(&new_session(SessionId::new(4)))
        .unwrap();
    let four = SessionId::new(4);
    // One pack named twice is refused whole.
    assert!(matches!(
        store.record_session_packs(four, &[named("a", "1"), named("a", "2")]),
        Err(StoreError::Sqlite(_))
    ));
    assert!(store.session_packs(four).unwrap().is_empty());
    // Refused whole: the choice wasn't recorded either, so it can be made.
    store
        .record_session_packs(four, &[named("a", "1")])
        .unwrap();
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
        "UPDATE session_pack SET digest = zeroblob(32)",
        "DELETE FROM session_pack",
        "INSERT OR REPLACE INTO session_pack VALUES (1, 0, 'pharmacy', '2', zeroblob(32))",
        "INSERT INTO session_pack VALUES (1, 0, 'other', '1', zeroblob(32))",
        // A replace by rowid would take session 1's row for session 2.
        "INSERT OR REPLACE INTO session_pack (rowid, session_id, position, name, version, digest) \
         VALUES (1, 2, 0, 'x', '1', zeroblob(32))",
        // From version 10 on, a pack is never recorded without its digest.
        "INSERT INTO session_pack (session_id, position, name, version) VALUES (2, 0, 'x', '1')",
        "UPDATE session_packs_chosen SET session_id = 2",
        "DELETE FROM session_packs_chosen",
        "INSERT OR REPLACE INTO session_packs_chosen VALUES (1)",
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
        .record_session_packs(ONE, &[glossary.pack_ref(), pharmacy.pack_ref()])
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
        assert_missing(&store, &installed, &named("pharmacy", "2"));
    }
}

/// Asserts that session 1's terms are refused, with `installed`, because
/// its pack `chosen` is missing.
fn assert_missing(store: &Store, installed: &[Pack], chosen: &PackRef) {
    let err = store.session_terms(ONE, installed).unwrap_err();
    assert!(
        matches!(&err, StoreError::PackMissing { session, pack }
            if *session == ONE && *pack == RecordedPack::from(chosen)),
        "{err:?}"
    );
}

/// Acceptance (#226): a pack rebuilt with other rows under the name and
/// version the session recorded is refused as missing, not used: its
/// terms aren't the ones the session's proposals came from.
#[test]
fn a_pack_rebuilt_with_other_rows_under_the_same_version_is_refused() {
    let dir = TestDir::new("session-terms-rebuilt");
    let mut store = library(&dir);
    let chosen = pack("glossary", "1", PackKind::Own, &["Naranjo"]);
    store
        .record_session_packs(ONE, &[chosen.pack_ref()])
        .unwrap();
    // The glossary's default version again, with a term more.
    let rebuilt = pack("glossary", "1", PackKind::Own, &["Naranjo", "Beers"]);
    assert_missing(&store, std::slice::from_ref(&rebuilt), &chosen.pack_ref());
    // The pack it recorded is still found, beside the rebuilt one.
    let terms = store
        .session_terms(ONE, &[rebuilt, chosen.clone()])
        .unwrap();
    assert_eq!(terms.terms, chosen.offered());
}

/// Acceptance (#226): a pack of the recorded name and version but
/// another kind (a user's own built with a shipped pack's name and
/// version) is refused as missing: its draft rows would all be offered.
#[test]
fn a_pack_of_another_kind_under_the_same_version_is_refused() {
    let dir = TestDir::new("session-terms-kind");
    let mut store = library(&dir);
    let shipped = pack("pharmacy", "2026.11", PackKind::Shipped, &["amlodipine"]);
    store
        .record_session_packs(ONE, &[shipped.pack_ref()])
        .unwrap();
    // The same rows: only the kind differs.
    let own = pack("pharmacy", "2026.11", PackKind::Own, &["amlodipine"]);
    assert_missing(&store, &[own], &shipped.pack_ref());
}
