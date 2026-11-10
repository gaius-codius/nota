//! C2, the candidate gate: what makes a model's yes safe. A model proposes
//! a replacement for a heard span, or says yes to a candidate; whatever it
//! says, the gate applies nothing that code didn't offer for that span.
//!
//! It's the bench's gate U (`regate.py`), ported. A proposal is applied only
//! if all of these hold, checked in this order:
//! 1. the heard span is found in the chunk, on word boundaries;
//! 2. C1 ([`letter_similarity`], heard span first) is at least
//!    [`SIMILARITY_MIN`];
//! 3. an **offered** candidate's term is inside the replacement (in C1's
//!    letters, so case, spaces and punctuation don't count);
//! 4. that candidate's window shares a word with the heard span.
//!
//! **Only the gate makes an [`AppliedFix`]:** its fields are private and its
//! one constructor is in this module, so holding one proves the four rules
//! held. The gate reads the chunk through a shared borrow and returns a new
//! value, the revised text, so nothing it does can change the text it was
//! given.
//!
//! Where it differs from the bench, deliberately (the code checks spec, C2):
//! - **A heard span with no letters or digits is refused** ([`Rejection::EmptySpan`]).
//!   C1 scores two such strings 1, so it would pass rule 2 on its own.
//! - **A term with no letters or digits counts for nothing.** In Python an
//!   empty string is inside every replacement.
//! - **The score is compared at full precision.** The bench compared the
//!   logged score, rounded to two places, so 0.595 passed there; no bench
//!   fix falls in [0.595, 0.6), so the decisions agree.
//! - **Each fix is placed in the chunk as given,** at the first match that
//!   doesn't overlap a fix already placed; a proposal whose every match
//!   overlaps one is [`Rejection::AlreadyChanged`]. The bench replaced
//!   inside its running result, so a later span could match inside an
//!   earlier replacement. Every bench chunk revises to the same text.
//! - **A side C1 refuses is refused** ([`Rejection::TooLong`]): more than
//!   [`crate::MAX_LETTERS`] letters and digits. No bench span comes near.
//!
//! What it shares with the bench, which callers should know:
//! - A word boundary is the edge of the chunk or a character other than an
//!   ASCII letter, digit or `'`; so "up" is found in "follow-up".
//! - The span is matched ignoring case, a character at a time: two
//!   characters match when they're equal or have the same simple lowercase
//!   (`İ` matches `i`). Python also folds a few pairs that don't lowercase
//!   alike (`ſ` and `s`); those don't match here.
//! - A term is found inside the replacement anywhere, even inside a longer
//!   word ("NSAID" in "NSAIDs").
//! - Words, for rule 4, are runs of `a`–`z` and `0`–`9` after lowercasing.
//! - A replacement may hold words beyond the term ("amylo edema" →
//!   "ankle oedema" through the term "oedema"), and may equal the span.

use std::collections::BTreeSet;
use std::ops::Range;

use crate::similarity::{Similarity, TooLong, letter_similarity, letters};

/// The least C1 score a proposal needs: the bench's filter, used by every
/// bench script, and not yet measured beyond the bench (the code checks
/// spec, "Measuring the thresholds").
pub const SIMILARITY_MIN: f64 = 0.6; // check-bound

/// A candidate code offered for a chunk: a window of the heard text, and a
/// term it may have been meant as (C4's lookup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    /// The words of the chunk the term was offered for.
    pub window: String,
    /// The term.
    pub term: String,
}

/// What a model proposed: a span of the chunk as it heard it, and the text
/// to put there. A yes to a candidate is the proposal of its window as the
/// span and its term as the replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// The span of the chunk to replace.
    pub heard: String,
    /// The text to put in its place.
    pub replacement: String,
}

/// Why the gate refused a proposal, in the order the rules are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The heard span has no letters or digits, so it can't be compared.
    EmptySpan,
    /// The heard span isn't in the chunk on word boundaries (rule 1).
    SpanNotFound,
    /// The span or the replacement is longer than C1 compares.
    TooLong,
    /// C1 is below [`SIMILARITY_MIN`] (rule 2).
    Dissimilar,
    /// No offered term is inside the replacement (rule 3).
    NotOffered,
    /// An offered term is inside the replacement, but no such candidate's
    /// window shares a word with the heard span (rule 4).
    WindowElsewhere,
    /// Every rule held, but each match of the span overlaps a fix placed
    /// before it, so there was nothing left to change.
    AlreadyChanged,
}

