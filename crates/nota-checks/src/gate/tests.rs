//! The gate's rules one at a time, on invented text: each refusal, each
//! boundary of the span match, the threshold, and how fixes are placed.

use super::*;

/// An offered candidate: `term` for the chunk's `window`.
fn offer(window: &str, term: &str) -> Offered {
    Offered {
        window: window.to_owned(),
        term: term.to_owned(),
    }
}

/// A proposal to replace `heard` with `replacement`.
fn propose(heard: &str, replacement: &str) -> Proposal {
    Proposal {
        heard: heard.to_owned(),
        replacement: replacement.to_owned(),
    }
}

/// The C1 score of `heard` against `replacement`.
fn score(heard: &str, replacement: &str) -> Similarity {
    letter_similarity(heard, replacement).unwrap()
}

/// The one decision the gate makes on `proposal` alone.
fn one(chunk: &str, offered: &[Offered], proposal: Proposal) -> Decision {
    let gated = gate(chunk, offered, &[proposal]);
    let [decision] = gated.decisions() else {
        panic!("{gated:?}")
    };
    decision.clone()
}

/// The amlodipine candidate the tests offer.
fn amlodipine() -> Offered {
    offer("amylo diping", "amlodipine")
}

/// A proposal that passes every rule is applied where its span is, through
/// the candidate offered, and the revised text has the replacement.
#[test]
fn a_proposal_that_passes_every_rule_is_applied() {
    let chunk = "start amylo diping daily";
    let gated = gate(
        chunk,
        &[amlodipine()],
        &[propose("amylo diping", "amlodipine")],
    );
    let similarity = score("amylo diping", "amlodipine");
    let fix = AppliedFix::new(
        6..18,
        "amylo diping",
        "amlodipine",
        &amlodipine(),
        similarity,
    );
    assert_eq!(
        gated,
        Gated {
            decisions: vec![Decision {
                similarity: Some(similarity),
                verdict: Verdict::Applied(fix),
            }],
            text: "start amlodipine daily".to_owned(),
        }
    );
}

/// A span of punctuation is refused before C1, which would score it 1
/// against punctuation; even an offered candidate of punctuation doesn't
/// let it through.
#[test]
fn a_span_with_no_letters_is_refused_before_c1() {
    let decision = one("well !! then", &[offer("!!", "--")], propose("!!", "--"));
    assert_eq!(decision, Decision::rejected(Rejection::EmptySpan, None));
}

/// An offered term of punctuation is inside no replacement, though in
/// Python an empty string is inside every one.
#[test]
fn a_term_with_no_letters_counts_for_nothing() {
    let decision = one(
        "start amylo diping daily",
        &[offer("amylo diping", "!!")],
        propose("amylo diping", "amlodipine"),
    );
    let similarity = score("amylo diping", "amlodipine");
    assert_eq!(
        decision,
        Decision::rejected(Rejection::NotOffered, Some(similarity))
    );
}

/// A span that isn't in the chunk is refused, and no score is taken.
#[test]
fn a_span_not_in_the_chunk_is_refused() {
    let decision = one(
        "start the tablets daily",
        &[amlodipine()],
        propose("amylo diping", "amlodipine"),
    );
    assert_eq!(decision, Decision::rejected(Rejection::SpanNotFound, None));
}

/// Whether `heard` is found in `chunk`, for the boundary tests.
fn found(chunk: &str, heard: &str) -> bool {
    matches(chunk, heard).next().is_some()
}

/// A span inside a longer word isn't found: a letter, digit or apostrophe
/// next to it is part of the word.
#[test]
fn a_span_inside_a_word_is_not_found() {
    let inside = [
        ("the oedema", "edema"),
        ("the edemas", "edema"),
        ("over 1000", "100"),
        ("the dose's size", "dose"),
        ("the 'dose", "dose"),
    ];
    let found: Vec<_> = inside.iter().filter(|(c, h)| found(c, h)).collect();
    assert_eq!(found, Vec::<&(&str, &str)>::new());
}

/// A span next to punctuation other than an apostrophe, or at either end of
/// the chunk, is found: "up" in "follow-up", as in the bench.
#[test]
fn a_span_next_to_other_punctuation_is_found() {
    let edges = [
        ("a follow-up", "up"),
        ("edema, then", "edema"),
        ("(edema)", "edema"),
        ("edema", "edema"),
        ("over 100.", "100"),
    ];
    let missed: Vec<_> = edges.iter().filter(|(c, h)| !found(c, h)).collect();
    assert_eq!(missed, Vec::<&(&str, &str)>::new());
}

