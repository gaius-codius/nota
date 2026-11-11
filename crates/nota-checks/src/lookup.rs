//! C4, the term lookup: which known terms a chunk's heard words might be
//! mishearings of. It's one of the two sources of candidates (the hotword
//! pass is the other); a model then says yes or no to each, and C2 lets a
//! fix through only for a candidate offered here.
//!
//! It's the bench's `lookup.py` `find()`, ported so its decisions hold
//! (the code checks spec, C4):
//! - **Windows:** the chunk's words (runs of ASCII letters, digits,
//!   apostrophes and hyphens), 1 to 3 at a time. A window with fewer than
//!   5 letters and digits is never a candidate.
//! - **Skipped:** one everyday word (Zipf ≥ 3.3); several common short
//!   words (each Zipf ≥ 5.0 and none longer than 4 characters); a window
//!   that starts on a function word (Zipf ≥ 6.3) other than "a", or ends on
//!   one; and a window with a word that is a term, or that is a term.
//! - **Scored** against each term within reach of its length:
//!   0.5 × how alike they sound + 0.25 × letter-trigram Jaccard + 0.25 ×
//!   Jaro-Winkler on their letters and digits.
//! - **Kept:** each window's best term, at 0.58 or more, or 0.42 when the
//!   window has a word that's nearly unknown (Zipf < 1.5); then the best
//!   first, never two windows sharing a word, up to 6.
//!
//! New here, not benched:
//! - **Exact matches first.** A window whose letters and digits are those of
//!   a term's alias (a spoken form, or a span the user accepted the term
//!   for) is offered that term ahead of any scored window. Only the 5-letter
//!   floor and the known-term rule apply to it; it may be up to
//!   [`MAX_EXACT_WORDS`] words.
//! - **The term list's order doesn't matter.** On a tie the bench kept the
//!   term listed first; here the term that sorts first wins.
//! - **Rejected pairs.** The caller says which window and term pairs the
//!   user rejected; a rejected term is passed over for the next best.
//!
//! The same text and terms always give the same candidates.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::language::Language;
use crate::phonetic::{self, Sounds};

/// The fewest letters and digits a window needs to be a candidate.
pub const MIN_LETTERS: usize = 5; // check-bound

/// The most words in a scored window.
pub const MAX_WORDS: usize = 3; // check-bound

/// The most words in a window matched exactly against an alias, and in the
/// alias itself (counting runs of letters and digits, so "gee-el-pee-one"
/// is four). An alias of more words is never matched.
pub const MAX_EXACT_WORDS: usize = 8; // check-bound

/// A single word at least this common (Zipf) is an everyday word, and not
/// a candidate.
pub const EVERYDAY_ZIPF: f64 = 3.3; // check-bound

/// A window of several words each at least this common (Zipf), none longer
/// than [`SHORT_WORD_LEN`] characters, is skipped.
pub const COMMON_ZIPF: f64 = 5.0; // check-bound

/// The longest a word may be and still count as short, in characters.
pub const SHORT_WORD_LEN: usize = 4; // check-bound

/// A word at least this common (Zipf) is a function word: a window of
/// several words may not start (but for [`Language::may_open_on`]) or end
/// on one.
pub const FUNCTION_ZIPF: f64 = 6.3; // check-bound

/// A word less common than this (Zipf) is nearly unknown, likely a garbled
/// term, and lowers the bar to [`KEEP_RARE`].
pub const RARE_ZIPF: f64 = 1.5; // check-bound

/// The score a window's best term needs to be kept.
pub const KEEP: f64 = 0.58; // check-bound

/// The score needed when the window has a nearly unknown word.
pub const KEEP_RARE: f64 = 0.42; // check-bound

/// The most candidates offered for one chunk.
pub const MAX_CANDIDATES: usize = 6; // check-bound

