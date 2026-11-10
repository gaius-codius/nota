//! One running llama-server: starting it, waiting until it has loaded its
//! model, asking it things, and stopping it.
//!
//! The server is the stock `llama-server` from llama.cpp, which nota can't
//! change, so it's held from outside:
//! - **Its own socket.** It listens on a UNIX socket in a directory only
//!   this user can open ([`ServerConfig::socket_dir`]), a new path for each
//!   start: a server that was killed leaves its socket file behind, and
//!   llama-server won't bind over one.
//! - **One device, named.** It's always told which device to use
//!   ([`Device`]): left to itself, llama.cpp spreads a model across every
//!   GPU it finds.
//! - **Its output captured.** Stdout is dropped and the end of stderr kept,
//!   for the reason it stopped: the screens own the terminal. A thread reads
//!   stderr as it comes, so a full pipe can't stall the server.
//! - **Its own process group**, so a signal to nota's group (the terminal's
//!   SIGHUP) doesn't reach it: only nota decides when it stops.
//! - **Ended with nota.** llama-server can't tie itself to nota as the
//!   speech engine does ([`nota_core::lifeline`]), so it's started through a
//!   launcher that ties itself and then becomes the server
//!   ([`ServerConfig::launcher`], normally `nota engine llama-server`; see
//!   [`crate::lifeline`]). The kernel kills it when the thread that started
//!   it ends, so servers are started by a thread of their own (`Spawner`)
//!   that outlives them.
//!
//! Every wait here is bounded and watches `stop`, at least every [`POLL`].

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::Clock;
use nota_core::SessionTime;
use nota_core::lifeline::RECORDER_PID_VAR;

use crate::http::{self, HttpError, Response};

/// How often a wait looks at `stop`, the deadline and the server.
pub const POLL: Duration = Duration::from_millis(50);

/// How much of the end of the server's stderr is kept.
pub const STDERR_TAIL: usize = 2_048;

/// How long a broken connection waits to see whether the server exited.
const EXIT_WAIT: Duration = Duration::from_millis(500);

/// The longest socket path a UNIX socket takes, less its closing zero.
const MAX_SOCKET_PATH: usize = 107;

/// A GPU's name as llama-server lists it (`llama-server --list-devices`):
/// `Vulkan0`, `CUDA1`. One device, so never a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuName(String);

impl GpuName {
    /// The GPU named `name`, or `None` if it's blank, a list (has a comma)
    /// or `none` (llama-server's word for the CPU).
    #[must_use]
    pub fn new(name: &str) -> Option<Self> {
        let name = name.trim();
        let usable = !name.is_empty() && !name.contains(',') && !name.eq_ignore_ascii_case("none");
        usable.then(|| Self(name.to_owned()))
    }

    /// The name, as llama-server takes it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where the model runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Device {
    /// On the CPU alone.
    Cpu,
    /// All of it on one GPU.
    Gpu(GpuName),
}

impl Device {
    /// llama-server's arguments for this device.
    fn args(&self) -> [&str; 4] {
        match self {
            Self::Cpu => ["--device", "none", "--n-gpu-layers", "0"],
            Self::Gpu(name) => ["--device", name.as_str(), "--n-gpu-layers", "999"],
        }
    }
}

/// A command that starts the server for nota: `program args… server
/// server-args…`. It must end up running the server as the same process
/// (by exec), so the process nota started is the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher {
    /// The program: normally the `nota` executable itself.
    pub program: PathBuf,
    /// Its arguments before the server's program: `engine llama-server`.
    pub args: Vec<OsString>,
}

/// How to run llama-server, and how long to wait on it.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The `llama-server` program. Until nota downloads its own (M7), from
    /// the user's settings.
    pub program: PathBuf,
    /// The model file (GGUF). Likewise from the settings for now.
    pub model: PathBuf,
    /// Where the model runs.
    pub device: Device,
    /// The context, in tokens.
    pub context: u32,
    /// The directory the server's socket goes in. It's made, readable only
    /// by this user, if it isn't there; one that others can open is
    /// refused.
    pub socket_dir: PathBuf,
    /// What starts the server, tying it to nota; `None` starts it directly,
    /// untied, so it outlives a nota that dies.
    pub launcher: Option<Launcher>,
    /// How long a new server has to load its model and say it's ready.
    pub start_timeout: Duration,
    /// How long one question may take to answer.
    pub request_timeout: Duration,
    /// The wait before the first restart; it doubles with each.
    pub initial_backoff: Duration,
    /// The longest wait before a restart.
    pub max_backoff: Duration,
    /// How many times one question restarts a failed server before the
    /// decider gives up.
    pub restarts: u32,
}

