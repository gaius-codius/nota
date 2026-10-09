//! Starting a recording: the terminal, the session, the tracks and the
//! threads that record them.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use nota_core::recorder::{self, Input, Setup};
use nota_core::{Clock, EpochId, SessionId, TrackId, TrackTimeline, wall_now};
use nota_recorder::capture::{
    Capture, CaptureBackend, RecordError, RecorderEvent, Source, record_tracks, start_tracks,
};
use nota_recorder::engine::{EngineCommand, EngineConfig, EngineSupervisor};
use nota_recorder::fs::StdFs;
use nota_recorder::segment::Publisher;
use nota_recorder::session::{SessionDir, SessionLock, SessionStore, SessionWriter, Syncing};
use nota_store::{NewSession, Track, TrackKind};
use nota_tui::Event;

use super::live::{LiveInput, spawn_live};
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
}

/// What stopping needs of what was started.
pub(super) struct Started<B: CaptureBackend> {
    pub(super) outcome: Outcome,
    pub(super) signals: SignalThread,
    pub(super) draws: Option<DrawEnds>,
    pub(super) library: Library,
    pub(super) session: SessionId,
    pub(super) lock: SessionLock<StdFs>,
    pub(super) captures: Vec<Capture<B::Stream>>,
    pub(super) publisher: Publisher,
    pub(super) live_inputs: Sender<LiveInput>,
    pub(super) live: JoinHandle<Option<LatencyLog>>,
    pub(super) recorder: JoinHandle<Recorded>,
}

/// The recorder thread's result: the writer, how recording ended, how
/// many journal failures it reported, and the streams that failed.
pub(super) type Recorded = (
    SessionWriter<StdFs>,
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
    let lock = SessionDir::new(session.id, StdFs, &session.audio()).lock()?;
    // A track's fsyncs on a thread of its own: neither track's audio waits
    // for the other's disk.
    let mut writer = SessionWriter::open(&lock, RATE, segment_length(), Arc::clone(clock))?
        .with_syncing(Syncing::Threads);

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
    let mut timelines = open_timelines(&mut writer, &captures)?;

    // The session's row is added by the publisher, before its first
    // segment's: recording never waits on the database.
    let rows = session_rows(&library, setup, session.id, &sources, &captures);
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
        log.map(|log| (log, Arc::clone(clock))),
    )?;
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
                let _ = ui.send(Event::Recorder(recorder::Event::Stopping));
                (writer, result, failures, lost)
            })?
    };

    let started = Started {
        outcome,
        signals,
        draws,
        library,
        session: session.id,
        lock,
        captures,
        publisher,
        live_inputs,
        live,
        recorder,
    };
    let screening = Screening {
        screen,
        ui,
        ui_events,
        listening: listening.join(" + "),
    };
    Ok((started, screening))
}

/// Starts each track in the writer and opens its timeline's first epoch.
fn open_timelines<S>(
    writer: &mut SessionWriter<StdFs>,
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

/// The rows the publisher adds for this session: its title and the tracks
/// that started.
fn session_rows<S>(
    library: &Library,
    setup: &Setup,
    id: SessionId,
    sources: &[(TrackId, Source); 2],
    captures: &[Capture<S>],
) -> NewSessionRows {
    NewSessionRows::new(
        library.db().clone(),
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
        },
    )
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
