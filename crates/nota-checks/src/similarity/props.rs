//! Property tests for C1, letter similarity.
//!
//! The contract, checked on every input:
//! - the score is between 0 and 1;
//! - a string scores 1 against itself;
//! - the case of ASCII letters, and characters other than ASCII letters and
//!   digits, don't change the score (non-ASCII letters are dropped, but
//!   their case can change how many there are: "ß" upper-cases to "SS", so
//!   these strings are ASCII);
//! - two strings with no letter or digit in common score 0, unless both are
//!   empty once normalised, which Python's difflib scores 1;
//! - a side with more than [`MAX_LETTERS`] letters and digits is refused.

use proptest::prelude::*;

use super::*;

/// Any text, Unicode included, short enough never to be refused.
fn any_text() -> impl Strategy<Value = String> {
    ".{0,60}"
}

/// ASCII letters and digits: the characters C1 keeps.
fn any_kept() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9]{0,30}"
}

/// `s` with each letter's case set from `upper`, and the punctuation in
/// `noise` put after the characters it's paired with.
fn disguise(s: &str, upper: &[bool], noise: &[&str]) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        let up = upper.get(i).copied().unwrap_or(false);
        out.push(if up {
            c.to_ascii_uppercase()
        } else {
            c.to_ascii_lowercase()
        });
        out.push_str(noise.get(i).copied().unwrap_or(""));
    }
    out
}

/// Characters C1 drops: ASCII punctuation, spaces and non-ASCII letters
/// that lower-case to no ASCII letter.
fn any_noise() -> impl Strategy<Value = Vec<&'static str>> {
    prop::collection::vec(
        prop::sample::select(vec![
            "", "", " ", "-", "'", ".", ",", "!", "\t", "é", "ß", "日",
        ]),
        0..30,
    )
}

proptest! {
    /// Every score is a ratio between 0 and 1.
    #[test]
    fn the_score_is_between_zero_and_one(a in any_text(), b in any_text()) {
        let score = letter_similarity(&a, &b).unwrap().value();
        prop_assert!((0.0..=1.0).contains(&score), "{a:?} {b:?}: {score}");
    }

    /// The same text scores 1, whatever it holds.
    #[test]
    fn identical_strings_score_one(a in any_text()) {
        prop_assert_eq!(letter_similarity(&a, &a), Ok(Similarity(1.0)));
    }

    /// Case and punctuation are normalised away before the comparison.
    #[test]
    fn case_and_punctuation_dont_change_the_score(
        a in any_kept(),
        b in any_kept(),
        upper in prop::collection::vec(any::<bool>(), 0..30),
        noise in any_noise(),
    ) {
        let plain = letter_similarity(&a, &b).unwrap();
        // Disguise each side differently, so neither matches the other's
        // spelling by accident.
        let mut flipped = upper.clone();
        flipped.reverse();
        let a2 = disguise(&a, &upper, &noise);
        let b2 = disguise(&b, &flipped, &noise);
        prop_assert_eq!(letter_similarity(&a2, &b2).unwrap(), plain, "{:?} {:?}", a2, b2);
    }

    /// Strings that share no letter or digit score 0.
    #[test]
    fn strings_with_nothing_in_common_score_zero(a in "[a-mA-M0-4]{1,30}", b in "[n-zN-Z5-9]{1,30}") {
        prop_assert_eq!(letter_similarity(&a, &b), Ok(Similarity(0.0)));
    }

    /// A side longer than the limit is refused, whichever side it is.
    #[test]
    fn a_side_over_the_limit_is_refused(extra in 1..50_usize, short in any_kept()) {
        let long = "a".repeat(MAX_LETTERS + extra);
        prop_assert_eq!(letter_similarity(&long, &short), Err(TooLong));
        prop_assert_eq!(letter_similarity(&short, &long), Err(TooLong));
    }
}
