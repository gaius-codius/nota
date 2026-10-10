//! Capturing a track's audio and recording it into the journal.
//!
//! A [`CaptureBackend`] opens a stream on a [`Source`] and delivers mono
//! 16-bit samples at the requested rate as [`CaptureEvent`]s on a channel.
//! The stream's callback only copies the samples into the channel; it never
//! touches the disk or waits on the recorder, so an fsync that takes a
//! second can't make the audio server drop audio. [`record_track`] runs on
//! the recorder thread, takes the samples off the channel and appends them
//! to the [`SessionWriter`], which does every
//! write and fsync.
//!
//! The channel is unbounded: while the disk stalls, audio queues in memory
//! rather than being dropped, and the recorder catches up when the disk
//! does. Each buffer queued holds at least 8 KB whatever the stream's
//! quantum, so a stall costs about 0.2 to 0.4 MB a second per track, not
//! the 32 KB a second of the audio itself. In the steady state the
//! callback doesn't allocate: the channel has room reserved, and the
//! recorder hands each sample buffer back for the callback to fill again. Each track's
//! [`Progress`] counts the audio waiting in the channel, so what a crash
//! would lose can be measured from the audio the server delivered, not
//! only from what reached the journal.
//!
//! # Lost audio and drift
//!
//! When the audio server overruns the stream's buffer, the audio it lost
//! never arrives, so the track's sample count falls behind session time.
//! The track then moves to a new epoch, in its [`TrackTimeline`] and in its
//! journals alike: the samples after the loss are timed from when they were
//! captured, not early by the audio lost, and the loss shows as a gap
//! between the two epochs.
//!
//! A stream that stamps its buffers ([`CaptureSender::audio_captured`], as
//! [`PipeWireBackend`] does with the audio server's timestamps) is timed by
//! those stamps, read through each track's [`DriftMeter`]:
//! - A buffer captured at least [`LOSS_MIN`](nota_core::drift::LOSS_MIN)
//!   later than the buffer before it puts it opens a new epoch where it
//!   came, whether or not an overrun was reported (cpal drops a
//!   report it can't deliver at once). An overrun after which the stream's
//!   next buffer is on time lost nothing on this stream (the server reports
//!   overruns of its whole graph), and opens nothing. Buffers are compared
//!   by the stream's cycle times, so a step in the delay the stream takes
//!   off them, as when the graph's quantum changes, isn't taken for a loss
//!   (see [`nota_core::drift`]).
//! - Losses come in bursts under I/O pressure, and each epoch costs a
//!   journal. Within [`BURST_WINDOW`] of the last epoch opened for a loss,
//!   no other opens: the audio after a second loss is timed early by it
//!   until the window has passed, and the first buffer after that opens
//!   one epoch for all of them. A burst of overruns within the window so
//!   opens two epochs, and two journals.
//! - Otherwise the stamps measure the device's drift, and the meter
//!   retimes the track when its mapping strays (see [`nota_core::drift`]),
//!   so a loss still shows as a gap however long the track has drifted.
//!   For [`SETTLE`](nota_core::drift::SETTLE) after a track's first buffer,
//!   and after each epoch opened for a loss or a reopening, the stamps
//!   settle, and the meter only watches for a loss.
//!
//! A stream that doesn't stamp its buffers opens an epoch at each overrun,
//! at the session time the overrun was reported on the stream's thread. It
//! has no drift measured. That time is approximate: the audio that follows
//! was captured up to a buffer earlier, a stream that lost nothing still
//! gets an epoch, and if the device's clock has run fast by more than the
//! audio lost, the timeline takes the overrun as drift
//! ([`TrackTimeline::open_epoch`]) and leaves no gap.
//!
//! # Route changes and suspend
//!
//! A stream that follows the default device goes quiet while it moves to a
//! new one, and every stream goes quiet while the machine is suspended, yet
//! the track's sample count carries on as if nothing was missed. So the
//! first audio after either opens a new epoch, at the time that audio was
//! captured ([`CaptureEvent::Reopened`]): the stretch with no audio is a
//! gap between epochs, as it is after an overrun. The stream's sender
//! notices a suspend by the session clock's count of time spent suspended
//! ([`Clock::suspended`]), read on each buffer, and reports it as a
//! [`CaptureNotice::Suspended`].
//!
//! The audio server may also report an overrun for the same stretch, just
//! before that audio. From a stream that doesn't stamp its buffers, that
//! overrun's epoch, opened when it was reported and holding no audio yet,
//! already makes the stretch a gap, so a reopening timed before it opens
//! no second one: the audio is timed from the overrun, late by up to a
//! buffer, as after any overrun. A reopening timed after it (the stream
//! stalled on after the overrun) opens its own.
//!
//! # Resumed sessions
//!
//! A track that joins a session recorded before carries on from its newest
//! epoch ([`SessionWriter::resumed_timeline`]): its first epoch is numbered
//! above every earlier one, and must start after the earlier audio ends,
//! which takes the session's clock resumed from
//! [`SessionWriter::resume_from`].
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
//! [`SessionWriter`]: each track in its own
//! journals, epochs and segments, timed by its own [`TrackTimeline`]. A
//! track whose stream fails is reported and the others record on; the
//! recorder returns once every stream has stopped or failed. Every
//! journal's fsync is checked after every event, whichever track it came
//! from, after a stream ends, and while no audio arrives.
//!
//! Opening a stream can take seconds (cpal waits about 4 s for an audio
//! server that accepts the connection and doesn't answer), so `nota
//! record` starts the recorder first: [`prepare_tracks`] gives the
//! receiver before any stream opens, [`record_tracks`] runs on it, and
//! the [`TrackStarter`] then opens the streams one after another. Each
//! track joins the recorder as its stream starts, so its audio is
//! journaled while the next stream opens, and its first epoch opens when
//! its first audio was captured ([`CaptureEvent::Began`]): the moment a
//! stream was asked to start is early by its start-up latency, by a
//! different amount on each track.
//!
//! # Detectors
//!
//! [`record_tracks`] runs the three detectors of [`crate::detect`] for
//! each track and reports what they raise or clear as
//! [`RecorderEvent::Detected`], with the track:
//! - **Digital zeros and quiet** read the samples as they're recorded,
//!   after the [`RecorderEvent::Audio`] they came in. A change is timed by
//!   the track's timeline, so it's at the session time of the sample it
//!   began or ended at.
//! - **Stalled** reads the track's delivered count ([`Progress::now`]),
//!   which the stream's thread moves, after every event and while the
//!   recorder is idle. A recorder that's behind on its queue therefore
//!   doesn't stall a track whose audio is waiting there. It's watched from
//!   the stream's [`CaptureEvent::Started`], so a stream still opening
//!   isn't stalled, and time spent suspended doesn't count.
//!
//! A track's detectors end with its stream, clearing what they raised. The
//! thresholds come from the track's [`Source`] ([`prepare_tracks`]), or
//! from [`CaptureReceiver::set_thresholds`].
//!
//! # The bound
//!
//! A track's durable position trails the audio the server delivered by at
//! most [`SYNC_INTERVAL`](crate::journal::SYNC_INTERVAL) (850 ms), the
//! stream's buffering (about 40 ms) and its own journal's fsync, so the
//! bounded-loss rule, about 2 s on a quiet disk, holds while that fsync
//! stays under about 1.1 s (and under about 1.95 s together with the one
//! before it, which held it back if it overran the interval).
//!
//! Which fsyncs count depends on where they run. `nota record` runs them
//! as [`Syncing::Auto`](crate::session::Syncing::Auto) does:
//! - **One track: inline**
//!   ([`Syncing::Inline`](crate::session::Syncing::Inline)), on the
//!   recorder thread. Only the track's own fsync holds its audio back, so
//!   the bound above holds as it stands, on the path the crash tests sweep
//!   exhaustively.
//! - **Two or more: a thread per track**
//!   ([`Syncing::Threads`](crate::session::Syncing::Threads)). The
//!   recorder thread never waits on an fsync, and a track whose fsync is
//!   slow holds only its own audio back, in memory, until it completes:
//!   the bound holds for each track whatever the others do. Inline, a
//!   track's audio would also wait in the channel while the other tracks'
//!   journals are fsync'd, and the rule would hold only while all their
//!   fsyncs together stayed under about 1.1 s.
//!
//! Creating a journal, at each window boundary, still
//! fsyncs the new file and its directory on the recorder thread, once per
//! track per segment window (five minutes by default).
//!
//! On Linux, [`PipeWireBackend`] captures through cpal's `PipeWire` host. It
//! links `libpipewire-0.3`, `libasound` (cpal's ALSA host is always built
//! on Linux) and `libdbus-1` (to ask rtkit for real-time priority)
//! dynamically, and building it needs their development headers,
//! `pkg-config`, and clang (libclang, for bindgen):
//! `libpipewire-0.3-dev libasound2-dev libdbus-1-dev libclang-dev
//! pkg-config` on Debian and Ubuntu, `pipewire alsa-lib dbus clang pkgconf`
//! on Arch. The stream's thread runs at real-time priority where the
//! system allows it (see [`PipeWireBackend`]).
//! Other platforms build without it; they record nothing until v2.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use std::collections::{BTreeMap, BTreeSet};

