//! Capture with nothing recorded: the level meters Setup shows before a
//! session exists.
//!
//! A [`Preview`] opens a stream for each source it's asked to listen to and
//! reports the level each one hears, as [`PreviewEvent::Level`]. It has no
//! writer, filesystem, session or journal: the audio is measured and
//! dropped, so a preview can't leave anything behind. It's also separate
//! from what a recording does with its audio (epochs, stamps and drift are
//! the recorder's business, and none of it runs here).
//!
//! It runs on a thread of its own, named `nota-preview`, which opens and
//! drops every stream (a backend's streams needn't be `Send`). Asking it to
//! [`listen`](Preview::listen) to other sources stops the old streams first,
//! so the same device is never open twice.
//!
//! The same thread asks the backend for its [`Devices`] when it starts and
//! every [`DEVICES_EVERY`] after, and tells the screen only when the list
//! has changed, so a headset plugged in while Setup is open appears in it.
//! Reading the list can take a moment (a few seconds at worst, if the audio
//! server doesn't answer), during which the levels wait.
//!
//! A stream that can't be opened, or fails while it runs, is reported once
//! as [`PreviewEvent::Failed`]; the other streams carry on.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::recorder::Level;
use nota_core::{Clock, SampleRate, SessionTime, TrackId};

use super::queue::Received;
use super::{
    Capture, CaptureBackend, CaptureError, CaptureEvent, CaptureReceiver, Devices, Source,
    prepare_tracks,
};

#[cfg(test)]
mod tests;

/// The least time between two levels of one track: 100 ms, as the live
/// thread's.
pub(super) const LEVEL_EVERY: Duration = Duration::from_millis(100);

/// How often the backend is asked for its devices again: 2 s.
pub(super) const DEVICES_EVERY: Duration = Duration::from_secs(2);

/// How long the thread waits for audio or a command before it looks at
/// the devices again, which is also the longest a command waits to be
/// heard.
const TICK: Duration = Duration::from_millis(50);

/// What a preview tells the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewEvent {
    /// The loudest the track's stream was since its last level.
    Level {
        /// The track.
        track: TrackId,
        /// The peak.
        level: Level,
    },
    /// The track's stream couldn't be opened, or stopped. It's told once,
    /// and nothing more comes from the track until it's listened to again.
    Failed {
        /// The track.
        track: TrackId,
        /// Why.
        error: CaptureError,
    },
    /// The audio server's devices, when they're first read and whenever
    /// they change.
    Devices(Devices),
}

/// A running preview. Dropping it stops the streams and the thread, and
/// waits for it.
#[derive(Debug)]
pub struct Preview {
    /// Sends the sources to listen to; `None` once it's closing.
    commands: Option<Sender<Vec<(TrackId, Source)>>>,
    /// The preview's thread.
    thread: Option<JoinHandle<()>>,
}

impl Preview {
    /// Starts a preview thread on `backend`, listening to nothing yet, and
    /// the receiver of what it tells. Levels are paced and devices polled by
    /// `clock`, and streams record at `rate`.
    ///
    /// The thread ends when the preview is dropped, or when the receiver is.
    ///
    /// # Errors
    ///
    /// If the thread can't be started.
    pub fn start<B: CaptureBackend + Send + 'static>(
        backend: B,
        rate: SampleRate,
        clock: Arc<dyn Clock>,
    ) -> io::Result<(Self, Receiver<PreviewEvent>)> {
        let (commands, command_rx) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        // The worker is built on its thread: its streams needn't be `Send`.
        let thread = thread::Builder::new()
            .name("nota-preview".into())
            .spawn(move || {
                Worker {
                    backend,
                    rate,
                    clock,
                    commands: command_rx,
                    events: events_tx,
                    listening: None,
                    devices: DevicesPoll::default(),
                }
                .run();
            })?;
        let preview = Self {
            commands: Some(commands),
            thread: Some(thread),
        };
        Ok((preview, events))
    }

    /// Listens to `sources` instead of what it listened to: the old streams
    /// stop, then each of the new ones opens. A source that can't be opened
    /// is told as [`PreviewEvent::Failed`].
    pub fn listen(&self, sources: &[(TrackId, Source)]) {
        if let Some(commands) = &self.commands {
            // A thread that has ended (its receiver was dropped) has nothing
            // left to listen with.
            let _ = commands.send(sources.to_vec());
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        // The thread ends when it finds its commands gone.
        drop(self.commands.take());
        if let Some(thread) = self.thread.take() {
            // A thread that panicked has no streams left to stop.
            let _ = thread.join();
        }
    }
}