impl ServerConfig {
    /// The defaults for `program` running `model` on `device`, with its
    /// socket in `socket_dir`: a 48k context (49,152 tokens), 120 s to load,
    /// 120 s per question (a CPU takes about 10 s for one), and up to 3
    /// restarts per question, after waits from 500 ms to 10 s.
    #[must_use]
    pub const fn new(
        program: PathBuf,
        model: PathBuf,
        device: Device,
        socket_dir: PathBuf,
    ) -> Self {
        Self {
            program,
            model,
            device,
            context: 49_152,
            socket_dir,
            launcher: None,
            start_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_secs(120),
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(10),
            restarts: 3,
        }
    }

    /// The command that starts a server listening on `socket`.
    fn command(&self, socket: &Path) -> Command {
        let mut args: Vec<OsString> = vec![
            "--model".into(),
            self.model.clone().into(),
            "--host".into(),
            socket.into(),
        ];
        args.extend(self.device.args().map(OsString::from));
        args.extend(
            [
                "--ctx-size",
                &self.context.to_string(),
                "--parallel",
                "1",
                "--no-webui",
                "--offline",
            ]
            .map(OsString::from),
        );
        let mut command = match &self.launcher {
            Some(launcher) => {
                let mut c = Command::new(&launcher.program);
                c.args(&launcher.args).arg(&self.program);
                c
            }
            None => Command::new(&self.program),
        };
        command.args(args);
        command
    }
}

/// Why a server isn't working.
#[derive(Debug)]
pub enum ServerError {
    /// The program couldn't be started (it's missing, or not executable).
    Spawn(io::Error),
    /// The socket directory couldn't be made or read.
    SocketDir(PathBuf, io::Error),
    /// The socket directory can be opened by other users: its path.
    SocketDirOpen(PathBuf),
    /// The socket's path is too long for a UNIX socket: the path.
    SocketPathTooLong(PathBuf),
    /// The server exited: how, if known, and the end of its stderr.
    Exited(Option<ExitStatus>, String),
    /// It didn't say it was ready within the start timeout, which it's
    /// given here.
    NotReady(Duration),
    /// It didn't answer within the request timeout, given here.
    NoAnswer(Duration),
    /// Talking to it failed.
    Connection(HttpError),
    /// It answered with a server error: the status code.
    Status(u16),
    /// It failed this many times in a row on one question; the last
    /// failure.
    GaveUp(u32, Box<Self>),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "llama-server couldn't be started: {e}"),
            Self::SocketDir(dir, e) => write!(
                f,
                "the socket directory {} can't be used: {e}",
                dir.display()
            ),
            Self::SocketDirOpen(dir) => {
                write!(
                    f,
                    "the socket directory {} is open to other users",
                    dir.display()
                )
            }
            Self::SocketPathTooLong(path) => {
                write!(
                    f,
                    "the socket path {} is too long for a socket",
                    path.display()
                )
            }
            Self::Exited(status, tail) => {
                match status {
                    Some(status) => write!(f, "llama-server stopped ({status})")?,
                    None => f.write_str("llama-server stopped")?,
                }
                if tail.trim().is_empty() {
                    Ok(())
                } else {
                    write!(f, ": {}", tail.trim())
                }
            }
            Self::NotReady(limit) => {
                write!(f, "llama-server wasn't ready within {} s", limit.as_secs())
            }
            Self::NoAnswer(limit) => {
                write!(f, "llama-server didn't answer within {} s", limit.as_secs())
            }
            Self::Connection(e) => write!(f, "llama-server's connection failed: {e}"),
            Self::Status(status) => write!(f, "llama-server answered with error {status}"),
            Self::GaveUp(times, last) => {
                write!(f, "llama-server failed {times} times in a row: {last}")
            }
        }
    }
}

