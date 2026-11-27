//! `nota bench --engines local,claude,codex`: the same candidates through
//! several deciders, side by side, for development. Cloud models answer
//! fast, so they're the quick engine to iterate with; a local run confirms
//! a result before it ships.
//!
//! **The same candidates, prompts, gate and checks as clean-up.** The
//! candidates are those the lookup offers in a session's final text (with
//! its packs, as a clean-up run reads them) or in a fixture's chunks
//! ([`fixture`]). Every candidate is asked about, including those a
//! clean-up run already decided: the bench compares deciders, not runs.
//! Each engine's answers go through C3 (when `--precheck` turns it on) and
//! the gate as clean-up's do ([`crate::cleanup::settle_chunk`]), but
//! nothing is ranked into cards: a yes the gate refused is reported as
//! refused, which in a run would be ranked for a card.
//!
//! **It never writes to the session.** No proposal, revision or job is
//! stored; the library is only read (nota-store brings an older schema up
//! to date as it opens, as the app would).
//!
//! **The report** ([`report`]) states the diagnosis rule from the
//! 2026-09-30 bench, then each engine's questions, time and cost (where
//! it's known: none of the deciders reports one yet), the candidates
//! whose answer or outcome differs between engines, and every candidate's.
//! **Each engine's run is logged** to a file of its own ([`log`]), so a
//! later run can be compared with it (`nota bench compare OLD NEW`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read as _};
use std::path::Path;
use std::time::Duration;

use nota_checks::Skipped;
use nota_core::decide::{Decider, Place};
use nota_core::{Clock, SampleCount, SampleRate};
use nota_store::{ChunkKey, Decided, DecidedBy, JobEnd, Refusal, WordSpan};

use crate::cleanup::chunk::{Chunk, Context, Offer};
use crate::cleanup::{Ask, Offering, Precheck, Saved, Screen, Settled, settle_chunk, thresholds};

mod command;
pub(crate) mod fixture;
pub(crate) mod log;
mod session;

pub(crate) use command::{BenchCommand, dispatch, parse};
pub(crate) use session::from_session;

#[cfg(test)]
mod tests;

/// The diagnosis rule from the 2026-09-30 bench, which every report
/// opens with.
pub(crate) const DIAGNOSIS: &str = "If the cloud models succeed where the local model \
     fails, the limit is the model; if they all fail, the problem is in nota's design or code.";

/// A decider the bench can run, by the name `--engines` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EngineName {
    /// The local decider the settings describe (`qwen` names it too: it's
    /// the model nota ships with).
    Local,
    /// Claude, through the user's `claude -p`.
    Claude,
    /// Codex, through the user's `codex exec`.
    Codex,
}

impl EngineName {
    /// The engine `--engines` names `text`, if it's one.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "local" | "qwen" => Some(Self::Local),
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// Its name in reports and log files.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// A decider to run, with what it records as having answered.
pub(crate) struct Engine {
    /// Its name in the report.
    pub(crate) name: String,
    /// The decider.
    pub(crate) decider: Box<dyn Decider + Send>,
    /// What answered: its program, model and place.
    pub(crate) by: DecidedBy,
}

/// The candidates every engine is asked about.
pub(crate) struct Candidates {
    /// Where they came from, for the report: a session or a fixture.
    pub(crate) source: String,
    /// The chunks in order, each with every candidate the lookup offers.
    pub(crate) offering: Vec<Offering>,
    /// Whether C3 sorts them before each question, and its low band.
    pub(crate) screen: Screen,
    /// Whether C3 runs: what the proposals' thresholds record.
    pub(crate) precheck: Precheck,
    /// What C3 saved the session's own clean-up runs so far, for a
    /// session; none for a fixture.
    pub(crate) saved: Option<Saved>,
}

impl Candidates {
    /// How many candidates there are, in how many chunks with one.
    fn counts(&self) -> (usize, usize) {
        let with: Vec<usize> = self
            .offering
            .iter()
            .map(|(_, offered)| offered.len())
            .filter(|n| *n > 0)
            .collect();
        (with.iter().sum(), with.len())
    }
}

/// A candidate, as the bench and its logs name it: the chunk, the words
/// it was offered for, the heard window and the term.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Key {
    /// The chunk.
    pub(crate) chunk: ChunkKey,
    /// The chunk's words the lookup offered it for.
    pub(crate) offered: WordSpan,
    /// The heard words.
    pub(crate) window: String,
    /// The term.
    pub(crate) term: String,
}

impl Key {
    /// The key of `offer` in `chunk`.
    fn of(chunk: &Chunk, offer: &Offer) -> Self {
        Self {
            chunk: chunk.key(),
            offered: offer.words,
            window: offer.window.clone(),
            term: offer.term.clone(),
        }
    }
}

/// What a decider said on a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Said {
    /// Yes.
    Yes,
    /// No.
    No,
    /// No answer that could be read, or a question that couldn't be put.
    NoAnswer,
    /// Not asked: C3 skipped it.
    Skipped,
}

impl Said {
    /// Its name in reports and logs.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::NoAnswer => "no answer",
            Self::Skipped => "skipped",
        }
    }
}

