//! C4 against the research bench: the term lookup offers the candidates the
//! bench's `lookup.py` `find()` offered.
//!
//! The committed fixture (`fixtures/c4_invented.json`) holds invented chunks
//! and the candidates `find()` offers in each, at full precision; its terms
//! are sorted, so the bench's tie-break (the term listed first) is nota's
//! (the term that sorts first). The bench's own 60 chunks are workshop text
//! and stay in the local bench; their test is ignored here and run by
//! `scripts/bench-fixtures.sh`. The committed file is written by the bench's
//! `fixtures/c4_invented.py`, the local one by `fixtures/export.py`.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use nota_checks::{English, Lookup, Matched, Offered, Proposal, Term, gate, repeats_before};
use serde_json::Value;

/// English's rules, unpacked once per test process.
static ENGLISH: LazyLock<English> = LazyLock::new(|| English::new().unwrap());

/// One candidate: its window, its term and its score.
type Found = (String, String, f64);

/// The fixture at `path`, parsed.
fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("fixture is JSON")
}

/// `name` as an English term with no aliases.
fn term(name: &str) -> Term {
    Term {
        term: name.to_owned(),
        aliases: Vec::new(),
        language: "en".to_owned(),
    }
}

/// The candidates a fixture chunk lists.
fn listed(chunk: &Value) -> Vec<Found> {
    chunk["candidates"]
        .as_array()
        .expect("a candidate list")
        .iter()
        .map(|c| {
            (
                c["window"].as_str().expect("a window").to_owned(),
                c["term"].as_str().expect("a term").to_owned(),
                c["score"].as_f64().expect("a score"),
            )
        })
        .collect()
}

/// The candidates `lookup` finds in `text`, as the fixture lists them.
fn found(lookup: &Lookup<'_>, text: &str) -> Vec<Found> {
    lookup
        .find(text, &|_, _| true)
        .iter()
        .map(|c| {
            let Matched::Scored(score) = c.matched() else {
                panic!("{c:?}: no term has aliases")
            };
            (c.window().to_owned(), c.term().to_owned(), score.total)
        })
        .collect()
}

/// Whether `got` is `want`, in order, each score within `tolerance`.
fn same(got: &[Found], want: &[Found], tolerance: f64) -> bool {
    got.len() == want.len()
        && got
            .iter()
            .zip(want)
            .all(|(g, w)| g.0 == w.0 && g.1 == w.1 && (g.2 - w.2).abs() <= tolerance)
}