/// A fix the gate applied: the chunk's text at `range` gives way to the
/// replacement, through an offered candidate.
///
/// Only [`gate()`] makes one: the fields are private and the one constructor
/// is the gate's own, so a value of this type shows the four rules held.
/// Reached through the gate, it compiles:
///
/// ```
/// use nota_checks::{AppliedFix, Offered, Proposal, gate, letter_similarity};
///
/// let offered = [Offered { window: "amylo diping".to_owned(), term: "amlodipine".to_owned() }];
/// let proposal = Proposal { heard: "amylo diping".to_owned(), replacement: "amlodipine".to_owned() };
/// let gated = gate("start amylo diping daily", &offered, &[proposal]);
/// let fix: &AppliedFix = gated.applied().next().unwrap();
/// assert_eq!(fix.range(), 6..18);
/// # let _ = letter_similarity;
/// ```
///
/// Built any other way, it doesn't, whether by its fields:
///
/// ```compile_fail,E0451
/// use nota_checks::{AppliedFix, Offered, Proposal, gate, letter_similarity};
///
/// let offered = Offered { window: "amylo diping".to_owned(), term: "amlodipine".to_owned() };
/// let fix = AppliedFix {
///     range: 6..18,
///     heard: "amylo diping".to_owned(),
///     replacement: "amlodipine".to_owned(),
///     offered,
///     similarity: letter_similarity("amylo diping", "amlodipine").unwrap(),
/// };
/// # let _ = (gate, Proposal { heard: String::new(), replacement: String::new() });
/// ```
///
/// or by its constructor:
///
/// ```compile_fail,E0624
/// use nota_checks::{AppliedFix, Offered, Proposal, gate, letter_similarity};
///
/// let offered = Offered { window: "amylo diping".to_owned(), term: "amlodipine".to_owned() };
/// let similarity = letter_similarity("amylo diping", "amlodipine").unwrap();
/// let fix = AppliedFix::new(6..18, "amylo diping", "amlodipine", &offered, similarity);
/// # let _ = (gate, Proposal { heard: String::new(), replacement: String::new() });
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedFix {
    /// Where the replaced text is in the chunk, in bytes.
    range: Range<usize>,
    /// The chunk's text at `range`, as it was given.
    heard: String,
    /// The text that replaces it.
    replacement: String,
    /// The offered candidate that let it through (rules 3 and 4).
    offered: Offered,
    /// Its C1 score (rule 2).
    similarity: Similarity,
}

impl AppliedFix {
    /// A fix of the chunk's `heard` text at `range`; only the gate calls
    /// it, once all four rules held.
    fn new(
        range: Range<usize>,
        heard: &str,
        replacement: &str,
        offered: &Offered,
        similarity: Similarity,
    ) -> Self {
        Self {
            range,
            heard: heard.to_owned(),
            replacement: replacement.to_owned(),
            offered: offered.clone(),
            similarity,
        }
    }

    /// Where the replaced text is in the chunk, in bytes, on character
    /// boundaries.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// The chunk's text that's replaced, as it was given (its case may
    /// differ from the proposal's).
    #[must_use]
    pub fn heard(&self) -> &str {
        &self.heard
    }

    /// The text that replaces it.
    #[must_use]
    pub fn replacement(&self) -> &str {
        &self.replacement
    }

    /// The offered candidate that let it through.
    #[must_use]
    pub fn offered(&self) -> &Offered {
        &self.offered
    }

    /// Its C1 score.
    #[must_use]
    pub fn similarity(&self) -> Similarity {
        self.similarity
    }
}

/// What the gate decided on one proposal.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every rule held, and the fix was placed.
    Applied(AppliedFix),
    /// A rule refused it.
    Rejected(Rejection),
}

/// The gate's decision on one proposal, with the score it rests on, for
/// the decision log.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// C1's score, once rule 2 was reached.
    similarity: Option<Similarity>,
    /// The verdict, and the rule that decided it.
    verdict: Verdict,
}

impl Decision {
    /// A refusal by `rule`, with the score if C1 was reached.
    fn rejected(rule: Rejection, similarity: Option<Similarity>) -> Self {
        Self {
            similarity,
            verdict: Verdict::Rejected(rule),
        }
    }

    /// C1's score, if the gate got as far as rule 2 and C1 compared the
    /// two sides.
    #[must_use]
    pub fn similarity(&self) -> Option<Similarity> {
        self.similarity
    }

