//! Supervising the speech engine child (`nota engine asr`).
//!
//! The recorder never waits on the engine: [`EngineSupervisor::send_audio`]
//! only puts the audio on a channel, and a supervisor thread owns the child.
//! That thread:
//! - restarts the engine when it exits, sends something that doesn't parse
//!   or doesn't fit, or hangs (no handshake within
//!   [`EngineConfig::start_timeout`], or audio unconfirmed with nothing
//!   confirmed for [`EngineConfig::request_timeout`]), or falls more than
//!   [`EngineConfig::max_unconfirmed`] behind, after a backoff that
//!   doubles from [`EngineConfig::initial_backoff`] up to
//!   [`EngineConfig::max_backoff`], is cut to at most 2 s when a new engine
//!   says hello, and resets once it confirms audio;
//! - keeps each track's unconfirmed audio and resends it to the new engine,
//!   so transcription resumes from the last sample the engine confirmed;
//! - passes text on only once the engine has confirmed it, so no text is
//!   lost or repeated across a restart;
//! - reports [`EngineStatus::Offline`] (the "⚠ transcriber offline" state)
//!   and [`EngineStatus::Online`] as [`EngineEvent`]s, with the end of the
//!   engine's stderr when it exits.
//!
//! The engine's stderr is always captured, never passed through: the TUI
//! owns the terminal.
//!
//! Audio goes to the journal separately and never through here, so the
//! recording carries on whatever the engine does.

mod replay;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufReader, BufWriter, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::messages::{AudioChunk, FromEngine, ProtocolVersion, ToEngine, Transcript};
use nota_core::protocol::{Frame, FrameReader, ReadError, write_frame};
use nota_core::{Clock, SampleCount, SampleIndex, SampleRange, SampleRate, SessionTime, TrackId};

use replay::Replay;

/// How to start the engine.
#[derive(Debug, Clone)]
pub struct EngineCommand {
    /// The program: normally the `nota` executable itself.
    pub program: PathBuf,
    /// Its arguments, e.g. `engine asr --parakeet DIR --vad FILE`.
    pub args: Vec<OsString>,
}

/// How the supervisor runs the engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// How to start it.
    pub command: EngineCommand,
    /// How long a new engine has to load its models and say hello.
    pub start_timeout: Duration,
    /// How long audio may go unconfirmed, with nothing confirmed on any
    /// track, before the engine counts as hung. Text alone doesn't count.
    /// The live chunker confirms within its 10 s cap plus decoding; a slow
    /// engine working through a backlog keeps confirming, so it isn't hung. Session time counts through
    /// suspend, so after a long sleep the engine is restarted; that's
    /// harmless, as it resumes from the last confirmed sample.
    pub request_timeout: Duration,
    /// The wait before the first restart.
    pub initial_backoff: Duration,
    /// The longest wait between restarts.
    pub max_backoff: Duration,
    /// How long a track may go without new audio before what the engine
    /// holds of it is flushed. The recorder flushes when a track stops or
    /// its epoch ends; this covers audio that just stops coming. The
    /// recorder must deliver audio more often than this, or the flush cuts
    /// mid-speech.
    pub idle_flush: Duration,
    /// The most unconfirmed audio kept per track. While the engine is
    /// down, older audio is dropped from the live view (it's still in the
    /// recording) and reported as [`EngineEvent::Skipped`]. A new engine
    /// is given at most half of it, the oldest dropped likewise, so it has
    /// room to catch up; one that still falls further behind than this is
    /// restarted. Half of it must be well over what a healthy engine holds
    /// before it confirms (the live chunker's 10 s cap plus decoding), so
    /// set it to at least 30 s of audio; less restarts every engine as
    /// behind before it can confirm anything.
    pub max_unconfirmed: SampleCount,
}

impl EngineConfig {
    /// The defaults: 60 s to start, 20 s per request, backoff from 250 ms to
    /// 30 s, a flush after 5 s without audio, and 10 minutes of audio at
    /// 16 kHz kept while down.
    #[must_use]
    pub const fn new(command: EngineCommand) -> Self {
        Self {
            command,
            start_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(20),
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(30),
            idle_flush: Duration::from_secs(5),
            max_unconfirmed: SampleCount::new(16_000 * 600),
        }
    }
}

/// What the supervisor reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// Final text for a run of a track's samples. Each sample is covered at
    /// most once, in order.
    Transcript(Transcript),
    /// The engine has dealt with every sample of `track` before `up_to`.
    Confirmed {
        /// The track.
        track: TrackId,
        /// The first sample not yet confirmed.
        up_to: SampleIndex,
    },
    /// The engine came up or went down.
    Status(EngineStatus),
    /// Audio dropped from the live view: kept too long while the engine was
    /// down, or the audio engines kept failing on.
    Skipped {
        /// The track.
        track: TrackId,
        /// The samples never transcribed live.
        range: SampleRange,
    },
}

