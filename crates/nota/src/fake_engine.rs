//! `nota engine fake`: the engine child with stand-in models, for nota's
//! end-to-end tests of the final pass without speech models. Built only
//! with the `fake-capture` feature, which nota's own tests turn on.
//!
//! It runs the real child loop and chunker ([`nota_engine::child::run`]):
//! a sample louder than [`LOUD`] is speech, and the text of a chunk is how
//! many samples it holds, so a test can check the text covers the audio.

use std::io;

use nota_core::SampleIndex;
use nota_core::SampleRange;
use nota_core::lifeline::{self, Tie};
use nota_engine::EngineError;
use nota_engine::child::{Detector, Models, Transcriber, run as run_child};
use nota_engine::chunker::{ChunkerConfig, Labels};

/// The loudness above which a sample is speech.
const LOUD: f32 = 0.01;

/// Runs the protocol on stdin and stdout with the stand-in models, cutting
/// as `chunking` says, until stdin closes or the recorder dies.
///
/// # Errors
///
/// As [`nota_engine::child::run`], or if it can't be tied to the
/// recorder.
pub(crate) fn run(chunking: ChunkerConfig) -> Result<(), EngineError> {
    if lifeline::tie_to_recorder().map_err(EngineError::Tie)? == Tie::Orphaned {
        return Ok(());
    }
    let input = io::BufReader::new(io::stdin().lock());
    run_child(input, io::stdout().lock(), chunking, || Ok(Stand))
}

/// The stand-in models.
struct Stand;

/// Loud samples are speech; a run of them is reported once a quiet sample
/// follows it.
struct Loudness {
    next: SampleIndex,
    since: Option<SampleIndex>,
}

impl Detector for Loudness {
    fn accept(&mut self, samples: &[f32]) -> Labels {
        let mut labels = Labels::default();
        for &sample in samples {
            match (sample.abs() > LOUD, self.since) {
                (true, None) => self.since = Some(self.next),
                (false, Some(from)) => {
                    labels.segments.extend(SampleRange::new(from, self.next));
                    self.since = None;
                }
                _ => {}
            }
            self.next = self.next.saturating_add(nota_core::SampleCount::new(1));
        }
        labels.silent_until = self.since.unwrap_or(self.next);
        labels
    }

    fn flush(&mut self) -> Labels {
        let mut labels = Labels::default();
        if let Some(from) = self.since.take() {
            labels.segments.extend(SampleRange::new(from, self.next));
        }
        labels.silent_until = self.next;
        labels
    }
}

impl Transcriber for Stand {
    fn transcribe(&mut self, audio: &[f32]) -> String {
        format!("{} samples", audio.len())
    }
}

impl Models for Stand {
    type Detector = Loudness;
    type Transcriber = Self;

    fn detector(&self, first: SampleIndex) -> Result<Loudness, EngineError> {
        Ok(Loudness {
            next: first,
            since: None,
        })
    }

    fn transcriber(&mut self) -> &mut Self {
        self
    }
}
