//! `nota record`: records the system audio and the microphone as two tracks
//! of a new session, shows the Recording screen with live text, and stops
//! safely however it's asked to.
//!
//! # Threads
//!
//! - **Main:** starts everything, runs the screen, and stops everything in
//!   order. The capture streams live here: they stop on the thread that
//!   started them.
//! - **Recorder:** records both tracks into one session writer
//!   ([`record_tracks`]); hands finished journals to the publisher and
//!   everything else to the live thread. It never waits on either.
//! - **Publisher:** publishes finished journals as segments.
//! - **Live:** feeds the engine, flushes it at each new epoch, and turns
//!   levels and text into screen updates (see [`crate::live`]).
//! - **Engine events:** passes the supervisor's events to the live thread.
//! - **Signals:** turns SIGHUP, SIGTERM and SIGINT into a request to close
//!   the screen, and notes SIGXCPU (see below).
//!
//! # Stopping
//!
//! The screen closes when `s` is confirmed with `y`, when a signal arrives,
//! or when the terminal fails (it's gone after a hangup). Whichever it is,
//! the same steps follow: the terminal is restored, the streams stop, the
//! recorder records what they had sent and returns, the writer finishes
//! (a last fsync of every journal), the publisher publishes the last
//! journals, and the engine is shut down. Further signals are ignored
//! meanwhile: the default action would kill the process before the last
//! segments are published. Anything left unpublished (a disk error, say) is
//! salvaged at the next start.
//!
//! # SIGXCPU
//!
//! Where rtkit grants the capture thread real-time priority, it also sets
//! the process's `RLIMIT_RTTIME` soft limit to about one quantum: a
//! real-time thread that runs longer than that without blocking gets
//! SIGXCPU, whose default action would end the recording. Here it's only
//! noted, and the summary warns about it. rtkit's hard limit (SIGKILL,
//! 200 ms by default) still stops a runaway real-time thread.

use std::error::Error;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use nota_core::{Clock, EpochId, SampleRate, SystemClock, TrackId, TrackTimeline};
use nota_recorder::capture::{
    Capture, CaptureBackend, RecordError, RecorderEvent, Source, record_tracks, start_tracks,
};
use nota_recorder::engine::{EngineCommand, EngineConfig, EngineEvent, EngineSupervisor};
use nota_recorder::fs::StdFs;
use nota_recorder::segment::{PublishReport, Publisher, SegmentLength};
use nota_recorder::session::{SessionDir, SessionStore, SessionWriter};
use nota_store::Store;
use nota_tui::{Annotation, Ended, Event, InputThread, Recording, RunError, Theme};

use crate::library::{Library, Salvaged};
use crate::live::{Actions, Live};
use crate::terminal::Screen;

/// Every track is recorded at the engine's rate.
const RATE: SampleRate = SampleRate::SPEECH;
/// The microphone's track.
pub(crate) const MIC: TrackId = TrackId::new(0);
/// The system audio's track.
pub(crate) const SYSTEM: TrackId = TrackId::new(1);

type BoxError = Box<dyn Error + Send + Sync>;

/// What to record, and where.
#[derive(Debug, Clone)]
pub(crate) struct RecordArgs {
    /// The data directory, holding `sessions/`.
    pub(crate) data: PathBuf,
    /// The session's title, shown on the screen.
    pub(crate) title: String,
    /// The engine's models, `--parakeet` and `--vad`; without them there's
    /// no live text.
    pub(crate) models: Option<(PathBuf, PathBuf)>,
    /// The microphone's source.
    pub(crate) mic: Source,
    /// The system audio's source.
    pub(crate) system: Source,
    /// Record a tone instead of the audio server (tests only).
    pub(crate) tone: bool,
}

/// How a recording went, for the summary printed after it.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// The session's directory.
    pub(crate) session: PathBuf,
    /// Things worth saying: sessions salvaged, streams that didn't start,
    /// journals left for salvage.
    pub(crate) notes: Vec<String>,
    /// Segments published.
    pub(crate) segments: usize,
    /// Whether everything recorded was published, with nothing left for
    /// the next start's salvage.
    pub(crate) complete: bool,
}

