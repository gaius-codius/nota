use super::*;

/// The bench's case: "amylo edema" was heard for "ankle oedema", and the
/// lookup offered "amlodipine" for it. C1 scores the pair under the
/// minimum, so with nothing else to go on it isn't asked.
#[test]
fn a_dissimilar_candidate_is_skipped() {
    let PreCheck::Skip(skipped) = precheck("amylo edema", "amlodipine", None) else {
        panic!("asked");
    };
    let want = letter_similarity("amylo edema", "amlodipine").unwrap();
    assert_eq!(skipped.similarity(), want);
    assert!(want.value() < SIMILARITY_MIN, "{want:?}");
}

/// A low log-probability on the span gets the same candidate asked, so a
/// yes on it can still make a card.
#[test]
fn a_marked_candidate_is_asked() {
    let similarity = letter_similarity("amylo edema", "amlodipine").unwrap();
    assert_eq!(
        precheck("amylo edema", "amlodipine", Some(Signal::LowLogProb)),
        PreCheck::Ask(Asked::Marked {
            similarity,
            signal: Signal::LowLogProb
        })
    );
}

/// A candidate C1 scores at the minimum is asked: the gate may apply it.
#[test]
fn a_similar_candidate_is_asked() {
    let similarity = letter_similarity("amylo diping", "amlodipine").unwrap();
    assert!(similarity.value() >= SIMILARITY_MIN);
    assert_eq!(
        precheck("amylo diping", "amlodipine", None),
        PreCheck::Ask(Asked::Similar(similarity))
    );
}

/// A score of exactly the minimum is asked, as the gate passes it: "abc"
/// against "abcdefg" scores 2 × 3 / 10 = 0.6.
#[test]
fn a_score_of_exactly_the_minimum_is_asked() {
    let similarity = letter_similarity("abc", "abcdefg").unwrap();
    // Exactly, to the bit: not a hair under it.
    assert_eq!(similarity.value().to_bits(), SIMILARITY_MIN.to_bits());
    assert_eq!(
        precheck("abc", "abcdefg", None),
        PreCheck::Ask(Asked::Similar(similarity))
    );
}

/// A side too long for C1 to score is left to the gate.
#[test]
fn an_unscored_candidate_is_asked() {
    let long = "a".repeat(crate::MAX_LETTERS + 1);
    assert_eq!(
        precheck(&long, "amlodipine", None),
        PreCheck::Ask(Asked::Unscored)
    );
}

/// The band of 100 values is the lowest five; its cut is the fifth
/// lowest, which it marks, and the sixth it doesn't.
#[test]
fn the_band_is_the_lowest_five_percent() {
    let values = (0..100_u8).map(|i| -f32::from(i) / 10.0);
    let low = LowLogProb::of(values).unwrap();
    assert_eq!(low, LowLogProb { cut: -9.5 });
    assert!(low.marks(-9.5));
    assert!(low.marks(-9.9));
    assert!(!low.marks(-9.4));
}

/// T10's count: 4,963 words make a band of 248, rounded down.
#[test]
fn the_band_rounds_down_as_t10_counted() {
    let values: Vec<f32> = (0..4963_u16).map(|i| -f32::from(i)).collect();
    let low = LowLogProb::of(values).unwrap();
    // The 248th lowest of 0, −1, … −4962 is −4715.
    assert_eq!(low, LowLogProb { cut: -4715.0 });
}

/// Fewer than 20 known values make no band: 5% of them is no word.
#[test]
fn too_few_values_make_no_band() {
    assert_eq!(LowLogProb::of((0..19_u8).map(|i| -f32::from(i))), None);
    assert!(LowLogProb::of((0..20_u8).map(|i| -f32::from(i))).is_some());
    assert_eq!(LowLogProb::of([]), None);
}

/// A value that isn't a number is left out of the count.
#[test]
fn values_that_are_not_numbers_are_left_out() {
    let mut values: Vec<f32> = (0..20_u8).map(|i| -f32::from(i)).collect();
    values.push(f32::NAN);
    values.push(f32::NEG_INFINITY);
    let low = LowLogProb::of(values).unwrap();
    assert_eq!(low, LowLogProb { cut: -19.0 });
}