/// Where `window`, words joined by one space, first is in `text`: the byte
/// its first word starts at. Words are the lookup's, runs of ASCII letters,
/// digits, apostrophes and hyphens.
fn window_start(text: &str, window: &str) -> Option<usize> {
    let in_word = |c: char| c.is_ascii_alphanumeric() || c == '\'' || c == '-';
    let mut words: Vec<(usize, &str)> = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        match (start, in_word(c)) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                words.push((s, &text[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    let n = window.split(' ').count();
    words.windows(n).find_map(|run| {
        let joined: Vec<&str> = run.iter().map(|&(_, w)| w).collect();
        (joined.join(" ") == window).then_some(run[0].0)
    })
}

/// `want`, the bench's candidates for `text`, without those nota passes
/// over deliberately: a term that starts with the words just before its
/// window and goes on past them ("beta blocker" for "blocker" after
/// "beta"), which a yes would make read twice (the spec, C4, "New here").
/// The bench offered them; the count left out is returned too.
fn without_repeats(text: &str, want: Vec<Found>) -> (Vec<Found>, usize) {
    let before = want.len();
    let kept: Vec<Found> = want
        .into_iter()
        .filter(|(window, term, _)| {
            let start = window_start(text, window)
                .unwrap_or_else(|| panic!("{window:?} isn't in {text:?}"));
            !repeats_before(&text[..start], term)
        })
        .collect();
    let left_out = before - kept.len();
    (kept, left_out)
}

/// Every invented chunk gets the bench's candidates, in its order, with its
/// scores to the last bits, but for those that repeat the words before
/// their window.
#[test]
fn every_invented_chunk_gets_the_bench_candidates() {
    let fixture =
        read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c4_invented.json"));
    let terms: Vec<Term> = fixture["terms"]
        .as_array()
        .expect("a term list")
        .iter()
        .map(|t| term(t.as_str().expect("a term")))
        .collect();
    let lookup = Lookup::new(&*ENGLISH, &terms);
    let chunks = fixture["chunks"].as_array().expect("a chunk list");
    let mut wrong = Vec::new();
    let mut full = 0;
    let mut repeats = 0;
    for chunk in chunks {
        let text = chunk["text"].as_str().expect("a text");
        full += usize::from(listed(chunk).len() == nota_checks::MAX_CANDIDATES);
        let (want, left_out) = without_repeats(text, listed(chunk));
        repeats += left_out;
        let got = found(&lookup, text);
        if !same(&got, &want, 1e-12) {
            wrong.push(format!("{text:?}\n  got   {got:?}\n  bench {want:?}"));
        }
    }
    // The fixture reaches the cap of 6 in 22 of its 250 chunks; a fixture
    // that never reached it couldn't show the cap holds.
    let count = chunks.len();
    assert!(count >= 250 && full >= 20, "{count} chunks, {full} full"); // check-bound
    // The bench offered 9 terms that repeat the words before their window
    // ("beta blocker" for "blocker" after "beta"); a fixture with none
    // couldn't show they're passed over.
    assert_eq!(repeats, 9, "{repeats} repeats left out"); // check-bound
    assert_eq!(wrong, Vec::<String>::new());
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

/// The bench chunks where nota offers another term, deliberately: excerpt,
/// chunk, the bench's term and nota's. Each is in the spec (C4, "Ported").
/// Two terms tie exactly when Double Metaphone drops the digit that tells
/// them apart; the bench kept the one listed first in its term file, and
/// nota keeps the one that sorts first, so the list's order doesn't matter.
const DIFFERENCES: &[(u64, u64, &str, &str)] = &[(1200, 6, "type 2 diabetes", "type 1 diabetes")];

/// `want` with the bench's term swapped for nota's where [`DIFFERENCES`]
/// lists the chunk at `at`.
fn with_differences(at: (u64, u64), mut want: Vec<Found>) -> Vec<Found> {
    for &(excerpt, chunk, bench, nota) in DIFFERENCES {
        if (excerpt, chunk) == at {
            for found in &mut want {
                if found.1 == bench {
                    nota.clone_into(&mut found.1);
                }
            }
        }
    }
    want
}

/// Every chunk of the bench gets the candidates the bench offered, but for
/// the differences the spec explains and those that repeat the words
/// before their window.
#[test]
#[ignore = "needs the local bench fixtures"]
fn every_bench_chunk_gets_the_bench_candidates() {
    let fixture = read(&bench_fixtures().join("c4_lookup.json"));
    let terms: Vec<Term> = fixture["terms"]
        .as_array()
        .expect("a term list")
        .iter()
        .map(|t| term(t["term"].as_str().expect("a term")))
        .collect();
    let lookup = Lookup::new(&*ENGLISH, &terms);
    let chunks = fixture["chunks"].as_array().expect("a chunk list");
    let mut wrong = Vec::new();
    let mut offered = 0;
    let mut repeats = 0;
    for chunk in chunks {
        let text = chunk["text"].as_str().expect("a text");
        let at = (
            chunk["excerpt"].as_u64().unwrap(),
            chunk["chunk"].as_u64().unwrap(),
        );
        let want = listed(chunk);
        offered += want.len();
        let (want, left_out) = without_repeats(text, with_differences(at, want));
        repeats += left_out;
        let got = found(&lookup, text);
        // The bench's scores are rounded to three places.
        if !same(&got, &want, 0.000_5 + 1e-9) {
            wrong.push(format!("{at:?}\n  got   {got:?}\n  bench {want:?}"));
        }
    }
    // The export holds 74 candidates in 60 chunks (laya/cands.json).
    let count = chunks.len();
    assert!(count == 60 && offered == 74, "{count} chunks, {offered}"); // check-bound
    // The T3 bench's one wrong fix: "type 2 diabetes" for "2 diabetes"
    // after "type" (excerpt 8400, chunk 16).
    assert_eq!(repeats, 1, "{repeats} repeats left out"); // check-bound
    assert_eq!(wrong, Vec::<String>::new());
}

/// The chunk as clean-up revises it from `lookup`'s candidates in `text`,
/// with a yes to each: the window at its place, the term in its stead, and
/// the gate deciding.
fn revised_on_yes(lookup: &Lookup<'_>, text: &str) -> String {
    let found = lookup.find(text, &|_, _| true);
    let offered: Vec<Offered> = found
        .iter()
        .map(|c| Offered {
            window: c.window().to_owned(),
            term: c.term().to_owned(),
        })
        .collect();
    let yeses: Vec<Proposal> = found
        .iter()
        .map(|c| Proposal {
            heard: c.window().to_owned(),
            replacement: c.term().to_owned(),
            at: Some(c.span()),
        })
        .collect();
    gate(text, &offered, &yeses).text().to_owned()
}

/// The T3 bench's one wrong fix, on invented text: where the chunk already
/// reads "type 2 diabetes", a yes to every candidate leaves it as it was,
/// since "type 2 diabetes" isn't offered for "2 diabetes" after "type".
#[test]
fn type_2_diabetes_heard_right_is_not_made_to_read_type_twice() {
    let terms = [term("type 2 diabetes")];
    let lookup = Lookup::new(&*ENGLISH, &terms);
    let text = "She was told she has type 2 diabetes last year.";
    assert_eq!(revised_on_yes(&lookup, text), text);
}

/// The same term for words with another word before them is offered, and
/// applied on a yes.
#[test]
fn two_diabetes_is_still_fixed_on_a_yes() {
    let terms = [term("type 2 diabetes")];
    let lookup = Lookup::new(&*ENGLISH, &terms);
    assert_eq!(
        revised_on_yes(&lookup, "She was told she has two diabetes last year."),
        "She was told she has type 2 diabetes last year."
    );
}
