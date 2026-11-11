use std::path::PathBuf;
use std::sync::atomic::Ordering;

use nota_core::{SampleRange, WallTime};
use nota_store::{
    Decided, FinalText, FinalWord, HeardBy, JobId, NewSession, Outcome, Refusal, Reviewed, Verdict,
};

use super::fake::{self, Fake, FakeSources, Gives, Trouble};
use super::*;
use crate::pack::{GlossaryAbout, glossary};

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-cleanup-{name}-{}", std::process::id()));
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
const MIC: TrackId = TrackId::new(0);
/// A chunk's length: 30 s at 16 kHz.
const CHUNK: u64 = 480_000;

/// A library whose session has `chunks` of final text (one per 30 s, on
/// the microphone, words split at spaces), its final pass done, and the
/// glossary pack of `terms` recorded as its pack.
struct Fixture {
    /// Removed at the end.
    _dir: TestDir,
    /// The library.
    db: Writer,
    /// The session's pack.
    pack: Pack,
}

impl Fixture {
    fn new(name: &str, chunks: &[&str], terms: &[&str]) -> Self {
        let dir = TestDir::new(name);
        let db = Writer::new(&dir.0.join("library.db"));
        let pack = glossary(
            &terms.join("\n"),
            &GlossaryAbout {
                name: "terms".to_owned(),
                version: "1".to_owned(),
                language: "en".to_owned(),
                author: "a tester".to_owned(),
            },
        )
        .unwrap();
        db.with(|db| {
            db.create_session(&NewSession::bare(SESSION))?;
            db.record_session_packs(SESSION, &[pack.pack_ref()])?;
            db.finish_recording(SESSION, None)?;
            for (i, text) in (0_u64..).zip(chunks) {
                let line = final_text(i * CHUNK, text);
                db.add_final_text(SESSION, MIC, line.range.end(), &[line])?;
            }
            Ok(())
        })
        .unwrap();
        let fixture = Self {
            _dir: dir,
            db,
            pack,
        };
        fixture.end(JobKind::FinalPass, &JobEnd::Done);
        fixture
    }

    /// The session's job of `kind`.
    fn job(&self, kind: JobKind) -> JobId {
        let jobs = self.db.with(|db| db.session_jobs(SESSION)).unwrap();
        jobs.into_iter().find(|j| j.kind == kind).unwrap().id
    }

    /// Ends the session's job of `kind` as `end`.
    fn end(&self, kind: JobKind, end: &JobEnd) {
        let id = self.job(kind);
        self.db.with(|db| db.end_job(id, end)).unwrap();
    }

    /// Clean-up with `gives` as its decider and this session's pack
    /// installed.
    fn clean_up(&self, gives: Gives) -> CleanUp {
        let sources = FakeSources {
            gives,
            packs: vec![self.pack.clone()],
        };
        CleanUp::new(Box::new(sources), SampleRate::SPEECH)
    }

    /// The session's proposals.
    fn proposals(&self) -> Vec<Proposal> {
        self.db.with(|db| db.proposals(SESSION)).unwrap()
    }

    /// The session's newest revision's text, chunk by chunk.
    fn shown(&self) -> Vec<String> {
        let newest = *self
            .db
            .with(|db| db.revisions(SESSION))
            .unwrap()
            .last()
            .unwrap();
        self.db
            .with(|db| db.revision(SESSION, newest))
            .unwrap()
            .into_iter()
            .map(|line| line.text.unwrap_or_default())
            .collect()
    }

    /// The session's revisions.
    fn revisions(&self) -> Vec<RevisionNumber> {
        self.db.with(|db| db.revisions(SESSION)).unwrap()
    }
}

/// The final text at sample `start`: `text`'s words spread evenly over
/// [`CHUNK`].
fn final_text(start: u64, text: &str) -> FinalText {
    let words: Vec<&str> = text.split(' ').collect();
    let step = CHUNK / u64::try_from(words.len()).unwrap();
    let range =
        |a: u64, b: u64| SampleRange::new(SampleIndex::new(a), SampleIndex::new(b)).unwrap();
    FinalText {
        track: MIC,
        range: range(start, start + CHUNK),
        text: Some(text.to_owned()),
        words: (0_u64..)
            .zip(&words)
            .map(|(i, w)| FinalWord {
                text: (*w).to_owned(),
                range: range(start + i * step, start + (i + 1) * step),
                log_prob: None,
            })
            .collect(),
        heard_by: HeardBy {
            engine: "sherpa-onnx".to_owned(),
            model: "parakeet".to_owned(),
        },
    }
}

