//! The `nota` program. Every role is this one executable:
//!
//! - `nota record`: records a session, the system audio and the microphone
//!   as two tracks, with live text on the Recording screen (see `record`
//!   for how it stops safely);
//! - `nota engine asr`: the speech engine child, which `nota record`
//!   starts and talks to over stdin and stdout.
//!
//! The binary's `main` only calls [`main`]; the code is here so its tests
//! can turn on the `fake-capture` feature.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use nota_engine::sherpa::ModelPaths;
use nota_recorder::capture::Source;

mod latency;
mod library;
mod live;
mod record;
mod terminal;
#[cfg(feature = "fake-capture")]
mod tone;

use record::RecordArgs;

const USAGE: &str = "\
usage: nota record [--title TEXT] [--data DIR] [--parakeet DIR --vad FILE]
                   [--mic NODE] [--system NODE]
       nota engine asr --parakeet DIR --vad FILE [--threads N]";

/// What was asked for.
#[derive(Debug)]
enum Command {
    Record(RecordArgs),
    EngineAsr(ModelPaths),
}

/// Runs the command in `args` (without the program name).
#[must_use]
pub fn main(args: &[OsString]) -> ExitCode {
    let command = match parse(args) {
        Ok(command) => command,
        Err(err) => return fail(&format!("{err}\n{USAGE}"), 2),
    };
    match command {
        // The child's stderr is the recorder's to route; it never reaches
        // the screen's terminal.
        Command::EngineAsr(paths) => match nota_engine::run_asr(&paths) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => fail(&format!("nota engine asr: {err}"), 1),
        },
        Command::Record(args) => match record::record(&args) {
            Ok(outcome) => {
                let mut text = format!(
                    "nota: recorded to {} ({} segments)",
                    outcome.session.display(),
                    outcome.segments
                );
                for note in &outcome.notes {
                    text.push_str("\n  ");
                    text.push_str(note);
                }
                say(&text);
                if outcome.complete {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
            Err(err) => fail(&format!("nota record: {err}"), 1),
        },
    }
}

/// Writes `message` to stderr, once the screen is closed. After a hangup
/// there's nowhere to write it; nothing more can be done.
fn say(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
}

fn fail(message: &str, code: u8) -> ExitCode {
    say(message);
    ExitCode::from(code)
}

fn parse(args: &[OsString]) -> Result<Command, String> {
    let mut words = args.iter();
    match words.next().and_then(|a| a.to_str()) {
        Some("engine") if words.next().and_then(|a| a.to_str()) == Some("asr") => {
            parse_engine(words.as_slice()).map(Command::EngineAsr)
        }
        Some("record") => parse_record(words.as_slice()).map(Command::Record),
        _ => Err("unknown command".into()),
    }
}

/// `--flag value` pairs, in order.
fn pairs(args: &[OsString]) -> Result<Vec<(&str, &OsString)>, String> {
    let mut out = Vec::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let name = flag
            .to_str()
            .ok_or_else(|| format!("unknown option {}", flag.display()))?;
        let value = args.next().ok_or_else(|| format!("{name} needs a value"))?;
        out.push((name, value));
    }
    Ok(out)
}

/// `engine asr` and its options.
fn parse_engine(args: &[OsString]) -> Result<ModelPaths, String> {
    let (mut parakeet, mut vad, mut threads) = (None, None, 4_u16);
    for (flag, value) in pairs(args)? {
        match flag {
            "--parakeet" => parakeet = Some(PathBuf::from(value)),
            "--vad" => vad = Some(PathBuf::from(value)),
            "--threads" => {
                threads = value
                    .to_str()
                    .and_then(|v| v.parse().ok())
                    .filter(|&n| n > 0)
                    .ok_or("--threads takes a positive number")?;
            }
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    Ok(ModelPaths {
        parakeet_dir: parakeet.ok_or("--parakeet is required")?,
        vad_model: vad.ok_or("--vad is required")?,
        threads,
    })
}

/// `record` and its options. Without `--data`, sessions go in the
/// platform's data directory (`~/.local/share/nota` on Linux).
fn parse_record(args: &[OsString]) -> Result<RecordArgs, String> {
    let mut record = RecordArgs {
        data: PathBuf::new(),
        title: "Recording".to_owned(),
        models: None,
        mic: Source::Microphone,
        system: Source::SystemAudio,
        tone: false,
        latency_log: None,
    };
    let (mut data, mut parakeet, mut vad) = (None, None, None);
    for (flag, value) in pairs(args)? {
        let text = || {
            value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{flag} takes text"))
        };
        match flag {
            "--title" => record.title = text()?,
            "--data" => data = Some(PathBuf::from(value)),
            "--parakeet" => parakeet = Some(PathBuf::from(value)),
            "--vad" => vad = Some(PathBuf::from(value)),
            "--mic" => record.mic = Source::Device(text()?),
            "--system" => record.system = Source::Device(text()?),
            #[cfg(feature = "fake-capture")]
            "--tone" => record.tone = text()? == "yes",
            #[cfg(feature = "latency-log")]
            "--latency-log" => record.latency_log = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    record.models = match (parakeet, vad) {
        (Some(p), Some(v)) => Some((p, v)),
        (None, None) => None,
        _ => return Err("--parakeet and --vad go together".into()),
    };
    record.data = match data {
        // Absolute, so every directory in it has a parent to make it in.
        Some(dir) => {
            std::path::absolute(&dir).map_err(|e| format!("--data {}: {e}", dir.display()))?
        }
        None => directories::ProjectDirs::from("", "", "nota")
            .ok_or("no home directory to keep sessions in; pass --data")?
            .data_dir()
            .to_path_buf(),
    };
    Ok(record)
}

#[cfg(test)]
mod tests;
