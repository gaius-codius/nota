//! Property tests for C2, the candidate gate.
//!
//! The contract, checked on every input, with its own reading of the rules
//! rather than the gate's helpers:
//! - every proposal gets one decision, in order, and no input panics;
//! - no applied fix holds a term that wasn't offered for its words: some
//!   offered term with letters is inside the replacement's letters, and
//!   that candidate's window shares a word with the replaced text;
//! - an applied fix replaces its own span, on word boundaries, and scores
//!   at least [`SIMILARITY_MIN`];
//! - the text given is never changed: the revised text is that text with
//!   the fixes' ranges, which don't overlap, replaced, and nothing else;
//! - with nothing offered, nothing is applied.

use std::collections::BTreeSet;

use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;

use super::*;

/// Words the cases are built from: terms and the ways they're misheard,
/// words around them, punctuation, a digit run inside a longer one, an
/// apostrophe, a hyphen, and the Kelvin sign, which lowercases to `k`.
const VOCAB: &[&str] = &[
    "amylo",
    "diping",
    "amlodipine",
    "Amlo-Dipine",
    "edema",
    "oedema",
    "Inhale",
    "inhaler",
    "dose",
    "dose's",
    "follow-up",
    "up",
    "100",
    "1000",
    "NSAID",
    "NSAIDs",
    "hyperprofen",
    "ibuprofen",
    "\u{212A}elvin",
    "kelvin",
    "the",
    "and",
    "!!",
    "--",
];

/// What goes after a word in a chunk.
const SEPARATORS: &[&str] = &[" ", " ", " ", ", ", "-", "'", ". "];

/// A fix for each of some of [`VOCAB`]'s words, close enough in its
/// letters that a proposal made of them can pass C1.
const FIXES: &[(&str, &str)] = &[
    ("amylo", "amlo"),
    ("diping", "dipine"),
    ("edema", "oedema"),
    ("Inhale", "inhaler"),
    ("dose", "doze"),
    ("up", "cup"),
    ("hyperprofen", "ibuprofen"),
    ("NSAID", "NSAIDs"),
    ("kelvin", "Kelvin"),
];

/// `heard` with each word whose `swap` is set put through [`FIXES`].
fn fixed(heard: &str, swap: &[bool]) -> String {
    let words = heard.split(' ').enumerate().map(|(i, w)| {
        let fix = FIXES.iter().find(|(from, _)| *from == w).map(|(_, to)| *to);
        match fix {
            Some(to) if swap.get(i).copied().unwrap_or(true) => to,
            _ => w,
        }
    });
    words.collect::<Vec<_>>().join(" ")
}

/// A term a candidate offers: a fix from [`FIXES`], or any phrase.
fn any_term() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => prop::sample::select(FIXES).prop_map(|(_, to)| to.to_owned()),
        1 => any_phrase(2),
    ]
}

/// One gate call's inputs.
#[derive(Debug, Clone)]
struct Case {
    /// The chunk's text.
    chunk: String,
    /// The candidates offered for it.
    offered: Vec<Offered>,
    /// What the model proposed.
    proposals: Vec<Proposal>,
}

/// One to `max` words from [`VOCAB`], joined by spaces.
fn any_phrase(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(VOCAB), 1..=max).prop_map(|w| w.join(" "))
}

/// The text of `parts`, each word followed by its separator, except that
/// the last one's is left off.
fn joined(parts: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (word, sep) in parts {
        out.push_str(word);
        out.push_str(sep);
    }
    if let Some((_, sep)) = parts.last() {
        out.truncate(out.len() - sep.len());
    }
    out
}

