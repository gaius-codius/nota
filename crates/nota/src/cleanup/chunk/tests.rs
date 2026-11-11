use nota_core::decide::{Answer, Place};
use nota_core::{SampleIndex, TrackId};

use super::*;

/// The one chunk these tests read.
fn key() -> ChunkKey {
    ChunkKey {
        track: TrackId::new(0),
        start: SampleIndex::new(0),
    }
}

/// The chunk of `text`'s words (split at spaces), with log-probabilities
/// `log_probs` where given.
fn chunk(text: &str, log_probs: &[Option<f32>]) -> Chunk {
    let words = text.split(' ').enumerate().map(|(i, w)| {
        (
            w,
            log_probs.get(i).copied().flatten().and_then(LogProb::new),
        )
    });
    Chunk::new(key(), words).unwrap()
}

/// The offer of `term` for the first `window` in `chunk` from byte `from`.
fn offer(chunk: &Chunk, window: &str, from: usize, term: &str) -> Offer {
    let start = from + chunk.text()[from..].find(window).unwrap();
    let span = start..start + window.len();
    Offer {
        window: window.to_owned(),
        words: chunk.words(&span).unwrap(),
        span,
        term: term.to_owned(),
        found: Found::Alias,
        origin: Origin::Learned,
    }
}

fn context() -> Context {
    Context {
        decided_by: DecidedBy {
            name: "fake".to_owned(),
            model: "m".to_owned(),
            place: Place::Local,
        },
        thresholds: "t".to_owned(),
    }
}

fn yes() -> Said {
    Said {
        answer: Answer::Yes,
        yes: None,
    }
}

fn no() -> Said {
    Said {
        answer: Answer::No,
        yes: None,
    }
}

fn span(first: u32, last: u32) -> WordSpan {
    WordSpan::new(first, last).unwrap()
}

/// Each proposal's words, replacement and decision kind.
fn summary(proposals: &[NewProposal]) -> Vec<(WordSpan, &str, &'static str)> {
    proposals
        .iter()
        .map(|p| {
            let kind = match p.decided {
                Decided::Applied { .. } => "applied",
                Decided::Unchanged { .. } => "unchanged",
                Decided::Covered { .. } => "covered",
                Decided::Refused { .. } => "refused",
                Decided::No { .. } => "no",
                Decided::NoAnswer(_) => "no answer",
            };
            (p.proposed.words, p.proposed.replacement.as_str(), kind)
        })
        .collect()
}

/// A chunk is its words joined by single spaces; a range names the whole
/// words it touches, even part of one, and none in the space between.
#[test]
fn a_range_names_the_words_it_touches() {
    let c = chunk("a follow-up plan", &[]);
    assert_eq!(c.text(), "a follow-up plan");
    assert_eq!(c.words(&(9..11)), Some(span(1, 1)), "up in follow-up");
    assert_eq!(c.words(&(0..6)), Some(span(0, 1)));
    assert_eq!(c.words(&(1..2)), None, "the space after a");
    assert_eq!(c.bytes(span(1, 2)), 2..16);
    assert!(Chunk::new(key(), std::iter::empty()).is_none());
}

/// A run of words' log-probability is its least sure word's, of those
/// known.
#[test]
fn a_run_of_words_is_as_sure_as_its_least_sure_word() {
    let c = chunk("a b c d", &[Some(-0.5), None, Some(-2.0), Some(-0.1)]);
    let lowest = |s| c.lowest_log_prob(s).map(LogProb::get);
    assert_eq!(lowest(span(0, 3)), Some(-2.0));
    assert_eq!(lowest(span(0, 1)), Some(-0.5));
    assert_eq!(lowest(span(1, 1)), None);
}

/// A yes the gate places is applied, and its change is the whole words it
/// touches, keeping the rest of each.
#[test]
fn a_placed_fix_changes_whole_words() {
    let c = chunk("start amylo diping, daily", &[]);
    let offers = [offer(&c, "amylo diping", 0, "amlodipine")];
    let settled = settle(
        &c,
        &offers,
        &offers,
        &Answers::Given(vec![yes()]),
        &[],
        &context(),
    );
    assert_eq!(summary(&settled), [(span(1, 2), "amlodipine,", "applied")]);
    assert_eq!(settled[0].proposed.offered, span(1, 2));
}

/// Two fixes the gate places in one word become one change, which both
/// proposals name.
#[test]
fn two_fixes_in_one_word_are_one_change() {
    let c = chunk("a fallow-upp plan", &[]);
    let offers = [offer(&c, "fallow", 0, "follow"), offer(&c, "upp", 0, "up")];
    let answers = Answers::Given(vec![yes(), yes()]);
    let settled = settle(&c, &offers, &offers, &answers, &[], &context());
    assert_eq!(
        summary(&settled),
        [
            (span(1, 1), "follow-up", "applied"),
            (span(1, 1), "follow-up", "applied")
        ]
    );
}