/// Runs `clean_up` on the fixture's session, stopping when `stop` says.
fn run_with(clean_up: &mut CleanUp, fixture: &Fixture, stop: &dyn Fn() -> bool) -> JobEnd {
    let progress = |_| {};
    let answered = || {};
    let engine_down = |_: Option<&str>| {};
    let running = Running {
        stop,
        progress: &progress,
        answered: &answered,
        engine_down: &engine_down,
    };
    clean_up.run(&fixture.db, SESSION, &running)
}

/// Runs `clean_up` on the fixture's session to its end.
fn run(clean_up: &mut CleanUp, fixture: &Fixture) -> JobEnd {
    run_with(clean_up, fixture, &|| false)
}

/// A proposal without its number, to compare two runs.
fn unnumbered(p: &Proposal) -> (nota_store::Proposed, Decided, Outcome, Option<Reviewed>) {
    (p.proposed.clone(), p.decided.clone(), p.outcome, p.review)
}

/// Each proposal's heard span, term and outcome.
fn outcomes(fixture: &Fixture) -> Vec<(String, String, Outcome)> {
    fixture
        .proposals()
        .iter()
        .map(|p| (p.proposed.heard.clone(), p.proposed.term.clone(), p.outcome))
        .collect()
}

const AMLODIPINE: &str = "start amylo diping daily";

/// A yes the gate lets through is applied as a new revision, built on the
/// final text, which names the proposal it came from.
#[test]
fn a_yes_the_gate_passes_is_applied_in_a_new_revision() {
    let fixture = Fixture::new("applied", &[AMLODIPINE], &["amlodipine"]);
    let fake = Fake::saying_yes([("amylo diping", "amlodipine")]);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake));
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fixture.shown(), ["start amlodipine daily"]);
    let one = RevisionNumber::FINAL;
    assert_eq!(fixture.revisions().len(), 2, "{:?}", fixture.revisions());
    let proposals = fixture.proposals();
    let [p] = &proposals[..] else {
        panic!("{proposals:?}")
    };
    assert!(
        matches!(p.outcome, Outcome::Applied(Some(r)) if r != one),
        "{p:?}"
    );
    assert!(matches!(p.decided, Decided::Applied { .. }), "{p:?}");
    assert_eq!(p.proposed.decided_by, fake::fake_by());
    assert_eq!(
        p.proposed.origin,
        Origin::Pack {
            name: "terms".to_owned(),
            version: "1".to_owned(),
            digest: fixture.pack.pack_ref().digest(),
        }
    );
    assert_eq!(p.proposed.thresholds, thresholds());
}

/// A no is rejected and logged: nothing changes, and no revision is made.
#[test]
fn a_no_is_rejected_and_logged() {
    let fixture = Fixture::new("no", &[AMLODIPINE], &["amlodipine"]);
    let mut clean_up = fixture.clean_up(Gives::Fake(Fake::saying_yes([])));
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fixture.revisions(), [RevisionNumber::FINAL]);
    assert_eq!(
        outcomes(&fixture),
        [(
            "amylo diping".to_owned(),
            "amlodipine".to_owned(),
            Outcome::Rejected
        )]
    );
    assert!(matches!(
        fixture.proposals()[0].decided,
        Decided::No { yes: None }
    ));
}

/// A yes the gate refuses (here C1 is below its threshold) is a "to
/// check" card, within the cap, and changes nothing.
#[test]
fn a_yes_the_gate_refuses_is_a_card() {
    let text = "patients with chronic conditions in my consult";
    let fixture = Fixture::new("card", &[text], &["chronic kidney disease"]);
    let fake = Fake::saying_yes([("chronic conditions", "chronic kidney disease")]);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake));
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fixture.revisions(), [RevisionNumber::FINAL]);
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
                similarity: Some(s),
                ..
            } if s < SIMILARITY_MIN
        ),
        "{p:?}"
    );
}

