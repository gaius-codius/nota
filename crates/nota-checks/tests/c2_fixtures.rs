//! C2 against the research bench: the gate accepts and refuses the same
//! fixes as the bench's gate U, scores them as it did, and revises each
//! chunk to the same text.
//!
//! The committed fixture (`fixtures/c2_gate.json`) holds the bench fixes
//! whose strings are all short spans, each gated alone in its heard span
//! (or an empty chunk where the bench didn't find the span), so it runs in
//! CI. Gated alone, a fix's span always sits at the chunk's edges in its
//! own case, so this fixture doesn't reach the word boundary or the case
//! match; the gate's own tests do. Every fix run with its chunk text, the
//! workshop's, stays in the local bench (`c2_gate_u.json`); its tests are
//! ignored here and run by `scripts/bench-fixtures.sh`. Both files are
//! written by the bench's `fixtures/export.py`.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nota_checks::{Offered, Proposal, Rejection, gate};
use serde_json::Value;

/// How far a score may be from Python's (the fixture prints its floats).
const TOLERANCE: f64 = 1e-9;

/// The bench's decision on a fix, as the fixtures name it, mapped to what
/// the gate should decide: `None` for applied. `applied` says whether the
/// bench placed an accepted fix in its running text.
fn expected(decision: &str, applied: bool) -> Result<Vec<Option<Rejection>>, String> {
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

/// One fix to gate and what the bench made of it.
struct Fix {
    /// The proposal, as the model made it.
    proposal: Proposal,
    /// The decisions that agree with the bench's (`None` for applied).
    want: Vec<Option<Rejection>>,
    /// C1's score at full precision, `None` where the span wasn't found.
    sim: Option<f64>,
    /// The fix as the fixture holds it, for messages.
    shown: String,
}

/// A chunk to gate: its text, what was offered for it, its fixes in order,
/// and the text the bench revised it to.
struct Case {
    /// Where the case is in its fixture, for messages.
    at: String,
    /// The chunk's text.
    text: String,
    /// The candidates offered for it.
    offered: Vec<Offered>,
    /// The fixes proposed for it.
    fixes: Vec<Fix>,
    /// The bench's revised text.
    result: String,
}

/// The string at `key` in `v`.
fn text(v: &Value, key: &str) -> String {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {v}"))
        .to_owned()
}

/// The candidates in `v`'s `offered` list.
fn offered(v: &Value) -> Vec<Offered> {
    v["offered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| Offered {
            window: text(o, "window"),
            term: text(o, "term"),
        })
        .collect()
}

/// The fix `f`, whose bench decision is `want`.
fn fix(f: &Value, want: Vec<Option<Rejection>>) -> Fix {
    Fix {
        proposal: Proposal {
            heard: text(f, "heard"),
            replacement: text(f, "repl"),
        },
        want,
        sim: f["sim"].as_f64(),
        shown: f.to_string(),
    }
}

/// `case` gated, with the problems found: decisions, scores or text that
/// differ from the bench's.
fn mismatches(case: &Case) -> Vec<String> {
    let at = &case.at;
    let proposals: Vec<Proposal> = case.fixes.iter().map(|f| f.proposal.clone()).collect();
    let gated = gate(&case.text, &case.offered, &proposals);
    let mut bad = Vec::new();
    if gated.decisions().len() != case.fixes.len() {
        bad.push(format!(
            "{at}: {} decisions for {} fixes",
            gated.decisions().len(),
            case.fixes.len()
        ));
    }
    for (fix, decision) in case.fixes.iter().zip(gated.decisions()) {
        if !fix.want.contains(&decision.rejection()) {
            bad.push(format!(
                "{at}: {} decided {:?}",
                fix.shown,
                decision.verdict()
            ));
        }
        // The bench scored every found span; the gate scores it once the
        // span is found too.
        let got = decision.similarity().map(nota_checks::Similarity::value);
        let close = match (fix.sim, got) {
            (Some(w), Some(g)) => (w - g).abs() <= TOLERANCE,
            (None, None) => true,
            _ => false,
        };
        if !close {
            bad.push(format!("{at}: {} scored {got:?}", fix.shown));
        }
    }
    if gated.text() != case.result {
        bad.push(format!("{at}: revised to {:?}", gated.text()));
    }
    bad
}

