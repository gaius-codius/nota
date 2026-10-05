//! The engine child outlives a signal to the recorder's process group, and
//! still ends when the recorder dies (GAI-202).
//!
//! When a terminal closes, the kernel sends SIGHUP to its whole foreground
//! process group. The recorder catches it and stops in order; the engine
//! must not die of it first. Each test runs a recorder in a process of its
//! own (this test binary again, running [`host`]), in a process group of
//! its own as a terminal's job is, catching SIGHUP as `nota record` does,
//! with `nota-fake-engine` as its engine. The test then signals it.

// Test code throughout: clippy allows unwraps and panics in it. Process
// groups and `/proc` are Linux here.
#![cfg(test)]
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use nota_core::messages::AudioChunk;
use nota_core::{Clock, SampleIndex, SampleRate, SystemClock, TrackId};
use nota_recorder::engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineStatus, EngineSupervisor,
};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};

/// The environment variable that makes [`host`] act as the recorder, and
/// says how.
const HOST: &str = "NOTA_HANGUP_HOST";
const TRACK: TrackId = TrackId::new(0);
/// 100 ms at 16 kHz, as the recorder sends it.
const CHUNK: u64 = 1_600;
/// How long a closed engine gets to exit before the supervisor kills it
/// (the supervisor's `SHUTDOWN_GRACE`).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Waits on a channel nothing sends on: a pause that isn't a sleep.
fn pause(d: Duration) {
    let (_keep, never) = mpsc::channel::<()>();
    let _ = never.recv_timeout(d);
}

/// Waits up to `limit` for `done`, checking every 10 ms.
fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let step = Duration::from_millis(10);
    let mut waited = Duration::ZERO;
    while waited < limit {
        if done() {
            return true;
        }
        pause(step);
        waited += step;
    }
    done()
}

fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

/// The process group `pid` is in, from `/proc/<pid>/stat`.
fn process_group(pid: u32) -> u32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // After the command's name, in parentheses: state, parent, group.
    let (_, fields) = stat.rsplit_once(") ").unwrap();
    fields.split(' ').nth(2).unwrap().parse().unwrap()
}

fn chunk(k: u64) -> AudioChunk {
    let samples = (0..CHUNK)
        .map(|i| i16::try_from(i % 1_000).unwrap())
        .collect();
    AudioChunk::new(
        TRACK,
        SampleIndex::new(k * CHUNK),
        SampleRate::SPEECH,
        samples,
    )
    .unwrap()
}

/// Says `line` to the test that started this host.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").unwrap();
    out.flush().unwrap();
}

/// The recorder's side, run in its own process by the tests below; does
/// nothing when run as a test of its own. `hup` records until SIGHUP, then
/// stops as `nota record` does and checks the engine saw it through; `kill`
/// records until it's killed.
#[test]
fn host() {
    let Some(mode) = std::env::var_os(HOST) else {
        return;
    };
    let hangup = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&hangup)).unwrap();
    let mut config = EngineConfig::new(EngineCommand {
        program: PathBuf::from(env!("CARGO_BIN_EXE_nota-fake-engine")),
        // Answers every 4 chunks, so it holds audio it hasn't confirmed
        // until it's flushed.
        args: ["echo", "--every", "4"].map(OsString::from).to_vec(),
    });
    config.initial_backoff = Duration::from_millis(20);
    config.max_backoff = Duration::from_millis(200);
    let clock = Arc::new(SystemClock::start().unwrap());
    let (mut supervisor, events) = EngineSupervisor::start(config, clock).unwrap();
    let mut seen = Vec::new();
    let mut next = || {
        let event = events.recv_timeout(Duration::from_secs(10)).unwrap();
        seen.push(event.clone());
        event
    };
    let pid = loop {
        if let EngineEvent::Status(EngineStatus::Online { pid }) = next() {
            break pid;
        }
    };
    for k in 0..10 {
        supervisor.send_audio(chunk(k)).unwrap();
    }
    // Two answers (8 chunks); the last two are held.
    while !matches!(next(), EngineEvent::Confirmed { up_to, .. } if up_to.get() == 8 * CHUNK) {}
    say(&format!("engine {pid}"));
    say("ready");
    if mode == "kill" {
        pause(Duration::from_secs(60));
        panic!("not killed");
    }

    assert!(wait_until(Duration::from_secs(10), || hangup.load(Ordering::SeqCst)));
    // Stop as `nota record` does once the hangup arrives: the last audio,
    // a flush, then the shutdown.
    for k in 10..12 {
        supervisor.send_audio(chunk(k)).unwrap();
    }
    supervisor.flush(TRACK);
    while !matches!(next(), EngineEvent::Confirmed { up_to, .. } if up_to.get() == 12 * CHUNK) {}
    assert!(alive(pid), "the engine died before the shutdown");
    supervisor.shutdown();
    seen.extend(events.try_iter());
    assert!(!alive(pid), "the shutdown left the engine running");

    // One engine throughout, never offline, and its text tiles the audio
    // to the end.
    let started = seen
        .iter()
        .filter(|e| matches!(e, EngineEvent::Status(EngineStatus::Online { .. })))
        .count();
    assert_eq!(started, 1, "{seen:#?}");
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            EngineEvent::Status(EngineStatus::Offline(_)) | EngineEvent::Skipped { .. }
        )),
        "{seen:#?}"
    );
    let mut end = 0;
    for event in &seen {
        if let EngineEvent::Transcript(t) = event {
            assert_eq!(t.range().start().get(), end, "{seen:#?}");
            end = t.range().end().get();
        }
    }
    assert_eq!(end, 12 * CHUNK, "{seen:#?}");
    say("done");
}

