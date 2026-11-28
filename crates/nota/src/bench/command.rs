//! `nota bench`'s command line, and the run it asks for:
//!
//! - `nota bench --engines NAMES (--session N [--data DIR] | --fixture
//!   FILE) [--precheck on|off] [--log DIR]` asks each engine about the
//!   candidates, writes the report to stdout, and logs each engine's run
//!   to a file of its own in `DIR` (`<data>/bench` unless given).
//! - `nota bench compare OLD NEW` lists what changed between two logs.
//!
//! The engines are built from the user's own setup: `local` from the
//! settings (as clean-up's is), `claude` and `codex` from the CLIs on
//! `PATH`, each started through nota's own `engine` shim so it ends when
//! nota does. They're built on unix only, where the settings and CLIs
//! are read.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use nota_core::{Clock, SessionId, SystemClock, WallTime, wall_now};

use super::fixture::from_fixture;
use super::{Candidates, Engine, EngineName, from_session, log};
use crate::cleanup::Precheck;

/// What `nota bench` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BenchCommand {
    /// Ask engines about a session's or a fixture's candidates.
    Run(RunArgs),
    /// List what changed between two logs.
    Compare {
        /// The earlier log.
        old: PathBuf,
        /// The later log.
        new: PathBuf,
    },
}

/// What a bench run asks, of which engines, and where it logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunArgs {
    /// The engines, in the order given; at least one, none twice.
    pub(crate) engines: Vec<EngineName>,
    /// Where the candidates come from.
    pub(crate) source: Source,
    /// Whether C3 runs: off unless `--precheck on`.
    pub(crate) precheck: Precheck,
    /// The folder each engine's log goes in.
    pub(crate) log: PathBuf,
}

/// Where a run's candidates come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// A session in the library.
    Session {
        /// The data directory the library is in.
        data: PathBuf,
        /// The session.
        session: SessionId,
    },
    /// A fixture file ([`super::fixture`]).
    Fixture(PathBuf),
}

/// The flags of a bench run, as given, before they're checked together.
#[derive(Debug, Default)]
struct Flags {
    /// `--engines`.
    engines: Option<Vec<EngineName>>,
    /// `--session`.
    session: Option<SessionId>,
    /// `--data`.
    data: Option<PathBuf>,
    /// `--fixture`.
    fixture: Option<PathBuf>,
    /// `--precheck`.
    precheck: Precheck,
    /// `--log`.
    log: Option<PathBuf>,
}