/// The bench fixture's chunk `chunk` from run `run`, as a case.
fn bench_case(run: &str, chunk: &Value) -> Case {
    let fixes = chunk["fixes"].as_array().unwrap();
    Case {
        at: format!("{run} {}/{}", chunk["excerpt"], chunk["chunk"]),
        text: text(chunk, "text"),
        offered: offered(chunk),
        fixes: fixes
            .iter()
            .map(|f| {
                let decision = f["decision"].as_str().unwrap();
                fix(
                    f,
                    expected(decision, f["applied"].as_bool().unwrap()).unwrap(),
                )
            })
            .collect(),
        result: text(chunk, "result"),
    }
}

/// The committed fixture's entry `entry`, the `i`th, as a case of one fix.
fn committed_case(i: usize, entry: &Value) -> Case {
    // Gated alone, nothing placed before it can overlap an accepted fix,
    // so the bench's decision is the whole of it.
    let want = expected(entry["decision"].as_str().unwrap(), true).unwrap();
    Case {
        at: format!("c2_gate.json fixes[{i}]"),
        text: text(entry, "chunk"),
        offered: offered(entry),
        fixes: vec![fix(entry, want)],
        result: text(entry, "result"),
    }
}

/// The fixture at `path`, parsed.
fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("fixture is JSON")
}

/// The committed fixture.
fn committed() -> Value {
    read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c2_gate.json"))
}

/// The committed fixture's entries.
fn committed_fixes(fixture: &Value) -> &[Value] {
    fixture["fixes"].as_array().unwrap()
}

/// Every committed bench fix is decided, scored and placed as the bench's
/// gate U did, gated alone.
#[test]
fn every_committed_bench_fix_is_gated_as_the_bench_did() {
    let fixture = committed();
    let fixes = committed_fixes(&fixture);
    let mut decisions = BTreeMap::<&str, usize>::new();
    let mut bad = Vec::new();
    for (i, entry) in fixes.iter().enumerate() {
        *decisions
            .entry(entry["decision"].as_str().unwrap())
            .or_default() += 1;
        bad.extend(mismatches(&committed_case(i, entry)));
    }
    // The export holds 429 distinct short fixes. A floor on each decision
    // too, so an export that lost one kind (every accepted fix, say) can't
    // pass on the rest.
    assert!(fixes.len() >= 429, "{} fixes", fixes.len()); // check-bound
    let floors = [
        ("accepted", 60),
        ("no offered term in the replacement", 276),
        ("similarity below 0.6", 60),
        ("span not found", 33),
    ];
    for (decision, floor) in floors {
        let n = decisions.get(decision).copied().unwrap_or(0);
        assert!(n >= floor, "{decision}: {n} fixes"); // check-bound
    }
    assert_eq!(bad, Vec::<String>::new());
}

/// The longest committed span: the bench's limit for what nota may commit
/// (fixtures/export.py), written here again, as `c1_fixtures.rs` does, so a
/// change there, or a hand edit of the fixture, can't commit workshop text
/// unnoticed.
const SPAN_MAX_CHARS: usize = 32; // check-bound
/// The most words in a committed span.
const SPAN_MAX_WORDS: usize = 4; // check-bound

/// Whether `s` is within the committed span limit.
fn short(s: &str) -> bool {
    s.chars().count() <= SPAN_MAX_CHARS && s.split_whitespace().count() <= SPAN_MAX_WORDS
}

/// The spans of transcript text in `entry`: its chunk, heard span,
/// replacement and result, and every offered window and term. Its decision
/// and run names are the bench's labels, not transcript text.
fn spans(entry: &Value) -> Vec<String> {
    let mut out: Vec<String> = ["chunk", "heard", "repl", "result"]
        .iter()
        .map(|key| text(entry, key))
        .collect();
    for o in entry["offered"].as_array().unwrap() {
        out.extend([text(o, "window"), text(o, "term")]);
    }
    out
}

/// Every span of transcript text in every committed entry, chunk, windows
/// and terms included, is short, and the fixture states the same limit.
#[test]
fn committed_bench_fixes_are_short_spans() {
    let fixture = committed();
    let long: Vec<String> = committed_fixes(&fixture)
        .iter()
        .flat_map(spans)
        .filter(|s| !short(s))
        .collect();
    assert_eq!(long, Vec::<String>::new());
    assert_eq!(
        fixture["span_limit"],
        serde_json::json!({ "max_chars": SPAN_MAX_CHARS, "max_words": SPAN_MAX_WORDS })
    );
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
    read(&bench_fixtures().join("c2_gate_u.json"))
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
            bad.extend(mismatches(&bench_case(run, chunk)));
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
