//! `nota record` end to end, on a pseudo-terminal, recording a synthetic
//! tone on both tracks (`--tone yes`, built only for tests): stopping from
//! the keyboard asks first; SIGHUP and SIGTERM stop it with every track's
//! audio published (exactly what the tone sent, by the count the summary
//! gives in tone mode); a second signal during the stop is ignored; SIGXCPU
//! only warns; the terminal is restored however
//! it ends; a recording killed outright is salvaged at the next start; and the engine child
//! sits outside the recorder's process group, so the terminal's hangup
//! leaves it to the recorder, yet it ends when the recorder is killed. The
//! engine's tests skip (and say so on stderr) without the test models, and
//! fail instead when `NOTA_REQUIRE_TEST_MODELS=1`, as in CI; see
//! `scripts/fetch-test-models.sh`.

// Test code throughout: clippy allows unwraps and panics in it. Recording
// works on Linux only for now.
#![cfg(test)]
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nota_core::{Clock, SessionId, SystemClock};
use nota_recorder::fs::{Fs, StdFs};
use nota_recorder::segment::needs_salvage;
use nota_recorder::session::SessionDir;
use nota_store::{Annotation, JobState, SessionState, Store, TrackKind};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize};

/// Waits on a channel nothing sends on: a pause that isn't a sleep.
fn pause(d: Duration) {
    let (_keep, never) = mpsc::channel::<()>();
    let _ = never.recv_timeout(d);
}

/// Waits up to `limit` for `done`, checking every 20 ms.
fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    // Measured on the session clock, not summed from the pauses: a pause
    // can run long on a loaded machine.
    let clock = SystemClock::start().unwrap();
    while clock.now().elapsed() < limit {
        if done() {
            return true;
        }
        pause(Duration::from_millis(20));
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
    fn start_with(data: &Path, extra: &[&str]) -> Self {
        Self::start_as(data, extra, false, &[])
    }

    /// Starts `nota record`; with `own_group`, in a process group of its
    /// own, as a terminal's job is, so the test can signal the group.
    /// Otherwise it stays in the test's group, so it goes with the test if
    /// the test is killed.
    fn start_as(data: &Path, extra: &[&str], own_group: bool, env: &[(&str, &str)]) -> Self {
        let record = ["record", "--tone", "yes", "--title", "Workshop"];
        Self::start_command(&record, data, extra, own_group, env)
    }

    /// `nota` with no command: Home, recording a tone when asked to. Its
    /// home directory is `data`, so the screen draws in the default theme
    /// whatever the machine's Omarchy theme is.
    fn start_home(data: &Path) -> Self {
        let home = data.to_str().unwrap();
        Self::start_command(&["--tone", "yes"], data, &[], false, &[("HOME", home)])
    }

    /// Starts nota with `words`, then `--data` and `extra`.
    #[expect(
        clippy::disallowed_methods,
        reason = "opening the pseudo-terminal's other end"
    )]
    fn start_command(
        words: &[&str],
        data: &Path,
        extra: &[&str],
        own_group: bool,
        env: &[(&str, &str)],
    ) -> Self {
        // Close-on-exec, so nota holds only the slave: closing the test's
        // master is then a real hangup.
        let master =
            openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = rustix::fs::open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_nota"));
        command
            .args(words)
            .arg("--data")
            .arg(data)
            .args(extra)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .env_remove("NO_COLOR")
            .envs(env.iter().copied());
        if own_group {
            command.process_group(0);
        }
        let child = command.spawn().unwrap();
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

    /// Sends `signal` to the process group, as a closing terminal does.
    fn signal_group(&self, signal: Signal) {
        kill_process_group(Pid::from_child(&self.child), signal).unwrap();
    }

    fn pid(&self) -> u32 {
        self.child.id()
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
    recording_with(data, &[])
}

/// [`recording`], with more environment for nota.
fn recording_with(data: &Path, env: &[(&str, &str)]) -> Running {
    let running = Running::start_as(data, &[], false, env);
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
    let rows = Store::open(&data.join("library.db"))
        .unwrap()
        .segments(SessionId::new(session))
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
/// Returns where each track's audio ends.
fn assert_saved(data: &Path, session: u64, tracks: &[u32], ms: u64) -> Vec<(u32, u64)> {
    let mut ends = Vec::new();
    let (rows, left) = published(data, session);
    assert!(!left, "journals left after the stop");
    // The session stopped, with the title its start command gave and the
    // tracks it recorded.
    let store = Store::open(&data.join("library.db")).unwrap();
    let id = SessionId::new(session);
    let row = store.session(id).unwrap().unwrap();
    assert_eq!(row.state, SessionState::Stopped);
    assert_eq!(row.title.as_deref(), Some("Workshop"));
    let recorded: Vec<u32> = store
        .tracks(id)
        .unwrap()
        .iter()
        .map(|t| t.track.get())
        .collect();
    assert_eq!(recorded, tracks);
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
        ends.push((track, end));
    }
    ends
}

/// How many samples the tone sent on each track, from the summary.
fn tone_sent(said: &str) -> Vec<(u32, u64)> {
    let mut sent = Vec::new();
    for line in said.lines() {
        // A line not yet read whole is skipped.
        let Some(rest) = line
            .trim()
            .strip_prefix("the tone for ")
            .filter(|rest| rest.ends_with(" samples"))
        else {
            continue;
        };
        let (source, count) = rest.split_once(" sent ").unwrap();
        let track = match source {
            "the microphone" => 0,
            "the system audio" => 1,
            other => panic!("a tone for {other}"),
        };
        let count = count.strip_suffix(" samples").unwrap().parse().unwrap();
        sent.push((track, count));
    }
    sent.sort_unstable();
    sent
}

/// As [`assert_saved`], and each track published exactly what its tone
/// sent: nothing lost at the end.
fn assert_everything_sent_saved(
    nota: &Running,
    data: &Path,
    session: u64,
    tracks: &[u32],
    ms: u64,
) {
    let saved = assert_saved(data, session, tracks, ms);
    // The summary is written just before nota exits; the thread reading
    // the terminal may not have it yet.
    wait_until(Duration::from_secs(10), || {
        tone_sent(&visible(&nota.output.lock().unwrap())).len() == tracks.len()
    });
    let said = visible(&nota.output.lock().unwrap());
    let sent = tone_sent(&said);
    assert_eq!(
        sent.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        tracks,
        "{said}"
    );
    assert_eq!(saved, sent, "saved, then sent");
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
    // Ctrl+C asks too; `y` stops, once the question has been open long
    // enough for it to be an answer rather than typing.
    let at = nota.len();
    nota.press("\u{3}");
    assert!(nota.shows_after(at, "stop recording?"));
    pause(Duration::from_millis(600));
    nota.press("y");
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert!(nota.terminal_restored());
    assert!(
        nota.output().contains("\u{1b}[?1049l"),
        "never left the alternate screen"
    );
    assert!(nota.output().contains("recorded to"), "{}", nota.output());
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_450);
}

