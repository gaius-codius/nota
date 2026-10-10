//! The `nota` program. Every role is this one executable:
//!
//! - `nota`: the Home screen, which lists the library's sessions and
//!   records a new one with `R` (see `app`);
//! - `nota record`: records a session, the system audio and the microphone
//!   as two tracks, with live text on the Recording screen (see `record`
//!   for how it stops safely);
//! - `nota engine asr`: the speech engine child, which `nota record`
//!   starts and talks to over stdin and stdout, and the app starts for the
//!   final pass after each recording (see `jobs` and `final_pass`).
//!
//! The binary's `main` only calls [`main`]; the code is here so its tests
//! can turn on the `fake-capture` feature.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use nota_core::SampleRate;
use nota_core::recorder::{self, Input, Setup};
use nota_engine::chunker::ChunkerConfig;
use nota_engine::sherpa::ModelPaths;

mod app;
#[cfg(feature = "fake-capture")]
mod fake_engine;
mod final_pass;
mod inhibit;
mod jobs;
mod latency;
mod library;
mod live;
mod record;
mod terminal;
#[cfg(feature = "fake-capture")]
mod tone;

use record::RecordArgs;

const USAGE: &str = "\
usage: nota [--data DIR] [--parakeet DIR --vad FILE]
       nota record [--title TEXT] [--data DIR] [--parakeet DIR --vad FILE]
                   [--mic NODE] [--system NODE]
       nota engine asr --parakeet DIR --vad FILE [--threads N] [--pass live|final]";

/// What was asked for.
#[derive(Debug)]
enum Command {
    /// Home, and recordings from it.
    App(RecordArgs),
    Record(RecordArgs),
    EngineAsr(ModelPaths, ChunkerConfig),
    /// `nota engine fake`: the engine child with stand-in models (tests
    /// only).
    #[cfg(feature = "fake-capture")]
    EngineFake(ChunkerConfig),
}

/// Runs the command in `args` (without the program name).
#[must_use]
pub fn main(args: &[OsString]) -> ExitCode {
    let command = match parse(args) {
        Ok(command) => command,
        Err(err) => return fail(&format!("{err}\n{USAGE}"), 2),
    };
    match command {
        Command::App(args) => {
            let (said, ran) = app::app(&args);
            // What the recordings reported, even if nota then failed.
            if !said.is_empty() {
                say(&said.join("\n"));
            }
            match ran {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => fail(&format!("nota: {err}"), 1),
            }
        }
        // The child's stderr is the recorder's to route; it never reaches
        // the screen's terminal.
        Command::EngineAsr(paths, chunking) => match nota_engine::run_asr(&paths, chunking) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => fail(&format!("nota engine asr: {err}"), 1),
        },
        #[cfg(feature = "fake-capture")]
        Command::EngineFake(chunking) => match fake_engine::run(chunking) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => fail(&format!("nota engine fake: {err}"), 1),
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
            parse_engine(words.as_slice())
                .map(|(paths, chunking)| Command::EngineAsr(paths, chunking))
        }
        #[cfg(feature = "fake-capture")]
        Some("engine") if args.get(1).and_then(|a| a.to_str()) == Some("fake") => {
            parse_pass(args.get(2..).unwrap_or_default()).map(Command::EngineFake)
        }
        Some("record") => parse_record(words.as_slice()).map(Command::Record),
        // No command, or options only: Home.
        None => parse_app(args).map(Command::App),
        Some(word) if word.starts_with("--") => parse_app(args).map(Command::App),
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

/// `engine asr` and its options: the models, and how to cut the audio
/// (`--pass live`, the default, or `--pass final`).
fn parse_engine(args: &[OsString]) -> Result<(ModelPaths, ChunkerConfig), String> {
    let (mut parakeet, mut vad, mut threads) = (None, None, 4_u16);
    let mut chunking = ChunkerConfig::live(SampleRate::SPEECH);
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
            "--pass" => chunking = pass(value)?,
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    let paths = ModelPaths {
        parakeet_dir: parakeet.ok_or("--parakeet is required")?,
        vad_model: vad.ok_or("--vad is required")?,
        threads,
    };
    Ok((paths, chunking))
}