/// A question the decider gives no readable answer to logs its
/// candidates as rejected, with the reason, and the job goes on to the
/// next chunk.
#[test]
fn an_unreadable_answer_logs_its_candidates_and_goes_on() {
    let fixture = Fixture::new(
        "unreadable",
        &[AMLODIPINE, "then amylo diping again"],
        &["amlodipine"],
    );
    let fake = Fake::saying_yes([("amylo diping", "amlodipine")]).with_trouble(0, Trouble::Failed);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake));
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    let proposals = fixture.proposals();
    let [first, second] = &proposals[..] else {
        panic!("{proposals:?}")
    };
    assert_eq!(
        first.decided,
        Decided::NoAnswer("the decider's answer couldn't be read: garbled".to_owned())
    );
    assert_eq!(first.outcome, Outcome::Rejected);
    assert!(
        matches!(second.outcome, Outcome::Applied(Some(_))),
        "{second:?}"
    );
    assert_eq!(
        fixture.shown(),
        ["start amylo diping daily", "then amlodipine again"]
    );
}

/// With no decider set up the job waits for one, and asks nothing.
#[test]
fn without_a_decider_the_job_waits_for_one() {
    let fixture = Fixture::new("no-decider", &[AMLODIPINE], &["amlodipine"]);
    let mut clean_up = fixture.clean_up(Gives::NotSetUp);
    assert_eq!(clean_up.lacks(&fixture.db, SESSION), Some(Wait::Decider));
    assert_eq!(
        run(&mut clean_up, &fixture),
        JobEnd::Waiting(Some(Wait::Decider))
    );
    assert_eq!(fixture.proposals(), []);
}

/// A decider that can't be used ends the run as an engine failure, with
/// the reason, so it's tried again later.
#[test]
fn an_unusable_decider_ends_the_run_as_an_engine_failure() {
    let fixture = Fixture::new("unusable", &[AMLODIPINE], &["amlodipine"]);
    let mut clean_up = fixture.clean_up(Gives::Unavailable("the model isn't at /m"));
    assert_eq!(clean_up.lacks(&fixture.db, SESSION), None);
    assert_eq!(
        run(&mut clean_up, &fixture),
        JobEnd::EngineDown("the model isn't at /m".to_owned())
    );
}

/// A decider that stops working mid-run ends it as an engine failure;
/// what was committed stays, and the next run carries on from there.
#[test]
fn a_decider_that_stops_working_ends_the_run_keeping_what_it_did() {
    let chunks = [AMLODIPINE, "then amylo diping again"];
    let fixture = Fixture::new("stops-working", &chunks, &["amlodipine"]);
    let fake =
        Fake::saying_yes([("amylo diping", "amlodipine")]).with_trouble(1, Trouble::Unavailable);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake.clone()));
    assert_eq!(
        run(&mut clean_up, &fixture),
        JobEnd::EngineDown("the decider isn't working: no server".to_owned())
    );
    // The first chunk's proposal is stored, not yet applied.
    assert_eq!(
        fixture
            .proposals()
            .iter()
            .map(|p| p.outcome)
            .collect::<Vec<_>>(),
        [Outcome::Applied(None)]
    );
    assert_eq!(fixture.revisions(), [RevisionNumber::FINAL]);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    // Only the second chunk was asked again.
    assert_eq!(fake.questions(), 3);
    assert_eq!(
        fixture.shown(),
        ["start amlodipine daily", "then amlodipine again"]
    );
}

/// The job waits for its session's final pass, and fails, saying why, if
/// that failed.
#[test]
fn the_job_waits_for_the_final_pass() {
    let fixture = Fixture::new("waits-final", &[AMLODIPINE], &["amlodipine"]);
    let fake = Fake::saying_yes([]);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake.clone()));
    fixture.end(JobKind::FinalPass, &JobEnd::Waiting(Some(Wait::Engine)));
    assert_eq!(clean_up.lacks(&fixture.db, SESSION), Some(Wait::FinalText));
    assert_eq!(
        run(&mut clean_up, &fixture),
        JobEnd::Waiting(Some(Wait::FinalText))
    );
    fixture.end(JobKind::FinalPass, &JobEnd::Failed("it broke".to_owned()));
    assert_eq!(clean_up.lacks(&fixture.db, SESSION), None);
    assert_eq!(
        run(&mut clean_up, &fixture),
        JobEnd::Failed("there's no final transcript to fix: it broke".to_owned())
    );
    assert_eq!(fake.questions(), 0);
}

