//! The cloud deciders reached through the user's own subscription CLIs:
//! `claude -p` and `codex exec`, each on the subscription the user signed
//! in to it with. nota never signs in itself; it runs the CLI the user
//! installed, once per question.
//!
//! The question goes in on the CLI's stdin, never in its arguments, which
//! anyone on the machine can read. The arguments hold only the answer's
//! schema, a fixed instruction and the model's name. Each CLI runs in an
//! empty folder of nota's own, keeps no session, and is told to use no
//! tools and load none of the user's MCP servers or hooks: Claude with
//! `--tools ""` and no setting sources, Codex without the user's config
//! and with its shell, web, app and browser tools turned off.
//!
//! Given a [`CliConfig::launcher`] (nota's own `engine` shim, as for
//! llama-server; see [`crate::lifeline`]), the CLI is started through it
//! and is killed when nota dies, however it dies.
//! - `claude -p --output-format json --json-schema …` answers with a
//!   result object whose `structured_output` is the answer.
//! - `codex exec --output-schema <file> -` prints the answer alone on its
//!   stdout. Its schema has to be a file, so one is written to the folder
//!   for the question and removed after.
//!
//! What happens to a question:
//! - **`stop` says to stop:** the CLI is killed with everything it
//!   started, and the question comes back [`DecideError::Stopped`].
//! - **The CLI can't run** (it isn't installed, it exits with an error,
//!   Claude reports an error such as a usage limit, or it hasn't answered
//!   within [`CliConfig::timeout`]): [`DecideError::Unavailable`], with
//!   the reason. It isn't asked again: a CLI that failed once, or ran out
//!   of time, will most likely do so again, and each try spends the
//!   user's subscription.
//! - **The answer can't be read** (not one yes or no per candidate, or
//!   too long): a failed decision, [`DecideError::Failed`].
//!
//! No reason ever holds what the CLI said: both echo the question on
//! stderr, and an answer may quote the transcript. A reason names the
//! CLI, its exit status, the error status Claude gives, or what was wrong
//! with the shape of the answer.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nota_core::Clock;
use nota_core::decide::{DecideError, Decider, Decision, Decisions, Place, Question};
use nota_core::lifeline::RECORDER_PID_VAR;
use serde_json::Value;

use crate::prompt::Prompt;
use crate::reply::{self, ReplyError};
use crate::request;
use crate::server::{Launcher, ServerError, private_dir, runnable};

mod run;

use run::{Ran, RunFailure};

/// How long a CLI has to answer by default: a cloud model behind a CLI
/// that starts afresh for each question takes ten to thirty seconds.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// The system prompt `claude -p` is given in place of its own, which is
/// about writing code. nota's words, which name the session's subject,
/// go in on stdin with the question.
pub const CLAUDE_SYSTEM: &str = "Answer the user's message exactly as it asks, in the JSON \
    shape it gives. Use no tools.";

/// The Codex features turned off for a question, so it has no tool to
/// run: its shells, its apps and plugins, and its browsers.
pub const CODEX_TOOLS_OFF: [&str; 7] = [
    "shell_tool",
    "unified_exec",
    "apps",
    "plugins",
    "browser_use",
    "computer_use",
    "in_app_browser",
];

/// Which CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cli {
    /// Anthropic's Claude Code, `claude -p`.
    Claude,
    /// Codex, `codex exec`.
    Codex,
}

impl Cli {
    /// The program's usual name, looked for on `PATH`.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl fmt::Display for Cli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.program())
    }
}

/// How to reach a CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliConfig {
    /// Which CLI it is.
    pub cli: Cli,
    /// The program: its name, looked for on `PATH`, or its path.
    pub program: PathBuf,
    /// The model to ask for, or `None` for the CLI's own default.
    pub model: Option<String>,
    /// The empty folder the CLI runs in, made if missing; only this user
    /// may open it.
    pub work_dir: PathBuf,
    /// How long it has to answer a question.
    pub timeout: Duration,
    /// What to start the CLI through, if anything: normally nota's own
    /// shim, which ties the CLI to nota. It must run the CLI by exec.
    pub launcher: Option<Launcher>,
}

impl CliConfig {
    /// `cli` by its usual name, with its own default model, run in
    /// `work_dir`, with [`DEFAULT_TIMEOUT`].
    #[must_use]
    pub fn new(cli: Cli, work_dir: PathBuf) -> Self {
        Self {
            cli,
            program: PathBuf::from(cli.program()),
            model: None,
            work_dir,
            timeout: DEFAULT_TIMEOUT,
            launcher: None,
        }
    }
}

