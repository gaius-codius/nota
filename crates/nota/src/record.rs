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
//!
//! # Files
//!
//! - `record/start.rs`: starts everything.
//! - `record/live.rs`: the live thread.
//! - `record/signals.rs`: the signal thread.
//! - `record/stop.rs`: stops everything, in order.
//! - `record/summary.rs`: the screen and the summary of how it went.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use nota_core::{Clock, SampleRate, SystemClock, TrackId};
use nota_recorder::capture::{CaptureBackend, Source};
use nota_recorder::segment::SegmentLength;

mod live;
mod signals;
mod start;
mod stop;
mod summary;

pub(crate) use summary::Outcome;
use summary::show;

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
    /// Log when each text reached the screen to this file
    /// (`--latency-log`, built only with the `latency-log` feature).
    pub(crate) latency_log: Option<PathBuf>,
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

/// Starts a recording, shows its screen until it's closed, then stops it.
fn record_with<B: CaptureBackend>(
    args: &RecordArgs,
    backend: &B,
    clock: &Arc<dyn Clock>,
) -> Result<Outcome, BoxError> {
    let (started, screening) = start::start(args, backend, clock)?;

    // The screen, until it's closed.
    let shown = show(
        screening.screen,
        args,
        &screening.listening,
        clock,
        &screening.ui,
        &screening.ui_events,
    );
    drop(screening.ui);

    stop::stop(started, shown)
}
