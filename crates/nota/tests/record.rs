//! `nota record` end to end, on a pseudo-terminal, recording a synthetic
//! tone on both tracks (`--tone yes`, built only for tests): stopping from
//! the keyboard asks first; SIGHUP and SIGTERM stop it with every track's
//! audio published; the terminal is restored however it ends; and a
//! recording killed outright is salvaged at the next start.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nota_core::SessionId;
use nota_recorder::fs::{Fs, StdFs};
use nota_recorder::segment::needs_salvage;
use nota_recorder::session::SessionDir;
use nota_store::Store;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal, kill_process};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize};

/// Waits on a channel nothing sends on: a pause that isn't a sleep.
fn pause(d: Duration) {
    let (_keep, never) = mpsc::channel::<()>();
    let _ = never.recv_timeout(d);
}

/// Waits up to `limit` for `done`, checking every 20 ms.
fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let step = Duration::from_millis(20);
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

/// `bytes` without terminal control sequences (CSI and OSC), so text
/// drawn in several styles reads as one run.
fn visible(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                for c in chars.by_ref() {
                    if c == '\u{7}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A scratch directory, removed when dropped.
struct TestDir(PathBuf);

impl TestDir {
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nota-record-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TestDir {
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `nota record` running on a pseudo-terminal, 80×24.
struct Running {
    child: Child,
    /// The terminal's end the test drives; `None` once hung up.
    master: Option<std::fs::File>,
    /// Stops the thread reading the terminal, which holds a copy of it.
    reading: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    /// The terminal's other end, kept to read its modes after the child
    /// exits.
    slave: OwnedFd,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Running {
    fn start(data: &Path) -> Self {
        Self::start_with(data, &[])
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "opening the pseudo-terminal's other end"
    )]
    fn start_with(data: &Path, extra: &[&str]) -> Self {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = rustix::fs::open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        )
        .unwrap();
        tcsetwinsize(
            &master,
            Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_nota"))
            .args(["record", "--tone", "yes", "--title", "Workshop", "--data"])
            .arg(data)
            .args(extra)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .env_remove("NO_COLOR")
            .spawn()
            .unwrap();
        let master = std::fs::File::from(master);
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut copy = master.try_clone().unwrap();
        let sink = Arc::clone(&output);
        let reading = Arc::new(AtomicBool::new(true));
        let still = Arc::clone(&reading);
        let reader = thread::spawn(move || {
            let mut buf = [0_u8; 4096];
            while still.load(Ordering::SeqCst) {
                let mut fds = [PollFd::new(&copy, PollFlags::IN)];
                let timeout = Timespec {
                    tv_sec: 0,
                    tv_nsec: 50_000_000,
                };
                if poll(&mut fds, Some(&timeout)).unwrap_or(0) == 0 {
                    continue;
                }
                match copy.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            child,
            master: Some(master),
            reading,
            reader: Some(reader),
            slave,
            output,
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Waits for `text` to be drawn after the output's first `from` bytes.
    fn shows_after(&self, from: usize, text: &str) -> bool {
        wait_until(Duration::from_secs(10), || {
            let out = self.output.lock().unwrap();
            visible(out.get(from..).unwrap_or_default()).contains(text)
        })
    }

    fn len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn press(&mut self, keys: &str) {
        self.master
            .as_mut()
            .unwrap()
            .write_all(keys.as_bytes())
            .unwrap();
    }

    /// Closes the terminal's other end, as closing the window does: the
    /// program's reads and writes on it fail from now on.
    fn hang_up(&mut self) {
        self.reading.store(false, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        self.master = None;
    }

    fn signal(&self, signal: Signal) {
        let pid = Pid::from_child(&self.child);
        kill_process(pid, signal).unwrap();
    }

    fn exits(&mut self) -> Option<ExitStatus> {
        let mut status = None;
        wait_until(Duration::from_secs(20), || {
            status = self.child.try_wait().unwrap();
            status.is_some()
        });
        status
    }

    /// Whether the terminal is back in its usual (cooked, echoing) mode.
    fn terminal_restored(&self) -> bool {
        let modes = tcgetattr(&self.slave).unwrap().local_modes;
        modes.contains(LocalModes::ICANON | LocalModes::ECHO)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts recording, and waits until the screen is up and both tracks
/// have recorded a while.
fn recording(data: &Path) -> Running {
    let running = Running::start(data);
    assert!(running.shows_after(0, "s stop"), "{}", running.output());
    assert!(!running.terminal_restored());
    pause(Duration::from_millis(1_500));
    running
}

/// The session's published segments per track, and whether it has
/// journals left.
fn published(data: &Path, session: u64) -> (Vec<(u32, u64, u64)>, bool) {
    let dir = data.join("sessions").join(session.to_string());
    let audio = dir.join("audio");
    let left = needs_salvage(&SessionDir::new(SessionId::new(session), StdFs, &audio)).unwrap();
    let rows = Store::open(&dir.join("nota.db"))
        .unwrap()
        .segments()
        .unwrap();
    for row in &rows {
        let name = format!(
            "seg-t{}-{:012}.flac",
            row.track().get(),
            row.range().start().get()
        );
        assert!(StdFs.list(&audio).unwrap().contains(&audio.join(name)));
    }
    let mut tracks: Vec<(u32, u64, u64)> = rows
        .iter()
        .map(|r| {
            (
                r.track().get(),
                r.range().start().get(),
                r.range().len().get(),
            )
        })
        .collect();
    tracks.sort_unstable();
    (tracks, left)
}

/// Each of `tracks` published at least `ms` of audio, from its first
/// sample without a gap, and nothing is left over; the others nothing.
fn assert_saved(data: &Path, session: u64, tracks: &[u32], ms: u64) {
    let (rows, left) = published(data, session);
    assert!(!left, "journals left after the stop");
    for track in [0, 1] {
        let mut ranges: Vec<(u64, u64)> = rows
            .iter()
            .filter(|(t, ..)| *t == track)
            .map(|&(_, start, len)| (start, start + len))
            .collect();
        ranges.sort_unstable();
        if !tracks.contains(&track) {
            assert!(ranges.is_empty(), "track {track} recorded: {ranges:?}");
            continue;
        }
        let mut end = 0;
        for (start, next) in ranges {
            assert_eq!(start, end, "track {track} has a gap at {end}");
            end = next;
        }
        assert!(end >= ms * 16, "track {track}: only {end} samples");
    }
}

#[test]
fn s_asks_first_and_y_stops_with_everything_saved() {
    let tmp = TestDir::new("keys");
    let mut nota = recording(&tmp.0);
    let at = nota.len();
    nota.press("s");
    assert!(nota.shows_after(at, "stop recording?"), "{}", nota.output());
    // Not yet: `n` keeps recording. (The offset is taken before the key:
    // the screen redraws only cells that change, so a redraw missed
    // isn't drawn again.)
    let at = nota.len();
    nota.press("n");
    assert!(nota.shows_after(at, "s stop"));
    assert_eq!(nota.child.try_wait().unwrap(), None);
    // Ctrl+C asks too; `y` stops.
    let at = nota.len();
    nota.press("\u{3}");
    assert!(nota.shows_after(at, "stop recording?"));
    nota.press("y");
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert!(nota.terminal_restored());
    assert!(
        nota.output().contains("\u{1b}[?1049l"),
        "never left the alternate screen"
    );
    assert!(nota.output().contains("recorded to"), "{}", nota.output());
    assert_saved(&tmp.0, 1, &[0, 1], 1_450);
}

fn stops_on(signal: Signal, name: &str) {
    let tmp = TestDir::new(name);
    let mut nota = recording(&tmp.0);
    nota.signal(signal);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert!(nota.terminal_restored());
    assert_saved(&tmp.0, 1, &[0, 1], 1_450);
}

#[test]
fn sighup_stops_with_everything_saved_and_the_terminal_restored() {
    stops_on(Signal::HUP, "hup");
}

#[test]
fn sigterm_stops_with_everything_saved_and_the_terminal_restored() {
    stops_on(Signal::TERM, "term");
}

#[test]
fn a_signal_while_the_stop_question_is_open_still_stops_safely() {
    let tmp = TestDir::new("asking");
    let mut nota = recording(&tmp.0);
    let at = nota.len();
    nota.press("s");
    assert!(nota.shows_after(at, "stop recording?"));
    nota.signal(Signal::HUP);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}");
    assert!(nota.terminal_restored());
    assert_saved(&tmp.0, 1, &[0, 1], 1_450);
}

#[test]
fn a_recording_killed_outright_is_salvaged_at_the_next_start() {
    let tmp = TestDir::new("killed");
    let mut killed = recording(&tmp.0);
    killed.signal(Signal::KILL);
    assert!(killed.exits().is_some());
    let (_, left) = published(&tmp.0, 1);
    assert!(left, "the killed recording left no journals");

    let mut next = recording(&tmp.0);
    next.signal(Signal::TERM);
    assert!(next.exits().expect("nota didn't stop").success());
    assert!(
        next.output().contains("salvaged session 1"),
        "{}",
        next.output()
    );
    let (rows, left) = published(&tmp.0, 1);
    assert!(!left);
    for track in [0, 1] {
        assert!(rows.iter().any(|(t, ..)| *t == track), "{rows:?}");
    }
    assert_saved(&tmp.0, 2, &[0, 1], 1_450);
}

#[test]
fn sigint_stops_with_everything_saved_and_the_terminal_restored() {
    stops_on(Signal::INT, "int");
}

#[test]
fn a_closed_terminal_and_its_hangup_still_save_everything() {
    let tmp = TestDir::new("closed");
    let mut nota = recording(&tmp.0);
    nota.hang_up();
    nota.signal(Signal::HUP);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert_saved(&tmp.0, 1, &[0, 1], 1_450);
}

#[test]
fn when_every_stream_fails_the_recording_stops_by_itself() {
    let tmp = TestDir::new("all-fail");
    let mut nota = Running::start_with(&tmp.0, &["--mic", "fails", "--system", "fails"]);
    let status = nota
        .exits()
        .expect("nota kept going with nothing to record");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert!(nota.terminal_restored());
    let said = visible(&nota.output.lock().unwrap());
    assert!(said.contains("stopped recording the mic"), "{said}");
    assert!(
        said.contains("stopped recording the system audio"),
        "{said}"
    );
    assert_saved(&tmp.0, 1, &[0, 1], 450);
}

#[test]
fn a_stream_that_cant_start_leaves_the_other_recording() {
    let tmp = TestDir::new("one-missing");
    let mut nota = Running::start_with(&tmp.0, &["--mic", "missing"]);
    assert!(nota.shows_after(0, "s stop"), "{}", nota.output());
    pause(Duration::from_millis(1_500));
    nota.signal(Signal::TERM);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}");
    let said = visible(&nota.output.lock().unwrap());
    assert!(said.contains("not recording device missing"), "{said}");
    assert_saved(&tmp.0, 1, &[1], 1_450);
}