/// What the tones and the stand-in logind did, in order, from the
/// summary.
fn tone_events(said: &str) -> Vec<String> {
    said.lines()
        .filter_map(|line| line.trim().strip_prefix("tone: "))
        .map(ToOwned::to_owned)
        .collect()
}

/// The sleep lock is taken before the first stream opens and let go once
/// the last has stopped: held for the recording, and for no more.
#[test]
fn the_sleep_lock_is_held_for_exactly_the_recording() {
    let tmp = TestDir::new("sleep-lock");
    let mut nota = recording(&tmp.0);
    nota.signal(Signal::TERM);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    // The summary is written just before nota exits; the thread reading
    // the terminal may not have all of it yet.
    assert!(
        wait_until(Duration::from_secs(10), || {
            tone_events(&visible(&nota.output.lock().unwrap())).len() == 6
        }),
        "{}",
        nota.output()
    );
    let events = tone_events(&visible(&nota.output.lock().unwrap()));
    let mic = "the microphone";
    let system = "the system audio";
    assert_eq!(events.first().map(String::as_str), Some("sleep lock taken"));
    assert_eq!(
        events.last().map(String::as_str),
        Some("sleep lock released")
    );
    // Both streams opened after the lock, and stopped before its release.
    let mut inside: Vec<_> = events[1..5].to_vec();
    inside.sort();
    let mut expected = [
        format!("{mic} started"),
        format!("{mic} stopped"),
        format!("{system} started"),
        format!("{system} stopped"),
    ];
    expected.sort();
    assert_eq!(inside, expected, "{events:?}");
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_450);
}

/// A logind that refuses leaves the recording whole, and the summary
/// says the machine may sleep.
#[test]
fn a_refused_sleep_lock_is_said_and_recording_carries_on() {
    let tmp = TestDir::new("sleep-refused");
    let mut nota = recording_with(&tmp.0, &[("NOTA_TONE_SLEEP_REFUSED", "1")]);
    nota.signal(Signal::TERM);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_450);
    // The tones' own lines come after the samples sent, and may not all
    // have been read yet.
    assert!(
        wait_until(Duration::from_secs(10), || {
            tone_events(&visible(&nota.output.lock().unwrap())).len() == 4
        }),
        "{}",
        nota.output()
    );
    let said = visible(&nota.output.lock().unwrap());
    assert!(
        said.contains(
            "sleep couldn't be held off: the stand-in logind refused; \
             if the machine sleeps, the recording has a gap there"
        ),
        "{said}"
    );
    // The two tones started and stopped, and no lock was taken or let go.
    let mut events = tone_events(&said);
    events.sort();
    assert_eq!(
        events,
        [
            "the microphone started",
            "the microphone stopped",
            "the system audio started",
            "the system audio stopped",
        ],
        "{said}"
    );
}

/// A start that fails (no stream opens) says nothing of sleep, though
/// logind refused: there was no recording to sleep through.
#[test]
fn a_failed_start_says_nothing_of_a_refused_sleep_lock() {
    let tmp = TestDir::new("sleep-refused-no-start");
    let mut nota = Running::start_as(
        &tmp.0,
        &["--mic", "missing", "--system", "missing"],
        false,
        &[("NOTA_TONE_SLEEP_REFUSED", "1")],
    );
    let status = nota
        .exits()
        .expect("nota did not reject the missing inputs");
    assert!(!status.success(), "{status:?}");
    let said = visible(&nota.output.lock().unwrap());
    assert_eq!(
        said.matches("not recording device missing").count(),
        2,
        "{said}"
    );
    assert!(!said.contains("sleep"), "{said}");
}