/// Whether the transcriber is working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStatus {
    /// An engine said hello and is transcribing.
    Online {
        /// Its process id.
        pid: u32,
    },
    /// The engine is down and will be restarted: "⚠ transcriber offline".
    Offline(OfflineReason),
}

/// Why the engine went down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineReason {
    /// It couldn't be started.
    SpawnFailed(String),
    /// It exited, or closed its output.
    Exited {
        /// How it ended, if known.
        status: Option<ExitStatus>,
        /// The end of what it wrote to stderr (up to [`STDERR_TAIL`]
        /// bytes), which says why; empty if nothing.
        stderr: String,
    },
    /// No hello within the start timeout.
    StartTimeout,
    /// Audio went unconfirmed past the request timeout.
    Hung,
    /// It fell more than [`EngineConfig::max_unconfirmed`] behind.
    Behind,
    /// It speaks another protocol version.
    Version(ProtocolVersion),
    /// It sent something that doesn't parse or doesn't fit.
    Protocol(String),
}

impl fmt::Display for OfflineReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed(err) => write!(f, "couldn't start: {err}"),
            Self::Exited { status, stderr } => {
                match status {
                    Some(status) => write!(f, "exited ({status})")?,
                    None => write!(f, "exited")?,
                }
                match stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
                    Some(last) => write!(f, ": {last}"),
                    None => Ok(()),
                }
            }
            Self::StartTimeout => write!(f, "didn't start in time"),
            Self::Hung => write!(f, "stopped answering"),
            Self::Behind => write!(f, "fell too far behind"),
            Self::Version(v) => write!(f, "speaks protocol v{}", v.get()),
            Self::Protocol(what) => write!(f, "protocol error: {what}"),
        }
    }
}

/// Audio the supervisor refuses, without queueing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRefused {
    /// It starts before the end of the audio already sent for its track.
    Overlaps {
        /// Where the track's next audio may start, at the earliest.
        expected: SampleIndex,
        /// Where the refused audio starts.
        got: SampleIndex,
    },
    /// It isn't at 16 kHz, the only rate the engine takes.
    Rate(SampleRate),
    /// The supervisor thread has stopped (it panicked); nothing more is
    /// transcribed.
    Stopped,
}

impl fmt::Display for AudioRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overlaps { expected, got } => write!(
                f,
                "audio at sample {} overlaps audio already sent (next is {})",
                got.get(),
                expected.get()
            ),
            Self::Rate(rate) => write!(f, "audio at {} Hz; the engine takes 16 kHz", rate.hz()),
            Self::Stopped => write!(f, "the engine supervisor has stopped"),
        }
    }
}

impl std::error::Error for AudioRefused {}

/// The recorder's handle on the engine. Dropping it shuts the engine down.
#[derive(Debug)]
pub struct EngineSupervisor {
    inputs: Sender<Input>,
    thread: Option<JoinHandle<()>>,
    next: BTreeMap<TrackId, SampleIndex>,
}

impl EngineSupervisor {
    /// Starts the supervisor thread, which starts the engine. Events come
    /// on the returned receiver.
    ///
    /// # Errors
    ///
    /// If the thread can't be spawned. Failing to start the engine itself
    /// isn't an error here: it's reported as [`EngineStatus::Offline`] and
    /// retried.
    pub fn start(
        config: EngineConfig,
        clock: Arc<dyn Clock>,
    ) -> io::Result<(Self, Receiver<EngineEvent>)> {
        let (inputs, rx) = mpsc::channel();
        let (events, events_rx) = mpsc::channel();
        let loop_inputs = inputs.clone();
        let thread = thread::Builder::new()
            .name("nota-engine-supervisor".into())
            .spawn(move || Supervisor::new(config, clock, loop_inputs, events).run(&rx))?;
        Ok((
            Self {
                inputs,
                thread: Some(thread),
                next: BTreeMap::new(),
            },
            events_rx,
        ))
    }

