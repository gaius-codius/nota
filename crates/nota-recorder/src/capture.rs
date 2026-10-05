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
//! # Overruns
//!
//! When the audio server overruns the stream's buffer, the audio it lost
//! never arrives, so the track's sample count falls behind session time.
//! [`record_track`] then starts a new epoch, in the track's
//! [`TrackTimeline`] and in its journals alike, at the session time the
//! overrun was reported: the samples after it are timed from there, not
//! early by the audio lost, and the loss shows as a gap between the two
//! epochs. The time is read on the stream's thread as the overrun is
//! reported, not when the recorder gets to it, which may be seconds later
//! while the disk stalls.
//!
//! That time is approximate until holes are measured from the stream's
//! own timestamps:
//! - The audio that follows was captured up to a buffer before the
//!   overrun is reported, so it's timed late by up to that much.
//! - The audio server reports overruns of its whole graph, so a stream
//!   that lost nothing may still get a new epoch, with a gap of about a
//!   buffer.
//! - If the device's clock has run fast by more than the audio lost, the
//!   timeline takes the overrun as drift
//!   ([`TrackTimeline::open_epoch`]): the new epoch starts where the old
//!   one's audio ends, with no gap.
//!
//! [`start`] returns a [`Capture`], which keeps the stream running until it's
//! dropped, on the thread that started it, and a
//! [`CaptureReceiver`] for the recorder thread. Stopping ends the stream and
//! then tells the recorder, which records everything the stream delivered
//! before it ends.
//!
//! # Several tracks
//!
//! [`start_tracks`] starts one stream per track (the system audio and the
//! microphone, say), all feeding one [`CaptureReceiver`], and
//! [`record_tracks`] records them on one recorder thread into one
//! [`SessionWriter`](crate::session::SessionWriter): each track in its own
//! journals, epochs and segments, timed by its own [`TrackTimeline`]. A
//! track whose stream fails is reported and the others record on; the
//! recorder returns once every stream has stopped or failed. Every
//! journal's fsync is checked after every event, whichever track it came
//! from, and while no audio arrives, so each track keeps its own
//! durable-loss bound.
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

use std::collections::BTreeSet;

use nota_core::messages::AudioChunk;
use nota_core::{Clock, Epoch, EpochError, SampleRate, SessionTime, TrackId, TrackTimeline};

use crate::fs::Fs;
use crate::session::{FinishedJournal, SessionError, SessionWriter};

#[cfg(target_os = "linux")]
mod pipewire;
#[cfg(test)]
mod stop_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tracks_tests;

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
    /// the track's sample count fell behind session time by that much.
    /// [`record_track`] moves the track to a new epoch, so the loss is a
    /// gap, unless the timeline refuses one.
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
    Notice {
        /// What happened.
        notice: CaptureNotice,
        /// When it was reported, by the session clock.
        at: SessionTime,
    },
    /// The stream failed and delivers nothing more.
    Failed(CaptureError),
    /// The stream was stopped; nothing follows.
    Stopped,
}

/// The stream's end of the channel, given to [`CaptureBackend::start`]. It
/// never blocks.
#[derive(Debug, Clone)]
pub struct CaptureSender {
    events: mpsc::Sender<(TrackId, CaptureEvent)>,
    /// The track the stream captures.
    track: TrackId,
    /// Stamps notices as they're reported.
    clock: Arc<dyn Clock>,
    /// Set once the capture is being stopped.
    stopping: Arc<AtomicBool>,
}

impl CaptureSender {
    /// Sends a copy of `samples`. Empty calls send nothing.
    pub fn audio(&self, samples: &[i16]) {
        if !samples.is_empty() {
            // The recorder has gone: nothing is listening, and the stream is
            // about to be stopped.
            let _ = self
                .events
                .send((self.track, CaptureEvent::Audio(samples.to_vec())));
        }
    }

    /// Reports something that doesn't stop the stream, stamped with the
    /// session time now.
    pub fn notice(&self, notice: CaptureNotice) {
        let at = self.clock.now();
        let _ = self
            .events
            .send((self.track, CaptureEvent::Notice { notice, at }));
    }

    /// Reports that the stream failed, unless the capture is being stopped:
    /// a stream torn down on purpose may report that as a failure (cpal
    /// 0.18's `PipeWire` host says a named device disconnected).
    pub fn failed(&self, error: CaptureError) {
        if !self.stopping.load(Ordering::SeqCst) {
            let _ = self.events.send((self.track, CaptureEvent::Failed(error)));
        }
    }
}

/// The recorder thread's end of the channel: the events of every stream
/// started into it, each with its track, and the rate they capture at.
#[derive(Debug)]
pub struct CaptureReceiver {
    events: mpsc::Receiver<(TrackId, CaptureEvent)>,
    rate: SampleRate,
    /// The tracks whose streams started, in order.
    tracks: Vec<TrackId>,
}

