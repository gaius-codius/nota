//! Starting a recording: the terminal, the session, the tracks and the
//! threads that record them.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use nota_core::recorder::{self, Cause, Input, Setup, Warning, WarningState};
use nota_core::{Clock, EpochId, SessionId, TrackId, TrackTimeline, wall_now};
use nota_recorder::capture::{
    Capture, CaptureBackend, CaptureReceiver, RecordError, RecorderEvent, Source, record_tracks,
    start_tracks,
};
use nota_recorder::disk::{
    BALLAST_LEN, CHECK_INTERVAL, DiskMonitor, DiskReport, DiskWatch, MonitorConfig, Usage,
    WatchedFs, WatchedStore,
};
use nota_recorder::engine::{EngineCommand, EngineConfig, EngineSupervisor};
use nota_recorder::fs::StdFs;
use nota_recorder::segment::{PublishQueue, Publisher};
use nota_recorder::session::{SessionDir, SessionLock, SessionStore, SessionWriter, Syncing};
use nota_store::{Heard, NewSession, Track, TrackKind};
use nota_tui::Event;

use super::live::{LiveInput, spawn_live};
use super::save::{Saver, ToSave};
use super::signals::{SignalThread, listen_for_signals};
use super::summary::{Outcome, file_names, track_name};
use super::{BoxError, MIC, RATE, RecordArgs, SYSTEM, segment_length};
use crate::latency::{DrawEnds, LatencyLog};
use crate::library::{Library, NewSessionRows, Salvaged};
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
    /// Checks the disk and keeps the ballast; stopped after publishing, so
    /// a full disk then is still in the summary.
    pub(super) disk: DiskMonitor<StdFs>,
    pub(super) publisher: Publisher,
    pub(super) live_inputs: Sender<LiveInput>,
    pub(super) live: JoinHandle<Option<LatencyLog>>,
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
/// terminal first (without one there's nothing to record into), then the
/// session, the tracks, the publisher, the live thread and the recorder.
/// The terminal is `screen` and the library `library` if given (the
/// app's, already set up and open), else they're set up here. A lent
/// terminal was set up after the app's own signal listener, so the order
/// holds for it too.
///
/// # Errors
///
/// If any of it can't be started, or no stream at all can.
pub(super) fn start<B: CaptureBackend>(
    args: &RecordArgs,
    setup: &Setup,
    backend: &B,
    clock: &Arc<dyn Clock>,
    screen: Option<Screen>,
    library: Option<Library>,
) -> Result<(Started<B>, Screening), BoxError> {
    let mut outcome = Outcome::default();
    let (ui, ui_events) = mpsc::channel::<Event>();
    let signals = listen_for_signals(ui.clone())?;
    // The terminal first: without one there's nothing to record into.
    let draws = args.latency_log.as_ref().map(|_| DrawEnds::default());
    let screen = match screen {
        Some(screen) => screen,
        None => Screen::enter(draws.clone().map(|d| (d, Arc::clone(clock))))?,
    };

    let library = library.map_or_else(|| Library::open(&args.data), Ok)?;
    note_salvaged(&library, &mut outcome)?;
    let session = library.create()?;
    outcome.session.clone_from(&session.dir);
    let Opened(watch, lock, mut writer) = open_session(session.id, &session.audio(), clock)?;

    let sources = sources(setup);
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
    let timelines = open_timelines(&mut writer, &captures)?;

    // Checked, and a ballast already there held, before the recorder
    // starts; then while recording. A full disk stops it.
    let disk = watch_disk(args, &session.audio(), captures.len(), &watch, &ui, clock)?;

    // Recording never waits on the database: the session's row is kept in
    // its directory too, so it's adopted with its title and tracks if the
    // database never takes it.
    let row = session_row(setup, session.id, &sources, &captures);
    if let Err(e) = session.keep(&row) {
        outcome.notes.push(format!(
            "the session's title and tracks weren't kept with its audio: {e}"
        ));
    }
    let rows = NewSessionRows::new(library.db().clone(), row);
    // The live text, marks and notes are stored as they come, on a thread
    // of their own; whichever of it and the publisher writes first adds the
    // session's row.
    let saver = Saver::spawn(
        saving(rows.clone(), session.id, args.models.as_ref().map(heard_by)),
        ui.clone(),
        Arc::clone(clock),
    )?;
    // SQLite writes the database itself, so the segment rows' commits are
    // watched for a full disk too.
    let rows = WatchedStore::new(rows, watch, &library.db_path());
    let publisher = Publisher::spawn(SessionStore::new(lock.clone(), rows), segment_length())?;
    let (live_inputs, live_received) = mpsc::channel::<LiveInput>();
    let engine = match &args.models {
        Some((parakeet, vad)) => Some(start_engine(parakeet, vad, clock, &live_inputs)?),
        None => None,
    };
    let log = args.latency_log.clone().map(LatencyLog::new);
    let live = spawn_live(
        Live::new(&timelines),
        engine,
        live_received,
        ui.clone(),
        saver.sender(),
        log.map(|log| (log, Arc::clone(clock))),
    )?;
    let recorder = spawn_recorder(
        writer,
        timelines,
        events,
        publisher.queue(),
        live_inputs.clone(),
        ui.clone(),
    )?;

    let started_save = saver.sender();
    let started = Started {
        outcome,
        signals,
        draws,
        library,
        session: session.id,
        lock,
        captures,
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
        listening: listening.join(" + "),
        save: started_save,
    };
    Ok((started, screening))
}

/// The new session's audio directory, owned through a watched
/// filesystem, and its writer, with the watch.
struct Opened(
    Arc<DiskWatch<StdFs>>,
    SessionLock<RecordFs>,
    SessionWriter<RecordFs>,
);

