//! A stand-in for llama-server, for the decider's supervision tests: it
//! listens where `--host` says, answers `/health` and
//! `/v1/chat/completions` as llama-server does, and misbehaves as told.
//!
//! Its `--model` is a directory of the test's, not a model:
//! - it appends a line to `log` there as it starts (`start <pid> <args>`)
//!   and as a question arrives (`request <pid>`);
//! - `plan` there says what each start does, comma-separated: the first
//!   start does the first, and so on, the last repeating.
//!   - `answer`: says it's loading once, then ready; answers yes to odd
//!     candidates and no to even ones
//!   - `hang`: takes a question and never answers it
//!   - `exit`: exits as a question arrives, saying so on stderr
//!   - `garbage`: answers with a body that isn't JSON
//!   - `refuse`: answers 400, as for a question too long for its context
//!   - `busy`: answers 500
//!   - `never-ready`: says it's loading, forever
//!   - `crash`: exits at once, saying the model is missing
//!   - `trickle`: answers, a byte every 20 ms
//!   - `huge`: answers with a length over nota's limit
//!
//! `nota-fake-llama-server exec-tied PROGRAM ARGS…` is the launch shim
//! nota uses (`nota_llm::lifeline::exec_tied`), so the tests can start it
//! as nota starts the real server.
//!
//! Unix only, as the decider is: elsewhere it exits at once with an error.

use std::process::ExitCode;

#[cfg(unix)]
mod unix;

#[cfg(unix)]
fn main() -> ExitCode {
    unix::main()
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    ExitCode::from(2)
}
