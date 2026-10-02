//! Supervising the speech engine child (`nota engine asr`).
//!
//! The recorder never waits on the engine: [`EngineSupervisor::send_audio`]
//! only puts the audio on a channel, and a supervisor thread owns the child.
//! That thread:
//! - restarts the engine when it exits, sends something that doesn't parse
//!   or doesn't fit, or hangs (no handshake within
//!   [`EngineConfig::start_timeout`], or audio unconfirmed for
//!   [`EngineConfig::request_timeout`]), after a backoff that doubles from
//!   [`EngineConfig::initial_backoff`] up to [`EngineConfig::max_backoff`]
//!   and resets once a new engine confirms audio;
//! - keeps each track's unconfirmed audio and resends it to the new engine,
//!   so transcription resumes from the last sample the engine confirmed;
//! - passes text on only once the engine has confirmed it, so no text is
//!   lost or repeated across a restart;
//! - reports [`EngineStatus::Offline`] (the "⚠ transcriber offline" state)
//!   and [`EngineStatus::Online`] as [`EngineEvent`]s.
//!
//! Audio goes to the journal separately and never through here, so the
//! recording carries on whatever the engine does.

mod replay;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::messages::{AudioChunk, FromEngine, ProtocolVersion, ToEngine, Transcript};
use nota_core::protocol::{Frame, FrameReader, ReadError, write_frame};
use nota_core::{Clock, SampleCount, SampleIndex, SampleRange, SessionTime, TrackId};

use replay::Replay;

/// How to start the engine.
#[derive(Debug, Clone)]
pub struct EngineCommand {
    /// The program: normally the `nota` executable itself.
    pub program: PathBuf,
    /// Its arguments, e.g. `engine asr --parakeet DIR --vad FILE`.
    pub args: Vec<OsString>,
    /// Where its stderr goes. Never the TUI's terminal in normal use.
    pub stderr: EngineStderr,
}

/// Where the engine's stderr goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStderr {
    /// Discarded.
    Null,
    /// The recorder's own stderr (for tests and debugging).
    Inherit,
}

/// How the supervisor runs the engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// How to start it.
    pub command: EngineCommand,
    /// How long a new engine has to load its models and say hello.
    pub start_timeout: Duration,
    /// How long audio may go unconfirmed before the engine counts as hung.
    /// The live chunker confirms within its 10 s cap plus decoding.
    pub request_timeout: Duration,
    /// The wait before the first restart.
    pub initial_backoff: Duration,
    /// The longest wait between restarts.
    pub max_backoff: Duration,
    /// The most unconfirmed audio kept per track while the engine is down;
    /// older audio is dropped from the live view (it's still in the
    /// recording) and reported as [`EngineEvent::Skipped`].
    pub max_unconfirmed: SampleCount,
}

impl EngineConfig {
    /// The defaults: 60 s to start, 20 s per request, backoff from 250 ms to
    /// 30 s, and 10 minutes of audio at 16 kHz kept while down.
    #[must_use]
    pub const fn new(command: EngineCommand) -> Self {
        Self {
            command,
            start_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(20),
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(30),
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
    /// Audio dropped from the live view while the engine was down too long.
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
    Exited(Option<ExitStatus>),
    /// No hello within the start timeout.
    StartTimeout,
    /// Audio went unconfirmed past the request timeout.
    Hung,
    /// It speaks another protocol version.
    Version(ProtocolVersion),
    /// It sent something that doesn't parse or doesn't fit.
    Protocol(String),
}

impl fmt::Display for OfflineReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed(err) => write!(f, "couldn't start: {err}"),
            Self::Exited(Some(status)) => write!(f, "exited ({status})"),
            Self::Exited(None) => write!(f, "exited"),
            Self::StartTimeout => write!(f, "didn't start in time"),
            Self::Hung => write!(f, "stopped answering"),
            Self::Version(v) => write!(f, "speaks protocol v{}", v.get()),
            Self::Protocol(what) => write!(f, "protocol error: {what}"),
        }
    }
}

/// Audio that starts before the end of the audio already sent for its
/// track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioOutOfOrder {
    /// Where the track's next audio may start, at the earliest.
    pub expected: SampleIndex,
    /// Where the refused audio starts.
    pub got: SampleIndex,
}

impl fmt::Display for AudioOutOfOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "audio at sample {} overlaps audio already sent (next is {})",
            self.got.get(),
            self.expected.get()
        )
    }
}