/// The [`Preview`] was dropped: no more commands will come.
#[derive(Debug)]
struct Dropped;

/// The receiver of the preview's events is gone: nothing is listening.
#[derive(Debug)]
struct ScreenGone;

/// Tells the screen `event`.
fn tell(events: &Sender<PreviewEvent>, event: PreviewEvent) -> Result<(), ScreenGone> {
    events.send(event).map_err(|_| ScreenGone)
}

/// The streams a preview has open, and what's needed to read them.
struct Listening<T> {
    /// The streams. They go before the receiver, so what they send as they
    /// stop is still queued, and dropped with it.
    _captures: Vec<Capture<T>>,
    /// Where the streams' audio arrives.
    receiver: CaptureReceiver,
    /// What each track has heard since its last level.
    meters: Meters,
    /// The tracks whose failure has been told.
    failed: BTreeSet<TrackId>,
}

/// The preview thread's state.
struct Worker<B: CaptureBackend> {
    backend: B,
    rate: SampleRate,
    clock: Arc<dyn Clock>,
    commands: Receiver<Vec<(TrackId, Source)>>,
    events: Sender<PreviewEvent>,
    /// The open streams, if it listens to anything.
    listening: Option<Listening<B::Stream>>,
    /// When the devices were last read, and what they were.
    devices: DevicesPoll,
}

impl<B: CaptureBackend> Worker<B> {
    /// Runs until the preview is dropped or its receiver is.
    fn run(mut self) {
        loop {
            match self.take_command() {
                Ok(Some(sources)) => {
                    if self.listen(&sources).is_err() {
                        return;
                    }
                }
                Ok(None) => {}
                // Dropped: the streams go with this thread's state.
                Err(Dropped) => return,
            }
            if self.poll_devices().is_err() || self.step().is_err() {
                return;
            }
        }
    }

    /// The newest sources waiting to be listened to, if any: an earlier
    /// request that nothing heard is replaced by a later one.
    fn take_command(&self) -> Result<Option<Vec<(TrackId, Source)>>, Dropped> {
        let mut newest = None;
        loop {
            match self.commands.try_recv() {
                Ok(sources) => newest = Some(sources),
                Err(TryRecvError::Empty) => return Ok(newest),
                Err(TryRecvError::Disconnected) => return Err(Dropped),
            }
        }
    }

    /// Stops what's open and opens `sources`, telling the screen of each
    /// that can't be.
    fn listen(&mut self, sources: &[(TrackId, Source)]) -> Result<(), ScreenGone> {
        // The old streams first: a device is never open twice.
        self.listening = None;
        if sources.is_empty() {
            return Ok(());
        }
        let (starter, receiver) = prepare_tracks(sources, self.rate, &self.clock);
        let mut captures = Vec::new();
        for ((track, _), started) in sources.iter().zip(starter.start(&self.backend)) {
            match started {
                Ok(capture) => captures.push(capture),
                Err(error) => tell(
                    &self.events,
                    PreviewEvent::Failed {
                        track: *track,
                        error,
                    },
                )?,
            }
        }
        self.listening = Some(Listening {
            _captures: captures,
            receiver,
            meters: Meters::default(),
            failed: BTreeSet::new(),
        });
        Ok(())
    }

    /// Asks the backend for its devices if they're due, and tells the screen
    /// if they changed.
    fn poll_devices(&mut self) -> Result<(), ScreenGone> {
        if !self.devices.due(self.clock.now()) {
            return Ok(());
        }
        let found = self.backend.devices();
        // After the read, so the time it took counts towards the next one.
        match self.devices.took(self.clock.now(), found) {
            Some(devices) => tell(&self.events, PreviewEvent::Devices(devices)),
            None => Ok(()),
        }
    }