impl Error for ServerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(e) | Self::SocketDir(_, e) => Some(e),
            Self::Connection(e) => Some(e),
            Self::GaveUp(_, last) => Some(&**last),
            _ => None,
        }
    }
}

/// Why a wait on the server ended without what it waited for.
#[derive(Debug)]
pub(crate) enum Interrupted {
    /// `stop` said to stop.
    Stopped,
    /// The server failed.
    Failed(ServerError),
}

impl From<ServerError> for Interrupted {
    fn from(e: ServerError) -> Self {
        Self::Failed(e)
    }
}

/// What a wait on the server is for, and how long it may take.
#[derive(Debug, Clone, Copy)]
enum Waiting {
    /// For it to load its model and say it's ready.
    Ready(Duration),
    /// For its answer to a question.
    Answer(Duration),
}

impl Waiting {
    /// The error once the wait has taken too long.
    const fn late(self) -> ServerError {
        match self {
            Self::Ready(limit) => ServerError::NotReady(limit),
            Self::Answer(limit) => ServerError::NoAnswer(limit),
        }
    }
}

/// A point in session time to wait until; never, if it's past the end of
/// time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline(Option<SessionTime>);

impl Deadline {
    /// The time `wait` from now.
    pub(crate) fn after(clock: &dyn Clock, wait: Duration) -> Self {
        Self(clock.now().checked_add(wait))
    }

    /// Whether it has passed.
    pub(crate) fn passed(self, clock: &dyn Clock) -> bool {
        self.0.is_some_and(|at| clock.now() >= at)
    }
}

/// Waits `wait`, or less if `stop` says to stop.
///
/// # Errors
///
/// [`Interrupted::Stopped`] if `stop` said to.
pub(crate) fn pause(
    clock: &dyn Clock,
    wait: Duration,
    stop: &dyn Fn() -> bool,
) -> Result<(), Interrupted> {
    let until = Deadline::after(clock, wait);
    // Nothing is ever sent: the channel only gives a wait that wakes in
    // time to look at `stop` again.
    let (_keep, ticks) = mpsc::channel::<()>();
    while !until.passed(clock) {
        if stop() {
            return Err(Interrupted::Stopped);
        }
        let _ = ticks.recv_timeout(POLL);
    }
    Ok(())
}

/// Starts servers from a thread of its own that outlives them, so the
/// kernel's tie to the thread that started each one holds while it runs.
#[derive(Debug)]
pub(crate) struct Spawner {
    /// Commands to start, each with where to send the result.
    commands: Option<Sender<(Command, SyncSender<io::Result<Child>>)>>,
    /// The thread, joined once the spawner is dropped.
    thread: Option<JoinHandle<()>>,
}

impl Spawner {
    /// Starts the thread.
    pub(crate) fn new() -> io::Result<Self> {
        let (commands, rx) = mpsc::channel::<(Command, SyncSender<io::Result<Child>>)>();
        let thread = thread::Builder::new()
            .name("nota-llama-spawner".into())
            .spawn(move || {
                for (mut command, reply) in rx {
                    // The caller waits for this, so the send only fails if
                    // its thread has gone, and then nobody holds the child.
                    let _ = reply.send(command.spawn());
                }
            })?;
        Ok(Self {
            commands: Some(commands),
            thread: Some(thread),
        })
    }

    /// Starts `command` on the spawner's thread.
    fn spawn(&self, command: Command) -> io::Result<Child> {
        let (reply, result) = mpsc::sync_channel(1);
        let gone = || io::Error::other("the thread that starts llama-server has stopped");
        self.commands
            .as_ref()
            .ok_or_else(gone)?
            .send((command, reply))
            .map_err(|_| gone())?;
        result.recv().map_err(|_| gone())?
    }
}

