//! Properties of settling a chunk, over random chunks, offers, decider
//! answers and the real gate's verdicts on them. The offers' windows are
//! the chunk's own words and their terms near misses of them or not, so
//! the gate places some yeses and refuses others as dissimilar, placed
//! elsewhere or already changed; its other rules (an empty, missing or
//! over-long span, a term not offered) can't arise from what clean-up
//! offers, and the gate's own properties cover them:
//!
//! - every asked candidate ends in exactly one outcome, in order, naming
//!   its heard span, term and offered words;
//! - no answer is never applied, and neither is a no;
//! - only a yes is applied, refused or covered;
//! - an applied fix changes the text, names words no review holds, and
//!   reads with its term in place;
//! - the changes of one chunk never share a word, so they go in one
//!   revision.

use std::collections::BTreeMap;

use nota_core::decide::Probability;
use proptest::prelude::*;

use super::*;

/// The words chunks are made of: terms' near misses, hyphenated words,
/// and everyday words.
const WORDS: [&str; 10] = [
    "amylo",
    "diping",
    "dose",
    "then",
    "fallow-upp",
    "clinic",
    "the",
    "semaglotide",
    "doses",
    "plan",
];

/// The terms offered.
const TERMS: [&str; 6] = [
    "amlodipine",
    "doses",
    "follow",
    "semaglutide",
    "clinician",
    "plan",
];

/// A chunk of one to twelve words.
fn any_chunk() -> impl Strategy<Value = Chunk> {
    prop::collection::vec(prop::sample::select(&WORDS[..]), 1..12)
        .prop_map(|words| Chunk::new(key(), words.into_iter().map(|w| (w, None))).unwrap())
}

/// Offers on `chunk`: windows of one to three of its words, each with a
/// term.
fn any_offers(chunk: Chunk) -> impl Strategy<Value = Vec<Offer>> {
    let n = chunk.spans.len();
    prop::collection::vec((0..n, 1..=3_usize, prop::sample::select(&TERMS[..])), 0..8).prop_map(
        move |picks| {
            picks
                .into_iter()
                .map(|(first, len, term)| {
                    let last = (first + len - 1).min(chunk.spans.len() - 1);
                    let words = span(position(first), position(last));
                    let bytes = chunk.bytes(words);
                    Offer {
                        window: chunk.text()[bytes.clone()].to_owned(),
                        span: bytes,
                        words,
                        term: term.to_owned(),
                        found: Found::Alias,
                        origin: Origin::Learned,
                    }
                })
                .collect()
        },
    )
}

/// A decider's answers on `n` candidates: none at all, or a yes or no
/// each, with a probability or not.
fn any_answers(n: usize) -> impl Strategy<Value = Answers> {
    let said = (any::<bool>(), prop::option::of(0.0..=1.0_f64)).prop_map(|(yes, p)| Said {
        answer: if yes { Answer::Yes } else { Answer::No },
        yes: p.and_then(Probability::new),
    });
    prop_oneof![
        1 => Just(Answers::None("no answer".to_owned())),
        6 => prop::collection::vec(said, n).prop_map(Answers::Given),
    ]
}

/// A chunk, its offers, the ones asked (a subset, in order), answers on
/// those, and words held by reviews.
fn any_case() -> impl Strategy<Value = (Chunk, Vec<Offer>, Vec<Offer>, Answers, Vec<WordSpan>)> {
    any_chunk()
        .prop_flat_map(|chunk| {
            let offers = any_offers(chunk.clone());
            (Just(chunk), offers)
        })
        .prop_flat_map(|(chunk, offers)| {
            let n = offers.len();
            let held = chunk.spans.len();
            (
                Just(chunk),
                Just(offers),
                prop::collection::vec(any::<bool>(), n),
                prop::collection::vec((0..held, 0..2_usize), 0..3),
            )
        })
        .prop_flat_map(|(chunk, offers, keep, held)| {
            let asked: Vec<Offer> = offers
                .iter()
                .zip(&keep)
                .filter(|(_, k)| **k)
                .map(|(o, _)| o.clone())
                .collect();
            let last = chunk.spans.len() - 1;
            let blocked: Vec<WordSpan> = held
                .into_iter()
                .map(|(first, len)| span(position(first), position((first + len).min(last))))
                .collect();
            let answers = any_answers(asked.len());
            (
                Just(chunk),
                Just(offers),
                Just(asked),
                answers,
                Just(blocked),
            )
        })
}

proptest! {
    #[test]
    fn every_candidate_ends_in_exactly_one_outcome(
        (chunk, offered, asked, answers, blocked) in any_case()
    ) {
        let settled = settle(&chunk, &offered, &asked, &answers, &blocked, &context());
        prop_assert_eq!(settled.len(), asked.len());
        let said = per_candidate(&answers, asked.len());
        let mut changes: BTreeMap<WordSpan, &str> = BTreeMap::new();
        for ((p, offer), said) in settled.iter().zip(&asked).zip(&said) {
            prop_assert_eq!(&p.proposed.heard, &offer.window);
            prop_assert_eq!(&p.proposed.term, &offer.term);
            prop_assert_eq!(p.proposed.offered, offer.words);
            let yes = matches!(said, Ok(Said { answer: Answer::Yes, .. }));
            match (&p.decided, said) {
                (Decided::NoAnswer(_), Err(_)) => {}
                (Decided::NoAnswer(_), Ok(_)) | (_, Err(_)) => {
                    prop_assert!(false, "answer {:?} decided {:?}", said, p.decided);
                }
                (Decided::No { .. }, _) => prop_assert!(!yes),
                (
                    Decided::Applied { .. }
                    | Decided::Unchanged { .. }
                    | Decided::Covered { .. }
                    | Decided::Refused { .. },
                    _,
                ) => prop_assert!(yes, "{:?}", p.decided),
            }
            if matches!(p.decided, Decided::Applied { .. }) {
                let words = p.proposed.words;
                prop_assert!(blocked.iter().all(|b| !b.overlaps(words)));
                let was = &chunk.text()[chunk.bytes(words)];
                prop_assert_ne!(was, p.proposed.replacement.as_str());
                prop_assert!(p.proposed.replacement.contains(offer.term.as_str()));
                // Fixes merged into one change agree on its text.
                let text = changes.entry(words).or_insert(&p.proposed.replacement);
                prop_assert_eq!(*text, p.proposed.replacement.as_str());
            } else {
                prop_assert_eq!(p.proposed.words, offer.words);
            }
        }
        let spans: Vec<WordSpan> = changes.into_keys().collect();
        for pair in spans.windows(2) {
            prop_assert!(!pair[0].overlaps(pair[1]), "{:?}", spans);
        }
    }
}
