//! Capturing a track's audio and recording it into the journal.
//!
//! A [`CaptureBackend`] opens a stream on a [`Source`] and delivers mono
//! 16-bit samples at the requested rate as [`CaptureEvent`]s on a channel.
//! The stream's callback only copies the samples into the channel; it never
//! touches the disk or waits on anything, so an fsync that takes a second
//! can't make the audio server drop audio. [`record_track`] runs on the
//! recorder thread, takes the samples off the channel and appends them to
//! the [`SessionWriter`](crate::session::SessionWriter), which does every
//! write and fsync.
//!
//! The channel is unbounded: while the disk stalls, audio queues in memory
//! (32 KB a second at 16 kHz) rather than being dropped, and the recorder
//! catches up when the disk does.
//!
//! [`start`] returns a [`Capture`], which keeps the stream running until it's
//! dropped, on the thread that started it, and a
//! [`CaptureReceiver`] for the recorder thread. Stopping ends the stream and
//! then tells the recorder, which records everything the stream delivered
//! before it ends.
//!
//! On Linux, [`PipeWireBackend`] captures through cpal's `PipeWire` host. It
//! links `libpipewire-0.3` and `libasound` (cpal's ALSA host is always built
//! on Linux) dynamically, and building it needs their development headers,
//! `pkg-config`, and clang (libclang, for bindgen):
//! `libpipewire-0.3-dev libasound2-dev libclang-dev pkg-config` on
//! Debian and Ubuntu, `pipewire alsa-lib clang pkgconf` on Arch.
//! Other platforms build without it; they record nothing until v2.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use nota_core::{SampleRate, TrackId};

use crate::fs::Fs;
use crate::session::{FinishedJournal, SessionError, SessionWriter};

#[cfg(target_os = "linux")]
mod pipewire;
#[cfg(test)]
mod tests;

#[cfg(target_os = "linux")]
pub use pipewire::PipeWireBackend;

/// What to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// What the default output plays: the system's audio.
    SystemAudio,
    /// The default input: the microphone.
    Microphone,
    /// One device, by the backend's name for it. On `PipeWire`, a node name;
    /// a sink's name captures what plays on it.
    Device(String),
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SystemAudio => f.write_str("the system audio"),
            Self::Microphone => f.write_str("the microphone"),
            Self::Device(name) => write!(f, "device {name}"),
        }
    }
}

