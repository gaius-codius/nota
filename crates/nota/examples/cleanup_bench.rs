//! Readies a library of bench recordings for term clean-up, and reads back
//! what clean-up made of them, for the combined clean-up test (#168).
//!
//! Usage: `cargo run -p nota --example cleanup_bench -- COMMAND DATA [ARG]`,
//! where DATA is a nota data directory whose sessions were recorded by
//! `nota record` and not yet processed:
//!
//! - `attach DATA PACK`: records PACK, a pack file, as the chosen pack of
//!   every session that has chosen none. Setup can't choose packs yet, so
//!   this is the one way a recorded session gets its terms. The pack must
//!   also be installed in DATA's `packs/` folder for clean-up to read it.
//! - `lookup-only DATA`: ends every hotword pass not yet run as failed, so
//!   clean-up runs with the lookup's candidates alone, as it does after a
//!   failed pass.
//! - `status DATA`: one line per job, `SESSION KIND STATE`, and exits 0
//!   once every clean-up job has ended, 1 while any is still to run.
//! - `export DATA`: one JSON object per session: its final text (revision
//!   0) and its newest revision chunk by chunk, the hotword pass's texts
//!   with their boost, every proposal with its decision and outcome, and
//!   its jobs.
//!
//! Only the library database is read or written, through `nota-store`.
//! The export holds the sessions' text: send it to a private bench
//! directory, never into the repository.

use std::error::Error;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nota_core::SessionId;
use nota_store::{
    FinalLine, JobEnd, JobKind, JobState, Outcome, Pack, Proposal, RevisionNumber, Store,
};
use serde_json::{Value, json};

/// Any error, said plainly by `main`.
type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// The library database's name in a data directory, as the app names it.
const LIBRARY_DB: &str = "library.db";

/// Why `lookup-only` ends a hotword pass.
const LOOKUP_ONLY: &str = "the bench runs clean-up with the lookup alone";

/// Runs the command the arguments name.
fn main() -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [command, data, rest @ ..] = &args[..] else {
        return Err(usage());
    };
    let db = PathBuf::from(data).join(LIBRARY_DB);
    if !db.is_file() {
        return Err(format!("there's no library at {}", db.display()).into());
    }
    match (command.as_str(), rest) {
        ("attach", [pack]) => attach(&db, Path::new(pack)).map(|()| ExitCode::SUCCESS),
        ("lookup-only", []) => lookup_only(&db).map(|()| ExitCode::SUCCESS),
        ("status", []) => status(&db),
        ("export", []) => export(&db).map(|()| ExitCode::SUCCESS),
        _ => Err(usage()),
    }
}

/// The usage line, as an error.
fn usage() -> Box<dyn Error + Send + Sync> {
    "usage: cleanup_bench attach DATA PACK | lookup-only DATA | status DATA | export DATA".into()
}

/// Records `pack` as the chosen pack of each session that chose none.
fn attach(db: &Path, pack: &Path) -> Result<()> {
    let pack = Pack::open(pack)?;
    let mut store = Store::open(db)?;
    let mut out = io::stdout().lock();
    for session in store.sessions()? {
        if store.session_packs(session.id)?.is_empty() {
            store.record_session_packs(session.id, &[pack.pack_ref()])?;
            writeln!(
                out,
                "session {} chose {}",
                session.id.get(),
                pack.pack_ref().name()
            )?;
        }
    }
    Ok(())
}

/// Ends each hotword pass that hasn't ended as failed.
fn lookup_only(db: &Path) -> Result<()> {
    let mut store = Store::open(db)?;
    let mut out = io::stdout().lock();
    for job in store.jobs()? {
        if job.kind == JobKind::HotwordPass && !ended(&job.state) {
            store.end_job(job.id, &JobEnd::Failed(LOOKUP_ONLY.to_owned()))?;
            writeln!(out, "session {}: hotword pass ended", job.session.get())?;
        }
    }
    Ok(())
}

/// Prints every job, and says whether every clean-up has ended.
fn status(db: &Path) -> Result<ExitCode> {
    let store = Store::open_as_is(db)?;
    let mut out = io::stdout().lock();
    let mut all_ended = true;
    for job in store.jobs()? {
        writeln!(out, "{} {:?} {:?}", job.session.get(), job.kind, job.state)?;
        if job.kind == JobKind::CleanUp && !ended(&job.state) {
            all_ended = false;
        }
    }
    Ok(if all_ended {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Whether a job in `state` has ended for good.
const fn ended(state: &JobState) -> bool {
    matches!(state, JobState::Done | JobState::Failed(_))
}

/// Prints each session as one JSON object.
fn export(db: &Path) -> Result<()> {
    let store = Store::open_as_is(db)?;
    let mut out = io::stdout().lock();
    for session in store.sessions()? {
        let object = session_json(&store, session.id, session.title.as_deref())?;
        writeln!(out, "{object}")?;
    }
    Ok(())
}

/// One session's texts, proposals and jobs.
fn session_json(store: &Store, id: SessionId, title: Option<&str>) -> Result<Value> {
    let revisions = store.revisions(id)?;
    let newest = revisions.last().copied().unwrap_or(RevisionNumber::FINAL);
    let first = lines_json(&store.revision(id, RevisionNumber::FINAL)?);
    let last = lines_json(&store.revision(id, newest)?);
    let hotword: Vec<Value> = store
        .hotword_texts(id)?
        .iter()
        .map(|h| {
            json!({
                "track": h.track.get(),
                "start": h.range.start().get(),
                "text": h.text,
                "boost": h.boost,
            })
        })
        .collect();
    let proposals: Vec<Value> = store.proposals(id)?.iter().map(proposal_json).collect();
    let jobs: Vec<Value> = store
        .session_jobs(id)?
        .iter()
        .map(|j| json!({"kind": format!("{:?}", j.kind), "state": format!("{:?}", j.state)}))
        .collect();
    Ok(json!({
        "session": id.get(),
        "title": title,
        "revisions": revisions.len(),
        "final": first,
        "newest": last,
        "hotword": hotword,
        "proposals": proposals,
        "jobs": jobs,
    }))
}

/// A revision's chunks: each one's track, first sample and text.
fn lines_json(lines: &[FinalLine]) -> Vec<Value> {
    lines
        .iter()
        .map(|line| {
            json!({
                "track": line.heard.track.get(),
                "start": line.heard.range.start().get(),
                "text": line.text,
            })
        })
        .collect()
}

/// A proposal's candidate, decision and outcome.
fn proposal_json(p: &Proposal) -> Value {
    let outcome = match p.outcome {
        Outcome::Applied(_) => "applied",
        Outcome::Rejected => "rejected",
        Outcome::ToRank => "to_rank",
        Outcome::Card => "card",
        Outcome::OverCap => "over_cap",
    };
    json!({
        "track": p.proposed.chunk.track.get(),
        "chunk": p.proposed.chunk.start.get(),
        "heard": p.proposed.heard,
        "term": p.proposed.term,
        "replacement": p.proposed.replacement,
        "found": format!("{:?}", p.proposed.found),
        "decided": format!("{:?}", p.decided),
        "outcome": outcome,
    })
}