/// What the recorder thread got from the channel.
#[derive(Debug)]
enum Next {
    /// An event from a track's stream.
    Event(TrackId, CaptureEvent),
    /// Nothing came in time.
    Idle,
    /// Every sender has left: no stream sends anything more.
    Closed,
}

impl CaptureReceiver {
    /// The rate the streams capture at.
    #[must_use]
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The tracks whose streams started, in the order they were asked for.
    #[must_use]
    pub fn tracks(&self) -> &[TrackId] {
        &self.tracks
    }

    /// The next event, waiting at most `timeout`.
    fn next(&self, timeout: Duration) -> Next {
        match self.events.recv_timeout(timeout) {
            Ok((track, event)) => Next::Event(track, event),
            Err(mpsc::RecvTimeoutError::Timeout) => Next::Idle,
            Err(mpsc::RecvTimeoutError::Disconnected) => Next::Closed,
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
    track: TrackId,
    stopped: mpsc::Sender<(TrackId, CaptureEvent)>,
    stopping: Arc<AtomicBool>,
}

impl<T> Capture<T> {
    /// The track the stream captures.
    #[must_use]
    pub const fn track(&self) -> TrackId {
        self.track
    }
}

impl<T> Drop for Capture<T> {
    fn drop(&mut self) {
        // The stream goes first, so nothing it sends comes after `Stopped`.
        // What it sends as it goes is still recorded, except a failure.
        self.stopping.store(true, Ordering::SeqCst);
        drop(self.stream.take());
        let _ = self.stopped.send((self.track, CaptureEvent::Stopped));
    }
}

/// Starts capturing `source` as `track` at `rate` through `backend`. The
/// stream's notices are stamped with `clock`, the session's.
///
/// # Errors
///
/// The backend's [`CaptureError`] if the stream can't be opened.
pub fn start<B: CaptureBackend>(
    backend: &B,
    track: TrackId,
    source: &Source,
    rate: SampleRate,
    clock: &Arc<dyn Clock>,
) -> Result<(Capture<B::Stream>, CaptureReceiver), CaptureError> {
    let (mut started, events) = start_tracks(backend, &[(track, source.clone())], rate, clock);
    match started.pop() {
        Some(Ok(capture)) => Ok((capture, events)),
        Some(Err(error)) => Err(error),
        // One source asked for, so one result.
        None => Err(CaptureError::Backend("no stream was started".into())),
    }
}

/// One stream [`start_tracks`] was asked for: running, or why it isn't.
pub type Started<T> = Result<Capture<T>, CaptureError>;

/// Starts capturing each `(track, source)` at `rate` through `backend`, all
/// into one [`CaptureReceiver`], for [`record_tracks`] on one recorder
/// thread. The streams' notices are stamped with `clock`, the session's.
///
/// Returns each stream's outcome, in the order given: a stream that can't
/// be opened doesn't stop the others, and the receiver expects only those
/// that started. A track listed twice isn't started again: that's an error
/// for its second entry.
pub fn start_tracks<B: CaptureBackend>(
    backend: &B,
    sources: &[(TrackId, Source)],
    rate: SampleRate,
    clock: &Arc<dyn Clock>,
) -> (Vec<Started<B::Stream>>, CaptureReceiver) {
    let (tx, rx) = mpsc::channel();
    let mut tracks = Vec::new();
    let mut started = Vec::new();
    for (track, source) in sources {
        if tracks.contains(track) {
            started.push(Err(CaptureError::Backend(format!(
                "track {} was asked for twice",
                track.get()
            ))));
            continue;
        }
        let stopping = Arc::new(AtomicBool::new(false));
        let sender = CaptureSender {
            events: tx.clone(),
            track: *track,
            clock: Arc::clone(clock),
            stopping: Arc::clone(&stopping),
        };
        let capture = backend.start(source, rate, sender).map(|stream| Capture {
            stream: Some(stream),
            track: *track,
            stopped: tx.clone(),
            stopping,
        });
        if capture.is_ok() {
            tracks.push(*track);
        }
        started.push(capture);
    }
    (
        started,
        CaptureReceiver {
            events: rx,
            rate,
            tracks,
        },
    )
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
    /// After an overrun, the track moved to this epoch, in its timeline and
    /// its journals: its samples from [`Epoch::first_sample`] on play from
    /// [`Epoch::start`]. Reported after the overrun's
    /// [`Capture`](Self::Capture).
    Epoch(Epoch),
    /// After an overrun, the timeline refused a new epoch, so the track
    /// stays in its current one: the samples after the overrun are timed
    /// early by the audio lost. Recording goes on.
    EpochRefused(EpochError),
    /// Audio the track recorded, as it was appended: for the engine, and
    /// for level meters. Its samples are numbered as the journals number
    /// them, and it never spans an epoch. Reported even if a journal broke
    /// on it, since the samples are numbered all the same (see
    /// [`Self::JournalFailed`]).
    Audio(AudioChunk),
    /// From [`record_tracks`] only: the track's stream failed and delivers
    /// nothing more. What it sent before is recorded, and the other tracks
    /// record on.
    CaptureFailed(CaptureError),
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
    /// The track's timeline isn't in the epoch the writer records it in,
    /// starting at the same sample and at the writer's rate, so its samples
    /// would be timed wrongly. Nothing is recorded.
    TimelineMismatch,
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
            Self::TimelineMismatch => f.write_str(
                "the track's timeline doesn't match the epoch or rate its journals are written in",
            ),
        }
    }
}

