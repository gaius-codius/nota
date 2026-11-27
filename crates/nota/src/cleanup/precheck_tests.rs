//! C3 in the clean-up job: what the similarity pre-check skips, what it
//! still asks, and that the job decides the same with it as without.

use super::*;

/// A chunk whose "amylo edema" the lookup offers as "amlodipine", which C1
/// scores under the gate's minimum.
const EDEMA: &str = "her ankle amylo edema was worse";

/// A chunk with a candidate the gate can apply.
const DIPING: &str = "start amylo diping daily";

/// Words to pad a session with, so it has enough log-probabilities for a
/// low band (a band needs 20).
const FILLER: &str = "we went over the notes again and then talked about \
                      what each of the patients had said in the clinic today";

/// Each proposal's heard span, term and decision.
fn decisions(fixture: &Fixture) -> Vec<(String, String, Decided)> {
    fixture
        .proposals()
        .iter()
        .map(|p| {
            (
                p.proposed.heard.clone(),
                p.proposed.term.clone(),
                p.decided.clone(),
            )
        })
        .collect()
}

/// C1's score on `window` as `term`.
fn c1(window: &str, term: &str) -> f64 {
    nota_checks::letter_similarity(window, term)
        .unwrap()
        .value()
}

/// The log-probability `value`.
fn lp(value: f32) -> Option<LogProb> {
    LogProb::new(value)
}

/// With the pre-check on, a candidate C1 scores under the minimum isn't
/// asked about: no question is put, and it's rejected and logged with
/// its score, so it's no card.
#[test]
fn a_dissimilar_candidate_is_rejected_without_a_question() {
    let fixture = Fixture::new("c3-skip", &[EDEMA], &["amlodipine"]);
    let fake = Fake::saying_yes([("amylo edema", "amlodipine")]);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(fake.clone()), Precheck::On);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    // Nothing else in the chunk is offered, so the chunk is no question.
    assert_eq!(fake.questions(), 0);
    let similarity = c1("amylo edema", "amlodipine");
    assert_eq!(
        decisions(&fixture),
        [(
            "amylo edema".to_owned(),
            "amlodipine".to_owned(),
            Decided::Skipped { similarity }
        )]
    );
    assert_eq!(fixture.proposals()[0].outcome, Outcome::Rejected);
    assert_eq!(fixture.revisions(), [RevisionNumber::FINAL]);
}

/// With it off, the same yes is asked and refused by the gate, and
/// becomes a card: the difference the pre-check makes.
#[test]
fn with_the_pre_check_off_the_same_yes_is_a_card() {
    let fixture = Fixture::new("c3-off", &[EDEMA], &["amlodipine"]);
    let fake = Fake::saying_yes([("amylo edema", "amlodipine")]);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(fake.clone()), Precheck::Off);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fake.questions(), 1);
    let proposals = fixture.proposals();
    let [p] = &proposals[..] else {
        panic!("{proposals:?}")
    };
    assert_eq!(p.outcome, Outcome::Card);
    assert!(
        matches!(
            p.decided,
            Decided::Refused {
                rule: Refusal::Dissimilar,
                ..
            }
        ),
        "{p:?}"
    );
}

/// A low log-probability on the span marks it, so the candidate is still
/// asked about, and a yes on it still makes a card.
#[test]
fn a_candidate_a_low_log_probability_marks_is_asked() {
    let chunks = [FILLER, EDEMA];
    // "amylo" is the one word the recogniser was unsure of.
    let heard = |w: &str| if w == "amylo" { lp(-6.0) } else { lp(-0.01) };
    let fixture = Fixture::heard("c3-marked", &chunks, &["amlodipine"], &heard);
    let fake = Fake::saying_yes([("amylo edema", "amlodipine")]);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(fake.clone()), Precheck::On);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fake.questions(), 1);
    let proposals = fixture.proposals();
    let [p] = &proposals[..] else {
        panic!("{proposals:?}")
    };
    assert_eq!(p.outcome, Outcome::Card);
}