use nota_core::drift::Reading;
use nota_core::messages::AudioChunk;
use nota_core::recorder::{DeviceChange, WarningState};
use nota_core::{
    Clock, Drift, DriftMeter, Epoch, EpochError, OpenedEpoch, SampleCount, SampleIndex, SampleRate,
    SessionTime, Stamp, TrackId, TrackTimeline,
};

use crate::detect::{Condition, Thresholds};
use crate::fs::Fs;
use crate::session::{FinishedJournal, SessionError, SessionWriter};

#[cfg(test)]
mod detect_tests;
mod detectors;
mod devices;
#[cfg(target_os = "linux")]
mod pipewire;
mod preview;
mod queue;
// The route is followed by the PipeWire backend's watch; other platforms
// have no backend yet, and nothing there would use it.
#[cfg(target_os = "linux")]
mod route;
#[cfg(test)]
mod stop_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tracks_tests;

pub use devices::{Device, Devices};
#[cfg(target_os = "linux")]
pub use pipewire::PipeWireBackend;
pub use preview::{Preview, PreviewEvent};
pub use queue::{Positions, Progress};

use detectors::Detectors;
use queue::{Queue, QueueSender, Received};

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
    /// The audio server overran the stream's buffer: audio may have been
    /// lost, and the track's sample count fallen behind session time by
    /// that much. [`record_track`] moves the track to a new epoch, so the
    /// loss is a gap, unless the timeline refuses one; from a stream that
    /// stamps its buffers, only if its next buffer shows a loss (see the
    /// module docs).
    Overrun,
    /// The default device changed and the stream followed it. Its next
    /// audio opens a new epoch ([`CaptureEvent::Reopened`]).
    RouteChanged,
    /// The machine was suspended while the stream ran. Reported with the
    /// first audio after it, which opens a new epoch
    /// ([`CaptureEvent::Reopened`]).
    Suspended,
    /// The track's device clock was measured running this far from the
    /// session clock: past [`DRIFT_LIMIT`](nota_core::drift::DRIFT_LIMIT),
    /// more than a working device's. Its audio is still timed by the drift
    /// measured. Reported once per track, by the recorder rather than the
    /// stream.
    Drifted(Drift),
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
    /// The next samples, as [`Self::Audio`], from a stream that stamps its
    /// buffers.
    TimedAudio {
        /// The samples.
        samples: Vec<i16>,
        /// When the first of them was captured, by the stream's stamp, on
        /// the session clock, and the stream's delay.
        stamp: Stamp,
    },
    /// Something to note; capture goes on.
    Notice {
        /// What happened.
        notice: CaptureNotice,
        /// When it was reported, by the session clock.
        at: SessionTime,
    },
    /// Something happened to the stream's device, as the backend's watch
    /// on the audio server saw it; capture goes on, unless a
    /// [`Self::Failed`] follows.
    Device {
        /// What happened.
        change: DeviceChange,
        /// When it was noticed, by the session clock.
        at: SessionTime,
    },
    /// The stream failed and delivers nothing more.
    Failed(CaptureError),
    /// The stream was stopped; nothing follows.
    Stopped,
    /// The stream's first audio follows. Its first sample was captured at
    /// about `at`: when the first buffer arrived, less the time its samples
    /// span. A track that joins the recorder opens its first epoch here.
    Began {
        /// When the first sample was captured, by the session clock.
        at: SessionTime,
    },
    /// The stream's audio resumes after a stretch it didn't capture: it
    /// followed a new device, or the machine was suspended. The next
    /// sample was captured at about `at`, and opens a new epoch there.
    Reopened {
        /// When the next sample was captured, by the session clock.
        at: SessionTime,
    },
    /// The stream's start returned: what it sent before, and sends from
    /// now on, is the track's.
    Started,
    /// The stream couldn't be opened: what it sent is dropped, and nothing
    /// follows.
    NotStarted,
}

/// The stream's end of the channel, given to [`CaptureBackend::start`]. It
/// never waits on the recorder: it holds the channel's lock only to copy
/// and queue what it sends.
#[derive(Debug, Clone)]
pub struct CaptureSender {
    events: QueueSender,
    /// The track the stream captures.
    track: TrackId,
    /// Counts the samples sent and not yet recorded.
    progress: Progress,
    /// Stamps notices as they're reported.
    clock: Arc<dyn Clock>,
    /// Set once the capture is being stopped.
    stopping: Arc<AtomicBool>,
    /// Set once the stream's failure is sent: it sends no audio after.
    failed: Arc<AtomicBool>,
    /// Set once the device's loss is sent, until another device is.
    lost: Arc<AtomicBool>,
    /// Set once the first audio is sent.
    began: Arc<AtomicBool>,
    /// Set when the stream followed a new device, until its next audio.
    rerouted: Arc<AtomicBool>,
    /// The clock's count of time suspended when the last audio was sent,
    /// in nanoseconds.
    asleep: Arc<AtomicU64>,
    /// The rate the stream captures at, to time the first audio.
    rate: SampleRate,
}

