use std::path::PathBuf;

use nota_core::decide::{Place, Probability};
use nota_core::{LogProb, SampleIndex, SampleRange, SessionId, TrackId};
use nota_store::{
    ChunkKey, DecidedBy, FinalText, FinalWord, Found, HeardBy, NewProposal, NewSession, Origin,
    Proposed, Refusal, RevisionNumber, Store, WordSpan,
};

use super::*;

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-cards-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SESSION: SessionId = SessionId::new(1);
/// How many words the one chunk has.
const WORDS: u32 = 20;

/// The chunk every proposal here is in.
fn chunk() -> ChunkKey {
    ChunkKey {
        track: TrackId::new(0),
        start: SampleIndex::new(0),
    }
}

/// A library whose session has one chunk of [`WORDS`] words.
fn library(name: &str) -> (TestDir, Store) {
    let dir = TestDir::new(name);
    let mut store = Store::open(&dir.0.join("library.db")).unwrap();
    store.create_session(&NewSession::bare(SESSION)).unwrap();
    let range =
        |a: u64, b: u64| SampleRange::new(SampleIndex::new(a), SampleIndex::new(b)).unwrap();
    let text = FinalText {
        track: TrackId::new(0),
        range: range(0, u64::from(WORDS) * 100),
        text: Some("w".repeat(usize::try_from(WORDS).unwrap())),
        words: (0..u64::from(WORDS))
            .map(|i| FinalWord {
                text: format!("w{i}"),
                range: range(i * 100, i * 100 + 100),
                log_prob: None,
            })
            .collect(),
        heard_by: HeardBy {
            engine: "e".to_owned(),
            model: "m".to_owned(),
        },
    };
    store
        .add_final_text(SESSION, TrackId::new(0), text.range.end(), &[text])
        .unwrap();
    (dir, store)
}

/// A yes the gate refused, on word `word`, with these signals.
fn refused(
    word: u32,
    term: &str,
    yes: Option<f64>,
    log_prob: Option<f32>,
    similarity: Option<f64>,
) -> NewProposal {
    let words = WordSpan::new(word, word).unwrap();
    NewProposal {
        proposed: Proposed {
            chunk: chunk(),
            words,
            offered: words,
            heard: format!("w{word}"),
            term: term.to_owned(),
            replacement: term.to_owned(),
            origin: Origin::Learned,
            found: Found::Alias,
            decided_by: DecidedBy {
                name: "fake".to_owned(),
                model: "m".to_owned(),
                place: Place::Local,
            },
            log_prob: log_prob.and_then(LogProb::new),
            thresholds: String::new(),
        },
        decided: Decided::Refused {
            yes: yes.and_then(Probability::new),
            rule: Refusal::Dissimilar,
            similarity,
        },
    }
}

/// An applied fix of words `first` to `last`, reading `text`.
fn applied(first: u32, last: u32, text: &str) -> NewProposal {
    let mut p = refused(first, text, None, None, None);
    p.proposed.words = WordSpan::new(first, last).unwrap();
    p.proposed.replacement = text.to_owned();
    p.decided = Decided::Applied {
        yes: None,
        similarity: 0.9,
    };
    p
}

/// Each to-rank proposal's term, best first.
fn terms(proposals: &[Proposal]) -> Vec<String> {
    ranked(proposals)
        .iter()
        .map(|p| p.proposed.term.clone())
        .collect()
}

/// One card per ten minutes of recording, rounded up.
#[test]
fn a_card_is_allowed_per_ten_minutes_rounded_up() {
    let minutes = |m: u64| Duration::from_secs(m * 60);
    assert_eq!(card_cap(Duration::ZERO), 0);
    assert_eq!(card_cap(Duration::from_secs(1)), 1);
    assert_eq!(card_cap(minutes(10)), 1);
    assert_eq!(card_cap(minutes(10) + Duration::from_nanos(1)), 2);
    assert_eq!(card_cap(minutes(22)), 3);
    assert_eq!(card_cap(minutes(60)), 6);
}

/// The ranking's keys, each deciding between two proposals equal on the
/// keys before it: the yes probability, then the lowest log-probability,
/// then C1's score, then place in the session, then the term.
#[test]
fn signals_rank_in_order_of_their_keys() {
    let (_dir, mut store) = library("rank-keys");
    let proposals = [
        refused(0, "place-later", Some(0.5), Some(-1.0), Some(0.5)),
        refused(5, "no-probability", None, Some(-9.0), Some(0.9)),
        refused(1, "sure-words", Some(0.5), Some(-0.1), Some(0.9)),
        refused(1, "less-similar", Some(0.5), Some(-1.0), Some(0.4)),
        refused(0, "most-likely", Some(0.9), None, None),
        refused(1, "unknown-words", Some(0.5), None, Some(0.9)),
        refused(0, "a-term-first", Some(0.5), Some(-1.0), Some(0.5)),
        refused(1, "unsure-words", Some(0.5), Some(-3.0), Some(0.1)),
    ];
    store.add_proposals(SESSION, &proposals).unwrap();
    assert_eq!(
        terms(&store.proposals(SESSION).unwrap()),
        [
            "most-likely",
            "unsure-words",
            "a-term-first",
            "place-later",
            "less-similar",
            "sure-words",
            "unknown-words",
            "no-probability",
        ]
    );
}