/// What the live thread is told.
#[derive(Debug)]
enum LiveInput {
    Recorder(Option<TrackId>, RecorderEvent),
    Engine(EngineEvent),
    /// Recording has ended: shut the engine down.
    Done,
}

/// Records a session until it's stopped, then finishes it.
///
/// # Errors
///
/// If the session can't be started (no clock, no data directory, no
/// stream at all), or the screen failed; in the second case the recording
/// was still finished first.
pub(crate) fn record(args: &RecordArgs) -> Result<Outcome, BoxError> {
    let clock: Arc<dyn Clock> =
        Arc::new(SystemClock::start().map_err(|_| "the system clock can't be read")?);
    #[cfg(feature = "fake-capture")]
    if args.tone {
        let tone = crate::tone::Tone::new(Arc::clone(&clock));
        let mut outcome = record_with(args, &tone, &clock)?;
        outcome.notes.extend(tone.report());
        return Ok(outcome);
    }
    if args.tone {
        return Err("--tone is only for nota's tests".into());
    }
    #[cfg(target_os = "linux")]
    return record_with(args, &nota_recorder::capture::PipeWireBackend, &clock);
    #[cfg(not(target_os = "linux"))]
    Err("recording needs Linux for now".into())
}

/// The window every segment is cut to.
pub(crate) fn segment_length() -> SegmentLength {
    SegmentLength::default_at(RATE)
}

/// The recorder thread's result: the writer, how recording ended, how
/// many journal failures it reported, and the streams that failed.
type Recorded = (
    SessionWriter<StdFs>,
    Result<(), RecordError>,
    usize,
    Vec<String>,
);