/// Why capture couldn't start, or stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// The audio server isn't running or can't be reached.
    HostUnavailable(String),
    /// The source has no device, or its device went away.
    DeviceNotAvailable(Source),
    /// The device can't give mono 16-bit audio at the rate asked for.
    UnsupportedConfig(String),
    /// Any other failure the backend reported.
    Backend(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable(e) => write!(f, "the audio server isn't available: {e}"),
            Self::DeviceNotAvailable(source) => write!(f, "{source} isn't available"),
            Self::UnsupportedConfig(e) => write!(f, "the device can't capture as asked: {e}"),
            Self::Backend(e) => write!(f, "capture failed: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Something the stream reported that doesn't stop it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureNotice {
    /// The audio server overran the stream's buffer: audio was lost, and
    /// the track's sample count fell behind the wall clock by that much.
    Overrun,
    /// The default device changed and the stream followed it.
    RouteChanged,
    /// Anything else the backend reported without stopping the stream,
    /// such as real-time priority refused, or default-device changes no
    /// longer watched.
    Warning(String),
}

/// What a capture stream sends the recorder thread.
#[derive(Debug)]
pub enum CaptureEvent {
    /// The next samples, mono, at the rate the stream was started with.
    Audio(Vec<i16>),
    /// Something to note; capture goes on.
    Notice(CaptureNotice),
    /// The stream failed and delivers nothing more.
    Failed(CaptureError),
    /// The stream was stopped; nothing follows.
    Stopped,
}

/// The stream's end of the channel, given to [`CaptureBackend::start`]. It
/// never blocks.
#[derive(Debug, Clone)]
pub struct CaptureSender {
    events: mpsc::Sender<CaptureEvent>,
    /// Set once the capture is being stopped.
    stopping: Arc<AtomicBool>,
}

impl CaptureSender {
    /// Sends a copy of `samples`. Empty calls send nothing.
    pub fn audio(&self, samples: &[i16]) {
        if !samples.is_empty() {
            // The recorder has gone: nothing is listening, and the stream is
            // about to be stopped.
            let _ = self.events.send(CaptureEvent::Audio(samples.to_vec()));
        }
    }

    /// Reports something that doesn't stop the stream.
    pub fn notice(&self, notice: CaptureNotice) {
        let _ = self.events.send(CaptureEvent::Notice(notice));
    }

    /// Reports that the stream failed, unless the capture is being stopped:
    /// a stream torn down on purpose may report that as a failure (cpal
    /// 0.18's `PipeWire` host says a named device disconnected).
    pub fn failed(&self, error: CaptureError) {
        if !self.stopping.load(Ordering::SeqCst) {
            let _ = self.events.send(CaptureEvent::Failed(error));
        }
    }
}

/// The recorder thread's end of the channel, and the rate the stream
/// captures at.
#[derive(Debug)]
pub struct CaptureReceiver {
    events: mpsc::Receiver<CaptureEvent>,
    rate: SampleRate,
}

impl CaptureReceiver {
    /// The rate the stream captures at.
    #[must_use]
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The next event, waiting at most `timeout`. `None` if none came in
    /// time. A channel every sender has left reads as [`CaptureEvent::Stopped`].
    fn next(&self, timeout: Duration) -> Option<CaptureEvent> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(CaptureEvent::Stopped),
        }
    }
}

/// A source of audio, such as an audio server, or synthetic audio in tests.
pub trait CaptureBackend {
    /// A running stream. Capture stops when it's dropped.
    type Stream;

    /// Starts capturing `source` as mono 16-bit samples at `rate`, sent to
    /// `events` as they arrive.
    ///
    /// # Errors
    ///
    /// A [`CaptureError`] if the stream can't be opened.
    fn start(
        &self,
        source: &Source,
        rate: SampleRate,
        events: CaptureSender,
    ) -> Result<Self::Stream, CaptureError>;
}

/// A running capture. Dropping it stops the stream, then tells the
/// recorder, which records what the stream sent before stopping and then
/// returns.
#[derive(Debug)]
pub struct Capture<T> {
    stream: Option<T>,
    stopped: mpsc::Sender<CaptureEvent>,
    stopping: Arc<AtomicBool>,
}

impl<T> Drop for Capture<T> {
    fn drop(&mut self) {
        // The stream goes first, so nothing it sends comes after `Stopped`.
        // What it sends as it goes is still recorded, except a failure.
        self.stopping.store(true, Ordering::SeqCst);
        drop(self.stream.take());
        let _ = self.stopped.send(CaptureEvent::Stopped);
    }
}

/// Starts capturing `source` at `rate` through `backend`.
///
/// # Errors
///
/// The backend's [`CaptureError`] if the stream can't be opened.
pub fn start<B: CaptureBackend>(
    backend: &B,
    source: &Source,
    rate: SampleRate,
) -> Result<(Capture<B::Stream>, CaptureReceiver), CaptureError> {
    let (tx, rx) = mpsc::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let sender = CaptureSender {
        events: tx.clone(),
        stopping: Arc::clone(&stopping),
    };
    let stream = backend.start(source, rate, sender)?;
    Ok((
        Capture {
            stream: Some(stream),
            stopped: tx,
            stopping,
        },
        CaptureReceiver { events: rx, rate },
    ))
}

/// How long the recorder waits for audio before checking whether a journal
/// is due an fsync anyway.
const IDLE_SYNC_CHECK: Duration = Duration::from_millis(100);

