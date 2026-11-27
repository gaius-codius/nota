//! Property tests for the hotword diff.
//!
//! The contract, checked on every input:
//! - every candidate's term is on the list, and its word is one of that
//!   term's listed words, found among the boosted words;
//! - no candidate's heard words hold a listed word;
//! - every candidate's similarity is at least [`SIMILARITY_MIN`];
//! - a beam pass that only drops words (a dropped sentence, or several)
//!   gives no candidate;
//! - the same heard and boosted words give no candidate;
//! - the matching blocks are real matches, in order on both sides, and
//!   never adjacent, so difflib's joining of adjacent blocks has nothing to
//!   do.

use proptest::prelude::*;

use super::*;

/// A short word from a small alphabet, so the two sides often share words
/// and the listed words often turn up.
fn any_word() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-d]{1,6}",
        Just("amlodipine".to_owned()),
        Just("amylo".to_owned()),
        Just("Amlodipine.".to_owned()),
        Just("care".to_owned()),
        Just("Care.".to_owned()),
        Just("cared".to_owned()),
        Just("self-care".to_owned()),
    ]
}

/// A chunk's words.
fn any_words() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(any_word(), 0..24)
}

/// A hotword list from the same alphabet.
fn any_terms() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop_oneof![
            "[a-d]{1,6}( [a-d]{1,6})?",
            Just("amlodipine".to_owned()),
            Just("care".to_owned()),
            Just("self-care".to_owned())
        ],
        0..5,
    )
}

/// `words` as the `&str`s [`propose`] takes.
fn refs(words: &[String]) -> Vec<&str> {
    words.iter().map(String::as_str).collect()
}

/// The positions to keep of `len` words: all but those `drop` marks.
fn kept(words: &[String], drop: &[bool]) -> Vec<String> {
    words
        .iter()
        .zip(drop.iter().chain(std::iter::repeat(&false)))
        .filter(|(_, d)| !**d)
        .map(|(w, _)| w.clone())
        .collect()
}

proptest! {
    /// Every candidate keeps the rules: its term listed, its word among the
    /// boosted words, no listed word in what it replaces, C1 high enough.
    #[test]
    fn every_candidate_keeps_the_rules(heard in any_words(), boosted in any_words(), terms in any_terms()) {
        let list = HotwordList::new(terms.iter().map(String::as_str));
        for c in propose(&refs(&heard), &refs(&boosted), &list).unwrap() {
            let (_, listed) = list.terms.iter().find(|(t, _)| *t == c.term).unwrap();
            prop_assert!(listed.contains(&c.word), "{c:?}");
            let new: Vec<String> = c.replacement.split_whitespace().map(normalise).collect();
            prop_assert!(new.iter().flat_map(|w| parts(w)).any(|p| p == c.word), "{c:?}");
            let old: Vec<String> = heard[c.heard.clone()].iter().map(|w| normalise(w)).collect();
            prop_assert!(!list.holds_any(&old), "{c:?}");
            prop_assert_eq!(c.heard_text.clone(), heard[c.heard.clone()].join(" "));
            prop_assert!(c.similarity >= SIMILARITY_MIN, "{c:?}");
        }
    }

    /// A beam pass that only drops words from what was heard gives nothing.
    #[test]
    fn dropped_words_give_no_candidate(
        heard in any_words(),
        drop in prop::collection::vec(any::<bool>(), 0..24),
        terms in any_terms(),
    ) {
        let list = HotwordList::new(terms.iter().map(String::as_str));
        let boosted = kept(&heard, &drop);
        prop_assert_eq!(propose(&refs(&heard), &refs(&boosted), &list).unwrap(), Vec::new());
    }

    /// Each matching block is equal words on both sides, each starts after
    /// the last one ends on both sides, and none starts where the last one
    /// ends on both, which would make them one longer block.
    #[test]
    fn matching_blocks_are_ordered_real_and_never_adjacent(a in any_words(), b in any_words()) {
        let blocks = matching_blocks(&a, &b);
        prop_assert_eq!(blocks.last().copied(), Some(Block { a: a.len(), b: b.len(), len: 0 }));
        for block in &blocks {
            prop_assert_eq!(&a[block.a..block.a + block.len], &b[block.b..block.b + block.len]);
        }
        for pair in blocks.windows(2) {
            let (last, next) = (pair[0], pair[1]);
            prop_assert!(last.a + last.len <= next.a && last.b + last.len <= next.b, "{blocks:?}");
            prop_assert!(
                next.len == 0 || last.a + last.len != next.a || last.b + last.len != next.b,
                "{blocks:?}"
            );
        }
    }

    /// The same words on both sides differ nowhere.
    #[test]
    fn the_same_words_give_no_candidate(heard in any_words(), terms in any_terms()) {
        let list = HotwordList::new(terms.iter().map(String::as_str));
        prop_assert_eq!(propose(&refs(&heard), &refs(&heard), &list).unwrap(), Vec::new());
    }
}