/// Opens the new session's audio directory through a watched filesystem.
fn open_session(
    id: SessionId,
    audio: &std::path::Path,
    clock: &Arc<dyn Clock>,
) -> Result<Opened, BoxError> {
    // Every write of the recording, the publisher's too, goes through the
    // watch: the first to meet a full disk frees the ballast.
    let watch = DiskWatch::new(StdFs);
    let lock = SessionDir::new(id, watch.fs(), audio).lock()?;
    // A track's fsyncs on a thread of its own: neither track's audio waits
    // for the other's disk.
    let writer = SessionWriter::open(&lock, RATE, segment_length(), Arc::clone(clock))?
        .with_syncing(Syncing::Threads);
    Ok(Opened(watch, lock, writer))
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
        ballast_len: if args.tone {
            TONE_BALLAST_LEN
        } else {
            BALLAST_LEN
        },
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
    mut timelines: Vec<TrackTimeline>,
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
            let result = record_tracks(&mut writer, &mut timelines, &events, &mut |track, e| {
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

/// Starts each track in the writer and opens its timeline's first epoch.
fn open_timelines<S>(
    writer: &mut SessionWriter<RecordFs>,
    captures: &[Capture<S>],
) -> Result<Vec<TrackTimeline>, BoxError> {
    let mut timelines = Vec::new();
    for capture in captures {
        let track = capture.track();
        let first = writer.first_free_sample(track);
        writer.start_track(track, EpochId::new(0), first)?;
        let mut timeline = TrackTimeline::new(track);
        timeline.open_epoch(capture.started_at(), first, RATE)?;
        timelines.push(timeline);
    }
    Ok(timelines)
}

/// Salvages the sessions an earlier run left, and notes what became of
/// each.
fn note_salvaged(library: &Library, outcome: &mut Outcome) -> Result<(), BoxError> {
    for salvaged in library.salvage_all(segment_length())? {
        outcome.notes.push(salvage_note(&salvaged));
    }
    Ok(())
}

/// What the summary says of one salvaged session.
fn salvage_note(salvaged: &Salvaged) -> String {
    match salvaged {
        Salvaged::Done(id, aside) => {
            format!("salvaged session {}{}", id.get(), set_aside_part(aside))
        }
        Salvaged::Left(id, aside) => format!(
            "salvaged session {}, but some of its journals are still to publish{}",
            id.get(),
            set_aside_part(aside)
        ),
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

/// This session's row: its title and the tracks that started.
fn session_row<S>(
    setup: &Setup,
    id: SessionId,
    sources: &[(TrackId, Source); 2],
    captures: &[Capture<S>],
) -> NewSession {
    NewSession {
        id,
        title: Some(setup.title.clone()),
        language: None,
        // When it started, for its date; read once, here.
        started_at: wall_now(),
        tracks: sources
            .iter()
            .filter(|(track, _)| captures.iter().any(|c| c.track() == *track))
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
fn heard_by((parakeet, _): &(PathBuf, PathBuf)) -> (String, String) {
    let model = parakeet.file_name().map_or_else(
        || "parakeet".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    ("sherpa-onnx".to_owned(), model)
}

/// How the saver stores each item: in session `session`, adding its row
/// first if nothing has yet. Text is stored as heard by `heard_by`'s
/// engine and model, without word times: the engine doesn't give them yet.
fn saving(
    rows: NewSessionRows,
    session: SessionId,
    heard_by: Option<(String, String)>,
) -> impl FnMut(&ToSave) -> Result<(), nota_store::StoreError> + Send + 'static {
    move |item| {
        rows.added(session)?;
        rows.db().with(|db| match item {
            ToSave::Heard(utterance) => {
                // Text comes only from an engine, which has models.
                let (engine, model) = heard_by
                    .clone()
                    .unwrap_or_else(|| ("unknown".to_owned(), "unknown".to_owned()));
                let heard = Heard {
                    utterance: utterance.clone(),
                    engine,
                    model,
                    words: Vec::new(),
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

/// Each track's source, as `setup` chose it.
fn sources(setup: &Setup) -> [(TrackId, Source); 2] {
    let source = |input: &Input, default| match input {
        Input::Default => default,
        Input::Device(name) => Source::Device(name.clone()),
    };
    [
        (MIC, source(&setup.mic, Source::Microphone)),
        (SYSTEM, source(&setup.system, Source::SystemAudio)),
    ]
}

/// The setup the last session recorded with: its title, and each track's
/// source, as [`source_name`] named it in the library. A track it didn't
/// record (a stream that didn't start) follows the default. `None` if
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
        mic: input(TrackKind::Microphone, &source_name(&Source::Microphone)),
        system: input(TrackKind::System, &source_name(&Source::SystemAudio)),
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

    /// The setup a start command carries decides what each track records.
    #[test]
    fn the_setup_chooses_each_track_s_source() {
        let mut setup = Setup {
            title: "Workshop".to_owned(),
            mic: Input::Default,
            system: Input::Default,
        };
        assert_eq!(
            sources(&setup),
            [(MIC, Source::Microphone), (SYSTEM, Source::SystemAudio)]
        );
        setup.mic = Input::Device("usb-mic".to_owned());
        setup.system = Input::Device("speakers.monitor".to_owned());
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
        write(&ToSave::Heard(heard.clone())).unwrap();
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
        write(&ToSave::Heard(crash_heard().unwrap())).unwrap();
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
        );
        assert_eq!(
            salvage_note(&left),
            "salvaged session 3, but some of its journals are still to publish; \
             journals were damaged, so parts of it weren't published: \
             journal-000000.unreadable, journal-000004.unreadable are kept"
        );
    }
}
