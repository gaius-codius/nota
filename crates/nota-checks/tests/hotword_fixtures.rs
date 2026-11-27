//! The hotword diff against the research bench: on every chunk of the three
//! hotword runs (`hw15`, `hw20`, `hw30`, boost 1.5, 2.0 and 3.0), the diff
//! keeps exactly the differences `propose.py` kept, and applying them gives
//! its text.
//!
//! The fixture (`hotword_diff.json`) holds whole chunks of the bench's
//! transcripts, so it stays in the local bench and this test is ignored
//! here; `scripts/bench-fixtures.sh` runs it. The rule's own tests, on
//! invented chunks, are in `src/hotword_diff/`.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::path::PathBuf;

use nota_checks::{HotwordCandidate, HotwordList, hotword_candidates};
use serde_json::Value;

/// The local bench fixtures' folder: `NOTA_BENCH_FIXTURES`, which
/// `scripts/bench-fixtures.sh` sets.
fn bench_fixtures() -> PathBuf {
    std::env::var_os("NOTA_BENCH_FIXTURES")
        .map(PathBuf::from)
        .expect(
            "NOTA_BENCH_FIXTURES names the exported bench fixtures; run scripts/bench-fixtures.sh",
        )
}

/// The text of `field` in `value`.
fn text<'v>(value: &'v Value, field: &str) -> &'v str {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("no {field} in {value}"))
}

/// The greedy words with each candidate's heard words replaced by its
/// boosted ones, as `propose.py` applied them.
fn applied(heard: &[&str], candidates: &[HotwordCandidate]) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut at = 0;
    for c in candidates {
        out.extend(&heard[at..c.heard.start]);
        out.extend(c.replacement.split_whitespace());
        at = c.heard.end;
    }
    out.extend(&heard[at..]);
    out.join(" ")
}

/// What one run's chunks keep, as `heard -> proposed` lines, from the
/// fixture (`want`) and from the diff (`got`), and how many chunks' applied
/// text differs from `propose.py`'s.
fn compare(chunks: &[Value], list: &HotwordList) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut want, mut got, mut texts) = (Vec::new(), Vec::new(), Vec::new());
    for chunk in chunks {
        let heard: Vec<&str> = text(chunk, "greedy").split_whitespace().collect();
        let boosted: Vec<&str> = text(chunk, "biased").split_whitespace().collect();
        let found = hotword_candidates(&heard, &boosted, list).unwrap();
        for d in chunk["diffs"].as_array().unwrap() {
            if d["kept"].as_bool().unwrap() {
                want.push(format!("{} -> {}", text(d, "heard"), text(d, "proposed")));
            }
        }
        got.extend(
            found
                .iter()
                .map(|c| format!("{} -> {}", c.heard_text, c.replacement)),
        );
        if applied(&heard, &found) != text(chunk, "result") {
            texts.push(format!("chunk {} of {}", chunk["chunk"], chunk["excerpt"]));
        }
    }
    (want, got, texts)
}

/// Every hotword run keeps the bench's differences and gives its text. The
/// diff's C1 keeps digits where `propose.py`'s ratio dropped them; no
/// bench decision changes, so there is no difference to list.
#[test]
#[ignore = "needs the local bench fixtures"]
fn every_hotword_run_keeps_what_propose_kept() {
    let raw = std::fs::read_to_string(bench_fixtures().join("hotword_diff.json")).unwrap();
    let fixture: Value = serde_json::from_str(&raw).unwrap();
    // The fixture lists the glossary's listed words; as terms, each lists
    // itself, which is the rule `propose.py` ran.
    let words: Vec<&str> = fixture["glossary_words"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    let list = HotwordList::new(words.iter().copied());
    let mut kept = Vec::new();
    for run in ["hw15", "hw20", "hw30"] {
        let chunks = fixture["runs"][run].as_array().unwrap();
        // Three excerpts of twenty 30 s chunks each.
        assert_eq!(chunks.len(), 60, "{run}"); // check-bound
        let (want, got, texts) = compare(chunks, &list);
        assert_eq!(got, want, "{run}");
        assert_eq!(texts, Vec::<String>::new(), "{run}");
        kept.push(got.len());
    }
    // propose.py's counts: 13, 16 and 23 kept (hw30's 23 are the issue's
    // "14 of 21 terms", scored by score_a.py on the applied text).
    assert_eq!(kept, [13, 16, 23]); // check-bound
}
