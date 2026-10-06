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

/// How often the tone sends its next samples, as a callback would.
const PERIOD: Duration = Duration::from_millis(20);

/// Each source plays a square wave of its own pitch, timed by the session
/// clock. Two device names act out failures: `missing` can't be opened,
/// and `fails` stops with an error after half a second. Each source's
/// count of samples sent is kept, for the summary ([`Tone::report`]), so
/// the tests can check that everything sent was saved.
#[derive(Debug)]
pub(crate) struct Tone {
    clock: Arc<dyn Clock>,
    sent: Mutex<Vec<(Source, Arc<AtomicU64>)>>,
}

impl Tone {
    pub(crate) fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            sent: Mutex::new(Vec::new()),
        }
    }

    /// How many samples each source's tone sent, one line each.
    pub(crate) fn report(&self) -> Vec<String> {
        let Ok(sent) = self.sent.lock() else {
            return Vec::new();
        };
        sent.iter()
            .map(|(source, count)| {
                format!(
                    "the tone for {source} sent {} samples",
                    count.load(Ordering::SeqCst)
                )
            })
            .collect()
    }
}

/// A running tone. Dropping it stops the thread and waits for it.
#[derive(Debug)]
pub(crate) struct ToneStream {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ToneStream {
    fn drop(&mut self) {
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
        Ok(ToneStream {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}