/// What the recorder reports while it records.
#[derive(Debug)]
pub enum RecorderEvent {
    /// These journals have ended and are ready to publish, in order.
    Finished(Vec<FinishedJournal>),
    /// A journal broke and so did its replacement: the samples since the
    /// last durable position are a gap. Recording goes on in a new journal.
    JournalFailed(SessionError),
    /// The stream noted something; recording goes on.
    Capture(CaptureNotice),
}

/// Why [`record_track`] stopped early.
#[derive(Debug)]
pub enum RecordError {
    /// The stream failed. What it sent before failing is recorded.
    Capture(CaptureError),
    /// The track can't be recorded (it wasn't started, or ran out of sample
    /// numbers or journal ids).
    Session(SessionError),
    /// The stream captures at another rate than the writer stamps on its
    /// journals, so its samples would be timed wrongly. Nothing is recorded.
    RateMismatch {
        /// The stream's rate.
        capture: SampleRate,
        /// The writer's.
        journal: SampleRate,
    },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(e) => e.fmt(f),
            Self::Session(e) => e.fmt(f),
            Self::RateMismatch { capture, journal } => write!(
                f,
                "the stream captures at {} Hz but journals are written at {} Hz",
                capture.hz(),
                journal.hz()
            ),
        }
    }
}

impl std::error::Error for RecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Capture(e) => Some(e),
            Self::Session(e) => Some(e),
            Self::RateMismatch { .. } => None,
        }
    }
}

/// Records `track` from `events` into `writer` until the capture stops,
/// reporting finished journals, journal failures and the stream's notices
/// to `report` as they happen. Run it on the recorder thread; start the
/// track on `writer` first.
///
/// Journals are fsync'd as audio arrives, and also when it stops arriving,
/// so durable stays within about
/// [`SYNC_INTERVAL`](crate::journal::SYNC_INTERVAL) of captured either
/// way. The writer isn't finished on return: call
/// [`SessionWriter::finish`](crate::session::SessionWriter::finish) for the
/// last journals.
///
/// `report` runs on this thread, between appends: return quickly (hand
/// finished journals to another thread to publish). Audio that arrives
/// meanwhile waits in memory, not yet counted as captured, and is lost if
/// the process dies.
///
/// # Errors
///
/// [`RecordError::RateMismatch`], before anything is recorded, if the
/// stream and `writer` run at different rates;
/// [`RecordError::Capture`] if the stream failed;
/// [`RecordError::Session`] if `track` can't be recorded. A journal that
/// breaks isn't an error here: it's reported, and recording goes on.
pub fn record_track<S: Fs>(
    writer: &mut SessionWriter<S>,
    track: TrackId,
    events: &CaptureReceiver,
    report: &mut dyn FnMut(RecorderEvent),
) -> Result<(), RecordError> {
    if events.rate() != writer.rate() {
        return Err(RecordError::RateMismatch {
            capture: events.rate(),
            journal: writer.rate(),
        });
    }
    loop {
        let event = events.next(IDLE_SYNC_CHECK);
        let outcome = match event {
            Some(CaptureEvent::Audio(samples)) => writer.append(track, &samples),
            Some(CaptureEvent::Notice(notice)) => {
                report(RecorderEvent::Capture(notice));
                Ok(())
            }
            Some(CaptureEvent::Failed(error)) => return Err(RecordError::Capture(error)),
            Some(CaptureEvent::Stopped) => return Ok(()),
            None => Ok(()),
        };
        let synced = writer.sync_if_due();
        for result in [outcome, synced] {
            match result {
                Ok(()) => {}
                Err(error @ SessionError::Journal(_)) => {
                    report(RecorderEvent::JournalFailed(error));
                }
                Err(error) => return Err(RecordError::Session(error)),
            }
        }
        let finished = writer.take_finished();
        if !finished.is_empty() {
            report(RecorderEvent::Finished(finished));
        }
    }
}
