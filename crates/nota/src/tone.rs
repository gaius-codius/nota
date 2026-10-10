//! A capture backend that plays a steady tone in real time, for testing
//! the whole program without an audio server. Built only with the
//! `fake-capture` feature, which nota's own tests turn on.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::{Clock, SampleCount, SampleRate};
use nota_recorder::capture::{CaptureBackend, CaptureError, CaptureSender, Source};

use crate::inhibit::{Logind, SleepLock, SleepNotHeld};

/// How often the tone sends its next samples, as a callback would.
const PERIOD: Duration = Duration::from_millis(20);

/// Each source plays a square wave of its own pitch, timed by the session
/// clock. Two device names act out failures: `missing` can't be opened,
/// and `fails` stops with an error after half a second. Each source's
/// count of samples sent is kept, for the summary ([`Tone::report`]), so
/// the tests can check that everything sent was saved. With
/// `NOTA_TONE_STOP_DELAY_MS` set, stopping a tone takes that long, which
/// holds the recording's stop open for a test to signal it meanwhile.
#[derive(Debug)]
pub(crate) struct Tone {
    clock: Arc<dyn Clock>,
    happenings: Happenings,
    sent: Mutex<Vec<(Source, Arc<AtomicU64>)>>,
    stop_delay: Duration,
}

impl Tone {
    /// Tones timed by `clock`, noting their streams in `happenings`.
    pub(crate) fn new(clock: Arc<dyn Clock>, happenings: Happenings) -> Self {
        let stop_delay = std::env::var("NOTA_TONE_STOP_DELAY_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map_or(Duration::ZERO, Duration::from_millis);
        Self {
            clock,
            happenings,
            sent: Mutex::new(Vec::new()),
            stop_delay,
        }
    }

    /// How many samples each source's tone sent, one line each, then what
    /// happened to the streams and the sleep lock, in order.
    pub(crate) fn report(&self) -> Vec<String> {
        let Ok(sent) = self.sent.lock() else {
            return Vec::new();
        };
        let mut lines: Vec<String> = sent
            .iter()
            .map(|(source, count)| {
                format!(
                    "the tone for {source} sent {} samples",
                    count.load(Ordering::SeqCst)
                )
            })
            .collect();
        lines.extend(self.happenings.lines());
        lines
    }
}

/// What the tones and the stand-in logind did, in order, so a test can see
/// that the sleep lock was held for the recording and no longer. Shared
/// between them, and given in the summary ([`Tone::report`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct Happenings(Arc<Mutex<Vec<String>>>);

impl Happenings {
    /// Notes that `what` happened.
    fn note(&self, what: String) {
        if let Ok(mut happened) = self.0.lock() {
            happened.push(what);
        }
    }

    /// What happened so far, one line each.
    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .map(|happened| happened.iter().map(|w| format!("tone: {w}")).collect())
            .unwrap_or_default()
    }
}

/// A stand-in for logind: the lock it gives is a note in the
/// [`Happenings`] when taken and when dropped. With
/// `NOTA_TONE_SLEEP_REFUSED` set, it refuses.
#[derive(Debug)]
pub(crate) struct ToneLogind {
    happenings: Happenings,
    refuses: bool,
}

impl ToneLogind {
    pub(crate) fn new(happenings: Happenings) -> Self {
        Self {
            happenings,
            refuses: std::env::var_os("NOTA_TONE_SLEEP_REFUSED").is_some(),
        }
    }
}

/// What a stand-in lock holds: dropping it notes the release.
struct Released(Happenings);

impl Drop for Released {
    fn drop(&mut self) {
        self.0.note("sleep lock released".to_owned());
    }
}

impl Logind for ToneLogind {
    fn inhibit_sleep(&self) -> Result<SleepLock, SleepNotHeld> {
        if self.refuses {
            return Err(SleepNotHeld::new("the stand-in logind refused"));
        }
        self.happenings.note("sleep lock taken".to_owned());
        Ok(SleepLock::new(Released(self.happenings.clone())))
    }
}

/// A running tone. Dropping it stops the thread and waits for it.
#[derive(Debug)]
pub(crate) struct ToneStream {
    happenings: Happenings,
    source: Source,
    stop_delay: Duration,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ToneStream {
    fn drop(&mut self) {
        self.happenings.note(format!("{} stopped", self.source));
        if !self.stop_delay.is_zero() {
            // A pause that isn't a sleep: nothing sends on this channel.
            let (_keep, never) = mpsc::channel::<()>();
            let _ = never.recv_timeout(self.stop_delay);
        }
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl CaptureBackend for Tone {
    type Stream = ToneStream;

    fn start(
        &self,
        source: &Source,
        rate: SampleRate,
        events: CaptureSender,
    ) -> Result<ToneStream, CaptureError> {
        if *source == Source::Device("missing".into()) {
            return Err(CaptureError::DeviceNotAvailable(source.clone()));
        }
        let fails = *source == Source::Device("fails".into());
        let half_period: u64 = match source {
            Source::SystemAudio => 20,
            _ => 30,
        };
        let (stop, stopped) = mpsc::channel::<()>();
        let clock = Arc::clone(&self.clock);
        let count = Arc::new(AtomicU64::new(0));
        if let Ok(mut sent) = self.sent.lock() {
            sent.push((source.clone(), Arc::clone(&count)));
        }
        let thread = thread::Builder::new()
            .name("nota-tone".into())
            .spawn(move || {
                let started = clock.now();
                let mut sent = 0_u64;
                loop {
                    // Stopping, it still sends what's due, as a stream's
                    // last callback does.
                    let stopping =
                        !matches!(stopped.recv_timeout(PERIOD), Err(RecvTimeoutError::Timeout));
                    let elapsed = clock
                        .now()
                        .checked_duration_since(started)
                        .unwrap_or_default();
                    let due =
                        SampleCount::started_within(elapsed, rate).map_or(0, SampleCount::get);
                    let chunk: Vec<i16> = (sent..due)
                        .map(|i| {
                            if (i / half_period).is_multiple_of(2) {
                                6_000
                            } else {
                                -6_000
                            }
                        })
                        .collect();
                    events.audio(&chunk);
                    sent = due.max(sent);
                    count.store(sent, Ordering::SeqCst);
                    if stopping {
                        break;
                    }
                    if fails && elapsed >= Duration::from_millis(500) {
                        events.failed(CaptureError::Backend("the tone failed".into()));
                        break;
                    }
                }
            })
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        self.happenings.note(format!("{source} started"));
        Ok(ToneStream {
            happenings: self.happenings.clone(),
            source: source.clone(),
            stop_delay: self.stop_delay,
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}