impl std::error::Error for RecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Capture(e) => Some(e),
            Self::Session(e) => Some(e),
            Self::RateMismatch { .. } | Self::TimelineMismatch => None,
        }
    }
}

/// Records `timeline`'s track from `events` into `writer` until the
/// capture stops, reporting finished journals, journal failures, the
/// stream's notices, new epochs and the audio recorded to `report` as they
/// happen. Run it on the recorder thread. Start the track on `writer`
/// first, and open the same epoch on `timeline`, at the session time the
/// stream was started.
///
/// At each overrun the track moves to a new epoch, in `timeline` and
/// `writer` alike, starting at the time the overrun was reported (see the
/// module docs), and the epoch is reported.
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
/// As [`record_tracks`], and [`RecordError::Capture`] if the stream failed.
pub fn record_track<S: Fs>(
    writer: &mut SessionWriter<S>,
    timeline: &mut TrackTimeline,
    events: &CaptureReceiver,
    report: &mut dyn FnMut(RecorderEvent),
) -> Result<(), RecordError> {
    let mut failed = None;
    record_tracks(
        writer,
        std::slice::from_mut(timeline),
        events,
        &mut |_track, event| match event {
            RecorderEvent::CaptureFailed(error) => {
                failed.get_or_insert(error);
            }
            event => report(event),
        },
    )?;
    failed.map_or(Ok(()), |error| Err(RecordError::Capture(error)))
}

/// Records every track `events` captures, each into its own journals on
/// `writer` and timed by its own timeline in `timelines`, until every
/// stream has stopped or failed. Reports what [`record_track`] reports, with
/// the track it's about (`None` for what concerns the session's journals as
/// a whole: journals that ended, which may be any track's, and a journal
/// that broke at an fsync), and a stream's failure as
/// [`RecorderEvent::CaptureFailed`]: the other tracks record on. Run it on
/// the recorder thread. Start each track on `writer` first, and open the
/// same epoch on its timeline, at the session time its stream was started.
///
/// After every event, from whichever track, every journal due an fsync is
/// fsync'd, and so are they all when nothing arrives for 100 ms: each
/// track's durable position stays within about
/// [`SYNC_INTERVAL`](crate::journal::SYNC_INTERVAL) of what it captured,
/// however busy or quiet the other tracks are.
///
/// # Errors
///
/// Before anything is recorded: [`RecordError::RateMismatch`] if the
/// streams and `writer` run at different rates;
/// [`RecordError::Session`] with [`SessionError::UnknownTrack`] for a
/// timeline whose track `writer` didn't start, or a stream with no
/// timeline; [`RecordError::TimelineMismatch`] unless each timeline's
/// current epoch is the one `writer` records its track in, from the same
/// first sample, at `writer`'s rate. While recording,
/// [`RecordError::Session`] if a track can't be recorded. A journal that
/// breaks isn't an error here: it's reported, and recording goes on.
pub fn record_tracks<S: Fs>(
    writer: &mut SessionWriter<S>,
    timelines: &mut [TrackTimeline],
    events: &CaptureReceiver,
    report: &mut dyn FnMut(Option<TrackId>, RecorderEvent),
) -> Result<(), RecordError> {
    if events.rate() != writer.rate() {
        return Err(RecordError::RateMismatch {
            capture: events.rate(),
            journal: writer.rate(),
        });
    }
    for timeline in timelines.iter() {
        let track = timeline.track();
        let Some((epoch, first_sample)) = writer.epoch(track) else {
            return Err(RecordError::Session(SessionError::UnknownTrack(track)));
        };
        let current = timeline
            .current()
            .map(|e| (e.id(), e.first_sample(), e.rate()));
        if current != Some((epoch, first_sample, writer.rate())) {
            return Err(RecordError::TimelineMismatch);
        }
    }
    if let Some(&track) = events
        .tracks()
        .iter()
        .find(|&&t| !timelines.iter().any(|timeline| timeline.track() == t))
    {
        return Err(RecordError::Session(SessionError::UnknownTrack(track)));
    }
    let mut live: BTreeSet<TrackId> = events.tracks().iter().copied().collect();
    while !live.is_empty() {
        let outcome = match events.next(IDLE_SYNC_CHECK) {
            Next::Event(track, event) => match handle(writer, timelines, track, event, report)? {
                Handled::Recorded(outcome) => outcome,
                Handled::Ended => {
                    live.remove(&track);
                    continue;
                }
            },
            Next::Idle => Ok(()),
            Next::Closed => return Ok(()),
        };
        settle(writer, outcome, report)?;
    }
    Ok(())
}