/// A case built from [`VOCAB`]: a chunk of up to a dozen words,
/// candidates mostly for runs of its words, and proposals whose spans are
/// mostly runs of its words and whose replacements mostly fix them.
fn any_case() -> impl Strategy<Value = Case> {
    let parts = prop::collection::vec(
        (
            prop::sample::select(VOCAB),
            prop::sample::select(SEPARATORS),
        ),
        1..12,
    );
    parts.prop_flat_map(|parts| {
        let n = parts.len();
        let chunk = joined(&parts);
        // A run of the chunk's own words is usually found; a phrase from
        // the vocabulary may not be.
        let own = (0..n, 1..=3_usize).prop_map(move |(i, len)| joined(&parts[i..(i + len).min(n)]));
        let span = prop_oneof![3 => own, 1 => any_phrase(3)];
        let window = span.clone();
        let offered = prop::collection::vec(
            (window, any_term()).prop_map(|(window, term)| Offered { window, term }),
            0..4,
        );
        let proposal = (
            span,
            prop::collection::vec(any::<bool>(), 3),
            any_phrase(3),
            0..4_u8,
        )
            .prop_map(|(heard, swap, other, pick)| {
                // Mostly the span with its words fixed; sometimes a phrase.
                let replacement = if pick == 0 {
                    other
                } else {
                    fixed(&heard, &swap)
                };
                Proposal { heard, replacement }
            });
        (Just(chunk), offered, prop::collection::vec(proposal, 0..5)).prop_map(
            |(chunk, offered, proposals)| Case {
                chunk,
                offered,
                proposals,
            },
        )
    })
}

/// A case of any text, Unicode included: a chunk, spans cut from it or
/// made up, and candidates of any text.
fn any_text_case() -> impl Strategy<Value = Case> {
    (".{0,40}", any_offered_text()).prop_flat_map(|(chunk, offered)| {
        let len = chunk.chars().count();
        let cut = chunk.clone();
        // A span of the chunk's own characters, so it's sometimes found.
        let own = (0..=len, 0..=len).prop_map(move |(a, b)| {
            let (a, b) = (a.min(b), a.max(b));
            cut.chars().skip(a).take(b - a).collect::<String>()
        });
        let heard = prop_oneof![own, ".{0,10}"];
        let proposal =
            (heard, ".{0,20}").prop_map(|(heard, replacement)| Proposal { heard, replacement });
        (
            Just(chunk),
            Just(offered),
            prop::collection::vec(proposal, 0..4),
        )
            .prop_map(|(chunk, offered, proposals)| Case {
                chunk,
                offered,
                proposals,
            })
    })
}

/// Up to three candidates of any text.
fn any_offered_text() -> impl Strategy<Value = Vec<Offered>> {
    prop::collection::vec(
        (".{0,12}", ".{0,8}").prop_map(|(window, term)| Offered { window, term }),
        0..3,
    )
}