/// The gate places a yes on the first free occurrence of its heard span,
/// which can be earlier than the one the lookup offered: the outcome names
/// where it landed, and the proposal keeps where it was offered.
#[test]
fn a_fix_can_land_before_where_it_was_offered() {
    let c = chunk("dose then dose", &[]);
    let offers = [offer(&c, "dose", 5, "doses")];
    let settled = settle(
        &c,
        &offers,
        &offers,
        &Answers::Given(vec![yes()]),
        &[],
        &context(),
    );
    assert_eq!(summary(&settled), [(span(0, 0), "doses", "applied")]);
    assert_eq!(settled[0].proposed.offered, span(2, 2));
}

/// A fix that lands on words a review or an earlier fix holds is
/// rejected as covered, and changes nothing.
#[test]
fn a_fix_on_held_words_is_covered() {
    let c = chunk("dose then dose", &[]);
    let offers = [offer(&c, "dose", 5, "doses")];
    let settled = settle(
        &c,
        &offers,
        &offers,
        &Answers::Given(vec![yes()]),
        &[span(0, 0)],
        &context(),
    );
    assert_eq!(summary(&settled), [(span(2, 2), "doses", "covered")]);
}

/// A yes whose replacement is the text that's there changes nothing: it's
/// rejected and logged.
#[test]
fn a_fix_that_changes_nothing_is_rejected() {
    let c = chunk("take Inhale now", &[]);
    let offers = [offer(&c, "Inhale", 0, "Inhale")];
    let settled = settle(
        &c,
        &offers,
        &offers,
        &Answers::Given(vec![yes()]),
        &[],
        &context(),
    );
    assert_eq!(summary(&settled), [(span(1, 1), "Inhale", "unchanged")]);
}

/// A yes the gate refuses keeps the rule and score; a no and a missing
/// answer are rejected with what was said.
#[test]
fn refusals_noes_and_missing_answers_keep_why() {
    let c = chunk("patients with chronic conditions here", &[]);
    let offers = [
        offer(&c, "chronic conditions", 0, "chronic kidney disease"),
        offer(&c, "patients", 0, "patience"),
    ];
    let answers = Answers::Given(vec![yes(), no()]);
    let settled = settle(&c, &offers, &offers, &answers, &[], &context());
    assert_eq!(
        summary(&settled),
        [
            (span(2, 3), "chronic kidney disease", "refused"),
            (span(0, 0), "patience", "no")
        ]
    );
    assert!(matches!(
        settled[0].decided,
        Decided::Refused {
            rule: Refusal::Dissimilar,
            similarity: Some(s),
            ..
        } if s < 0.6
    ));
    let none = settle(
        &c,
        &offers,
        &offers,
        &Answers::None("cut short".to_owned()),
        &[],
        &context(),
    );
    assert_eq!(
        none.iter().map(|p| &p.decided).collect::<Vec<_>>(),
        [
            &Decided::NoAnswer("cut short".to_owned()),
            &Decided::NoAnswer("cut short".to_owned())
        ]
    );
    // A list of answers that doesn't match the candidates is no answer.
    let short = settle(
        &c,
        &offers,
        &offers,
        &Answers::Given(vec![yes()]),
        &[],
        &context(),
    );
    assert_eq!(
        short[1].decided,
        Decided::NoAnswer("1 answers for 2 candidates".to_owned())
    );
}

/// The gate checks a yes against everything offered in the chunk, not
/// only what was asked: a candidate asked again on a later run still has
/// its window offered.
#[test]
fn a_yes_is_gated_against_every_offer_in_the_chunk() {
    let c = chunk("amylo diping daily", &[]);
    let offered = [
        offer(&c, "amylo diping", 0, "amlodipine"),
        offer(&c, "daily", 0, "DailyMed"),
    ];
    // Asked about a proposal whose term the asked list doesn't hold.
    let asked = [Offer {
        term: "amlodipine".to_owned(),
        ..offered[1].clone()
    }];
    let settled = settle(
        &c,
        &offered,
        &asked,
        &Answers::Given(vec![yes()]),
        &[],
        &context(),
    );
    assert!(
        matches!(
            settled[0].decided,
            Decided::Refused {
                rule: Refusal::WindowElsewhere | Refusal::Dissimilar,
                ..
            }
        ),
        "{settled:?}"
    );
}

#[path = "props.rs"]
mod props;