    /// Queues audio for transcription. Never blocks. Each track's audio
    /// must not start before the end of its previous audio; if it starts
    /// later, the engine treats it as a new stream.
    ///
    /// # Errors
    ///
    /// [`AudioRefused`] if it overlaps audio already sent or isn't at
    /// 16 kHz (such audio would only make the engine fail), or if the
    /// supervisor thread has stopped; nothing is queued.
    pub fn send_audio(&mut self, chunk: AudioChunk) -> Result<(), AudioRefused> {
        let range = chunk.range();
        if chunk.rate() != SampleRate::SPEECH {
            return Err(AudioRefused::Rate(chunk.rate()));
        }
        if let Some(&expected) = self.next.get(&chunk.track())
            && range.start() < expected
        {
            return Err(AudioRefused::Overlaps {
                expected,
                got: range.start(),
            });
        }
        let track = chunk.track();
        // Fails only if the supervisor thread is gone: it ends only at
        // shutdown, or by panicking.
        self.inputs
            .send(Input::Audio(chunk))
            .map_err(|_| AudioRefused::Stopped)?;
        self.next.insert(track, range.end());
        Ok(())
    }

    /// Asks the engine to transcribe what it holds for `track` without
    /// waiting for a pause (the track stopped or its epoch ended).
    pub fn flush(&self, track: TrackId) {
        let _ = self.inputs.send(Input::Flush(track));
    }

    /// Stops the engine: flushes every track, closes its stdin so it
    /// exits, and kills it if it hasn't within three seconds. Text the
    /// engine sends in that time is passed on; audio it never confirmed is
    /// reported as [`EngineEvent::Skipped`].
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.inputs.send(Input::Shutdown);
        if let Some(thread) = self.thread.take() {
            // A panic in the supervisor thread has nothing left to clean up.
            let _ = thread.join();
        }
    }
}

impl Drop for EngineSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How long a closed engine gets to exit before it's killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// The most the backoff can be once an engine has said hello: it started,
/// so the next restart is quick enough for text to resume within 10 s,
/// but an engine that dies after every hello still restarts no faster
/// than this.
const HELLO_BACKOFF: Duration = Duration::from_secs(2);

/// How long a killed engine gets to be reaped. One stuck in the kernel
/// is left to a thread of its own rather than holding up the supervisor.
const REAP_WAIT: Duration = Duration::from_secs(2);

/// How long a dead engine's stderr gets to reach its end.
const STDERR_WAIT: Duration = Duration::from_millis(500);

/// The engine's output ended; `fail` fills in how and why.
const EXITED: OfflineReason = OfflineReason::Exited {
    status: None,
    stderr: String::new(),
};

/// How much of the end of the engine's stderr is kept.
pub const STDERR_TAIL: usize = 2_048;

/// What the supervisor thread waits on.
#[derive(Debug)]
enum Input {
    Audio(AudioChunk),
    Flush(TrackId),
    Shutdown,
    /// From the reader thread of engine number `generation`.
    Engine {
        generation: u64,
        event: FromChild,
    },
}

#[derive(Debug)]
enum FromChild {
    Frame(Frame<FromEngine>),
    /// The output ended cleanly between frames.
    End,
    Failed(ReadError),
}

/// A running engine process. Dropping it kills the engine, so even a panic
/// in the supervisor doesn't leave one running.
#[derive(Debug)]
struct Running {
    generation: u64,
    pid: u32,
    /// `None` once killed.
    process: Option<Child>,
    /// Frames for the writer thread, which owns the child's stdin; dropping
    /// it closes stdin once queued frames are written.
    writer: Sender<Frame<ToEngine>>,
    /// The end of its stderr, sent once stderr closes.
    stderr: Receiver<String>,
}

impl Running {
    /// Kills and reaps the engine; returns how it ended, if known.
    fn kill(&mut self) -> Option<ExitStatus> {
        self.process.take().and_then(kill)
    }

    /// The end of what the engine wrote to stderr, once it has closed.
    fn stderr_tail(&self) -> String {
        self.stderr.recv_timeout(STDERR_WAIT).unwrap_or_default()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.kill();
    }
}

#[derive(Debug)]
enum Phase {
    /// Down; restart at the given time.
    Waiting(SessionTime),
    /// Started; must say hello by the given time.
    Starting(Running, SessionTime),
    Online(Running),
}

struct Supervisor {
    config: EngineConfig,
    clock: Arc<dyn Clock>,
    inputs: Sender<Input>,
    events: Sender<EngineEvent>,
    tracks: BTreeMap<TrackId, Replay>,
    phase: Phase,
    backoff: Duration,
    generation: u64,
    /// When the running engine last made progress: said hello or confirmed
    /// audio. A slow engine working through a backlog isn't hung; one that
    /// sends text but never confirms it is.
    progress: SessionTime,
}

/// After this many engines in a row fail with a track's first unconfirmed
/// sample unmoved, the audio from there is taken to be what kills them.
const POISON_STRIKES: u32 = 3;

/// The same for a run of failures ending in a hang. A hang can't be told
/// from a stalled machine, so it takes more of them; but audio that always
/// hangs the engine mustn't keep it offline for the rest of the session.
const HANG_STRIKES: u32 = 5;