impl Drop for Spawner {
    fn drop(&mut self) {
        // Closing the channel ends the thread's loop.
        self.commands = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A running llama-server.
#[derive(Debug)]
pub(crate) struct Server {
    /// The process; `None` once it's been reaped.
    child: Option<Child>,
    /// Its socket.
    socket: PathBuf,
    /// The end of its stderr, once it has closed.
    stderr: Receiver<String>,
}

impl Server {
    /// Starts a server from `config` with `spawner`, its socket named for
    /// `generation`.
    ///
    /// # Errors
    ///
    /// If the socket directory can't be used, or the program can't be
    /// started.
    pub(crate) fn start(
        config: &ServerConfig,
        spawner: &Spawner,
        generation: u64,
    ) -> Result<Self, ServerError> {
        private_dir(&config.socket_dir)?;
        let socket = config
            .socket_dir
            .join(format!("llama-{}-{generation}.sock", std::process::id()));
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(ServerError::SocketPathTooLong(socket));
        }
        let mut command = config.command(&socket);
        command
            .env(RECORDER_PID_VAR, std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // A process group of its own: see the module's notes.
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = spawner.spawn(command).map_err(ServerError::Spawn)?;
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ServerError::Spawn(io::Error::other(
                "started without its stderr",
            )));
        };
        let stderr = keep_tail(stderr).map_err(|e| {
            let _ = child.kill();
            let _ = child.wait();
            ServerError::Spawn(e)
        })?;
        Ok(Self {
            child: Some(child),
            socket,
            stderr,
        })
    }

    /// Waits until it says it's ready, for up to `limit`.
    ///
    /// # Errors
    ///
    /// If `stop` says to stop, it exits, it isn't ready in time, or its
    /// health check answers with an error.
    pub(crate) fn wait_ready(
        &mut self,
        clock: &dyn Clock,
        limit: Duration,
        stop: &dyn Fn() -> bool,
    ) -> Result<(), Interrupted> {
        let until = Deadline::after(clock, limit);
        let waiting = Waiting::Ready(limit);
        loop {
            self.check(clock, until, stop, waiting)?;
            match self.exchange(&http::request("/health", None), clock, until, stop, waiting) {
                Ok(Response { status: 200, .. }) => return Ok(()),
                Ok(Response { status, .. }) if status != 503 => {
                    return Err(ServerError::Status(status).into());
                }
                Err(Interrupted::Stopped) => return Err(Interrupted::Stopped),
                // 503: still loading its model. A failure: not listening
                // yet, or it stopped, which the next check says.
                Ok(_) | Err(Interrupted::Failed(_)) => {}
            }
            pause(clock, POLL, stop)?;
        }
    }

    /// Posts `body` to its chat endpoint and reads the answer, waiting up
    /// to `limit`.
    ///
    /// # Errors
    ///
    /// If `stop` says to stop, it exits, it doesn't answer in time, or the
    /// connection fails.
    pub(crate) fn ask(
        &mut self,
        body: &[u8],
        clock: &dyn Clock,
        limit: Duration,
        stop: &dyn Fn() -> bool,
    ) -> Result<Response, Interrupted> {
        let until = Deadline::after(clock, limit);
        let request = http::request("/v1/chat/completions", Some(body));
        self.exchange(&request, clock, until, stop, Waiting::Answer(limit))
            .map_err(|e| self.explain(clock, e))
    }