/// The least growth in the clock's count of time suspended taken for a
/// suspend. The count is two clocks read a moment apart, so it can move by
/// the time between the reads, which preemption can make milliseconds;
/// a real suspend lasts seconds.
const MIN_SUSPEND: Duration = Duration::from_millis(100);

impl CaptureSender {
    /// Sends a copy of `samples`, in a buffer the recorder has finished
    /// with if there is one. Empty calls send nothing. The first call that
    /// sends anything says first when its first sample was captured
    /// ([`CaptureEvent::Began`]); a later one after a route change or a
    /// suspend says so, and when ([`CaptureEvent::Reopened`]). Those times
    /// are when the call came, less the time the samples span.
    pub fn audio(&self, samples: &[i16]) {
        self.send_audio(samples, None);
    }

    /// [`Self::audio`], for a stream that stamps its buffers: the first of
    /// `samples` was captured at `captured`, on the system's clock that
    /// stops during suspend (`CLOCK_MONOTONIC`, as `PipeWire` stamps them),
    /// `delay` before the stream's cycle that delivered them. The samples
    /// are timed by the stamp, and so are a first buffer and one after a
    /// gap; one the session clock can't place is sent as [`Self::audio`]
    /// sends it.
    pub fn audio_captured(&self, samples: &[i16], captured: Duration, delay: Duration) {
        if samples.is_empty() {
            return;
        }
        let stamp = self
            .clock
            .awake_to_session(captured)
            .map(|at| Stamp { at, delay });
        self.send_audio(samples, stamp);
    }

    /// Sends `samples`, stamped `captured` if the stream gave a time the
    /// session clock could place.
    fn send_audio(&self, samples: &[i16], captured: Option<Stamp>) {
        // After a failure the track has ended: audio the stream still
        // delivers, from a device it was moved to say, isn't the track's.
        if samples.is_empty() || self.failed.load(Ordering::SeqCst) {
            return;
        }
        let began = self.began.load(Ordering::SeqCst);
        // Checked on every buffer, the first too, so a suspend before it
        // isn't taken for one after it.
        let reopened = self.reopened();
        // Without a stamp, the clock is read only when a time is needed: a
        // stream's first buffers, and its first after a gap.
        let at = (!began || reopened)
            .then(|| captured.map_or_else(|| self.captured_at(samples.len()), |s| s.at));
        let first = at.filter(|_| !began).map(|at| (&*self.began, at));
        // Counted before it's queued, so it's never missed.
        self.progress.sent(samples.len());
        self.events.first_audio(
            self.track,
            samples,
            first,
            at.filter(|_| reopened),
            captured,
        );
    }

    /// Whether the audio about to be sent follows a stretch the stream
    /// didn't capture: it followed a new device since its last audio, or
    /// the machine was suspended since. A suspend is reported here, as a
    /// [`CaptureNotice::Suspended`].
    fn reopened(&self) -> bool {
        let asleep = u64::try_from(self.clock.suspended().as_nanos()).unwrap_or(u64::MAX);
        let before = self.asleep.swap(asleep, Ordering::SeqCst);
        let slept = Duration::from_nanos(asleep.saturating_sub(before)) >= MIN_SUSPEND;
        if slept {
            self.notice(CaptureNotice::Suspended);
        }
        let rerouted = self.rerouted.swap(false, Ordering::SeqCst);
        slept || rerouted
    }

    /// When the first of `len` samples just delivered was captured: now,
    /// less the time they span.
    fn captured_at(&self, len: usize) -> SessionTime {
        let now = self.clock.now();
        let span = SampleCount::new(len as u64)
            .duration_at(self.rate)
            .unwrap_or_default();
        SessionTime::from_elapsed(now.elapsed().saturating_sub(span)).unwrap_or(now)
    }

    /// Reports something that doesn't stop the stream, stamped with the
    /// session time now. After a [`CaptureNotice::RouteChanged`], the
    /// stream's next audio opens a new epoch.
    pub fn notice(&self, notice: CaptureNotice) {
        if notice == CaptureNotice::RouteChanged {
            self.rerouted.store(true, Ordering::SeqCst);
        }
        let at = self.clock.now();
        self.events
            .send(self.track, CaptureEvent::Notice { notice, at });
    }

    /// Reports something that happened to the stream's device, stamped
    /// with the session time now. Nothing is sent once the stream has
    /// failed (its track has ended), and a loss only once until another
    /// device follows.
    pub fn device(&self, change: DeviceChange) {
        self.device_at(change, self.clock.now());
    }

    /// [`Self::device`], for a change that happened at `at`, before it
    /// was reported: a default that went missing and is lost once its
    /// grace has run out.
    fn device_at(&self, change: DeviceChange, at: SessionTime) {
        if self.failed.load(Ordering::SeqCst) {
            return;
        }
        let lost = change == DeviceChange::Lost;
        if self.lost.swap(lost, Ordering::SeqCst) && lost {
            return;
        }
        self.send_device(change, at);
    }

    /// Sends `change`, which happened at `at`.
    fn send_device(&self, change: DeviceChange, at: SessionTime) {
        self.events
            .send(self.track, CaptureEvent::Device { change, at });
    }

    /// Reports that the stream failed, unless the capture is being stopped:
    /// a stream torn down on purpose may report that as a failure (cpal
    /// 0.18's `PipeWire` host says a named device disconnected). Only the
    /// first failure is sent, and no audio after it. A failure because the
    /// device went away is told as [`DeviceChange::Lost`] first, unless the
    /// loss was already sent: whichever notices it, the stream or a watch
    /// on the audio server, the track's loss is on its timeline before it
    /// ends.
    pub fn failed(&self, error: CaptureError) {
        if self.stopping.load(Ordering::SeqCst) || self.failed.swap(true, Ordering::SeqCst) {
            return;
        }
        if matches!(error, CaptureError::DeviceNotAvailable(_))
            && !self.lost.swap(true, Ordering::SeqCst)
        {
            self.send_device(DeviceChange::Lost, self.clock.now());
        }
        self.events.send(self.track, CaptureEvent::Failed(error));
    }
}

/// The recorder thread's end of the channel: the events of every stream
/// started into it, each with its track, and the rate they capture at.
#[derive(Debug)]
pub struct CaptureReceiver {
    events: Arc<Queue>,
    rate: SampleRate,
    /// The tracks asked for, in order, with their progress.
    tracks: Vec<(TrackId, Progress)>,
    /// The session clock, which the detectors read.
    clock: Arc<dyn Clock>,
    /// What each track's detectors hold it to. A track with none here is
    /// held to [`Thresholds::MICROPHONE`].
    thresholds: BTreeMap<TrackId, Thresholds>,
}