/// From #153: a recording that starts between two questions stops the
/// job before the second, and the decider (with the server it keeps
/// loaded) is dropped as the run returns. Run again, it carries on.
#[test]
fn a_recording_between_two_questions_drops_the_decider() {
    let chunks = [AMLODIPINE, "then amylo diping again"];
    let fixture = Fixture::new("stop-between", &chunks, &["amlodipine"]);
    let fake = Fake::saying_yes([("amylo diping", "amlodipine")]).flagging_after(1);
    let log = Arc::clone(&fake.log);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake));
    let recording = || log.flag.load(Ordering::SeqCst);
    assert_eq!(
        run_with(&mut clean_up, &fixture, &recording),
        JobEnd::Waiting(None)
    );
    assert_eq!(log.asked.lock().unwrap().len(), 1);
    // The run's decider is gone; only the sources' own copy is left.
    assert_eq!(log.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(log.asked.lock().unwrap().len(), 2);
    assert_eq!(
        fixture.shown(),
        ["start amlodipine daily", "then amlodipine again"]
    );
}

/// A stop once the last chunk is asked leaves the placing to the next run,
/// which places what the first committed.
#[test]
fn a_stop_after_the_last_question_leaves_the_placing_to_the_next_run() {
    let fixture = Fixture::new("stop-placing", &[AMLODIPINE], &["amlodipine"]);
    let fake = Fake::saying_yes([("amylo diping", "amlodipine")]).flagging_after(1);
    let log = Arc::clone(&fake.log);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake));
    let stop = || log.flag.load(Ordering::SeqCst);
    assert_eq!(
        run_with(&mut clean_up, &fixture, &stop),
        JobEnd::Waiting(None)
    );
    assert_eq!(fixture.revisions(), [RevisionNumber::FINAL]);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(log.asked.lock().unwrap().len(), 1);
    assert_eq!(fixture.shown(), ["start amlodipine daily"]);
}

/// Acceptance: a kill mid-job reruns to the same result. The run is cut
/// off after each number of questions in turn (what a kill leaves is what
/// was committed), and run again to its end; each ends with the same
/// proposals and text as a run never cut off.
#[test]
fn a_run_cut_off_anywhere_reruns_to_the_same_result() {
    let chunks = [
        AMLODIPINE,
        "patients with chronic conditions in my consult",
        "then amylo diping again",
        "nothing here at all",
        "amylo diping twice amylo diping",
    ];
    let terms = ["amlodipine", "chronic kidney disease"];
    let yes = [
        ("amylo diping", "amlodipine"),
        ("chronic conditions", "chronic kidney disease"),
    ];
    let whole = Fixture::new("cut-whole", &chunks, &terms);
    let mut clean_up = whole.clean_up(Gives::Fake(Fake::saying_yes(yes)));
    assert_eq!(run(&mut clean_up, &whole), JobEnd::Done);
    let want: Vec<_> = whole.proposals().iter().map(unnumbered).collect();
    let questions = 4;
    // One proposal per chunk with a candidate, and two in the last.
    assert_eq!(want.len(), 5, "{want:?}"); // check-bound
    for cut in 1..=questions {
        let fixture = Fixture::new(&format!("cut-{cut}"), &chunks, &terms);
        let fake = Fake::saying_yes(yes).flagging_after(cut);
        let log = Arc::clone(&fake.log);
        let mut clean_up = fixture.clean_up(Gives::Fake(fake));
        let stop = || log.flag.load(Ordering::SeqCst);
        assert_eq!(
            run_with(&mut clean_up, &fixture, &stop),
            JobEnd::Waiting(None)
        );
        assert_eq!(
            run(&mut clean_up, &fixture),
            JobEnd::Done,
            "cut after {cut}"
        );
        let got: Vec<_> = fixture.proposals().iter().map(unnumbered).collect();
        assert_eq!(got, want, "cut after {cut}");
        assert_eq!(fixture.shown(), whole.shown(), "cut after {cut}");
        assert_eq!(fixture.revisions(), whole.revisions(), "cut after {cut}");
        assert_eq!(
            log.asked.lock().unwrap().len(),
            questions,
            "cut after {cut}"
        );
    }
}