fn stops_on(signal: Signal, name: &str) {
    let tmp = TestDir::new(name);
    let mut nota = recording(&tmp.0);
    nota.signal(signal);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert!(nota.terminal_restored());
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_450);
}

/// Starts a recording from the current page, then stops it and opens Processing.
fn record_from_page_and_stop(nota: &mut Running, key: &str) -> usize {
    // Wait for Recording before sending its stop key.
    let at = nota.len();
    nota.press(key);
    assert!(nota.shows_after(at, "s stop"), "{}", nota.output());
    let at = nota.len();
    nota.press("s");
    assert!(nota.shows_after(at, "stop recording?"), "{}", nota.output());
    // A quick answer is typing until the question's guard has passed.
    pause(Duration::from_millis(700));
    let at = nota.len();
    nota.press("y");
    assert!(nota.shows_after(at, "finishing"), "{}", nota.output());
    assert!(nota.shows_after(at, "tab view"), "{}", nota.output());
    at
}

/// Processing's next recording shows the first session even in a new library.
#[test]
fn home_lists_the_first_session_during_the_next_stop() {
    let tmp = TestDir::new("processing-first-session-home");
    let mut nota = Running::start_home(&tmp.0);
    // Starting from an empty Home catches a list kept from before the first stop.
    assert!(
        nota.shows_after(0, "nothing recorded yet"),
        "{}",
        nota.output()
    );
    record_from_page_and_stop(&mut nota, "R");
    // Request the second recording directly from Processing, without visiting Home.
    let at = record_from_page_and_stop(&mut nota, "r");
    let stopped = visible(&nota.output.lock().unwrap()[at..]);
    assert!(stopped.contains("recent"), "{stopped}");
    assert!(stopped.contains("Recording"), "{stopped}");
    assert!(!stopped.contains("nothing recorded yet"), "{stopped}");
    nota.press("q");
    assert!(nota.exits().unwrap().success());
}

/// A recording started from Processing keeps Home's sessions during its stop.
#[test]
fn home_lists_sessions_during_a_stop_started_from_processing() {
    let tmp = TestDir::new("processing-keeps-home");
    // An earlier recording gives Home a session to keep while Processing is open.
    let mut first = recording(&tmp.0);
    first.signal(Signal::TERM);
    assert!(first.exits().unwrap().success());
    let mut nota = Running::start_home(&tmp.0);
    assert!(nota.shows_after(0, "Workshop"), "{}", nota.output());
    // Open that session, then start the next recording from its page.
    let at = nota.len();
    nota.press("\r");
    assert!(nota.shows_after(at, "tab view"), "{}", nota.output());
    let at = nota.len();
    nota.press("r");
    assert!(nota.shows_after(at, "s stop"), "{}", nota.output());
    // Wait for the question's guard before answering the stop.
    let at = nota.len();
    nota.press("s");
    assert!(nota.shows_after(at, "stop recording?"), "{}", nota.output());
    pause(Duration::from_millis(700));
    let at = nota.len();
    nota.press("y");
    assert!(nota.shows_after(at, "finishing"), "{}", nota.output());
    assert!(nota.shows_after(at, "tab view"), "{}", nota.output());
    // The stop draws Home's list before opening Processing again.
    let stopped = visible(&nota.output.lock().unwrap()[at..]);
    assert!(stopped.contains("recent"), "{stopped}");
    assert!(stopped.contains("Workshop"), "{stopped}");
    assert!(!stopped.contains("nothing recorded yet"), "{stopped}");
    nota.press("q");
    assert!(nota.exits().unwrap().success());
}

/// `nota` with no command opens Home. `R` records with the last settings
/// (a first recording: the default devices, titled "Recording"), stopping
/// opens Processing, then esc returns to Home with the session listed.
/// `q` closes nota with the terminal restored and its summary said.
#[test]
fn home_records_opens_processing_and_returns_home() {
    let tmp = TestDir::new("home");
    let mut nota = Running::start_home(&tmp.0);
    assert!(
        nota.shows_after(0, "nothing recorded yet"),
        "{}",
        nota.output()
    );
    assert!(nota.shows_after(0, "R last settings"), "{}", nota.output());

    let at = nota.len();
    nota.press("R");
    assert!(nota.shows_after(at, "s stop"), "{}", nota.output());
    pause(Duration::from_millis(1_500));
    let at = nota.len();
    nota.press("s");
    assert!(nota.shows_after(at, "stop recording?"), "{}", nota.output());
    // Past the guard that takes a quick `y` for typing.
    pause(Duration::from_millis(700));
    let at = nota.len();
    nota.press("y");
    // One word: a draw skips cells that are blank already, such as the
    // spaces between words in the default theme.
    assert!(nota.shows_after(at, "finishing"), "{}", nota.output());
    assert!(nota.shows_after(at, "transcript"), "{}", nota.output());
    assert!(nota.shows_after(at, "tab view"), "{}", nota.output());
    let at = nota.len();
    nota.press("\u{1b}");
    assert!(nota.shows_after(at, "Recording"), "{}", nota.output());
    assert!(nota.shows_after(at, "⏎ open"), "{}", nota.output());
    assert!(!nota.terminal_restored());

    let at = nota.len();
    nota.press("\r");
    assert!(nota.shows_after(at, "transcript"), "{}", nota.output());
    assert!(nota.shows_after(at, "tab view"), "{}", nota.output());

    // The session is saved and stopped, with the setup's title.
    let (rows, left) = published(&tmp.0, 1);
    assert!(!left, "journals left after the stop");
    assert!(rows.iter().any(|&(track, ..)| track == 0));
    assert!(rows.iter().any(|&(track, ..)| track == 1));
    let session = Store::open(&tmp.0.join("library.db"))
        .unwrap()
        .session(SessionId::new(1))
        .unwrap()
        .unwrap();
    assert_eq!(session.state, SessionState::Stopped);
    assert_eq!(session.title.as_deref(), Some("Recording"));
    // Its start was written with its row: after this code was written.
    let started = session.started_at.expect("no start time");
    assert!(started.unix_seconds() > 1_767_225_600, "{started:?}");

    nota.press("q");
    let status = nota.exits().expect("nota didn't close");
    assert!(status.success(), "{status:?}");
    assert!(nota.terminal_restored());
    assert!(
        nota.shows_after(0, "nota: recorded to"),
        "{}",
        nota.output()
    );
}

