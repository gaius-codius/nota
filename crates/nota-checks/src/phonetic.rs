//! How alike two phrases sound, C4's main score: Double Metaphone codes for
//! each, with the words run together ("a bloody pine" sounds like
//! "abloodypine"), compared by edit distance.
//!
//! The codes are `rphonetic`'s, built with no length cap
//! (`DoubleMetaphone::new(None)`). Its default caps a code at 4 characters,
//! which changes 53 of the bench's 60 chunks; uncapped, it gives the bench's
//! candidates on all 60, though 26 of 8,607 codes differ from the Python
//! `metaphone` the bench used (research note 2026-11-10, #148). Those
//! differences, in three patterns, are pinned by this module's tests:
//! - a final `CH` has no `X` alternate ("much" is `MK` only, not `MK|MX`);
//! - `MB` before `ER` drops the `P` ("number" is `NMR`, not `NMPR`);
//! - an initial `GH` is one `H` ("high" is `H`, not `HH`).

use rphonetic::DoubleMetaphone;

/// A phrase's Double Metaphone codes: its primary code and, when it differs,
/// its alternate. None for a phrase with no letter `a`–`z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sounds(Vec<String>);

impl Sounds {
    /// The codes, primary first, with no repeat and none empty.
    #[must_use]
    pub fn codes(&self) -> &[String] {
        &self.0
    }
}

/// English Double Metaphone, as the bench's `lookup.py` `codes()` used it.
#[derive(Debug)]
pub(crate) struct Metaphone(DoubleMetaphone);

impl Metaphone {
    /// The encoder, with no cap on a code's length.
    pub(crate) fn new() -> Self {
        Self(DoubleMetaphone::new(None))
    }

    /// The codes of `text`: lower-cased, with everything but the letters
    /// `a`–`z` dropped (spaces, digits, accented letters), so its words run
    /// together.
    pub(crate) fn sounds(&self, text: &str) -> Sounds {
        let letters: String = text
            .chars()
            .flat_map(char::to_lowercase)
            .filter(char::is_ascii_lowercase)
            .collect();
        let found = self.0.double_metaphone(&letters);
        let mut codes = Vec::with_capacity(2);
        for code in [found.primary(), found.alternate()] {
            if !code.is_empty() && !codes.contains(&code) {
                codes.push(code);
            }
        }
        Sounds(codes)
    }
}

/// How alike `a` and `b` sound, from 0 to 1: the best, over each pair of
/// their codes, of 1 less the edit distance over the longer code's length.
/// 0 if either has no code.
pub(crate) fn similarity(a: &Sounds, b: &Sounds) -> f64 {
    a.0.iter()
        .flat_map(|x| b.0.iter().map(move |y| code_similarity(x, y)))
        .fold(0.0, f64::max)
}

/// 1 less the edit distance between codes `a` and `b` over the longer one's
/// length. Codes are ASCII capitals and digits, so bytes are characters.
#[expect(
    clippy::cast_precision_loss,
    reason = "a code's length is far below 2^52, exact in an f64"
)]
fn code_similarity(a: &str, b: &str) -> f64 {
    let longer = a.len().max(b.len());
    if longer == 0 {
        return 0.0;
    }
    1.0 - strsim::levenshtein(a, b) as f64 / longer as f64
}

#[cfg(test)]
mod tests;