/// How much of that audio is skipped: one chunk at the live cap, 10 s at
/// 16 kHz, the most the engine decodes at once.
const POISON_SKIP: SampleCount = SampleCount::new(160_000);

impl Supervisor {
    fn new(
        config: EngineConfig,
        clock: Arc<dyn Clock>,
        inputs: Sender<Input>,
        events: Sender<EngineEvent>,
    ) -> Self {
        let now = clock.now();
        Self {
            backoff: config.initial_backoff,
            config,
            clock,
            inputs,
            events,
            tracks: BTreeMap::new(),
            phase: Phase::Waiting(now),
            generation: 0,
            progress: now,
        }
    }

    fn run(mut self, rx: &Receiver<Input>) {
        loop {
            self.check_deadlines();
            let wait = self
                .next_deadline()
                .map_or(Duration::from_secs(3_600), |at| {
                    at.checked_duration_since(self.clock.now())
                        .unwrap_or(Duration::ZERO)
                });
            let input = match rx.recv_timeout(wait) {
                Ok(input) => input,
                Err(RecvTimeoutError::Timeout) => continue,
                // Can't happen while `self.inputs` is alive; stop anyway.
                Err(RecvTimeoutError::Disconnected) => Input::Shutdown,
            };
            match input {
                Input::Audio(chunk) => self.on_audio(&chunk),
                Input::Flush(track) => self.on_flush(track),
                Input::Shutdown => return self.shut_down(rx),
                Input::Engine { generation, event } if self.running() == Some(generation) => {
                    self.on_child(event);
                }
                // From an engine already killed.
                Input::Engine { .. } => {}
            }
        }
    }

    /// The generation of the engine running now, if one is.
    const fn running(&self) -> Option<u64> {
        match &self.phase {
            Phase::Starting(running, _) | Phase::Online(running) => Some(running.generation),
            Phase::Waiting(_) => None,
        }
    }

    fn emit(&self, event: EngineEvent) {
        // The recorder may have stopped listening; nothing to do then.
        let _ = self.events.send(event);
    }

