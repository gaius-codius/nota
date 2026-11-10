//! C1 against the research bench: every fixture's score agrees with Python's
//! `difflib` to 1e-9, and keeps the bench's decision at the 0.6 threshold.
//!
//! The committed fixture (`fixtures/c1_pairs.json`) holds the short term
//! pairs and invented edge cases. Every pair the bench scored, long spans
//! included, stays in the local bench; its test is ignored here and run by
//! `scripts/bench-fixtures.sh`. Both files are written by the bench's
//! `fixtures/export.py`.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::path::{Path, PathBuf};

use nota_checks::{TooLong, letter_similarity};
use serde_json::Value;

/// How far a score may be from Python's: the fixtures print Python's floats,
/// and their parse back need not be exact to the last bit.
const TOLERANCE: f64 = 1e-9;

/// C1's threshold in the bench: a fix is kept at 0.6 or more.
const SIM_MIN: f64 = 0.6;

/// One scored pair from a fixture.
#[derive(Debug)]
struct Pair {
    /// The heard text, the first argument (difflib's ratio isn't symmetric).
    a: String,
    /// The replacement or term.
    b: String,
    /// Python's score, or `None` where nota refuses a side as too long.
    score: Option<f64>,
    /// Whether the bench's filter kept it, when the fixture says.
    kept: Option<bool>,
}

/// The pairs in `list`, one of a fixture's arrays.
fn pairs(list: &Value) -> Vec<Pair> {
    let list = list.as_array().expect("a list of pairs");
    list.iter()
        .map(|p| Pair {
            a: p["a"].as_str().expect("a string").to_owned(),
            b: p["b"].as_str().expect("b string").to_owned(),
            score: p["score"].as_f64(),
            kept: p["kept"].as_bool(),
        })
        .collect()
}

/// The fixture at `path`, parsed.
fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("fixture is JSON")
}

/// The committed fixture.
fn committed() -> Value {
    read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c1_pairs.json"))
}

/// Checks each pair against the port, and returns the mismatches.
fn mismatches(pairs: &[Pair]) -> Vec<String> {
    let mut bad = Vec::new();
    for p in pairs {
        let got = letter_similarity(&p.a, &p.b);
        match (p.score, got) {
            (None, Err(TooLong)) => {}
            (Some(want), Ok(got)) => {
                let got = got.value();
                if (got - want).abs() > TOLERANCE {
                    bad.push(format!("{:?} -> {:?}: {got}, Python {want}", p.a, p.b));
                }
                // The bench's decision, compared as a bool: a score exactly
                // on 0.6 must stay kept.
                if p.kept.is_some_and(|kept| kept != (got >= SIM_MIN)) {
                    bad.push(format!(
                        "{:?} -> {:?}: {got} flips the 0.6 decision",
                        p.a, p.b
                    ));
                }
            }
            (want, got) => bad.push(format!("{:?} -> {:?}: {got:?}, Python {want:?}", p.a, p.b)),
        }
    }
    bad
}

/// Every committed pair the bench scored agrees with Python.
#[test]
fn the_committed_bench_pairs_agree_with_python() {
    let fixture = committed();
    let pairs = pairs(&fixture["pairs"]);
    // The export holds 264 term pairs; an empty list would pass vacuously.
    assert!(pairs.len() >= 264, "{} pairs", pairs.len()); // check-bound
    assert_eq!(mismatches(&pairs), Vec::<String>::new());
}

/// The invented edge cases agree with Python, and over-long sides are refused.
#[test]
fn the_edge_cases_agree_with_python() {
    let fixture = committed();
    let cases = pairs(&fixture["edge_cases"]);
    assert!(
        cases.iter().filter(|c| c.score.is_none()).count() >= 3,
        "{cases:?}"
    ); // check-bound
    assert_eq!(mismatches(&cases), Vec::<String>::new());
}

/// The spec's five named pairs score as the 2026-09-30 note says, at full
/// precision, and fall on its side of 0.6.
#[test]
fn the_specs_named_pairs_keep_their_scores() {
    let named = [
        ("amylo diping", "amlodipine", 0.857_142_857_142_857_1, true),
        ("hyperprofen", "ibuprofen", 0.6, true),
        (
            "bloody pine",
            "beta blocker",
            0.380_952_380_952_380_93,
            false,
        ),
        ("almen's", "allopurinol", 0.352_941_176_470_588_26, false),
        (
            "hyperprofen",
            "hydrochlorothiazide",
            0.333_333_333_333_333_3,
            false,
        ),
    ];
    let got: Vec<_> = named
        .iter()
        .map(|(a, b, ..)| {
            let s = letter_similarity(a, b).unwrap().value();
            (*a, *b, s, s >= SIM_MIN)
        })
        .collect();
    for ((a, b, want, kept), (.., s, k)) in named.iter().zip(&got) {
        assert!((s - want).abs() <= TOLERANCE, "{a} -> {b}: {s}");
        assert_eq!(k, kept, "{a} -> {b}: {s}");
    }
    // The same pairs are in the committed fixture, so it can't drop them.
    let fixture = committed();
    let listed = pairs(&fixture["pairs"]);
    for (a, b, ..) in named {
        assert!(
            listed.iter().any(|p| p.a == a && p.b == b),
            "{a} -> {b} missing"
        );
    }
}

/// The local bench fixtures' folder: `NOTA_BENCH_FIXTURES`, which
/// `scripts/bench-fixtures.sh` sets.
fn bench_fixtures() -> PathBuf {
    std::env::var_os("NOTA_BENCH_FIXTURES")
        .map(PathBuf::from)
        .expect(
            "NOTA_BENCH_FIXTURES names the exported bench fixtures; run scripts/bench-fixtures.sh",
        )
}

/// Every pair the bench scored, long spans and unchanged ones included,
/// agrees with Python.
#[test]
#[ignore = "needs the local bench fixtures"]
fn every_bench_pair_agrees_with_python() {
    let fixture = read(&bench_fixtures().join("c1_pairs.json"));
    let pairs = pairs(&fixture["pairs"]);
    // The export holds 585 pairs from 21 fixlogs, 3 hotword runs and the spec.
    assert!(pairs.len() >= 585, "{} pairs", pairs.len()); // check-bound
    assert_eq!(mismatches(&pairs), Vec::<String>::new());
}
