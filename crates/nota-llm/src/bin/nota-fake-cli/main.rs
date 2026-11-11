//! A stand-in for `claude -p` and `codex exec`, for the cloud deciders'
//! tests. It's Claude when its first argument is `-p`, and Codex
//! otherwise.
//!
//! It works in the folder it's started in (the decider's work folder):
//! - it writes its arguments to `args` there, one per line, its stdin to
//!   `stdin`, and its process id to `pid`;
//! - `plan` there says what it does:
//!   - `answer`: answers yes to odd candidates and no to even ones
//!   - `malformed`: answers with a key given twice, the key being the
//!     question's current chunk; as Codex, with the question and then
//!     text that isn't JSON
//!   - `error`: as Claude, a result that says it failed with status 429
//!     and quotes the question; as Codex, the question on stderr and exit
//!     status 1
//!   - `fail`: the question on stdout and stderr, and exit status 3
//!   - `hang`: the question on stderr, then waits forever
//!   - `huge`: more on stdout than nota reads
//!
//! Every way it fails puts the question's words where a careless reason
//! would pick them up, so the tests can show none does.
//!
//! On Unix it ends within a second of its parent's death, so a test that's
//! killed (nextest stopping a hang) leaves no CLI running.

use std::io::{Read, Write};
use std::process::ExitCode;

use serde_json::{Value, json};

#[cfg(unix)]
// The stand-in server's watch, shared rather than copied.
#[path = "../nota-fake-llama-server/parent_watch.rs"]
mod parent_watch;

fn main() -> ExitCode {
    #[cfg(unix)]
    if let Err(e) = parent_watch::end_with_parent() {
        let _ = writeln!(
            std::io::stderr(),
            "nota-fake-cli: couldn't watch its parent: {e}"
        );
        return ExitCode::from(2);
    }
    match run() {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "nota-fake-cli: {e}");
            ExitCode::from(2)
        }
    }
}

/// Does what the plan says.
#[expect(
    clippy::disallowed_methods,
    reason = "a test stand-in's notes for its test, not a recording"
)]
fn run() -> std::io::Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::fs::write("args", args.join("\n"))?;
    // Whole or not at all, for a test that waits for it to appear.
    std::fs::write("pid.tmp", std::process::id().to_string())?;
    std::fs::rename("pid.tmp", "pid")?;
    let mut question = String::new();
    std::io::stdin().read_to_string(&mut question)?;
    std::fs::write("stdin", &question)?;
    let claude = args.first().is_some_and(|a| a == "-p");
    let plan = std::fs::read_to_string("plan")?;
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    match plan.trim() {
        "answer" => {
            let answer = answer(&question);
            writeln!(out, "{}", if claude { result(&answer) } else { answer })?;
        }
        "malformed" if claude => {
            let chunk = serde_json::to_string(&current(&question))?;
            let twice = format!(r#"{{"decisions":[],{chunk}:1,{chunk}:2}}"#);
            writeln!(
                out,
                r#"{{"type":"result","is_error":false,"structured_output":{twice}}}"#
            )?;
        }
        "malformed" => writeln!(out, "{question} {{\"decisions\": oops")?,
        "error" if claude => {
            let failed = json!({
                "type": "result", "subtype": "success", "is_error": true,
                "api_error_status": 429, "result": question,
            });
            writeln!(out, "{failed}")?;
            return Ok(ExitCode::from(1));
        }
        "error" => {
            writeln!(err, "{question}")?;
            return Ok(ExitCode::from(1));
        }
        "fail" => {
            writeln!(out, "{question}")?;
            writeln!(err, "{question}")?;
            return Ok(ExitCode::from(3));
        }
        "hang" => {
            writeln!(err, "{question}")?;
            loop {
                std::thread::park();
            }
        }
        "huge" => out.write_all(&vec![b' '; 2 << 20])?,
        other => {
            writeln!(err, "no such plan: {other}")?;
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The answer to `question`: yes to odd candidates, no to even ones.
fn answer(question: &str) -> Value {
    let candidates = question
        .lines()
        .filter(|line| {
            line.split_once(". heard ")
                .is_some_and(|(no, _)| no.parse::<u32>().is_ok())
        })
        .count();
    let decisions: Vec<Value> = (1..=candidates)
        .map(|id| json!({"id": id, "apply": id % 2 == 1}))
        .collect();
    json!({ "decisions": decisions })
}

/// `answer` as Claude's JSON output gives it.
fn result(answer: &Value) -> Value {
    json!({
        "type": "result", "subtype": "success", "is_error": false,
        "result": answer.to_string(), "structured_output": answer,
    })
}

/// The question's current chunk: the line after `CURRENT CHUNK:`.
fn current(question: &str) -> String {
    let mut lines = question.lines();
    lines.by_ref().find(|line| *line == "CURRENT CHUNK:");
    lines.next().unwrap_or_default().to_owned()
}