/// A term is scored only if its letters and digits number within this many
/// of the window's, or within half its own count if that's more.
pub const LENGTH_SLACK: usize = 4; // check-bound

/// The weight of how alike the window and term sound.
const SOUNDS_WEIGHT: f64 = 0.5; // check-bound

/// The weight of letter-trigram Jaccard.
const TRIGRAM_WEIGHT: f64 = 0.25; // check-bound

/// The weight of Jaro-Winkler.
const JARO_WINKLER_WEIGHT: f64 = 0.25; // check-bound

/// A term the lookup may offer, as a pack or the user's own list gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// The term as it's written (`amlodipine`, `GLP-1`).
    pub term: String,
    /// Forms that are matched exactly: spoken forms (`gee el pee one`), and
    /// spans the user accepted this term for.
    pub aliases: Vec<String>,
    /// The language of the term (`en`).
    pub language: String,
}

/// How a candidate was found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Matched {
    /// The window is one of the term's aliases, letter for letter.
    Alias,
    /// The window sounds and looks like the term.
    Scored(Score),
}

/// How alike a window and a term are, and the bar it cleared.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    /// The weighted score, from 0 to 1.
    pub total: f64,
    /// How alike they sound, from 0 to 1.
    pub sounds: f64,
    /// Letter-trigram Jaccard, from 0 to 1.
    pub trigrams: f64,
    /// Jaro-Winkler on their letters and digits, from 0 to 1.
    pub jaro_winkler: f64,
    /// Which bar it cleared.
    pub bar: Bar,
}

/// The bar a scored candidate cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    /// [`KEEP`].
    Usual,
    /// [`KEEP_RARE`]: the window has a nearly unknown word.
    Rare,
}

/// A term offered for a window of a chunk. Only [`Lookup::find`] makes one.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The window's words, joined by one space.
    window: String,
    /// Where the window is in the chunk: bytes, from its first word's start
    /// to its last word's end.
    span: Range<usize>,
    /// The term offered.
    term: String,
    /// How it was found.
    matched: Matched,
}

impl Candidate {
    /// The window's words, joined by one space.
    #[must_use]
    pub fn window(&self) -> &str {
        &self.window
    }

    /// Where the window is in the chunk, in bytes.
    #[must_use]
    pub fn span(&self) -> Range<usize> {
        self.span.clone()
    }

    /// The term offered.
    #[must_use]
    pub fn term(&self) -> &str {
        &self.term
    }

    /// How it was found.
    #[must_use]
    pub fn matched(&self) -> Matched {
        self.matched
    }
}

/// The term lookup for one language and term list.
#[derive(Debug)]
pub struct Lookup<'a> {
    /// The language's rules.
    language: &'a dyn Language,
    /// The terms in the language, one per spelling, sorted by it.
    entries: Vec<Entry>,
    /// Every term's letters and digits: a window with a word among them is
    /// skipped.
    known: BTreeSet<String>,
    /// Each alias's letters and digits, and the entries it's an alias of,
    /// in order.
    aliases: BTreeMap<String, Vec<usize>>,
}

/// A term, made ready to score.
#[derive(Debug)]
struct Entry {
    /// The term as it's written.
    term: String,
    /// Its letters and digits, lower-cased.
    letters: String,
    /// How it sounds.
    sounds: Sounds,
    /// Its letter trigrams.
    trigrams: Trigrams,
}

/// A window, made ready to score.
#[derive(Debug)]
struct Window<'t> {
    /// Its words.
    words: &'t [Word<'t>],
    /// Its words joined by one space.
    text: String,
    /// Its letters and digits, lower-cased.
    letters: String,
}

/// A word of the chunk.
#[derive(Debug)]
struct Word<'t> {
    /// The word as written.
    text: &'t str,
    /// Where it is in the chunk, in bytes.
    span: Range<usize>,
    /// How common it is.
    zipf: f64,
}

