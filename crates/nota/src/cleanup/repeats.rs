//! Which time a candidate's heard words occur in its chunk, where they
//! occur more than once, so the decider's question can say which one it
//! asks about.
//!
//! A yes is applied only at the place the lookup offered it, but the chunk
//! the decider reads may show the same words twice. Without the time named,
//! two candidates on the same words read alike, and a lone one may be
//! taken for the other time (one a review holds, say).
//!
//! The words are counted as the lookup splits them (runs of ASCII letters,
//! digits, apostrophes and hyphens), since a candidate's heard words are
//! the lookup's window, joined by one space: "amylo, diping" in the chunk
//! is the window "amylo diping". Case is ignored, so "Amylo diping" counts
//! as the same words: a time named needlessly costs nothing, and one missed
//! leaves two lines alike.

use std::ops::Range;

use nota_core::decide::Occurrence;

/// Which time the heard words `window` occur in `text`, for the window
/// that starts at byte `start`, or `None` if they occur only once (or not
/// at all, which a window the lookup gave never does).
pub(crate) fn occurrence(text: &str, window: &str, start: usize) -> Option<Occurrence> {
    let starts = starts(text, window);
    let of = starts.len();
    let nth = starts.iter().filter(|&&at| at <= start).count();
    Occurrence::new(count(nth), count(of))
}

/// Where each time the words `window` occur in `text` starts, in bytes, in
/// order.
fn starts(text: &str, window: &str) -> Vec<usize> {
    let wanted: Vec<&str> = window.split(' ').filter(|w| !w.is_empty()).collect();
    if wanted.is_empty() {
        // `windows(0)` would panic, and no words occur nowhere.
        return Vec::new();
    }
    words(text)
        .windows(wanted.len())
        .filter(|run| {
            run.iter()
                .zip(&wanted)
                .all(|((word, _), want)| word.eq_ignore_ascii_case(want))
        })
        .filter_map(|run| run.first().map(|(_, at)| at.start))
        .collect()
}

/// The words of `text` as the lookup splits them, each with its byte
/// range: runs of ASCII letters, digits, apostrophes and hyphens.
fn words(text: &str) -> Vec<(&str, Range<usize>)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        let in_word = c.is_ascii_alphanumeric() || c == '\'' || c == '-';
        match (start, in_word) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                if let Some(word) = text.get(s..i) {
                    out.push((word, s..i));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// A count of times as an occurrence takes it. A chunk is at most a few
/// thousand bytes, far below `u32::MAX` words.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
