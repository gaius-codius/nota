//! The decider on any OpenAI-compatible server that nota doesn't run
//! itself: llama-server on another machine, Ollama, or a remote server.
//!
//! It sends the local decider's request, with the same HTTP client, over
//! TCP rather than a UNIX socket, to the base URL the user gives, as
//! the usual clients take one (`http://localhost:11434/v1`), and reads the
//! reply with the same parser. Only plain `http://` for now, with no key:
//! a server that needs TLS or a key waits for API keys, in Settings.
//!
//! Where it runs is told by the URL alone: on this machine only for
//! `localhost` or a loopback address; anything else, a machine on the
//! same network included, is the cloud (☁), since the transcript leaves
//! this machine.
//!
//! What happens to a question:
//! - **`stop` says to stop:** the connection is dropped, and the question
//!   comes back [`DecideError::Stopped`].
//! - **The server can't be reached, doesn't answer within
//!   [`EndpointConfig::timeout`], or fails** (a 5xx status, a connection
//!   that breaks): [`DecideError::Unavailable`]. It isn't asked again;
//!   nota doesn't run this server, so it can't restart it.
//! - **The answer can't be read** (a malformed reply, a refusal such as an
//!   unknown model, a reply cut short): a failed decision,
//!   [`DecideError::Failed`].
//!
//! No reason holds what the server said, only its status: a refusal may
//! quote the question back.

use std::error::Error;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv6Addr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use nota_core::Clock;
use nota_core::decide::{DecideError, Decider, Decisions, Place, Question};

use crate::http::{self, HttpError, Response};
use crate::prompt::Prompt;
use crate::reply::{self, ReplyError};
use crate::request;
use crate::server::{Deadline, POLL, Patient};

/// How long a server has to answer by default: a big model on another
/// machine can take a minute.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest base URL taken, in bytes.
pub const MAX_URL_LEN: usize = 2_048;

/// Where a server is: its host, port, and the path its API is
/// under. Only ever made by [`Endpoint::parse`], so the host and path are
/// safe to write into a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The host as written: a name, an IPv4 address, or an IPv6 one in
    /// brackets.
    host: String,
    /// The port.
    port: u16,
    /// The path the API is under, without a slash at its end: `/v1`, or
    /// empty.
    base: String,
}

/// Why a base URL wasn't taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    /// It starts `https://`, which nota can't speak yet.
    Https,
    /// It doesn't start `http://`.
    NotHttp,
    /// It names a user or password, which is a key, and keys wait for
    /// Settings.
    Credentials,
    /// Its host is missing, or isn't a name or an address.
    BadHost,
    /// Its port isn't a number from 1 to 65535.
    BadPort,
    /// Its path has a character other than letters, digits and `-._~/%`.
    BadPath,
    /// It's longer than [`MAX_URL_LEN`].
    TooLong,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Https => "https isn't supported yet: use an http:// address",
            Self::NotHttp => "the address must start with http://",
            Self::Credentials => "the address can't hold a user name or password",
            Self::BadHost => "the address has no host name, or one that isn't valid",
            Self::BadPort => "the address's port isn't a number from 1 to 65535",
            Self::BadPath => "the address's path has characters that aren't allowed",
            Self::TooLong => "the address is too long",
        })
    }
}

impl Error for UrlError {}

impl Endpoint {
    /// The server at `url`, a base URL such as `http://localhost:11434/v1`.
    ///
    /// # Errors
    ///
    /// If `url` isn't a plain `http://` URL with a valid host; see
    /// [`UrlError`].
    pub fn parse(url: &str) -> Result<Self, UrlError> {
        if url.len() > MAX_URL_LEN {
            return Err(UrlError::TooLong);
        }
        let scheme = url.get(..8).map(str::to_ascii_lowercase);
        if scheme.as_deref() == Some("https://") {
            return Err(UrlError::Https);
        }
        let rest = match url.get(..7) {
            Some(s) if s.eq_ignore_ascii_case("http://") => url.get(7..).unwrap_or_default(),
            _ => return Err(UrlError::NotHttp),
        };
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        if authority.contains('@') {
            return Err(UrlError::Credentials);
        }
        let (host, port) = host_and_port(authority)?;
        let path_ok = path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~/%".contains(&b));
        if !path_ok {
            return Err(UrlError::BadPath);
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            base: path.trim_end_matches('/').to_owned(),
        })
    }

    /// Where a decider on this server runs: on this machine for
    /// `localhost` or a loopback address, otherwise the cloud.
    #[must_use]
    pub fn place(&self) -> Place {
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if loopback { Place::Local } else { Place::Cloud }
    }

    /// The `Host` header's value: the host, and the port unless it's 80.
    fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The host as a socket address takes it: without an IPv6 address's
    /// brackets.
    fn bare_host(&self) -> &str {
        self.host.trim_start_matches('[').trim_end_matches(']')
    }

    /// The path questions are posted to.
    fn chat_path(&self) -> String {
        format!("{}/chat/completions", self.base)
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "http://{}:{}{}", self.host, self.port, self.base)
    }
}