/// Why a CLI gave no decisions. None of these holds anything the CLI said.
#[derive(Debug)]
pub enum CliError {
    /// The program wasn't found: the CLI isn't installed, or isn't where
    /// it was said to be.
    Missing(Cli, PathBuf),
    /// The program was found but couldn't be started.
    Spawn(Cli, io::Error),
    /// The folder it runs in couldn't be made or used.
    WorkDir(PathBuf, io::Error),
    /// The folder it runs in can be opened by other users.
    WorkDirOpen(PathBuf),
    /// Its schema file couldn't be written.
    Schema(Cli, io::Error),
    /// It hadn't answered within this long, and was killed.
    NoAnswer(Cli, Duration),
    /// It exited with this status, without an answer.
    Exited(Cli, ExitStatus),
    /// Claude answered with an error (a usage limit, a sign-in that has
    /// lapsed): the HTTP status it gives, if any.
    Refused(Cli, Option<u16>),
    /// Waiting for it, or reading what it wrote, failed.
    Io(Cli, io::Error),
    /// Its answer was longer than [`reply::MAX_REPLY_LEN`], and it was
    /// killed.
    TooLong(Cli),
    /// Its answer couldn't be read as one yes or no per candidate.
    Unreadable(Cli, ReplyError),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(cli, program) => write!(
                f,
                "{cli} isn't installed: there's no program {}",
                program.display()
            ),
            Self::Spawn(cli, e) => write!(f, "{cli} couldn't be started: {e}"),
            Self::WorkDir(dir, e) => write!(
                f,
                "the folder the CLI runs in, {}, can't be used: {e}",
                dir.display()
            ),
            Self::WorkDirOpen(dir) => write!(
                f,
                "the folder the CLI runs in, {}, can be opened by other users",
                dir.display()
            ),
            Self::Schema(cli, e) => write!(f, "{cli}'s answer schema couldn't be written: {e}"),
            Self::NoAnswer(cli, limit) => {
                write!(f, "{cli} gave no answer within {limit:?}")
            }
            Self::Exited(cli, status) => write!(f, "{cli} failed ({status})"),
            Self::Refused(cli, Some(status)) => {
                write!(f, "{cli} answered with an error (status {status})")
            }
            Self::Refused(cli, None) => write!(f, "{cli} answered with an error"),
            Self::Io(cli, e) => write!(f, "{cli}'s answer couldn't be read: {e}"),
            Self::TooLong(cli) => write!(
                f,
                "{cli}'s answer is longer than {} bytes",
                reply::MAX_REPLY_LEN
            ),
            Self::Unreadable(cli, e) => write!(f, "{cli}'s answer couldn't be used: {e}"),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(_, e) | Self::Schema(_, e) | Self::Io(_, e) | Self::WorkDir(_, e) => {
                Some(e)
            }
            Self::Unreadable(_, e) => Some(e),
            _ => None,
        }
    }
}

impl CliError {
    /// The decider's error for this: a failed decision if the answer
    /// couldn't be read, otherwise an unavailable decider.
    fn into_decide(self) -> DecideError {
        match self {
            Self::Unreadable(..) | Self::TooLong(_) => DecideError::Failed(Box::new(self)),
            _ => DecideError::Unavailable(Box::new(self)),
        }
    }
}

/// A decider that asks a cloud model through the user's own CLI.
#[derive(Debug)]
pub struct CliDecider {
    /// How to reach the CLI.
    config: CliConfig,
    /// The words for the session's language.
    prompt: Box<dyn Prompt>,
    /// The session clock, for the time limit.
    clock: Arc<dyn Clock>,
}

impl CliDecider {
    /// A decider that runs the CLI as `config` says, asking in `prompt`'s
    /// words, timed by `clock`. Nothing runs until the first question.
    #[must_use]
    pub fn new(config: CliConfig, prompt: Box<dyn Prompt>, clock: Arc<dyn Clock>) -> Self {
        Self {
            config,
            prompt,
            clock,
        }
    }

    /// Runs the CLI on `question` and reads its decisions.
    fn ask(&self, question: &Question, stop: &dyn Fn() -> bool) -> Result<Decisions, Asked> {
        let cli = self.config.cli;
        private_dir(&self.config.work_dir).map_err(work_dir_error)?;
        self.check_program()?;
        let schema = request::schema(question.len());
        let schema_file = match cli {
            Cli::Claude => None,
            Cli::Codex => Some(SchemaFile::write(&self.config.work_dir, &schema)?),
        };
        let command = self.command(&schema, schema_file.as_ref());
        let input = request::one_message(&*self.prompt, question).into_bytes();
        let ran = run::run(command, input, &*self.clock, self.config.timeout, stop)
            .map_err(|failure| self.failed(failure))?;
        let decisions = match cli {
            Cli::Claude => claude_answer(&ran, question.len()),
            Cli::Codex => codex_answer(&ran, question.len()),
        }
        .map_err(Asked::Error)?;
        Decisions::new(question, decisions).map_err(|_| {
            // `answer_of` gives one per candidate, so this can't happen;
            // if it did, it's an answer that can't be used.
            Asked::Error(CliError::Unreadable(
                cli,
                ReplyError::NotDecisions("not one per candidate"),
            ))
        })
    }

