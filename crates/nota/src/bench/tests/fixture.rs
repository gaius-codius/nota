//! A fixture's candidates, and the files that aren't fixtures.

use super::*;
use crate::bench::fixture::{Fixture, from_fixture, parse};

/// A fixture file of terms and chunks is read as written.
#[test]
fn a_fixture_is_read_as_written() {
    let text = r#"{"terms": ["amlodipine", "ibuprofen"],
                   "chunks": ["start amylo diping daily", "take hyper profen"]}"#;
    assert_eq!(
        parse(text),
        Ok(Fixture {
            terms: vec!["amlodipine".to_owned(), "ibuprofen".to_owned()],
            chunks: vec![
                "start amylo diping daily".to_owned(),
                "take hyper profen".to_owned()
            ],
        })
    );
}

/// A fixture's candidates are what the lookup offers in its chunks, each
/// chunk 30 s after the one before on track 0.
#[test]
fn a_fixtures_candidates_are_what_the_lookup_offers_in_its_chunks() {
    let dir = TestDir::new("fixture");
    let path = dir.0.join("terms.json");
    let text = serde_json::json!({ "terms": TERMS, "chunks": CHUNKS }).to_string();
    write_file(&path, &text);
    let candidates = from_fixture(&path, Precheck::On).unwrap();
    let offered: Vec<Key> = candidates
        .offering
        .iter()
        .flat_map(|(chunk, offers)| offers.iter().map(|o| Key::of(chunk, o)))
        .collect();
    assert_eq!(
        offered,
        [
            key(0, 1, 2, "amylo diping", "amlodipine"),
            key(1, 2, 2, "profen", "ibuprofen"),
        ]
    );
    assert_eq!(
        (candidates.source, candidates.precheck, candidates.saved),
        (format!("fixture {}", path.display()), Precheck::On, None)
    );
}

/// A file without its chunks isn't a fixture, and the message names the
/// file and what's missing.
#[test]
fn a_fixture_without_chunks_is_refused() {
    let dir = TestDir::new("fixture-missing");
    let path = dir.0.join("terms.json");
    write_file(&path, r#"{"terms": ["amlodipine"]}"#);
    let Err(err) = from_fixture(&path, Precheck::Off) else {
        panic!("a fixture without chunks was read");
    };
    assert_eq!(
        err,
        format!("{}: not a bench fixture: chunks is missing", path.display())
    );
}

/// A number among the chunks is refused: every chunk is text.
#[test]
fn a_number_among_the_chunks_is_refused() {
    assert_eq!(
        parse(r#"{"terms": [], "chunks": ["start amylo diping", 3]}"#),
        Err("chunks holds something other than text".to_owned())
    );
}

/// A key a fixture doesn't have is refused, not passed over: it's most
/// likely a misspelt one.
#[test]
fn an_unknown_key_is_refused() {
    assert_eq!(
        parse(r#"{"terms": [], "chunks": [], "trems": []}"#),
        Err("unknown key trems".to_owned())
    );
}

/// More terms than a fixture may hold are refused.
#[test]
fn too_many_terms_are_refused() {
    let terms = vec!["amlodipine"; 10_001];
    let text = serde_json::json!({ "terms": terms, "chunks": [] }).to_string();
    assert_eq!(parse(&text), Err("terms has over 10000 items".to_owned()));
}
