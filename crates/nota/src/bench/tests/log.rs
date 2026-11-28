//! The bench's logs: written and read back, compared, and refused when
//! they aren't logs.

use nota_core::WallTime;
use proptest::prelude::*;

use super::*;
use crate::bench::log::{Change, Log, REFUSALS, changed, compare, read, write};

/// The stamp the tests' logs are named with.
const STAMP: &str = "20251110-090000";

/// When the tests' benches started: the stamp's moment.
fn started() -> WallTime {
    WallTime::from_unix_seconds(1_762_765_200).unwrap()
}

/// Two runs over the invented chunks: `first` says yes to both
/// candidates, `second` only to the first.
fn two_runs() -> (Candidates, Vec<EngineRun>) {
    let both = Fake::saying_yes([("amylo diping", "amlodipine"), ("profen", "ibuprofen")]);
    let one = Fake::saying_yes([("amylo diping", "amlodipine")]);
    let candidates = candidates(&CHUNKS, &TERMS, Precheck::Off);
    let engines = vec![engine("first", &both), engine("second", &one)];
    let runs = run(&candidates, engines, &clock(), &|| false);
    (candidates, runs)
}

/// Acceptance (#164): a run's log can be compared with an earlier run's,
/// and the changed answers are listed: exactly the one candidate the two
/// runs differ on, with its old and new verdicts.
#[test]
fn a_run_can_be_compared_with_an_earlier_one_and_the_changed_answers_listed() {
    let dir = TestDir::new("compare");
    let (candidates, runs) = two_runs();
    let old = write(&dir.0, STAMP, &runs[0], &candidates, Some(started())).unwrap();
    let new = write(&dir.0, STAMP, &runs[1], &candidates, Some(started())).unwrap();
    assert_eq!(
        changed(&read(&old).unwrap(), &read(&new).unwrap()),
        [Change {
            key: key(1, 2, 2, "profen", "ibuprofen"),
            old: Some(verdict(Said::Yes, Ending::Applied)),
            new: Some(verdict(Said::No, Ending::Rejected)),
        }]
    );
    let mut out = Vec::new();
    compare(&old, &new, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!(
            "nota bench compare\n\
             \x20 old: first, a test ({})\n\
             \x20 new: second, a test ({})\n\
             Changed answers (1)\n\
             \x20 track 0 at 0:30 \"profen\" → ibuprofen: yes · applied → no · rejected\n",
            old.display(),
            new.display()
        )
    );
}

/// Two logs with the same answers have nothing to list.
#[test]
fn logs_with_the_same_answers_say_nothing_changed() {
    let dir = TestDir::new("same");
    let (candidates, runs) = two_runs();
    let old = write(&dir.0, STAMP, &runs[0], &candidates, Some(started())).unwrap();
    let new = write(&dir.0, "20251111-090000", &runs[0], &candidates, None).unwrap();
    let mut out = Vec::new();
    compare(&old, &new, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!(
            "nota bench compare\n\
             \x20 old: first, a test ({})\n\
             \x20 new: first, a test ({})\n\
             No answers changed.\n",
            old.display(),
            new.display()
        )
    );
}

/// A candidate only one log has is a change, from or to not asked: an
/// engine that went down didn't get to it.
#[test]
fn a_candidate_only_one_log_has_is_a_change() {
    let dir = TestDir::new("only-one");
    let fine = Fake::saying_yes([]);
    let down = Fake::saying_yes([]).with_trouble(1, Trouble::Unavailable);
    let candidates = candidates(&CHUNKS, &TERMS, Precheck::Off);
    let engines = vec![engine("fine", &fine), engine("down", &down)];
    let runs = run(&candidates, engines, &clock(), &|| false);
    let old = write(&dir.0, STAMP, &runs[0], &candidates, None).unwrap();
    let new = write(&dir.0, STAMP, &runs[1], &candidates, None).unwrap();
    assert_eq!(
        changed(&read(&old).unwrap(), &read(&new).unwrap()),
        [Change {
            key: key(1, 2, 2, "profen", "ibuprofen"),
            old: Some(verdict(Said::No, Ending::Rejected)),
            new: None,
        }]
    );
}

