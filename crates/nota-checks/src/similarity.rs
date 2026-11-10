//! C1, letter similarity: how alike a heard span and a term are in their
//! letters, from 0 to 1. It's the bench's filter (`regate.py` and
//! `fixes.py`'s `sim`), ported so its numbers stay valid: lowercase both
//! strings, keep only `a`–`z` and `0`–`9`, then take Python's
//! `difflib.SequenceMatcher(None, a, b).ratio()`.
//!
//! That ratio is Ratcliff/Obershelp matching, not an edit distance: find the
//! longest common block (the leftmost in `a`, then in `b`, on a tie), do the
//! same either side of it, and score twice the matched letters over the
//! letters in both strings. It isn't symmetric: on a tie the leftmost block
//! depends on which string is `a`, so the heard span goes first, as in the
//! bench.
//!
//! Where it differs from Python, deliberately:
//! - **A side of more than [`MAX_LETTERS`] letters and digits is refused.**
//!   From 200, Python's default drops letters that are common in `b`
//!   ("autojunk"), which changes the score; terms are far shorter, and the
//!   limit also bounds the work, which grows with the product of the two
//!   lengths.
//!
//! What it shares with Python, which callers should know:
//! - two strings that are both empty once normalised (`"!!"` and `"--"`)
//!   score 1, and one empty side scores 0;
//! - lowercasing is Unicode's, so the Kelvin sign `K` counts as `k`, and
//!   letters that lowercase to anything outside `a`–`z` (`é`, `ß`) are
//!   dropped.

use std::error::Error;
use std::fmt;
use std::ops::Range;

/// The most letters and digits C1 compares on either side, after
/// normalising: Python's autojunk starts at 200 (`difflib`'s
/// `SequenceMatcher.__chain_b`).
pub const MAX_LETTERS: usize = 199;

/// A C1 score: twice the matched letters and digits over the letters and
/// digits in both strings, from 0 (nothing in common) to 1 (the same).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Similarity(f64);

impl Similarity {
    /// The score, from 0 to 1.
    #[must_use]
    pub fn value(self) -> f64 {
        self.0
    }
}

/// A side had more than [`MAX_LETTERS`] letters and digits, so it wasn't
/// compared: no score is the right answer, since Python's would have changed
/// its rule there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLong;

impl fmt::Display for TooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "too long to compare: over {MAX_LETTERS} letters and digits"
        )
    }
}

impl Error for TooLong {}

/// C1, letter similarity: how alike `heard` and `term` are, as the bench
/// scored them.
///
/// # Errors
///
/// [`TooLong`] if either side has more than [`MAX_LETTERS`] letters and
/// digits once normalised.
pub fn letter_similarity(heard: &str, term: &str) -> Result<Similarity, TooLong> {
    let a = normalise(heard)?;
    let b = normalise(term)?;
    let whole = Region {
        a: 0..a.len(),
        b: 0..b.len(),
    };
    Ok(Similarity(ratio(
        matched(&a, &b, &whole),
        a.len() + b.len(),
    )))
}

/// The ASCII letters and digits of `s`, lowercased, as bytes.
///
/// # Errors
///
/// [`TooLong`] past [`MAX_LETTERS`]. It stops at the first letter or digit
/// past the limit, so it never keeps more than that; the characters it drops
/// are still read.
fn normalise(s: &str) -> Result<Vec<u8>, TooLong> {
    let kept: Vec<u8> = s
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| u8::try_from(c).ok())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .take(MAX_LETTERS + 1)
        .collect();
    if kept.len() > MAX_LETTERS {
        Err(TooLong)
    } else {
        Ok(kept)
    }
}

/// difflib's `_calculate_ratio`: twice `matched` over `total`, and 1 for two
/// empty strings.
#[expect(
    clippy::cast_precision_loss,
    reason = "both counts are at most twice MAX_LETTERS, exact in an f64"
)]
fn ratio(matched: usize, total: usize) -> f64 {
    if total == 0 {
        return 1.0;
    }
    // The same operations in the same order as Python, so the float is the
    // same to the last bit.
    2.0 * matched as f64 / total as f64
}

/// The part of `a` and `b` still to be matched.
#[derive(Debug, Clone)]
struct Region {
    /// The positions in `a`.
    a: Range<usize>,
    /// The positions in `b`.
    b: Range<usize>,
}

/// A block of `len` equal elements at `a` in one string and `b` in the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Block {
    /// Where it starts in `a`.
    a: usize,
    /// Where it starts in `b`.
    b: usize,
    /// How long it is.
    len: usize,
}

/// How many elements difflib's `get_matching_blocks` matches in `region`:
/// its longest block, then the same on each side of it.
///
/// difflib keeps a queue of regions; the blocks are the same in any order,
/// and only their total is needed. The depth is at most the number of
/// blocks, which [`MAX_LETTERS`] bounds.
fn matched(a: &[u8], b: &[u8], region: &Region) -> usize {
    let best = longest_match(a, b, region);
    if best.len == 0 {
        return 0;
    }
    let before = Region {
        a: region.a.start..best.a,
        b: region.b.start..best.b,
    };
    let after = Region {
        a: best.a + best.len..region.a.end,
        b: best.b + best.len..region.b.end,
    };
    best.len + matched(a, b, &before) + matched(a, b, &after)
}

/// difflib's `find_longest_match` with no junk: the longest block of equal
/// elements in `region`, the one starting earliest in `a` on a tie, and then
/// earliest in `b`. A region with no match gives a block of length 0.
///
/// Python then extends the block over junk and "popular" elements. With no
/// junk function and autojunk unreachable (no side reaches 200), there are
/// none, and the block found here is already as long as it can be.
fn longest_match(a: &[u8], b: &[u8], region: &Region) -> Block {
    let mut best = Block {
        a: region.a.start,
        b: region.b.start,
        len: 0,
    };
    // `prev[j + 1]` is the length of the match ending at the previous `a`
    // and `b[j]`, as difflib's `j2len[j]`; outside the region it stays 0.
    let mut prev = vec![0; b.len() + 1];
    let mut cur = vec![0; b.len() + 1];
    for (i, x) in a.iter().enumerate().take(region.a.end).skip(region.a.start) {
        for (j, y) in b.iter().enumerate().take(region.b.end).skip(region.b.start) {
            let len = if x == y { prev[j] + 1 } else { 0 };
            cur[j + 1] = len;
            // Strictly longer, so the first block found wins a tie.
            if len > best.len {
                best = Block {
                    a: i + 1 - len,
                    b: j + 1 - len,
                    len,
                };
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    best
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod props;