impl Drop for CaptureReceiver {
    /// Streams still running send into nothing from now on, as with a
    /// dropped `mpsc` receiver.
    fn drop(&mut self) {
        self.events.close();
    }
}

impl CaptureReceiver {
    /// The rate the streams capture at.
    #[must_use]
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The tracks asked for, each once, in the order they were asked for:
    /// those whose streams start, and those whose streams can't.
    #[must_use]
    pub fn tracks(&self) -> Vec<TrackId> {
        self.tracks.iter().map(|(track, _)| *track).collect()
    }

    /// How far `track`'s audio has got: delivered by the server, captured
    /// into its journals, durable. Readable from any thread while
    /// [`record_tracks`] runs, which keeps it up to date. `None` if the
    /// track wasn't asked for; a track whose stream didn't start stays at
    /// zero.
    #[must_use]
    pub fn progress(&self, track: TrackId) -> Option<Progress> {
        self.tracks
            .iter()
            .find(|(t, _)| *t == track)
            .map(|(_, progress)| progress.clone())
    }

    /// Holds `track`'s detectors to `thresholds`, for a caller that knows
    /// more than its source says: a pinned device that's the system audio
    /// ([`Thresholds::SYSTEM_AUDIO`]). Do it before [`record_tracks`] runs.
    /// Nothing happens for a track that wasn't asked for.
    pub fn set_thresholds(&mut self, track: TrackId, thresholds: Thresholds) {
        if self.tracks.iter().any(|(t, _)| *t == track) {
            self.thresholds.insert(track, thresholds);
        }
    }

    /// The next event, waiting at most `timeout`.
    fn next(&self, timeout: Duration) -> Received {
        self.events.next(timeout)
    }
}

/// A queue for tests that build their [`CaptureReceiver`] by hand: its
/// first sender, and the queue.
#[cfg(test)]
fn test_channel() -> (QueueSender, Arc<Queue>) {
    let (queue, sender) = Queue::new();
    (sender, queue)
}

/// `tracks`, each with a fresh [`Progress`], for a [`CaptureReceiver`]
/// built by hand.
#[cfg(test)]
fn test_tracks(tracks: &[TrackId]) -> Vec<(TrackId, Progress)> {
    tracks
        .iter()
        .map(|&t| (t, Progress::new(SampleIndex::ZERO, SampleIndex::ZERO)))
        .collect()
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

    /// The devices the audio server has now, for Setup to offer.
    ///
    /// A backend with no server to ask can't list any, which is what this
    /// says unless it's overridden.
    ///
    /// # Errors
    ///
    /// A [`CaptureError`] if the server can't be asked.
    fn devices(&self) -> Result<Devices, CaptureError> {
        Err(CaptureError::Backend(
            "this backend can't list devices".to_owned(),
        ))
    }
}

/// A running capture. Dropping it stops the stream, then tells the
/// recorder, which records what the stream sent before stopping and then
/// returns.
#[derive(Debug)]
pub struct Capture<T> {
    stream: Option<T>,
    track: TrackId,
    started_at: SessionTime,
    stopped: QueueSender,
    stopping: Arc<AtomicBool>,
}

impl<T> Capture<T> {
    /// The track the stream captures.
    #[must_use]
    pub const fn track(&self) -> TrackId {
        self.track
    }

    /// When the stream was started, by the session clock: read just before
    /// it was opened, so no audio it delivers can come from earlier. A
    /// track started first opens its first epoch here, early by the
    /// stream's start-up latency, and by any suspend or route change before
    /// its first audio; a track that joins the recorder opens it at its
    /// first audio instead ([`CaptureEvent::Began`]), as `nota record`'s
    /// tracks do.
    #[must_use]
    pub const fn started_at(&self) -> SessionTime {
        self.started_at
    }
}

