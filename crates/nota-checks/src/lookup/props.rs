//! Properties of the term lookup, over invented text and term lists:
//! - no candidate's window has fewer than 5 letters and digits;
//! - at most 6 candidates per chunk;
//! - no candidate's window has a word that is a term, or is a term;
//! - the term list's order, and terms listed twice, don't change the
//!   result;
//! - no two candidates share a word, and each one's span holds its window.

use proptest::prelude::*;

use super::*;
use crate::language::english;

/// Terms the lists are drawn from: one and several words, a digit.
const TERMS: [&str; 12] = [
    "amlodipine",
    "nifedipine",
    "felodipine",
    "ibuprofen",
    "naproxen",
    "beta blocker",
    "heart failure",
    "type 1 diabetes",
    "type 2 diabetes",
    "NSAID",
    "GLP-1",
    "ramipril",
];

/// Aliases a term may have.
const ALIASES: [&str; 5] = [
    "a dlp one",
    "endside",
    "en said",
    "gee el pee one",
    "ram a pril",
];

/// Words the chunks are made of: common and function words, the terms'
/// words, mishearings, numbers and punctuation.
const WORDS: [&str; 34] = [
    "the",
    "a",
    "of",
    "and",
    "to",
    "patient",
    "dose",
    "daily",
    "with",
    "for",
    "one",
    "amlodipine",
    "amlodipin",
    "amlo",
    "dipin",
    "a",
    "dlp",
    "ibuprofin",
    "eye",
    "bee",
    "profen",
    "beta",
    "blocker",
    "bluk",
    "heart",
    "failure",
    "type",
    "diabetes",
    "endside",
    "en",
    "said",
    "2",
    "45-year-old",
    "nifedipin",
];

/// A term list: some of [`TERMS`], each with some of [`ALIASES`].
fn any_terms() -> impl Strategy<Value = Vec<Term>> {
    proptest::collection::vec(
        (
            proptest::sample::select(&TERMS[..]),
            proptest::sample::subsequence(&ALIASES[..], 0..2),
        ),
        0..10,
    )
    .prop_map(|list| {
        list.into_iter()
            .map(|(t, aliases)| Term {
                term: t.to_owned(),
                aliases: aliases.into_iter().map(str::to_owned).collect(),
                language: "en".to_owned(),
            })
            .collect()
    })
}

/// A chunk: words from [`WORDS`] and random letters, with punctuation.
fn any_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            4 => proptest::sample::select(&WORDS[..]).prop_map(str::to_owned),
            1 => "[a-z]{1,10}[,.]?",
        ],
        0..30,
    )
    .prop_map(|words| words.join(" "))
}

/// The candidates `terms` offer in `text`, in English, with nothing
/// rejected.
fn find(terms: &[Term], text: &str) -> Vec<Candidate> {
    Lookup::new(english(), terms).find(text, &|_, _| true)
}

proptest! {
    /// No candidate's window has fewer than 5 letters and digits.
    #[test]
    fn no_candidate_is_under_five_letters(terms in any_terms(), text in any_text()) {
        for c in find(&terms, &text) {
            prop_assert!(letters(c.window()).len() >= MIN_LETTERS, "{:?}", c);
        }
    }

    /// A chunk gets at most 6 candidates.
    #[test]
    fn at_most_six_candidates(terms in any_terms(), text in any_text()) {
        let found = find(&terms, &text);
        prop_assert!(found.len() <= MAX_CANDIDATES, "{:?}", found);
    }

    /// No candidate's window has a word that is a term, or is a term.
    #[test]
    fn no_candidate_holds_a_known_term(terms in any_terms(), text in any_text()) {
        let known: Vec<String> = terms.iter().map(|t| letters(&t.term)).collect();
        for c in find(&terms, &text) {
            let window = letters(c.window());
            prop_assert!(!known.contains(&window), "{:?}", c);
            for (word, _) in window_words(c.window()) {
                prop_assert!(!known.contains(&letters(word)), "{:?}", c);
            }
        }
    }

    /// Reversing the term list and listing a term twice change nothing.
    #[test]
    fn the_term_lists_order_doesnt_matter(terms in any_terms(), text in any_text(), shift in 0usize..10) {
        let mut other = terms.clone();
        other.reverse();
        if !other.is_empty() {
            let n = other.len();
            other.rotate_left(shift % n);
            other.push(other[0].clone());
        }
        prop_assert_eq!(find(&terms, &text), find(&other, &text));
    }

    /// No two candidates share a word, and each one's span in the chunk
    /// holds its window's words.
    #[test]
    fn candidates_are_apart_and_where_they_say(terms in any_terms(), text in any_text()) {
        let found = find(&terms, &text);
        for (i, a) in found.iter().enumerate() {
            let words: Vec<&str> = window_words(&text[a.span()]).into_iter().map(|(w, _)| w).collect();
            prop_assert_eq!(words.join(" "), a.window());
            for b in &found[i + 1..] {
                prop_assert!(a.span().end <= b.span().start || b.span().end <= a.span().start, "{:?} {:?}", a, b);
            }
        }
    }
}
