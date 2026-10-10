//! Starting a recording: the terminal, the session, the tracks and the
//! threads that record them.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use nota_core::recorder::{self, Cause, Input, Setup, Warning, WarningState};
use nota_core::{Clock, SessionId, TrackId, wall_now};
use nota_recorder::capture::{
    Capture, CaptureBackend, CaptureReceiver, RecordError, RecorderEvent, Source, TrackStarter,
    prepare_tracks, record_tracks,
};
use nota_recorder::detect::Thresholds;
use nota_recorder::disk::{
    BALLAST_LEN, CHECK_INTERVAL, DiskMonitor, DiskReport, DiskWatch, MonitorConfig, Usage,
    WatchedFs, WatchedStore,
};
use nota_recorder::engine::{EngineCommand, EngineConfig, EngineSupervisor};
use nota_recorder::fs::{Fs, StdFs, is_disk_full};
use nota_recorder::segment::{PublishQueue, Publisher};
use nota_recorder::session::{SessionDir, SessionLock, SessionStore, SessionWriter, Syncing};
use nota_store::{Heard, NewSession, Track, TrackKind};
use nota_tui::Event;

use super::live::{LiveEnd, LiveInput, spawn_live};
use super::save::{Saver, ToSave};
use super::signals::{SignalThread, listen_for_signals};
use super::summary::{Outcome, file_names, held_notes, track_name};
use super::{BoxError, MIC, RATE, RecordArgs, SYSTEM, segment_length};
use crate::inhibit::{self, Logind, Sleep};
use crate::latency::{DrawEnds, LatencyLog};
use crate::library::{
    Library, NewSessionRows, Salvaged, SessionPaths, discard_empty, startup_watch,
};
use crate::live::Live;
use crate::terminal::Screen;

/// What the screen needs: the terminal, the channel its events come
/// through, and the footer's text.
pub(super) struct Screening {
    pub(super) screen: Screen,
    /// Closes the screen, among other things.
    pub(super) ui: Sender<Event>,
    pub(super) ui_events: Receiver<Event>,
    /// The footer: the sources recording.
    pub(super) listening: String,
    /// Where the screen's marks and notes go to be stored.
    pub(super) save: Sender<ToSave>,
}

/// The filesystem a recording writes through: the real one, watched for a
/// full disk (see [`nota_recorder::disk`]).
pub(super) type RecordFs = WatchedFs<StdFs>;

/// The ballast a tone recording keeps: nota's tests record many short
/// sessions, each into a data directory of its own, and needn't write
/// 256 MB for each.
const TONE_BALLAST_LEN: u64 = 1024 * 1024;

/// What stopping needs of what was started.
pub(super) struct Started<B: CaptureBackend> {
    pub(super) outcome: Outcome,
    pub(super) signals: SignalThread,
    pub(super) draws: Option<DrawEnds>,
    pub(super) library: Library,
    pub(super) session: SessionId,
    pub(super) lock: SessionLock<RecordFs>,
    pub(super) captures: Vec<Capture<B::Stream>>,
    /// Keeps the machine from sleeping, if logind gave the lock; dropped
    /// once recording has ended.
    pub(super) sleep: Sleep,
    /// Checks the disk and keeps the ballast; stopped after publishing, so
    /// a full disk then is still in the summary.
    pub(super) disk: DiskMonitor<StdFs>,
    pub(super) publisher: Publisher,
    pub(super) live_inputs: Sender<LiveInput>,
    pub(super) live: JoinHandle<LiveEnd>,
    pub(super) saver: Saver,
    pub(super) recorder: JoinHandle<Recorded>,
}

/// The recorder thread's result: the writer, how recording ended, how
/// many journal failures it reported, and the streams that failed.
pub(super) type Recorded = (
    SessionWriter<RecordFs>,
    Result<(), RecordError>,
    usize,
    Vec<String>,
);

/// Starts everything a recording needs, in order: the signals and the
/// terminal first (without one there's nothing to record into), then,
/// with a line on screen, the salvage of earlier sessions. Unless a stop
/// was asked for meanwhile, the session, the publisher and the recorder
/// follow, and only then the streams, one after another, each track
/// recorded as soon as its own has started; then the disk monitor, the
/// saver, the engine and the live thread. Sleep is held off (`logind`)
/// before the first stream opens. The terminal is `screen` and
/// the library `library` if given (the app's, already set up and open),
/// else they're set up here. A lent terminal was set up after the app's
/// own signal listener, so the order holds for it too.
///
/// # Errors
///
/// If any of it can't be started, a stop was asked for before the
/// session was made, or no stream at all starts. A start that fails
/// before any audio is recorded leaves no session behind.
pub(super) fn start<B: CaptureBackend>(
    args: &RecordArgs,
    setup: &Setup,
    backend: &B,
    clock: &Arc<dyn Clock>,
    logind: &dyn Logind,
    screen: Option<Screen>,
    library: Option<Library>,
) -> Result<(Started<B>, Screening), BoxError> {
    let mut notes = Vec::new();
    let handed = Handed { screen, library };
    start_with_notes(args, setup, backend, clock, logind, handed, &mut notes)
        .map_err(|error| startup_failure(error, notes))
}

/// What the app lent the recording: its terminal and its library, if it
/// did.
struct Handed {
    screen: Option<Screen>,
    library: Option<Library>,
}