impl<T> Drop for Capture<T> {
    fn drop(&mut self) {
        // The stream goes first, so nothing it sends comes after `Stopped`.
        // What it sends as it goes is still recorded, except a failure.
        self.stopping.store(true, Ordering::SeqCst);
        drop(self.stream.take());
        self.stopped.send(self.track, CaptureEvent::Stopped);
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
/// be opened doesn't stop the others. A track listed twice isn't started
/// again: that's an error for its second entry.
///
/// The streams start before anything records them, so the first one's
/// audio waits in memory while the others open. To record each track as
/// soon as its stream starts, take the receiver from [`prepare_tracks`],
/// start the recorder on it, then start the streams.
pub fn start_tracks<B: CaptureBackend>(
    backend: &B,
    sources: &[(TrackId, Source)],
    rate: SampleRate,
    clock: &Arc<dyn Clock>,
) -> (Vec<Started<B::Stream>>, CaptureReceiver) {
    let (starter, events) = prepare_tracks(sources, rate, clock);
    (starter.start(backend), events)
}

/// The streams of [`prepare_tracks`], to start once the recorder runs.
#[derive(Debug)]
pub struct TrackStarter {
    sources: Vec<(TrackId, Source)>,
    rate: SampleRate,
    clock: Arc<dyn Clock>,
    /// The receiver's tracks, each with its progress, for its sender.
    tracks: Vec<(TrackId, Progress)>,
    /// Sends each stream's start, or its failure; dropped once every
    /// stream is started.
    events: QueueSender,
}

/// The receiver for capturing each `(track, source)` at `rate`, before any
/// stream starts, and the [`TrackStarter`] that starts them. Start
/// [`record_tracks`] on the receiver first: each track then joins it as
/// its stream starts, its first epoch opened at its first audio, while
/// the next stream opens.
///
/// Each track's [`Progress`] starts at sample zero, as the writer starts a
/// new session's tracks.
#[must_use]
pub fn prepare_tracks(
    sources: &[(TrackId, Source)],
    rate: SampleRate,
    clock: &Arc<dyn Clock>,
) -> (TrackStarter, CaptureReceiver) {
    let (queue, events) = Queue::new();
    let mut tracks: Vec<(TrackId, Progress)> = Vec::new();
    let mut thresholds = BTreeMap::new();
    for (track, source) in sources {
        if !tracks.iter().any(|(t, _)| t == track) {
            tracks.push((*track, Progress::new(SampleIndex::ZERO, SampleIndex::ZERO)));
            thresholds.insert(*track, thresholds_for(source));
        }
    }
    let starter = TrackStarter {
        sources: sources.to_vec(),
        rate,
        clock: Arc::clone(clock),
        tracks: tracks.clone(),
        events,
    };
    let receiver = CaptureReceiver {
        events: queue,
        rate,
        tracks,
        clock: Arc::clone(clock),
        thresholds,
    };
    (starter, receiver)
}

/// What the detectors hold a track on `source` to. A pinned device of no
/// known kind gets the microphone's, which warns sooner.
const fn thresholds_for(source: &Source) -> Thresholds {
    match source {
        Source::SystemAudio => Thresholds::SYSTEM_AUDIO,
        Source::Microphone | Source::Device(_) => Thresholds::MICROPHONE,
    }
}

impl TrackStarter {
    /// Starts each stream through `backend`, one after another, and tells
    /// the receiver of each start or failure as it returns. Returns each
    /// stream's outcome, in the order given (see [`start_tracks`]).
    pub fn start<B: CaptureBackend>(self, backend: &B) -> Vec<Started<B::Stream>> {
        let mut started = Vec::new();
        let mut asked: Vec<TrackId> = Vec::new();
        for (track, source) in &self.sources {
            if asked.contains(track) {
                started.push(Err(CaptureError::Backend(format!(
                    "track {} was asked for twice",
                    track.get()
                ))));
                continue;
            }
            asked.push(*track);
            let capture = self.start_one(backend, *track, source);
            let told = if capture.is_ok() {
                CaptureEvent::Started
            } else {
                CaptureEvent::NotStarted
            };
            self.events.send(*track, told);
            started.push(capture);
        }
        started
    }

    /// Starts `track`'s stream on `source`.
    fn start_one<B: CaptureBackend>(
        &self,
        backend: &B,
        track: TrackId,
        source: &Source,
    ) -> Started<B::Stream> {
        let progress = self
            .tracks
            .iter()
            .find(|(t, _)| *t == track)
            .map(|(_, progress)| progress.clone())
            .ok_or_else(|| {
                CaptureError::Backend(format!("track {} wasn't asked for", track.get()))
            })?;
        let stopping = Arc::new(AtomicBool::new(false));
        let sender = CaptureSender {
            events: self.events.clone(),
            track,
            progress,
            clock: Arc::clone(&self.clock),
            stopping: Arc::clone(&stopping),
            failed: Arc::new(AtomicBool::new(false)),
            lost: Arc::new(AtomicBool::new(false)),
            began: Arc::new(AtomicBool::new(false)),
            rerouted: Arc::new(AtomicBool::new(false)),
            // Suspends before the stream starts aren't its to report.
            asleep: Arc::new(AtomicU64::new(
                u64::try_from(self.clock.suspended().as_nanos()).unwrap_or(u64::MAX),
            )),
            rate: self.rate,
        };
        let started_at = self.clock.now();
        backend
            .start(source, self.rate, sender)
            .map(|stream| Capture {
                stream: Some(stream),
                track,
                started_at,
                stopped: self.events.clone(),
                stopping,
            })
    }
}

/// How long the recorder waits for audio before checking whether a journal
/// is due an fsync anyway.
const IDLE_SYNC_CHECK: Duration = Duration::from_millis(100);

/// How long after one epoch opened for lost audio no other opens for a
/// loss: 2 s. A burst of overruns then costs about one epoch, and one
/// journal, a window, at the price of timing the audio within the burst
/// early by the losses since the window opened (see the module docs).
pub const BURST_WINDOW: Duration = Duration::from_secs(2);

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
    /// After lost audio, a route change or a suspend, or to correct drift,
    /// the track moved to this epoch, in its timeline and its journals: its
    /// samples from [`Epoch::first_sample`] on play from [`Epoch::start`].
    /// Reported after an unstamped stream's overrun's
    /// [`Capture`](Self::Capture), and before the first audio after a loss,
    /// a route change or a suspend, or the audio it retimes. From
    /// [`record_tracks`], also a joining track's first epoch, before its
    /// first audio.
    Epoch(Epoch),
    /// After lost audio, a route change or a suspend, or to correct drift,
    /// the timeline refused a new epoch, so the track stays in its current
    /// one: the samples after it are timed early by the stretch missed, or
    /// at the drift it had. Recording goes on.
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
    /// Something happened to the track's device: it went away, or the
    /// default the track follows changed ([`CaptureEvent::Device`]). A
    /// device lost this way may be followed by
    /// [`CaptureFailed`](Self::CaptureFailed).
    Device {
        /// What happened.
        change: DeviceChange,
        /// When it was noticed, by the session clock.
        at: SessionTime,
    },
    /// One of the track's detectors ([`crate::detect`]) raised or cleared
    /// its condition.
    Detected {
        /// What it's about.
        condition: Condition,
        /// Whether it's raised or cleared.
        state: WarningState,
        /// If raised, when the condition began; if cleared, when it ended.
        at: SessionTime,
    },
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
/// After lost audio the track moves to a new epoch, in `timeline` and
/// `writer` alike, starting when the audio after the loss was captured, or
/// for a stream that doesn't stamp its buffers when the overrun was
/// reported, and the epoch is reported (see the module docs). So it does
/// at the first audio after a route change or a suspend, starting when
/// that audio was captured, and when a stamped stream's drift needs
/// correcting.
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
/// meanwhile waits in memory, not yet captured, and is lost if the process
/// dies; the track's [`Progress`] counts it as delivered.
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
/// `writer` and timed by its own timeline, until every stream has stopped,
/// failed or not started. Reports what [`record_track`] reports, with the
/// track it's about (`None` for what concerns the session's journals as a
/// whole: journals that ended, which may be any track's, and a journal
/// that broke at an fsync), and a stream's failure as
/// [`RecorderEvent::CaptureFailed`]: the other tracks record on. Run it on
/// the recorder thread.
///
/// A track is recorded one of two ways:
/// - **Started first:** start it on `writer` and give its timeline in
///   `timelines`, with the same epoch opened at the session time its
///   stream was started. Its events are recorded as they come.
/// - **Joining:** a track `events` was prepared for ([`prepare_tracks`])
///   with no timeline here joins once its stream has started and its first
///   audio has come: its timeline carries on from its earlier recordings
///   ([`SessionWriter::resumed_timeline`]), and it's started on `writer`
///   at [`SessionWriter::first_free_sample`], with its first epoch opened
///   at that audio's time ([`CaptureEvent::Began`]) and reported as a
///   [`RecorderEvent::Epoch`]. Until its start returns, its events wait
///   in memory; if it doesn't start, they're dropped, and so is anything
///   it sends later. A track that can't join (its first epoch would start
///   before its earlier audio ends, as when the session's clock wasn't
///   resumed) is reported as [`RecorderEvent::CaptureFailed`], and the
///   others record on.
///
/// After every event, from whichever track, and after a stream ends,
/// every journal due an fsync has one started, and so do they all when
/// nothing arrives for 100 ms: each track's durable position stays within
/// about [`SYNC_INTERVAL`](crate::journal::SYNC_INTERVAL) of what it
/// captured, however busy or quiet the other tracks are. Against what the
/// server *delivered*, it also trails by its own fsync and, unless
/// `writer` syncs on a thread per track, by the other tracks' fsyncs (see
/// the module docs).
///
/// Each track's [`Progress`] (from [`CaptureReceiver::progress`]) moves on
/// as its audio is appended and its journals are fsync'd.
///
/// # Errors
///
/// Before anything is recorded: [`RecordError::RateMismatch`] if the
/// streams and `writer` run at different rates;
/// [`RecordError::Session`] with [`SessionError::UnknownTrack`] for a
/// timeline whose track `writer` didn't start;
/// [`RecordError::TimelineMismatch`] unless each timeline's current epoch
/// is the one `writer` records its track in, from the same first sample,
/// at `writer`'s rate. While recording, [`RecordError::Session`] if a
/// track can't be recorded. A journal that breaks, or a track that can't
/// join, isn't an error here: it's reported, and recording goes on.
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
        let Some(epoch) = writer.epoch(track) else {
            return Err(RecordError::Session(SessionError::UnknownTrack(track)));
        };
        // The writer takes only epochs at its rate, so this checks the
        // timeline's too.
        if timeline.current().map(Epoch::anchor) != Some(epoch.anchor()) {
            return Err(RecordError::TimelineMismatch);
        }
    }
    let mut joining: BTreeMap<TrackId, Joining> = events
        .tracks()
        .into_iter()
        .filter(|&t| !timelines.iter().any(|timeline| timeline.track() == t))
        .map(|t| (t, Joining::Unconfirmed(Vec::new())))
        .collect();
    let mut timelines = Timelines {
        given: timelines,
        joined: Vec::new(),
        stamps: BTreeMap::new(),
    };
    for (track, _) in &events.tracks {
        note_started(writer, events, *track);
    }
    note_durable(writer, events);
    let mut live: BTreeSet<TrackId> = events.tracks().into_iter().collect();
    let mut detectors = Detectors::new(events);
    while !live.is_empty() {
        let (track, event) = match events.next(IDLE_SYNC_CHECK) {
            // A stream can report something before its start fails; its
            // track was never started, so there's nothing to record it in.
            // So can one whose track has ended: audio its callback sent as
            // the failure was, or a device event after it.
            Received::Event(track, event) if !live.contains(&track) => {
                events.discard(event);
                continue;
            }
            Received::Event(track, event) => (track, event),
            Received::Idle => {
                settle(writer, Ok(()), report)?;
                note_durable(writer, events);
                detectors.check_stalls(events, &live, report);
                continue;
            }
            Received::Closed => return Ok(()),
        };
        // Before `admit`, which swallows a joining track's `Started`.
        detectors.note(track, &event);
        let admitted = admit(&mut joining, track, event, events);
        if admitted.is_empty() {
            // It waits; the other tracks' fsyncs still run when due.
            settle(writer, Ok(()), report)?;
            note_durable(writer, events);
        }
        for event in admitted {
            let outcome =
                match handle(writer, &mut timelines, &mut detectors, track, event, report)? {
                    Handled::Recorded(outcome, spent) => {
                        if let Some(buffer) = spent {
                            // An append refused outright (`Overflow`) recorded
                            // nothing: those samples still count as queued.
                            if !matches!(outcome, Err((_, SessionError::Overflow))) {
                                note_appended(writer, events, track, buffer.len());
                            }
                            events.events.recycle(buffer);
                        }
                        outcome
                    }
                    Handled::Joined => {
                        note_started(writer, events, track);
                        Ok(())
                    }
                    Handled::Refused(error) => {
                        // Its later events are dropped; its `Stopped` ends it.
                        joining.insert(track, Joining::Refused);
                        report(Some(track), RecorderEvent::CaptureFailed(error));
                        Ok(())
                    }
                    Handled::Ended => {
                        let next = writer.next_sample(track);
                        let timeline = timelines.get_mut(track).map(|(timeline, _)| &*timeline);
                        detectors.end(track, next, timeline, report);
                        live.remove(&track);
                        Ok(())
                    }
                };
            settle(writer, outcome, report)?;
            note_durable(writer, events);
        }
        detectors.check_stalls(events, &live, report);
    }
    Ok(())
}