/// A signal during a recording started from Home stops it in order, and
/// then closes nota rather than showing Home again: as `nota record` does,
/// a logout or shutdown ends it.
#[test]
fn a_signal_during_a_recording_from_home_closes_nota() {
    for signal in [Signal::TERM, Signal::INT, Signal::HUP] {
        let tmp = TestDir::new(&format!("home-signal-{}", signal.as_raw()));
        let mut nota = Running::start_home(&tmp.0);
        assert!(nota.shows_after(0, "R last settings"), "{}", nota.output());
        let at = nota.len();
        nota.press("R");
        assert!(nota.shows_after(at, "s stop"), "{}", nota.output());
        pause(Duration::from_millis(1_500));
        nota.signal(signal);
        let status = nota.exits().expect("nota didn't close after the signal");
        assert!(
            status.success(),
            "{signal:?}: {status:?}\n{}",
            nota.output()
        );
        assert!(nota.terminal_restored());
        let (rows, left) = published(&tmp.0, 1);
        assert!(!left, "journals left after the stop");
        assert!(!rows.is_empty());
    }
}

/// A signal on Home closes nota, with the terminal restored.
#[test]
fn a_signal_on_home_closes_nota() {
    let tmp = TestDir::new("home-quit");
    let mut nota = Running::start_home(&tmp.0);
    assert!(nota.shows_after(0, "R last settings"), "{}", nota.output());
    nota.signal(Signal::TERM);
    let status = nota.exits().expect("nota didn't close after the signal");
    assert!(status.success(), "{status:?}");
    assert!(nota.terminal_restored());
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
fn sigxcpu_only_warns_and_recording_carries_on() {
    // What rtkit's RLIMIT_RTTIME soft limit sends when the capture thread
    // runs past its real-time budget.
    let tmp = TestDir::new("xcpu");
    let mut nota = recording(&tmp.0);
    nota.signal(Signal::XCPU);
    pause(Duration::from_millis(500));
    assert_eq!(
        nota.child.try_wait().unwrap(),
        None,
        "SIGXCPU ended the recording: {}",
        nota.output()
    );
    let at = nota.len();
    nota.signal(Signal::TERM);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert!(nota.terminal_restored());
    assert!(
        nota.shows_after(
            at,
            "the capture thread ran past its real-time budget at least once (SIGXCPU)"
        ),
        "{}",
        nota.output()
    );
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_950);
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
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_450);
}

/// A second signal while stopping is ignored: the first one's stop
/// carries on to the end, and nothing is lost.
#[test]
fn a_second_signal_during_the_stop_is_ignored() {
    let tmp = TestDir::new("twice");
    // Stopping the streams takes 2 s, so the second signal lands in the
    // stop, after the screen has closed.
    let mut nota = recording_with(&tmp.0, &[("NOTA_TONE_STOP_DELAY_MS", "2000")]);
    nota.signal(Signal::HUP);
    assert!(wait_until(Duration::from_secs(10), || nota.terminal_restored()));
    assert_eq!(
        nota.child.try_wait().unwrap(),
        None,
        "stopped before the second signal"
    );
    nota.signal(Signal::TERM);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert!(nota.terminal_restored());
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_450);
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
    assert_everything_sent_saved(&next, &tmp.0, 2, &[0, 1], 1_450);
}

/// Makes a mark, a note and another mark, a moment apart.
fn annotate(nota: &mut Running) {
    nota.press("m");
    pause(Duration::from_millis(300));
    nota.press("nbring clamps\r");
    pause(Duration::from_millis(300));
    nota.press("m");
    pause(Duration::from_millis(300));
}

/// The session's marks and notes in the library, as `(text, at_ms)`: a
/// mark's text is `"◆"`.
fn annotations(data: &Path, session: u64) -> Vec<(String, u64)> {
    Store::open(&data.join("library.db"))
        .unwrap()
        .annotations(SessionId::new(session))
        .unwrap()
        .into_iter()
        .map(|a| {
            let at = a.at().as_nanos() / 1_000_000;
            match a {
                Annotation::Mark(_) => ("◆".to_owned(), at),
                Annotation::Note(n) => (n.text().to_owned(), at),
            }
        })
        .collect()
}

