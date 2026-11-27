use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use nota_checks::{English, Term};
use nota_core::{FakeClock, SampleIndex, SessionTime, TrackId};

use super::*;
use crate::cleanup::fake::{Fake, Trouble, fake_by};
use crate::cleanup::offering_in;

mod command;
mod fixture;
mod log;
mod session;

/// A fresh directory under the system temp dir, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    /// The directory for the test `name`, empty.
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding outside the recorder's write path"
    )]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-bench-{name}-{}", std::process::id()));
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

/// Writes `text` to the file `path`, for a test to read back.
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding outside the recorder's write path"
)]
fn write_file(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

/// The track the invented chunks are on.
const MIC: TrackId = TrackId::new(0);

/// A chunk's length: 30 s at 16 kHz.
const CHUNK: u64 = 480_000;

/// The invented chunks the engines are asked about.
const CHUNKS: [&str; 3] = [
    "start amylo diping daily",
    "take hyper profen with food",
    "we went over the notes again",
];

/// The terms offered in them.
const TERMS: [&str; 2] = ["amlodipine", "ibuprofen"];

/// The candidates the lookup offers in `chunks` from `terms`, each chunk
/// 30 s after the one before, with C3 as `precheck`.
fn candidates(chunks: &[&str], terms: &[&str], precheck: Precheck) -> Candidates {
    let english = English::new().unwrap();
    let vocab: Vec<Term> = terms
        .iter()
        .map(|t| Term {
            term: (*t).to_owned(),
            aliases: Vec::new(),
            language: "en".to_owned(),
        })
        .collect();
    let chunks = (0_u64..)
        .zip(chunks)
        .filter_map(|(i, text)| {
            let key = ChunkKey {
                track: MIC,
                start: SampleIndex::new(i * CHUNK),
            };
            Chunk::new(key, text.split(' ').map(|w| (w, None)))
        })
        .collect();
    let offering = offering_in(&english, &vocab, &BTreeMap::new(), &|_, _| true, chunks);
    Candidates {
        source: "a test".to_owned(),
        offering,
        screen: Screen::new(precheck, &[]),
        precheck,
        saved: None,
    }
}

/// An engine named `name` that answers as `fake` does.
fn engine(name: &str, fake: &Fake) -> Engine {
    Engine {
        name: name.to_owned(),
        decider: Box::new(fake.clone()),
        by: fake_by(),
    }
}

/// The key of the candidate `window` as `term` in chunk `n`.
fn key(n: u64, first: u32, last: u32, window: &str, term: &str) -> Key {
    Key {
        chunk: ChunkKey {
            track: MIC,
            start: SampleIndex::new(n * CHUNK),
        },
        offered: WordSpan::new(first, last).unwrap(),
        window: window.to_owned(),
        term: term.to_owned(),
    }
}

/// A verdict.
const fn verdict(said: Said, ending: Ending) -> Verdict {
    Verdict { said, ending }
}

/// The clock every test times its engines by: it doesn't move.
fn clock() -> FakeClock {
    FakeClock::new(SessionTime::from_nanos(0))
}

/// Acceptance (#164): two engines that differ on one candidate are
/// reported with exactly that difference, and every candidate with each
/// engine's verdict.
#[test]
fn two_engines_that_differ_on_one_candidate_are_reported_with_that_difference() {
    let both = Fake::saying_yes([("amylo diping", "amlodipine"), ("profen", "ibuprofen")]);
    let one = Fake::saying_yes([("amylo diping", "amlodipine")]);
    let candidates = candidates(&CHUNKS, &TERMS, Precheck::Off);
    let engines = vec![engine("first", &both), engine("second", &one)];
    let runs = run(&candidates, engines, &clock(), &|| false);
    let profen = key(1, 2, 2, "profen", "ibuprofen");
    assert_eq!(
        differences(&runs),
        [(
            profen,
            vec![
                Some(verdict(Said::Yes, Ending::Applied)),
                Some(verdict(Said::No, Ending::Rejected)),
            ]
        )]
    );
    let mut out = Vec::new();
    report(&candidates, &runs, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(
        text,
        format!(
            "nota bench: a test, 2 candidates in 2 chunks, C3 off\n\
             {DIAGNOSIS}\n\
             \n\
             Engines\n\
             \x20 first: fake as-scripted (local), 2 questions, 0 skipped by C3, 0.0 s, \
             cost not reported\n\
             \x20 second: fake as-scripted (local), 2 questions, 0 skipped by C3, 0.0 s, \
             cost not reported\n\
             \n\
             Differences (1 of 2 candidates)\n\
             \x20 track 0 at 0:30 \"profen\" → ibuprofen: first yes · applied | \
             second no · rejected\n\
             \n\
             Every candidate\n\
             \x20 track 0 at 0:00 \"amylo diping\" → amlodipine: first yes · applied | \
             second yes · applied\n\
             \x20 track 0 at 0:30 \"profen\" → ibuprofen: first yes · applied | \
             second no · rejected\n"
        )
    );
}

/// Engines that agree on everything have no differences.
#[test]
fn engines_that_agree_have_no_differences() {
    let fake = Fake::saying_yes([("amylo diping", "amlodipine")]);
    let candidates = candidates(&CHUNKS, &TERMS, Precheck::Off);
    let engines = vec![engine("first", &fake), engine("second", &fake)];
    let runs = run(&candidates, engines, &clock(), &|| false);
    assert_eq!(differences(&runs), []);
    assert_eq!(runs[0].verdicts, runs[1].verdicts);
}

/// An engine that can't be run stops where it failed, says why, and the
/// candidates it didn't get to are differences.
#[test]
fn an_engine_that_goes_down_stops_and_says_why() {
    let fine = Fake::saying_yes([]);
    let down = Fake::saying_yes([]).with_trouble(1, Trouble::Unavailable);
    let candidates = candidates(&CHUNKS, &TERMS, Precheck::Off);
    let engines = vec![engine("fine", &fine), engine("down", &down)];
    let runs = run(&candidates, engines, &clock(), &|| false);
    assert_eq!(
        runs[1].down.as_deref(),
        Some("the decider isn't working: no server")
    );
    // The question that found it down was put, and counts.
    assert_eq!((runs[1].questions, down.questions()), (2, 2));
    assert_eq!(
        differences(&runs),
        [(
            key(1, 2, 2, "profen", "ibuprofen"),
            vec![Some(verdict(Said::No, Ending::Rejected)), None]
        )]
    );
}

/// With C3 on, the bench skips what clean-up would, and counts it: a
/// chunk whose every candidate is skipped is no question.
#[test]
fn with_the_pre_check_on_the_bench_skips_as_clean_up_would() {
    let chunks = [
        "her ankle amylo edema was worse",
        "start amylo diping daily",
    ];
    let fake = Fake::saying_yes([("amylo edema", "amlodipine")]);
    let candidates = candidates(&chunks, &["amlodipine"], Precheck::On);
    let runs = run(&candidates, vec![engine("only", &fake)], &clock(), &|| {
        false
    });
    let [only] = &runs[..] else {
        panic!("{runs:?}")
    };
    assert_eq!((only.questions, only.skipped), (1, 1));
    assert_eq!(fake.questions(), 1);
    assert_eq!(
        only.verdicts
            .get(&key(0, 2, 3, "amylo edema", "amlodipine"))
            .copied(),
        Some(verdict(Said::Skipped, Ending::Rejected))
    );
}

/// A yes the gate refuses is reported as refused, with its rule: in a run
/// it would be ranked for a card.
#[test]
fn a_refused_yes_is_reported_with_its_rule() {
    let fake = Fake::saying_yes([("amylo edema", "amlodipine")]);
    let candidates = candidates(
        &["her ankle amylo edema was worse"],
        &["amlodipine"],
        Precheck::Off,
    );
    let runs = run(&candidates, vec![engine("only", &fake)], &clock(), &|| {
        false
    });
    let verdict = runs[0].verdicts.values().next().copied().unwrap();
    assert_eq!(verdict.to_string(), "yes · refused (dissimilar)");
}

/// A decider that answers as a fake does, taking a second and a half of
/// `clock`'s time over each question.
struct Slow {
    /// The answers.
    fake: Fake,
    /// The clock it moves.
    clock: Arc<FakeClock>,
}

impl Decider for Slow {
    fn place(&self) -> Place {
        Place::Cloud
    }

    fn decide(
        &mut self,
        question: &nota_core::decide::Question,
        stop: &dyn Fn() -> bool,
    ) -> Result<nota_core::decide::Decisions, nota_core::decide::DecideError> {
        self.clock.advance(Duration::from_millis(1500));
        self.fake.decide(question, stop)
    }
}

/// The time an engine took is read from the clock: two questions of a
/// second and a half each.
#[test]
fn an_engine_is_timed_by_the_clock() {
    let clock = Arc::new(clock());
    let slow = Slow {
        fake: Fake::saying_yes([]),
        clock: Arc::clone(&clock),
    };
    let engine = Engine {
        name: "slow".to_owned(),
        decider: Box::new(slow),
        by: fake_by(),
    };
    let candidates = candidates(&CHUNKS, &TERMS, Precheck::Off);
    let runs = run(&candidates, vec![engine], &*clock, &|| false);
    assert_eq!(runs[0].questions, 2);
    assert_eq!(runs[0].took, Duration::from_secs(3));
}

/// Text from a log or a fixture is shown with its control characters
/// escaped, so it can't start a line of its own or drive the terminal.
#[test]
fn control_characters_are_shown_escaped() {
    assert_eq!(
        plain("amylo\n  track 0 at 0:00 \u{1b}[2J"),
        "amylo\\n  track 0 at 0:00 \\u{1b}[2J"
    );
    assert_eq!(
        plain("hyper profen → ibuprofen"),
        "hyper profen → ibuprofen"
    );
}