    /// The command that asks the question, with the answer held to
    /// `schema` (for Codex, in `schema_file`), started through the
    /// launcher if there is one.
    fn command(&self, schema: &Value, schema_file: Option<&SchemaFile>) -> Command {
        let mut command = match &self.config.launcher {
            Some(launcher) => {
                let mut c = Command::new(&launcher.program);
                c.args(&launcher.args).arg(&self.config.program);
                c
            }
            None => Command::new(&self.config.program),
        };
        command
            .current_dir(&self.config.work_dir)
            // Whom the launcher ties the CLI to.
            .env(RECORDER_PID_VAR, std::process::id().to_string());
        match self.config.cli {
            Cli::Claude => command.args(claude_args(schema)),
            Cli::Codex => command.args(codex_args(schema_file)),
        };
        if let Some(model) = &self.config.model {
            command.args([OsString::from("--model"), OsString::from(model)]);
        }
        if self.config.cli == Cli::Codex {
            // The prompt from stdin.
            command.arg("-");
        }
        command
    }

    /// Whether a CLI named by its path can be run, when it's started
    /// through a launcher: the launcher would start, then fail to run it,
    /// which would read as a CLI that fails. A bare name is looked for on
    /// `PATH` by the launcher's exec, so it's left to that.
    fn check_program(&self) -> Result<(), CliError> {
        let program = &self.config.program;
        if self.config.launcher.is_none() || program.components().count() < 2 {
            return Ok(());
        }
        runnable(program).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => CliError::Missing(self.config.cli, program.clone()),
            _ => CliError::Spawn(self.config.cli, e),
        })
    }

    /// The error for a run that didn't reach its end.
    fn failed(&self, failure: RunFailure) -> Asked {
        let cli = self.config.cli;
        Asked::Error(match failure {
            RunFailure::Stopped => return Asked::Stopped,
            RunFailure::Spawn(e) if e.kind() == io::ErrorKind::NotFound => {
                CliError::Missing(cli, self.config.program.clone())
            }
            RunFailure::Spawn(e) => CliError::Spawn(cli, e),
            RunFailure::Late => CliError::NoAnswer(cli, self.config.timeout),
            RunFailure::TooLong => CliError::TooLong(cli),
            RunFailure::Io(e) => CliError::Io(cli, e),
        })
    }
}

/// What asking came to, short of decisions.
#[derive(Debug)]
enum Asked {
    /// `stop` said to stop.
    Stopped,
    /// The CLI failed, or its answer couldn't be read.
    Error(CliError),
}

impl From<CliError> for Asked {
    fn from(e: CliError) -> Self {
        Self::Error(e)
    }
}

impl Decider for CliDecider {
    /// Always the cloud: the question goes to the CLI's provider.
    fn place(&self) -> Place {
        Place::Cloud
    }

    fn decide(
        &mut self,
        question: &Question,
        stop: &dyn Fn() -> bool,
    ) -> Result<Decisions, DecideError> {
        if stop() {
            return Err(DecideError::Stopped);
        }
        match self.ask(question, stop) {
            Ok(decisions) => Ok(decisions),
            Err(Asked::Stopped) => Err(DecideError::Stopped),
            Err(Asked::Error(e)) => Err(e.into_decide()),
        }
    }
}

/// `claude -p`'s arguments, before the model's: JSON out, held to
/// `schema`, with no tools, session, settings, MCP servers or commands,
/// and nota's own system prompt.
fn claude_args(schema: &Value) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-p", "--output-format", "json", "--json-schema"]
        .map(OsString::from)
        .into();
    args.push(schema.to_string().into());
    args.extend(
        [
            "--tools",
            "",
            "--no-session-persistence",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--system-prompt",
            CLAUDE_SYSTEM,
        ]
        .map(OsString::from),
    );
    args
}

/// `codex exec`'s arguments, before the model's: without the user's
/// config or a session, read-only, with no tools, and held to the schema
/// in `schema_file`.
fn codex_args(schema_file: Option<&SchemaFile>) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "exec",
        "--ignore-user-config",
        "--ephemeral",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
        "--color",
        "never",
        // Its default with no config of the user's is no reasoning, which
        // was seen to hang.
        "-c",
        "model_reasoning_effort=\"low\"",
        "-c",
        "web_search=\"disabled\"",
    ]
    .map(OsString::from)
    .into();
    // No tools: a read-only sandbox still lets its shell read any file,
    // and the transcript could ask it to.
    for tool in CODEX_TOOLS_OFF {
        args.extend(["--disable", tool].map(OsString::from));
    }
    if let Some(file) = schema_file {
        args.push("--output-schema".into());
        args.push(file.0.clone().into());
    }
    args
}

