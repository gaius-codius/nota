//! Unit tests for the hotword diff, on invented chunks.

use super::*;

/// The words of `text`, split on whitespace.
fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// The candidates for `heard` against `boosted` with `terms` listed.
fn found(heard: &str, boosted: &str, terms: &[&str]) -> Vec<HotwordCandidate> {
    propose(
        &words(heard),
        &words(boosted),
        &HotwordList::new(terms.iter().copied()),
    )
    .unwrap()
}

/// What a candidate changes, as `heard -> replacement (term, word)`.
fn changes(candidates: &[HotwordCandidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|c| {
            format!(
                "{} -> {} ({}, {})",
                c.heard_text, c.replacement, c.term, c.word
            )
        })
        .collect()
}

/// A mishearing the beam pass corrects to a listed term is a candidate,
/// with the heard words' positions and the beam's own spelling.
#[test]
fn a_listed_term_brought_in_by_the_beam_is_a_candidate() {
    let got = found(
        "we start amylo diping daily",
        "we start amlodipine daily",
        &["amlodipine"],
    );
    assert_eq!(
        changes(&got),
        ["amylo diping -> amlodipine (amlodipine, amlodipine)"]
    );
    assert_eq!(got[0].heard, 2..4);
    assert!((got[0].similarity - 6.0 / 7.0).abs() < 1e-12, "{got:?}");
}

/// A term of several words comes in by one of its listed words, which the
/// candidate names for the gate.
#[test]
fn a_term_of_several_words_comes_in_by_one() {
    let got = found(
        "he had varycous veins",
        "he had varicose veins",
        &["varicose veins"],
    );
    assert_eq!(
        changes(&got),
        ["varycous -> varicose (varicose veins, varicose)"]
    );
}

/// A sentence the beam drops is a difference with no boosted words, so it
/// never becomes a candidate, whatever it held.
#[test]
fn a_dropped_sentence_is_no_candidate() {
    let got = found(
        "the plan. we then review amlodipine weekly. good",
        "the plan. good",
        &["amlodipine"],
    );
    assert_eq!(changes(&got), Vec::<String>::new());
}

/// A difference whose heard words already hold a listed word proposes
/// nothing, as in the bench: "self care" to "self-care" isn't new.
#[test]
fn a_term_already_heard_is_not_proposed_again() {
    let got = found(
        "good self care matters",
        "good self-care matters",
        &["self-care"],
    );
    assert_eq!(changes(&got), Vec::<String>::new());
}

/// A difference that brings in a listed word but sounds unlike what was
/// heard (C1 under 0.6) is refused: a long span replaced by one term.
#[test]
fn a_replacement_unlike_the_heard_words_is_refused() {
    let got = found(
        "notes say the patient gained weight. so",
        "notes say Node safety so",
        &["safety"],
    );
    assert_eq!(changes(&got), Vec::<String>::new());
}

/// A word the beam inserts has no heard words, which C1 scores 0, so an
/// insertion is never a candidate.
#[test]
fn an_inserted_term_is_no_candidate() {
    let got = found("take it daily", "take amlodipine it daily", &["amlodipine"]);
    assert_eq!(changes(&got), Vec::<String>::new());
}

/// Words of three characters or fewer aren't listed, as in `propose.py`:
/// "gp" in "gp review" introduces nothing, "review" does.
#[test]
fn short_words_of_a_term_are_not_listed() {
    let list = HotwordList::new(["gp review"]);
    assert_eq!(list.words, BTreeSet::from(["review".to_owned()]));
    let got = found("see the cheap", "see the gp", &["gp review"]);
    assert_eq!(changes(&got), Vec::<String>::new());
}

/// Digits count in the similarity, as in C1 (where `propose.py` dropped
/// them): "h b a one c" against `HbA1c` is scored on "hbaonec" and
/// "hba1c", 2 × 4 / 12.
#[test]
fn digits_count_in_the_similarity() {
    let got = found(
        "check the h b a one c level",
        "check the HbA1c level",
        &["HbA1c"],
    );
    assert_eq!(changes(&got), ["h b a one c -> HbA1c (HbA1c, hba1c)"]);
    assert!((got[0].similarity - 8.0 / 12.0).abs() < 1e-12, "{got:?}");
}

/// When two listed terms could name a candidate, the first in list order
/// does.
#[test]
fn the_first_listed_term_names_the_candidate() {
    let got = found(
        "a progressing breathliness",
        "a progressive breathlessness",
        &["exertional breathlessness", "breathlessness"],
    );
    assert_eq!(
        changes(&got),
        [
            "progressing breathliness -> progressive breathlessness (exertional breathlessness, breathlessness)"
        ]
    );
}

/// A chunk over the word limit gives no candidates rather than an
/// unbounded comparison.
#[test]
fn a_chunk_over_the_word_limit_is_refused() {
    let long = vec!["word"; MAX_WORDS + 1];
    assert_eq!(
        propose(&long, &["word"], &HotwordList::default()),
        Err(TooManyWords)
    );
    assert_eq!(
        propose(&["word"], &long, &HotwordList::default()),
        Err(TooManyWords)
    );
    assert!(propose(&long[..MAX_WORDS], &["word"], &HotwordList::default()).is_ok());
}

/// The differences are difflib's opcodes without the equal runs: a
/// replacement, a deletion and an insertion, each where Python puts it.
#[test]
fn the_differences_are_difflibs() {
    let a = ["a", "b", "c", "d", "e"];
    let b = ["a", "x", "c", "e", "f"];
    // Python: [('equal',0,1,0,1), ('replace',1,2,1,2), ('equal',2,3,2,3),
    // ('delete',3,4,3,3), ('equal',4,5,3,4), ('insert',5,5,4,5)].
    assert_eq!(
        differences(&a, &b),
        [(1..2, 1..2), (3..4, 3..3), (5..5, 4..5)]
    );
}

/// difflib finds the leftmost longest block first, so a repeated word
/// lines up where Python lines it up.
#[test]
fn a_repeated_word_lines_up_as_in_python() {
    let a = ["x", "y", "x"];
    let b = ["x"];
    // Python: [('equal',0,1,0,1), ('delete',1,3,1,1)].
    assert_eq!(differences(&a, &b), [(1..3, 1..1)]);
}