    fn next_deadline(&self) -> Option<SessionTime> {
        let idle = self
            .tracks
            .values()
            .filter_map(Replay::idle_since)
            .min()
            .and_then(|since| since.checked_add(self.config.idle_flush));
        let phase = self.phase_deadline();
        match (idle, phase) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn phase_deadline(&self) -> Option<SessionTime> {
        match &self.phase {
            Phase::Waiting(at) | Phase::Starting(_, at) => Some(*at),
            // Hung: audio waiting for its confirmation for the whole
            // timeout, with nothing else confirmed in that time either.
            Phase::Online(_) => self
                .tracks
                .values()
                .filter_map(Replay::oldest_sent)
                .min()
                .map(|sent| sent.max(self.progress))
                .and_then(|since| since.checked_add(self.config.request_timeout)),
        }
    }

    fn check_deadlines(&mut self) {
        let now = self.clock.now();
        let idle: Vec<TrackId> = self
            .tracks
            .iter()
            .filter(|(_, replay)| {
                replay
                    .idle_since()
                    .and_then(|since| since.checked_add(self.config.idle_flush))
                    .is_some_and(|at| at <= now)
            })
            .map(|(&track, _)| track)
            .collect();
        for track in idle {
            self.on_flush(track);
        }
        let due = self.phase_deadline().is_some_and(|at| at <= now);
        if !due {
            return;
        }
        match self.phase {
            Phase::Waiting(_) => self.spawn(),
            Phase::Starting(..) => self.fail(OfflineReason::StartTimeout),
            Phase::Online(_) => self.fail(OfflineReason::Hung),
        }
    }

    fn on_audio(&mut self, chunk: &AudioChunk) {
        let track = chunk.track();
        self.tracks
            .entry(track)
            .or_insert_with(|| Replay::new(track))
            .push_audio(chunk, self.clock.now());
        self.after_push(track);
    }

    fn on_flush(&mut self, track: TrackId) {
        if let Some(replay) = self.tracks.get_mut(&track) {
            replay.push_flush();
            self.after_push(track);
        }
    }

    /// Sends what's new to a running engine, or bounds what's kept for the
    /// next one.
    fn after_push(&mut self, track: TrackId) {
        let keep = self.config.max_unconfirmed;
        if matches!(self.phase, Phase::Online(_)) {
            self.send_unsent();
            let behind = self
                .tracks
                .get(&track)
                .is_some_and(|replay| replay.unconfirmed() > keep.get());
            if behind {
                self.fail(OfflineReason::Behind);
            }
            return;
        }
        self.trim(track, keep);
    }

    /// Drops the track's oldest audio down to `keep` samples, and says so.
    fn trim(&mut self, track: TrackId, keep: SampleCount) {
        let dropped = self
            .tracks
            .get_mut(&track)
            .map(|replay| replay.trim(keep))
            .unwrap_or_default();
        for range in dropped {
            self.emit(EngineEvent::Skipped { track, range });
        }
    }

    fn send_unsent(&mut self) {
        let Phase::Online(running) = &self.phase else {
            return;
        };
        let now = self.clock.now();
        for replay in self.tracks.values_mut() {
            for message in replay.take_unsent(now) {
                // A send fails only if the writer thread has stopped, which
                // means the engine's stdin broke; its output ending will
                // say why.
                let _ = running.writer.send(Frame::Message(message));
            }
        }
    }

    fn on_child(&mut self, event: FromChild) {
        let frame = match event {
            FromChild::Frame(frame) => frame,
            FromChild::End => return self.fail(EXITED),
            // A crash can leave half a frame, or break the pipe: the engine
            // died, it didn't misspeak.
            FromChild::Failed(ReadError::Truncated | ReadError::Io(_)) => {
                return self.fail(EXITED);
            }
            FromChild::Failed(err) => return self.fail(OfflineReason::Protocol(err.to_string())),
        };
        match (&self.phase, frame) {
            (Phase::Starting(..), Frame::Hello(version)) if version == ProtocolVersion::CURRENT => {
                self.online();
            }
            (Phase::Starting(..), Frame::Hello(version)) => {
                self.fail(OfflineReason::Version(version));
            }
            (Phase::Online(_), Frame::Message(message)) => {
                if let Err(err) = self.on_message(message) {
                    self.fail(OfflineReason::Protocol(err.into()));
                }
            }
            (_, Frame::Hello(_)) => self.fail(OfflineReason::Protocol("second hello".into())),
            (_, Frame::Message(_)) => {
                self.fail(OfflineReason::Protocol("message before hello".into()));
            }
        }
    }

    fn online(&mut self) {
        let phase = std::mem::replace(&mut self.phase, Phase::Waiting(self.clock.now()));
        self.progress = self.clock.now();
        // It started: if it dies before confirming anything, the restart
        // still comes soon enough for text to resume within 10 s.
        self.backoff = self.backoff.min(HELLO_BACKOFF);
        self.phase = match phase {
            Phase::Starting(running, _) => {
                let pid = running.pid;
                self.emit(EngineEvent::Status(EngineStatus::Online { pid }));
                Phase::Online(running)
            }
            other => other,
        };
        // A new engine gets at most half the limit, so it has room to catch
        // up rather than falling behind on the next audio however long the
        // outage was.
        let half = SampleCount::new(self.config.max_unconfirmed.get() / 2);
        let tracks: Vec<TrackId> = self.tracks.keys().copied().collect();
        for track in tracks {
            self.trim(track, half);
        }
        self.send_unsent();
    }

    fn on_message(&mut self, message: FromEngine) -> Result<(), &'static str> {
        match message {
            FromEngine::Transcript(transcript) => {
                let replay = self
                    .tracks
                    .get_mut(&transcript.track)
                    .ok_or("text for an unknown track")?;
                replay.on_transcript(transcript).map_err(|v| v.0)
            }
            FromEngine::Confirmed { track, up_to } => {
                let replay = self
                    .tracks
                    .get_mut(&track)
                    .ok_or("confirmed an unknown track")?;
                let done = replay.on_confirmed(up_to).map_err(|v| v.0)?;
                // Only confirmations count: text alone doesn't move the
                // first unconfirmed sample.
                self.progress = self.clock.now();
                // The new engine works: the next failure starts the backoff
                // afresh.
                self.backoff = self.config.initial_backoff;
                for transcript in done {
                    self.emit(EngineEvent::Transcript(transcript));
                }
                self.emit(EngineEvent::Confirmed { track, up_to });
                Ok(())
            }
        }
    }

    fn spawn(&mut self) {
        self.generation += 1;
        match start_child(&self.config.command, self.generation, &self.inputs) {
            Ok(running) => {
                let deadline = self.clock.now().checked_add(self.config.start_timeout);
                let deadline = deadline.unwrap_or(SessionTime::from_nanos(u64::MAX));
                self.phase = Phase::Starting(running, deadline);
            }
            Err(err) => self.fail(OfflineReason::SpawnFailed(err.to_string())),
        }
    }