#[expect(
    clippy::too_many_lines,
    reason = "starting and stopping in one place keeps the order visible"
)]
fn record_with<B: CaptureBackend>(
    args: &RecordArgs,
    backend: &B,
    clock: &Arc<dyn Clock>,
) -> Result<Outcome, BoxError> {
    let mut outcome = Outcome::default();
    let (ui, ui_events) = mpsc::channel::<Event>();
    let signals = listen_for_signals(ui.clone())?;
    // The terminal first: without one there's nothing to record into.
    let screen = Screen::enter()?;

    let library = Library::open(&args.data)?;
    for salvaged in library.salvage_all(segment_length())? {
        outcome.notes.push(match salvaged {
            Salvaged::Done(id) => format!("salvaged session {}", id.get()),
            Salvaged::Left(id) => format!(
                "salvaged session {}, but some of its journals are still to publish",
                id.get()
            ),
            Salvaged::InUse(id) => format!("session {} is in use; not salvaged", id.get()),
            Salvaged::Failed(id, e) => format!("salvaging session {} failed: {e}", id.get()),
        });
    }
    let session = library.create()?;
    outcome.session.clone_from(&session.dir);
    let lock = SessionDir::new(session.id, StdFs, &session.audio()).lock()?;
    let store = Store::open(&session.store())?;
    let mut writer = SessionWriter::open(&lock, RATE, segment_length(), Arc::clone(clock))?;

    let sources = [(MIC, args.mic.clone()), (SYSTEM, args.system.clone())];
    let (started, events) = start_tracks(backend, &sources, RATE, clock);
    let mut captures: Vec<Capture<B::Stream>> = Vec::new();
    let mut listening = Vec::new();
    for (result, (_, source)) in started.into_iter().zip(&sources) {
        match result {
            Ok(capture) => {
                captures.push(capture);
                listening.push(source_name(source));
            }
            Err(e) => outcome.notes.push(format!("not recording {source}: {e}")),
        }
    }
    if captures.is_empty() {
        return Err(format!("nothing to record: {}", outcome.notes.join("; ")).into());
    }
    // Each track's first epoch starts when its own stream did: a stream
    // opened later doesn't push the first one's audio later.
    let mut timelines = Vec::new();
    for capture in &captures {
        let track = capture.track();
        let first = writer.first_free_sample(track);
        writer.start_track(track, EpochId::new(0), first)?;
        let mut timeline = TrackTimeline::new(track);
        timeline.open_epoch(capture.started_at(), first, RATE)?;
        timelines.push(timeline);
    }

    let publisher = Publisher::spawn(SessionStore::new(lock.clone(), store), segment_length())?;
    let (live_inputs, live_received) = mpsc::channel::<LiveInput>();
    let engine = match &args.models {
        Some((parakeet, vad)) => Some(start_engine(parakeet, vad, clock, &live_inputs)?),
        None => None,
    };
    let live = spawn_live(Live::new(&timelines), engine, live_received, ui.clone())?;
    let recorder = {
        let queue = publisher.queue();
        let live_inputs = live_inputs.clone();
        let ui = ui.clone();
        thread::Builder::new()
            .name("nota-recorder".into())
            .spawn(move || -> Recorded {
                let mut failures = 0;
                let mut lost = Vec::new();
                let result =
                    record_tracks(&mut writer, &mut timelines, &events, &mut |track, e| {
                        match e {
                            RecorderEvent::Finished(journals) => {
                                // A refused batch stays on disk for salvage.
                                let _sent = queue.send(journals);
                            }
                            e => {
                                match &e {
                                    RecorderEvent::JournalFailed(_) => failures += 1,
                                    RecorderEvent::CaptureFailed(error) => {
                                        lost.push(format!("{}: {error}", track_name(track)));
                                    }
                                    _ => {}
                                }
                                let _ = live_inputs.send(LiveInput::Recorder(track, e));
                            }
                        }
                    });
                // Every stream has ended, or recording failed: the screen
                // has nothing more to show, so close it and stop.
                let _ = ui.send(Event::Close);
                (writer, result, failures, lost)
            })?
    };

    // The screen, until it's closed.
    let shown = show(screen, args, &listening.join(" + "), clock, &ui, &ui_events);
    drop(ui);

    // Stop, in order.
    drop(captures);
    let (writer, how_it_ended, failures, failed_streams) = recorder
        .join()
        .map_err(|_| "the recorder stopped unexpectedly")?;
    if let Err(e) = how_it_ended {
        outcome.notes.push(format!("recording stopped early: {e}"));
    }
    for stream in failed_streams {
        outcome.notes.push(format!("stopped recording {stream}"));
    }
    if failures > 0 {
        outcome.notes.push(format!(
            "{failures} journal failures; the audio around them may have gaps"
        ));
    }
    let queue = publisher.queue();
    match writer.finish() {
        Ok(last) => {
            let _sent = queue.send(last);
        }
        Err(e) => {
            outcome.notes.push(format!("finishing the recording: {e}"));
            let _sent = queue.send(e.into_finished());
        }
    }
    drop(queue);
    let _ = live_inputs.send(LiveInput::Done);
    drop(live_inputs);
    let _ = live.join();
    let report = publisher.finish()?;
    note_published(&mut outcome, &report);
    if signals.close() {
        outcome.notes.push(
            "the capture thread ran past its real-time budget at least once \
             (SIGXCPU); the recording carried on"
                .to_owned(),
        );
    }
    drop(lock);

    let shown = shown?;
    if let Some(problem) = shown.problem {
        outcome.notes.push(problem);
    }
    if shown.marks > 0 {
        outcome.notes.push(format!(
            "{} marks and notes made; they aren't saved yet",
            shown.marks
        ));
    }
    Ok(outcome)
}

/// How the screen went.
struct Shown {
    /// Marks and notes made.
    marks: usize,
    /// Why it closed, if not as asked: the terminal failed (as after a
    /// hangup). The recording stops all the same.
    problem: Option<String>,
}