    /// Waits up to [`TICK`] for the next thing to do, and does it.
    fn step(&mut self) -> Result<(), ScreenGone> {
        let Some(listening) = &mut self.listening else {
            // Nothing is open: wait for a command rather than for audio.
            match self.commands.recv_timeout(TICK) {
                Ok(sources) => return self.listen(&sources),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    return Ok(());
                }
            }
        };
        match listening.receiver.next(TICK) {
            Received::Event(track, event) => {
                let now = self.clock.now();
                match listening.hear(track, event, now) {
                    Some(event) => tell(&self.events, event),
                    None => Ok(()),
                }
            }
            Received::Idle => Ok(()),
            // Every stream has gone: nothing more will come.
            Received::Closed => {
                self.listening = None;
                Ok(())
            }
        }
    }
}

impl<T> Listening<T> {
    /// What the screen is to be told of `event` from `track`, at `now`: a
    /// level if the track's is due, or its failure the first time.
    fn hear(
        &mut self,
        track: TrackId,
        event: CaptureEvent,
        now: SessionTime,
    ) -> Option<PreviewEvent> {
        match event {
            CaptureEvent::Audio(samples) | CaptureEvent::TimedAudio { samples, .. } => {
                let level = self.meters.hear(track, &samples, now);
                self.receiver.events.recycle(samples);
                level.map(|level| PreviewEvent::Level { track, level })
            }
            CaptureEvent::Failed(error) => self
                .failed
                .insert(track)
                .then_some(PreviewEvent::Failed { track, error }),
            // Starts, stops, notices and device changes aren't a preview's
            // business: a recording turns them into epochs and warnings.
            CaptureEvent::Notice { .. }
            | CaptureEvent::Device { .. }
            | CaptureEvent::Stopped
            | CaptureEvent::Began { .. }
            | CaptureEvent::Reopened { .. }
            | CaptureEvent::Started
            | CaptureEvent::NotStarted => None,
        }
    }
}

/// What one track has heard since its last level, and when it told it.
#[derive(Debug, Default)]
struct Meter {
    /// The loudest sample since the last level.
    peak: Level,
    /// When the last level was told.
    told_at: Option<SessionTime>,
}

/// The tracks' meters: a level at most every [`LEVEL_EVERY`] for each.
#[derive(Debug, Default)]
struct Meters(BTreeMap<TrackId, Meter>);

impl Meters {
    /// Takes in `samples` heard on `track` at `now`. Returns the peak since
    /// the last level if one is due: at the track's first audio, and then
    /// once [`LEVEL_EVERY`] has passed since the last.
    fn hear(&mut self, track: TrackId, samples: &[i16], now: SessionTime) -> Option<Level> {
        let meter = self.0.entry(track).or_default();
        meter.peak = meter.peak.max(Level::of_samples(samples));
        let due = match meter.told_at {
            None => true,
            Some(last) => now
                .checked_duration_since(last)
                .is_some_and(|since| since >= LEVEL_EVERY),
        };
        if !due {
            return None;
        }
        meter.told_at = Some(now);
        Some(std::mem::take(&mut meter.peak))
    }
}

/// When the devices are next read, and what they were the last time.
#[derive(Debug, Default)]
struct DevicesPoll {
    /// The devices last read.
    last: Option<Devices>,
    /// When they're next due; `None` before the first read.
    next: Option<SessionTime>,
}

impl DevicesPoll {
    /// Whether the devices are due at `now`.
    fn due(&self, now: SessionTime) -> bool {
        self.next.is_none_or(|next| now >= next)
    }

    /// Takes in a read made at `now`. Returns the devices if they differ
    /// from the last read (the first always does). A failed read is nothing
    /// to tell: the screen keeps the list it has.
    fn took(&mut self, now: SessionTime, found: Result<Devices, CaptureError>) -> Option<Devices> {
        self.next = Some(now.checked_add(DEVICES_EVERY).unwrap_or(now));
        let devices = found.ok()?;
        if self.last.as_ref() == Some(&devices) {
            return None;
        }
        self.last = Some(devices.clone());
        Some(devices)
    }
}