    /// The verdict.
    #[must_use]
    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// The fix, if it was applied.
    #[must_use]
    pub fn applied(&self) -> Option<&AppliedFix> {
        match &self.verdict {
            Verdict::Applied(fix) => Some(fix),
            Verdict::Rejected(_) => None,
        }
    }

    /// The rule that refused it, if it was refused.
    #[must_use]
    pub fn rejection(&self) -> Option<Rejection> {
        match self.verdict {
            Verdict::Applied(_) => None,
            Verdict::Rejected(rule) => Some(rule),
        }
    }
}

/// The gate's answer for a chunk: a decision per proposal, in order, and
/// the revised text.
#[derive(Debug, Clone, PartialEq)]
pub struct Gated {
    /// One decision per proposal, in the proposals' order.
    decisions: Vec<Decision>,
    /// The chunk with every applied fix in place.
    text: String,
}

impl Gated {
    /// One decision per proposal, in the proposals' order.
    #[must_use]
    pub fn decisions(&self) -> &[Decision] {
        &self.decisions
    }

    /// The applied fixes, in the proposals' order.
    pub fn applied(&self) -> impl Iterator<Item = &AppliedFix> {
        self.decisions.iter().filter_map(Decision::applied)
    }

    /// The chunk with every applied fix in place: a new text, so the one
    /// the gate was given is unchanged.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// C2: gates each of `proposals` for `chunk` against the candidates
/// `offered` for it, in order, and makes the revised text.
///
/// Each proposal is checked against the chunk as given; an earlier fix
/// only changes where a later one may go (it can't overlap it).
#[must_use]
pub fn gate(chunk: &str, offered: &[Offered], proposals: &[Proposal]) -> Gated {
    let mut placed = Vec::new();
    let decisions: Vec<Decision> = proposals
        .iter()
        .map(|proposal| decide(chunk, offered, proposal, &mut placed))
        .collect();
    let text = revise(chunk, decisions.iter().filter_map(Decision::applied));
    Gated { decisions, text }
}

/// What rules 1 to 4 found for a proposal that passed them.
struct Passed<'a> {
    /// Its C1 score.
    similarity: Similarity,
    /// The candidate that let it through.
    offered: &'a Offered,
}

/// The decision on `proposal`, placing its fix after the ranges already
/// `placed` and adding its own.
fn decide(
    chunk: &str,
    offered: &[Offered],
    proposal: &Proposal,
    placed: &mut Vec<Range<usize>>,
) -> Decision {
    let passed = match check(chunk, offered, proposal) {
        Ok(passed) => passed,
        Err(refused) => return Decision::rejected(refused.rule, refused.similarity),
    };
    // The first match clear of every earlier fix: a fix changes only the
    // chunk's own words, never another fix's replacement.
    let free = matches(chunk, &proposal.heard).find(|r| !placed.iter().any(|p| overlap(p, r)));
    let Some(range) = free else {
        return Decision::rejected(Rejection::AlreadyChanged, Some(passed.similarity));
    };
    placed.push(range.clone());
    let heard = &chunk[range.clone()];
    let fix = AppliedFix::new(
        range,
        heard,
        &proposal.replacement,
        passed.offered,
        passed.similarity,
    );
    Decision {
        similarity: Some(passed.similarity),
        verdict: Verdict::Applied(fix),
    }
}

/// A refusal by a rule, and the score if C1 was reached: what [`check`]
/// returns for a proposal that fails.
struct Refused {
    /// The rule that refused it.
    rule: Rejection,
    /// C1's score, once rule 2 was reached.
    similarity: Option<Similarity>,
}

/// Rules 1 to 4 on `proposal`, in the bench's order, after refusing a span
/// with nothing to compare.
fn check<'a>(
    chunk: &str,
    offered: &'a [Offered],
    proposal: &Proposal,
) -> Result<Passed<'a>, Refused> {
    let refused = |rule, similarity| Refused { rule, similarity };
    // Before C1, which scores two empty strings 1: a span of punctuation
    // would pass rule 2 against a replacement of punctuation.
    if letters(&proposal.heard).next().is_none() {
        return Err(refused(Rejection::EmptySpan, None));
    }
    if matches(chunk, &proposal.heard).next().is_none() {
        return Err(refused(Rejection::SpanNotFound, None));
    }
    let similarity = letter_similarity(&proposal.heard, &proposal.replacement)
        .map_err(|TooLong| refused(Rejection::TooLong, None))?;
    if similarity.value() < SIMILARITY_MIN {
        return Err(refused(Rejection::Dissimilar, Some(similarity)));
    }
    let offered = offered_for(offered, proposal).map_err(|rule| refused(rule, Some(similarity)))?;
    Ok(Passed {
        similarity,
        offered,
    })
}