/// A window that may become a candidate.
#[derive(Debug)]
struct Hit {
    /// The window's first word.
    start: usize,
    /// One past its last word.
    end: usize,
    /// The candidate it would be.
    candidate: Candidate,
}

impl<'a> Lookup<'a> {
    /// The lookup over `terms` in `language`. Terms in another language are
    /// left out; a term listed twice is one term with both lists of aliases.
    /// A term with no letter or digit, and an alias that's spelled like a
    /// term, are never matched.
    #[must_use]
    pub fn new(language: &'a dyn Language, terms: &[Term]) -> Self {
        let mut merged: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for t in terms
            .iter()
            .filter(|t| same_language(&t.language, language.code()))
        {
            merged
                .entry(t.term.as_str())
                .or_default()
                .extend(t.aliases.iter().map(String::as_str));
        }
        // In the terms' order, so ties go to the term that sorts first.
        let (entries, term_aliases): (Vec<Entry>, Vec<BTreeSet<&str>>) = merged
            .into_iter()
            .map(|(term, aliases)| (Entry::new(language, term), aliases))
            .filter(|(e, _)| !e.letters.is_empty())
            .unzip();
        let known: BTreeSet<String> = entries.iter().map(|e| e.letters.clone()).collect();
        let aliases = index_aliases(&term_aliases);
        Self {
            language,
            entries,
            known,
            aliases,
        }
    }

    /// The candidates in `text`: alias matches first, in the order their
    /// windows come (fewer words first, then position), then scored
    /// candidates best first. `allows(window, term)` says
    /// whether `term` may be offered for `window`; a term it refuses is
    /// passed over (the user's rejected pairs).
    #[must_use]
    pub fn find(&self, text: &str, allows: &dyn Fn(&str, &str) -> bool) -> Vec<Candidate> {
        let words: Vec<Word<'_>> = window_words(text)
            .into_iter()
            .map(|(text, span)| Word {
                text,
                span,
                zipf: self.language.zipf(text).value(),
            })
            .collect();
        let mut exact = Vec::new();
        let mut scored = Vec::new();
        // An alias may be heard as more words than it's written in ("endside"
        // as "end side"), so with any alias every window size is tried.
        let most = if self.aliases.is_empty() {
            MAX_WORDS
        } else {
            MAX_EXACT_WORDS
        };
        for n in 1..=most {
            for (start, run) in words.windows(n).enumerate() {
                let window = Window::new(run);
                if window.letters.len() < MIN_LETTERS || self.has_known_word(&window) {
                    continue;
                }
                if let Some(hit) = self.exact(&window, start, allows) {
                    exact.push(hit);
                } else if n <= MAX_WORDS
                    && !self.skipped(&window)
                    && let Some(hit) = self.scored(&window, start, allows)
                {
                    scored.push(hit);
                }
            }
        }
        // Stable: windows that tie keep their order, fewer words first, then
        // earlier in the chunk, as in the bench.
        scored.sort_by(|(a, _), (b, _): &(f64, Hit)| b.total_cmp(a));
        pick(
            exact
                .into_iter()
                .chain(scored.into_iter().map(|(_, hit)| hit)),
        )
    }

    /// Whether a word of `window` is a term, or the whole window is: the
    /// heard text already has it right.
    fn has_known_word(&self, window: &Window<'_>) -> bool {
        self.known.contains(&window.letters)
            || window
                .words
                .iter()
                .any(|w| self.known.contains(&letters(w.text)))
    }

    /// Whether the bench's rules skip `window` before scoring.
    fn skipped(&self, window: &Window<'_>) -> bool {
        let words = window.words;
        let [first, .., last] = words else {
            // One word: skipped if it's an everyday word.
            return words.iter().any(|w| w.zipf >= EVERYDAY_ZIPF);
        };
        let common_short = words
            .iter()
            .all(|w| w.zipf >= COMMON_ZIPF && w.text.len() <= SHORT_WORD_LEN);
        let opens_on_function_word =
            first.zipf >= FUNCTION_ZIPF && !self.language.may_open_on(first.text);
        common_short || opens_on_function_word || last.zipf >= FUNCTION_ZIPF
    }

