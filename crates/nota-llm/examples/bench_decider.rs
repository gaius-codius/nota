//! Puts the research bench's yes/no questions to a real llama-server
//! through the local decider, and reports its answers against the bench's.
//!
//! ```sh
//! cargo run -p nota-llm --example bench_decider -- \
//!     --server ~/.local/opt/llama.cpp/bin/llama-server \
//!     --model ~/models/Qwen3.5-4B-Q4_K_M.gguf --device Vulkan0 \
//!     --questions questions.json --subject "an Australian pharmacy lecture"
//! ```
//!
//! The questions file is a JSON list, one entry per chunk the bench asked
//! about: `previous` (or null), `current`, `candidates` (each `heard`,
//! `term` and `fact`, or null) and `bench` (the bench's answer per
//! candidate: true, false, or null where its reply failed). It holds
//! transcript text, so it's made locally from the bench and never
//! committed. The report gives counts and candidate numbers only.
//!
//! The server runs on the device named (`cpu` for none), with its socket
//! in `$XDG_RUNTIME_DIR/nota-bench`, and is stopped at the end.
//! `--server-flags "-c 8192 -fa on"` adds flags after the decider's own
//! (through `sh`, as a launcher), to compare with the bench's server.

use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nota_core::decide::{Answer, Candidate, Decider, Question};
use nota_core::{Clock, SystemClock};
use nota_llm::local::LocalDecider;
use nota_llm::prompt::English;
use nota_llm::server::{Device, GpuName, Launcher, ServerConfig};
use serde_json::Value;

/// What the example was asked to do.
struct Args {
    /// The llama-server program.
    server: PathBuf,
    /// The model file.
    model: PathBuf,
    /// Where it runs.
    device: Device,
    /// The questions file.
    questions: PathBuf,
    /// The recording's subject, for the prompt.
    subject: Option<String>,
    /// Flags added after the decider's own.
    server_flags: Option<String>,
}

/// The value after each flag, or the usage.
fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Args, Box<dyn Error>> {
    let (mut server, mut model, mut device, mut questions, mut subject) =
        (None, None, None, None, None);
    let mut server_flags = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("each flag needs a value")?;
        match flag.to_str() {
            Some("--server") => server = Some(PathBuf::from(value)),
            Some("--model") => model = Some(PathBuf::from(value)),
            Some("--questions") => questions = Some(PathBuf::from(value)),
            Some("--subject") => subject = value.into_string().ok(),
            Some("--server-flags") => server_flags = value.into_string().ok(),
            Some("--device") => {
                let name = value.to_string_lossy();
                device = Some(if name == "cpu" {
                    Device::Cpu
                } else {
                    Device::Gpu(GpuName::new(&name).ok_or("not a device name")?)
                });
            }
            _ => return Err(format!("unknown flag {}", flag.to_string_lossy()).into()),
        }
    }
    let usage = "usage: bench_decider --server PATH --model PATH --device NAME|cpu --questions FILE [--subject TEXT]";
    Ok(Args {
        server: server.ok_or(usage)?,
        model: model.ok_or(usage)?,
        device: device.ok_or(usage)?,
        questions: questions.ok_or(usage)?,
        subject,
        server_flags,
    })
}

/// The text at `key` in `value`, if there is some.
fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// One entry of the questions file: the question, and the bench's answers.
fn entry(value: &Value) -> Result<(Question, Vec<Option<bool>>), Box<dyn Error>> {
    let candidates = value["candidates"]
        .as_array()
        .ok_or("no candidates")?
        .iter()
        .map(|c| {
            let field = |k| text(c, k).ok_or("a candidate without heard or term");
            Ok(Candidate::new(
                field("heard")?,
                field("term")?,
                text(c, "fact"),
            )?)
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    let question = Question::new(
        text(value, "previous"),
        text(value, "current").ok_or("no chunk")?,
        candidates,
    )?;
    let bench = value["bench"]
        .as_array()
        .ok_or("no bench answers")?
        .iter()
        .map(Value::as_bool)
        .collect();
    Ok((question, bench))
}

/// The tally of answers against the bench's.
#[derive(Debug, Default)]
struct Tally {
    /// Candidates where both said yes.
    both_yes: u32,
    /// Both said no.
    both_no: u32,
    /// nota said yes, the bench no.
    only_nota: u32,
    /// The bench said yes, nota no.
    only_bench: u32,
    /// The bench's reply had failed on it.
    bench_failed: u32,
    /// Questions nota's decider failed on, and their candidates.
    failed: (u32, u32),
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse(std::env::args_os().skip(1))?;
    let file: Value = serde_json::from_slice(&std::fs::read(&args.questions)?)?;
    let entries = file.as_array().ok_or("the questions file isn't a list")?;
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let mut config = ServerConfig::new(
        args.server,
        args.model,
        args.device,
        runtime.join("nota-bench"),
    );
    config.start_timeout = Duration::from_secs(300);
    config.request_timeout = Duration::from_secs(600);
    if let Some(flags) = &args.server_flags {
        // `sh -c SCRIPT server args…`: the script runs the server with its
        // arguments, then the extra flags, which llama-server takes last.
        config.launcher = Some(Launcher {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), format!("exec \"$0\" \"$@\" {flags}").into()],
        });
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::start()?);
    let mut decider = LocalDecider::new(
        config,
        Box::new(English::new(args.subject.as_deref())),
        Arc::clone(&clock),
    );
    let mut out = io::stdout().lock();
    let (mut tally, mut differ) = (Tally::default(), Vec::new());
    let started = clock.now();
    for (index, value) in entries.iter().enumerate() {
        let (question, bench) = entry(value)?;
        match decider.decide(&question, &|| false) {
            Ok(decisions) => {
                for ((no, decision), bench) in decisions.iter().zip(&bench) {
                    let yes = decision.answer == Answer::Yes;
                    match (yes, bench) {
                        (_, None) => tally.bench_failed += 1,
                        (true, Some(true)) => tally.both_yes += 1,
                        (false, Some(false)) => tally.both_no += 1,
                        (true, Some(false)) => tally.only_nota += 1,
                        (false, Some(true)) => tally.only_bench += 1,
                    }
                    if bench.is_some_and(|b| b != yes) {
                        let p = decision
                            .yes
                            .map_or_else(|| "-".to_owned(), |p| format!("{:.3}", p.get()));
                        differ.push(format!(
                            "question {} candidate {no}: nota {yes}, P(yes) {p}",
                            index + 1
                        ));
                    }
                }
            }
            Err(e) => {
                tally.failed.0 += 1;
                tally.failed.1 += u32::try_from(question.len())?;
                writeln!(out, "question {}: {e}", index + 1)?;
            }
        }
    }
    let took = clock
        .now()
        .checked_duration_since(started)
        .unwrap_or_default();
    drop(decider);
    writeln!(
        out,
        "{} questions in {:.1} s",
        entries.len(),
        took.as_secs_f64()
    )?;
    writeln!(out, "{tally:#?}")?;
    for line in differ {
        writeln!(out, "{line}")?;
    }
    Ok(())
}