/// A log-probability outside the low band marks nothing: the candidate is
/// skipped as if there were none.
#[test]
fn a_log_probability_outside_the_band_marks_nothing() {
    let chunks = [FILLER, EDEMA];
    // Every word is as sure as the rest, except two filler words that take
    // the band's places.
    let heard = |w: &str| match w {
        "clinic" | "patients" => lp(-6.0),
        _ => lp(-0.01),
    };
    let fixture = Fixture::heard("c3-unmarked", &chunks, &["amlodipine"], &heard);
    let fake = Fake::saying_yes([("amylo edema", "amlodipine")]);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(fake.clone()), Precheck::On);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fake.questions(), 0);
    assert!(
        matches!(fixture.proposals()[0].decided, Decided::Skipped { .. }),
        "{:?}",
        fixture.proposals()
    );
}

/// In a chunk with both kinds, only the candidate that may be applied is
/// put in the question; both are stored, the skipped one rejected.
#[test]
fn only_the_candidates_left_are_put_in_the_question() {
    let chunk = "start amylo diping daily for her ankle amylo edema";
    let fixture = Fixture::new("c3-both", &[chunk], &["amlodipine"]);
    let fake = Fake::saying_yes([
        ("amylo diping", "amlodipine"),
        ("amylo edema", "amlodipine"),
    ]);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(fake.clone()), Precheck::On);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(
        *fake.log.candidates.lock().unwrap(),
        [vec!["amylo diping -> amlodipine".to_owned()]]
    );
    assert_eq!(
        fixture.shown(),
        ["start amlodipine daily for her ankle amylo edema"]
    );
    let outcomes: Vec<(String, Outcome)> = fixture
        .proposals()
        .iter()
        .map(|p| (p.proposed.heard.clone(), p.outcome))
        .collect();
    assert!(
        matches!(
            &outcomes[..],
            [(a, Outcome::Applied(Some(_))), (b, Outcome::Rejected)]
                if a == "amylo diping" && b == "amylo edema"
        ),
        "{outcomes:?}"
    );
}

/// A skipped candidate is stored like any other proposal, so a later run
/// doesn't ask about it, even with the pre-check off.
#[test]
fn a_skipped_candidate_isnt_asked_again() {
    let fixture = Fixture::new("c3-again", &[EDEMA], &["amlodipine"]);
    let fake = Fake::saying_yes([("amylo edema", "amlodipine")]);
    let mut on = fixture.clean_up_with(Gives::Fake(fake.clone()), Precheck::On);
    assert_eq!(run(&mut on, &fixture), JobEnd::Done);
    let mut off = fixture.clean_up_with(Gives::Fake(fake.clone()), Precheck::Off);
    assert_eq!(run(&mut off, &fixture), JobEnd::Done);
    assert_eq!(fake.questions(), 0);
    assert_eq!(fixture.proposals().len(), 1);
}

/// Each proposal records that the pre-check ran, and its low band's share.
#[test]
fn proposals_record_that_the_pre_check_ran() {
    let fixture = Fixture::new("c3-thresholds", &[DIPING], &["amlodipine"]);
    let fake = Fake::saying_yes([("amylo diping", "amlodipine")]);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(fake), Precheck::On);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(
        fixture.proposals()[0].proposed.thresholds,
        "similarity_min=0.6;keep=0.58;keep_rare=0.42;max_candidates=6;card_every_s=600;\
         precheck=on;low_log_prob_percent=5"
    );
}

/// The invented session the with-and-without comparison runs on. With
/// "oedema" in the pack the lookup offers "amylo" and "edema" apart:
/// "amylo" as "amlodipine" scores under the minimum, the rest over it.
const SESSION_CHUNKS: [&str; 5] = [
    "start amylo diping daily",
    "her ankle amylo edema was worse",
    "start amylo diping daily for her ankle amylo edema",
    "take hyper profen with food",
    "we went over the notes again",
];

/// The terms the comparison's pack offers.
const SESSION_TERMS: [&str; 3] = ["amlodipine", "ibuprofen", "oedema"];

/// Runs the comparison session once with `precheck`, a fake saying yes
/// to every candidate but the third chunk's "amylo", and gives its
/// proposals by chunk and candidate, and the text it ends with.
fn compared(name: &str, precheck: Precheck) -> (BTreeMap<(String, String), Proposal>, Vec<String>) {
    let fixture = Fixture::new(name, &SESSION_CHUNKS, &SESSION_TERMS);
    let [diping, edema, both, profen, _] = SESSION_CHUNKS;
    let yes = [
        (diping, "amylo diping", "amlodipine"),
        (edema, "amylo", "amlodipine"),
        (edema, "edema", "oedema"),
        (both, "amylo diping", "amlodipine"),
        (both, "edema", "oedema"),
        (profen, "profen", "ibuprofen"),
    ];
    let fake = Fake::saying_yes_in(yes);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(fake), precheck);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    let by_candidate = fixture
        .proposals()
        .into_iter()
        .map(|p| {
            let key = (
                format!("{:?} {}", p.proposed.chunk, p.proposed.heard),
                p.proposed.term.clone(),
            );
            (key, p)
        })
        .collect();
    (by_candidate, fixture.shown())
}

