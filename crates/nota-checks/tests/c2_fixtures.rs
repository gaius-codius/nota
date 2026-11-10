//! C2 against the research bench: on every fix run the bench logged, the
//! gate accepts and refuses the same fixes as the bench's gate U, scores
//! them as it did, and revises every chunk to the same text.
//!
//! The fixture (`c2_gate_u.json`, from the bench's `fixtures/export.py`)
//! holds the workshop's chunk text, so it stays in the local bench and this
//! test is ignored here; `scripts/bench-fixtures.sh` runs it.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::path::PathBuf;

use nota_checks::{Offered, Proposal, Rejection, gate};
use serde_json::Value;

/// How far a score may be from Python's (the fixture prints its floats).
const TOLERANCE: f64 = 1e-9;

/// The bench's decision on a fix, as the fixture names it, mapped to what
/// the gate should decide: `None` for applied.
fn expected(fix: &Value) -> Result<Vec<Option<Rejection>>, String> {
    let decision = fix["decision"].as_str().ok_or("no decision")?;
    let applied = fix["applied"].as_bool().ok_or("no applied")?;
    Ok(match (decision, applied) {
        ("accepted", true) => vec![None],
        // The bench accepted it, but the span was gone from its running
        // text: the gate's already-changed refusal.
        ("accepted", false) => vec![Some(Rejection::AlreadyChanged)],
        ("span not found", _) => vec![Some(Rejection::SpanNotFound)],
        ("similarity below 0.6", _) => vec![Some(Rejection::Dissimilar)],
        // The bench's one rule is two here: no term, or one for other words.
        ("no offered term in the replacement", _) => {
            vec![
                Some(Rejection::NotOffered),
                Some(Rejection::WindowElsewhere),
            ]
        }
        (other, _) => return Err(format!("unknown decision {other:?}")),
    })
}

/// The string at `key` in `v`.
fn text(v: &Value, key: &str) -> String {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {v}"))
        .to_owned()
}

/// The fixture's chunk `chunk` gated, with the problems found: decisions,
/// scores or text that differ from the bench's.
fn mismatches(run: &str, chunk: &Value) -> Vec<String> {
    let at = format!("{run} {}/{}", chunk["excerpt"], chunk["chunk"]);
    let offered: Vec<Offered> = chunk["offered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| Offered {
            window: text(o, "window"),
            term: text(o, "term"),
        })
        .collect();
    let fixes = chunk["fixes"].as_array().unwrap();
    let proposals: Vec<Proposal> = fixes
        .iter()
        .map(|f| Proposal {
            heard: text(f, "heard"),
            replacement: text(f, "repl"),
        })
        .collect();
    let gated = gate(&text(chunk, "text"), &offered, &proposals);
    let mut bad = Vec::new();
    for (fix, decision) in fixes.iter().zip(gated.decisions()) {
        let want = expected(fix).unwrap();
        if !want.contains(&decision.rejection()) {
            bad.push(format!("{at}: {fix} decided {:?}", decision.verdict()));
        }
        // The bench scored every found span; the gate scores it once the
        // span is found too.
        let got = decision.similarity().map(nota_checks::Similarity::value);
        let close = match (fix["sim"].as_f64(), got) {
            (Some(w), Some(g)) => (w - g).abs() <= TOLERANCE,
            (None, None) => true,
            _ => false,
        };
        if !close {
            bad.push(format!("{at}: {fix} scored {got:?}"));
        }
    }
    if gated.text() != text(chunk, "result") {
        bad.push(format!("{at}: revised to {:?}", gated.text()));
    }
    bad
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

/// The exported C2 fixture.
fn fixture() -> Value {
    let path = bench_fixtures().join("c2_gate_u.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("fixture is JSON")
}

/// Every fix in every logged fix run is decided, scored and placed as the
/// bench's gate U did.
#[test]
#[ignore = "needs the local bench fixtures"]
fn every_bench_fix_is_gated_as_the_bench_did() {
    let fixture = fixture();
    let runs = fixture["runs"].as_object().unwrap();
    let mut bad = Vec::new();
    let mut fixes = 0;
    for (run, chunks) in runs {
        for chunk in chunks.as_array().unwrap() {
            fixes += chunk["fixes"].as_array().unwrap().len();
            bad.extend(mismatches(run, chunk));
        }
    }
    // 16 fix runs with 1,270 fixes; an empty export would pass vacuously.
    assert!(runs.len() >= 16, "{} runs", runs.len()); // check-bound
    assert!(fixes >= 1_270, "{fixes} fixes"); // check-bound
    assert_eq!(bad, Vec::<String>::new());
}

/// No bench fix's score is moved across 0.6 by the bench's rounding, so
/// the gate's full-precision comparison makes the bench's decisions. A fix
/// that is would need its expected decision changed above.
#[test]
#[ignore = "needs the local bench fixtures"]
fn no_bench_score_is_rounded_across_the_threshold() {
    let fixture = fixture();
    let crossing: Vec<_> = fixture["runs"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|chunks| chunks.as_array().unwrap())
        .flat_map(|chunk| chunk["fixes"].as_array().unwrap())
        .filter(|fix| fix["rounding_crosses_threshold"].as_bool() != Some(false))
        .collect();
    assert_eq!(crossing, Vec::<&Value>::new());
}