/// What came of an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    /// The gate let the yes through and it changes the text.
    Applied,
    /// The gate refused the yes, by this rule: in a run, ranked for a
    /// card.
    Refused(Refusal),
    /// Rejected and logged: a no, no answer, a skip, or a yes that would
    /// change nothing or words already held.
    Rejected,
}

/// One engine's answer on one candidate and what came of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Verdict {
    /// What it said.
    pub(crate) said: Said,
    /// What came of it.
    pub(crate) ending: Ending,
}

impl Verdict {
    /// The verdict a proposal's decision records.
    fn of(decided: &Decided) -> Self {
        let (said, ending) = match decided {
            Decided::Applied { .. } => (Said::Yes, Ending::Applied),
            Decided::Unchanged { .. } | Decided::Covered { .. } => (Said::Yes, Ending::Rejected),
            Decided::Refused { rule, .. } => (Said::Yes, Ending::Refused(*rule)),
            Decided::No { .. } => (Said::No, Ending::Rejected),
            // The bench asks once and never gives up, but a stored decision may.
            Decided::NoAnswer(_) | Decided::GaveUp(_) => (Said::NoAnswer, Ending::Rejected),
            Decided::Skipped { .. } => (Said::Skipped, Ending::Rejected),
        };
        Self { said, ending }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ending {
            Ending::Applied => write!(f, "{} · applied", self.said.label()),
            Ending::Refused(rule) => {
                write!(f, "{} · refused ({})", self.said.label(), rule.as_str())
            }
            Ending::Rejected => write!(f, "{} · rejected", self.said.label()),
        }
    }
}

/// How one engine's run went.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EngineRun {
    /// Its name in the report.
    pub(crate) name: String,
    /// What answered.
    pub(crate) by: DecidedBy,
    /// Its verdict on each candidate it got to.
    pub(crate) verdicts: BTreeMap<Key, Verdict>,
    /// The questions it was asked.
    pub(crate) questions: usize,
    /// The candidates C3 skipped, so it wasn't asked about.
    pub(crate) skipped: usize,
    /// How long it took, by the session clock.
    pub(crate) took: Duration,
    /// Why it stopped before the end, if it did: it couldn't be run.
    pub(crate) down: Option<String>,
}

/// Runs each of `engines` in turn over `candidates`, timed by `clock`,
/// until `stop` says to stop.
pub(crate) fn run(
    candidates: &Candidates,
    engines: Vec<Engine>,
    clock: &dyn Clock,
    stop: &dyn Fn() -> bool,
) -> Vec<EngineRun> {
    engines
        .into_iter()
        .map(|engine| run_one(candidates, engine, clock, stop))
        .collect()
}

/// Runs `engine` over `candidates`, chunk by chunk.
fn run_one(
    candidates: &Candidates,
    mut engine: Engine,
    clock: &dyn Clock,
    stop: &dyn Fn() -> bool,
) -> EngineRun {
    let context = Context {
        decided_by: engine.by.clone(),
        thresholds: thresholds(candidates.precheck),
    };
    let started = clock.now();
    let mut out = EngineRun {
        name: engine.name.clone(),
        by: engine.by.clone(),
        verdicts: BTreeMap::new(),
        questions: 0,
        skipped: 0,
        took: Duration::ZERO,
        down: None,
    };
    for (i, (chunk, offered)) in candidates.offering.iter().enumerate() {
        if offered.is_empty() {
            continue;
        }
        let (asked, skipped) = candidates.screen.sort(chunk, offered.clone());
        let previous = i
            .checked_sub(1)
            .and_then(|p| candidates.offering.get(p))
            .map(|(c, _)| c);
        let ask = Ask {
            chunk,
            previous,
            offered,
            asked: &asked,
            skipped: &skipped,
            blocked: &[],
        };
        match settle_chunk(&ask, &mut *engine.decider, stop, &context) {
            Ok(settled) => note(&mut out, chunk, &asked, &skipped, &settled),
            Err(end) => {
                // An error comes only from a question put to the decider.
                out.questions += 1;
                out.down = Some(ended(&end));
                break;
            }
        }
    }
    out.took = clock
        .now()
        .checked_duration_since(started)
        .unwrap_or_default();
    out
}

/// Notes in `out` what `settled` made of `chunk`'s `asked` and `skipped`
/// candidates, in the order it gives them.
fn note(
    out: &mut EngineRun,
    chunk: &Chunk,
    asked: &[Offer],
    skipped: &[(Offer, Skipped)],
    settled: &Settled,
) {
    out.questions += usize::from(settled.asked);
    out.skipped += skipped.len();
    let offers = asked.iter().chain(skipped.iter().map(|(o, _)| o));
    for (offer, p) in offers.zip(&settled.proposals) {
        out.verdicts
            .insert(Key::of(chunk, offer), Verdict::of(&p.decided));
    }
}

/// Why an engine's run ended early, for the report.
fn ended(end: &JobEnd) -> String {
    match end {
        JobEnd::EngineDown(why) => why.clone(),
        JobEnd::Waiting(_) => "stopped".to_owned(),
        other => format!("{other:?}"),
    }
}

