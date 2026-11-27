//! Property tests for C3, the similarity pre-check, against C2's gate on
//! the gate's own cases.
//!
//! The contract, checked on every input:
//! - a candidate the pre-check skips is one the gate refuses: a yes on it
//!   (its window as the span, its term as the replacement) is never
//!   applied, whatever the chunk, the offered list and the other yeses;
//! - leaving the skipped yeses out changes no other decision, nor the
//!   revised text;
//! - it skips exactly the candidates C1 scores below [`SIMILARITY_MIN`],
//!   and never one a signal marks;
//! - a recording's low band holds its lowest [`LOW_LOG_PROB_PERCENT`],
//!   rounded down.

use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;

use super::*;
use crate::gate::props::{Case, any_case, any_text_case};
use crate::gate::{Proposal, gate};

/// Whether the pre-check, with no signal, skips the candidate behind
/// `proposal`.
fn skipped(proposal: &Proposal) -> bool {
    matches!(
        precheck(&proposal.heard, &proposal.replacement, None),
        PreCheck::Skip(_)
    )
}

/// The problems with skipping on `case`, by the contract above; none if
/// it holds.
fn problems(case: &Case) -> Vec<String> {
    let mut bad = Vec::new();
    let all = gate(&case.chunk, &case.offered, &case.proposals);
    for (proposal, decision) in case.proposals.iter().zip(all.decisions()) {
        if skipped(proposal) && decision.applied().is_some() {
            bad.push(format!("skipped but applied: {proposal:?}"));
        }
    }
    let kept: Vec<Proposal> = case
        .proposals
        .iter()
        .filter(|p| !skipped(p))
        .cloned()
        .collect();
    let without = gate(&case.chunk, &case.offered, &kept);
    let others: Vec<_> = case
        .proposals
        .iter()
        .zip(all.decisions())
        .filter(|(p, _)| !skipped(p))
        .map(|(_, d)| d)
        .collect();
    let left: Vec<_> = without.decisions().iter().collect();
    if others != left {
        bad.push(format!("other decisions changed: {others:?} → {left:?}"));
    }
    if all.text() != without.text() {
        bad.push(format!("text {:?} → {:?}", all.text(), without.text()));
    }
    bad
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// On the vocabulary's cases, skipping keeps the contract.
    #[test]
    fn a_skipped_candidate_is_one_the_gate_refuses(case in any_case()) {
        prop_assert_eq!(problems(&case), Vec::<String>::new());
    }

    /// On cases of any text, too.
    #[test]
    fn a_skipped_candidate_is_one_the_gate_refuses_on_any_text(case in any_text_case()) {
        prop_assert_eq!(problems(&case), Vec::<String>::new());
    }

    /// With no signal it skips just what C1 scores below the minimum; a
    /// signal stops every skip.
    #[test]
    fn it_skips_only_what_scores_below_the_minimum(
        window in ".{0,12}",
        term in ".{0,12}",
    ) {
        let below = letter_similarity(&window, &term)
            .is_ok_and(|s| s.value() < SIMILARITY_MIN);
        let plain = precheck(&window, &term, None);
        prop_assert_eq!(matches!(plain, PreCheck::Skip(_)), below, "{:?}", plain);
        let marked = precheck(&window, &term, Some(Signal::LowLogProb));
        prop_assert!(matches!(marked, PreCheck::Ask(_)), "{:?}", marked);
    }

    /// The band holds the lowest 5% of the known values, rounded down:
    /// fewer than that are below its cut, and at least that many are at or
    /// below it.
    #[test]
    fn the_band_is_the_lowest_share(
        values in prop::collection::vec(-20.0_f32..=0.0, 0..300),
    ) {
        let band = values.len() * LOW_LOG_PROB_PERCENT / 100;
        let Some(low) = LowLogProb::of(values.iter().copied()) else {
            prop_assert_eq!(band, 0);
            return Ok(());
        };
        let below = values.iter().filter(|v| **v < low.cut()).count();
        let within = values.iter().filter(|v| low.marks(**v)).count();
        prop_assert!(below < band, "{} below the cut, band {}", below, band);
        prop_assert!(within >= band, "{} within, band {}", within, band);
    }
}

/// How many of 512 of `strategy`'s cases' proposals the pre-check skips,
/// and how many it keeps that the gate applies.
fn reached(strategy: &impl Strategy<Value = Case>) -> (usize, usize) {
    let mut runner = TestRunner::deterministic();
    let (mut skips, mut applied) = (0, 0);
    for _ in 0..512 {
        let case = strategy.new_tree(&mut runner).unwrap().current();
        let gated = gate(&case.chunk, &case.offered, &case.proposals);
        for (proposal, decision) in case.proposals.iter().zip(gated.decisions()) {
            if skipped(proposal) {
                skips += 1;
            } else if decision.applied().is_some() {
                applied += 1;
            }
        }
    }
    (skips, applied)
}

/// The cases both skip proposals and apply ones the pre-check keeps, so
/// the properties aren't passing on cases that never skip, or never
/// apply what's left.
#[test]
fn the_cases_skip_some_and_apply_others() {
    for strategy in [any_case().boxed(), any_text_case().boxed()] {
        let (skips, applied) = reached(&strategy);
        assert!(skips >= 100, "{skips} skipped"); // check-bound
        assert!(applied >= 30, "{applied} applied"); // check-bound
    }
}
