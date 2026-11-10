use super::*;

/// The score of `a` against `b`, for cases that are never refused.
/// Compared as a [`Similarity`], so each assert claims the exact float
/// Python computes.
fn score(a: &str, b: &str) -> Similarity {
    letter_similarity(a, b).unwrap()
}

/// A score of `value`, to compare with.
fn exactly(value: f64) -> Similarity {
    Similarity(value)
}

/// Two strings that are both empty once normalised score 1, as in Python.
#[test]
fn two_empty_strings_score_one() {
    assert_eq!(score("", ""), exactly(1.0));
    // Punctuation alone normalises to nothing, so it scores 1 too.
    assert_eq!(score("!!", "--"), exactly(1.0));
}

/// One empty side has nothing to match, so it scores 0.
#[test]
fn one_empty_side_scores_zero() {
    assert_eq!(score("", "amlodipine"), exactly(0.0));
    assert_eq!(score("amlodipine", "..."), exactly(0.0));
}

/// The longest block found first is kept on a tie: in "aaa" against "aaba"
/// that's "aa" at the start, leaving "a" to match after it.
#[test]
fn a_tie_goes_to_the_first_block() {
    assert_eq!(score("aaa", "aaba"), exactly(6.0 / 7.0));
}

/// Swapping the strings can change the score, so the heard span must go
/// first.
#[test]
fn the_order_of_the_strings_matters() {
    assert_eq!(score("aba", "bca"), exactly(1.0 / 3.0));
    assert_eq!(score("bca", "aba"), exactly(2.0 / 3.0));
}

/// Blocks either side of the longest are matched too.
#[test]
fn blocks_either_side_of_the_longest_count() {
    // "cd" first, then "ab" before it and "ef" after it.
    assert_eq!(score("qabxcdyefz", "abcdef"), exactly(12.0 / 16.0));
}

/// Each side may have up to [`MAX_LETTERS`] letters and digits; one more is
/// refused, counting only what's kept.
#[test]
fn the_limit_counts_letters_and_digits() {
    let at_limit = "a".repeat(MAX_LETTERS);
    assert_eq!(score(&at_limit, "a"), exactly(2.0 / 200.0));
    assert_eq!(score("a", &at_limit), exactly(2.0 / 200.0));
    // Spaces and dashes don't count towards the limit.
    assert_eq!(score(&"a- ".repeat(MAX_LETTERS), "a"), exactly(2.0 / 200.0));
    let over = "a".repeat(MAX_LETTERS + 1);
    assert_eq!(letter_similarity(&over, "a"), Err(TooLong));
    assert_eq!(letter_similarity("a", &over), Err(TooLong));
}

/// The refusal says what the limit is.
#[test]
fn too_long_names_the_limit() {
    assert_eq!(
        TooLong.to_string(),
        "too long to compare: over 199 letters and digits"
    );
}
