//! The local decider: questions put to a llama-server it runs itself.
//!
//! The server starts with the first question and keeps running until the
//! decider is dropped or [`LocalDecider::stop_server`] is called; a job
//! drops its decider when it ends, so nothing is left loaded between jobs.
//! `stop` is only seen while a question is being decided, so a job whose
//! `stop` says to stop between questions must drop its decider (or stop
//! its server) then: the decider can't see it.
//!
//! What happens to a question:
//! - **`stop` says to stop** (a recording started, or nota is closing):
//!   the server is killed at once, and the question comes back
//!   [`DecideError::Stopped`]. No server is started while `stop` says to.
//!   If it says to just as the answer has come, the answer stands and the
//!   server is killed.
//! - **The server fails** (it exits, hangs past
//!   [`ServerConfig::request_timeout`], doesn't load within
//!   [`ServerConfig::start_timeout`], or answers with a server error): it's
//!   killed and started again after a backoff, and the question is asked
//!   again, up to [`ServerConfig::restarts`] times. After that the decider
//!   is [`DecideError::Unavailable`], with the last reason. So is a program
//!   that can't be started at all, at once, with or without a launcher.
//! - **The answer can't be read** (a malformed reply, a reply cut short or
//!   over its limit, a request the server refuses): a failed decision,
//!   [`DecideError::Failed`]. It isn't asked again, since the model at
//!   temperature 0 would answer the same; the server keeps running for the
//!   next question.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use nota_core::Clock;
use nota_core::decide::{DecideError, Decider, Decisions, Place, Question};

use crate::http::Response;
use crate::prompt::Prompt;
use crate::reply;
use crate::request;
use crate::server::{Interrupted, Server, ServerConfig, ServerError, Spawner, pause, said};

/// The server refused the request (a 4xx status): too long for the
/// context, say. Asking again gets the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The status code.
    pub status: u16,
    /// The start of what it said.
    pub said: String,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "llama-server refused the question ({}): {}",
            self.status, self.said
        )
    }
}

impl Error for Refused {}

/// What one try at a question ended in, short of an answer.
#[derive(Debug)]
enum Trouble {
    /// `stop` said to stop.
    Stopped,
    /// The server failed; another may do better.
    Broken(ServerError),
    /// No server can run (a missing program, an unusable socket directory):
    /// trying again won't help.
    Hopeless(ServerError),
    /// The server's response couldn't be read; the server is fine, and
    /// would answer the same again.
    Unreadable(ServerError),
}

impl From<Interrupted> for Trouble {
    fn from(interrupted: Interrupted) -> Self {
        match interrupted {
            Interrupted::Stopped => Self::Stopped,
            Interrupted::Failed(e @ ServerError::Unreadable(_)) => Self::Unreadable(e),
            Interrupted::Failed(e) => Self::Broken(e),
        }
    }
}

/// A decider that runs Qwen (or any model) in a llama-server of its own.
#[derive(Debug)]
pub struct LocalDecider {
    /// How to run the server.
    config: ServerConfig,
    /// The words for the session's language.
    prompt: Box<dyn Prompt>,
    /// The session clock, for every timeout.
    clock: Arc<dyn Clock>,
    /// The server, while one runs. Declared before the spawner, so it's
    /// dropped (killed) before the thread that started it ends.
    server: Option<Server>,
    /// The thread that starts servers, from the first start on.
    spawner: Option<Spawner>,
    /// How many servers it has started: each gets its own socket.
    started: u64,
}

impl LocalDecider {
    /// A decider that runs a server as `config` says, asking in `prompt`'s
    /// words, timed by `clock`. Nothing starts until the first question.
    #[must_use]
    pub fn new(config: ServerConfig, prompt: Box<dyn Prompt>, clock: Arc<dyn Clock>) -> Self {
        Self {
            config,
            prompt,
            clock,
            server: None,
            spawner: None,
            started: 0,
        }
    }

    /// Stops the server, if one is running. The next question starts
    /// another.
    pub fn stop_server(&mut self) {
        self.server = None;
    }

    /// The running server, started and loaded if there wasn't one.
    fn running(&mut self, stop: &dyn Fn() -> bool) -> Result<&mut Server, Trouble> {
        let server = match self.server.take() {
            Some(server) => server,
            None => self.start(stop)?,
        };
        Ok(self.server.insert(server))
    }

    /// Starts a server and waits until it has loaded its model.
    fn start(&mut self, stop: &dyn Fn() -> bool) -> Result<Server, Trouble> {
        if stop() {
            return Err(Trouble::Stopped);
        }
        self.started += 1;
        let spawner = match self.spawner.take() {
            Some(spawner) => spawner,
            None => Spawner::new().map_err(|e| Trouble::Hopeless(ServerError::Spawn(e)))?,
        };
        let spawner = self.spawner.insert(spawner);
        let mut server =
            Server::start(&self.config, spawner, self.started).map_err(Trouble::Hopeless)?;
        server.wait_ready(&*self.clock, self.config.start_timeout, stop)?;
        Ok(server)
    }

