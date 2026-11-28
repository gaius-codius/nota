//! Whether a replacement repeats the words just before the span it
//! replaces: "type 2 diabetes" for "2 diabetes" where the chunk reads "type
//! 2 diabetes" would make it read "type type 2 diabetes". Neither source of
//! candidates offers one (the code checks spec, C4 and the hotword diff).
//!
//! A replacement that is **wholly** the words before isn't a repeat here:
//! "furosemide" for "horosemi de" in "furosemide horosemi de" is a term
//! said twice, and the fix writes what was said. Only a replacement that
//! goes on past the repeated words, so its first words stand in front of
//! the span's own, is one.
//!
//! Words here are runs of letters and digits of any script, lowercased as
//! the gate's span match does, so case, punctuation and hyphens don't count
//! ("Type-2 diabetes" repeats "type"), "α" is a word, and an apostrophe
//! inside a word keeps it whole ("patient's" isn't "s"). Every count of
//! words is checked, so "type 2" before "diabetes" repeats too. The words
//! before may be across a sentence's end; a replacement that repeats them
//! is still passed over.

use crate::gate::fold;

/// Whether `replacement` starts with the last words of `before`, the text
/// just before its span, and goes on past them: for some count k of one or
/// more, fewer than its own words, its first k words are the last k words
/// of `before`.
///
/// ```
/// use nota_checks::repeats_before;
///
/// assert!(repeats_before("She has type", "type 2 diabetes"));
/// assert!(repeats_before("She has Type 2", "type 2 diabetes"));
/// assert!(!repeats_before("She has", "type 2 diabetes"));
/// // A term said twice.
/// assert!(!repeats_before("Then furosemide", "furosemide"));
/// ```
#[must_use]
pub fn repeats_before(before: &str, replacement: &str) -> bool {
    repeats(&words(before), &words(replacement))
}

/// Whether `replacement`'s words start with the last of `before`'s: the
/// same test as [`repeats_before`], on words already split.
pub(crate) fn repeats(before: &[String], replacement: &[String]) -> bool {
    // Fewer than all of the replacement's words: one wholly the words
    // before is a term said twice.
    let most = before.len().min(replacement.len().saturating_sub(1));
    (1..=most).any(|k| before[before.len() - k..] == replacement[..k])
}

/// The words of `s`, in order: see [`spans`].
pub(crate) fn words(s: &str) -> Vec<String> {
    spans(s).into_iter().map(|(_, word)| word).collect()
}

/// The words of `s`, in order, each with the byte where it ends: runs of
/// letters and digits of any script, each lowercased as the gate's span
/// match does, with an apostrophe between two of them kept inside the word
/// ("patient's" is one word, so it isn't "s").
pub(crate) fn spans(s: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next_in_word = chars.peek().is_some_and(|&(_, n)| n.is_alphanumeric());
        if c.is_alphanumeric() {
            word.push(fold(c));
        } else if matches!(c, '\'' | '\u{2019}') && !word.is_empty() && next_in_word {
            word.push('\'');
        } else if !word.is_empty() {
            out.push((i, std::mem::take(&mut word)));
        }
    }
    if !word.is_empty() {
        out.push((s.len(), word));
    }
    out
}

#[cfg(test)]
mod tests;