/// The texts of [`annotations`], checking their times rise from after the
/// start (the screen was up 1.5 s before the first) and stay within `ms`.
fn annotated(data: &Path, session: u64, ms: u64) -> Vec<String> {
    let found = annotations(data, session);
    let mut last = 1_000;
    for (text, at) in &found {
        assert!(*at >= last && *at < ms, "{text} at {at} ms: {found:?}");
        last = *at;
    }
    found.into_iter().map(|(text, _)| text).collect()
}

/// Acceptance (GAI-200): marks and notes are stored as they're made, in
/// session time, and read back in order after a stop by `s y`.
#[test]
fn marks_and_notes_are_saved_through_a_stop_by_s_y() {
    let tmp = TestDir::new("marks-keys");
    let mut nota = recording(&tmp.0);
    annotate(&mut nota);
    // Stored before the stop: each goes to the library as it's made.
    assert!(
        wait_until(Duration::from_secs(10), || annotations(&tmp.0, 1).len()
            == 3),
        "{:?}",
        annotations(&tmp.0, 1)
    );
    let at = nota.len();
    nota.press("s");
    assert!(nota.shows_after(at, "stop recording?"));
    pause(Duration::from_millis(600));
    nota.press("y");
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert_eq!(annotated(&tmp.0, 1, 10_000), ["◆", "bring clamps", "◆"]);
    let said = visible(&nota.output.lock().unwrap());
    assert!(!said.contains("saved"), "{said}");
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_450);
}

/// Acceptance (GAI-200): the same through SIGHUP, with a note still being
/// typed when it came: that one's kept too.
#[test]
fn marks_and_notes_are_saved_through_a_hangup() {
    let tmp = TestDir::new("marks-hup");
    let mut nota = recording(&tmp.0);
    annotate(&mut nota);
    nota.press("nhalf typed");
    pause(Duration::from_millis(300));
    nota.signal(Signal::HUP);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert_eq!(
        annotated(&tmp.0, 1, 10_000),
        ["◆", "bring clamps", "◆", "half typed"]
    );
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_450);
}

/// Acceptance (GAI-310): what was stored before nota was killed outright
/// is there after the next start's salvage.
#[test]
fn marks_and_notes_stored_before_a_kill_survive_it_and_salvage() {
    let tmp = TestDir::new("marks-killed");
    let mut killed = recording(&tmp.0);
    annotate(&mut killed);
    assert!(
        wait_until(Duration::from_secs(10), || annotations(&tmp.0, 1).len()
            == 3),
        "{:?}",
        annotations(&tmp.0, 1)
    );
    killed.signal(Signal::KILL);
    assert!(killed.exits().is_some());

    let mut next = recording(&tmp.0);
    next.signal(Signal::TERM);
    assert!(next.exits().expect("nota didn't stop").success());
    assert!(
        next.output().contains("salvaged session 1"),
        "{}",
        next.output()
    );
    assert_eq!(annotated(&tmp.0, 1, 10_000), ["◆", "bring clamps", "◆"]);
    assert!(annotations(&tmp.0, 2).is_empty());
}

/// A connection to the library behind nota's back, to hold its lock.
#[expect(clippy::disallowed_methods, reason = "test scaffolding")]
fn open_library(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}

/// Acceptance (GAI-310): the library database failing mid-session (another
/// connection holding its write lock past the busy timeout) holds up
/// nothing that records. The recording carries on, and once the lock goes,
/// what was made meanwhile is saved and every sample is published.
#[test]
fn a_library_locked_mid_session_costs_nothing_once_it_is_back() {
    let tmp = TestDir::new("locked-library");
    let mut nota = recording(&tmp.0);
    nota.press("m");
    assert!(
        wait_until(Duration::from_secs(10), || annotations(&tmp.0, 1).len()
            == 1),
        "{:?}",
        annotations(&tmp.0, 1)
    );
    let lock = open_library(&tmp.0.join("library.db"));
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    nota.press("nmade while locked\r");
    pause(Duration::from_millis(300));
    nota.press("m");
    // Past the store's 5 s busy timeout: its writes fail meanwhile.
    pause(Duration::from_secs(7));
    assert_eq!(annotations(&tmp.0, 1).len(), 1, "written through the lock");
    assert_eq!(
        nota.child.try_wait().unwrap(),
        None,
        "the recording stopped"
    );
    lock.execute_batch("COMMIT").unwrap();
    drop(lock);
    assert!(
        wait_until(Duration::from_secs(10), || annotations(&tmp.0, 1).len()
            == 3),
        "{:?}",
        annotations(&tmp.0, 1)
    );
    nota.signal(Signal::TERM);
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert_eq!(
        annotated(&tmp.0, 1, 30_000),
        ["◆", "made while locked", "◆"]
    );
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 8_000);
}