impl std::error::Error for AudioOutOfOrder {}

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
    /// [`AudioOutOfOrder`] if it overlaps audio already sent; nothing is
    /// queued.
    pub fn send_audio(&mut self, chunk: AudioChunk) -> Result<(), AudioOutOfOrder> {
        let range = chunk.range();
        if let Some(&expected) = self.next.get(&chunk.track())
            && range.start() < expected
        {
            return Err(AudioOutOfOrder {
                expected,
                got: range.start(),
            });
        }
        self.next.insert(chunk.track(), range.end());
        // Fails only if the supervisor thread is gone, which happens only
        // at shutdown.
        let _ = self.inputs.send(Input::Audio(chunk));
        Ok(())
    }

    /// Asks the engine to transcribe what it holds for `track` without
    /// waiting for a pause (the track stopped or its epoch ended).
    pub fn flush(&self, track: TrackId) {
        let _ = self.inputs.send(Input::Flush(track));
    }

    /// Stops the engine: closes its stdin so it exits, and kills it if it
    /// hasn't within a second.
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
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

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

/// A running engine process.
#[derive(Debug)]
struct Running {
    generation: u64,
    process: Child,
    /// Frames for the writer thread, which owns the child's stdin; dropping
    /// it closes stdin once queued frames are written.
    writer: Sender<Frame<ToEngine>>,
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
}

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
        match &self.phase {
            Phase::Waiting(at) | Phase::Starting(_, at) => Some(*at),
            Phase::Online(_) => self
                .tracks
                .values()
                .filter_map(Replay::oldest_sent)
                .min()
                .and_then(|sent| sent.checked_add(self.config.request_timeout)),
        }
    }

    fn check_deadlines(&mut self) {
        let now = self.clock.now();
        let due = self.next_deadline().is_some_and(|at| at <= now);
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
            .push_audio(chunk);
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
        if matches!(self.phase, Phase::Online(_)) {
            self.send_unsent();
            return;
        }
        let keep = self.config.max_unconfirmed;
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
            FromChild::End => return self.fail(OfflineReason::Exited(None)),
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
        self.phase = match phase {
            Phase::Starting(running, _) => {
                let pid = running.process.id();
                self.emit(EngineEvent::Status(EngineStatus::Online { pid }));
                Phase::Online(running)
            }
            other => other,
        };
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
        let status = match phase {
            Phase::Starting(mut running, _) | Phase::Online(mut running) => {
                kill(&mut running.process)
            }
            Phase::Waiting(_) => None,
        };
        let reason = match reason {
            OfflineReason::Exited(None) => OfflineReason::Exited(status),
            other => other,
        };
        self.backoff = (self.backoff * 2).min(self.config.max_backoff);
        for replay in self.tracks.values_mut() {
            replay.reset_sent();
        }
        self.emit(EngineEvent::Status(EngineStatus::Offline(reason)));
        // Bound what's kept while down.
        let tracks: Vec<TrackId> = self.tracks.keys().copied().collect();
        for track in tracks {
            self.after_push(track);
        }
    }

    /// Closes the engine's stdin so it exits, waits up to
    /// [`SHUTDOWN_GRACE`] for its output to end, then kills it.
    fn shut_down(mut self, rx: &Receiver<Input>) {
        let phase = std::mem::replace(&mut self.phase, Phase::Waiting(self.clock.now()));
        let (Phase::Starting(running, _) | Phase::Online(running)) = phase else {
            return;
        };
        let Running {
            generation,
            mut process,
            writer,
        } = running;
        drop(writer);
        let deadline = self.clock.now().checked_add(SHUTDOWN_GRACE);
        while let Some(left) = deadline.and_then(|at| at.checked_duration_since(self.clock.now())) {
            match rx.recv_timeout(left) {
                Ok(Input::Engine {
                    generation: g,
                    event: FromChild::End | FromChild::Failed(_),
                }) if g == generation => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        kill(&mut process);
    }
}

/// Kills and reaps the engine; returns how it ended, if known.
fn kill(process: &mut Child) -> Option<ExitStatus> {
    // Fails only if it has already exited, which is what's wanted.
    let _ = process.kill();
    process.wait().ok()
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
        .stderr(match command.stderr {
            EngineStderr::Null => Stdio::null(),
            EngineStderr::Inherit => Stdio::inherit(),
        })
        .spawn()?;
    let pipes = process.stdin.take().zip(process.stdout.take());
    let Some((stdin, stdout)) = pipes else {
        let _ = process.kill();
        let _ = process.wait();
        return Err(io::Error::other("engine started without pipes"));
    };
    let (writer, frames) = mpsc::channel();
    let threads = spawn_writer(stdin, frames, generation)
        .and_then(|()| spawn_reader(stdout, inputs.clone(), generation));
    if let Err(err) = threads {
        let _ = process.kill();
        let _ = process.wait();
        return Err(err);
    }
    let _ = writer.send(Frame::Hello(ProtocolVersion::CURRENT));
    Ok(Running {
        generation,
        process,
        writer,
    })
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
