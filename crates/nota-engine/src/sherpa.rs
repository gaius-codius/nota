//! The models, on the official sherpa-onnx crate: Silero VAD as a
//! [`Detector`], Parakeet TDT as a [`Transcriber`].
//!
//! sherpa-onnx is C++ (with onnxruntime) linked statically. Its Rust API is
//! safe, but the code under it can still abort the process on bad input, so
//! everything here runs in the engine child and its inputs are checked
//! first: 16 kHz audio only, model files that exist.

use std::path::{Path, PathBuf};

use nota_core::{SampleIndex, SampleRange};
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, SileroVadModelConfig, VadModelConfig,
    VoiceActivityDetector,
};

use crate::EngineError;
use crate::child::{Detector, Models, Transcriber};
use crate::chunker::Labels;

/// The only rate the models take.
const RATE_HZ: i32 = 16_000;

/// Silero's window: 512 samples (32 ms) at 16 kHz.
const WINDOW: usize = 512;

/// How far the detector's settled silence trails the audio it has seen, in
/// samples. Speech is confirmed only after 0.25 s of it, and a segment's
/// start is then backdated by that plus two windows (about 0.31 s), so
/// anything older than 0.5 s that isn't in a segment is silence.
const SETTLE_LAG: u64 = 8_000;

/// Reset the detector after this many samples of silence, about an hour.
/// sherpa-onnx counts samples in an `i32`, which would overflow after 37
/// hours.
const RESET_AFTER: u64 = 16_000 * 3_600;

/// Where the model files are.
#[derive(Debug, Clone)]
pub struct ModelPaths {
    /// Parakeet TDT 0.6B v3 int8: `encoder.int8.onnx`, `decoder.int8.onnx`,
    /// `joiner.int8.onnx` and `tokens.txt`.
    pub parakeet_dir: PathBuf,
    /// Silero VAD v6 (`silero_vad.onnx`).
    pub vad_model: PathBuf,
    /// Threads for the recogniser.
    pub threads: u16,
}

/// Parakeet and Silero, loaded.
#[derive(Debug)]
pub struct SherpaModels {
    recognizer: Parakeet,
    vad: VadModelConfig,
}

impl SherpaModels {
    /// Loads the recogniser and checks the VAD model loads.
    ///
    /// # Errors
    ///
    /// [`EngineError::Model`] if a file is missing or sherpa-onnx refuses
    /// it.
    pub fn load(paths: &ModelPaths) -> Result<Self, EngineError> {
        let file = |name: &str| existing(&paths.parakeet_dir.join(name));
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer.encoder = Some(file("encoder.int8.onnx")?);
        config.model_config.transducer.decoder = Some(file("decoder.int8.onnx")?);
        config.model_config.transducer.joiner = Some(file("joiner.int8.onnx")?);
        config.model_config.tokens = Some(file("tokens.txt")?);
        config.model_config.model_type = Some("nemo_transducer".into());
        config.model_config.num_threads = i32::from(paths.threads.max(1));
        config.model_config.provider = Some("cpu".into());
        config.decoding_method = Some("greedy_search".into());
        let recognizer = OfflineRecognizer::create(&config)
            .ok_or_else(|| EngineError::Model("sherpa-onnx refused the Parakeet model".into()))?;

        let vad = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(existing(&paths.vad_model)?),
                threshold: 0.5,
                // The pause threshold: a pause this long ends a segment.
                min_silence_duration: 0.15,
                min_speech_duration: 0.25,
                window_size: 512,
                // A real value: a huge one silently disables
                // `min_silence_duration` in 1.13.8.
                max_speech_duration: 30.0,
            },
            sample_rate: RATE_HZ,
            num_threads: 1,
            provider: Some("cpu".into()),
            ..VadModelConfig::default()
        };
        // Fail now rather than on the first audio.
        create_vad(&vad)?;
        Ok(Self {
            recognizer: Parakeet { recognizer },
            vad,
        })
    }
}

impl Models for SherpaModels {
    type Detector = SileroDetector;
    type Transcriber = Parakeet;

    fn detector(&self, first: SampleIndex) -> Result<SileroDetector, EngineError> {
        Ok(SileroDetector {
            vad: create_vad(&self.vad)?,
            base: first.get(),
            processed: 0,
            pending: Vec::with_capacity(WINDOW),
            onset: None,
            silent_until: first.get(),
        })
    }