/// Acceptance (GAI-310): a library database that can't be written holds up
/// nothing that records. The recording goes on and stops as usual, the
/// summary says what wasn't saved, and the journals hold every sample, so
/// the next start publishes all of it. Acceptance (GAI-335): the next start
/// adopts it with its title and tracks.
#[test]
#[expect(clippy::disallowed_methods, reason = "test scaffolding")]
fn a_library_that_cant_be_written_leaves_the_recording_whole() {
    let tmp = TestDir::new("no-library");
    let db = tmp.0.join("library.db");
    // A directory where the database should be: nothing can open it.
    std::fs::create_dir_all(db.join("not a database")).unwrap();
    let mut nota = recording(&tmp.0);
    annotate(&mut nota);
    nota.signal(Signal::TERM);
    let status = nota.exits().expect("nota didn't stop");
    // It stopped as asked, saying the recording isn't published yet.
    assert_eq!(status.code(), Some(1), "{}", nota.output());
    wait_until(Duration::from_secs(10), || {
        tone_sent(&visible(&nota.output.lock().unwrap())).len() == 2
    });
    let said = visible(&nota.output.lock().unwrap());
    assert!(
        said.contains("3 of 3 marks and notes weren't saved to the library"),
        "{said}"
    );
    assert!(said.contains("journals weren't published"), "{said}");
    let sent = tone_sent(&said);
    assert_eq!(sent.len(), 2, "{said}");

    // The database is back: the next start publishes every sample sent.
    std::fs::remove_dir_all(&db).unwrap();
    let mut next = recording(&tmp.0);
    next.signal(Signal::TERM);
    assert!(next.exits().expect("nota didn't stop").success());
    let (rows, left) = published(&tmp.0, 1);
    assert!(!left, "journals left: {}", next.output());
    let store = Store::open(&db).unwrap();
    let id = SessionId::new(1);
    let session = store.session(id).unwrap().unwrap();
    assert_eq!(session.title.as_deref(), Some("Workshop"));
    assert!(session.started_at.is_some());
    let tracks: Vec<(u32, TrackKind, Option<String>)> = store
        .tracks(id)
        .unwrap()
        .into_iter()
        .map(|t| (t.track.get(), t.kind, t.source))
        .collect();
    assert_eq!(
        tracks
            .iter()
            .map(|(n, kind, _)| (*n, *kind))
            .collect::<Vec<_>>(),
        [(0, TrackKind::Microphone), (1, TrackKind::System)]
    );
    assert!(
        tracks.iter().all(|(.., source)| source.is_some()),
        "{tracks:?}"
    );
    for (track, count) in sent {
        let mut ranges: Vec<(u64, u64)> = rows
            .iter()
            .filter(|(t, ..)| *t == track)
            .map(|&(_, start, len)| (start, start + len))
            .collect();
        ranges.sort_unstable();
        let mut end = 0;
        for (start, next) in ranges {
            assert_eq!(start, end, "track {track} has a gap at {end}");
            end = next;
        }
        assert_eq!(end, count, "track {track}");
    }
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

/// A rejected start reports each input failure once.
#[test]
fn a_start_with_no_inputs_reports_each_failure_once() {
    let tmp = TestDir::new("both-missing");
    // Both inputs fail before capture opens, so startup owns both failure notes.
    let mut nota = Running::start_with(&tmp.0, &["--mic", "missing", "--system", "missing"]);
    let status = nota
        .exits()
        .expect("nota did not reject the missing inputs");
    assert!(!status.success(), "{status:?}");
    assert!(nota.terminal_restored());
    let said = visible(&nota.output.lock().unwrap());
    assert_eq!(
        said.matches("not recording device missing").count(),
        2,
        "{said}"
    );
    // Acceptance (GAI-212): a failed start leaves no empty session.
    assert_eq!(sessions(&tmp.0), Vec::<String>::new());
}

/// The sessions in the data directory `data`, by name.
fn sessions(data: &Path) -> Vec<String> {
    let mut names: Vec<String> = StdFs
        .list(&data.join("sessions"))
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// The names of the threads of process `pid` that run a track's fsyncs.
fn sync_threads(pid: u32) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| name.starts_with("nota-sync"))
        .collect();
    names.sort();
    names
}

/// Acceptance (GAI-220): a recording of one track fsyncs inline, on the
/// recorder thread; one of two tracks runs a sync thread for each.
#[test]
fn one_track_fsyncs_inline_and_two_on_a_thread_each() {
    let tmp = TestDir::new("syncing");
    let mut one = Running::start_with(&tmp.0, &["--mic", "missing"]);
    assert!(one.shows_after(0, "s stop"), "{}", one.output());
    // The track has joined the recorder once its first journal is there.
    let audio = tmp.0.join("sessions/1/audio");
    assert!(wait_until(Duration::from_secs(10), || {
        StdFs.list(&audio).unwrap_or_default().iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("journal-"))
        })
    }));
    assert_eq!(sync_threads(one.pid()), Vec::<String>::new());
    one.signal(Signal::TERM);
    assert!(one.exits().expect("nota didn't stop").success());

    let mut two = recording(&tmp.0);
    assert!(
        wait_until(Duration::from_secs(10), || sync_threads(two.pid())
            == ["nota-sync-0", "nota-sync-1"]),
        "{:?}",
        sync_threads(two.pid())
    );
    two.signal(Signal::TERM);
    assert!(two.exits().expect("nota didn't stop").success());
    assert_everything_sent_saved(&two, &tmp.0, 2, &[0, 1], 1_450);
}