/// `s`'s ASCII letters and digits once lowercased, read the plain way.
fn plain_letters(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

/// `s`'s words once lowercased: its runs of ASCII letters and digits.
fn plain_words(s: &str) -> BTreeSet<String> {
    s.to_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether some candidate in `offered` was offered for `fix`'s words and
/// has its term in the replacement.
fn offered_for_its_words(offered: &[Offered], fix: &AppliedFix) -> bool {
    let replacement = plain_letters(fix.replacement());
    let heard = plain_words(fix.heard());
    offered.iter().any(|o| {
        let term = plain_letters(&o.term);
        !term.is_empty()
            && replacement.contains(&term)
            && !plain_words(&o.window).is_disjoint(&heard)
    })
}

/// Whether `c` is part of a word for the bench's boundary test.
fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '\''
}

/// The problems with `gated` as the gate's answer to `case`, by the
/// contract above; none for a good answer.
fn problems(case: &Case, gated: &Gated) -> Vec<String> {
    let mut bad = Vec::new();
    if gated.decisions().len() != case.proposals.len() {
        bad.push(format!("{} decisions", gated.decisions().len()));
    }
    for fix in gated.applied() {
        if !offered_for_its_words(&case.offered, fix) {
            bad.push(format!("not offered: {fix:?}"));
        }
        if case.chunk.get(fix.range()) != Some(fix.heard()) {
            bad.push(format!("not the chunk's text: {fix:?}"));
        }
        if fix.similarity().value() < SIMILARITY_MIN {
            bad.push(format!("under the threshold: {fix:?}"));
        }
    }
    let expected = splice(&case.chunk, gated, &mut bad);
    if gated.text() != expected {
        bad.push(format!("text {:?}, expected {expected:?}", gated.text()));
    }
    bad
}

/// `chunk` with `gated`'s fixes put in by hand, noting fixes that overlap
/// or aren't on character boundaries in `bad`.
fn splice(chunk: &str, gated: &Gated, bad: &mut Vec<String>) -> String {
    let mut fixes: Vec<_> = gated.applied().collect();
    fixes.sort_by_key(|f| f.range().start);
    let mut out = String::new();
    let mut at = 0;
    for fix in fixes {
        let r = fix.range();
        match chunk.get(at..r.start) {
            Some(kept) => out.push_str(kept),
            None => bad.push(format!("overlaps or splits a character: {fix:?}")),
        }
        out.push_str(fix.replacement());
        at = r.end;
    }
    out.push_str(chunk.get(at..).unwrap_or_default());
    out
}

/// The problems with each applied fix's place: its text must match the
/// proposal's span, ASCII case aside, with no word character either side.
fn placement_problems(case: &Case, gated: &Gated) -> Vec<String> {
    let mut bad = Vec::new();
    for (proposal, decision) in case.proposals.iter().zip(gated.decisions()) {
        let Some(fix) = decision.applied() else {
            continue;
        };
        if fix.heard().to_lowercase() != proposal.heard.to_lowercase() {
            bad.push(format!("{proposal:?} placed at {fix:?}"));
        }
        let r = fix.range();
        let before = case.chunk[..r.start].chars().next_back();
        let after = case.chunk[r.end..].chars().next();
        if before.is_some_and(word_char) || after.is_some_and(word_char) {
            bad.push(format!("inside a word: {fix:?}"));
        }
        let want = letter_similarity(&proposal.heard, &proposal.replacement).ok();
        if want != Some(fix.similarity()) {
            bad.push(format!("score {want:?} for {fix:?}"));
        }
    }
    bad
}

proptest! {
    // Each case is small, and most proposals are refused, so more of them.
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// On cases from the vocabulary, every answer keeps the contract.
    #[test]
    fn the_gate_keeps_its_contract(case in any_case()) {
        let gated = gate(&case.chunk, &case.offered, &case.proposals);
        prop_assert_eq!(problems(&case, &gated), Vec::<String>::new());
    }

    /// On cases of any text, no input panics and the contract holds.
    #[test]
    fn the_gate_keeps_its_contract_on_any_text(case in any_text_case()) {
        let gated = gate(&case.chunk, &case.offered, &case.proposals);
        prop_assert_eq!(problems(&case, &gated), Vec::<String>::new());
    }

    /// Each applied fix replaces its own span, on word boundaries, with the
    /// score C1 gives the proposal.
    #[test]
    fn an_applied_fix_replaces_its_own_span(case in any_case()) {
        let gated = gate(&case.chunk, &case.offered, &case.proposals);
        prop_assert_eq!(placement_problems(&case, &gated), Vec::<String>::new());
    }

    /// With no candidates offered, nothing is applied and the text stays.
    #[test]
    fn with_nothing_offered_nothing_is_applied(case in any_case()) {
        let gated = gate(&case.chunk, &[], &case.proposals);
        prop_assert_eq!(gated.applied().count(), 0);
        prop_assert_eq!(gated.text(), case.chunk.as_str());
    }
}

/// The vocabulary's cases reach every verdict but the over-long one, so the
/// properties above aren't passing on cases that are all refused early.
#[test]
fn the_cases_reach_every_verdict() {
    let mut runner = TestRunner::deterministic();
    let strategy = any_case();
    let mut applied = 0;
    let mut seen = BTreeSet::new();
    for _ in 0..512 {
        let case = strategy.new_tree(&mut runner).unwrap().current();
        let gated = gate(&case.chunk, &case.offered, &case.proposals);
        for decision in gated.decisions() {
            match decision.rejection() {
                None => applied += 1,
                Some(rule) => {
                    seen.insert(format!("{rule:?}"));
                }
            }
        }
    }
    // 44 of 512 cases' proposals are applied with this seed.
    assert!(applied >= 30, "{applied} applied"); // check-bound
    let every = [
        "AlreadyChanged",
        "Dissimilar",
        "EmptySpan",
        "NotOffered",
        "SpanNotFound",
        "WindowElsewhere",
    ];
    assert_eq!(seen, every.iter().map(|r| (*r).to_owned()).collect()); // check-bound
}