/// A log read back is the run it was written from: its header and every
/// verdict.
#[test]
fn a_log_reads_back_as_the_run_it_was_written_from() {
    let dir = TestDir::new("round-trip");
    let fake = Fake::saying_yes([("amylo edema", "amlodipine")]);
    let candidates = candidates(
        &[
            "her ankle amylo edema was worse",
            "start amylo diping daily",
        ],
        &["amlodipine"],
        Precheck::On,
    );
    let mut runs = run(&candidates, vec![engine("only", &fake)], &clock(), &|| {
        false
    });
    let mut only = runs.remove(0);
    // Whole milliseconds, which is what a log keeps.
    only.took = Duration::from_millis(1_250);
    only.down = Some("the decider isn't working: no server".to_owned());
    let path = write(&dir.0, STAMP, &only, &candidates, Some(started())).unwrap();
    assert_eq!(
        path,
        dir.0.join(format!("{STAMP}-only.jsonl")),
        "named by stamp and engine"
    );
    assert_eq!(
        read(&path).unwrap(),
        Log {
            engine: "only".to_owned(),
            by: fake_by(),
            source: "a test".to_owned(),
            precheck: Precheck::On,
            started: Some(started()),
            questions: 1,
            skipped: 1,
            took: Duration::from_millis(1_250),
            down: Some("the decider isn't working: no server".to_owned()),
            verdicts: only.verdicts,
        }
    );
}

/// A log is never overwritten: a second with the same name is refused,
/// and the first is left as it was.
#[test]
fn a_log_is_never_overwritten() {
    let dir = TestDir::new("no-overwrite");
    let (candidates, runs) = two_runs();
    let path = write(&dir.0, STAMP, &runs[0], &candidates, None).unwrap();
    let first = read(&path).unwrap();
    let mut again = runs[1].clone();
    again.name = "first".to_owned();
    let err = write(&dir.0, STAMP, &again, &candidates, None).unwrap_err();
    // The cause after the colon is the system's own words for it.
    let made = format!("the log {} can't be made: ", path.display());
    assert!(err.starts_with(&made), "{err}");
    assert_eq!(read(&path).unwrap(), first);
}

/// A log holds words from a transcript, so only its owner can read it.
#[cfg(unix)]
#[test]
fn a_log_is_readable_by_its_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::new("private");
    let (candidates, runs) = two_runs();
    let path = write(&dir.0, STAMP, &runs[0], &candidates, None).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// The log folder is made if it's missing, with the folders above it.
#[test]
fn the_log_folder_is_made_if_missing() {
    let dir = TestDir::new("folder");
    let (candidates, runs) = two_runs();
    let folder = dir.0.join("data").join("bench");
    let path = write(&folder, STAMP, &runs[0], &candidates, None).unwrap();
    assert_eq!(path, folder.join(format!("{STAMP}-first.jsonl")));
    assert!(path.is_file());
}

/// A file whose first line isn't a bench log's header is refused, naming
/// the file and the line.
#[test]
fn a_file_that_is_not_a_log_is_refused_at_its_header() {
    let dir = TestDir::new("not-a-log");
    let path = dir.0.join("other.jsonl");
    write_file(&path, "{\"nota_bench_log\": 2}\n");
    assert_eq!(
        read(&path).unwrap_err(),
        format!(
            "{}: line 1: not a bench log header: nota_bench_log isn't 1",
            path.display()
        )
    );
}

/// A verdict line that can't be read is refused, naming the file and its
/// line number.
#[test]
fn a_bad_verdict_line_is_refused_with_its_line_number() {
    let dir = TestDir::new("bad-line");
    let (candidates, runs) = two_runs();
    let written = write(&dir.0, STAMP, &runs[0], &candidates, None).unwrap();
    let text = read_text(&written, 1 << 20).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    // A third line, after the header and the first verdict, that says
    // what no decider says.
    let bad = lines[1].replace("\"said\":\"yes\"", "\"said\":\"maybe\"");
    let text = format!("{}\n{}\n{bad}\n", lines[0], lines[1]);
    let path = dir.0.join("bad.jsonl");
    write_file(&path, &text);
    assert_eq!(
        read(&path).unwrap_err(),
        format!(
            "{}: line 3: not a bench log line: said isn't yes, no, no-answer or skipped",
            path.display()
        )
    );
}

/// A log that gives one candidate's verdict twice is refused at the line
/// that repeats it: a log holds one verdict for each candidate.
#[test]
fn a_candidate_given_twice_is_refused_at_its_second_line() {
    let dir = TestDir::new("twice");
    let (candidates, runs) = two_runs();
    let written = write(&dir.0, STAMP, &runs[0], &candidates, None).unwrap();
    let text = read_text(&written, 1 << 20).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    // The first verdict again as the third line, in place of the second.
    let path = dir.0.join("twice.jsonl");
    write_file(
        &path,
        &format!("{}\n{}\n{}\n", lines[0], lines[1], lines[1]),
    );
    assert_eq!(
        read(&path).unwrap_err(),
        format!(
            "{}: line 3: not a bench log line: its candidate is there already",
            path.display()
        )
    );
}