    /// The exact match for `window`, which starts at word `start`: the
    /// first term, in order, that has the window as an alias and that
    /// `allows` lets through.
    fn exact(
        &self,
        window: &Window<'_>,
        start: usize,
        allows: &dyn Fn(&str, &str) -> bool,
    ) -> Option<Hit> {
        let entry = self
            .aliases
            .get(&window.letters)?
            .iter()
            .map(|&i| &self.entries[i])
            .find(|e| allows(&window.text, &e.term))?;
        Some(window.hit(start, entry, Matched::Alias))
    }

    /// The best term for `window`, which starts at word `start`, with its
    /// score, if it clears its bar.
    fn scored(
        &self,
        window: &Window<'_>,
        start: usize,
        allows: &dyn Fn(&str, &str) -> bool,
    ) -> Option<(f64, Hit)> {
        let sounds = self.language.sounds(&window.text);
        let trigrams = Trigrams::of(&window.letters);
        let mut best: Option<(&Entry, Score)> = None;
        // A window spelled like a term never gets here: it has a known word.
        for entry in &self.entries {
            if !within_reach(&window.letters, &entry.letters) || !allows(&window.text, &entry.term)
            {
                continue;
            }
            let score = score(&sounds, &trigrams, &window.letters, entry);
            // Strictly better: on a tie the term that sorts first, met
            // first, stays.
            if best.is_none_or(|(_, b)| score.total > b.total) {
                best = Some((entry, score));
            }
        }
        let (entry, mut score) = best?;
        let rare = window.words.iter().any(|w| w.zipf < RARE_ZIPF);
        score.bar = if rare { Bar::Rare } else { Bar::Usual };
        let bar = if rare { KEEP_RARE } else { KEEP };
        (score.total >= bar).then(|| {
            (
                score.total,
                window.hit(start, entry, Matched::Scored(score)),
            )
        })
    }
}

impl Entry {
    /// `term`, made ready to score in `language`.
    fn new(language: &dyn Language, term: &str) -> Self {
        let letters = letters(term);
        Self {
            term: term.to_owned(),
            sounds: language.sounds(term),
            trigrams: Trigrams::of(&letters),
            letters,
        }
    }
}

impl<'t> Window<'t> {
    /// The window of `words`.
    fn new(words: &'t [Word<'t>]) -> Self {
        let text = words.iter().map(|w| w.text).collect::<Vec<_>>().join(" ");
        Self {
            letters: letters(&text),
            text,
            words,
        }
    }

    /// The hit offering `entry` for this window, which starts at word
    /// `start`.
    fn hit(&self, start: usize, entry: &Entry, matched: Matched) -> Hit {
        let span = match (self.words.first(), self.words.last()) {
            (Some(first), Some(last)) => first.span.start..last.span.end,
            _ => 0..0,
        };
        Hit {
            start,
            end: start + self.words.len(),
            candidate: Candidate {
                window: self.text.clone(),
                span,
                term: entry.term.clone(),
                matched,
            },
        }
    }
}

/// Each alias's letters and digits, and the indexes in `term_aliases` (one
/// list per entry) of the entries it's an alias of, in order. An alias of
/// more than [`MAX_EXACT_WORDS`] words is left out. (An alias spelled like a
/// term needs no check here: a window spelled like a term is skipped as
/// known. Nor does one under 5 letters: no window that short is offered.)
fn index_aliases(term_aliases: &[BTreeSet<&str>]) -> BTreeMap<String, Vec<usize>> {
    let mut aliases: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, list) in term_aliases.iter().enumerate() {
        for alias in list {
            let key = letters(alias);
            let words = alias
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .count();
            if words > MAX_EXACT_WORDS {
                continue;
            }
            aliases.entry(key).or_default().push(index);
        }
    }
    aliases
}

