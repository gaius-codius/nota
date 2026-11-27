use nota_checks::{English, Lookup, Term};

use super::*;

/// The time `window` is at `start` in `text`, as (nth, of).
fn time(text: &str, window: &str, start: usize) -> Option<(u32, u32)> {
    occurrence(text, window, start).map(|o| (o.nth(), o.of()))
}

/// Words that occur once name no time.
#[test]
fn words_that_occur_once_name_no_time() {
    assert_eq!(time("start amylo diping daily", "amylo diping", 6), None);
}

/// Each time the words occur is named by its place, from 1, out of how
/// many times they occur.
#[test]
fn each_time_is_named_by_its_place() {
    let text = "amylo diping then amylo diping then amylo diping";
    assert_eq!(time(text, "amylo diping", 0), Some((1, 3)));
    assert_eq!(time(text, "amylo diping", 18), Some((2, 3)));
    assert_eq!(time(text, "amylo diping", 36), Some((3, 3)));
}

/// The words are counted as the lookup splits them: punctuation between
/// them, or case, doesn't make them other words, and a longer word that
/// holds them isn't them.
#[test]
fn words_are_counted_as_the_lookup_splits_them() {
    let text = "Amylo, diping first; amylo diping next; amylo dipings last";
    assert_eq!(time(text, "amylo diping", 21), Some((2, 2)));
    assert_eq!(time(text, "Amylo diping", 0), Some((1, 2)));
}

/// Quotes the split keeps on a word don't make it another word: the
/// quoted time counts, so the decider's count and the question's agree.
#[test]
fn quotes_on_the_words_dont_make_them_other_words() {
    let text = "Start amylo diping, quote 'amylo diping', then stop amylo diping.";
    assert_eq!(time(text, "amylo diping", 6), Some((1, 3)));
    assert_eq!(time(text, "'amylo diping'", 26), Some((2, 3)));
    assert_eq!(time(text, "amylo diping", 52), Some((3, 3)));
}

/// A place the words aren't counted at names no time, rather than a
/// wrong one.
#[test]
fn a_place_not_counted_names_no_time() {
    let text = "amylo diping then amylo diping";
    assert_eq!(time(text, "amylo diping", 3), None);
}

/// A blank window names no time, rather than panicking.
#[test]
fn a_blank_window_names_no_time() {
    assert_eq!(time("a a", "", 0), None);
    assert_eq!(time("a a", " ", 0), None);
}

/// Every place the real lookup offers a candidate is one this module
/// counts the candidate's words at, so the two ways of splitting words
/// can't drift apart unnoticed. The chunk repeats its words with
/// punctuation, a hyphen and an apostrophe between and around them.
#[test]
fn every_place_the_lookup_offers_is_counted() {
    let english = English::new().unwrap();
    let alias = |term: &str, heard: &str| Term {
        term: term.to_owned(),
        aliases: vec![heard.to_owned()],
        language: "en".to_owned(),
    };
    let terms = [
        alias("amlodipine", "amylo diping"),
        alias("GLP-1", "glp-one"),
        alias("Crohn's disease", "crone's disease"),
    ];
    let lookup = Lookup::new(&english, &terms);
    let text = "Amylo, diping for crone's disease; then amylo diping, \
                (glp-one) and crone's disease. Glp-one again.";
    let found = lookup.find(text, &|_, _| true);
    let counted: Vec<(String, Option<(u32, u32)>)> = found
        .iter()
        .map(|c| {
            (
                c.window().to_owned(),
                time(text, c.window(), c.span().start),
            )
        })
        .collect();
    // In the lookup's order: exact matches, fewer words first, then by
    // place.
    assert_eq!(
        counted,
        [
            ("glp-one".to_owned(), Some((1, 2))),
            ("Glp-one".to_owned(), Some((2, 2))),
            ("Amylo diping".to_owned(), Some((1, 2))),
            ("crone's disease".to_owned(), Some((1, 2))),
            ("amylo diping".to_owned(), Some((2, 2))),
            ("crone's disease".to_owned(), Some((2, 2))),
        ]
    );
    for c in &found {
        assert!(
            starts(text, c.window()).contains(&c.span().start),
            "{c:?} isn't counted at its own place"
        );
    }
}