/// Starts as [`start`] says, keeping recovery notes for a failed start.
fn start_with_notes<B: CaptureBackend>(
    args: &RecordArgs,
    setup: &Setup,
    backend: &B,
    clock: &Arc<dyn Clock>,
    logind: &dyn Logind,
    Handed { screen, library }: Handed,
    notes: &mut Vec<String>,
) -> Result<(Started<B>, Screening), BoxError> {
    let (ui, ui_events) = mpsc::channel::<Event>();
    let signals = listen_for_signals(ui.clone())?;
    // The terminal first: without one there's nothing to record into.
    let draws = args.latency_log.as_ref().map(|_| DrawEnds::default());
    let mut screen = match screen {
        Some(screen) => screen,
        None => Screen::enter(draws.clone().map(|d| (d, Arc::clone(clock))))?,
    };

    let library = library.map_or_else(|| Library::open(&args.data), Ok)?;
    show_recovering(&mut screen);
    let watch = recover(&library, args, notes)?;
    if let Some(why) = stop_asked(&ui, &ui_events) {
        return Err(why.into());
    }
    // Before a session is made for it: a setup with no track has nothing
    // to record.
    let sources = sources(setup);
    if sources.is_empty() {
        return Err("nothing to record".into());
    }
    let session = create_session(&library, &watch)?;
    // From here, a start that fails removes the session again, unless
    // audio reached it.
    let unmade = Unmade {
        fs: watch.fs(),
        session: session.clone(),
        discard: true,
    };
    // Kept before any stream opens, naming every track asked for; narrowed
    // to those that start. With no room, startup leaves no session behind.
    let asked = session_row(setup, session.id, &sources);
    keep_before_recording(&watch, &session, &asked, notes)?;
    let (lock, writer) = open_session(&session, &watch, clock)?;
    watch.start_recording();

    // Everything that can fail to start does so before any stream opens,
    // while there's no audio to lose.
    let rows = NewSessionRows::new(library.db().clone(), asked.clone());
    // SQLite writes the database itself, so the segment rows' commits are
    // watched for a full disk too.
    let store = WatchedStore::new(rows.clone(), Arc::clone(&watch), &library.db_path());
    let publisher = Publisher::spawn(SessionStore::new(lock.clone(), store), segment_length())?;
    // The live text, marks and notes are stored as they come, on a thread
    // of their own; whichever of it and the publisher writes first adds the
    // session's row.
    let saver = Saver::spawn(
        saving(rows.clone(), session.id, args.models.as_ref().map(heard_by)),
        ui.clone(),
        Arc::clone(clock),
    )?;
    let (live_inputs, live) = start_live(args, clock, &ui, &saver)?;
    // Usage is reckoned for every track asked for: a little early with a
    // low disk if one doesn't start.
    let disk = watch_disk(args, &session.audio(), sources.len(), &watch, &ui, clock)?;
    // The recorder runs before any stream opens: each track joins it as
    // its stream starts, while the next one opens.
    let (starter, mut events) = prepare_tracks(&sources, RATE, clock);
    // A pinned system device is a `Source::Device`, held to the
    // microphone's 5 s of zeros unless it's told it's the system audio.
    events.set_thresholds(SYSTEM, Thresholds::SYSTEM_AUDIO);
    let recorder = spawn_recorder(
        writer,
        events,
        publisher.queue(),
        live_inputs.clone(),
        ui.clone(),
    )?;
    let ready = Ready {
        recorder,
        publisher,
        saver,
        live_inputs,
        live,
        disk,
        lock,
    };
    // Before the first audio, so no stretch of the recording is unguarded.
    let sleep = inhibit::hold(logind);
    let (captures, listening, ready) =
        open_streams(ready, starter, backend, &sources, (&ui, &ui_events), notes)?;
    sleep.report(clock.as_ref(), &ui, notes);
    unmade.keep();
    keep_started(&rows, &watch, &session, &asked, &captures, notes);
    let Ready {
        recorder,
        publisher,
        saver,
        live_inputs,
        live,
        disk,
        lock,
    } = ready;

    let mut outcome = Outcome::new(session.id, session.dir.clone());
    outcome.notes = std::mem::take(notes);
    let started_save = saver.sender();
    let running = Started {
        outcome,
        signals,
        draws,
        library,
        session: session.id,
        lock,
        captures,
        sleep,
        disk,
        publisher,
        live_inputs,
        live,
        recorder,
        saver,
    };
    let screening = Screening {
        screen,
        ui,
        ui_events,
        listening,
        save: started_save,
    };
    Ok((running, screening))
}

/// Says on screen that earlier sessions are being checked, so a long
/// salvage doesn't look like a blank, stuck screen. Only a courtesy: if
/// drawing fails, the next screen says so.
fn show_recovering(screen: &mut Screen) {
    let _ = screen.clear();
    let _ = screen.terminal().draw(|frame| {
        let area = frame.area();
        let line = ratatui::widgets::Paragraph::new(RECOVERING)
            .alignment(ratatui::layout::Alignment::Center);
        let middle = ratatui::layout::Rect {
            y: area.y + area.height / 2,
            height: 1.min(area.height),
            ..area
        };
        frame.render_widget(line, middle);
    });
}

/// The line shown while earlier sessions are checked.
pub(super) const RECOVERING: &str = "Checking earlier recordings…";

/// Why the start gives up, if a stop was asked for since the signals were
/// first listened for: the full disk, when the disk monitor's warning came
/// with it, otherwise [`STOPPED_BEFORE`]. Whatever else is waiting (the disk
/// monitor's first report, say) is put back on `ui` for the screen, after
/// the others: it's read only once the screen runs.
fn stop_asked(ui: &Sender<Event>, ui_events: &Receiver<Event>) -> Option<&'static str> {
    // Taken first, so what is put back isn't read again.
    let waiting: Vec<Event> = ui_events.try_iter().collect();
    let disk_full = waiting.iter().any(|event| {
        matches!(
            event,
            Event::Recorder(recorder::Event::Warning(Warning {
                cause: Cause::DiskFull,
                state: WarningState::Raised,
                ..
            }))
        )
    });
    let mut asked = false;
    for event in waiting {
        if matches!(event, Event::Recorder(recorder::Event::Stopping)) {
            asked = true;
        } else {
            // The receiver is the caller's, still open.
            let _ = ui.send(event);
        }
    }
    asked.then_some(if disk_full {
        STOPPED_BY_FULL_DISK
    } else {
        STOPPED_BEFORE
    })
}

/// Opens the streams once everything else has started, unless a stop was
/// asked for meanwhile; with none started, or stopped first, stops what
/// did start ([`Ready::abandon`]) and fails.
fn open_streams<B: CaptureBackend>(
    ready: Ready,
    starter: TrackStarter,
    backend: &B,
    sources: &[(TrackId, Source)],
    (ui, ui_events): (&Sender<Event>, &Receiver<Event>),
    notes: &mut Vec<String>,
) -> Result<Opened<B::Stream>, BoxError> {
    // A stop asked for while all that started: still no audio.
    if let Some(why) = stop_asked(ui, ui_events) {
        drop(starter);
        ready.abandon();
        return Err(why.into());
    }
    let (captures, listening) = start_streams(starter, backend, sources, notes);
    if captures.is_empty() {
        ready.abandon();
        return Err("nothing to record".into());
    }
    Ok((captures, listening, ready))
}

/// Opens each source's stream through `starter`, one after another,
/// noting those that don't start. Returns the running captures and the
/// footer's names for their sources.
fn start_streams<B: CaptureBackend>(
    starter: TrackStarter,
    backend: &B,
    sources: &[(TrackId, Source)],
    notes: &mut Vec<String>,
) -> (Vec<Capture<B::Stream>>, String) {
    let mut captures = Vec::new();
    let mut listening = Vec::new();
    for (result, (_, source)) in starter.start(backend).into_iter().zip(sources) {
        match result {
            Ok(capture) => {
                captures.push(capture);
                listening.push(source_name(source));
            }
            Err(e) => notes.push(format!("not recording {source}: {e}")),
        }
    }
    (captures, listening.join(" + "))
}

/// Starts the engine, if there are models, and the live thread that
/// feeds it: the live thread's inputs, and the thread.
fn start_live(
    args: &RecordArgs,
    clock: &Arc<dyn Clock>,
    ui: &Sender<Event>,
    saver: &Saver,
) -> Result<(Sender<LiveInput>, JoinHandle<LiveEnd>), BoxError> {
    let (live_inputs, live_received) = mpsc::channel::<LiveInput>();
    let engine = match &args.models {
        Some((parakeet, vad)) => Some(start_engine(parakeet, vad, clock, &live_inputs)?),
        None => None,
    };
    let log = args.latency_log.clone().map(LatencyLog::new);
    // Each track's epochs reach the live thread from the recorder, the
    // first ones too.
    let live = spawn_live(
        Live::new(&[]),
        engine,
        live_received,
        ui.clone(),
        saver.sender(),
        log.map(|log| (log, Arc::clone(clock))),
    )?;
    Ok((live_inputs, live))
}

/// The streams that started, the footer's names for their sources, and
/// what started before them.
type Opened<S> = (Vec<Capture<S>>, String, Ready);

/// What a start that fails before any stream opens says.
const STOPPED_BEFORE: &str = "stopped before recording started; no session was made";

