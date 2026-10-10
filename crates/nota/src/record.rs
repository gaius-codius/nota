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
//!   ([`record_tracks`](nota_recorder::capture::record_tracks)); hands
//!   finished journals to the publisher and everything else to the live
//!   thread. It never waits on either.
//! - **Publisher:** publishes finished journals as segments.
//! - **Live:** feeds the engine, flushes it at each new epoch, and turns
//!   levels and text into screen updates (see [`crate::live`]).
//! - **Engine events:** passes the supervisor's events to the live thread.
//! - **Disk:** checks the free space at once and every few seconds, keeps
//!   the ballast, and sends the screen the space left, the low-disk
//!   warning, and a full disk (see [`nota_recorder::disk`]).
//! - **Saver:** stores the live text, marks and notes in the library
//!   database as they come (see `record/save.rs`). Nothing waits on it.
//! - **Marks:** passes the screen's marks and notes to the saver as
//!   they're made.
//! - **Signals:** turns SIGHUP, SIGTERM and SIGINT into a request to close
//!   the screen, and notes SIGXCPU (see below).
//!
//! # The screen
//!
//! The screen and `nota record` talk only in the recorder protocol
//! ([`nota_core::recorder`]). `nota record` itself gives the start
//! command, from its arguments; its [`Setup`] decides what the tracks
//! record. The live thread and the recorder send the screen events, and
//! the screen gives back marks, notes and the stop as commands.
//!
//! # Starting
//!
//! Salvage of earlier sessions runs first, with a line on screen; a
//! signal meanwhile ends nota before any session is made. Then the
//! session, the publisher and the recorder start, then the sleep lock is
//! taken (see "Sleep" below), and only then the streams, one after
//! another: each track joins the recorder as its stream starts, so its
//! audio is journaled while the next one opens, and its first epoch opens
//! when its first audio was captured. The writer fsyncs inline while one
//! track records and on a thread per track once a second joins (see
//! [`nota_recorder::capture`]). If no stream starts, the session is
//! removed again.
//!
//! # Stopping
//!
//! The screen closes when `s` is confirmed with `y`, when a signal arrives,
//! every stream has ended or the disk is full (each sends it `Stopping`),
//! or when the terminal fails (it's gone after a hangup). Whichever it is,
//! the same steps follow: the terminal is restored (or, when the app lent
//! it, kept, with Home showing that the recording is finishing), the
//! streams stop, the recorder records what they had sent and returns, the
//! sleep lock is let go, the writer finishes (a last fsync of every
//! journal), the publisher publishes the last journals while the engine is
//! shut down (its answer to the last flush is still saved), and the saver
//! stores what it was given before the session is marked stopped and the
//! jobs that follow a stop (the final pass) are queued; after a full disk
//! they wait for space. The app runs them (see `jobs`). Further signals
//! are ignored meanwhile: the default action would kill the process before
//! the last segments are published. (The app listens for them too, for its
//! whole life, and closes once the recording has stopped.) Anything left
//! unpublished (a disk error, say) is salvaged at the next start.
//!
//! # A full disk
//!
//! Every write of the recording goes through a
//! [`WatchedFs`](nota_recorder::disk::WatchedFs). The first to fail for want
//! of space frees the ballast (256 MB kept in the data directory), so the
//! journal it broke is replaced without losing audio, and the recording
//! stops: the warning and `Stopping` go to the screen, the last segments
//! are published into the ballast's room, and the summary says the
//! recording stopped early. A low disk never stops a start: nota records
//! what fits, sending the low-disk warning from the start (the screen
//! doesn't show warnings yet; the summary says the disk ran low).
//!
//! # Sleep
//!
//! The machine is kept awake while it records, by a logind lock held from
//! before the first stream opens to the end of the recording. If logind
//! refuses, the screen gets a warning from the start and the summary says
//! the machine may sleep; recording goes on. If the machine sleeps anyway,
//! the capture opens a new epoch at the resume, and the live thread tells
//! the screen (a warning, the epoch and its gap) and the summary says when
//! and for how long (see `crate::inhibit` and `crate::live`). The screen
//! doesn't show these warnings yet.
//!
//! # SIGXCPU
//!
//! Where rtkit grants the capture thread real-time priority, it also sets
//! the process's `RLIMIT_RTTIME` soft limit to about one quantum: a
//! real-time thread that runs longer than that without blocking gets
//! SIGXCPU, whose default action would end the recording. Here it's only
//! noted, and the summary warns about it. rtkit's hard limit (SIGKILL,
//! 200 ms by default) still stops a runaway real-time thread.
//!
//! # Files
//!
//! - `record/start.rs`: starts everything.
//! - `record/live.rs`: the live thread.
//! - `record/signals.rs`: the signal thread.
//! - `record/save.rs`: the saver thread.
//! - `record/stop.rs`: stops everything, in order.
//! - `record/summary.rs`: the screen and the summary of how it went.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use nota_core::recorder::{Command, Setup};
use nota_core::{Clock, SampleRate, SystemClock, TrackId};
use nota_recorder::capture::CaptureBackend;
use nota_recorder::engine::EngineCommand;
use nota_recorder::segment::SegmentLength;
use nota_store::HeardBy;

