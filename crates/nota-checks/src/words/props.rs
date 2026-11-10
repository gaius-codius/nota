//! Properties of the word list's reader and the Zipf port:
//! - any bytes are read or refused, never a panic, gzipped or not;
//! - a cBpack read back gives each word the list it was written in;
//! - tokens are lower-case letters, digits and inner apostrophes, never
//!   empty;
//! - a Zipf value is from 0 to 9, to two places.

use proptest::prelude::*;

use super::tests::{HEADER, cbpack};
use super::*;
use crate::Language;
use crate::language::english;

/// Words of a cBpack: short strings of any characters.
fn any_word() -> impl Strategy<Value = String> {
    "\\PC{1,8}"
}

/// Text in the lookup's word domain, with other characters mixed in.
fn any_text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9'\\- é’.]{0,24}"
}

proptest! {
    /// Any bytes are read or refused, never a panic.
    #[test]
    fn any_bytes_are_read_or_refused(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = WordList::from_cbpack(&bytes);
        let _ = WordList::from_gzip(&bytes);
    }

    /// A valid cBpack's bytes with one changed are read or refused, never a
    /// panic.
    #[test]
    fn a_changed_byte_is_read_or_refused(at in any::<prop::sample::Index>(), to in any::<u8>()) {
        let mut bytes = cbpack(HEADER, &[&["red"], &[], &["blue", "fish"]]);
        let i = at.index(bytes.len());
        bytes[i] = to;
        let _ = WordList::from_cbpack(&bytes);
    }

    /// A cBpack reads back as the words and lists it was written with; a
    /// word written twice keeps its last list.
    #[test]
    fn a_cbpack_reads_back(lists in proptest::collection::vec(proptest::collection::vec(any_word(), 0..6), 0..12)) {
        let refs: Vec<Vec<&str>> = lists.iter().map(|l| l.iter().map(String::as_str).collect()).collect();
        let slices: Vec<&[&str]> = refs.iter().map(Vec::as_slice).collect();
        let list = WordList::from_cbpack(&cbpack(HEADER, &slices)).unwrap();
        let mut want = HashMap::new();
        for (i, words) in lists.iter().enumerate() {
            for w in words {
                want.insert(w.as_str().into(), u16::try_from(i).unwrap());
            }
        }
        prop_assert_eq!(list.buckets, want);
    }

    /// Tokens are non-empty runs of lower-case letters and digits, joined
    /// only by single inner apostrophes.
    #[test]
    fn tokens_are_letters_digits_and_inner_apostrophes(text in any_text()) {
        for token in tokens(&text) {
            prop_assert!(!token.is_empty() && !token.starts_with('\'') && !token.ends_with('\''), "{:?}", token);
            prop_assert!(!token.contains("''"), "{:?}", token);
            prop_assert!(token.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '\''), "{:?}", token);
        }
    }

    /// A Zipf value is from 0 to 9 and has at most two places.
    #[test]
    fn a_zipf_value_is_from_0_to_9(text in any_text()) {
        let zipf = english().zipf(&text).value();
        prop_assert!((0.0..=9.0).contains(&zipf), "{} for {:?}", zipf, text);
        prop_assert!(((zipf * 100.0).round() / 100.0 - zipf).abs() < 1e-9, "{} for {:?}", zipf, text);
    }
}
