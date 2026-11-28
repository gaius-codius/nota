//! Each engine's bench run, logged to a file of its own, and two logs
//! compared (`nota bench compare OLD NEW`): the candidates whose answer or
//! outcome changed between them.
//!
//! **A log** is JSON lines, `<stamp>-<engine>.jsonl` in the log folder.
//! The first line is the run's header: `nota_bench_log` (the format, 1),
//! `engine`, `decider`, `model`, `place` (`local` or `cloud`), `source`,
//! `precheck` (`on` or `off`), `started` (Unix seconds, or null),
//! `questions`, `skipped`, `took_ms` and `down` (why it stopped early, or
//! null). Then one line per verdict, in key order: `track`, `start`,
//! `first`, `last`, `window`, `term`, `said` (`yes`, `no`, `no-answer` or
//! `skipped`), `ending` (`applied`, `rejected` or `refused`) and `rule`
//! (the gate's rule for a refused yes, null otherwise).
//!
//! A log is never overwritten: a second run in the same second with the
//! same engine fails to write its log, and says so.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use nota_core::decide::Place;
use nota_core::{SampleIndex, TrackId, WallTime};
use nota_store::{ChunkKey, DecidedBy, Refusal, WordSpan};
use serde_json::{Map, Value, json};

use super::{Candidates, Ending, EngineRun, Key, Said, Verdict, place, plain, read_text};
use crate::cleanup::Precheck;

/// The log format's number, in each header's `nota_bench_log`.
const FORMAT: u64 = 1;

/// The largest log read, in bytes (64 MiB): a session offers a few
/// thousand candidates at most, each a line of well under 1 KiB, so this
/// is far more than any run writes.
const MAX_LOG: u64 = 64 << 20;

/// A log read back: the run's header and its verdicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Log {
    /// The engine's name in the report.
    pub(crate) engine: String,
    /// What answered.
    pub(crate) by: DecidedBy,
    /// Where the candidates came from: a session or a fixture.
    pub(crate) source: String,
    /// Whether C3 ran.
    pub(crate) precheck: Precheck,
    /// When the bench started, by the wall clock, if it was known.
    pub(crate) started: Option<WallTime>,
    /// The questions it was asked.
    pub(crate) questions: usize,
    /// The candidates C3 skipped.
    pub(crate) skipped: usize,
    /// How long it took, to the millisecond.
    pub(crate) took: Duration,
    /// Why it stopped before the end, if it did.
    pub(crate) down: Option<String>,
    /// Its verdict on each candidate it got to.
    pub(crate) verdicts: BTreeMap<Key, Verdict>,
}

/// A candidate whose verdict differs between two logs, or that only one
/// of them has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    /// The candidate.
    pub(crate) key: Key,
    /// The older log's verdict; none if it didn't get to it.
    pub(crate) old: Option<Verdict>,
    /// The newer log's verdict; none if it didn't get to it.
    pub(crate) new: Option<Verdict>,
}

/// Writes `run`'s log over `candidates`, begun at `started`, to the new
/// file `<stamp>-<engine>.jsonl` in `dir` (made if missing), and gives
/// its path.
///
/// # Errors
///
/// The reason, for the user, if the folder can't be made or the file is
/// there already or can't be written.
pub(crate) fn write(
    dir: &Path,
    stamp: &str,
    run: &EngineRun,
    candidates: &Candidates,
    started: Option<WallTime>,
) -> Result<PathBuf, String> {
    let path = dir.join(format!("{stamp}-{}.jsonl", run.name));
    let mut text = header(run, candidates, started).to_string();
    text.push('\n');
    for (key, verdict) in &run.verdicts {
        text.push_str(&verdict_line(key, *verdict).to_string());
        text.push('\n');
    }
    let shown = path.display();
    make_folder(dir)?;
    let mut file = create_file(&path).map_err(|e| format!("the log {shown} can't be made: {e}"))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| format!("the log {shown} can't be written: {e}"))?;
    Ok(path)
}

/// Makes the log folder `dir` if it's missing.
///
/// # Errors
///
/// The reason, for the user, if it can't be made.
pub(crate) fn make_folder(dir: &Path) -> Result<(), String> {
    create_dir(dir).map_err(|e| format!("the log folder {} can't be made: {e}", dir.display()))
}