    fn transcriber(&mut self) -> &mut Parakeet {
        &mut self.recognizer
    }
}

fn existing(path: &Path) -> Result<String, EngineError> {
    if !path.is_file() {
        return Err(EngineError::Model(format!(
            "model file missing: {}",
            path.display()
        )));
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| EngineError::Model(format!("model path isn't UTF-8: {}", path.display())))
}

fn create_vad(config: &VadModelConfig) -> Result<VoiceActivityDetector, EngineError> {
    // A 60 s buffer: segments are at most 30 s.
    VoiceActivityDetector::create(config, 60.0)
        .ok_or_else(|| EngineError::Model("sherpa-onnx refused the Silero VAD model".into()))
}

/// Parakeet TDT 0.6B v3, greedy.
pub struct Parakeet {
    recognizer: OfflineRecognizer,
}

impl std::fmt::Debug for Parakeet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parakeet").finish_non_exhaustive()
    }
}

impl Transcriber for Parakeet {
    fn transcribe(&mut self, audio: &[f32]) -> String {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(RATE_HZ, audio);
        self.recognizer.decode(&stream);
        stream
            .get_result()
            .map(|result| result.text.trim().to_owned())
            .unwrap_or_default()
    }
}

/// Silero VAD over one track, reporting in track samples.
pub struct SileroDetector {
    vad: VoiceActivityDetector,
    /// The track sample the detector's own count starts at.
    base: u64,
    /// Samples fed to the detector since `base`.
    processed: u64,
    /// Samples waiting to fill a window.
    pending: Vec<f32>,
    /// Where (in the detector's count) speech was first detected, while it
    /// still is.
    onset: Option<u64>,
    silent_until: u64,
}

impl std::fmt::Debug for SileroDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SileroDetector")
            .field("base", &self.base)
            .field("processed", &self.processed)
            .field("onset", &self.onset)
            .field("silent_until", &self.silent_until)
            .finish_non_exhaustive()
    }
}

impl SileroDetector {
    fn feed_window(&mut self, window: &[f32], labels: &mut Labels) {
        self.vad.accept_waveform(window);
        self.processed += window.len() as u64;
        if self.vad.detected() {
            self.onset.get_or_insert(self.processed);
        } else {
            self.onset = None;
        }
        self.drain(labels);
        let settled = self
            .onset
            .unwrap_or(self.processed)
            .saturating_sub(SETTLE_LAG);
        self.silent_until = self.silent_until.max(self.base + settled);
        if self.onset.is_none() && self.vad.is_empty() && self.processed >= RESET_AFTER {
            self.vad.reset();
            self.base += self.processed;
            self.processed = 0;
        }
    }

    fn drain(&mut self, labels: &mut Labels) {
        while let Some(segment) = self.vad.front() {
            let start = u64::try_from(segment.start()).unwrap_or(0);
            let len = u64::try_from(segment.n()).unwrap_or(0);
            let (from, to) = (self.base + start, self.base + start + len);
            if let Some(range) = SampleRange::new(SampleIndex::new(from), SampleIndex::new(to)) {
                labels.segments.push(range);
            }
            self.vad.pop();
        }
    }

    fn labels(&self, mut labels: Labels) -> Labels {
        labels.silent_until = SampleIndex::new(self.silent_until);
        labels
    }
}

impl Detector for SileroDetector {
    fn accept(&mut self, samples: &[f32]) -> Labels {
        let mut labels = Labels::default();
        self.pending.extend_from_slice(samples);
        let pending = std::mem::take(&mut self.pending);
        let (windows, rest) = pending.as_chunks::<WINDOW>();
        for window in windows {
            self.feed_window(window, &mut labels);
        }
        self.pending = rest.to_vec();
        self.labels(labels)
    }

    fn flush(&mut self) -> Labels {
        let mut labels = Labels::default();
        if !self.pending.is_empty() {
            // Pad the last part-window with silence so it's looked at too.
            let mut window = std::mem::take(&mut self.pending);
            window.resize(WINDOW, 0.0);
            self.feed_window(&window, &mut labels);
        }
        self.vad.flush();
        self.drain(&mut labels);
        self.onset = None;
        self.silent_until = self.silent_until.max(self.base + self.processed);
        self.labels(labels)
    }
}
