//! Double Metaphone as nota uses it, and where it differs from the Python
//! `metaphone` the bench used.

use proptest::prelude::*;

use super::*;

/// The codes of `text`, as strings.
fn codes(text: &str) -> Vec<String> {
    Metaphone::new().sounds(text).codes().to_vec()
}

/// rphonetic's three known differences from Python's `metaphone`, as
/// rphonetic 4.0.0 gives them. A release that changes them changes C4's
/// scores, and fails here first.
#[test]
fn rphonetics_known_differences_hold() {
    // Python: MK and MX; RK and RX; NMPR; HH.
    assert_eq!(codes("much"), ["MK"]);
    assert_eq!(codes("reach"), ["RK"]);
    assert_eq!(codes("number"), ["NMR"]);
    assert_eq!(codes("high"), ["H"]);
}

/// Codes aren't cut at 4 characters, as rphonetic's default would cut them.
#[test]
fn codes_are_not_cut_short() {
    assert_eq!(codes("amlodipine"), ["AMLTPN"]);
    assert_eq!(codes("Smith"), ["SM0", "XMT"]);
}

/// Spaces, digits, punctuation and accented letters are dropped before
/// encoding, so a phrase's words run together.
#[test]
fn only_letters_a_to_z_are_encoded() {
    assert_eq!(codes("a bloody pine"), codes("abloodypine"));
    assert_eq!(codes("GLP-1"), codes("glp"));
    assert_eq!(codes("café"), codes("caf"));
    assert_eq!(codes("123 - !"), Vec::<String>::new());
}

/// Two phrases sound alike by the best pair of their codes; one with no
/// code is like nothing.
#[test]
fn similarity_takes_the_best_pair_of_codes() {
    let m = Metaphone::new();
    // SM0|XMT against XMT: the alternates match.
    assert!((similarity(&m.sounds("Smith"), &m.sounds("Schmidt")) - 1.0).abs() < 1e-12);
    // AMLTPN against AMLTP: one edit in six.
    let got = similarity(&m.sounds("amlodipine"), &m.sounds("amlodip"));
    assert!((got - 5.0 / 6.0).abs() < 1e-12, "{got}");
    assert!(similarity(&m.sounds("123"), &m.sounds("amlodipine")).abs() < 1e-12);
    assert!(similarity(&m.sounds("amlodipine"), &m.sounds("")).abs() < 1e-12);
}

proptest! {
    /// Any text is encoded without a panic, into at most two distinct
    /// non-empty codes.
    #[test]
    fn any_text_encodes(text in "\\PC{0,40}") {
        let sounds = Metaphone::new().sounds(&text);
        let codes = sounds.codes();
        prop_assert!(codes.len() <= 2 && codes.iter().all(|c| !c.is_empty()), "{:?}", codes);
        prop_assert!(codes.len() < 2 || codes[0] != codes[1], "{:?}", codes);
    }
}
