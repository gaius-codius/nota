//! The end of a clean-up run: which yeses the gate refused become "to
//! check" cards, and the change each applied fix makes.
//!
//! **The cap.** A session shows at most one card per [`CARD_EVERY`] of
//! recording, rounded up, so any recording with a signal can show one.
//! Cards already shown count against it, so a run on a session that has
//! them adds only what's left.
//!
//! **The ranking**, best first, is a total order, so the same proposals
//! always rank the same way whatever order they're read in: the decider's
//! yes probability (highest first, none last); then the lowest
//! log-probability among the heard words (lowest first, none last): the
//! recogniser was least sure of them; then C1's score (highest first);
//! then place in the session (track, chunk, first word); then the term;
//! then the proposal's number.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::time::Duration;

use nota_core::decide::Probability;
use nota_store::{Change, Decided, Outcome, Placing, Proposal};

#[cfg(test)]
mod tests;

/// How much recording earns a session one card: ten minutes.
pub(crate) const CARD_EVERY: Duration = Duration::from_secs(600); // check-bound

/// The most cards a recording `recorded` long shows: one per
/// [`CARD_EVERY`], rounded up.
pub(crate) fn card_cap(recorded: Duration) -> usize {
    let every = CARD_EVERY.as_nanos();
    let cards = recorded.as_nanos().div_ceil(every);
    usize::try_from(cards).unwrap_or(usize::MAX)
}

/// The decider's yes probability and C1's score that a refused yes is
/// ranked by; none for any other proposal.
fn signals(proposal: &Proposal) -> (Option<f64>, Option<f64>) {
    match &proposal.decided {
        Decided::Refused {
            yes, similarity, ..
        } => (yes.map(Probability::get), *similarity),
        _ => (None, None),
    }
}

/// `a` against `b` where higher is better and none is worst: the better
/// one first.
fn higher_first(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// `a` against `b` in card order (the module's docs).
fn rank(a: &Proposal, b: &Proposal) -> Ordering {
    let (a_yes, a_similarity) = signals(a);
    let (b_yes, b_similarity) = signals(b);
    let lowest = |p: &Proposal| p.proposed.log_prob.map(|l| -f64::from(l.get()));
    let place = |p: &Proposal| (p.proposed.chunk, p.proposed.offered);
    higher_first(a_yes, b_yes)
        .then_with(|| higher_first(lowest(a), lowest(b)))
        .then_with(|| higher_first(a_similarity, b_similarity))
        .then_with(|| place(a).cmp(&place(b)))
        .then_with(|| a.proposed.term.cmp(&b.proposed.term))
        .then_with(|| a.id.cmp(&b.id))
}

/// The proposals still to rank, best first.
pub(crate) fn ranked(proposals: &[Proposal]) -> Vec<&Proposal> {
    let mut to_rank: Vec<&Proposal> = proposals
        .iter()
        .filter(|p| p.outcome == Outcome::ToRank)
        .collect();
    to_rank.sort_by(|a, b| rank(a, b));
    to_rank
}

/// What the end of a run writes for a session with `proposals` and
/// `recorded` of recording: its cards up to the cap, the rest over it,
/// and the change each applied fix not yet written makes (fixes merged
/// into one change carry the same words and text, so each is written
/// once).
pub(crate) fn placing(proposals: &[Proposal], recorded: Duration) -> Placing {
    let shown = proposals
        .iter()
        .filter(|p| p.outcome == Outcome::Card)
        .count();
    let room = card_cap(recorded).saturating_sub(shown);
    let ranked = ranked(proposals);
    let (cards, over) = ranked.split_at(room.min(ranked.len()));
    let mut seen = BTreeSet::new();
    let mut placing = Placing {
        cards: cards.iter().map(|p| p.id).collect(),
        over_cap: over.iter().map(|p| p.id).collect(),
        ..Placing::default()
    };
    for p in proposals
        .iter()
        .filter(|p| p.outcome == Outcome::Applied(None))
    {
        placing.applied.push(p.id);
        let key = (p.proposed.chunk, p.proposed.words);
        if seen.insert(key) {
            placing.changes.push(Change {
                chunk: p.proposed.chunk,
                words: p.proposed.words,
                text: p.proposed.replacement.clone(),
            });
        }
    }
    placing
}