impl CaptureReceiver {
    /// Drops `event`, unrecorded; if it's audio, its buffer goes back to be
    /// filled again.
    fn discard(&self, event: CaptureEvent) {
        if let CaptureEvent::Audio(samples) | CaptureEvent::TimedAudio { samples, .. } = event {
            self.events.recycle(samples);
        }
    }
}

/// The timelines [`record_tracks`] times its tracks by: those it was
/// given, and those of tracks that joined; and how each is timed by its
/// stream's stamps.
struct Timelines<'a> {
    given: &'a mut [TrackTimeline],
    joined: Vec<TrackTimeline>,
    stamps: BTreeMap<TrackId, Stamps>,
}

impl Timelines<'_> {
    /// `track`'s timeline, if it has one yet, and its stamps' state.
    fn get_mut(&mut self, track: TrackId) -> Option<(&mut TrackTimeline, &mut Stamps)> {
        let timeline = self
            .given
            .iter_mut()
            .chain(self.joined.iter_mut())
            .find(|t| t.track() == track)?;
        Some((timeline, self.stamps.entry(track).or_default()))
    }
}

/// How a track is timed by its stream's stamps.
#[derive(Debug, Default)]
struct Stamps {
    /// Reads each stamped buffer against the track's epoch.
    meter: DriftMeter,
    /// When the last epoch opened for a loss starts, for [`BURST_WINDOW`].
    last_loss: Option<SessionTime>,
    /// Whether the stream has stamped a buffer: if so, its overrun reports
    /// open nothing themselves, and its buffers say what was lost.
    stamped: bool,
}

impl Stamps {
    /// Whether an epoch opened for a loss at `at` would come within
    /// [`BURST_WINDOW`] of the last.
    fn in_burst(&self, at: SessionTime) -> bool {
        self.last_loss
            .and_then(|last| at.checked_duration_since(last))
            .is_some_and(|since| since < BURST_WINDOW)
    }

    /// The drift a new epoch of `timeline` opens with: the measured one, or
    /// its current epoch's while none is.
    fn drift_for(&self, timeline: &TrackTimeline) -> Drift {
        self.meter
            .measured()
            .unwrap_or_else(|| timeline.current().map_or(Drift::ZERO, Epoch::drift))
    }
}

/// A track that will join [`record_tracks`] once its stream starts.
enum Joining {
    /// Its start hasn't returned: what it sends waits here.
    Unconfirmed(Vec<CaptureEvent>),
    /// Its stream started; it joins at its first audio.
    Confirmed,
    /// Its stream didn't start, or it couldn't join: nothing more of it is
    /// recorded, and it ends with its stream.
    Refused,
}

/// What of `track`'s `event` to handle now: the event itself, unless the
/// track's stream hasn't started yet, when it waits. Once the start
/// returns, everything that waited, in order; if it fails, only the
/// failure (the rest is dropped). Of a refused track, only the end of its
/// stream.
fn admit(
    joining: &mut BTreeMap<TrackId, Joining>,
    track: TrackId,
    event: CaptureEvent,
    events: &CaptureReceiver,
) -> Vec<CaptureEvent> {
    let held = match joining.get_mut(&track) {
        Some(Joining::Unconfirmed(held)) => held,
        Some(Joining::Refused) => {
            return match event {
                CaptureEvent::Stopped | CaptureEvent::Failed(_) => vec![CaptureEvent::Stopped],
                event => {
                    events.discard(event);
                    Vec::new()
                }
            };
        }
        Some(Joining::Confirmed) | None => return vec![event],
    };
    match event {
        CaptureEvent::Started => {
            let held = std::mem::take(held);
            joining.insert(track, Joining::Confirmed);
            held
        }
        CaptureEvent::NotStarted => {
            for dropped in std::mem::take(held) {
                events.discard(dropped);
            }
            joining.insert(track, Joining::Refused);
            vec![CaptureEvent::NotStarted]
        }
        event => {
            held.push(event);
            Vec::new()
        }
    }
}