/// Acceptance (#160): on the invented session, the job applies the same
/// fixes and rejects the same candidates with the pre-check as without.
/// What differs is only which candidates were asked: a skip stands where
/// the decider said no (rejected either way), or where it said yes and
/// the gate refused for C1's score, a card or over the cap without the
/// pre-check. Each is counted.
#[test]
fn the_job_decides_the_same_with_the_pre_check_as_without() {
    let (off, off_text) = compared("c3-compare-off", Precheck::Off);
    let (on, on_text) = compared("c3-compare-on", Precheck::On);
    assert_eq!(on_text, off_text);
    assert_eq!(
        on.keys().collect::<Vec<_>>(),
        off.keys().collect::<Vec<_>>()
    );
    let applied = |p: &Proposal| matches!(p.outcome, Outcome::Applied(_));
    let (mut unasked_noes, mut lost_cards, mut moved) = (Vec::new(), Vec::new(), Vec::new());
    for (key, without) in &off {
        let with = &on[key];
        assert_eq!(applied(with), applied(without), "{key:?}");
        if with.decided == without.decided && with.outcome == without.outcome {
            continue;
        }
        // The room a skip leaves under the cap, taken by another refused yes.
        if with.decided == without.decided {
            moved.push((key.1.clone(), without.outcome, with.outcome));
            continue;
        }
        assert!(
            matches!(with.decided, Decided::Skipped { .. }),
            "{key:?}: {without:?} → {with:?}"
        );
        match without.decided {
            Decided::No { .. } => unasked_noes.push(key.1.clone()),
            Decided::Refused {
                rule: Refusal::Dissimilar,
                ..
            } => lost_cards.push((key.1.clone(), without.outcome)),
            _ => panic!("{key:?}: {without:?} → {with:?}"),
        }
    }
    // The third chunk's "amylo", which C1 already ruled out.
    assert_eq!(unasked_noes, ["amlodipine"]);
    // The second chunk's "amylo": a yes the gate refused, a card without
    // the pre-check and not with it.
    assert_eq!(lost_cards, [("amlodipine".to_owned(), Outcome::Card)]);
    // No other refused yes here to take its place.
    assert_eq!(moved, []);
    assert_eq!(
        off_text,
        [
            "start amlodipine daily",
            "her ankle amylo oedema was worse",
            "start amlodipine daily for her ankle amylo oedema",
            "take hyper ibuprofen with food",
            "we went over the notes again",
        ]
    );
}

/// Acceptance (#160): the calls the pre-check saved are counted per
/// session, from its proposals: a chunk whose every candidate was skipped
/// is a question saved; a chunk asked about fewer candidates isn't.
#[test]
fn the_calls_saved_are_counted_per_session() {
    let chunks = [EDEMA, "start amylo diping daily for her ankle amylo edema"];
    let fixture = Fixture::new("c3-saved", &chunks, &["amlodipine"]);
    let fake = Fake::saying_yes([]);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(fake.clone()), Precheck::On);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fake.questions(), 1);
    assert_eq!(
        saved(&fixture.proposals()),
        Saved {
            candidates: 2,
            questions: 1
        }
    );
}

/// With the pre-check off, nothing is saved.
#[test]
fn with_the_pre_check_off_nothing_is_saved() {
    let fixture = Fixture::new("c3-saved-off", &[EDEMA], &["amlodipine"]);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(Fake::saying_yes([])), Precheck::Off);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(saved(&fixture.proposals()), Saved::default());
}

/// A chunk with a candidate C3 skips and one it asks about.
const MIXED: &str = "start amylo diping daily for her ankle amylo edema";