/// The candidates from `hits`, in their order, leaving out any that shares
/// a word with one already taken, up to [`MAX_CANDIDATES`].
fn pick(hits: impl Iterator<Item = Hit>) -> Vec<Candidate> {
    let mut taken: Vec<Range<usize>> = Vec::new();
    let mut out = Vec::new();
    for hit in hits {
        if out.len() == MAX_CANDIDATES {
            break;
        }
        if taken.iter().any(|t| hit.start < t.end && t.start < hit.end) {
            continue;
        }
        taken.push(hit.start..hit.end);
        out.push(hit.candidate);
    }
    out
}

/// The score of a window against `entry`, from the window's `sounds`,
/// `trigrams` and `letters`. Its bar is set by the caller.
fn score(sounds: &Sounds, trigrams: &Trigrams, letters: &str, entry: &Entry) -> Score {
    let sounds = phonetic::similarity(sounds, &entry.sounds);
    let trigrams = trigrams.jaccard(&entry.trigrams);
    let jaro_winkler = strsim::jaro_winkler(letters, &entry.letters);
    Score {
        // In the bench's order, so the float is the same to the last bit.
        total: SOUNDS_WEIGHT * sounds
            + TRIGRAM_WEIGHT * trigrams
            + JARO_WINKLER_WEIGHT * jaro_winkler,
        sounds,
        trigrams,
        jaro_winkler,
        bar: Bar::Usual,
    }
}

/// Whether a window with `window` letters and digits is close enough in
/// length to a term with `term` to be scored against it.
fn within_reach(window: &str, term: &str) -> bool {
    window.len().abs_diff(term.len()) <= LENGTH_SLACK.max(term.len() / 2)
}

/// Whether a term in `language` belongs to a lookup for `code`: the same
/// primary language, in any case (`en`, `EN`, `en-GB`).
fn same_language(language: &str, code: &str) -> bool {
    language
        .split(['-', '_'])
        .next()
        .is_some_and(|primary| primary.eq_ignore_ascii_case(code))
}

/// The words of `text` a window is made of, each with its byte range: runs
/// of ASCII letters, digits, apostrophes and hyphens (the bench's
/// `[A-Za-z0-9'\-]+`).
fn window_words(text: &str) -> Vec<(&str, Range<usize>)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        let in_word = c.is_ascii_alphanumeric() || c == '\'' || c == '-';
        match (start, in_word) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                out.push((&text[s..i], s..i));
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// The letters and digits of `text`, lower-cased: `a`–`z` and `0`–`9`, as
/// the bench's `letters()` and C1 keep them.
fn letters(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

/// A string's letter trigrams, with `^` before it and `$` after, sorted and
/// without repeats.
#[derive(Debug)]
struct Trigrams(Vec<[u8; 3]>);

impl Trigrams {
    /// The trigrams of `letters`, ASCII letters and digits.
    fn of(letters: &str) -> Self {
        let marked = format!("^{letters}$");
        let mut grams: Vec<[u8; 3]> = marked
            .as_bytes()
            .windows(3)
            .map(|w| [w[0], w[1], w[2]])
            .collect();
        grams.sort_unstable();
        grams.dedup();
        Self(grams)
    }

    /// The share of the two sets' union that both have; 0 if both are
    /// empty.
    #[expect(
        clippy::cast_precision_loss,
        reason = "counts of trigrams are far below 2^52, exact in an f64"
    )]
    fn jaccard(&self, other: &Self) -> f64 {
        // Both lists are sorted, so a binary search finds each shared one.
        let both = self
            .0
            .iter()
            .filter(|g| other.0.binary_search(g).is_ok())
            .count();
        let either = self.0.len() + other.0.len() - both;
        both as f64 / either.max(1) as f64
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod props;