/// Moves `track`'s progress to where `writer` starts it, if it's started:
/// nothing before is this run's to lose.
fn note_started<S: Fs>(writer: &SessionWriter<S>, events: &CaptureReceiver, track: TrackId) {
    let Some((_, progress)) = events.tracks.iter().find(|(t, _)| *t == track) else {
        return;
    };
    if let Some(next) = writer.next_sample(track) {
        progress.appended(0, next);
        if writer.durable(track).is_none() {
            progress.synced(next);
        }
    }
}

/// Moves `track`'s progress on by the `n` samples just appended.
fn note_appended<S: Fs>(
    writer: &SessionWriter<S>,
    events: &CaptureReceiver,
    track: TrackId,
    n: usize,
) {
    if let (Some(progress), Some(next)) = (
        events.tracks.iter().find(|(t, _)| *t == track),
        writer.next_sample(track),
    ) {
        progress.1.appended(n, next);
    }
}

/// Notes how far each track's current journal is durable. Between
/// journals nothing moves: the last journal may have ended with its fsync,
/// or broken, leaving a gap, and a journal's own durable position is the
/// only proof either way. A track that has started no journal yet has
/// nothing at risk before its first sample.
fn note_durable<S: Fs>(writer: &SessionWriter<S>, events: &CaptureReceiver) {
    for (track, progress) in &events.tracks {
        if let Some(durable) = writer.durable(*track) {
            progress.synced(durable.end());
        }
    }
}

/// What became of one event.
enum Handled {
    /// It was recorded, with this outcome from the writer, and the buffer
    /// its audio came in, if it was audio, to fill again.
    Recorded(Result<(), (TrackId, SessionError)>, Option<Vec<i16>>),
    /// The track joined: it's started on the writer, its first epoch open.
    Joined,
    /// The track couldn't join, for this reason: it isn't recorded.
    Refused(CaptureError),
    /// The track's stream stopped, failed or never started: nothing more
    /// comes from it.
    Ended,
}

/// Records one `event` from `track`'s stream: audio appended to its
/// journal and reported, an overrun turned into a new epoch, a failure
/// reported, a joining track's first audio turned into its first epoch.
/// The writer's error, if any, comes back for [`settle`].
fn handle<S: Fs>(
    writer: &mut SessionWriter<S>,
    timelines: &mut Timelines<'_>,
    detectors: &mut Detectors,
    track: TrackId,
    event: CaptureEvent,
    report: &mut dyn FnMut(Option<TrackId>, RecorderEvent),
) -> Result<Handled, RecordError> {
    let outcome = match event {
        CaptureEvent::Audio(samples) => {
            return record_audio(writer, timelines, detectors, track, (samples, None), report);
        }
        CaptureEvent::TimedAudio { samples, stamp } => {
            return record_audio(
                writer,
                timelines,
                detectors,
                track,
                (samples, Some(stamp)),
                report,
            );
        }
        CaptureEvent::Notice { notice, at } => {
            let lost = notice == CaptureNotice::Overrun;
            report(Some(track), RecorderEvent::Capture(notice));
            match timelines.get_mut(track) {
                // Before its first audio, a track has no epoch to leave. A
                // stream that stamps its buffers shows its losses in them.
                Some((timeline, stamps)) if lost && !stamps.stamped => {
                    let drift = stamps.drift_for(timeline);
                    open_epoch_after_loss(writer, timeline, at, drift, &mut |e| {
                        report(Some(track), e);
                    })
                }
                _ => Ok(()),
            }
        }
        CaptureEvent::Reopened { at } => match timelines.get_mut(track) {
            Some((timeline, stamps)) => {
                // The audio no longer follows on from the run before.
                stamps.meter.restart();
                if opened_since(writer, timeline, at) {
                    // An epoch with no audio yet, opened no earlier (an
                    // overrun's, just before), already makes the stretch a
                    // gap; see the module docs.
                    Ok(())
                } else {
                    let drift = stamps.drift_for(timeline);
                    open_epoch_after_loss(writer, timeline, at, drift, &mut |e| {
                        report(Some(track), e);
                    })
                }
            }
            // Before its first audio, a track has no epoch to leave.
            None => Ok(()),
        },
        CaptureEvent::Began { at } => {
            if timelines.get_mut(track).is_some() {
                // Started first: its epoch is already open.
                return Ok(Handled::Recorded(Ok(()), None));
            }
            let timeline = match join(writer, track, at) {
                Ok(timeline) => timeline,
                Err(error) => return Ok(Handled::Refused(error)),
            };
            if let Some(&epoch) = timeline.current() {
                report(Some(track), RecorderEvent::Epoch(epoch));
            }
            timelines.joined.push(timeline);
            return Ok(Handled::Joined);
        }
        CaptureEvent::Device { change, at } => {
            report(Some(track), RecorderEvent::Device { change, at });
            Ok(())
        }
        CaptureEvent::Started => Ok(()),
        CaptureEvent::Failed(error) => {
            report(Some(track), RecorderEvent::CaptureFailed(error));
            return Ok(Handled::Ended);
        }
        CaptureEvent::Stopped | CaptureEvent::NotStarted => return Ok(Handled::Ended),
    };
    Ok(Handled::Recorded(outcome.map_err(|e| (track, e)), None))
}

/// Records `track`'s `audio`: samples, and the stream's stamp if it gave
/// one. It's read against the track's epoch
/// first, which may move it to a new one ([`time_buffer`]), then appended,
/// reported, and fed to the track's detectors.
fn record_audio<S: Fs>(
    writer: &mut SessionWriter<S>,
    timelines: &mut Timelines<'_>,
    detectors: &mut Detectors,
    track: TrackId,
    audio: (Vec<i16>, Option<Stamp>),
    report: &mut dyn FnMut(Option<TrackId>, RecorderEvent),
) -> Result<Handled, RecordError> {
    let (samples, stamp) = audio;
    let Some((timeline, stamps)) = timelines.get_mut(track) else {
        return Err(RecordError::Session(SessionError::UnknownTrack(track)));
    };
    let first = writer
        .next_sample(track)
        .ok_or(SessionError::UnknownTrack(track))
        .map_err(RecordError::Session)?;
    if let Some(stamp) = stamp {
        let moved = time_buffer(writer, timeline, stamps, first, stamp, &mut |e| {
            report(Some(track), e);
        });
        // Reported now, so a journal that broke as the epoch moved isn't
        // hidden by what the append does.
        settle(writer, moved.map_err(|e| (track, e)), report)?;
    }
    let appended = writer.append(track, &samples);
    // A copy, made here rather than in the stream's callback: the buffer
    // goes back to the stream.
    if let Some(chunk) = AudioChunk::new(track, first, writer.rate(), samples.clone()) {
        report(Some(track), RecorderEvent::Audio(chunk));
    }
    // Fed whatever the append did: the samples are numbered either way.
    detectors.audio(track, writer.rate(), first, &samples, timeline, report);
    Ok(Handled::Recorded(
        appended.map_err(|e| (track, e)),
        Some(samples),
    ))
}