/// Acceptance (GAI-212): salvage shows a line on screen, and a stop asked
/// for meanwhile ends nota before any session is made. The library is
/// held locked, so salvaging the killed session waits on it.
#[test]
fn a_stop_during_salvage_makes_no_session() {
    let tmp = TestDir::new("stop-in-salvage");
    let mut killed = recording(&tmp.0);
    killed.signal(Signal::KILL);
    assert!(killed.exits().is_some());
    let lock = open_library(&tmp.0.join("library.db"));
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();

    let mut nota = Running::start_with(&tmp.0, &[]);
    // Drawn word by word, so the spaces between are cursor moves.
    assert!(nota.shows_after(0, "Checking"), "{}", nota.output());
    assert!(
        visible(&nota.output.lock().unwrap()).contains("recordings…"),
        "{}",
        nota.output()
    );
    nota.signal(Signal::TERM);
    lock.execute_batch("COMMIT").unwrap();
    drop(lock);
    let status = nota.exits().expect("nota didn't stop");
    assert!(!status.success(), "{status:?}");
    assert!(nota.terminal_restored());
    let said = visible(&nota.output.lock().unwrap());
    assert!(said.contains("stopped before recording started"), "{said}");
    assert!(
        !said.contains("s stop"),
        "the Recording screen started: {said}"
    );
    assert_eq!(sessions(&tmp.0), ["1"]);
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
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[1], 1_450);
}

/// The test models' directory, or `None` (said on stderr) if they're
/// absent and not required.
fn test_models() -> Option<PathBuf> {
    let root = std::env::var_os("NOTA_TEST_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/nota/test-models"))
        })
        .unwrap_or_default();
    let present = root
        .join("parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx")
        .is_file()
        && root.join("silero_vad_v6.onnx").is_file();
    if !present {
        assert!(
            std::env::var_os("NOTA_REQUIRE_TEST_MODELS").is_none_or(|v| v != "1"),
            "NOTA_REQUIRE_TEST_MODELS=1 but no test models in {} (run scripts/fetch-test-models.sh)",
            root.display()
        );
        let _ = writeln!(
            std::io::stderr(),
            "skipped: no test models in {} (run scripts/fetch-test-models.sh)",
            root.display()
        );
    }
    present.then_some(root)
}

/// Fields of `/proc/<pid>/stat` after the command's name: state, parent,
/// process group, and so on; `None` once it's gone.
fn stat(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    Some(fields.split(' ').map(str::to_owned).collect())
}

/// Whether `pid` is running (not gone, and not a zombie).
fn alive(pid: u32) -> bool {
    stat(pid).is_some_and(|f| f[0] != "Z")
}

fn process_group(pid: u32) -> u32 {
    stat(pid).unwrap()[2].parse().unwrap()
}

/// The engine children `nota` (process `parent`) is running now, leaving
/// out any that have exited and are still to be reaped.
fn engines_of(parent: u32) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let child = stat(pid).is_some_and(|f| f[0] != "Z" && f[1] == parent.to_string());
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        if child && cmdline.windows(11).any(|w| w == b"engine\0asr\0") {
            found.push(pid);
        }
    }
    found
}

/// `nota record` with the engine, recording; returns it and the engine's
/// process id.
fn recording_with_the_engine(data: &Path, models: &Path) -> (Running, u32) {
    let parakeet = models.join("parakeet-tdt-0.6b-v3-int8");
    let vad = models.join("silero_vad_v6.onnx");
    let nota = Running::start_as(
        data,
        &[
            "--parakeet",
            parakeet.to_str().unwrap(),
            "--vad",
            vad.to_str().unwrap(),
        ],
        true,
        &[],
    );
    assert!(nota.shows_after(0, "s stop"), "{}", nota.output());
    pause(Duration::from_millis(1_500));
    // One engine, still the first: nothing has restarted it.
    let engines = engines_of(nota.pid());
    assert_eq!(engines.len(), 1, "{engines:?}");
    (nota, engines[0])
}

/// Acceptance (GAI-202), end to end: the engine runs in a process group of
/// its own, so the hangup a closing terminal sends nota's group doesn't
/// reach it or restart it, and nota still stops with everything saved. (A
/// tone has no words, so the text reaching the end is shown by the
/// recorder's `hangup` test.)
#[test]
fn a_hangup_to_the_group_leaves_the_engine_to_nota() {
    let Some(models) = test_models() else {
        return;
    };
    let tmp = TestDir::new("group-hup");
    let (mut nota, engine) = recording_with_the_engine(&tmp.0, &models);
    assert_eq!(process_group(nota.pid()), nota.pid());
    assert_eq!(process_group(engine), engine);
    nota.signal_group(Signal::HUP);
    // While nota stops, the engine it had is the only one: none restarted.
    let mut others = Vec::new();
    let stopped = wait_until(Duration::from_secs(20), || {
        others.extend(engines_of(nota.pid()).into_iter().filter(|&p| p != engine));
        nota.child.try_wait().unwrap().is_some()
    });
    assert!(stopped, "nota didn't stop");
    assert!(others.is_empty(), "the engine was restarted: {others:?}");
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
    assert!(nota.terminal_restored());
    assert!(!alive(engine), "nota left its engine running");
    assert_everything_sent_saved(&nota, &tmp.0, 1, &[0, 1], 1_450);
}