/// `nota bench`'s arguments, after the word `bench`.
///
/// # Errors
///
/// What's wrong with them, for the user.
pub(crate) fn parse(args: &[OsString]) -> Result<BenchCommand, String> {
    if args.first().and_then(|a| a.to_str()) == Some("compare") {
        return match args.get(1..) {
            Some([old, new]) => Ok(BenchCommand::Compare {
                old: PathBuf::from(old),
                new: PathBuf::from(new),
            }),
            _ => Err("bench compare takes two logs: the old and the new".to_owned()),
        };
    }
    let mut flags = Flags::default();
    for (flag, value) in crate::pairs(args)? {
        match flag {
            "--engines" => flags.engines = Some(engine_names(value)?),
            "--session" => flags.session = Some(session(value)?),
            "--data" => flags.data = Some(PathBuf::from(value)),
            "--fixture" => flags.fixture = Some(PathBuf::from(value)),
            "--precheck" => flags.precheck = precheck(value)?,
            "--log" => flags.log = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    run_args(flags).map(BenchCommand::Run)
}

/// The run `flags` ask for, once they're checked together.
fn run_args(flags: Flags) -> Result<RunArgs, String> {
    let engines = flags.engines.ok_or("--engines is required")?;
    let source = match (flags.session, flags.fixture) {
        (Some(session), None) => Source::Session {
            data: crate::data_dir(flags.data.clone())?,
            session,
        },
        (None, Some(fixture)) => Source::Fixture(fixture),
        (Some(_), Some(_)) => return Err("--session and --fixture don't go together".to_owned()),
        (None, None) => return Err("bench needs --session or --fixture".to_owned()),
    };
    let log = match flags.log {
        Some(log) => log,
        None => crate::data_dir(flags.data)?.join("bench"),
    };
    Ok(RunArgs {
        engines,
        source,
        precheck: flags.precheck,
        log,
    })
}

/// `--engines`' value: engine names split at commas, none twice.
fn engine_names(value: &OsString) -> Result<Vec<EngineName>, String> {
    let text = value.to_str().ok_or("--engines takes engine names")?;
    let mut names = Vec::new();
    for word in text.split(',') {
        let name = EngineName::parse(word).ok_or_else(|| {
            format!("unknown engine {word:?}: --engines takes local, claude or codex")
        })?;
        // Checked by engine, not by word: `local` and `qwen` are one.
        if names.contains(&name) {
            return Err(format!("--engines names {} twice", name.label()));
        }
        names.push(name);
    }
    Ok(names)
}

/// `--session`'s value: a session's number.
fn session(value: &OsString) -> Result<SessionId, String> {
    value
        .to_str()
        .and_then(|v| v.parse().ok())
        .map(SessionId::new)
        .ok_or_else(|| "--session takes a session's number".to_owned())
}

/// `--precheck`'s value.
fn precheck(value: &OsString) -> Result<Precheck, String> {
    match value.to_str() {
        Some("on") => Ok(Precheck::On),
        Some("off") => Ok(Precheck::Off),
        _ => Err("--precheck takes on or off".to_owned()),
    }
}

/// Runs `command`, writing to `out` what it prints.
///
/// # Errors
///
/// The reason, for the user, if it couldn't be done.
pub(crate) fn dispatch(command: &BenchCommand, out: &mut dyn io::Write) -> Result<(), String> {
    match command {
        BenchCommand::Compare { old, new } => log::compare(old, new, out),
        BenchCommand::Run(args) => bench(args, out),
    }
}

/// Runs the bench `args` ask for: the engines, the candidates, each
/// engine's log and the report. The clocks, the engines, the candidates
/// and the log folder are checked, in that order, before any question is
/// asked. Every engine's log is written before the report, so a report
/// that can't be printed loses no run.
///
/// # Errors
///
/// The reason, for the user, if a check fails, a log can't be written,
/// the report can't be printed, or no engine got through the candidates.
fn bench(args: &RunArgs, out: &mut dyn io::Write) -> Result<(), String> {
    let clock: Arc<dyn Clock> =
        Arc::new(SystemClock::start().map_err(|_| "the system clock can't be read")?);
    let started = wall_now().ok_or("the calendar's time can't be read, to name the logs")?;
    let stamp = stamp(started).ok_or("the calendar's time can't be read, to name the logs")?;
    let engines = engines(&args.engines, &clock)?;
    let candidates = candidates(&args.source, args.precheck, &clock)?;
    log::make_folder(&args.log)?;
    let runs = super::run(&candidates, engines, &*clock, &|| false);
    let logged: Vec<_> = runs
        .iter()
        .map(|run| log::write(&args.log, &stamp, run, &candidates, Some(started)))
        .collect();
    let written = |e: io::Error| format!("couldn't write to stdout: {e}");
    super::report(&candidates, &runs, out).map_err(written)?;
    let mut failed = Vec::new();
    for (run, log) in runs.iter().zip(logged) {
        match log {
            Ok(path) => {
                writeln!(out, "Logged {}: {}", run.name, path.display()).map_err(written)?;
            }
            Err(why) => failed.push(why),
        }
    }
    if !failed.is_empty() {
        return Err(failed.join("; "));
    }
    if runs.iter().all(|run| run.down.is_some()) {
        return Err("no engine got through the candidates".to_owned());
    }
    Ok(())
}

/// The candidates from `source`, with C3 as `precheck`.
fn candidates(
    source: &Source,
    precheck: Precheck,
    clock: &Arc<dyn Clock>,
) -> Result<Candidates, String> {
    match source {
        Source::Session { data, session } => {
            from_session(data, *session, precheck, Arc::clone(clock))
        }
        Source::Fixture(path) => from_fixture(path, precheck),
    }
}

/// `at` as a log file's stamp, `YYYYMMDD-HHMMSS` in UTC, so the logs sort
/// by when their bench started.
pub(crate) fn stamp(at: WallTime) -> Option<String> {
    let at = jiff::Timestamp::from_second(at.unix_seconds()).ok()?;
    Some(
        at.to_zoned(jiff::tz::TimeZone::UTC)
            .strftime("%Y%m%d-%H%M%S")
            .to_string(),
    )
}

/// The engines `names` name, in order, each timed by `clock`.
#[cfg(unix)]
pub(super) fn engines(names: &[EngineName], clock: &Arc<dyn Clock>) -> Result<Vec<Engine>, String> {
    names
        .iter()
        .map(|name| real::engine(*name, clock))
        .collect()
}

/// Off unix no decider can be built: the settings and CLIs are read on
/// unix only.
#[cfg(not(unix))]
pub(super) fn engines(
    _names: &[EngineName],
    _clock: &Arc<dyn Clock>,
) -> Result<Vec<Engine>, String> {
    Err("the bench needs unix: its deciders are set up there only".to_owned())
}

/// The real deciders, as the user has them set up.
#[cfg(unix)]
pub(super) mod real {
    use std::path::PathBuf;
    use std::sync::Arc;

    use nota_core::Clock;
    use nota_core::decide::{Decider, Place};
    use nota_llm::cli::{Cli, CliConfig, CliDecider};
    use nota_llm::server::Launcher;
    use nota_store::DecidedBy;

    use super::super::{Engine, EngineName};
    use crate::cleanup::Setup;
    use crate::cleanup::sources::local_decider;
    use crate::settings::Environment;

    /// A decider, and what it records as having answered.
    type Built = (Box<dyn Decider + Send>, DecidedBy);

    /// The engine `name`, timed by `clock`.
    pub(super) fn engine(name: EngineName, clock: &Arc<dyn Clock>) -> Result<Engine, String> {
        let label = name.label();
        let (decider, by) = match name {
            EngineName::Local => local(clock),
            EngineName::Claude => cli(Cli::Claude, "claude -p", clock),
            EngineName::Codex => cli(Cli::Codex, "codex exec", clock),
        }
        .map_err(|why| format!("{label}: {why}"))?;
        Ok(Engine {
            name: label.to_owned(),
            decider,
            by,
        })
    }

    /// The local decider the settings describe, as clean-up would use it.
    fn local(clock: &Arc<dyn Clock>) -> Result<Built, String> {
        match local_decider(&Environment::current(), clock) {
            Setup::Ready(decider, by) => Ok((decider, by)),
            Setup::NotSetUp => Err("no local decider is set up in the settings".to_owned()),
            Setup::Unavailable(why) => Err(why),
        }
    }

    /// `cli` with its own default model, recorded as `program`, started
    /// through nota's shim.
    fn cli(cli: Cli, program: &str, clock: &Arc<dyn Clock>) -> Result<Built, String> {
        let mut config = CliConfig::new(cli, work_dir()?);
        let exe = std::env::current_exe()
            .map_err(|e| format!("nota's own program can't be found: {e}"))?;
        // The shim execs whatever program it's given, tied to this nota,
        // as it does llama-server.
        config.launcher = Some(Launcher {
            program: exe,
            args: vec!["engine".into(), "llama-server".into()],
        });
        let prompt = Box::new(nota_llm::prompt::English::new(None));
        let decider = CliDecider::new(config, prompt, Arc::clone(clock));
        let by = DecidedBy {
            name: program.to_owned(),
            model: "default".to_owned(),
            place: Place::Cloud,
        };
        Ok((Box::new(decider), by))
    }

    /// The empty folder the CLIs run in: `bench-cli` in nota's runtime
    /// folder, or its cache folder where there's no runtime one.
    pub(in crate::bench) fn work_dir() -> Result<PathBuf, String> {
        let dirs = directories::ProjectDirs::from("", "", "nota")
            .ok_or("no home directory to run the CLI in")?;
        let base = dirs.runtime_dir().unwrap_or_else(|| dirs.cache_dir());
        Ok(base.join("bench-cli"))
    }
}