mod live;
mod save;
mod signals;
mod start;
mod stop;
mod summary;

pub(crate) use signals::QuitSignals;
pub(crate) use start::last_setup;
use summary::show;
#[cfg(test)]
pub(crate) use summary::tests::cleanup_report;
pub(crate) use summary::{Outcome, held_notes};

use crate::final_pass::Engine;
use crate::inhibit::Logind;
#[cfg(target_os = "linux")]
use crate::inhibit::SystemLogind;
use crate::library::Library;
use crate::terminal::Screen;

/// Every track is recorded at the engine's rate.
pub(crate) const RATE: SampleRate = SampleRate::SPEECH;
/// The microphone's track.
pub(crate) const MIC: TrackId = TrackId::new(0);
/// The system audio's track.
pub(crate) const SYSTEM: TrackId = TrackId::new(1);

pub(crate) type BoxError = Box<dyn Error + Send + Sync>;

/// What to record, and where.
#[derive(Debug, Clone)]
pub(crate) struct RecordArgs {
    /// The data directory, holding `sessions/`.
    pub(crate) data: PathBuf,
    /// The command that starts the recording: [`Command::Start`], with
    /// what to record and the session's title.
    pub(crate) start: Command,
    /// The engine's models, `--parakeet` and `--vad`; without them there's
    /// no live text.
    pub(crate) models: Option<(PathBuf, PathBuf)>,
    /// Record a tone instead of the audio server (tests only).
    pub(crate) tone: bool,
    /// Log when each text reached the screen to this file
    /// (`--latency-log`, built only with the `latency-log` feature).
    pub(crate) latency_log: Option<PathBuf>,
    /// Run the final pass with nota's stand-in engine, `nota engine fake`
    /// (`--fake-engine yes`, tests only).
    pub(crate) fake_engine: bool,
}

/// The engine the final pass runs: the stand-in if asked for, else the
/// real one if there are models; `None` without either.
///
/// # Errors
///
/// If nota's own executable can't be found.
pub(crate) fn final_engine(args: &RecordArgs) -> std::io::Result<Option<Engine>> {
    let program = std::env::current_exe()?;
    if args.fake_engine {
        return Ok(Some(Engine {
            command: EngineCommand {
                program,
                args: ["engine", "fake", "--pass", "final"]
                    .map(Into::into)
                    .to_vec(),
            },
            heard_by: HeardBy {
                engine: "fake".to_owned(),
                model: "fake".to_owned(),
            },
        }));
    }
    let Some(models) = &args.models else {
        return Ok(None);
    };
    let (engine, model) = start::heard_by(models);
    Ok(Some(Engine {
        command: EngineCommand {
            program,
            args: vec![
                "engine".into(),
                "asr".into(),
                "--parakeet".into(),
                models.0.as_os_str().to_owned(),
                "--vad".into(),
                models.1.as_os_str().to_owned(),
                "--pass".into(),
                "final".into(),
            ],
        },
        heard_by: HeardBy { engine, model },
    }))
}

/// Records a session until it's stopped, then finishes it. Once the
/// session is recording, what goes wrong (the screen failing, say) is a
/// note on the outcome.
///
/// # Errors
///
/// If the session can't be started (no clock, no data directory, no
/// stream at all, a stop asked for first).
pub(crate) fn record(args: &RecordArgs) -> Result<Outcome, BoxError> {
    record_in(args, None).map(|(outcome, _)| outcome)
}