    /// The engine is down: kill what's left of it, report it, and schedule
    /// the restart.
    fn fail(&mut self, reason: OfflineReason) {
        let now = self.clock.now();
        let restart = now.checked_add(self.backoff).unwrap_or(now);
        let phase = std::mem::replace(&mut self.phase, Phase::Waiting(restart));
        // A track whose audio was sent to an engine that then crashed,
        // misspoke or hung, with the track's first unconfirmed sample
        // unmoved, may hold the audio that kills it. Falling behind says
        // nothing about the audio.
        let strikes = match reason {
            OfflineReason::Behind => None,
            OfflineReason::Hung => Some(HANG_STRIKES),
            _ => Some(POISON_STRIKES),
        };
        let poisoned: Vec<TrackId> = match strikes {
            // Only audio sent to an engine counts against a track, and none
            // is sent until it's online.
            Some(limit) => self
                .tracks
                .iter_mut()
                .filter_map(|(&track, replay)| replay.note_failure(limit).then_some(track))
                .collect(),
            None => Vec::new(),
        };
        let reason = match (phase, reason) {
            (
                Phase::Starting(mut running, _) | Phase::Online(mut running),
                OfflineReason::Exited { .. },
            ) => {
                let status = running.kill();
                OfflineReason::Exited {
                    status,
                    stderr: running.stderr_tail(),
                }
            }
            (Phase::Starting(mut running, _) | Phase::Online(mut running), reason) => {
                running.kill();
                reason
            }
            (Phase::Waiting(_), reason) => reason,
        };
        self.backoff = self.backoff.saturating_mul(2).min(self.config.max_backoff);
        for replay in self.tracks.values_mut() {
            replay.reset_sent();
        }
        self.emit(EngineEvent::Status(EngineStatus::Offline(reason)));
        // Tracks are sent in order, so the first one standing still is the
        // likeliest culprit; the others get a fresh count, and if the
        // poison was theirs they're caught next.
        for &track in poisoned.iter().skip(1) {
            if let Some(replay) = self.tracks.get_mut(&track) {
                replay.clear_strikes();
            }
        }
        for track in poisoned.into_iter().take(1) {
            let skipped = self
                .tracks
                .get_mut(&track)
                .map(|replay| replay.skip(POISON_SKIP))
                .unwrap_or_default();
            for range in skipped {
                self.emit(EngineEvent::Skipped { track, range });
            }
        }
        // Bound what's kept while down.
        let tracks: Vec<TrackId> = self.tracks.keys().copied().collect();
        for track in tracks {
            self.trim(track, self.config.max_unconfirmed);
        }
    }

    /// Flushes every track and closes the engine's stdin so it exits,
    /// passing on its last replies for up to [`SHUTDOWN_GRACE`] until its
    /// output ends, then kills it. Whatever is still unconfirmed is
    /// reported as skipped.
    fn shut_down(mut self, rx: &Receiver<Input>) {
        if matches!(self.phase, Phase::Online(_)) {
            for replay in self.tracks.values_mut() {
                replay.push_flush();
            }
            self.send_unsent();
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Waiting(self.clock.now()));
        if let Phase::Starting(running, _) | Phase::Online(running) = phase {
            self.drain(running, rx);
        }
        // Never transcribed live; it's still in the recording.
        let tracks: Vec<TrackId> = self.tracks.keys().copied().collect();
        for track in tracks {
            self.trim(track, SampleCount::new(0));
        }
    }