/// Rules 3 and 4: the first of `offered` whose term is inside the
/// replacement and whose window shares a word with the heard span.
///
/// # Errors
///
/// [`Rejection::WindowElsewhere`] if a term is inside but no such window
/// shares a word, and [`Rejection::NotOffered`] if no term is inside.
fn offered_for<'a>(offered: &'a [Offered], proposal: &Proposal) -> Result<&'a Offered, Rejection> {
    let replacement: Vec<u8> = letters(&proposal.replacement).collect();
    let heard = words(&proposal.heard);
    let mut inside = false;
    for candidate in offered {
        let term: Vec<u8> = letters(&candidate.term).collect();
        // An empty term would be inside every replacement.
        if term.is_empty() || !replacement.windows(term.len()).any(|w| w == term) {
            continue;
        }
        inside = true;
        if !words(&candidate.window).is_disjoint(&heard) {
            return Ok(candidate);
        }
    }
    Err(if inside {
        Rejection::WindowElsewhere
    } else {
        Rejection::NotOffered
    })
}

/// The words of `s` for rule 4: its runs of `a`–`z` and `0`–`9` once
/// lowercased, as the bench's `re.findall(r"[a-z0-9]+", s.lower())`.
fn words(s: &str) -> BTreeSet<Vec<u8>> {
    let mut out = BTreeSet::new();
    let mut word = Vec::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        match u8::try_from(c) {
            Ok(b) if b.is_ascii_lowercase() || b.is_ascii_digit() => word.push(b),
            _ if !word.is_empty() => {
                out.insert(std::mem::take(&mut word));
            }
            _ => {}
        }
    }
    if !word.is_empty() {
        out.insert(word);
    }
    out
}

/// Every place `heard` is in `chunk` on word boundaries, ignoring case, as
/// byte ranges, by where they start.
///
/// Matches may overlap one another, unlike Python's `finditer`; the gate
/// takes the first that's clear of earlier fixes.
fn matches<'a>(chunk: &'a str, heard: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
    let mut before = None;
    chunk.char_indices().filter_map(move |(start, c)| {
        // The bench's `(?<![A-Za-z0-9'])`: nothing of a word just before.
        if before.replace(c).is_some_and(in_word) {
            return None;
        }
        let end = start + match_len(&chunk[start..], heard)?;
        // And its `(?![A-Za-z0-9'])` just after.
        if chunk[end..].chars().next().is_some_and(in_word) {
            return None;
        }
        Some(start..end)
    })
}

/// How many bytes at the start of `text` match `heard`, a character at a
/// time ignoring case, or `None` if they don't.
fn match_len(text: &str, heard: &str) -> Option<usize> {
    let mut rest = text.char_indices();
    for h in heard.chars() {
        let (_, c) = rest.next()?;
        if !same_letter(h, c) {
            return None;
        }
    }
    Some(rest.next().map_or(text.len(), |(i, _)| i))
}

/// Whether `a` and `b` match ignoring case: equal, or with the same
/// lowercase.
fn same_letter(a: char, b: char) -> bool {
    a == b || fold(a) == fold(b)
}

/// `c`'s lowercase as one character, as Python's `re.IGNORECASE` takes it:
/// the first of a longer lowercase, which is Unicode's simple mapping for
/// the one letter that has one (`İ` lowercases to `i` and a dot).
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Whether `c` counts as part of a word for the boundary test: an ASCII
/// letter or digit, or `'`.
fn in_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '\''
}

/// Whether ranges `a` and `b` share a byte.
fn overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// `chunk` with each of `fixes` in place: a new text. The fixes' ranges
/// don't overlap, since the gate placed each clear of the others.
fn revise<'a>(chunk: &str, fixes: impl Iterator<Item = &'a AppliedFix>) -> String {
    let mut fixes: Vec<&AppliedFix> = fixes.collect();
    fixes.sort_by_key(|fix| fix.range.start);
    let mut out = String::with_capacity(chunk.len());
    let mut at = 0;
    for fix in fixes {
        out.push_str(&chunk[at..fix.range.start]);
        out.push_str(&fix.replacement);
        at = fix.range.end;
    }
    out.push_str(&chunk[at..]);
    out
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod props;