/// `--pass`'s value: how the engine cuts the audio.
fn pass(value: &OsString) -> Result<ChunkerConfig, String> {
    match value.to_str() {
        Some("live") => Ok(ChunkerConfig::live(SampleRate::SPEECH)),
        Some("final") => Ok(ChunkerConfig::final_pass(SampleRate::SPEECH)),
        _ => Err("--pass takes live or final".into()),
    }
}

/// `engine fake`'s only option, `--pass`.
#[cfg(feature = "fake-capture")]
fn parse_pass(args: &[OsString]) -> Result<ChunkerConfig, String> {
    let mut chunking = ChunkerConfig::live(SampleRate::SPEECH);
    for (flag, value) in pairs(args)? {
        match flag {
            "--pass" => chunking = pass(value)?,
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    Ok(chunking)
}

/// Home's options: where the library is, and the engine's models. The
/// recording options that Setup will choose (`--title`, `--mic`,
/// `--system`) and the latency log are `nota record`'s alone.
fn parse_app(args: &[OsString]) -> Result<RecordArgs, String> {
    for (flag, _) in pairs(args)? {
        if matches!(flag, "--title" | "--mic" | "--system" | "--latency-log") {
            return Err(format!("{flag} is an option of nota record"));
        }
    }
    parse_record(args)
}

/// `record` and its options. Without `--data`, sessions go in the
/// platform's data directory (`~/.local/share/nota` on Linux).
fn parse_record(args: &[OsString]) -> Result<RecordArgs, String> {
    let mut setup = Setup {
        title: "Recording".to_owned(),
        mic: Input::Default,
        system: Input::Default,
    };
    // Only the test and measurement builds take the options that set these.
    #[cfg(feature = "fake-capture")]
    let (mut tone, mut fake_engine) = (false, false);
    #[cfg(not(feature = "fake-capture"))]
    let (tone, fake_engine) = (false, false);
    #[cfg(feature = "latency-log")]
    let mut latency_log = None;
    #[cfg(not(feature = "latency-log"))]
    let latency_log = None;
    let (mut data, mut parakeet, mut vad) = (None, None, None);
    for (flag, value) in pairs(args)? {
        let text = || {
            value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{flag} takes text"))
        };
        match flag {
            "--title" => setup.title = text()?,
            "--data" => data = Some(PathBuf::from(value)),
            "--parakeet" => parakeet = Some(PathBuf::from(value)),
            "--vad" => vad = Some(PathBuf::from(value)),
            "--mic" => setup.mic = Input::Device(text()?),
            "--system" => setup.system = Input::Device(text()?),
            #[cfg(feature = "fake-capture")]
            "--tone" => tone = text()? == "yes",
            #[cfg(feature = "fake-capture")]
            "--fake-engine" => fake_engine = text()? == "yes",
            #[cfg(feature = "latency-log")]
            "--latency-log" => latency_log = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    let models = match (parakeet, vad) {
        (Some(p), Some(v)) => Some((p, v)),
        (None, None) => None,
        _ => return Err("--parakeet and --vad go together".into()),
    };
    let data = match data {
        // Absolute, so every directory in it has a parent to make it in.
        Some(dir) => {
            std::path::absolute(&dir).map_err(|e| format!("--data {}: {e}", dir.display()))?
        }
        None => directories::ProjectDirs::from("", "", "nota")
            .ok_or("no home directory to keep sessions in; pass --data")?
            .data_dir()
            .to_path_buf(),
    };
    Ok(RecordArgs {
        data,
        start: recorder::Command::Start(setup),
        models,
        tone,
        latency_log,
        fake_engine,
    })
}

#[cfg(test)]
mod tests;
