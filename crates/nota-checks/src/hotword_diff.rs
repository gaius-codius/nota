//! The hotword diff: candidates from the difference between what the final
//! pass heard and what a hotword-boosted beam pass heard in the same chunk.
//! It's the bench's `propose.py`, ported. The two transcripts are compared
//! a word at a time, as Python's `difflib.SequenceMatcher` with autojunk
//! off compares them, and a difference becomes a [`HotwordCandidate`] only
//! if all of these hold:
//! 1. its boosted words hold a word of a listed term (a **listed word**:
//!    a run of `a`–`z` and `0`–`9` in a term, lowercased, of more than
//!    [`MIN_LISTED_WORD`] characters);
//! 2. its heard words hold no listed word, so a term the final pass already
//!    heard isn't proposed again;
//! 3. C1 ([`letter_similarity`], the heard words first) is at least
//!    [`SIMILARITY_MIN`].
//!
//! A sentence the beam pass drops is a difference with no boosted words, so
//! rule 1 refuses it: the beam's known habit of losing whole sentences can't
//! become a proposal. Nor can a beam text that **only** lost words, however
//! the matching lines it up: if the boosted words are the heard ones with
//! some left out (a subsequence, once normalised), the chunk gives nothing,
//! though the longest-match alignment may set a lost word opposite a kept
//! one and read it as a replacement.
//!
//! Where it differs from `propose.py`, deliberately (the code checks spec,
//! "The hotword diff"):
//! - **Rule 3 is C1.** `propose.py` compared letters only, dropping digits;
//!   C1 keeps them, as every other check does.
//! - **Each candidate names its term:** the first listed term, in list
//!   order, one of whose listed words is among the boosted words, and that
//!   word.
//! - **A chunk with more than [`MAX_WORDS`] words on either side gives no
//!   candidates** ([`TooManyWords`]), which bounds the work; a final-pass
//!   chunk is at most 25 s of speech, far shorter.
//! - **A beam text that only lost words gives nothing,** as above.
//!   `propose.py` compared the alignment alone; no bench chunk is affected.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::ops::Range;

use crate::gate::SIMILARITY_MIN;
use crate::similarity::{Block, Region, letter_similarity, longest_match};

/// A listed word is longer than this many characters: `propose.py`'s
/// `len(w) > 3`, so "care" counts and "copd" counts, but "gp" doesn't.
pub const MIN_LISTED_WORD: usize = 3;

/// The most words the diff compares on either side of a chunk. Matching
/// grows with the product of the two lengths; a final-pass chunk (25 s at
/// most) holds a few hundred words at the very most.
pub const MAX_WORDS: usize = 2_000;

/// The session's hotwords, as the diff reads them: each term with its
/// listed words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotwordList {
    /// Each term, in list order, with its listed words.
    terms: Vec<(String, BTreeSet<String>)>,
    /// Every listed word of every term.
    words: BTreeSet<String>,
}

impl HotwordList {
    /// The list of `terms`, in their order. A term with no listed word
    /// (all its words are short) is kept, but can't introduce anything.
    #[must_use]
    pub fn new<'a>(terms: impl IntoIterator<Item = &'a str>) -> Self {
        let terms: Vec<(String, BTreeSet<String>)> = terms
            .into_iter()
            .map(|term| (term.to_owned(), listed_words(term)))
            .collect();
        let words = terms.iter().flat_map(|(_, w)| w.iter().cloned()).collect();
        Self { terms, words }
    }

    /// Whether any of `words` holds a listed word.
    fn holds_any(&self, words: &[String]) -> bool {
        words
            .iter()
            .flat_map(|word| parts(word))
            .any(|part| self.words.contains(part))
    }

    /// The first term, in list order, with a listed word among `words`,
    /// and the first such word in the order `words` hold them.
    fn introduced<'s>(&'s self, words: &'s [String]) -> Option<(&'s str, &'s str)> {
        let found: Vec<&str> = words.iter().flat_map(|word| parts(word)).collect();
        self.terms.iter().find_map(|(term, listed)| {
            let word = found.iter().find(|part| listed.contains(**part))?;
            Some((term.as_str(), *word))
        })
    }
}

/// A difference the diff keeps: replace the heard words with the boosted
/// ones, which introduce a listed term.
#[derive(Debug, Clone, PartialEq)]
pub struct HotwordCandidate {
    /// Which of the chunk's heard words it replaces, by position.
    pub heard: Range<usize>,
    /// Those words, joined by single spaces, as heard.
    pub heard_text: String,
    /// The boosted words to put there, joined by single spaces, as the beam
    /// pass gave them (its case and punctuation).
    pub replacement: String,
    /// The listed term the replacement introduces.
    pub term: String,
    /// The listed word of [`HotwordCandidate::term`] that the replacement
    /// holds, lowercased: what the gate finds inside the replacement, since
    /// a term of several words ("varicose veins") may come in by one.
    pub word: String,
    /// C1 of the heard words against the replacement.
    pub similarity: f64,
}

/// A chunk had more than [`MAX_WORDS`] words on one side, so it wasn't
/// compared and gives no candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyWords;

impl fmt::Display for TooManyWords {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "too many words to compare: over {MAX_WORDS}")
    }
}

impl Error for TooManyWords {}