    /// One try at `body`: the server's response, once it's one that a
    /// restart wouldn't change (an answer, or a refusal).
    fn try_once(&mut self, body: &[u8], stop: &dyn Fn() -> bool) -> Result<Response, Trouble> {
        let (clock, limit) = (Arc::clone(&self.clock), self.config.request_timeout);
        let response = self.running(stop)?.ask(body, &*clock, limit, stop)?;
        if response.status >= 500 {
            return Err(Trouble::Broken(ServerError::Status(
                response.status,
                said(&response.body),
            )));
        }
        Ok(response)
    }

    /// The wait before restart number `failures` (from 1): the first
    /// backoff, doubled for each failure before it, up to the longest.
    fn backoff(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        self.config
            .initial_backoff
            .saturating_mul(1 << doublings)
            .min(self.config.max_backoff)
    }
}

/// The decisions in `response`, the server's answer to `question`.
fn read(question: &Question, response: &Response) -> Result<Decisions, DecideError> {
    if response.status != 200 {
        return Err(DecideError::Failed(Box::new(Refused {
            status: response.status,
            said: said(&response.body),
        })));
    }
    let decisions = reply::parse(&response.body, question.len())
        .map_err(|e| DecideError::Failed(Box::new(e)))?;
    Decisions::new(question, decisions).map_err(|e| DecideError::Failed(Box::new(e)))
}

impl Decider for LocalDecider {
    fn place(&self) -> Place {
        Place::Local
    }

    fn decide(
        &mut self,
        question: &Question,
        stop: &dyn Fn() -> bool,
    ) -> Result<Decisions, DecideError> {
        if stop() {
            self.stop_server();
            return Err(DecideError::Stopped);
        }
        let body = request::body(&*self.prompt, question);
        let mut failures = 0;
        loop {
            let trouble = match self.try_once(&body, stop) {
                Ok(response) => {
                    let decided = read(question, &response);
                    // A stop that came with the answer: the answer stands,
                    // but no server stays loaded for a recording.
                    if stop() {
                        self.stop_server();
                    }
                    return decided;
                }
                Err(Trouble::Unreadable(e)) => return Err(DecideError::Failed(Box::new(e))),
                Err(trouble) => trouble,
            };
            // Whatever went wrong, this server is done with.
            self.stop_server();
            let failed = match trouble {
                Trouble::Stopped => return Err(DecideError::Stopped),
                Trouble::Hopeless(e) => return Err(DecideError::Unavailable(Box::new(e))),
                Trouble::Broken(e) | Trouble::Unreadable(e) => e,
            };
            failures += 1;
            if failures > self.config.restarts {
                return Err(DecideError::Unavailable(Box::new(ServerError::GaveUp(
                    failures,
                    Box::new(failed),
                ))));
            }
            if pause(&*self.clock, self.backoff(failures), stop).is_err() {
                return Err(DecideError::Stopped);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nota_core::SystemClock;

    use super::*;
    use crate::prompt::English;
    use crate::server::{Device, SAID_LEN};

    /// A decider whose backoff runs from `first` to `longest`.
    fn decider(first: Duration, longest: Duration) -> LocalDecider {
        let mut config =
            ServerConfig::new(PathBuf::new(), PathBuf::new(), Device::Cpu, PathBuf::new());
        config.initial_backoff = first;
        config.max_backoff = longest;
        let clock = Arc::new(SystemClock::start().unwrap());
        LocalDecider::new(config, Box::new(English::new(None)), clock)
    }

    /// The backoff doubles from the first wait and stops at the longest.
    #[test]
    fn the_backoff_doubles_up_to_the_longest() {
        let d = decider(Duration::from_millis(500), Duration::from_secs(10));
        let waits: Vec<u128> = (1..=7).map(|n| d.backoff(n).as_millis()).collect();
        assert_eq!(waits, [500, 1_000, 2_000, 4_000, 8_000, 10_000, 10_000]);
        assert_eq!(d.backoff(u32::MAX), Duration::from_secs(10));
    }

    /// It says it runs on this machine.
    #[test]
    fn it_runs_locally() {
        assert_eq!(
            decider(Duration::ZERO, Duration::ZERO).place(),
            Place::Local
        );
    }

    /// A refusal is a failed decision that keeps the start of what the
    /// server said.
    #[test]
    fn a_refusal_is_a_failed_decision() {
        let question = Question::new(
            None,
            "x".to_owned(),
            vec![nota_core::decide::Candidate::new("a".to_owned(), "b".to_owned(), None).unwrap()],
        )
        .unwrap();
        let said = "x".repeat(SAID_LEN + 50);
        let response = Response {
            status: 400,
            body: said.clone().into_bytes(),
        };
        let Err(DecideError::Failed(e)) = read(&question, &response) else {
            panic!("read a refusal")
        };
        let refused = e.downcast_ref::<Refused>().unwrap();
        assert_eq!(refused.status, 400);
        assert_eq!(refused.said, said[..SAID_LEN]);
    }
}