/// The span is matched ignoring case, and the fix keeps the chunk's own
/// text as the part it replaced.
#[test]
fn the_span_is_matched_ignoring_case() {
    let gated = gate(
        "start Amylo Diping daily",
        &[amlodipine()],
        &[propose("AMYLO diping", "amlodipine")],
    );
    let fixes: Vec<_> = gated.applied().map(|f| (f.range(), f.heard())).collect();
    assert_eq!(fixes, [(6..18, "Amylo Diping")]);
}

/// Case is matched by simple lowercase, as Python's: the dotted capital I
/// matches "i", though its full lowercase is two characters.
#[test]
fn a_dotted_capital_i_matches_i() {
    assert_eq!(
        matches("x İbuprofen y", "ibuprofen").collect::<Vec<_>>(),
        vec![2..12]
    );
}

/// A side C1 won't compare is refused, with no score.
#[test]
fn a_replacement_too_long_to_compare_is_refused() {
    let long = "amlodipine ".repeat(20);
    let decision = one(
        "start amylo diping daily",
        &[amlodipine()],
        propose("amylo diping", &long),
    );
    assert_eq!(decision, Decision::rejected(Rejection::TooLong, None));
}

/// A score exactly on the threshold passes: "hyperprofen" against
/// "ibuprofen" is 0.6 to the last bit.
#[test]
fn a_score_on_the_threshold_passes() {
    let gated = gate(
        "take hyperprofen",
        &[offer("hyperprofen", "ibuprofen")],
        &[propose("hyperprofen", "ibuprofen")],
    );
    // Equal to the last bit, so the test is of the comparison, not of a
    // score near it.
    assert_eq!(
        score("hyperprofen", "ibuprofen").value().to_bits(),
        SIMILARITY_MIN.to_bits()
    );
    assert_eq!(gated.text(), "take ibuprofen");
}

/// A score just under the threshold is refused at full precision, with the
/// score kept for the log: 25 of 42 letters a side matched is 0.5952, which
/// the bench's rounded score (0.60) let through.
#[test]
fn a_score_under_the_threshold_is_refused_at_full_precision() {
    let heard = format!("{}{}", "a".repeat(25), "x".repeat(17));
    let replacement = format!("{}{}", "a".repeat(25), "y".repeat(17));
    let decision = one(
        &format!("say {heard} now"),
        &[offer(&heard, &replacement)],
        propose(&heard, &replacement),
    );
    let similarity = score(&heard, &replacement);
    assert!(
        (similarity.value() - 50.0 / 84.0).abs() < 1e-12,
        "{similarity:?}"
    );
    assert_eq!(
        decision,
        Decision::rejected(Rejection::Dissimilar, Some(similarity))
    );
}

/// A replacement holding no offered term is refused, however alike it is.
#[test]
fn a_replacement_with_no_offered_term_is_refused() {
    let decision = one(
        "start amylo diping daily",
        &[amlodipine()],
        propose("amylo diping", "amylodipine"),
    );
    let similarity = score("amylo diping", "amylodipine");
    assert_eq!(
        decision,
        Decision::rejected(Rejection::NotOffered, Some(similarity))
    );
}

/// An offered term inside the replacement doesn't count when its window
/// shares no word with the span: it was offered for other words.
#[test]
fn a_term_offered_for_other_words_is_refused() {
    let decision = one(
        "the amylo diping and amlo dipine",
        &[offer("amlo dipine", "amlodipine")],
        propose("amylo diping", "amlodipine"),
    );
    let similarity = score("amylo diping", "amlodipine");
    assert_eq!(
        decision,
        Decision::rejected(Rejection::WindowElsewhere, Some(similarity))
    );
}

/// A window sharing one word with the span is enough, and the term is
/// found in the replacement's letters whatever its case and spacing.
#[test]
fn one_shared_word_and_the_terms_letters_are_enough() {
    let gated = gate(
        "the amylo diping dose",
        &[offer("diping dose", "Amlo-Dipine")],
        &[propose("amylo diping", "amlo dipine")],
    );
    assert_eq!(gated.text(), "the amlo dipine dose");
}

/// The fix records the first candidate that passes both rules 3 and 4,
/// after one whose window is elsewhere.
#[test]
fn the_fix_names_the_candidate_that_let_it_through() {
    let elsewhere = offer("amlo dipine", "amlodipine");
    let here = offer("amylo", "amlodipine");
    let gated = gate(
        "the amylo diping and amlo dipine",
        &[elsewhere, here.clone()],
        &[propose("amylo diping", "amlodipine")],
    );
    let offered: Vec<_> = gated.applied().map(AppliedFix::offered).collect();
    assert_eq!(offered, [&here]);
}

/// The rules are checked in the bench's order: a span that isn't found is
/// refused for that, though its score would also fail.
#[test]
fn a_missing_span_is_refused_before_its_score() {
    let decision = one(
        "start",
        &[amlodipine()],
        propose("bloody pine", "amlodipine"),
    );
    assert_eq!(decision, Decision::rejected(Rejection::SpanNotFound, None));
}