/// Shows the Recording screen on `screen` until it's closed, then restores
/// the terminal.
///
/// # Errors
///
/// Only if the input thread can't start.
fn show(
    mut screen: Screen,
    args: &RecordArgs,
    listening: &str,
    clock: &Arc<dyn Clock>,
    ui: &Sender<Event>,
    ui_events: &Receiver<Event>,
) -> io::Result<Shown> {
    let theme = if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        Theme::no_color()
    } else {
        Theme::default()
    };
    let mut recording = Recording::new(
        args.title.clone(),
        listening.to_owned(),
        Arc::clone(clock),
        theme,
    );
    let (annotations, made) = mpsc::channel::<Annotation>();
    let input = InputThread::spawn(ui.clone(), Arc::clone(clock))?;
    let ran = nota_tui::run(screen.terminal(), &mut recording, ui_events, &annotations);
    // Keys may be gone with the terminal; nothing to do about it.
    let _ = input.stop();
    drop(screen);
    let marks = made.try_iter().count();
    let (marks, problem) = match ran {
        Ok(Ended::Stopped | Ended::Closed) => (marks, None),
        Err(RunError::InputLost(kind)) => (marks, Some(format!("the keyboard was lost: {kind}"))),
        Err(RunError::Terminal(e)) => (marks, Some(format!("the screen failed: {e}"))),
        Err(RunError::AnnotationsClosed(_)) => (marks + 1, None),
    };
    Ok(Shown { marks, problem })
}

/// The handle that stops the signal thread.
#[cfg(unix)]
struct SignalThread {
    handle: signal_hook::iterator::Handle,
    /// Returns whether a SIGXCPU arrived.
    thread: JoinHandle<bool>,
}

#[cfg(unix)]
impl SignalThread {
    /// Stops the thread, and returns whether a SIGXCPU arrived.
    fn close(self) -> bool {
        self.handle.close();
        self.thread.join().unwrap_or(false)
    }
}

/// From now on, SIGHUP, SIGTERM and SIGINT close the screen rather than
/// end the process: the recording then stops in order (see the module
/// docs). SIGXCPU is only noted. Signals of one kind that arrive close
/// together may come through as one, so whether any arrived is all that's
/// known.
#[cfg(unix)]
fn listen_for_signals(ui: Sender<Event>) -> io::Result<SignalThread> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGXCPU};
    let signals = signal_hook::iterator::Signals::new([SIGHUP, SIGTERM, SIGINT, SIGXCPU])?;
    let handle = signals.handle();
    let thread = thread::Builder::new()
        .name("nota-signals".into())
        .spawn(move || watch(signals, &ui))?;
    Ok(SignalThread { handle, thread })
}

/// The signal thread: until `signals` is closed, asks `ui` to close for
/// every signal but SIGXCPU. Returns whether a SIGXCPU arrived.
#[cfg(unix)]
fn watch(mut signals: signal_hook::iterator::Signals, ui: &Sender<Event>) -> bool {
    use signal_hook::consts::SIGXCPU;
    let mut overran = false;
    for signal in signals.forever() {
        if signal == SIGXCPU {
            overran = true;
        } else {
            let _ = ui.send(Event::Close);
        }
    }
    // Once closed, the iterator stops without reading what's still
    // pending: a SIGXCPU during the stop would be missed.
    overran || signals.pending().any(|signal| signal == SIGXCPU)
}

/// Elsewhere nothing records yet (see [`record`]), so there's nothing to
/// stop in order.
#[cfg(not(unix))]
struct SignalThread;

#[cfg(not(unix))]
impl SignalThread {
    fn close(self) -> bool {
        false
    }
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same shape as the Unix version"
)]
fn listen_for_signals(_ui: Sender<Event>) -> io::Result<SignalThread> {
    Ok(SignalThread)
}

