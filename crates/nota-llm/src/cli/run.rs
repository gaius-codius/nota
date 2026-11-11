//! Running a CLI once: the question in on its stdin, the answer out on its
//! stdout, watched for `stop` and a time limit.
//!
//! Its stderr goes nowhere. Both CLIs echo the prompt there (`codex exec`
//! prints the whole question), and nothing that holds the transcript's
//! words may end up in a reason, so a failure is told by its exit status
//! alone. The CLI runs in a process group of its own, and on a stop or a
//! timeout the whole group is killed, so nothing it started is left
//! running. The group is killed only while the CLI itself is still
//! unreaped: its id can't have been given to another process then.

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use nota_core::Clock;
use rustix::process::{Pid, Signal, kill_process_group};

use crate::reply::MAX_REPLY_LEN;
use crate::server::{Deadline, POLL};

/// What a CLI that ran to its end left: how it exited, and its stdout.
#[derive(Debug)]
pub(crate) struct Ran {
    /// How it exited.
    pub(crate) status: ExitStatus,
    /// Everything it wrote to stdout, at most [`MAX_REPLY_LEN`] bytes.
    pub(crate) stdout: Vec<u8>,
}

/// Why a CLI didn't run to its end.
#[derive(Debug)]
pub(crate) enum RunFailure {
    /// It couldn't be started: no such program, or one that can't be run.
    Spawn(io::Error),
    /// `stop` said to stop; it was killed.
    Stopped,
    /// It hadn't finished when the time was up; it was killed.
    Late,
    /// Its stdout went past [`MAX_REPLY_LEN`]; it was killed.
    TooLong,
    /// Reading its stdout, or waiting for it, failed.
    Io(io::Error),
}

/// A started CLI, killed with its group and reaped when dropped unless it
/// has already been reaped.
struct Running(Option<Child>);

impl Running {
    /// The CLI, reaped: how it exited.
    fn reaped(&mut self) -> io::Result<Option<ExitStatus>> {
        let Some(child) = self.0.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        if status.is_some() {
            self.0 = None;
        }
        Ok(status)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // Still unreaped, so its id is still its group's. Fails only if
            // the group has gone already.
            // A CLI that left its group (by `setsid`, say) is killed
            // alone, so the wait below can't block for good.
            if kill_process_group(Pid::from_child(&child), Signal::KILL).is_err() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

/// Runs `command` with `input` on its stdin, until it exits, `stop` says
/// to stop, or `limit` (timed by `clock`) is up.
///
/// # Errors
///
/// See [`RunFailure`]. Whatever the failure, the CLI and its group are
/// gone by the time this returns.
pub(crate) fn run(
    mut command: Command,
    input: Vec<u8>,
    clock: &dyn Clock,
    limit: Duration,
    stop: &dyn Fn() -> bool,
) -> Result<Ran, RunFailure> {
    if stop() {
        return Err(RunFailure::Stopped);
    }
    let until = Deadline::after(clock, limit);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // A group of its own, so a stop kills whatever it started too.
    command.process_group(0);
    let mut child = command.spawn().map_err(RunFailure::Spawn)?;
    let pipes = (child.stdin.take(), child.stdout.take());
    let mut running = Running(Some(child));
    let (Some(stdin), Some(stdout)) = pipes else {
        return Err(RunFailure::Io(io::Error::other(
            "started without its pipes",
        )));
    };
    feed(stdin, input).map_err(RunFailure::Io)?;
    let output = collect(stdout).map_err(RunFailure::Io)?;
    let stdout = wait_for_output(&output, clock, until, stop)?;
    let status = wait_for_exit(&mut running, clock, until, stop)?;
    Ok(Ran { status, stdout })
}

/// Waits for all of the CLI's stdout: until it closes.
fn wait_for_output(
    output: &Receiver<io::Result<Vec<u8>>>,
    clock: &dyn Clock,
    until: Deadline,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<u8>, RunFailure> {
    loop {
        check(clock, until, stop)?;
        match output.recv_timeout(POLL) {
            Ok(Ok(stdout)) if stdout.len() > MAX_REPLY_LEN => return Err(RunFailure::TooLong),
            Ok(Ok(stdout)) => return Ok(stdout),
            Ok(Err(e)) => return Err(RunFailure::Io(e)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RunFailure::Io(io::Error::other(
                    "the thread reading the answer stopped",
                )));
            }
        }
    }
}

/// Waits for the CLI to exit, once its stdout has closed.
fn wait_for_exit(
    running: &mut Running,
    clock: &dyn Clock,
    until: Deadline,
    stop: &dyn Fn() -> bool,
) -> Result<ExitStatus, RunFailure> {
    // Nothing is ever sent: the channel only gives a wait that wakes in
    // time to look at `stop` again.
    let (_keep, ticks) = mpsc::channel::<()>();
    loop {
        check(clock, until, stop)?;
        match running.reaped() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => return Err(RunFailure::Io(e)),
        }
        let _ = ticks.recv_timeout(POLL);
    }
}

/// Whether to go on waiting: not once `stop` says to, or `until` passes.
fn check(clock: &dyn Clock, until: Deadline, stop: &dyn Fn() -> bool) -> Result<(), RunFailure> {
    if stop() {
        return Err(RunFailure::Stopped);
    }
    if until.passed(clock) {
        return Err(RunFailure::Late);
    }
    Ok(())
}

/// Writes `input` to the CLI's stdin on a thread, then closes it, so the
/// CLI sees the end of its prompt. A question can be bigger than a pipe
/// holds, and a CLI that never reads must not hold up the wait.
fn feed(mut stdin: ChildStdin, input: Vec<u8>) -> io::Result<()> {
    thread::Builder::new()
        .name("nota-cli-stdin".into())
        .spawn(move || {
            // A CLI that exits without reading closes the pipe; its exit
            // status says what went wrong, not this.
            let _ = stdin.write_all(&input);
        })
        .map(drop)
}

/// Reads the CLI's stdout to its end on a thread, keeping one byte past
/// [`MAX_REPLY_LEN`] to show it was too long, and sends it once it closes.
fn collect(stdout: ChildStdout) -> io::Result<Receiver<io::Result<Vec<u8>>>> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("nota-cli-stdout".into())
        .spawn(move || {
            let mut read = Vec::new();
            let limit = u64::try_from(MAX_REPLY_LEN).map_or(u64::MAX, |l| l.saturating_add(1));
            let done = stdout.take(limit).read_to_end(&mut read).map(|_| read);
            // Nobody may be waiting any more; that's fine.
            let _ = tx.send(done);
        })?;
    Ok(rx)
}