/// Reads a stamped buffer of `timeline`'s track, whose first sample is
/// `first`, stamped `stamp`, and moves the track to a new epoch if the
/// meter says to: where the stream's cycles put it after a loss (unless a
/// burst's window is still open), or straight on to correct drift. A drift past the limit is
/// reported, once.
///
/// A [`SessionError::Journal`] or [`SessionError::Marks`] from ending the
/// old epoch's journal comes back with the track moved all the same.
fn time_buffer<S: Fs>(
    writer: &mut SessionWriter<S>,
    timeline: &mut TrackTimeline,
    stamps: &mut Stamps,
    first: SampleIndex,
    stamp: Stamp,
    report: &mut dyn FnMut(RecorderEvent),
) -> Result<(), SessionError> {
    stamps.stamped = true;
    let Some(epoch) = timeline.current().copied() else {
        return Ok(());
    };
    let at = stamp.at;
    if first == epoch.first_sample() && at > epoch.start() {
        // The epoch holds no audio yet, and starts before its first audio
        // was captured: a track started first opens it when its stream was
        // asked to start, early by the stream's start-up. Its audio starts
        // when it was captured, as a joining track's does, and the time
        // before is a gap.
        return open_at_capture(writer, timeline, stamps, (first, at), stamp, report);
    }
    let moved = match stamps.meter.observe(&epoch, first, stamp) {
        Reading::Steady => Ok(()),
        // Within a burst, the next window's first buffer opens one epoch
        // for every loss.
        Reading::Lost { .. } if stamps.in_burst(at) => Ok(()),
        Reading::Lost { hole } => {
            // Where the stream's cycles put the buffer, not its capture
            // stamp: a step in the delay moves the stamp, not the audio.
            let resumed = epoch
                .time_of(first)
                .and_then(|mapped| mapped.checked_add(hole))
                .unwrap_or(at);
            stamps.last_loss = Some(resumed);
            open_at_capture(writer, timeline, stamps, (first, resumed), stamp, report)
        }
        Reading::Retime(drift) => {
            let opened = timeline.retime(first, drift);
            move_to_opened(writer, timeline, opened, report)
        }
    };
    if let Some(drift) = stamps.meter.past_limit() {
        report(RecorderEvent::Capture(CaptureNotice::Drifted(drift)));
    }
    moved
}

/// Starts joining `track` on `writer` at its first free sample, and
/// returns its timeline, carrying on from its earlier recordings with a
/// new epoch opened `at`; or why it can't record.
fn join<S: Fs>(
    writer: &mut SessionWriter<S>,
    track: TrackId,
    at: SessionTime,
) -> Result<TrackTimeline, CaptureError> {
    let refused = |e: &dyn fmt::Display| CaptureError::Backend(format!("couldn't record it: {e}"));
    let (timeline, epoch) = writer
        .open_first_epoch(track, at)
        .map_err(|e| refused(&e))?;
    writer.start_track(track, &epoch).map_err(|e| refused(&e))?;
    Ok(timeline)
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

/// Whether `timeline`'s current epoch holds none of its track's audio yet
/// (`writer` hasn't recorded past its first sample) and starts at `at` or
/// later: it already marks the stretch before audio captured at `at`.
fn opened_since<S: Fs>(
    writer: &SessionWriter<S>,
    timeline: &TrackTimeline,
    at: SessionTime,
) -> bool {
    match (timeline.current(), writer.next_sample(timeline.track())) {
        (Some(epoch), Some(next)) => next == epoch.first_sample() && epoch.start() >= at,
        _ => false,
    }
}

/// Moves `timeline`'s track to a new epoch at `at`, when the buffer from
/// sample `first`, stamped `stamp`, was captured, after a stretch with no
/// audio, and starts the meter's new run with that buffer. Errors as
/// [`open_epoch_after_loss`].
fn open_at_capture<S: Fs>(
    writer: &mut SessionWriter<S>,
    timeline: &mut TrackTimeline,
    stamps: &mut Stamps,
    (first, at): (SampleIndex, SessionTime),
    stamp: Stamp,
    report: &mut dyn FnMut(RecorderEvent),
) -> Result<(), SessionError> {
    let drift = stamps.drift_for(timeline);
    let moved = open_epoch_after_loss(writer, timeline, at, drift, report);
    stamps.meter.restart();
    if let Some(epoch) = timeline.current() {
        stamps.meter.observe(epoch, first, stamp);
    }
    moved
}

/// Moves `timeline`'s track to a new epoch starting at `at`, under
/// `drift`, in `timeline` and `writer` alike, after audio was lost or went
/// uncaptured: the samples that follow play from `at`, and the stretch
/// missed is a gap. The new epoch is reported, or the timeline's refusal,
/// with the track left in its epoch.
///
/// A [`SessionError::Journal`] or [`SessionError::Marks`] from ending the
/// old epoch's journal comes back with the track moved all the same.
fn open_epoch_after_loss<S: Fs>(
    writer: &mut SessionWriter<S>,
    timeline: &mut TrackTimeline,
    at: SessionTime,
    drift: Drift,
    report: &mut dyn FnMut(RecorderEvent),
) -> Result<(), SessionError> {
    let track = timeline.track();
    let next = writer
        .next_sample(track)
        .ok_or(SessionError::UnknownTrack(track))?;
    let opened = timeline.open_epoch_drifting(at, next, writer.rate(), drift);
    move_to_opened(writer, timeline, opened, report)
}

/// Moves `writer`'s track to the epoch `timeline` just `opened`, and
/// reports it; or reports why the timeline refused it, with the track left
/// in its epoch. Errors as [`open_epoch_after_loss`].
fn move_to_opened<S: Fs>(
    writer: &mut SessionWriter<S>,
    timeline: &TrackTimeline,
    opened: Result<OpenedEpoch, EpochError>,
    report: &mut dyn FnMut(RecorderEvent),
) -> Result<(), SessionError> {
    if let Err(refused) = opened {
        report(RecorderEvent::EpochRefused(refused));
        return Ok(());
    }
    // The writer is in the timeline's previous epoch (checked when
    // recording started, and kept in step since), so it takes the next.
    let Some(&epoch) = timeline.current() else {
        return Ok(());
    };
    let moved = writer.new_epoch(timeline.track(), &epoch);
    report(RecorderEvent::Epoch(epoch));
    moved
}