/// A chunk's skips are stored with its answer: while the decider is down
/// for its question, nothing of the chunk is stored, so a later run asks
/// again; once it's given up on, its skips are stored with the give-up.
#[test]
fn a_chunks_skips_are_stored_only_with_its_answer() {
    let chunks = [DIPING, MIXED];
    let fixture = Fixture::new("c3-down", &chunks, &["amlodipine"]);
    let fake = Fake::saying_yes([("amylo diping", "amlodipine")])
        .with_trouble_in(MIXED, Trouble::Unavailable);
    let mut clean_up = fixture.clean_up_with(Gives::Fake(fake), Precheck::On);
    let down = JobEnd::EngineDown("the decider isn't working: no server".to_owned());
    let mixed = SampleIndex::new(CHUNK);
    let in_mixed = |fixture: &Fixture| -> Vec<(String, Decided)> {
        fixture
            .proposals()
            .into_iter()
            .filter(|p| p.proposed.chunk.start == mixed)
            .map(|p| (p.proposed.heard, p.decided))
            .collect()
    };
    for _ in 0..2 {
        assert_eq!(run(&mut clean_up, &fixture), down);
        assert_eq!(in_mixed(&fixture), []);
    }
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    let similarity = c1("amylo edema", "amlodipine");
    assert_eq!(
        in_mixed(&fixture),
        [
            (
                "amylo diping".to_owned(),
                Decided::GaveUp("the decider isn't working: no server".to_owned())
            ),
            ("amylo edema".to_owned(), Decided::Skipped { similarity }),
        ]
    );
}

/// An offer too long to go in a question is asked about, not skipped, so
/// the question fails as it would with C3 off and C3 changes no other
/// answer in the chunk.
#[test]
fn an_offer_too_long_for_a_question_is_not_skipped() {
    let key = ChunkKey {
        track: TrackId::new(0),
        start: SampleIndex::new(0),
    };
    let chunk = Chunk::new(key, EDEMA.split(' ').map(|w| (w, None))).unwrap();
    let offer = |window: &str, term: String| {
        let start = EDEMA.find(window).unwrap();
        let span = start..start + window.len();
        Offer {
            window: window.to_owned(),
            words: chunk.words(&span).unwrap(),
            span,
            term,
            found: nota_store::Found::Alias,
            origin: Origin::Learned,
        }
    };
    // 101 characters, 202 bytes: C1 scores it, a question can't hold it.
    let long = "é".repeat(101);
    assert!(c1("ankle", &long) < SIMILARITY_MIN);
    let screen = Screen::new(Precheck::On, &[]);
    let (asked, skipped) = screen.sort(
        &chunk,
        vec![
            offer("ankle", long.clone()),
            offer("amylo edema", "amlodipine".to_owned()),
        ],
    );
    let asked: Vec<&str> = asked.iter().map(|o| o.term.as_str()).collect();
    let skipped: Vec<&str> = skipped.iter().map(|(o, _)| o.term.as_str()).collect();
    assert_eq!((asked, skipped), (vec![long.as_str()], vec!["amlodipine"]));
}

/// The room under the card cap a skipped yes leaves goes to the next
/// refused yes: the spec's second way cards differ. A short session has
/// room for one card. Without the pre-check the likelier of two refused
/// yeses is the card and the other over the cap; with it the likelier one
/// is skipped, and the other, which a low log-probability marks, is the
/// card.
#[test]
fn the_room_a_skipped_yes_leaves_under_the_cap_goes_to_the_next() {
    let chunks = [FILLER, EDEMA, "his ankle amilo edema was worse"];
    let heard = |w: &str| if w == "amylo" { lp(-6.0) } else { lp(-0.01) };
    let outcomes = |name: &str, precheck: Precheck| -> Vec<(String, Outcome)> {
        let fixture = Fixture::heard(name, &chunks, &["amlodipine"], &heard);
        let fake = Fake::with_probabilities([
            ("amylo edema", "amlodipine", Some(0.6)),
            ("amilo edema", "amlodipine", Some(0.9)),
        ]);
        let mut clean_up = fixture.clean_up_with(Gives::Fake(fake), precheck);
        assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
        fixture
            .proposals()
            .into_iter()
            .map(|p| (p.proposed.heard, p.outcome))
            .collect()
    };
    let marked = "amylo edema".to_owned();
    let likelier = "amilo edema".to_owned();
    assert_eq!(
        outcomes("c3-cap-off", Precheck::Off),
        [
            (marked.clone(), Outcome::OverCap),
            (likelier.clone(), Outcome::Card)
        ]
    );
    assert_eq!(
        outcomes("c3-cap-on", Precheck::On),
        [(marked, Outcome::Card), (likelier, Outcome::Rejected)]
    );
}
