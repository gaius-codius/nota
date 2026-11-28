//! The repeat check on the cases that bound it.

use super::*;

/// The T3 bench's fix: the replacement's first word is the word before.
#[test]
fn a_replacement_starting_with_the_word_before_repeats_it() {
    assert!(repeats_before("she has type ", "type 2 diabetes"));
}

/// Two words before, "type 2", are the replacement's first two.
#[test]
fn a_replacement_starting_with_the_two_words_before_repeats_them() {
    assert!(repeats_before("she has type 2 ", "type 2 diabetes"));
}

/// Case, punctuation and hyphens don't count: words are runs of letters
/// and digits.
#[test]
fn case_punctuation_and_hyphens_do_not_count() {
    assert!(repeats_before("She has Type-", "type 2 diabetes"));
    assert!(repeats_before("she has type.", "Type 2 diabetes"));
    assert!(repeats_before("she has type-2 ", "type 2 diabetes"));
}

/// Other words before, or none, don't repeat.
#[test]
fn other_words_before_do_not_repeat() {
    assert!(!repeats_before("she has ", "type 2 diabetes"));
    assert!(!repeats_before("", "type 2 diabetes"));
    assert!(!repeats_before("she has type ", ""));
    // Only the last words count: "type" earlier doesn't.
    assert!(!repeats_before("type of a ", "type 2 diabetes"));
    // A whole word: "types" isn't "type".
    assert!(!repeats_before("she has types ", "type 2 diabetes"));
}

/// The last words before must be the replacement's first, in order: "type
/// 1" before "type 2 diabetes" shares its first word but doesn't end on it.
#[test]
fn the_words_before_must_end_on_the_first_words() {
    assert!(!repeats_before("she has type 1 ", "type 2 diabetes"));
    assert!(!repeats_before("she has 2 type 1 ", "type 2 diabetes"));
    assert!(!repeats_before("2 ", "type 2 diabetes"));
}

/// A replacement that is wholly the words before is a term said twice,
/// not a repeat: the fix writes what was said.
#[test]
fn a_replacement_that_is_wholly_the_words_before_is_not_a_repeat() {
    assert!(!repeats_before("then furosemide ", "furosemide"));
    assert!(!repeats_before(
        "she has type 2 diabetes ",
        "type 2 diabetes"
    ));
    // One word past them is.
    assert!(repeats_before("then furosemide ", "furosemide forty"));
}

/// Words are split as the gate's rule 4 splits them.
#[test]
fn words_are_the_gate_words_in_order() {
    assert_eq!(
        words("Type-2 diabetes, İnhale"),
        ["type", "2", "diabetes", "inhale"]
    );
    assert_eq!(words("  "), Vec::<String>::new());
}