/// The error for a work folder [`private_dir`] refused.
fn work_dir_error(e: ServerError) -> CliError {
    match e {
        ServerError::SocketDir(dir, e) => CliError::WorkDir(dir, e),
        ServerError::SocketDirOpen(dir) => CliError::WorkDirOpen(dir),
        // `private_dir` gives no other; kept whole if it ever does.
        other => CliError::WorkDir(PathBuf::new(), io::Error::other(other.to_string())),
    }
}

/// The decisions in what `claude -p --output-format json` wrote: a result
/// object, whose `structured_output` is the answer unless `is_error` says
/// it failed.
fn claude_answer(ran: &Ran, candidates: usize) -> Result<Vec<Decision>, CliError> {
    let cli = Cli::Claude;
    let unreadable = |e| CliError::Unreadable(cli, e);
    let result = match reply::strict_json(&ran.stdout) {
        Ok(result) => result,
        // Not even a result: a CLI that failed says so by its status.
        Err(_) if !ran.status.success() => return Err(CliError::Exited(cli, ran.status)),
        Err(e) => return Err(unreadable(ReplyError::NotJson(e))),
    };
    if result.get("type").and_then(Value::as_str) != Some("result") {
        return Err(if ran.status.success() {
            unreadable(ReplyError::NotACompletion("it isn't a result"))
        } else {
            CliError::Exited(cli, ran.status)
        });
    }
    match result.get("is_error").and_then(Value::as_bool) {
        Some(false) => {}
        // Claude kept answering in the wrong shape and gave up: the same
        // as a malformed answer, not a CLI that can't run.
        Some(true) if gave_up_on_the_shape(&result) => {
            return Err(unreadable(ReplyError::NotDecisions(
                "it gave up on the answer's shape",
            )));
        }
        Some(true) => {
            let status = result
                .get("api_error_status")
                .and_then(Value::as_u64)
                .and_then(|s| u16::try_from(s).ok());
            return Err(CliError::Refused(cli, status));
        }
        None => {
            return Err(unreadable(ReplyError::NotACompletion(
                "it doesn't say whether it failed",
            )));
        }
    }
    let answer = result
        .get("structured_output")
        .ok_or_else(|| unreadable(ReplyError::NotACompletion("there's no structured answer")))?;
    reply::answer_of(answer, candidates).map_err(unreadable)
}

/// Whether Claude's error `result` is its giving up on the schema, after
/// answering in the wrong shape too many times.
fn gave_up_on_the_shape(result: &Value) -> bool {
    result
        .get("subtype")
        .and_then(Value::as_str)
        .is_some_and(|subtype| subtype.starts_with("error_max_structured_output"))
}

/// The decisions in what `codex exec` wrote: its last message, the answer
/// alone.
fn codex_answer(ran: &Ran, candidates: usize) -> Result<Vec<Decision>, CliError> {
    let cli = Cli::Codex;
    if !ran.status.success() {
        return Err(CliError::Exited(cli, ran.status));
    }
    let text = std::str::from_utf8(&ran.stdout).map_err(|_| {
        CliError::Unreadable(cli, ReplyError::NotACompletion("the answer isn't text"))
    })?;
    reply::parse_answer(text.trim(), candidates).map_err(|e| CliError::Unreadable(cli, e))
}

/// Tells each schema file apart from another decider's in the same folder.
static SCHEMA_FILES: AtomicU64 = AtomicU64::new(0);

/// A schema file for `codex exec --output-schema`, removed when dropped.
/// It holds the schema only: no question text.
#[derive(Debug)]
struct SchemaFile(PathBuf);

impl SchemaFile {
    /// Writes `schema` to a new file in `dir`, named by its whole path.
    ///
    /// # Errors
    ///
    /// If the file can't be written.
    #[expect(
        clippy::disallowed_methods,
        reason = "a schema file for codex, not a recording or the database"
    )]
    fn write(dir: &Path, schema: &Value) -> Result<Self, CliError> {
        let n = SCHEMA_FILES.fetch_add(1, Ordering::Relaxed);
        // Whole, since Codex runs in `dir` and would read a relative path
        // from there.
        let dir = std::path::absolute(dir).map_err(|e| CliError::Schema(Cli::Codex, e))?;
        let path = dir.join(format!("schema-{}-{n}.json", std::process::id()));
        std::fs::write(&path, schema.to_string()).map_err(|e| CliError::Schema(Cli::Codex, e))?;
        Ok(Self(path))
    }
}

impl Drop for SchemaFile {
    #[expect(
        clippy::disallowed_methods,
        reason = "a schema file for codex, not a recording or the database"
    )]
    fn drop(&mut self) {
        // Already gone is fine.
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod props;
#[cfg(test)]
mod tests;