/// Makes `dir` and any folder above it that's missing.
#[expect(
    clippy::disallowed_methods,
    reason = "the bench's development logs aren't recordings or the library, \
              so their folder doesn't go through the durable-write layer"
)]
fn create_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Makes the new file `path`, refusing one that's there already. It holds
/// words from a transcript, so where the system has file modes it's
/// readable by its owner only, as the library is.
#[expect(
    clippy::disallowed_methods,
    reason = "a development log the bench writes is not a recording or the library, \
              so it doesn't go through the durable-write layer"
)]
fn create_file(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// The log's first line: what ran, over what, and how it went.
fn header(run: &EngineRun, candidates: &Candidates, started: Option<WallTime>) -> Value {
    json!({
        "nota_bench_log": FORMAT,
        "engine": run.name,
        "decider": run.by.name,
        "model": run.by.model,
        "place": place_name(run.by.place),
        "source": candidates.source,
        "precheck": precheck_name(candidates.precheck),
        "started": started.map(WallTime::unix_seconds),
        "questions": run.questions,
        "skipped": run.skipped,
        "took_ms": u64::try_from(run.took.as_millis()).unwrap_or(u64::MAX),
        "down": run.down,
    })
}

/// A verdict's line: the candidate, what was said and what came of it.
fn verdict_line(key: &Key, verdict: Verdict) -> Value {
    let (ending, rule) = match verdict.ending {
        Ending::Applied => ("applied", None),
        Ending::Rejected => ("rejected", None),
        Ending::Refused(rule) => ("refused", Some(rule.as_str())),
    };
    json!({
        "track": key.chunk.track.get(),
        "start": key.chunk.start.get(),
        "first": key.offered.first(),
        "last": key.offered.last(),
        "window": key.window,
        "term": key.term,
        "said": said_name(verdict.said),
        "ending": ending,
        "rule": rule,
    })
}

/// A place's name in a log.
const fn place_name(place: Place) -> &'static str {
    match place {
        Place::Local => "local",
        Place::Cloud => "cloud",
    }
}

/// Whether C3 ran, as a log says it.
const fn precheck_name(precheck: Precheck) -> &'static str {
    match precheck {
        Precheck::Off => "off",
        Precheck::On => "on",
    }
}

/// What was said, as a log names it: one word each, unlike the report's
/// labels.
const fn said_name(said: Said) -> &'static str {
    match said {
        Said::Yes => "yes",
        Said::No => "no",
        Said::NoAnswer => "no-answer",
        Said::Skipped => "skipped",
    }
}

/// Every rule the gate refuses by, which a log's `rule` may name.
pub(super) const REFUSALS: [Refusal; 7] = [
    Refusal::EmptySpan,
    Refusal::SpanNotFound,
    Refusal::TooLong,
    Refusal::Dissimilar,
    Refusal::NotOffered,
    Refusal::WindowElsewhere,
    Refusal::AlreadyChanged,
];

/// A rule nota-store adds is a build error here until [`REFUSALS`] lists
/// it, so a log never holds a rule it can't read back: the match must
/// name every rule, whichever one it's given.
const _: () = match Refusal::EmptySpan {
    Refusal::EmptySpan
    | Refusal::SpanNotFound
    | Refusal::TooLong
    | Refusal::Dissimilar
    | Refusal::NotOffered
    | Refusal::WindowElsewhere
    | Refusal::AlreadyChanged => {}
};

/// The log at `path`.
///
/// # Errors
///
/// The reason, naming the file and the line, if it can't be read, is
/// over [`MAX_LOG`] bytes, or isn't a bench log.
pub(crate) fn read(path: &Path) -> Result<Log, String> {
    let shown = path.display();
    let text = read_text(path, MAX_LOG)?;
    let mut lines = text.lines().zip(1_usize..);
    let Some((first, _)) = lines.next() else {
        return Err(format!("{shown}: empty: not a bench log"));
    };
    let mut log = object(first)
        .and_then(|o| parse_header(&o))
        .map_err(|e| format!("{shown}: line 1: not a bench log header: {e}"))?;
    for (line, n) in lines {
        let (key, verdict) = object(line)
            .and_then(|o| parse_verdict(&o))
            .map_err(|e| format!("{shown}: line {n}: not a bench log line: {e}"))?;
        if log.verdicts.insert(key, verdict).is_some() {
            return Err(format!(
                "{shown}: line {n}: not a bench log line: its candidate is there already"
            ));
        }
    }
    Ok(log)
}