/// The host and port in `authority` (`host`, `host:port`, `[v6]:port`).
fn host_and_port(authority: &str) -> Result<(&str, u16), UrlError> {
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']').ok_or(UrlError::BadHost)?;
        let (host, after) = authority.split_at(end + 1);
        let inner = host.get(1..host.len() - 1).unwrap_or_default();
        inner.parse::<Ipv6Addr>().map_err(|_| UrlError::BadHost)?;
        match after.strip_prefix(':') {
            Some(port) => (host, Some(port)),
            None if after.is_empty() => (host, None),
            None => return Err(UrlError::BadHost),
        }
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let name_ok = authority.starts_with('[')
        || (!host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'));
    if !name_ok {
        return Err(UrlError::BadHost);
    }
    let port = match port {
        None => 80,
        Some(port) => port
            .parse::<u16>()
            .ok()
            .filter(|&p| p != 0)
            .ok_or(UrlError::BadPort)?,
    };
    Ok((host, port))
}

/// How to reach a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointConfig {
    /// Where it is.
    pub endpoint: Endpoint,
    /// The model to ask for, or `None` for a server that runs one.
    pub model: Option<String>,
    /// How long it has to answer a question, from the start of the
    /// connection.
    pub timeout: Duration,
}

impl EndpointConfig {
    /// The server at `endpoint`, with no model named and
    /// [`DEFAULT_TIMEOUT`].
    #[must_use]
    pub const fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            model: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Why a server gave no decisions. None of these holds anything the
/// server said.
#[derive(Debug)]
pub enum EndpointError {
    /// It couldn't be reached at this address: no such host, nothing
    /// listening, or the connection timed out.
    Unreachable(String, io::Error),
    /// The thread that connects couldn't be started.
    Spawn(io::Error),
    /// It hadn't answered within this long.
    NoAnswer(Duration),
    /// The connection broke before the answer was read.
    Connection(HttpError),
    /// It failed with this server error status (5xx).
    Failed(u16),
    /// It refused the question with this status (a 4xx: an unknown model,
    /// a question too long).
    Refused(u16),
    /// Its response wasn't HTTP as expected, or was too long.
    Unreadable(HttpError),
    /// Its reply wasn't one yes or no per candidate.
    Reply(ReplyError),
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(at, e) => write!(f, "the server at {at} can't be reached: {e}"),
            Self::Spawn(e) => write!(f, "the connection couldn't be started: {e}"),
            Self::NoAnswer(limit) => {
                write!(f, "the server gave no answer within {limit:?}")
            }
            Self::Connection(e) => write!(f, "the connection to the server failed: {e}"),
            Self::Failed(status) => write!(f, "the server failed (status {status})"),
            Self::Refused(status) => write!(f, "the server refused the question (status {status})"),
            Self::Unreadable(e) => write!(f, "the server's response couldn't be read: {e}"),
            Self::Reply(e) => write!(f, "the server's answer couldn't be used: {e}"),
        }
    }
}

impl Error for EndpointError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unreachable(_, e) | Self::Spawn(e) => Some(e),
            Self::Connection(e) | Self::Unreadable(e) => Some(e),
            Self::Reply(e) => Some(e),
            _ => None,
        }
    }
}

impl EndpointError {
    /// The decider's error for this: a failed decision for an answer that
    /// couldn't be used, otherwise an unavailable decider.
    fn into_decide(self) -> DecideError {
        match self {
            Self::Refused(_) | Self::Unreadable(_) | Self::Reply(_) => {
                DecideError::Failed(Box::new(self))
            }
            _ => DecideError::Unavailable(Box::new(self)),
        }
    }
}

/// What asking came to, short of a response.
#[derive(Debug)]
enum Asked {
    /// `stop` said to stop.
    Stopped,
    /// The server failed, or its answer couldn't be used.
    Error(EndpointError),
}

impl From<EndpointError> for Asked {
    fn from(e: EndpointError) -> Self {
        Self::Error(e)
    }
}

/// A decider on an OpenAI-compatible server that nota doesn't run.
#[derive(Debug)]
pub struct EndpointDecider {
    /// How to reach the server.
    config: EndpointConfig,
    /// The words for the session's language.
    prompt: Box<dyn Prompt>,
    /// The session clock, for the time limit.
    clock: Arc<dyn Clock>,
}

