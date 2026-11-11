//! Clean-up against the research bench: on the bench's 60 chunks, with its
//! terms as the session's pack and a decider answering as the bench's
//! Qwen did, the job applies what the bench's pick run applied.
//!
//! The bench's chunks are workshop text and stay in the local bench, so
//! this test is ignored here and run by `scripts/bench-fixtures.sh`, which
//! exports `cleanup_pick.json` (the yes or no on each candidate the bench
//! offered, and each chunk as the run revised it) and `c4_lookup.json`
//! (the terms). The bench applied each yes whose C1 score was at least
//! 0.6 (`pick.py`'s F output); nota's gate applies a yes on the same
//! score, and every chunk reads the same.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::*;

/// The local bench fixtures' folder: `NOTA_BENCH_FIXTURES`, which
/// `scripts/bench-fixtures.sh` sets.
fn bench_fixtures() -> PathBuf {
    std::env::var_os("NOTA_BENCH_FIXTURES")
        .map(PathBuf::from)
        .expect(
            "NOTA_BENCH_FIXTURES names the exported bench fixtures; run scripts/bench-fixtures.sh",
        )
}

/// The fixture at `path`, parsed.
fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("fixture is JSON")
}

/// Runs the job over pick run `tag`'s chunks with a decider saying yes
/// where Qwen did, in that chunk, and compares each chunk's revised
/// text.
fn agrees_with(tag: &str) {
    let fixtures = bench_fixtures();
    let terms: Vec<String> = read(&fixtures.join("c4_lookup.json"))["terms"]
        .as_array()
        .expect("a term list")
        .iter()
        .map(|t| t["term"].as_str().expect("a term").to_owned())
        .collect();
    let pick = read(&fixtures.join("cleanup_pick.json"));
    let chunks = pick["runs"][tag].as_array().expect("a run's chunks");
    let texts: Vec<&str> = chunks
        .iter()
        .map(|c| c["text"].as_str().expect("a text"))
        .collect();
    let yeses: Vec<(&str, &str, &str)> = chunks
        .iter()
        .flat_map(|chunk| {
            let text = chunk["text"].as_str().expect("a text");
            let candidates = chunk["candidates"].as_array().expect("candidates");
            candidates
                .iter()
                .filter(|c| c["yes"].as_bool().expect("a yes or no"))
                .map(move |c| {
                    (
                        text,
                        c["window"].as_str().expect("a window"),
                        c["term"].as_str().expect("a term"),
                    )
                })
        })
        .collect();
    let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();
    let fixture = Fixture::new(&format!("bench-{tag}"), &texts, &term_refs);
    let mut clean_up = fixture.clean_up(Gives::Fake(Fake::saying_yes_in(yeses)));
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    let shown = fixture.shown();
    let mut wrong = Vec::new();
    for (chunk, got) in chunks.iter().zip(&shown) {
        let at = (
            chunk["excerpt"].as_u64().expect("an excerpt"),
            chunk["chunk"].as_u64().expect("a chunk"),
        );
        let want = chunk["result"].as_str().expect("a result");
        if got != want {
            wrong.push(format!("{at:?}\n  nota  {got}\n  bench {want}"));
        }
    }
    assert_eq!(shown.len(), 60); // check-bound
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Acceptance: the pick run without facts, the reference, applies the
/// same fixes through nota's job as on the bench.
#[test]
#[ignore = "needs the local bench fixtures"]
fn the_bench_reference_run_applies_the_same_fixes() {
    agrees_with("q5-pick-nofacts");
}

/// The pick run with facts does too.
#[test]
#[ignore = "needs the local bench fixtures"]
fn the_bench_run_with_facts_applies_the_same_fixes() {
    agrees_with("q5-pick");
}