/// Acceptance: running again on a session with reviewed spans adds
/// nothing to them. A second run with nothing new adds nothing at all; a
/// pair learned since adds a proposal only where no review or fix holds
/// the words.
#[test]
fn a_rerun_adds_nothing_to_reviewed_spans() {
    let chunks = ["taking amylo diping daily", "twice daily"];
    let fixture = Fixture::new("rerun", &chunks, &["amlodipine"]);
    let fake = Fake::saying_yes([
        ("amylo diping", "amlodipine"),
        ("amylo diping", "amlodipinum"),
        ("daily", "DailyMed"),
    ]);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake.clone()));
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    let first = fixture.proposals();
    assert_eq!(first.len(), 1, "{first:?}");
    // The user reviews the fix; a second run finds nothing new to ask.
    fixture
        .db
        .with(|db| db.review_proposal(first[0].id, Reviewed::Rejected))
        .unwrap();
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fake.questions(), 1);
    assert_eq!(fixture.proposals().len(), 1);
    // Pairs learned since offer a term on the reviewed words, and one on
    // words no review holds.
    let at = WallTime::from_unix_seconds(1_762_765_200).unwrap();
    fixture
        .db
        .with(|db| {
            db.learn(
                SESSION,
                "amylo diping",
                "amlodipinum",
                Verdict::Accepted,
                at,
            )?;
            db.learn(SESSION, "daily", "DailyMed", Verdict::Accepted, at)
        })
        .unwrap();
    let revisions = fixture.revisions();
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    let proposals = fixture.proposals();
    let added: Vec<(u64, &str, &str)> = proposals[1..]
        .iter()
        .map(|p| {
            let at = &p.proposed;
            (at.chunk.start.get(), at.heard.as_str(), at.term.as_str())
        })
        .collect();
    assert_eq!(
        added,
        [(0, "daily", "DailyMed"), (CHUNK, "daily", "DailyMed")]
    );
    // The reviewed fix is as it was, and no new proposal names its words.
    let mut was = first[0].clone();
    was.review = Some(Reviewed::Rejected);
    assert_eq!(unnumbered(&proposals[0]), unnumbered(&was));
    let held = &proposals[0].proposed;
    assert!(
        proposals[1..]
            .iter()
            .all(|p| p.proposed.chunk != held.chunk || !p.proposed.words.overlaps(held.words)),
        "{proposals:?}"
    );
    assert_eq!(fixture.revisions().len(), revisions.len() + 1);
    assert_eq!(
        fixture.shown(),
        ["taking amlodipine DailyMed", "twice DailyMed"]
    );
}

/// Acceptance: the card cap holds on a recording with many signals, and
/// the cards are the best ranked. Twenty-two minutes of recording allows
/// three cards; eleven yeses the gate refuses are the signals.
#[test]
fn the_card_cap_holds_on_many_signals() {
    let mut chunks: Vec<String> = (0..44).map(|i| format!("nothing here {i}")).collect();
    let signals = 11;
    for i in 0..signals {
        chunks[i * 4] = format!("chunk {i} with chronic conditions here");
    }
    let chunks: Vec<&str> = chunks.iter().map(String::as_str).collect();
    let fixture = Fixture::new("cap", &chunks, &["chronic kidney disease"]);
    // The same span, term and answer in every chunk, so they rank by
    // place; the ranking's other keys are `cards`' tests.
    let fake =
        Fake::with_probabilities([("chronic conditions", "chronic kidney disease", Some(0.5))]);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake));
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    let proposals = fixture.proposals();
    assert_eq!(proposals.len(), signals);
    let cards: Vec<u64> = proposals
        .iter()
        .filter(|p| p.outcome == Outcome::Card)
        .map(|p| p.proposed.chunk.start.get() / CHUNK)
        .collect();
    // 44 chunks of 30 s is 22 minutes: three cards. With every signal
    // equal, the earliest in the session rank first.
    assert_eq!(cards, [0, 4, 8]); // check-bound
    let over = proposals
        .iter()
        .filter(|p| p.outcome == Outcome::OverCap)
        .count();
    assert_eq!(over, signals - 3);
}