/// The same stop, when the disk monitor found the disk full first.
const STOPPED_BY_FULL_DISK: &str =
    "stopped before recording started because the disk is full; no session was made";

/// The session a start made, removed again ([`discard_empty`]) when this
/// is dropped, unless it's kept: a start that fails before any audio
/// leaves no session behind.
struct Unmade {
    fs: RecordFs,
    session: SessionPaths,
    /// Cleared once the start has succeeded.
    discard: bool,
}

impl Unmade {
    /// The start succeeded: the session stays.
    fn keep(mut self) {
        self.discard = false;
    }
}

impl Drop for Unmade {
    fn drop(&mut self) {
        if self.discard {
            discard_empty(&self.fs, &self.session);
        }
    }
}

/// Everything started before the streams.
struct Ready {
    recorder: JoinHandle<Recorded>,
    publisher: Publisher,
    saver: Saver,
    live_inputs: Sender<LiveInput>,
    live: JoinHandle<LiveEnd>,
    disk: DiskMonitor<StdFs>,
    lock: SessionLock<RecordFs>,
}

impl Ready {
    /// Stops it all, in order, when no stream started: the recorder, which
    /// has heard that none did (or that none will), returns with nothing
    /// recorded, the publisher has nothing to publish, and the saver,
    /// the live thread (with the engine) and the disk monitor stop. The
    /// session is then removed, as [`Unmade`] does.
    fn abandon(self) {
        // A recorder that panicked leaves its journals, if any, for
        // salvage; `discard_empty` keeps a session with audio in it.
        drop(self.recorder.join());
        let _ = self.publisher.finish();
        let _ = self.live_inputs.send(LiveInput::Done);
        drop(self.live_inputs);
        drop(self.live.join());
        drop(self.saver.finish());
        drop(self.disk.stop());
        drop(self.lock);
    }
}

/// Narrows the session's row, `asked`, to the tracks of `captures`, those
/// that started, if some didn't: in the database's row, and in the row
/// kept with the audio. A failure is a note: the row names a track with
/// no audio.
fn keep_started<S>(
    rows: &NewSessionRows,
    watch: &Arc<DiskWatch<StdFs>>,
    session: &SessionPaths,
    asked: &NewSession,
    captures: &[Capture<S>],
    notes: &mut Vec<String>,
) {
    let mut row = asked.clone();
    row.tracks
        .retain(|t| captures.iter().any(|c| c.track() == t.track));
    if row.tracks.len() == asked.tracks.len() {
        return;
    }
    if !rows.set_tracks(&row.tracks) {
        notes.push(
            "the library database already lists every track asked for, \
             though not all of them recorded"
                .to_owned(),
        );
    }
    if let Err(error) = session.keep_on(&watch.fs(), &row) {
        notes.push(format!(
            "the session's tracks weren't kept with its audio: {error}"
        ));
    }
}

/// A failed start, with the recovery notes it collected first.
#[derive(Debug)]
struct StartupFailure {
    /// What stopped the new recording from starting.
    error: BoxError,
    /// What recovery found in earlier sessions.
    notes: Vec<String>,
}

impl std::fmt::Display for StartupFailure {
    /// Shows the startup failure followed by the earlier recovery notes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; {}", self.error, self.notes.join("; "))
    }
}

impl std::error::Error for StartupFailure {
    /// Keeps the original startup error available to callers.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Leaves the original cause intact when recovery added nothing.
fn startup_failure(error: BoxError, notes: Vec<String>) -> BoxError {
    if notes.is_empty() {
        error
    } else {
        Box::new(StartupFailure { error, notes })
    }
}

/// Recovers earlier sessions, then watches the disk for this start.
fn recover(
    library: &Library,
    args: &RecordArgs,
    notes: &mut Vec<String>,
) -> Result<Arc<DiskWatch<StdFs>>, BoxError> {
    let ballast_len = ballast_length(args);
    let recovery = startup_watch(&StdFs, &args.data, ballast_len)?;
    for salvaged in library.salvage_watched(segment_length(), &recovery)? {
        notes.push(salvage_note(&salvaged));
    }
    // Recovery has its own watch: a full disk it got past mustn't stop
    // the new recording. Hold whatever ballast is still there.
    Ok(startup_watch(&StdFs, &args.data, ballast_len)?)
}

/// A space failure rejects this start; other title failures remain notes.
fn keep_before_recording<S: Fs + Clone + 'static>(
    watch: &Arc<DiskWatch<S>>,
    session: &SessionPaths,
    row: &NewSession,
    notes: &mut Vec<String>,
) -> Result<(), BoxError> {
    if let Err(error) = keep_session(watch, session, row) {
        if is_disk_full(&error) {
            return Err(start_error(error));
        }
        notes.push(format!(
            "the session's title and tracks weren't kept with its audio: {error}"
        ));
    }
    Ok(())
}

/// Keeps the new row before recording starts, with one full-disk retry.
fn keep_session<S: Fs + Clone + 'static>(
    watch: &Arc<DiskWatch<S>>,
    session: &SessionPaths,
    row: &NewSession,
) -> std::io::Result<()> {
    let write = || session.keep_on(&watch.fs(), row);
    let kept = retry_start(write(), watch, write);
    if let Err(error) = kept {
        if is_disk_full(&error) {
            discard_empty(&watch.fs(), session);
        }
        return Err(error);
    }
    Ok(())
}

/// Makes the session through the watch, retrying after the ballast freed.
fn create_session<S: Fs + Clone + 'static>(
    library: &Library,
    watch: &Arc<DiskWatch<S>>,
) -> Result<SessionPaths, BoxError> {
    let first = library.create_on(&watch.fs());
    retry_start(first, watch, || library.create_on(&watch.fs())).map_err(start_error)
}

/// Retries a startup operation once if freeing the ballast made room.
fn retry_start<S: Fs + Clone, T>(
    first: std::io::Result<T>,
    watch: &DiskWatch<S>,
    again: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    if first.as_ref().is_err_and(is_disk_full)
        && watch
            .full()
            .is_some_and(|f| f.ballast == nota_recorder::disk::Freed::Freed)
    {
        again()
    } else {
        first
    }
}

/// Says plainly when starting failed for want of space.
fn start_error(error: std::io::Error) -> BoxError {
    if is_disk_full(&error) {
        "the disk is full: free some space".into()
    } else {
        error.into()
    }
}

/// Opens the new writer before any track can record audio.
fn open_session(
    session: &SessionPaths,
    watch: &Arc<DiskWatch<StdFs>>,
    clock: &Arc<dyn Clock>,
) -> Result<(SessionLock<RecordFs>, SessionWriter<RecordFs>), BoxError> {
    let lock = SessionDir::new(session.id, watch.fs(), &session.audio())
        .lock()
        .map_err(start_error)?;
    let writer = SessionWriter::open(&lock, RATE, segment_length(), Arc::clone(clock))?;
    // Inline for one track, the path the crash tests sweep; a thread per
    // track once a second starts (see `nota_recorder::capture`).
    Ok((lock, writer.with_syncing(Syncing::Auto)))
}

/// Tests keep a smaller ballast for their short recordings.
fn ballast_length(args: &RecordArgs) -> u64 {
    if args.tone {
        TONE_BALLAST_LEN
    } else {
        BALLAST_LEN
    }
}