/// Acceptance: the ranking is stable. The same proposals stored in
/// another order rank the same, and so give the same cards.
#[test]
fn the_ranking_is_stable_whatever_the_order() {
    let signals: Vec<NewProposal> = (0..WORDS)
        .map(|w| {
            let p = f64::from(w % 4) / 4.0;
            let log_prob = (w % 3 != 0).then(|| -f32::from(u8::try_from(w % 5).unwrap()));
            refused(w, &format!("t{w}"), Some(p), log_prob, Some(0.5))
        })
        .collect();
    let (_a, mut forwards) = library("stable-forwards");
    forwards.add_proposals(SESSION, &signals).unwrap();
    let mut backwards_signals = signals.clone();
    backwards_signals.reverse();
    let (_b, mut backwards) = library("stable-backwards");
    backwards
        .add_proposals(SESSION, &backwards_signals)
        .unwrap();
    let forwards = forwards.proposals(SESSION).unwrap();
    let backwards = backwards.proposals(SESSION).unwrap();
    assert_eq!(terms(&forwards), terms(&backwards));
    let cards = |proposals: &[Proposal]| -> Vec<String> {
        let placing = placing(proposals, Duration::from_mins(25));
        proposals
            .iter()
            .filter(|p| placing.cards.contains(&p.id))
            .map(|p| p.proposed.term.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    // The likeliest yeses are words 3, 7, 11, 15 and 19; 3 and 15 have no
    // log-probability, and of the rest 19's words are least sure, then 7's,
    // then 11's. 25 minutes allows three cards.
    assert_eq!(cards(&forwards), ["t11", "t19", "t7"]); // check-bound
    assert_eq!(cards(&forwards), cards(&backwards));
}

/// Cards already shown count against the cap: a run places only what's
/// left, and the rest go over it.
#[test]
fn cards_shown_already_count_against_the_cap() {
    let (_dir, mut store) = library("cap-shown");
    store
        .add_proposals(
            SESSION,
            &[
                refused(0, "shown", Some(0.9), None, None),
                refused(1, "next", Some(0.8), None, None),
                refused(2, "last", Some(0.7), None, None),
            ],
        )
        .unwrap();
    let first = store.proposals(SESSION).unwrap();
    let ten_minutes = Duration::from_secs(600);
    let shown = Placing {
        cards: vec![first[0].id],
        ..Placing::default()
    };
    store.place_proposals(SESSION, &shown).unwrap();
    let after = store.proposals(SESSION).unwrap();
    let placing = placing(&after, ten_minutes * 2);
    assert_eq!(placing.cards, [after[1].id]);
    assert_eq!(placing.over_cap, [after[2].id]);
    // With the cap spent, everything left is over it.
    let spent = super::placing(&after, ten_minutes);
    assert_eq!(spent.cards, []);
    assert_eq!(spent.over_cap, [after[1].id, after[2].id]);
}

/// Applied fixes not yet written become changes, one per run of words:
/// two fixes merged into one change carry the same words and text.
#[test]
fn applied_fixes_become_one_change_per_run_of_words() {
    let (_dir, mut store) = library("changes");
    store
        .add_proposals(
            SESSION,
            &[
                applied(2, 2, "follow-on"),
                applied(2, 2, "follow-on"),
                applied(4, 5, "amlodipine"),
            ],
        )
        .unwrap();
    let proposals = store.proposals(SESSION).unwrap();
    let placing = placing(&proposals, Duration::ZERO);
    assert_eq!(
        placing.applied,
        proposals.iter().map(|p| p.id).collect::<Vec<_>>()
    );
    let changes: Vec<(WordSpan, &str)> = placing
        .changes
        .iter()
        .map(|c| (c.words, c.text.as_str()))
        .collect();
    assert_eq!(
        changes,
        [
            (WordSpan::new(2, 2).unwrap(), "follow-on"),
            (WordSpan::new(4, 5).unwrap(), "amlodipine")
        ]
    );
    // Once written, they aren't written again.
    store.place_proposals(SESSION, &placing).unwrap();
    let again = super::placing(&store.proposals(SESSION).unwrap(), Duration::ZERO);
    assert_eq!(again, Placing::default());
    assert_eq!(
        store.revisions(SESSION).unwrap(),
        [RevisionNumber::FINAL, store.revisions(SESSION).unwrap()[1]]
    );
}