/// The candidates in one chunk: where the `boosted` words differ from the
/// `heard` ones, by the rules in the module docs, in chunk order.
///
/// # Errors
///
/// [`TooManyWords`] if either side has more than [`MAX_WORDS`] words.
pub fn propose(
    heard: &[&str],
    boosted: &[&str],
    list: &HotwordList,
) -> Result<Vec<HotwordCandidate>, TooManyWords> {
    if heard.len() > MAX_WORDS || boosted.len() > MAX_WORDS {
        return Err(TooManyWords);
    }
    let a: Vec<String> = heard.iter().map(|w| normalise(w)).collect();
    let b: Vec<String> = boosted.iter().map(|w| normalise(w)).collect();
    if is_subsequence(&b, &a) {
        return Ok(Vec::new());
    }
    Ok(differences(&a, &b)
        .into_iter()
        .filter_map(|(old, new)| {
            candidate(
                heard,
                boosted,
                &a[old.clone()],
                &b[new.clone()],
                old,
                new,
                list,
            )
        })
        .collect())
}

/// The candidate for the difference that replaces `heard[old]` with
/// `boosted[new]` (`a` and `b` the same words normalised), if the rules
/// keep it.
fn candidate(
    heard: &[&str],
    boosted: &[&str],
    a: &[String],
    b: &[String],
    old: Range<usize>,
    new: Range<usize>,
    list: &HotwordList,
) -> Option<HotwordCandidate> {
    // Rules 1 and 2: a listed word comes in, and none was there before.
    let (term, word) = list.introduced(b)?;
    if list.holds_any(a) {
        return None;
    }
    let heard_text = heard[old.clone()].join(" ");
    let replacement = boosted[new].join(" ");
    // Rule 3. An insertion has no heard words, which C1 scores 0 against
    // any replacement with letters; a side C1 refuses gives nothing.
    let similarity = letter_similarity(&heard_text, &replacement).ok()?.value();
    (similarity >= SIMILARITY_MIN).then(|| HotwordCandidate {
        heard: old,
        heard_text,
        replacement,
        term: term.to_owned(),
        word: word.to_owned(),
        similarity,
    })
}

/// Whether `short` is `long` with some (or no) items left out, in order.
fn is_subsequence<T: Eq>(short: &[T], long: &[T]) -> bool {
    let mut rest = long.iter();
    short.iter().all(|item| rest.any(|other| other == item))
}

/// `propose.py`'s word normalisation: lowercased, keeping only `a`–`z`,
/// `0`–`9`, `'` and `-`.
fn normalise(word: &str) -> String {
    word.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '\'' | '-'))
        .collect()
}

/// The runs of `a`–`z` and `0`–`9` in a normalised `word`.
fn parts(word: &str) -> impl Iterator<Item = &str> {
    word.split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|part| !part.is_empty())
}

/// The listed words of `term`: its runs of `a`–`z` and `0`–`9`, lowercased,
/// longer than [`MIN_LISTED_WORD`].
fn listed_words(term: &str) -> BTreeSet<String> {
    parts(&normalise_term(term))
        .filter(|part| part.len() > MIN_LISTED_WORD)
        .map(str::to_owned)
        .collect()
}

/// `term` lowercased, as `propose.py` read its glossary (`t.lower()`),
/// with every character outside `a`–`z` and `0`–`9` a separator.
fn normalise_term(term: &str) -> String {
    term.chars().flat_map(char::to_lowercase).collect()
}

/// difflib's `get_opcodes` without the equal runs: each difference between
/// `a` and `b`, as the range of `a` it replaces and the range of `b` it
/// becomes. A deletion has an empty range of `b`, an insertion an empty
/// range of `a`.
fn differences<T: Eq>(a: &[T], b: &[T]) -> Vec<(Range<usize>, Range<usize>)> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    for block in matching_blocks(a, b) {
        if i < block.a || j < block.b {
            out.push((i..block.a, j..block.b));
        }
        i = block.a + block.len;
        j = block.b + block.len;
    }
    out
}

/// difflib's `get_matching_blocks`: the blocks its recursive longest-match
/// finds, in order, adjacent ones joined, ending with an empty block at the
/// ends of both sequences.
fn matching_blocks<T: Eq>(a: &[T], b: &[T]) -> Vec<Block> {
    let mut found = Vec::new();
    let whole = Region {
        a: 0..a.len(),
        b: 0..b.len(),
    };
    collect_blocks(a, b, &whole, &mut found);
    found.sort_by_key(|block| (block.a, block.b));
    let mut joined: Vec<Block> = Vec::new();
    for block in found {
        match joined.last_mut() {
            Some(last) if last.a + last.len == block.a && last.b + last.len == block.b => {
                last.len += block.len;
            }
            _ => joined.push(block),
        }
    }
    joined.push(Block {
        a: a.len(),
        b: b.len(),
        len: 0,
    });
    joined
}

/// Adds to `found` the longest block in `region` and, as difflib's queue
/// does, the blocks either side of it. The depth is at most the number of
/// blocks, which [`MAX_WORDS`] bounds.
fn collect_blocks<T: Eq>(a: &[T], b: &[T], region: &Region, found: &mut Vec<Block>) {
    let best = longest_match(a, b, region);
    if best.len == 0 {
        return;
    }
    found.push(best);
    if region.a.start < best.a && region.b.start < best.b {
        let before = Region {
            a: region.a.start..best.a,
            b: region.b.start..best.b,
        };
        collect_blocks(a, b, &before, found);
    }
    if best.a + best.len < region.a.end && best.b + best.len < region.b.end {
        let after = Region {
            a: best.a + best.len..region.a.end,
            b: best.b + best.len..region.b.end,
        };
        collect_blocks(a, b, &after, found);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod props;
