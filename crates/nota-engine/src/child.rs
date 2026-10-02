//! The engine child's loop: read audio frames from the recorder, cut them
//! into chunks at pauses, transcribe the chunks with speech, and report text
//! and confirmed positions back.
//!
//! The loop knows the models only through [`Models`], so its protocol
//! behaviour is tested without them.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use nota_core::messages::{AudioChunk, FromEngine, ProtocolVersion, ToEngine, Transcript};
use nota_core::protocol::{Frame, FrameReader, write_frame};
use nota_core::{SampleIndex, SampleRate, TrackId};

use crate::EngineError;
use crate::chunker::{Chunk, Chunker, ChunkerConfig, Labels};

/// A voice activity detector over one track's audio.
pub trait Detector {
    /// Feeds the next samples; returns what's newly settled.
    fn accept(&mut self, samples: &[f32]) -> Labels;
    /// Ends the audio: everything fed is settled. The detector isn't fed
    /// again afterwards.
    fn flush(&mut self) -> Labels;
}

/// A speech recogniser.
pub trait Transcriber {
    /// The text in `audio` (16 kHz, in -1..1), trimmed; empty if none.
    fn transcribe(&mut self, audio: &[f32]) -> String;
}

/// The loaded models.
pub trait Models {
    /// The detector type.
    type Detector: Detector;
    /// The recogniser type.
    type Transcriber: Transcriber;

    /// A fresh detector for a track whose audio starts at `first`.
    ///
    /// # Errors
    ///
    /// If the detector can't be created.
    fn detector(&self, first: SampleIndex) -> Result<Self::Detector, EngineError>;

    /// The recogniser.
    fn transcriber(&mut self) -> &mut Self::Transcriber;
}

/// One track's state: its detector and chunker.
struct Track<D> {
    detector: D,
    chunker: Chunker,
}

/// Runs the protocol on `input` and `output` until `input` ends.
///
/// The recorder's `Hello` must come first; `load` is then called to load
/// the models, and the engine's `Hello` tells the recorder it's ready.
/// Every chunk is answered with its text (if any) and then a
/// [`FromEngine::Confirmed`] up to its end.
///
/// # Errors
///
/// The first protocol error, write failure or model failure. The recorder
/// restarts the engine on any of them, so nothing is retried here.
pub fn run<M: Models>(
    input: impl Read,
    mut output: impl Write,
    load: impl FnOnce() -> Result<M, EngineError>,
) -> Result<(), EngineError> {
    let mut reader = FrameReader::new(input);
    match reader.read_frame::<ToEngine>()? {
        Some(Frame::Hello(version)) if version == ProtocolVersion::CURRENT => {}
        Some(Frame::Hello(version)) => return Err(EngineError::Version(version)),
        Some(Frame::Message(_)) => return Err(EngineError::Protocol("message before hello")),
        None => return Ok(()),
    }
    let mut models = load()?;
    send(&mut output, &Frame::Hello(ProtocolVersion::CURRENT))?;

    let mut tracks: BTreeMap<TrackId, Track<M::Detector>> = BTreeMap::new();
    while let Some(frame) = reader.read_frame::<ToEngine>()? {
        match frame {
            Frame::Hello(_) => return Err(EngineError::Protocol("second hello")),
            Frame::Message(ToEngine::Audio(chunk)) => {
                let track = match tracks.entry(chunk.track()) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => entry.insert(Track {
                        detector: models.detector(chunk.range().start())?,
                        chunker: Chunker::new(
                            ChunkerConfig::live(SampleRate::SPEECH),
                            chunk.range().start(),
                        ),
                    }),
                };
                let done = accept(track, &chunk)?;
                answer(&mut output, models.transcriber(), chunk.track(), done)?;
            }
            Frame::Message(ToEngine::Flush { track: id }) => {
                if let Some(mut track) = tracks.remove(&id) {
                    let labels = track.detector.flush();
                    let done = track.chunker.finish(&labels);
                    answer(&mut output, models.transcriber(), id, done)?;
                }
            }
        }
    }
    Ok(())
}

/// Feeds a chunk of audio to its track.
fn accept<D: Detector>(
    track: &mut Track<D>,
    chunk: &AudioChunk,
) -> Result<Vec<Chunk>, EngineError> {
    if chunk.rate() != SampleRate::SPEECH {
        return Err(EngineError::Protocol("audio not at 16 kHz"));
    }
    if chunk.range().start() != track.chunker.next_sample() {
        return Err(EngineError::Protocol("audio doesn't follow on"));
    }
    let samples: Vec<f32> = chunk
        .samples()
        .iter()
        .map(|&s| f32::from(s) / 32_768.0)
        .collect();
    let labels = track.detector.accept(&samples);
    Ok(track.chunker.push(&samples, &labels))
}

/// Transcribes `chunks` and reports each, in order.
fn answer(
    output: &mut impl Write,
    transcriber: &mut impl Transcriber,
    track: TrackId,
    chunks: Vec<Chunk>,
) -> Result<(), EngineError> {
    for chunk in chunks {
        if chunk.has_speech {
            let text = transcriber.transcribe(&chunk.audio);
            if !text.is_empty() {
                let transcript = Transcript {
                    track,
                    range: chunk.range,
                    text,
                };
                send(output, &Frame::Message(FromEngine::Transcript(transcript)))?;
            }
        }
        let confirmed = FromEngine::Confirmed {
            track,
            up_to: chunk.range.end(),
        };
        send(output, &Frame::Message(confirmed))?;
    }
    Ok(())
}

fn send(output: &mut impl Write, frame: &Frame<FromEngine>) -> Result<(), EngineError> {
    write_frame(output, frame).map_err(EngineError::Write)
}

#[cfg(test)]
mod tests;
