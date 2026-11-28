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

/// Words are runs of letters and digits of any script, lowercased, with an
/// apostrophe inside a word kept.
#[test]
fn words_are_letters_and_digits_of_any_script() {
    assert_eq!(
        words("Type-2 diabetes, İnhale α patient's don’t 'quoted'"),
        [
            "type",
            "2",
            "diabetes",
            "inhale",
            "α",
            "patient's",
            "don't",
            "quoted"
        ]
    );
    assert_eq!(words("  "), Vec::<String>::new());
}

/// Each word's span ends where its last character does.
#[test]
fn spans_end_after_each_word() {
    assert_eq!(
        spans("a bé, c"),
        [
            (1, "a".to_owned()),
            (5, "bé".to_owned()),
            (8, "c".to_owned())
        ]
    );
}

/// A possessive isn't the letter after its apostrophe: "S phase" after
/// "patient's" doesn't repeat it.
#[test]
fn a_possessive_is_one_word() {
    assert!(!repeats_before("the patient's ", "S phase"));
}

/// A letter of another script is a word: "α amylase" after "α" repeats it.
#[test]
fn a_greek_letter_is_a_word() {
    assert!(repeats_before("this is α ", "α amylase"));
}