/// A session with no final text has nothing to clean up.
#[test]
fn a_session_with_no_final_text_is_done_at_once() {
    let fixture = Fixture::new("empty", &[], &["amlodipine"]);
    let fake = Fake::saying_yes([]);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake.clone()));
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fake.questions(), 0);
}

/// A pack the session recorded that isn't installed fails the job,
/// naming it.
#[test]
fn a_missing_pack_fails_the_job() {
    let fixture = Fixture::new("missing-pack", &[AMLODIPINE], &["amlodipine"]);
    let sources = FakeSources {
        gives: Gives::Fake(Fake::saying_yes([])),
        packs: Vec::new(),
    };
    let mut clean_up = CleanUp::new(Box::new(sources), SampleRate::SPEECH);
    let end = run(&mut clean_up, &fixture);
    assert_eq!(
        end,
        JobEnd::Failed(
            "the library database failed: session 1 used pack terms 1, which is not installed as it was then"
                .to_owned()
        )
    );
}

/// The lookup's terms: a learned pair is an alias of a pack's term, and a
/// term only the learned pairs give comes from them.
#[test]
fn learned_pairs_join_the_vocabulary() {
    let fixture = Fixture::new("vocabulary", &[AMLODIPINE], &["amlodipine"]);
    let at = WallTime::from_unix_seconds(1_762_765_200).unwrap();
    let terms = fixture
        .db
        .with(|db| {
            db.learn(SESSION, "a dlp one", "GLP-1", Verdict::Accepted, at)?;
            db.learn(SESSION, "amylo", "amlodipine", Verdict::Accepted, at)?;
            db.session_terms(SESSION, std::slice::from_ref(&fixture.pack))
        })
        .unwrap();
    let (terms, origins) = vocabulary(&terms, "en");
    let listed: Vec<(&str, Vec<&str>)> = terms
        .iter()
        .map(|t| {
            (
                t.term.as_str(),
                t.aliases.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("amlodipine", vec![]),
            ("GLP-1", vec!["a dlp one"]),
            ("amlodipine", vec!["amylo"]),
        ]
    );
    assert_eq!(origins.get("GLP-1"), Some(&Origin::Learned));
    assert!(matches!(
        origins.get("amlodipine"),
        Some(Origin::Pack { .. })
    ));
}

/// The recording's length for the cap is the longest track's span of
/// final text.
#[test]
fn the_recording_is_as_long_as_its_longest_track() {
    let fixture = Fixture::new("length", &["a b", "c d", "e f"], &[]);
    let lines = fixture
        .db
        .with(|db| db.revision(SESSION, RevisionNumber::FINAL))
        .unwrap();
    assert_eq!(
        recorded(&lines, SampleRate::SPEECH),
        Duration::from_secs(90)
    );
    assert_eq!(recorded(&[], SampleRate::SPEECH), Duration::ZERO);
}

/// A decider stopped mid-question (a recording started while it
/// answered) ends the run waiting, storing nothing for that chunk, which
/// the next run asks again.
#[test]
fn a_decider_stopped_mid_question_stores_nothing_for_it() {
    let fixture = Fixture::new("stopped-mid", &[AMLODIPINE], &["amlodipine"]);
    let fake = Fake::saying_yes([("amylo diping", "amlodipine")]).with_trouble(0, Trouble::Stopped);
    let mut clean_up = fixture.clean_up(Gives::Fake(fake.clone()));
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Waiting(None));
    assert_eq!(fixture.proposals(), []);
    assert_eq!(run(&mut clean_up, &fixture), JobEnd::Done);
    assert_eq!(fake.questions(), 2);
    assert_eq!(fixture.shown(), ["start amlodipine daily"]);
}
