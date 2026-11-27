//! C3, the similarity pre-check (proposed; test 3 decides whether it's
//! adopted): before the decider is asked about a candidate, C1 scores its
//! window against its term. Below [`SIMILARITY_MIN`] the gate refuses any
//! yes on it, so there's nothing for the decider to decide, unless
//! another signal already marks the span as suspect: then a yes is still
//! worth having, as one of the signals that make a "to check" card.
//!
//! The one other signal so far is a low log-probability ([`LowLogProb`]):
//! the recogniser was among its least sure on one of the span's words.
//! Packs have no facts yet, so a fact match can't mark a span. Where no
//! word of a recording has a log-probability, nothing marks any span, and
//! the pre-check is a plain one: every candidate below the minimum is
//! skipped, and none of them can become a card.
//!
//! **A skipped candidate could never have been applied.** A yes on it is
//! the proposal of its window as the span and its term as the
//! replacement, which is what C1 compares here, at the gate's rule 2. So
//! the gate refuses it whatever the chunk, and a refused proposal places
//! nothing that could move another: skipping it leaves every other
//! proposal's decision as it was. Only a [`Skipped`] (private fields, made
//! only by [`precheck`]) says a candidate may go unasked.
//!
//! ```
//! use nota_checks::{PreCheck, precheck};
//!
//! // "amylo edema" offered as "amlodipine" scores under the minimum.
//! let PreCheck::Skip(skipped) = precheck("amylo edema", "amlodipine", None) else {
//!     panic!("asked");
//! };
//! assert!(skipped.similarity().value() < nota_checks::SIMILARITY_MIN);
//! ```

use crate::gate::SIMILARITY_MIN;
use crate::similarity::{Similarity, TooLong, letter_similarity};

#[cfg(test)]
mod props;
#[cfg(test)]
mod tests;

/// The share of a recording's words, in percent, with the lowest
/// log-probabilities that mark their spans as suspect: T10's lowest 5%,
/// which held 19 of the bench's 21 garbled terms (#149). A fixed cut
/// near −0.5 did as well there, but was tuned on that recording; a share
/// of each recording is the safer default until a second one confirms a
/// value.
pub const LOW_LOG_PROB_PERCENT: usize = 5; // check-bound

/// A recording's lowest band of log-probabilities: the words at or below
/// its cut are the [`LOW_LOG_PROB_PERCENT`] the recogniser was least sure
/// of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LowLogProb {
    /// The highest log-probability inside the band.
    cut: f32,
}

impl LowLogProb {
    /// The band of a recording whose words have the log-probabilities
    /// `known` (one per word that has one): the lowest
    /// [`LOW_LOG_PROB_PERCENT`] of them, rounded down, as T10 counted (248
    /// of 4,963 words). `None` when that's no word at all: fewer than 20
    /// words have a log-probability, or none does. A value that isn't a
    /// number (NaN, or infinite) isn't counted.
    #[must_use]
    pub fn of(known: impl IntoIterator<Item = f32>) -> Option<Self> {
        let mut values: Vec<f32> = known.into_iter().filter(|v| v.is_finite()).collect();
        let n = values.len();
        // n × percent ÷ 100, rounded down, without overflowing.
        let band = n / 100 * LOW_LOG_PROB_PERCENT + n % 100 * LOW_LOG_PROB_PERCENT / 100;
        let last = band.checked_sub(1)?;
        values.sort_by(f32::total_cmp);
        values.get(last).map(|&cut| Self { cut })
    }

    /// The highest log-probability inside the band.
    #[must_use]
    pub const fn cut(self) -> f32 {
        self.cut
    }

    /// Whether a span whose lowest log-probability is `lowest` is in the
    /// band.
    #[must_use]
    pub fn marks(self, lowest: f32) -> bool {
        lowest <= self.cut
    }
}

/// What, beside the decider, marks a span as suspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// One of the span's words is in the recording's [`LowLogProb`] band.
    LowLogProb,
}

/// Why the pre-check lets the decider be asked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Asked {
    /// C1 is at least [`SIMILARITY_MIN`]: the gate may apply a yes.
    Similar(Similarity),
    /// C1 is below it, but `signal` marks the span: a yes can't be
    /// applied, but may make a card.
    Marked {
        /// C1's score.
        similarity: Similarity,
        /// What marks the span.
        signal: Signal,
    },
    /// C1 can't compare the two (one is too long): the gate decides.
    Unscored,
}

/// A candidate the decider needn't be asked about: C1 scores it below
/// [`SIMILARITY_MIN`] and nothing marks its span.
///
/// Only [`precheck`] makes one: its field is private, so holding one
/// shows the score was below the minimum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Skipped {
    /// C1's score.
    similarity: Similarity,
}

impl Skipped {
    /// C1's score, below [`SIMILARITY_MIN`].
    #[must_use]
    pub const fn similarity(self) -> Similarity {
        self.similarity
    }
}

/// The pre-check's answer on one candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PreCheck {
    /// Ask the decider, for the reason given.
    Ask(Asked),
    /// Don't: reject it and log it without a call.
    Skip(Skipped),
}

/// C3 on the candidate that `window` was meant as `term`, with `signal`
/// if something marks the window as suspect.
#[must_use]
pub fn precheck(window: &str, term: &str, signal: Option<Signal>) -> PreCheck {
    let similarity = match letter_similarity(window, term) {
        Ok(similarity) => similarity,
        // The gate refuses it too, but the rule is C1's score: a side too
        // long to score is left to the gate.
        Err(TooLong) => return PreCheck::Ask(Asked::Unscored),
    };
    if similarity.value() >= SIMILARITY_MIN {
        return PreCheck::Ask(Asked::Similar(similarity));
    }
    match signal {
        Some(signal) => PreCheck::Ask(Asked::Marked { similarity, signal }),
        None => PreCheck::Skip(Skipped { similarity }),
    }
}