/// Starts the disk monitor for a recording of `tracks` tracks into
/// `audio`, with the ballast in the data directory.
fn watch_disk(
    args: &RecordArgs,
    audio: &std::path::Path,
    tracks: usize,
    watch: &Arc<DiskWatch<StdFs>>,
    ui: &Sender<Event>,
    clock: &Arc<dyn Clock>,
) -> std::io::Result<DiskMonitor<StdFs>> {
    let config = MonitorConfig {
        data_dir: args.data.clone(),
        audio_dir: audio.to_path_buf(),
        usage: Usage::new(tracks, RATE, segment_length()),
        ballast_len: ballast_length(args),
        interval: CHECK_INTERVAL,
    };
    let reports = disk_reports(ui.clone(), Arc::clone(clock));
    DiskMonitor::spawn(Arc::clone(watch), config, reports)
}

/// Turns what the disk monitor reports into the screens' events: the
/// space, the low-disk warning, and on a full disk its warning and the
/// stop.
fn disk_reports(ui: Sender<Event>, clock: Arc<dyn Clock>) -> impl FnMut(DiskReport) + Send {
    move |report| {
        let warning = |cause, state| {
            recorder::Event::Warning(Warning {
                cause,
                track: None,
                at: clock.now(),
                state,
            })
        };
        let events = match report {
            DiskReport::Space(disk) => vec![recorder::Event::Disk(disk)],
            DiskReport::Low(state) => vec![warning(Cause::DiskLow, state)],
            DiskReport::Full(_) => vec![
                warning(Cause::DiskFull, WarningState::Raised),
                recorder::Event::Stopping,
            ],
            // In the summary.
            DiskReport::Unchecked(_) | DiskReport::Ballast(_) => Vec::new(),
        };
        for event in events {
            // The screen may have closed already.
            let _ = ui.send(Event::Recorder(event));
        }
    }
}