/// Acceptance (GAI-202, GAI-203), end to end: nota killed outright takes
/// the engine with it, even if the engine is still loading its models, as
/// it may be here: the kernel kills it when nota dies.
#[test]
fn a_killed_nota_takes_its_engine_with_it() {
    let Some(models) = test_models() else {
        return;
    };
    let tmp = TestDir::new("engine-killed");
    let (mut nota, engine) = recording_with_the_engine(&tmp.0, &models);
    nota.signal(Signal::KILL);
    // Counted from the kill: nota is gone at once.
    assert!(
        wait_until(Duration::from_secs(3), || !alive(engine)),
        "the engine outlived nota"
    );
    assert!(nota.exits().is_some());
}

/// Stops a recording with `s`, then `y`, and waits for nota to exit.
fn stop_with_keys(nota: &mut Running) {
    let at = nota.len();
    nota.press("s");
    assert!(nota.shows_after(at, "stop recording?"), "{}", nota.output());
    // Past the guard that takes a quick `y` for typing.
    pause(Duration::from_millis(700));
    nota.press("y");
    let status = nota.exits().expect("nota didn't stop");
    assert!(status.success(), "{status:?}: {}", nota.output());
}

/// Each session's jobs and their states, in queue order.
fn jobs(data: &Path) -> Vec<(u64, JobState)> {
    Store::open(&data.join("library.db"))
        .unwrap()
        .jobs()
        .unwrap()
        .into_iter()
        .map(|job| (job.session.get(), job.state))
        .collect()
}

/// The session's final-pass text covers each of its published samples
/// once, on every track, and says how many samples each chunk held (the
/// stand-in engine's text).
fn assert_final_pass_covers(data: &Path, session: u64) {
    let store = Store::open(&data.join("library.db")).unwrap();
    let id = SessionId::new(session);
    let mut published: Vec<(u32, u64, u64)> = store
        .segments(id)
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.track().get(),
                r.range().start().get(),
                r.range().end().get(),
            )
        })
        .collect();
    published.sort_unstable();
    assert!(!published.is_empty());
    let texts = store.final_texts(id).unwrap();
    let mut covered: Vec<(u32, u64, u64)> = Vec::new();
    for text in &texts {
        let (from, to) = (text.range.start().get(), text.range.end().get());
        assert_eq!(text.text, Some(format!("{} samples", to - from)));
        assert_eq!(text.heard_by.engine, "fake");
        // Its words came through the engine protocol and were stored.
        let words: Vec<(&str, u64, u64)> = text
            .words
            .iter()
            .map(|w| (w.text.as_str(), w.range.start().get(), w.range.end().get()))
            .collect();
        let len = (to - from).to_string();
        assert_eq!(words, [(len.as_str(), from, to), ("samples", to, to)]);
        let track = text.track.get();
        match covered.last_mut() {
            Some(last) if last.0 == track && last.2 == from => last.2 = to,
            _ => covered.push((track, from, to)),
        }
    }
    let mut runs: Vec<(u32, u64, u64)> = Vec::new();
    for (track, from, to) in published {
        match runs.last_mut() {
            Some(last) if last.0 == track && last.2 == from => last.2 = to,
            _ => runs.push((track, from, to)),
        }
    }
    assert_eq!(covered, runs, "session {session}");
    for &(track, _, end) in &runs {
        assert_eq!(
            store.final_progress(id).unwrap()[&nota_core::TrackId::new(track)].get(),
            end
        );
    }
}

/// Acceptance (GAI-316, GAI-317), end to end with the tone and the
/// stand-in engine: a stop queues the session's final pass; Home runs it,
/// but not while a recording is going (here, in another nota); once that
/// stops, both sessions' passes run, in order, and each covers every
/// published sample once on both tracks.
#[test]
fn the_final_pass_runs_after_the_stop_and_never_during_a_recording() {
    let tmp = TestDir::new("final-pass");
    // `nota record` queues the jobs at its stop, and runs none itself.
    let mut first = recording(&tmp.0);
    stop_with_keys(&mut first);
    assert_eq!(jobs(&tmp.0), [(1, JobState::Waiting(None))]);

    let mut second = recording(&tmp.0);
    let home_dir = tmp.0.to_str().unwrap();
    let mut home = Running::start_command(
        &["--tone", "yes", "--fake-engine", "yes"],
        &tmp.0,
        &[],
        false,
        &[("HOME", home_dir)],
    );
    assert!(home.shows_after(0, "R last settings"), "{}", home.output());
    // Capture first: nothing runs while the other nota records.
    pause(Duration::from_secs(3));
    assert_eq!(jobs(&tmp.0), [(1, JobState::Waiting(None))]);
    let store = Store::open(&tmp.0.join("library.db")).unwrap();
    assert!(store.final_texts(SessionId::new(1)).unwrap().is_empty());

    stop_with_keys(&mut second);
    let done = [(1, JobState::Done), (2, JobState::Done)];
    assert!(
        wait_until(Duration::from_secs(60), || jobs(&tmp.0) == done),
        "{:?}",
        jobs(&tmp.0)
    );
    assert_final_pass_covers(&tmp.0, 1);
    assert_final_pass_covers(&tmp.0, 2);

    home.press("q");
    let status = home.exits().expect("nota didn't close");
    assert!(status.success(), "{status:?}");
}