/// What became of one event.
enum Handled {
    /// It was recorded, with this outcome from the writer.
    Recorded(Result<(), (TrackId, SessionError)>),
    /// The track's stream stopped or failed: nothing more comes from it.
    Ended,
}

/// Records one `event` from `track`'s stream: audio appended to its
/// journal and reported, an overrun turned into a new epoch, a failure
/// reported. The writer's error, if any, comes back for [`settle`].
fn handle<S: Fs>(
    writer: &mut SessionWriter<S>,
    timelines: &mut [TrackTimeline],
    track: TrackId,
    event: CaptureEvent,
    report: &mut dyn FnMut(Option<TrackId>, RecorderEvent),
) -> Result<Handled, RecordError> {
    let Some(timeline) = timelines.iter_mut().find(|t| t.track() == track) else {
        return Err(RecordError::Session(SessionError::UnknownTrack(track)));
    };
    let outcome = match event {
        CaptureEvent::Audio(samples) => {
            let first = writer
                .next_sample(track)
                .ok_or(SessionError::UnknownTrack(track))
                .map_err(RecordError::Session)?;
            let appended = writer.append(track, &samples);
            if let Some(chunk) = AudioChunk::new(track, first, writer.rate(), samples) {
                report(Some(track), RecorderEvent::Audio(chunk));
            }
            appended
        }
        CaptureEvent::Notice { notice, at } => {
            let lost = notice == CaptureNotice::Overrun;
            report(Some(track), RecorderEvent::Capture(notice));
            if lost {
                open_epoch_after_loss(writer, timeline, at, &mut |e| report(Some(track), e))
            } else {
                Ok(())
            }
        }
        CaptureEvent::Failed(error) => {
            report(Some(track), RecorderEvent::CaptureFailed(error));
            return Ok(Handled::Ended);
        }
        CaptureEvent::Stopped => return Ok(Handled::Ended),
    };
    Ok(Handled::Recorded(outcome.map_err(|e| (track, e))))
}

/// After an event: fsyncs every journal due one, reports a broken journal
/// (from the event's `outcome`, with its track, or from the fsync) and the
/// journals that ended. Any other writer error stops recording.
fn settle<S: Fs>(
    writer: &mut SessionWriter<S>,
    outcome: Result<(), (TrackId, SessionError)>,
    report: &mut dyn FnMut(Option<TrackId>, RecorderEvent),
) -> Result<(), RecordError> {
    let synced = writer.sync_if_due().map_err(|e| (None, e));
    let outcome = outcome.map_err(|(track, e)| (Some(track), e));
    for result in [outcome, synced] {
        match result {
            Ok(()) => {}
            Err((track, error @ (SessionError::Journal(_) | SessionError::Marks(_)))) => {
                report(track, RecorderEvent::JournalFailed(error));
            }
            Err((_, error)) => return Err(RecordError::Session(error)),
        }
    }
    let finished = writer.take_finished();
    if !finished.is_empty() {
        report(None, RecorderEvent::Finished(finished));
    }
    Ok(())
}

/// Moves `timeline`'s track to a new epoch starting at `at`, in `timeline`
/// and `writer` alike, after audio was lost: the samples that follow play
/// from `at`, and the loss is a gap. The new epoch is reported, or the
/// timeline's refusal, with the track left in its epoch.
///
/// A [`SessionError::Journal`] or [`SessionError::Marks`] from ending the
/// old epoch's journal comes back with the track moved all the same.
fn open_epoch_after_loss<S: Fs>(
    writer: &mut SessionWriter<S>,
    timeline: &mut TrackTimeline,
    at: SessionTime,
    report: &mut dyn FnMut(RecorderEvent),
) -> Result<(), SessionError> {
    let track = timeline.track();
    let next = writer
        .next_sample(track)
        .ok_or(SessionError::UnknownTrack(track))?;
    let opened = match timeline.open_epoch(at, next, writer.rate()) {
        Ok(opened) => opened,
        Err(refused) => {
            report(RecorderEvent::EpochRefused(refused));
            return Ok(());
        }
    };
    // The writer is in the timeline's previous epoch (checked when
    // recording started, and kept in step since), so it takes the next.
    let moved = writer.new_epoch(track, opened.id);
    if let Some(&epoch) = timeline.current() {
        report(RecorderEvent::Epoch(epoch));
    }
    moved
}