    /// Closes the engine's stdin and passes on its replies until its
    /// output ends or [`SHUTDOWN_GRACE`] is up, then kills it.
    fn drain(&mut self, mut running: Running, rx: &Receiver<Input>) {
        let generation = running.generation;
        // Closing the queue to the writer thread closes the engine's stdin
        // once the queued frames are written.
        let Running { writer, .. } = &mut running;
        drop(std::mem::replace(writer, mpsc::channel().0));
        let deadline = self.clock.now().checked_add(SHUTDOWN_GRACE);
        while let Some(left) = deadline.and_then(|at| at.checked_duration_since(self.clock.now())) {
            match rx.recv_timeout(left) {
                Ok(Input::Engine {
                    generation: g,
                    event: FromChild::End | FromChild::Failed(_),
                }) if g == generation => break,
                Ok(Input::Engine {
                    generation: g,
                    event: FromChild::Frame(Frame::Message(message)),
                }) if g == generation => {
                    if self.on_message(message).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        drop(running);
    }
}

/// Kills and reaps the engine; returns how it ended, if known. An engine
/// that can't be reaped within [`REAP_WAIT`] (stuck in the kernel) is left
/// to a thread that reaps it whenever it goes.
fn kill(mut process: Child) -> Option<ExitStatus> {
    // Fails only if it has already exited, which is what's wanted.
    let _ = process.kill();
    if let Ok(Some(status)) = process.try_wait() {
        return Some(status);
    }
    let shared = Arc::new(Mutex::new(Some(process)));
    let reaper = Arc::clone(&shared);
    match within(REAP_WAIT, move || reap(&reaper)) {
        Ok(status) => status,
        Err(Late::TooLong) => None,
        // No thread to leave it to: wait here rather than leave it
        // unreaped.
        Err(Late::NoThread) => reap(&shared),
    }
}

/// Waits for the engine in `slot`, if it's still there.
fn reap(slot: &Mutex<Option<Child>>) -> Option<ExitStatus> {
    let taken = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    taken.and_then(|mut process| process.wait().ok())
}

/// Why [`within`] has no result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Late {
    /// The work took longer than allowed; it carries on regardless.
    TooLong,
    /// No thread could be started, so the work never ran.
    NoThread,
}

/// Runs `work` on a thread of its own and waits up to `limit` for its
/// result.
fn within<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Late> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("nota-engine-reaper".into())
        .spawn(move || {
            // Nobody may be waiting any more; that's fine.
            let _ = tx.send(work());
        })
        .map_err(|_| Late::NoThread)?;
    rx.recv_timeout(limit).map_err(|_| Late::TooLong)
}

fn start_child(
    command: &EngineCommand,
    generation: u64,
    inputs: &Sender<Input>,
) -> io::Result<Running> {
    let mut process = Command::new(&command.program)
        .args(&command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pipes = (
        process.stdin.take(),
        process.stdout.take(),
        process.stderr.take(),
    );
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        kill(process);
        return Err(io::Error::other("engine started without pipes"));
    };
    let (writer, frames) = mpsc::channel();
    let (tail, stderr_rx) = mpsc::sync_channel(1);
    let threads = spawn_writer(stdin, frames, generation)
        .and_then(|()| spawn_reader(stdout, inputs.clone(), generation))
        .and_then(|()| spawn_stderr(stderr, tail, generation));
    if let Err(err) = threads {
        kill(process);
        return Err(err);
    }
    let _ = writer.send(Frame::Hello(ProtocolVersion::CURRENT));
    Ok(Running {
        generation,
        pid: process.id(),
        process: Some(process),
        writer,
        stderr: stderr_rx,
    })
}

/// Reads the engine's stderr to its end, keeping the last [`STDERR_TAIL`]
/// bytes, and sends them on once it closes.
fn spawn_stderr(
    stderr: ChildStderr,
    tail: mpsc::SyncSender<String>,
    generation: u64,
) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("nota-engine-stderr-{generation}"))
        .spawn(move || {
            // The supervisor may have stopped waiting; that's fine.
            let _ = tail.send(read_tail(stderr, STDERR_TAIL));
        })
        .map(drop)
}

/// Reads `input` to its end (or a read error) and returns its last `keep`
/// bytes as text, from the first whole line if the start was cut off.
fn read_tail(mut input: impl Read, keep: usize) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut cut = false;
    let mut buf = [0_u8; 1_024];
    loop {
        let read = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        kept.extend_from_slice(buf.get(..read).unwrap_or_default());
        if let Some(over) = kept.len().checked_sub(keep).filter(|&over| over > 0) {
            kept.drain(..over);
            cut = true;
        }
    }
    let text = String::from_utf8_lossy(&kept);
    let text = match text.split_once('\n') {
        Some((_, rest)) if cut => rest,
        _ => &text,
    };
    text.trim().to_owned()
}

/// Writes queued frames to the engine's stdin until the queue closes or a
/// write fails. Blocking here (a full pipe to a hung engine) never holds up
/// the supervisor.
fn spawn_writer(
    stdin: ChildStdin,
    frames: Receiver<Frame<ToEngine>>,
    generation: u64,
) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("nota-engine-writer-{generation}"))
        .spawn(move || {
            let mut stdin = BufWriter::new(stdin);
            for frame in frames {
                if write_frame(&mut stdin, &frame).is_err() {
                    return;
                }
            }
        })
        .map(drop)
}