/// The app's terminal, lent to a recording, and what to draw on it while
/// the recording stops.
pub(crate) struct Lent<'a> {
    pub(crate) screen: Screen,
    /// The app's library, so the process keeps one connection to its
    /// database.
    pub(crate) library: Library,
    pub(crate) stopping: &'a mut dyn FnMut(&mut Screen),
}

/// Records as [`record`] does. With `lent`, on the app's terminal: once
/// the Recording screen closes, `stopping` draws on it while the recording
/// stops, and the terminal comes back with the outcome (unless it failed).
/// Without, the terminal is set up here and restored before the stop.
///
/// # Errors
///
/// As [`record`]'s. A lent terminal is restored, not handed back.
pub(crate) fn record_in(
    args: &RecordArgs,
    lent: Option<Lent<'_>>,
) -> Result<(Outcome, Option<Screen>), BoxError> {
    let Command::Start(setup) = &args.start else {
        return Err("a recording starts only with a start command".into());
    };
    let clock: Arc<dyn Clock> =
        Arc::new(SystemClock::start().map_err(|_| "the system clock can't be read")?);
    #[cfg(feature = "fake-capture")]
    if args.tone {
        let happenings = crate::tone::Happenings::default();
        let tone = crate::tone::Tone::new(Arc::clone(&clock), happenings.clone());
        let logind = crate::tone::ToneLogind::new(happenings);
        let (mut outcome, screen) = record_with(args, setup, &tone, &clock, &logind, lent)?;
        outcome.notes.extend(tone.report());
        return Ok((outcome, screen));
    }
    if args.tone {
        return Err("--tone is only for nota's tests".into());
    }
    #[cfg(target_os = "linux")]
    return record_with(
        args,
        setup,
        &nota_recorder::capture::PipeWireBackend,
        &clock,
        &SystemLogind,
        lent,
    );
    #[cfg(not(target_os = "linux"))]
    {
        drop(lent);
        Err("recording needs Linux for now".into())
    }
}

/// The window every segment is cut to.
pub(crate) fn segment_length() -> SegmentLength {
    SegmentLength::default_at(RATE)
}

/// Starts a recording as `setup` says, shows its screen until it's
/// closed, then stops it.
fn record_with<B: CaptureBackend>(
    args: &RecordArgs,
    setup: &Setup,
    backend: &B,
    clock: &Arc<dyn Clock>,
    logind: &dyn Logind,
    lent: Option<Lent<'_>>,
) -> Result<(Outcome, Option<Screen>), BoxError> {
    let (given, library, stopping) = match lent {
        Some(Lent {
            screen,
            library,
            stopping,
        }) => (Some(screen), Some(library), Some(stopping)),
        None => (None, None, None),
    };
    let (started, screening) = start::start(args, setup, backend, clock, logind, given, library)?;

    // The screen, until it's closed.
    let (shown, screen) = show(
        screening.screen,
        &setup.title,
        &screening.listening,
        clock,
        &screening.ui,
        &screening.ui_events,
        &screening.save,
    );
    drop(screening.ui);
    drop(screening.save);
    // `nota record` restores the terminal before stopping; the app keeps
    // it, and shows that the recording is stopping.
    let screen = match (screen, stopping) {
        (Some(mut screen), Some(stopping)) => {
            stopping(&mut screen);
            Some(screen)
        }
        _ => None,
    };

    // Recorded, whatever stopping says: anything that went wrong is a
    // note on the outcome.
    Ok((stop::stop(started, shown), screen))
}

#[cfg(test)]
mod tests {
    use nota_core::SessionTime;
    use nota_core::recorder::Mark;

    use super::*;

    /// The recorder starts on a start command, and on nothing else: the
    /// session isn't made, so the data directory stays empty.
    #[test]
    fn only_a_start_command_starts_a_recording() {
        let data = std::env::temp_dir().join(format!("nota-start-{}", std::process::id()));
        for given in [
            Command::Stop,
            Command::Mark(Mark {
                at: SessionTime::ZERO,
            }),
        ] {
            let args = RecordArgs {
                data: data.clone(),
                start: given,
                models: None,
                tone: false,
                latency_log: None,
                fake_engine: false,
            };
            let err = record(&args).unwrap_err();
            assert!(err.to_string().contains("start command"), "{err}");
        }
        assert!(!data.exists());
    }
}