impl EndpointDecider {
    /// A decider that asks the server `config` names, in `prompt`'s words,
    /// timed by `clock`. Nothing connects until the first question.
    #[must_use]
    pub fn new(config: EndpointConfig, prompt: Box<dyn Prompt>, clock: Arc<dyn Clock>) -> Self {
        Self {
            config,
            prompt,
            clock,
        }
    }

    /// Posts `question` and reads the server's response.
    fn ask(&self, question: &Question, stop: &dyn Fn() -> bool) -> Result<Response, Asked> {
        let clock = &*self.clock;
        let until = Deadline::after(clock, self.config.timeout);
        let endpoint = &self.config.endpoint;
        let body = request::body(&*self.prompt, question, self.config.model.as_deref());
        let request = http::request(&endpoint.host_header(), &endpoint.chat_path(), Some(&body));
        let connecting = connect(endpoint, self.config.timeout)?;
        let mut stream = self.connected(&connecting, until, stop)?;
        let timeouts = stream
            .set_read_timeout(Some(POLL))
            .and_then(|()| stream.set_write_timeout(Some(POLL)));
        timeouts.map_err(|e| EndpointError::Connection(HttpError::Io(e)))?;
        let mut why = None;
        let result = {
            let mut patient = Patient {
                stream: &mut stream,
                check: &mut || match self.check(until, stop) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        why = Some(e);
                        Err(io::Error::other("interrupted"))
                    }
                },
            };
            http::exchange(&mut patient, &request)
        };
        match (result, why) {
            (_, Some(asked)) => Err(asked),
            (Ok(response), None) => Ok(response),
            (Err(e @ (HttpError::BadHead(_) | HttpError::TooLong(_))), None) => {
                Err(EndpointError::Unreadable(e).into())
            }
            (Err(e), None) => Err(EndpointError::Connection(e).into()),
        }
    }

    /// The connection `connecting` makes, once it has.
    fn connected(
        &self,
        connecting: &Receiver<io::Result<TcpStream>>,
        until: Deadline,
        stop: &dyn Fn() -> bool,
    ) -> Result<TcpStream, Asked> {
        let at = || self.config.endpoint.to_string();
        loop {
            self.check(until, stop)?;
            match connecting.recv_timeout(POLL) {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(e)) => return Err(EndpointError::Unreachable(at(), e).into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let e = io::Error::other("the thread that connects stopped");
                    return Err(EndpointError::Unreachable(at(), e).into());
                }
            }
        }
    }

    /// Whether to go on waiting: not once `stop` says to, or `until`
    /// passes.
    fn check(&self, until: Deadline, stop: &dyn Fn() -> bool) -> Result<(), Asked> {
        if stop() {
            return Err(Asked::Stopped);
        }
        if until.passed(&*self.clock) {
            return Err(EndpointError::NoAnswer(self.config.timeout).into());
        }
        Ok(())
    }
}

/// Starts connecting to `endpoint` on a thread, which sends the connection
/// when it's made. Looking up a name can't be timed or stopped, so it's
/// done there too; the thread ends by itself, within `limit` of the
/// lookup, if nobody waits for it.
fn connect(endpoint: &Endpoint, limit: Duration) -> Result<Receiver<io::Result<TcpStream>>, Asked> {
    let (host, port) = (endpoint.bare_host().to_owned(), endpoint.port);
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("nota-llm-connect".into())
        .spawn(move || {
            // Nobody may be waiting any more; that's fine.
            let _ = tx.send(connect_now(&host, port, limit));
        })
        .map_err(EndpointError::Spawn)?;
    Ok(rx)
}

/// A connection to the first address of `host` that takes one within
/// `limit`.
fn connect_now(host: &str, port: u16, limit: Duration) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "the name has no address");
    for address in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, limit) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The decisions in `response`, the server's answer to `question`.
fn read(question: &Question, response: &Response) -> Result<Decisions, EndpointError> {
    match response.status {
        200 => {}
        status @ 500.. => return Err(EndpointError::Failed(status)),
        status => return Err(EndpointError::Refused(status)),
    }
    let decisions = reply::parse(&response.body, question.len()).map_err(EndpointError::Reply)?;
    // The parser gives one per candidate, so a wrong count is an answer
    // that can't be used.
    Decisions::new(question, decisions)
        .map_err(|_| EndpointError::Reply(ReplyError::NotDecisions("not one per candidate")))
}

impl Decider for EndpointDecider {
    /// Where the URL says: see [`Endpoint::place`].
    fn place(&self) -> Place {
        self.config.endpoint.place()
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
            Ok(response) => read(question, &response).map_err(EndpointError::into_decide),
            Err(Asked::Stopped) => Err(DecideError::Stopped),
            Err(Asked::Error(e)) => Err(e.into_decide()),
        }
    }
}

#[cfg(test)]
mod props;
#[cfg(test)]
mod tests;
