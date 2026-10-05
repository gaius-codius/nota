//! A capture backend that plays a steady tone in real time, for testing
//! the whole program without an audio server. Built only with the
//! `fake-capture` feature, which nota's own tests turn on.

use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::{Clock, SampleCount, SampleRate};
use nota_recorder::capture::{CaptureBackend, CaptureError, CaptureSender, Source};

/// How often the tone sends its next samples, as a callback would.
const PERIOD: Duration = Duration::from_millis(20);

/// Each source plays a square wave of its own pitch, timed by the session
/// clock.
#[derive(Debug)]
pub(crate) struct Tone {
    pub(crate) clock: Arc<dyn Clock>,
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
        let half_period: u64 = match source {
            Source::SystemAudio => 20,
            _ => 30,
        };
        let (stop, stopped) = mpsc::channel::<()>();
        let clock = Arc::clone(&self.clock);
        let thread = thread::Builder::new()
            .name("nota-tone".into())
            .spawn(move || {
                let started = clock.now();
                let mut sent = 0_u64;
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(PERIOD) {
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
                }
            })
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        Ok(ToneStream {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}