/// A recorder running [`host`] in a process group of its own.
struct Host {
    child: Child,
    lines: Receiver<String>,
    said: Vec<String>,
}

impl Host {
    /// Starts the host in `mode`, and waits until it's recording; returns
    /// it and its engine's process id.
    fn start(mode: &str) -> (Self, u32) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["host", "--exact", "--nocapture", "--test-threads", "1"])
            .env(HOST, mode)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut host = Self {
            child,
            lines,
            said: Vec::new(),
        };
        host.wait_for("ready");
        let engine = host
            .said
            .iter()
            // libtest may have started the line with the test's name.
            .find_map(|l| l.split_once("engine ").map(|(_, pid)| pid))
            .unwrap()
            .parse()
            .unwrap();
        (host, engine)
    }

    fn pid(&self) -> Pid {
        Pid::from_child(&self.child)
    }

    /// Waits for the host to say `line`.
    fn wait_for(&mut self, line: &str) {
        loop {
            let said = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("never said {line:?}; said {:#?}", self.said));
            self.said.push(said.clone());
            if said == line {
                return;
            }
        }
    }

    fn exits(&mut self) -> ExitStatus {
        let mut status = None;
        wait_until(Duration::from_secs(20), || {
            status = self.child.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap_or_else(|| panic!("the host didn't exit; said {:#?}", self.said))
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Acceptance (GAI-202): SIGHUP to the recorder's process group, as a
/// closing terminal sends, leaves the engine running until the recorder
/// shuts it down, and the text it held reaches the end.
#[test]
fn a_hangup_to_the_recorders_group_leaves_the_engine_to_the_recorder() {
    let (mut host, engine) = Host::start("hup");
    let group = u32::try_from(host.pid().as_raw_nonzero().get()).unwrap();
    assert_eq!(process_group(group), group);
    assert_ne!(process_group(engine), group);
    kill_process_group(host.pid(), Signal::HUP).unwrap();
    host.wait_for("done");
    let status = host.exits();
    assert!(status.success(), "{status:?}; said {:#?}", host.said);
}

/// Acceptance (GAI-202): a recorder killed outright still takes the engine
/// with it, within the shutdown grace: the engine's stdin closes.
#[test]
fn a_killed_recorder_still_ends_the_engine() {
    let (mut host, engine) = Host::start("kill");
    let clock = SystemClock::start().unwrap();
    kill_process(host.pid(), Signal::KILL).unwrap();
    let killed = clock.now();
    assert!(!host.exits().success());
    assert!(
        wait_until(SHUTDOWN_GRACE, || !alive(engine)),
        "the engine outlived its recorder"
    );
    let took = clock.now().checked_duration_since(killed).unwrap();
    assert!(took < SHUTDOWN_GRACE, "{took:?}");
}
