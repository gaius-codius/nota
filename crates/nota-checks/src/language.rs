//! What C4's lookup needs to know about a language: how common a word is,
//! how a phrase sounds, and which common word a misheard term may start
//! with. English is the one language so far; its rules are the bench's.

use std::fmt;

use crate::phonetic::{Metaphone, Sounds};
use crate::words::{WordList, WordListError, Zipf};

/// A language's rules for the term lookup. `Send` and `Sync`, so one
/// language's rules, and a lookup built on them, can be shared with the
/// thread that runs the clean-up.
pub trait Language: fmt::Debug + Send + Sync {
    /// The language's code, as a pack names it (`en`). The lookup leaves
    /// out terms in any other language.
    fn code(&self) -> &'static str;

    /// How common `word` is. The lookup asks only about one word of a
    /// window: ASCII letters, digits, apostrophes and hyphens.
    fn zipf(&self, word: &str) -> Zipf;

    /// How `text` sounds, its words run together.
    fn sounds(&self, text: &str) -> Sounds;

    /// Whether a window may start on `word` although it's a function word:
    /// a misheard term often starts with one (English: "a", as in "a
    /// bloody pine").
    fn may_open_on(&self, word: &str) -> bool;
}

/// English: wordfreq's English list and Double Metaphone.
#[derive(Debug)]
pub struct English {
    /// wordfreq's `large_en` list.
    words: WordList,
    /// Double Metaphone, uncapped.
    metaphone: Metaphone,
}

impl English {
    /// English's rules, with the word list unpacked from the copy built in
    /// (a few tenths of a second; build it once and share it).
    ///
    /// # Errors
    ///
    /// [`WordListError`] if the built-in word list is damaged.
    pub fn new() -> Result<Self, WordListError> {
        Ok(Self {
            words: WordList::english()?,
            metaphone: Metaphone::new(),
        })
    }
}

impl Language for English {
    fn code(&self) -> &'static str {
        "en"
    }

    fn zipf(&self, word: &str) -> Zipf {
        self.words.zipf(word)
    }

    fn sounds(&self, text: &str) -> Sounds {
        self.metaphone.sounds(text)
    }

    fn may_open_on(&self, word: &str) -> bool {
        word.eq_ignore_ascii_case("a")
    }
}

/// English's rules, built once for every test in the binary that needs
/// them: unpacking the word list takes a while.
#[cfg(test)]
pub(crate) fn english() -> &'static English {
    static ENGLISH: std::sync::LazyLock<English> =
        std::sync::LazyLock::new(|| English::new().expect("the built-in word list reads"));
    &ENGLISH
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In English a window may open on "a", in either case, and on no
    /// other function word.
    #[test]
    fn english_windows_may_open_on_a() {
        let english = english();
        assert!(english.may_open_on("a") && english.may_open_on("A"));
        assert!(!english.may_open_on("the") && !english.may_open_on("an"));
        assert_eq!(english.code(), "en");
    }
}