/// `line` as a JSON object.
fn object(line: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str(line) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err("not an object".to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

/// A header's fields, with no verdicts yet.
fn parse_header(o: &Map<String, Value>) -> Result<Log, String> {
    if o.get("nota_bench_log").and_then(Value::as_u64) != Some(FORMAT) {
        return Err(format!("nota_bench_log isn't {FORMAT}"));
    }
    let place = match text(o, "place")? {
        "local" => Place::Local,
        "cloud" => Place::Cloud,
        _ => return Err("place isn't local or cloud".to_owned()),
    };
    let precheck = match text(o, "precheck")? {
        "off" => Precheck::Off,
        "on" => Precheck::On,
        _ => return Err("precheck isn't on or off".to_owned()),
    };
    let started = match o.get("started") {
        Some(Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .and_then(WallTime::from_unix_seconds)
                .ok_or("started isn't a time in Unix seconds")?,
        ),
        None => return Err("started is missing".to_owned()),
    };
    Ok(Log {
        engine: text(o, "engine")?.to_owned(),
        by: DecidedBy {
            name: text(o, "decider")?.to_owned(),
            model: text(o, "model")?.to_owned(),
            place,
        },
        source: text(o, "source")?.to_owned(),
        precheck,
        started,
        questions: count(o, "questions")?,
        skipped: count(o, "skipped")?,
        took: Duration::from_millis(number(o, "took_ms")?),
        down: optional_text(o, "down")?,
        verdicts: BTreeMap::new(),
    })
}

/// A verdict line's candidate and verdict.
fn parse_verdict(o: &Map<String, Value>) -> Result<(Key, Verdict), String> {
    let track = u32::try_from(number(o, "track")?).map_err(|_| "track is too large")?;
    let first = u32::try_from(number(o, "first")?).map_err(|_| "first is too large")?;
    let last = u32::try_from(number(o, "last")?).map_err(|_| "last is too large")?;
    let key = Key {
        chunk: ChunkKey {
            track: TrackId::new(track),
            start: SampleIndex::new(number(o, "start")?),
        },
        offered: WordSpan::new(first, last).ok_or("first is after last")?,
        window: text(o, "window")?.to_owned(),
        term: text(o, "term")?.to_owned(),
    };
    let said = match text(o, "said")? {
        "yes" => Said::Yes,
        "no" => Said::No,
        "no-answer" => Said::NoAnswer,
        "skipped" => Said::Skipped,
        _ => return Err("said isn't yes, no, no-answer or skipped".to_owned()),
    };
    let rule = optional_text(o, "rule")?;
    let ending = match (text(o, "ending")?, rule.as_deref()) {
        ("applied", None) => Ending::Applied,
        ("rejected", None) => Ending::Rejected,
        ("refused", Some(rule)) => Ending::Refused(refusal(rule)?),
        ("applied" | "rejected", Some(_)) => {
            return Err("a rule is given for an ending that isn't refused".to_owned());
        }
        ("refused", None) => return Err("a refused ending has no rule".to_owned()),
        _ => return Err("ending isn't applied, rejected or refused".to_owned()),
    };
    Ok((key, Verdict { said, ending }))
}

/// The gate's rule named `name`.
fn refusal(name: &str) -> Result<Refusal, String> {
    REFUSALS
        .into_iter()
        .find(|rule| rule.as_str() == name)
        .ok_or_else(|| format!("{name} isn't one of the gate's rules"))
}

/// The text field `key` of `o`.
fn text<'o>(o: &'o Map<String, Value>, key: &str) -> Result<&'o str, String> {
    o.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} isn't text"))
}

/// The field `key` of `o`, text or null.
fn optional_text(o: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match o.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        _ => Err(format!("{key} isn't text or null")),
    }
}

/// The whole-number field `key` of `o`.
fn number(o: &Map<String, Value>, key: &str) -> Result<u64, String> {
    o.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{key} isn't a whole number"))
}

/// The count field `key` of `o`.
fn count(o: &Map<String, Value>, key: &str) -> Result<usize, String> {
    usize::try_from(number(o, key)?).map_err(|_| format!("{key} is too large"))
}

/// The candidates whose verdict differs between `old` and `new`, or that
/// only one of them has, in key order.
pub(crate) fn changed(old: &Log, new: &Log) -> Vec<Change> {
    let keys: std::collections::BTreeSet<&Key> =
        old.verdicts.keys().chain(new.verdicts.keys()).collect();
    keys.into_iter()
        .filter_map(|key| {
            let (old, new) = (
                old.verdicts.get(key).copied(),
                new.verdicts.get(key).copied(),
            );
            (old != new).then(|| Change {
                key: key.clone(),
                old,
                new,
            })
        })
        .collect()
}

/// Reads the logs `old` and `new` and writes to `out` what changed
/// between them: a line naming each log, then one line per change.
///
/// # Errors
///
/// The reason, for the user, if either log can't be read or `out` can't
/// be written to.
pub(crate) fn compare(old: &Path, new: &Path, out: &mut dyn io::Write) -> Result<(), String> {
    let (old_log, new_log) = (read(old)?, read(new)?);
    let changes = changed(&old_log, &new_log);
    write_changes(&[(old, &old_log), (new, &new_log)], &changes, out)
        .map_err(|e| format!("couldn't write to stdout: {e}"))
}

/// Writes the comparison of `logs` (old, then new), whose `changes` are
/// given, to `out`.
fn write_changes(
    logs: &[(&Path, &Log); 2],
    changes: &[Change],
    out: &mut dyn io::Write,
) -> io::Result<()> {
    writeln!(out, "nota bench compare")?;
    for (age, (path, log)) in ["old", "new"].iter().zip(logs) {
        writeln!(
            out,
            "  {age}: {}, {} ({})",
            plain(&log.engine),
            plain(&log.source),
            path.display()
        )?;
    }
    if changes.is_empty() {
        return writeln!(out, "No answers changed.");
    }
    writeln!(out, "Changed answers ({})", changes.len())?;
    for change in changes {
        writeln!(out, "  {}", change_line(change))?;
    }
    Ok(())
}

/// A change's line: where the candidate is, what was heard and offered,
/// and its verdict in each log.
fn change_line(change: &Change) -> String {
    let shown = |v: Option<Verdict>| v.map_or_else(|| "not asked".to_owned(), |v| v.to_string());
    format!(
        "{} \"{}\" → {}: {} → {}",
        place(change.key.chunk),
        plain(&change.key.window),
        plain(&change.key.term),
        shown(change.old),
        shown(change.new)
    )
}