/// And a score under the threshold is refused for that, though no term was
/// offered either.
#[test]
fn a_low_score_is_refused_before_the_terms() {
    let decision = one("a bloody pine", &[], propose("bloody pine", "beta blocker"));
    let similarity = score("bloody pine", "beta blocker");
    assert_eq!(
        decision,
        Decision::rejected(Rejection::Dissimilar, Some(similarity))
    );
}

/// The same fix proposed twice, for a span the chunk has once, is applied
/// once: the second has nothing left to change.
#[test]
fn a_second_fix_of_the_same_words_is_already_changed() {
    let gated = gate(
        "Inhale twice",
        &[offer("Inhale", "inhaler")],
        &[propose("Inhale", "inhaler"), propose("Inhale", "inhaler")],
    );
    let rejections: Vec<_> = gated.decisions().iter().map(Decision::rejection).collect();
    assert_eq!(rejections, [None, Some(Rejection::AlreadyChanged)]);
    assert_eq!(gated.text(), "inhaler twice");
}

/// The already-changed refusal keeps the score the proposal passed with.
#[test]
fn an_already_changed_refusal_keeps_its_score() {
    let gated = gate(
        "Inhale twice",
        &[offer("Inhale", "inhaler")],
        &[propose("Inhale", "inhaler"), propose("Inhale", "inhaler")],
    );
    assert_eq!(
        gated.decisions()[1],
        Decision::rejected(Rejection::AlreadyChanged, Some(score("Inhale", "inhaler")))
    );
}

/// With the span in the chunk twice, the second proposal fixes the second
/// place.
#[test]
fn a_second_fix_takes_the_next_place_the_span_is() {
    let gated = gate(
        "Inhale, then Inhale",
        &[offer("Inhale", "inhaler")],
        &[propose("Inhale", "inhaler"), propose("Inhale", "inhaler")],
    );
    let ranges: Vec<_> = gated.applied().map(AppliedFix::range).collect();
    assert_eq!(ranges, [0..6, 13..19]);
    assert_eq!(gated.text(), "inhaler, then inhaler");
}

/// A later span that would overlap an earlier fix takes its next place
/// clear of it.
#[test]
fn a_fix_never_overlaps_an_earlier_one() {
    let gated = gate(
        "amylo diping then diping",
        &[amlodipine(), offer("diping", "dipine")],
        &[
            propose("amylo diping", "amlodipine"),
            propose("diping", "dipine"),
        ],
    );
    let ranges: Vec<_> = gated.applied().map(AppliedFix::range).collect();
    assert_eq!(ranges, [0..12, 18..24]);
    assert_eq!(gated.text(), "amlodipine then dipine");
}

/// A fix's replacement is never matched by a later span: each is placed in
/// the chunk as given. In the bench's running text the second "edema" would
/// have been the one inside the first fix.
#[test]
fn a_later_span_is_never_found_inside_an_earlier_fix() {
    let gated = gate(
        "amylo diping and edema",
        &[
            offer("amylo diping", "amlodipine"),
            offer("edema", "oedema"),
        ],
        &[
            propose("amylo diping", "amlodipine edema"),
            propose("edema", "oedema"),
        ],
    );
    assert_eq!(gated.text(), "amlodipine edema and oedema");
}

/// A refused proposal leaves the text as it was, and the revised text is a
/// new value beside it.
#[test]
fn a_refused_proposal_leaves_the_text_as_it_was() {
    let chunk = String::from("start amylo diping daily");
    let gated = gate(&chunk, &[], &[propose("amylo diping", "amlodipine")]);
    assert_eq!(gated.text(), "start amylo diping daily");
    assert_eq!(chunk, "start amylo diping daily");
}

/// Ranges that share a byte overlap; ranges that only touch don't.
#[test]
fn ranges_overlap_only_when_they_share_a_byte() {
    let cases = [
        (0..3, 2..5, true),
        (0..3, 3..5, false),
        (3..5, 0..3, false),
        (1..2, 0..5, true),
    ];
    let got: Vec<_> = cases.iter().map(|(a, b, _)| overlap(a, b)).collect();
    let want: Vec<_> = cases.iter().map(|(.., w)| *w).collect();
    assert_eq!(got, want);
}

/// The words of a text are its runs of letters and digits, lowercased.
#[test]
fn words_are_runs_of_letters_and_digits() {
    let got: Vec<_> = words("Type-2 DIABETES, 500mg!")
        .into_iter()
        .map(|w| String::from_utf8(w).unwrap())
        .collect();
    assert_eq!(got, ["2", "500mg", "diabetes", "type"]);
}