/// The recorder thread: records every track into `writer` until every
/// stream has ended (or recording fails), handing finished journals to the
/// publisher's `queue` and everything else to the live thread, and never
/// waiting on either. Then it closes the screen.
fn spawn_recorder(
    mut writer: SessionWriter<RecordFs>,
    events: CaptureReceiver,
    queue: PublishQueue,
    live_inputs: Sender<LiveInput>,
    ui: Sender<Event>,
) -> std::io::Result<JoinHandle<Recorded>> {
    thread::Builder::new()
        .name("nota-recorder".into())
        .spawn(move || -> Recorded {
            let mut failures = 0;
            let mut lost = Vec::new();
            // Every track joins as its stream starts.
            let result = record_tracks(&mut writer, &mut [], &events, &mut |track, e| {
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
            // Every stream has ended, or recording failed: the screen has
            // nothing more to show, so close it and stop.
            let _ = ui.send(Event::Recorder(recorder::Event::Stopping));
            (writer, result, failures, lost)
        })
}

/// What the summary says of one salvaged session.
fn salvage_note(salvaged: &Salvaged) -> String {
    match salvaged {
        Salvaged::Done(id, aside) => {
            format!("salvaged session {}{}", id.get(), set_aside_part(aside))
        }
        Salvaged::Left(id, aside, held) => {
            let notes = held_notes(held);
            let why = if notes.is_empty() {
                "some of its journals are still to publish".to_owned()
            } else {
                notes.join("; ")
            };
            format!(
                "salvaged session {}: {why}{}",
                id.get(),
                set_aside_part(aside)
            )
        }
        Salvaged::InUse(id) => format!("session {} is in use; not salvaged", id.get()),
        Salvaged::Failed(id, e) => format!("salvaging session {} failed: {e}", id.get()),
    }
}

/// The end of a salvage note naming the journals set aside as damaged, or
/// nothing if there were none.
fn set_aside_part(aside: &[PathBuf]) -> String {
    match aside {
        [] => String::new(),
        [_] => format!(
            "; a journal was damaged, so part of it wasn't published: {} is kept",
            file_names(aside)
        ),
        _ => format!(
            "; journals were damaged, so parts of it weren't published: {} are kept",
            file_names(aside)
        ),
    }
}

/// This session's row: its title and every track asked for.
fn session_row(setup: &Setup, id: SessionId, sources: &[(TrackId, Source)]) -> NewSession {
    NewSession {
        id,
        title: Some(setup.title.clone()),
        language: None,
        // When it started, for its date; read once, here.
        started_at: wall_now(),
        tracks: sources
            .iter()
            .map(|(track, source)| Track {
                track: *track,
                kind: track_kind(*track),
                source: Some(source_name(source)),
            })
            .collect(),
    }
}

/// The engine that hears the live text, and its model: the Parakeet
/// directory's name.
pub(super) fn heard_by((parakeet, _): &(PathBuf, PathBuf)) -> (String, String) {
    let model = parakeet.file_name().map_or_else(
        || "parakeet".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    ("sherpa-onnx".to_owned(), model)
}

/// How the saver stores each item: in session `session`, adding its row
/// first if nothing has yet. Text is stored with its words, as heard by
/// `heard_by`'s engine and model.
fn saving(
    rows: NewSessionRows,
    session: SessionId,
    heard_by: Option<(String, String)>,
) -> impl FnMut(&ToSave) -> Result<(), nota_store::StoreError> + Send + 'static {
    move |item| {
        rows.added(session)?;
        rows.db().with(|db| match item {
            ToSave::Heard(utterance, words) => {
                // Text comes only from an engine, which has models.
                let (engine, model) = heard_by
                    .clone()
                    .unwrap_or_else(|| ("unknown".to_owned(), "unknown".to_owned()));
                let heard = Heard {
                    utterance: utterance.clone(),
                    engine,
                    model,
                    words: words.clone(),
                };
                db.add_utterance(session, &heard).map(drop)
            }
            ToSave::Annotation(annotation) => db.add_annotation(session, annotation),
        })
    }
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

/// What `input` captures: the device it names, or `default`, the source
/// that follows the system's default.
pub(crate) fn source_of(input: &Input, default: Source) -> Source {
    match input {
        Input::Default => default,
        Input::Device(name) => Source::Device(name.clone()),
    }
}

/// Each track `setup` asks for, with its source as `setup` chose it: the
/// microphone's, then the system audio's. A track the setup leaves out is
/// not recorded.
fn sources(setup: &Setup) -> Vec<(TrackId, Source)> {
    [
        (MIC, setup.mic.as_ref(), Source::Microphone),
        (SYSTEM, setup.system.as_ref(), Source::SystemAudio),
    ]
    .into_iter()
    .filter_map(|(track, input, default)| Some((track, source_of(input?, default))))
    .collect()
}

/// The setup the last session recorded with: its title, and each track's
/// source, as [`source_name`] named it in the library. A track it didn't
/// record (a stream that didn't start) follows the default, and both
/// tracks are asked for: the rows can't say whether one was left out on
/// purpose (Setup's own choice is kept apart, see `app`). `None` if
/// there's no last session, or the library database can't be read.
pub(crate) fn last_setup(library: &Library) -> Option<Setup> {
    let (session, tracks) = library
        .db()
        .with(|db| {
            let Some(last) = db.sessions()?.pop() else {
                return Ok(None);
            };
            let tracks = db.tracks(last.id)?;
            Ok(Some((last, tracks)))
        })
        .ok()??;
    let input = |kind: TrackKind, default: &str| {
        let source = tracks
            .iter()
            .find(|t| t.kind == kind)
            .and_then(|t| t.source.as_deref());
        match source {
            Some(name) if name != default => Input::Device(name.to_owned()),
            _ => Input::Default,
        }
    };
    Some(Setup {
        title: session.title.unwrap_or_else(|| "Recording".to_owned()),
        mic: Some(input(
            TrackKind::Microphone,
            &source_name(&Source::Microphone),
        )),
        system: Some(input(TrackKind::System, &source_name(&Source::SystemAudio))),
    })
}

/// What a track records.
const fn track_kind(track: TrackId) -> TrackKind {
    if track.get() == SYSTEM.get() {
        TrackKind::System
    } else {
        TrackKind::Microphone
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
    use super::*;

    /// A warning, as the disk monitor sends its first one.
    fn low_disk() -> Event {
        Event::Recorder(recorder::Event::Warning(Warning {
            cause: Cause::DiskLow,
            track: None,
            at: nota_core::SessionTime::ZERO,
            state: WarningState::Raised,
        }))
    }

    /// With no stop asked, whatever else is waiting is still there for the
    /// screen afterwards, in order: the disk monitor's first report would
    /// otherwise be lost.
    #[test]
    fn looking_for_a_stop_leaves_the_other_events_for_the_screen() {
        let (ui, waiting) = mpsc::channel::<Event>();
        let disk = Event::Recorder(recorder::Event::Disk(recorder::Disk {
            free_bytes: 7,
            left: None,
        }));
        ui.send(disk.clone()).unwrap();
        ui.send(low_disk()).unwrap();
        assert_eq!(stop_asked(&ui, &waiting), None);
        assert_eq!(waiting.try_iter().collect::<Vec<_>>(), [disk, low_disk()]);
    }

    /// A stop is found among the other events, taken off the channel, and
    /// the others stay.
    #[test]
    fn a_stop_is_found_among_the_other_events_and_they_stay() {
        let (ui, waiting) = mpsc::channel::<Event>();
        ui.send(low_disk()).unwrap();
        ui.send(Event::Recorder(recorder::Event::Stopping)).unwrap();
        assert_eq!(stop_asked(&ui, &waiting), Some(STOPPED_BEFORE));
        assert_eq!(waiting.try_iter().collect::<Vec<_>>(), [low_disk()]);
        // Asked once: nothing is left to ask again.
        assert_eq!(stop_asked(&ui, &waiting), None);
    }

    /// The disk monitor's full-disk warning, then its stop, as `disk_reports`
    /// sends them: the start says the disk is full, and the warning stays for
    /// the screen.
    #[test]
    fn a_stop_that_came_with_a_full_disk_says_the_disk_is_full() {
        let (ui, waiting) = mpsc::channel::<Event>();
        let full = Event::Recorder(recorder::Event::Warning(Warning {
            cause: Cause::DiskFull,
            track: None,
            at: nota_core::SessionTime::ZERO,
            state: WarningState::Raised,
        }));
        ui.send(full.clone()).unwrap();
        ui.send(Event::Recorder(recorder::Event::Stopping)).unwrap();
        let why = stop_asked(&ui, &waiting).unwrap();
        assert_eq!(why, STOPPED_BY_FULL_DISK);
        assert!(why.contains("disk is full"));
        assert_eq!(waiting.try_iter().collect::<Vec<_>>(), [full]);
    }

    /// A track the setup leaves out isn't asked for, and a setup that
    /// leaves out both asks for none, which `start` refuses before it makes
    /// a session.
    #[test]
    fn a_track_left_out_of_the_setup_is_not_recorded() {
        let mut setup = Setup {
            title: "Workshop".to_owned(),
            mic: Some(Input::Default),
            system: None,
        };
        assert_eq!(sources(&setup), [(MIC, Source::Microphone)]);
        setup.mic = None;
        setup.system = Some(Input::Device("speakers.monitor".to_owned()));
        assert_eq!(
            sources(&setup),
            [(SYSTEM, Source::Device("speakers.monitor".to_owned()))]
        );
        setup.system = None;
        assert_eq!(sources(&setup), []);
    }

    /// The setup a start command carries decides what each track records.
    #[test]
    fn the_setup_chooses_each_track_s_source() {
        let mut setup = Setup {
            title: "Workshop".to_owned(),
            mic: Some(Input::Default),
            system: Some(Input::Default),
        };
        assert_eq!(
            sources(&setup),
            [(MIC, Source::Microphone), (SYSTEM, Source::SystemAudio)]
        );
        setup.mic = Some(Input::Device("usb-mic".to_owned()));
        setup.system = Some(Input::Device("speakers.monitor".to_owned()));
        assert_eq!(
            sources(&setup),
            [
                (MIC, Source::Device("usb-mic".to_owned())),
                (SYSTEM, Source::Device("speakers.monitor".to_owned()))
            ]
        );
    }

    /// The saver's writes add the session's row if nothing has yet, then
    /// store text as heard by the engine and model the models name, and
    /// marks and notes, in the session.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn the_saver_stores_text_and_annotations_in_the_session() {
        use nota_core::recorder::Mark;
        use nota_core::{SessionTime, Utterance};
        use nota_store::{Annotation, RevisionNumber};

        let root = std::env::temp_dir().join(format!("nota-saving-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let library = Library::open(&root).unwrap();
        let id = SessionId::new(3);
        let rows = NewSessionRows::new(
            library.db().clone(),
            NewSession {
                id,
                title: Some("Workshop".to_owned()),
                language: None,
                started_at: None,
                tracks: Vec::new(),
            },
        );
        let models = (
            PathBuf::from("/models/parakeet-tdt-0.6b-v3-int8"),
            PathBuf::from("/models/silero_vad.onnx"),
        );
        let mut write = saving(rows, id, Some(heard_by(&models)));
        let at = SessionTime::from_nanos(2_000_000_000);
        let heard = Utterance::new(SYSTEM, at, at, "welcome".to_owned()).unwrap();
        write(&ToSave::Heard(heard.clone(), Vec::new())).unwrap();
        write(&ToSave::Annotation(Annotation::Mark(Mark { at }))).unwrap();

        let stored = library
            .db()
            .with(|db| {
                Ok((
                    db.session(id)?,
                    db.utterances(id)?,
                    db.annotations(id)?,
                    db.revision(id, RevisionNumber::HEARD)?,
                ))
            })
            .unwrap();
        let (session, utterances, annotations, shown) = stored;
        assert_eq!(session.unwrap().title.as_deref(), Some("Workshop"));
        assert_eq!(utterances.len(), 1);
        assert_eq!(utterances[0].heard.utterance, heard);
        assert_eq!(utterances[0].heard.engine, "sherpa-onnx");
        assert_eq!(utterances[0].heard.model, "parakeet-tdt-0.6b-v3-int8");
        assert!(utterances[0].heard.words.is_empty());
        assert_eq!(annotations, [Annotation::Mark(Mark { at })]);
        assert_eq!(shown[0].text, "welcome");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A live utterance, placed by the live thread through its track's
    /// epoch and stored by the saver's writes, reads back from the store
    /// with its words, each in session time within the utterance's span.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn a_live_utterance_is_stored_with_its_words_in_session_time() {
        use nota_core::messages::{HeardWord, Transcript};
        use nota_core::{SampleIndex, SampleRange, SessionTime};
        use nota_recorder::engine::EngineEvent;

        let root = std::env::temp_dir().join(format!("nota-saving-words-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let library = Library::open(&root).unwrap();
        let id = SessionId::new(4);
        let rows = NewSessionRows::new(
            library.db().clone(),
            NewSession {
                id,
                title: None,
                language: None,
                started_at: None,
                tracks: Vec::new(),
            },
        );
        let models = (PathBuf::from("/models/parakeet"), PathBuf::from("/vad"));
        let mut write = saving(rows, id, Some(heard_by(&models)));

        // The system audio opened 2 s into the session, at 16 kHz.
        let s = SessionTime::from_nanos;
        let mut timeline = nota_core::TrackTimeline::new(SYSTEM);
        timeline
            .open_epoch(s(2_000_000_000), SampleIndex::ZERO, RATE)
            .unwrap();
        let live = Live::new(&[timeline]);
        let range = |a, b| SampleRange::new(SampleIndex::new(a), SampleIndex::new(b)).unwrap();
        let word = |text: &str, a, b| HeardWord::new(text.to_owned(), range(a, b)).unwrap();
        let transcript = Transcript::new(SYSTEM, range(8_000, 40_000), "hello there".into())
            .unwrap()
            .with_words(vec![
                word("hello", 16_000, 24_000),
                word("there", 24_000, 32_000),
            ])
            .unwrap();
        let actions = live.engine(EngineEvent::Transcript(transcript));
        let (utterance, words) = actions.heard.unwrap();
        write(&ToSave::Heard(utterance, words)).unwrap();

        let stored = library.db().with(|db| db.utterances(id)).unwrap();
        assert_eq!(stored.len(), 1);
        let heard = &stored[0].heard;
        let u = &heard.utterance;
        assert_eq!((u.start(), u.end()), (s(2_500_000_000), s(4_500_000_000)));
        let words: Vec<_> = heard
            .words
            .iter()
            .map(|w| (w.text(), w.start(), w.end()))
            .collect();
        assert_eq!(
            words,
            [
                ("hello", s(3_000_000_000), s(3_500_000_000)),
                ("there", s(3_500_000_000), s(4_000_000_000)),
            ]
        );
        for w in &heard.words {
            assert!(u.start() <= w.start() && w.end() <= u.end(), "{w:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Where [`saving_crash_child`] works, when it's run as the child.
    const CRASH_DIR: &str = "NOTA_SAVING_CRASH_DIR";

    /// What the crash child stores.
    fn crash_heard() -> Option<nota_core::Utterance> {
        let at = nota_core::SessionTime::from_nanos(3_000_000_000);
        nota_core::Utterance::new(MIC, at, at, "said before the kill".to_owned())
    }

    /// The child of [`text_stored_before_a_kill_is_there_after_salvage`]:
    /// starts a session, stores text and a mark through the saver's
    /// writes, says so, and waits to be killed. Does nothing unless run
    /// as that child.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn saving_crash_child() {
        use nota_core::recorder::Mark;
        use nota_store::Annotation;

        let Some(root) = std::env::var_os(CRASH_DIR).map(PathBuf::from) else {
            return;
        };
        let library = Library::open(&root).unwrap();
        let session = library.create().unwrap();
        let rows = NewSessionRows::new(
            library.db().clone(),
            NewSession {
                id: session.id,
                title: Some("Workshop".to_owned()),
                language: None,
                started_at: None,
                tracks: Vec::new(),
            },
        );
        let models = (PathBuf::from("/m/parakeet"), PathBuf::from("/m/vad"));
        let mut write = saving(rows, session.id, Some(heard_by(&models)));
        write(&ToSave::Heard(crash_heard().unwrap(), Vec::new())).unwrap();
        let at = crash_heard().unwrap().start();
        write(&ToSave::Annotation(Annotation::Mark(Mark { at }))).unwrap();
        std::fs::write(root.join("stored"), b"").unwrap();
        let (_keep, never) = mpsc::channel::<()>();
        let _ = never.recv_timeout(std::time::Duration::from_secs(60));
    }

    /// Acceptance (GAI-310): text the saver committed before nota was
    /// killed outright is there after the next start's salvage. (Salvage
    /// leaves this session's state alone: it has no audio yet, as a session
    /// another nota has just made.)
    #[test]
    #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
    fn text_stored_before_a_kill_is_there_after_salvage() {
        use nota_core::recorder::Mark;
        use nota_store::Annotation;

        let root = std::env::temp_dir().join(format!("nota-saving-kill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "record::start::tests::saving_crash_child",
                "--nocapture",
            ])
            .env(CRASH_DIR, &root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stored = root.join("stored");
        let (_keep, never) = mpsc::channel::<()>();
        for _ in 0..1_000 {
            if stored.exists() {
                break;
            }
            let _ = never.recv_timeout(std::time::Duration::from_millis(20));
        }
        assert!(stored.exists(), "the child never stored its text");
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "{status:?}");

        let library = Library::open(&root).unwrap();
        library.salvage_all(segment_length()).unwrap();
        let id = SessionId::new(1);
        let (session, utterances, annotations) = library
            .db()
            .with(|db| Ok((db.session(id)?, db.utterances(id)?, db.annotations(id)?)))
            .unwrap();
        assert_eq!(session.unwrap().title.as_deref(), Some("Workshop"));
        assert_eq!(utterances.len(), 1);
        assert_eq!(utterances[0].heard.utterance, crash_heard().unwrap());
        assert_eq!(
            annotations,
            [Annotation::Mark(Mark {
                at: crash_heard().unwrap().start()
            })]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn each_track_is_named_for_what_it_records() {
        assert_eq!(track_kind(MIC), TrackKind::Microphone);
        assert_eq!(track_kind(SYSTEM), TrackKind::System);
    }
}

#[cfg(test)]
mod salvage_note_tests {
    use nota_core::SessionId;

    use super::*;

    fn aside(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from("/data/sessions/3/audio").join(n))
            .collect()
    }

    #[test]
    fn a_salvaged_session_with_a_journal_set_aside_names_it() {
        let done = Salvaged::Done(SessionId::new(3), aside(&["journal-000000.unreadable"]));
        assert_eq!(
            salvage_note(&done),
            "salvaged session 3; a journal was damaged, so part of it wasn't published: \
             journal-000000.unreadable is kept"
        );
        let clean = Salvaged::Done(SessionId::new(3), Vec::new());
        assert_eq!(salvage_note(&clean), "salvaged session 3");
    }

    #[test]
    fn a_session_with_journals_left_and_set_aside_says_both() {
        let left = Salvaged::Left(
            SessionId::new(3),
            aside(&["journal-000000.unreadable", "journal-000004.unreadable"]),
            Box::default(),
        );
        assert_eq!(
            salvage_note(&left),
            "salvaged session 3: some of its journals are still to publish; \
             journals were damaged, so parts of it weren't published: \
             journal-000000.unreadable, journal-000004.unreadable are kept"
        );
    }
    /// Removes the fixture's files through the filesystem boundary.
    fn remove_fixture(path: &std::path::Path) {
        if let Ok(children) = StdFs.list(path) {
            for child in children {
                remove_fixture(&child);
            }
            let _ = StdFs.remove_dir(path);
        } else {
            let _ = StdFs.remove(path);
        }
    }

    /// Startup names the real directory that prevents a segment being published.
    #[test]
    fn startup_names_a_directory_blocking_salvage() {
        use nota_core::{FakeClock, SessionTime};
        let root = std::env::temp_dir().join(format!("nota-start-held-{}", std::process::id()));
        remove_fixture(&root);
        let library = Library::open(&root).unwrap();
        let session = library.create().unwrap();
        let lock = SessionDir::new(session.id, StdFs, &session.audio())
            .lock()
            .unwrap();
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
        let mut writer = SessionWriter::open(&lock, RATE, segment_length(), clock).unwrap();
        let (_, epoch) = writer.open_first_epoch(MIC, SessionTime::ZERO).unwrap();
        writer.start_track(MIC, &epoch).unwrap();
        writer.append(MIC, &[7; 50]).unwrap();
        writer.finish().unwrap();
        // Release the recording lock so startup can salvage, rather than report in use.
        drop(lock);
        StdFs
            .create_dir(&session.audio().join("seg-t0-000000000000.flac"))
            .unwrap();
        let salvaged = library.salvage_all(segment_length()).unwrap();
        assert_eq!(salvaged.len(), 1);
        assert_eq!(
            salvage_note(&salvaged[0]),
            "salvaged session 1: seg-t0-000000000000.flac: a directory is in the way; its audio is still to publish"
        );
        // A blocked publish must preserve its journal for a later retry.
        assert!(StdFs.list(&session.audio()).unwrap().iter().any(|path| {
            path.file_name()
                .is_some_and(|name| name == "journal-000000")
        }));
        drop(library);
        remove_fixture(&root);
    }
    /// Startup names pending repair even when another journal only needs cleanup.
    #[test]
    fn startup_reports_cleanup_and_unfinished_repair_together() {
        use nota_recorder::segment::{FakeStore, salvage};
        let fixture = crate::record::summary::tests::mixed_fixture();
        let mut bound = SessionStore::new(
            fixture.lock.clone(),
            FakeStore::new(&fixture.fs, &PathBuf::from("/held-db")),
        );
        // Startup creates its own report from the two journals retained on disk.
        let held = salvage(&mut bound, fixture.length).unwrap();
        assert_eq!(held.not_deleted().len(), 1);
        assert_eq!(held.not_repaired().len(), 1);
        let note = salvage_note(&Salvaged::Left(
            SessionId::new(1),
            Vec::new(),
            Box::new(held),
        ));
        assert!(note.contains("all its audio is published"), "{note}");
        assert!(note.contains("seg-t1-000000000000.flac: the segment file doesn't match its stored audio; couldn't repair it (permission denied); its journals are still to publish"), "{note}");
    }
}

#[cfg(test)]
mod space_tests {
    use super::*;
    use nota_recorder::disk::{Ballast, Freed};
    use nota_recorder::fs::fake::FakeFs;
    use nota_recorder::fs::sweep::Sweep;

    /// The directories a new start owns before it keeps the session row.
    fn pending_session(fs: &FakeFs) -> SessionPaths {
        let session = SessionPaths {
            id: SessionId::new(1),
            dir: PathBuf::from("/data/sessions/1"),
        };
        fs.create_dir(&session.dir).unwrap();
        fs.create_dir(&session.audio()).unwrap();
        session
    }

    /// A full disk without ballast refuses startup and leaves no session.
    #[test]
    fn a_full_start_without_ballast_leaves_nothing() {
        let fs = FakeFs::with_dirs([PathBuf::from("/data"), PathBuf::from("/data/sessions")]);
        let session = pending_session(&fs);
        fs.set_capacity(Some(0));
        let watch = startup_watch(&fs, std::path::Path::new("/data"), 1_024).unwrap();
        // Keeping the row is the last startup write before capture runs.
        let error = keep_session(&watch, &session, &NewSession::bare(session.id)).unwrap_err();
        assert_eq!(
            start_error(error).to_string(),
            "the disk is full: free some space"
        );
        assert!(
            fs.list(std::path::Path::new("/data/sessions"))
                .unwrap()
                .is_empty()
        );
        // The removal is synced, so a crash doesn't bring the empty start back.
        for outcome in nota_recorder::fs::fake::CrashOutcome::standard() {
            let crashed = fs.crash(outcome);
            assert!(
                crashed
                    .list(std::path::Path::new("/data/sessions"))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    /// A title renamed before a full-disk sync failure still belongs to
    /// the rejected start, so no metadata-only session is left behind.
    #[test]
    fn a_full_start_after_the_title_rename_leaves_nothing() {
        let fs = FakeFs::with_dirs([PathBuf::from("/data"), PathBuf::from("/data/sessions")]);
        let session = pending_session(&fs);
        let watch = startup_watch(&fs, std::path::Path::new("/data"), 1_024).unwrap();
        // Removal, create, write, file sync and rename precede the last sync.
        fs.fail_after(5, std::io::ErrorKind::StorageFull);
        let error = keep_session(&watch, &session, &NewSession::bare(session.id)).unwrap_err();
        assert_eq!(
            start_error(error).to_string(),
            "the disk is full: free some space"
        );
        assert!(
            fs.list(std::path::Path::new("/data/sessions"))
                .unwrap()
                .is_empty()
        );
        // Both the title and the session name stay removed after a crash.
        for outcome in nota_recorder::fs::fake::CrashOutcome::standard() {
            let crashed = fs.crash(outcome);
            assert!(
                crashed
                    .list(std::path::Path::new("/data/sessions"))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    /// Startup retries a partial row write after freeing the ballast.
    #[test]
    fn a_full_start_frees_ballast_and_keeps_its_row() {
        let root = PathBuf::from("/data");
        let fs = FakeFs::with_dirs([root.clone(), root.join("sessions")]);
        Ballast::keep(&fs, &root, 1_024, || false).unwrap();
        let session = pending_session(&fs);
        // No byte fits until the watch unlinks the existing ballast.
        fs.set_capacity(Some(1_024));
        let watch = startup_watch(&fs, &root, 1_024).unwrap();
        keep_session(&watch, &session, &NewSession::bare(session.id)).unwrap();
        assert_eq!(watch.full().unwrap().ballast, Freed::Freed);
        assert!(
            fs.read(&session.dir.join("session.txt"))
                .unwrap()
                .starts_with(b"nota")
        );
        // A recovered startup must not stop the new recording's monitor.
        watch.start_recording();
        assert!(watch.full().is_none());
        assert!(!watch.holds_ballast());
    }

    /// A library on a real directory of its own, which numbers new
    /// sessions; the sessions themselves are made on a fake filesystem at
    /// the same paths. Removed when dropped.
    struct Numbering {
        library: Library,
        root: PathBuf,
    }

    impl Numbering {
        /// The library under a fresh directory named for `name`.
        /// Session 1 is on it, as on the fake disk, so a start numbers its
        /// session 2 straight away, as it would on a real one.
        #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("nota-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let library = Library::open(&root).unwrap();
            std::fs::create_dir_all(root.join("sessions/1/audio")).unwrap();
            Self { library, root }
        }
    }

    impl Drop for Numbering {
        #[expect(clippy::disallowed_methods, reason = "test scaffolding")]
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A fake disk under `root` holding an earlier session, number 1: a
    /// synced journal and its kept title. Its file names and bytes, too.
    fn with_an_earlier_session(root: &std::path::Path) -> (FakeFs, Vec<(PathBuf, Vec<u8>)>) {
        use nota_recorder::fs::FsFile;
        let earlier = SessionPaths {
            id: SessionId::new(1),
            dir: root.join("sessions/1"),
        };
        let fs = FakeFs::with_dirs([earlier.audio()]);
        let mut journal = fs.create(&earlier.audio().join("journal-000000")).unwrap();
        journal.write_all(&[7; 40]).unwrap();
        journal.sync().unwrap();
        drop(journal);
        fs.sync_dir(&earlier.audio()).unwrap();
        let row = NewSession {
            title: Some("Earlier".to_owned()),
            ..NewSession::bare(earlier.id)
        };
        earlier.keep_on(&fs, &row).unwrap();
        let files = files(&fs);
        (fs, files)
    }

    /// Every file on `fs`, with its bytes.
    fn files(fs: &FakeFs) -> Vec<(PathBuf, Vec<u8>)> {
        fs.paths()
            .into_iter()
            .map(|path| {
                let bytes = fs.read(&path).unwrap();
                (path, bytes)
            })
            .collect()
    }

    /// Makes a new session and keeps its row on `fs`, as a start does
    /// before any stream opens, through a startup watch over `fs`.
    fn start_session(numbering: &Numbering, fs: &FakeFs) -> Result<SessionPaths, BoxError> {
        let watch = startup_watch(fs, &numbering.root, 1_024)?;
        let session = create_session(&numbering.library, &watch)?;
        let row = NewSession {
            title: Some("New".to_owned()),
            ..NewSession::bare(session.id)
        };
        let mut notes = Vec::new();
        keep_before_recording(&watch, &session, &row, &mut notes)?;
        // Only a full disk rejects a start; nothing here is anything else.
        assert_eq!(notes, Vec::<String>::new());
        Ok(session)
    }

    /// How many operations a start makes on a copy of `fs`, failed ones
    /// (a session number already taken) included: each in turn gets a
    /// fault.
    fn start_operations(numbering: &Numbering, fs: &FakeFs) -> usize {
        let probe = fs.copy_disk();
        let made = start_session(numbering, &probe).unwrap();
        assert_eq!(made.id, SessionId::new(2));
        probe.attempted()
    }

    /// A full disk at each operation of a new start, every write of its
    /// session and the sync after its title's rename included, rejects the
    /// start: no new session is left, even after a crash, and the earlier
    /// session's journal and title are as they were.
    #[test]
    fn a_start_failing_at_any_write_leaves_no_session_and_the_earlier_one_untouched() {
        let numbering = Numbering::new("start-sweep");
        let (fs, earlier) = with_an_earlier_session(&numbering.root);
        let sessions = numbering.root.join("sessions");
        let operations = start_operations(&numbering, &fs);
        // Making both directories, keeping the title, and their syncs.
        assert!(operations >= 10, "{operations}"); // check-bound
        let mut sweep = Sweep::new();
        for at in 0..operations {
            let run = fs.copy_disk();
            run.fail_after(at, std::io::ErrorKind::StorageFull);
            assert!(start_session(&numbering, &run).is_err(), "at {at}");
            sweep.failure_point(&run);
            assert_eq!(
                run.list(&sessions).unwrap(),
                [sessions.join("1")],
                "a fault at operation {at}"
            );
            // Nothing added anywhere, the earlier session's files as they were.
            assert_eq!(files(&run), earlier, "at {at}");
            for outcome in nota_recorder::fs::fake::CrashOutcome::standard() {
                let crashed = run.crash(outcome);
                assert_eq!(
                    crashed.list(&sessions).unwrap(),
                    [sessions.join("1")],
                    "a fault at operation {at}, {outcome:?}"
                );
                assert_eq!(files(&crashed), earlier, "at {at}, {outcome:?}");
            }
        }
        // Not vacuous: the fault hit the start at every operation.
        sweep.interrupted_at_least(operations); // check-bound
    }

    /// With the ballast held, a full disk at each of a new start's writes
    /// frees it and the write is tried again: the start makes exactly one
    /// new session, with its title kept, and leaves the earlier one as it
    /// was. A fault in the watch's own look at the sessions, before any
    /// write, rejects the start and leaves nothing.
    #[test]
    fn a_start_failing_at_any_write_with_a_ballast_makes_one_session() {
        let numbering = Numbering::new("start-sweep-ballast");
        let (fs, earlier) = with_an_earlier_session(&numbering.root);
        Ballast::keep(&fs, &numbering.root, 1_024, || false).unwrap();
        let sessions = numbering.root.join("sessions");
        let looked = {
            let probe = fs.copy_disk();
            let _watch = startup_watch(&probe, &numbering.root, 1_024).unwrap();
            probe.attempted()
        };
        let operations = start_operations(&numbering, &fs);
        // Making both directories, keeping the title, and their syncs.
        assert!(operations >= looked + 10, "{looked}, {operations}"); // check-bound
        let mut sweep = Sweep::new();
        for at in 0..operations {
            let run = fs.copy_disk();
            run.fail_after(at, std::io::ErrorKind::StorageFull);
            let made = start_session(&numbering, &run);
            sweep.failure_point(&run);
            for (path, bytes) in &earlier {
                assert_eq!(&run.read(path).unwrap(), bytes, "{path:?}, at {at}");
            }
            if at < looked {
                assert!(made.is_err(), "at {at}");
                assert_eq!(run.list(&sessions).unwrap(), [sessions.join("1")]);
                continue;
            }
            // The fault freed the ballast; the retry found room.
            let made = made.unwrap_or_else(|e| panic!("at {at}: {e}"));
            assert_eq!(
                run.list(&sessions).unwrap(),
                [sessions.join("1"), made.dir.clone()],
                "at {at}"
            );
            let kept = run.read(&made.dir.join("session.txt")).unwrap();
            let kept = String::from_utf8(kept).unwrap();
            assert!(kept.lines().any(|line| line == "title New"), "{kept}");
            assert!(
                Ballast::find(&run, &numbering.root, 1_024)
                    .unwrap()
                    .is_none()
            );
        }
        // Not vacuous: the fault hit the start at every operation.
        sweep.interrupted_at_least(operations); // check-bound
    }

    /// Cleanup refuses to remove any session containing recorded audio.
    #[test]
    fn startup_cleanup_keeps_audio() {
        use nota_recorder::fs::FsFile;
        let fs = FakeFs::with_dirs([PathBuf::from("/data"), PathBuf::from("/data/sessions")]);
        let session = pending_session(&fs);
        let journal = session.audio().join("journal-000000");
        fs.create(&journal).unwrap().write_all(&[7; 40]).unwrap();
        // Another startup failure must never make the audio collateral.
        discard_empty(&fs, &session);
        assert_eq!(fs.read(&journal).unwrap(), [7; 40]);
        assert!(fs.list(&session.dir).unwrap().contains(&session.audio()));
    }

    /// A failed start still tells the user about earlier journals.
    #[test]
    fn a_failed_start_keeps_its_recovery_notes() {
        let error = start_error(std::io::Error::from(std::io::ErrorKind::StorageFull));
        // Recovery ran before the new session ran out of room.
        let failure = startup_failure(
            error,
            vec!["session 2: its journals are still to publish".into()],
        );
        assert_eq!(
            failure.to_string(),
            "the disk is full: free some space; session 2: its journals are still to publish"
        );
    }

    /// Errors unrelated to space keep their original cause.
    #[test]
    fn a_start_permission_error_keeps_its_cause() {
        let error = start_error(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        assert_eq!(error.to_string(), "denied");
    }
}