/// The candidates on which `runs` don't all give the same verdict, or
/// some engine gave none: each with every engine's verdict, in `runs`'
/// order.
pub(crate) fn differences(runs: &[EngineRun]) -> Vec<(Key, Vec<Option<Verdict>>)> {
    let keys: BTreeSet<&Key> = runs.iter().flat_map(|r| r.verdicts.keys()).collect();
    keys.into_iter()
        .filter_map(|key| {
            let row: Vec<Option<Verdict>> =
                runs.iter().map(|r| r.verdicts.get(key).copied()).collect();
            let first = row.first().copied().flatten();
            row.iter()
                .any(|v| *v != first || v.is_none())
                .then(|| (key.clone(), row))
        })
        .collect()
}

/// Writes the report on `runs` over `candidates` to `out`.
///
/// # Errors
///
/// The error writing to `out`.
pub(crate) fn report(
    candidates: &Candidates,
    runs: &[EngineRun],
    out: &mut dyn io::Write,
) -> io::Result<()> {
    let (n, chunks) = candidates.counts();
    let precheck = match candidates.precheck {
        Precheck::Off => "off",
        Precheck::On => "on",
    };
    writeln!(
        out,
        "nota bench: {}, {n} candidates in {chunks} chunks, C3 {precheck}",
        candidates.source
    )?;
    writeln!(out, "{DIAGNOSIS}")?;
    if let Some(saved) = candidates.saved {
        writeln!(
            out,
            "This session's clean-up runs so far: C3 skipped {} candidates and saved {} questions.",
            saved.candidates, saved.questions
        )?;
    }
    writeln!(out, "\nEngines")?;
    for run in runs {
        writeln!(out, "  {}", engine_line(run))?;
    }
    let differ = differences(runs);
    writeln!(out, "\nDifferences ({} of {n} candidates)", differ.len())?;
    for (key, row) in &differ {
        writeln!(out, "  {}", row_line(runs, key, row))?;
    }
    writeln!(out, "\nEvery candidate")?;
    for (key, row) in every(runs) {
        writeln!(out, "  {}", row_line(runs, &key, &row))?;
    }
    Ok(())
}

/// Every candidate any engine got to, with each engine's verdict.
fn every(runs: &[EngineRun]) -> Vec<(Key, Vec<Option<Verdict>>)> {
    let keys: BTreeSet<&Key> = runs.iter().flat_map(|r| r.verdicts.keys()).collect();
    keys.into_iter()
        .map(|key| {
            let row = runs.iter().map(|r| r.verdicts.get(key).copied()).collect();
            (key.clone(), row)
        })
        .collect()
}

/// An engine's line: what answered, where, its questions, skips, time
/// and cost, and why it stopped early if it did.
fn engine_line(run: &EngineRun) -> String {
    let place = match run.by.place {
        Place::Local => "local",
        Place::Cloud => "cloud",
    };
    let stopped = run
        .down
        .as_ref()
        .map_or_else(String::new, |why| format!("; stopped early: {why}"));
    format!(
        "{}: {} {} ({place}), {} questions, {} skipped by C3, {:.1} s, cost not reported{stopped}",
        run.name,
        run.by.name,
        run.by.model,
        run.questions,
        run.skipped,
        run.took.as_secs_f64()
    )
}

/// A candidate's line: where it is, what was heard and offered, and each
/// engine's verdict.
fn row_line(runs: &[EngineRun], key: &Key, row: &[Option<Verdict>]) -> String {
    let verdicts: Vec<String> = runs
        .iter()
        .zip(row)
        .map(|(run, v)| match v {
            Some(v) => format!("{} {v}", run.name),
            None => format!("{} not asked", run.name),
        })
        .collect();
    format!(
        "{} \"{}\" → {}: {}",
        place(key.chunk),
        plain(&key.window),
        plain(&key.term),
        verdicts.join(" | ")
    )
}

/// `text` with its control characters escaped, so text from a log or a
/// fixture can't start a new line or drive the terminal.
pub(crate) fn plain(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Where `chunk` starts, as `track N at m:ss`, at the final pass's rate.
fn place(chunk: ChunkKey) -> String {
    let at = SampleCount::new(chunk.start.get())
        .duration_at(SampleRate::SPEECH)
        .unwrap_or_default()
        .as_secs();
    format!("track {} at {}:{:02}", chunk.track.get(), at / 60, at % 60)
}

/// The file at `path` as text, if it's UTF-8 and at most `max` bytes: a
/// fixture or a log, read whole.
fn read_text(path: &Path, max: u64) -> Result<String, String> {
    let name = path.display();
    let file = std::fs::File::open(path).map_err(|e| format!("{name} can't be read: {e}"))?;
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{name} can't be read: {e}"))?;
    // The size first: a cut at the limit may fall inside a character.
    if max < bytes.len() as u64 {
        return Err(format!("{name} is too big: over {max} bytes"));
    }
    String::from_utf8(bytes).map_err(|e| format!("{name} isn't UTF-8: {e}"))
}