/// A refused ending without its rule is refused: the log can't say which
/// rule refused it.
#[test]
fn a_refused_ending_without_its_rule_is_refused() {
    let dir = TestDir::new("no-rule");
    let (candidates, runs) = two_runs();
    let header =
        std::fs::read_to_string(write(&dir.0, STAMP, &runs[0], &candidates, None).unwrap())
            .unwrap();
    let header = header.lines().next().unwrap();
    let line = "{\"track\":0,\"start\":0,\"first\":2,\"last\":3,\"window\":\"amylo diping\",\
                \"term\":\"amlodipine\",\"said\":\"yes\",\"ending\":\"refused\",\"rule\":null}";
    let path = dir.0.join("no-rule.jsonl");
    write_file(&path, &format!("{header}\n{line}\n"));
    assert_eq!(
        read(&path).unwrap_err(),
        format!(
            "{}: line 2: not a bench log line: a refused ending has no rule",
            path.display()
        )
    );
}

/// An applied or rejected ending that gives a rule is refused: only a
/// refused ending has one.
#[test]
fn a_rule_on_an_ending_that_isnt_refused_is_refused() {
    let dir = TestDir::new("stray-rule");
    let (candidates, runs) = two_runs();
    let written = write(&dir.0, STAMP, &runs[0], &candidates, None).unwrap();
    let text = read_text(&written, 1 << 20).unwrap();
    let header = text.lines().next().unwrap();
    for ending in ["applied", "rejected"] {
        let line = format!(
            "{{\"track\":0,\"start\":0,\"first\":2,\"last\":3,\"window\":\"amylo diping\",\
             \"term\":\"amlodipine\",\"said\":\"yes\",\"ending\":\"{ending}\",\"rule\":\"too_long\"}}"
        );
        let path = dir.0.join(format!("{ending}.jsonl"));
        write_file(&path, &format!("{header}\n{line}\n"));
        assert_eq!(
            read(&path).unwrap_err(),
            format!(
                "{}: line 2: not a bench log line: a rule is given for an ending that isn't refused",
                path.display()
            ),
            "{ending}"
        );
    }
}

/// Any said.
fn any_said() -> impl Strategy<Value = Said> {
    prop_oneof![
        Just(Said::Yes),
        Just(Said::No),
        Just(Said::NoAnswer),
        Just(Said::Skipped),
    ]
}

/// Any ending, with any of the gate's rules for a refused one.
fn any_ending() -> impl Strategy<Value = Ending> {
    prop_oneof![
        Just(Ending::Applied),
        Just(Ending::Rejected),
        proptest::sample::select(REFUSALS.to_vec()).prop_map(Ending::Refused),
    ]
}

/// Any candidate: any chunk, span, and window and term of any text
/// (quotes, backslashes, line breaks and all of Unicode).
fn any_key() -> impl Strategy<Value = Key> {
    (
        any::<u32>(),
        any::<u64>(),
        any::<(u32, u32)>(),
        any::<String>(),
        any::<String>(),
    )
        .prop_map(|(track, start, (a, b), window, term)| Key {
            chunk: ChunkKey {
                track: TrackId::new(track),
                start: SampleIndex::new(start),
            },
            offered: WordSpan::new(a.min(b), a.max(b)).unwrap(),
            window,
            term,
        })
}

/// Any run's decider's place, verdicts and counts.
fn any_run() -> impl Strategy<Value = EngineRun> {
    (
        proptest::collection::btree_map(
            any_key(),
            (any_said(), any_ending()).prop_map(|(said, ending)| verdict(said, ending)),
            0..20,
        ),
        any::<u32>(),
        any::<u32>(),
        any::<u32>(),
        proptest::option::of(any::<String>()),
        prop_oneof![Just(Place::Local), Just(Place::Cloud)],
    )
        .prop_map(
            |(verdicts, questions, skipped, ms, down, place)| EngineRun {
                name: "any".to_owned(),
                by: DecidedBy { place, ..fake_by() },
                verdicts,
                questions: usize::try_from(questions).unwrap(),
                skipped: usize::try_from(skipped).unwrap(),
                took: Duration::from_millis(u64::from(ms)),
                down,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Any run's log reads back with the same decider, verdicts and counts.
    #[test]
    fn any_run_reads_back_with_its_verdicts_and_counts(run in any_run()) {
        let dir = TestDir::new("props");
        let candidates = Candidates {
            source: "any \"source\" \\ ü".to_owned(),
            offering: Vec::new(),
            screen: Screen::new(Precheck::Off, &[]),
            precheck: Precheck::Off,
            saved: None,
        };
        let path = write(&dir.0, STAMP, &run, &candidates, None).unwrap();
        let log = read(&path).unwrap();
        prop_assert_eq!(
            (&log.by, &log.verdicts, log.questions, log.skipped, log.took, &log.down),
            (&run.by, &run.verdicts, run.questions, run.skipped, run.took, &run.down)
        );
        prop_assert_eq!(log.source, candidates.source);
    }
}