    /// One request and its response on a new connection, watching `stop`,
    /// `until` and the process while it waits for `waiting`.
    fn exchange(
        &mut self,
        request: &[u8],
        clock: &dyn Clock,
        until: Deadline,
        stop: &dyn Fn() -> bool,
        waiting: Waiting,
    ) -> Result<Response, Interrupted> {
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|e| ServerError::Connection(HttpError::Io(e)))?;
        let timeouts = stream
            .set_read_timeout(Some(POLL))
            .and_then(|()| stream.set_write_timeout(Some(POLL)));
        timeouts.map_err(|e| ServerError::Connection(HttpError::Io(e)))?;
        let mut why = None;
        let result = {
            let mut patient = Patient {
                stream: &mut stream,
                check: &mut || match self.check(clock, until, stop, waiting) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        why = Some(e);
                        Err(io::Error::other("interrupted"))
                    }
                },
            };
            http::exchange(&mut patient, request)
        };
        match (result, why) {
            (_, Some(interrupted)) => Err(interrupted),
            (Ok(response), None) => Ok(response),
            (Err(e), None) => Err(ServerError::Connection(e).into()),
        }
    }

    /// Whether to go on waiting for `waiting`: not if `stop` says to stop,
    /// the server has exited, or `until` has passed.
    fn check(
        &mut self,
        clock: &dyn Clock,
        until: Deadline,
        stop: &dyn Fn() -> bool,
        waiting: Waiting,
    ) -> Result<(), Interrupted> {
        if stop() {
            return Err(Interrupted::Stopped);
        }
        if let Some(exited) = self.exited() {
            return Err(exited.into());
        }
        if until.passed(clock) {
            return Err(waiting.late().into());
        }
        Ok(())
    }

    /// The error for a server that has exited, if it has.
    fn exited(&mut self) -> Option<ServerError> {
        let status = self.child.as_mut()?.try_wait().ok()??;
        self.child = None;
        Some(ServerError::Exited(Some(status), self.stderr_tail()))
    }

    /// `interrupted`, unless the server exited, which is the better
    /// reason: a connection that broke because the server died. A dying
    /// process closes its socket a moment before it can be reaped, so this
    /// waits up to [`EXIT_WAIT`] to see.
    fn explain(&mut self, clock: &dyn Clock, interrupted: Interrupted) -> Interrupted {
        if !matches!(interrupted, Interrupted::Failed(ServerError::Connection(_))) {
            return interrupted;
        }
        let until = Deadline::after(clock, EXIT_WAIT);
        let (_keep, ticks) = mpsc::channel::<()>();
        loop {
            if let Some(exited) = self.exited() {
                return exited.into();
            }
            if until.passed(clock) {
                return interrupted;
            }
            let _ = ticks.recv_timeout(Duration::from_millis(10));
        }
    }

    /// The end of its stderr: waits a moment for the reader to finish, as
    /// it does just after the server exits.
    fn stderr_tail(&self) -> String {
        self.stderr
            .recv_timeout(Duration::from_millis(500))
            .unwrap_or_default()
    }
}

/// Dropping a server kills it, waits for it to go, and removes its socket.
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Fails only if it has already exited, which is what's wanted.
            let _ = child.kill();
            let _ = child.wait();
        }
        remove_socket(&self.socket);
    }
}

/// Removes a server's socket file, if it left one.
#[expect(
    clippy::disallowed_methods,
    reason = "a socket file llama-server made, not a recording or the database"
)]
fn remove_socket(socket: &Path) {
    // Already gone is fine: a server that never bound leaves none.
    let _ = std::fs::remove_file(socket);
}

/// Makes `dir` if it's missing, readable only by this user, and checks
/// that no one else can open it.
#[expect(
    clippy::disallowed_methods,
    reason = "the directory for llama-server's socket, not a recording or the database"
)]
fn private_dir(dir: &Path) -> Result<(), ServerError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let failed = |e| ServerError::SocketDir(dir.to_owned(), e);
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(failed(e)),
    }
    let mode = std::fs::metadata(dir).map_err(failed)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(ServerError::SocketDirOpen(dir.to_owned()));
    }
    Ok(())
}

/// A reader that, when a read times out, asks `check` whether to go on
/// waiting, and fails if not.
struct Patient<'a, S> {
    /// The connection.
    stream: &'a mut S,
    /// Whether to keep waiting.
    check: &'a mut dyn FnMut() -> io::Result<()>,
}

impl<S: Read> Read for Patient<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.stream.read(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    (self.check)()?;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }
}

impl<S: Write> Write for Patient<'_, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.stream.write(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    (self.check)()?;
                }
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl<S> fmt::Debug for Patient<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Patient")
    }
}

/// Reads `stderr` to its end on a thread, keeping the last [`STDERR_TAIL`]
/// bytes, and sends them once it closes.
fn keep_tail(stderr: ChildStderr) -> io::Result<Receiver<String>> {
    let (tail, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("nota-llama-stderr".into())
        .spawn(move || {
            // Nobody may be waiting any more; that's fine.
            let _ = tail.send(read_tail(stderr, STDERR_TAIL));
        })?;
    Ok(rx)
}

/// Reads `input` to its end (or a read error) and returns its last `keep`
/// bytes as text.
fn read_tail(mut input: impl Read, keep: usize) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0_u8; 4_096];
    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => kept.extend_from_slice(buf.get(..n).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        if kept.len() > keep {
            kept.drain(..kept.len() - keep);
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

#[cfg(test)]
mod tests;