/// Starts the engine, with its events passed to the live thread.
fn start_engine(
    parakeet: &std::path::Path,
    vad: &std::path::Path,
    clock: &Arc<dyn Clock>,
    live: &Sender<LiveInput>,
) -> Result<EngineSupervisor, BoxError> {
    let command = EngineCommand {
        program: std::env::current_exe()?,
        args: vec![
            "engine".into(),
            "asr".into(),
            "--parakeet".into(),
            parakeet.as_os_str().to_owned(),
            "--vad".into(),
            vad.as_os_str().to_owned(),
        ],
    };
    let (supervisor, events) =
        EngineSupervisor::start(EngineConfig::new(command), Arc::clone(clock))?;
    let live = live.clone();
    thread::Builder::new()
        .name("nota-engine-events".into())
        .spawn(move || {
            for event in events {
                if live.send(LiveInput::Engine(event)).is_err() {
                    break;
                }
            }
        })?;
    Ok(supervisor)
}

/// The live thread: feeds the engine and the screen until told recording
/// is done, then shuts the engine down.
fn spawn_live(
    mut live: Live,
    mut engine: Option<EngineSupervisor>,
    inputs: Receiver<LiveInput>,
    ui: Sender<Event>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("nota-live".into())
        .spawn(move || {
            for input in inputs {
                let actions = match input {
                    LiveInput::Recorder(track, event) => live.recorder(track, event),
                    LiveInput::Engine(event) => live.engine(event),
                    LiveInput::Done => break,
                };
                apply(actions, engine.as_mut(), &ui);
            }
            if let Some(engine) = engine {
                engine.shutdown();
            }
        })
}

fn apply(actions: Actions, engine: Option<&mut EngineSupervisor>, ui: &Sender<Event>) {
    if let Some(engine) = engine {
        if let Some(chunk) = actions.transcribe {
            // Refused audio would only make the engine fail; the recording
            // has it.
            let _ = engine.send_audio(chunk);
        }
        if let Some(track) = actions.flush {
            engine.flush(track);
        }
    }
    for update in actions.updates {
        let _ = ui.send(Event::Update(update));
    }
}

fn note_published(outcome: &mut Outcome, report: &PublishReport) {
    outcome.segments = report.rows().len();
    outcome.complete = report.is_complete();
    if !report.is_complete() {
        let mut note = format!(
            "{} journals weren't published; the next start salvages them",
            report.left().len()
        );
        if let Some(e) = report.errors().last() {
            let _ = write!(note, " (last error: {e})");
        }
        outcome.notes.push(note);
    }
}

/// How the summary names a track.
fn track_name(track: Option<TrackId>) -> &'static str {
    match track {
        Some(MIC) => "the mic",
        Some(SYSTEM) => "the system audio",
        _ => "a track",
    }
}

/// How the footer names a source.
fn source_name(source: &Source) -> String {
    match source {
        Source::SystemAudio => "system audio".to_owned(),
        Source::Microphone => "mic".to_owned(),
        Source::Device(name) => name.clone(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use signal_hook::consts::{SIGHUP, SIGXCPU};
    use signal_hook::iterator::Signals;
    use signal_hook::low_level::raise;

    use super::*;

    /// A SIGXCPU that arrives during the stop, still pending when the
    /// signal thread is closed, is still noted.
    #[test]
    fn a_sigxcpu_still_pending_at_close_is_noted() {
        let (ui, closes) = mpsc::channel();
        let signals = Signals::new([SIGHUP, SIGXCPU]).unwrap();
        // Closed first, so the iterator never reads it: only what's
        // pending is left to find it.
        signals.handle().close();
        raise(SIGXCPU).unwrap();
        assert!(watch(signals, &ui));
        assert!(closes.try_recv().is_err(), "SIGXCPU closed the screen");
    }

    #[test]
    fn nothing_pending_at_close_is_no_overrun() {
        let (ui, _closes) = mpsc::channel();
        let signals = Signals::new([SIGHUP, SIGXCPU]).unwrap();
        signals.handle().close();
        raise(SIGHUP).unwrap();
        assert!(!watch(signals, &ui));
    }
}