/// Reads frames from the engine's stdout and hands them to the supervisor,
/// until the output ends or a frame is bad.
fn spawn_reader(stdout: ChildStdout, inputs: Sender<Input>, generation: u64) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("nota-engine-reader-{generation}"))
        .spawn(move || {
            let mut reader = FrameReader::new(BufReader::new(stdout));
            loop {
                let (event, last) = match reader.read_frame() {
                    Ok(Some(frame)) => (FromChild::Frame(frame), false),
                    Ok(None) => (FromChild::End, true),
                    Err(err) => (FromChild::Failed(err), true),
                };
                if inputs.send(Input::Engine { generation, event }).is_err() || last {
                    return;
                }
            }
        })
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_and_refusals_say_what_went_wrong() {
        assert_eq!(OfflineReason::Hung.to_string(), "stopped answering");
        assert_eq!(
            OfflineReason::Version(ProtocolVersion::new(2)).to_string(),
            "speaks protocol v2"
        );
        assert_eq!(
            OfflineReason::Protocol("bad".into()).to_string(),
            "protocol error: bad"
        );
        assert_eq!(EXITED.to_string(), "exited");
        assert_eq!(
            OfflineReason::Exited {
                status: None,
                stderr: "loading\nError: no model at /x\n  \n".into(),
            }
            .to_string(),
            "exited: Error: no model at /x"
        );
        assert_eq!(OfflineReason::Behind.to_string(), "fell too far behind");
        assert_eq!(
            AudioRefused::Stopped.to_string(),
            "the engine supervisor has stopped"
        );
        assert_eq!(
            OfflineReason::SpawnFailed("no such file".into()).to_string(),
            "couldn't start: no such file"
        );
        assert_eq!(
            OfflineReason::StartTimeout.to_string(),
            "didn't start in time"
        );
        let overlaps = AudioRefused::Overlaps {
            expected: SampleIndex::new(10),
            got: SampleIndex::new(5),
        };
        assert_eq!(
            overlaps.to_string(),
            "audio at sample 5 overlaps audio already sent (next is 10)"
        );
        let rate = AudioRefused::Rate(SampleRate::new(48_000).unwrap_or(SampleRate::SPEECH));
        assert_eq!(
            rate.to_string(),
            "audio at 48000 Hz; the engine takes 16 kHz"
        );
    }

    #[test]
    fn work_that_takes_too_long_is_left_behind() {
        assert_eq!(within(Duration::from_secs(5), || 7), Ok(7));
        // A wait stuck forever, as on an engine stuck in the kernel.
        let (keep, stuck) = mpsc::channel::<()>();
        let took = within(Duration::from_millis(50), move || stuck.recv().is_ok());
        assert_eq!(took, Err(Late::TooLong));
        drop(keep);
    }

    #[test]
    fn an_engine_already_taken_is_not_waited_for() {
        assert_eq!(reap(&Mutex::new(None)), None);
    }

    #[test]
    fn a_killed_engine_is_reaped() {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();
        let status = kill(child).unwrap();
        assert!(!status.success());
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn only_the_end_of_stderr_is_kept() {
        assert_eq!(read_tail(&b"  one\ntwo  \n"[..], 100), "one\ntwo");
        assert_eq!(read_tail(&b""[..], 100), "");
        // Cut to the last 12 bytes ("e\nfour\nfive\n", from mid-line), then
        // from the first whole line.
        let long = b"one\ntwo\nthree\nfour\nfive\n";
        assert_eq!(read_tail(&long[..], 12), "four\nfive");
        // Longer than one read.
        let many = (0..1_000).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let tail = read_tail(many.join("\n").as_bytes(), 20);
        assert_eq!(tail, "line 998\nline 999");
    }

    /// A reader that gives each of `parts` in turn, then ends.
    struct Scripted(std::collections::VecDeque<io::Result<&'static [u8]>>);

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(Err(err)) => Err(err),
                Some(Ok(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
            }
        }
    }

    #[test]
    fn reading_stderr_retries_interruptions_and_stops_at_errors() {
        let parts = vec![
            Ok(&b"one\n"[..]),
            Err(io::Error::from(io::ErrorKind::Interrupted)),
            Ok(&b"two\n"[..]),
            Err(io::Error::from(io::ErrorKind::BrokenPipe)),
            Ok(&b"three\n"[..]),
        ];
        assert_eq!(read_tail(Scripted(parts.into()), 100), "one\ntwo");
        // Exactly `keep` bytes: nothing was cut, so the first line stays.
        assert_eq!(read_tail(&b"ab\ncd"[..], 5), "ab\ncd");
    }

    #[test]
    fn audio_for_a_stopped_supervisor_is_refused() {
        let (inputs, rx) = mpsc::channel();
        // The thread ends, as a panic would end it, dropping its receiver.
        let thread = thread::spawn(move || drop(rx));
        thread.join().unwrap();
        let mut supervisor = EngineSupervisor {
            inputs,
            thread: None,
            next: BTreeMap::new(),
        };
        let chunk = |from| {
            AudioChunk::new(
                TrackId::new(0),
                SampleIndex::new(from),
                SampleRate::SPEECH,
                vec![0; 10],
            )
            .unwrap()
        };
        assert_eq!(supervisor.send_audio(chunk(0)), Err(AudioRefused::Stopped));
        // Nothing was taken as sent.
        assert!(supervisor.next.is_empty());
    }
}
