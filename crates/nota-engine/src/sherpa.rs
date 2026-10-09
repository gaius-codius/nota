//! The models, on the official sherpa-onnx crate: Silero VAD as a
//! [`Detector`], Parakeet TDT as a [`Transcriber`].
//!
//! sherpa-onnx is C++ (with onnxruntime) linked statically. Its Rust API is
//! safe, but the code under it can still abort the process on bad input, so
//! everything here runs in the engine child and its inputs are checked
//! first: 16 kHz audio only, model files that exist.

use std::path::{Path, PathBuf};
use std::time::Duration;

use nota_core::{SampleCount, SampleIndex, SampleRange, SampleRate};
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, SileroVadModelConfig, VadModelConfig,
    VoiceActivityDetector,
};

use crate::EngineError;
use crate::child::{Detector, Heard, Models, TimedWord, Transcriber};
use crate::chunker::Labels;

/// The only rate the models take.
const RATE_HZ: i32 = 16_000;

/// Silero's window: 512 samples (32 ms) at 16 kHz.
const WINDOW: usize = 512;

/// How far the detector's settled silence trails the audio it has seen, in
/// samples. Speech is confirmed only after 0.25 s of it, and a segment's
/// start is then backdated by that plus two windows (about 0.31 s), so
/// anything older than 0.5 s that isn't in a segment is silence.
const SETTLE_LAG: SampleCount = SampleCount::new(8_000);

/// How long a pause must last before the detector may be reset.
const RESET_PAUSE: SampleCount = SampleCount::new(2 * SETTLE_LAG.get());

/// How far sherpa-onnx backdates a segment's start from where speech is
/// confirmed: two windows plus the minimum speech (0.25 s).
const BACKDATE: SampleCount = SampleCount::new(2 * WINDOW as u64 + 4_000);

/// Reset the detector, during a pause, once it has counted this many
/// samples, about an hour. sherpa-onnx counts samples in an `i32`, which
/// would overflow after 37 hours.
const RESET_AFTER: SampleCount = SampleCount::new(16_000 * 3_600);

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
            base: first,
            processed: SampleCount::ZERO,
            pending: Vec::with_capacity(WINDOW),
            onset: None,
            silent_until: first,
            speech_until: first,
            reset_after: RESET_AFTER,
            reset: false,
            segment_end: first,
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
    fn transcribe(&mut self, audio: &[f32]) -> Heard {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(RATE_HZ, audio);
        self.recognizer.decode(&stream);
        let Some(result) = stream.get_result() else {
            return Heard::default();
        };
        let len = SampleCount::new(audio.len() as u64);
        let words = match (&result.timestamps, &result.durations) {
            (Some(starts), durations) => {
                words_of(&result.tokens, starts, durations.as_deref(), len)
            }
            (None, _) => Vec::new(),
        };
        Heard {
            text: result.text.trim().to_owned(),
            words,
        }
    }
}

/// The words in a recogniser's `tokens`, each started at its first token's
/// time and ended at its last token's time plus its duration (both in
/// seconds from the start of `len` samples of audio), or, without
/// durations, where the next word starts. A token beginning with a space
/// starts a word; punctuation stays with the word before. Words are kept
/// inside the audio, in order and not overlapping. Empty if the times
/// don't match the tokens one for one.
fn words_of(
    tokens: &[String],
    starts: &[f32],
    durations: Option<&[f32]>,
    len: SampleCount,
) -> Vec<TimedWord> {
    if starts.len() != tokens.len() || durations.is_some_and(|d| d.len() != tokens.len()) {
        return Vec::new();
    }
    let sample = |seconds: f32| {
        if seconds.is_nan() || seconds <= 0.0 {
            SampleCount::ZERO
        } else {
            at_seconds(seconds).unwrap_or(len).min(len)
        }
    };
    let mut words: Vec<TimedWord> = Vec::new();
    for (k, token) in tokens.iter().enumerate() {
        let end = durations.map(|d| sample(starts[k] + d[k]));
        let trimmed = token.trim();
        match words.last_mut() {
            Some(word) if !token.starts_with(char::is_whitespace) => {
                word.text.push_str(trimmed);
                if let Some(end) = end {
                    word.to = word.to.max(end);
                }
            }
            _ if trimmed.is_empty() => {}
            last => {
                let floor = last.map_or(SampleCount::ZERO, |word| word.to);
                let from = sample(starts[k]).max(floor);
                words.push(TimedWord {
                    text: trimmed.to_owned(),
                    from,
                    to: end.unwrap_or(from).max(from),
                });
            }
        }
    }
    // Without durations, each word runs on to the next, and the last to
    // the end of the audio.
    if durations.is_none() {
        let mut next = len;
        for word in words.iter_mut().rev() {
            word.to = next.max(word.from);
            next = word.from;
        }
    }
    words
}

/// The sample `seconds` into the audio, to the nearest; `None` if negative,
/// not a number, or too far for a [`Duration`].
fn at_seconds(seconds: f32) -> Option<SampleCount> {
    let half_sample = Duration::from_nanos(31_250);
    let elapsed = Duration::try_from_secs_f32(seconds)
        .ok()?
        .checked_add(half_sample)?;
    SampleCount::started_within(elapsed, SampleRate::SPEECH)
}

/// Silero VAD over one track, reporting in track samples.
pub struct SileroDetector {
    vad: VoiceActivityDetector,
    /// The track sample the detector's own count starts at.
    base: SampleIndex,
    /// Samples fed to the detector since `base`.
    processed: SampleCount,
    /// Samples waiting to fill a window.
    pending: Vec<f32>,
    /// Where (in the detector's count) speech was first detected, while it
    /// still is.
    onset: Option<SampleCount>,
    silent_until: SampleIndex,
    /// The end of the latest speech seen, in track samples.
    speech_until: SampleIndex,
    /// [`RESET_AFTER`], or less in tests.
    reset_after: SampleCount,
    /// Whether the detector has been reset since it started.
    reset: bool,
    /// The end of the latest segment reported, in track samples.
    segment_end: SampleIndex,
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
        self.processed = self
            .processed
            .saturating_add(SampleCount::new(window.len() as u64));
        if self.vad.detected() {
            self.onset.get_or_insert(self.processed);
        } else {
            self.onset = None;
        }
        self.drain(labels);
        let now = self.base.saturating_add(self.processed);
        if self.onset.is_some() {
            self.speech_until = now;
        }
        // In track samples, so a reset (which zeroes `processed`) can't
        // move it.
        let settled = self
            .base
            .saturating_add(self.onset.unwrap_or(self.processed))
            .saturating_sub(SETTLE_LAG);
        self.silent_until = self.silent_until.max(settled);
        // Reset only well into a pause: a speech candidate the model hasn't
        // confirmed yet would otherwise be forgotten.
        let in_pause = now.saturating_count_since(self.speech_until) >= RESET_PAUSE;
        if self.onset.is_none()
            && self.vad.is_empty()
            && in_pause
            && self.processed >= self.reset_after
        {
            self.vad.reset();
            self.reset = true;
            self.base = self.base.saturating_add(self.processed);
            self.processed = SampleCount::ZERO;
        }
    }

    fn drain(&mut self, labels: &mut Labels) {
        while let Some(segment) = self.vad.front() {
            let start = SampleCount::new(u64::try_from(segment.start()).unwrap_or(0));
            let len = SampleCount::new(u64::try_from(segment.n()).unwrap_or(0));
            let mut from = self.base.saturating_add(start);
            let to = from.saturating_add(len);
            // After a reset, sherpa-onnx clamps a backdated start to where
            // it was reset; the speech may have begun up to `BACKDATE`
            // earlier, though not before the speech before it.
            if start == SampleCount::ZERO && self.reset {
                from = from.saturating_sub(BACKDATE).max(self.segment_end);
            }
            self.segment_end = self.segment_end.max(to);
            self.speech_until = self.speech_until.max(to);
            if let Some(range) = SampleRange::new(from, to) {
                labels.segments.push(range);
            }
            self.vad.pop();
        }
    }

    fn labels(&self, mut labels: Labels) -> Labels {
        labels.silent_until = self.silent_until;
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
        // Pad the last part-window with silence, then add a whole window of
        // it: the model keeps some of each window as context, so without
        // the extra one the last samples would never be looked at.
        let mut tail = std::mem::take(&mut self.pending);
        tail.resize(WINDOW * 2, 0.0);
        let (windows, _) = tail.as_chunks::<WINDOW>();
        for window in windows {
            self.feed_window(window, &mut labels);
        }
        self.vad.flush();
        self.drain(&mut labels);
        // Settled silence stays where the windows put it; the chunker
        // treats anything after as possible speech.
        self.labels(labels)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::PathBuf;

    use super::*;

    /// The models and the fixture's audio, or `None` (said on stderr) if
    /// they're absent. Fails instead when `NOTA_REQUIRE_TEST_MODELS=1`.
    fn models_and_fixture() -> Option<(SherpaModels, Vec<f32>)> {
        let root = std::env::var_os("NOTA_TEST_MODELS")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| PathBuf::from(h).join(".local/share/nota/test-models"))
            })
            .unwrap_or_default();
        let wav = root.join("fixtures/invented-lecture.wav");
        let paths = ModelPaths {
            parakeet_dir: root.join("parakeet-tdt-0.6b-v3-int8"),
            vad_model: root.join("silero_vad_v6.onnx"),
            threads: 2,
        };
        if !wav.is_file() || !paths.vad_model.is_file() {
            assert!(
                std::env::var_os("NOTA_REQUIRE_TEST_MODELS").is_none_or(|v| v != "1"),
                "NOTA_REQUIRE_TEST_MODELS=1 but no test models in {}",
                root.display()
            );
            let _ = writeln!(
                std::io::stderr(),
                "skipped: no test models in {}",
                root.display()
            );
            return None;
        }
        let models = SherpaModels::load(&paths).unwrap();
        let wave = sherpa_onnx::Wave::read(wav.to_str().unwrap()).unwrap();
        Some((models, wave.samples().to_vec()))
    }

    /// Runs a detector from `first` over `audio` in 100 ms pieces; returns
    /// every segment and the final `silent_until`, checking the contract on
    /// the way.
    fn detect(mut detector: SileroDetector, first: u64, audio: &[f32]) -> (Vec<(u64, u64)>, u64) {
        let mut segments = Vec::new();
        let mut silent = first;
        let mut fed = first;
        let mut collect = |labels: Labels, fed: u64, segments: &mut Vec<(u64, u64)>| {
            assert!(
                labels.silent_until.get() >= silent,
                "silent_until went back"
            );
            assert!(
                labels.silent_until.get() <= fed,
                "silent_until past the audio"
            );
            silent = labels.silent_until.get();
            for s in labels.segments {
                let (from, to) = (s.start().get(), s.end().get());
                assert!(
                    from >= first && to <= fed + 2 * WINDOW as u64,
                    "{from}..{to}"
                );
                assert!(
                    segments.last().is_none_or(|&(_, end)| end <= from),
                    "out of order"
                );
                segments.push((from, to));
            }
        };
        for piece in audio.chunks(1_600) {
            fed += piece.len() as u64;
            let labels = detector.accept(piece);
            collect(labels, fed, &mut segments);
        }
        let labels = detector.flush();
        collect(labels, fed, &mut segments);
        (segments, silent)
    }

    #[test]
    fn segments_and_settled_silence_fit_the_speech() {
        let Some((models, audio)) = models_and_fixture() else {
            return;
        };
        let first = 48_000;
        let detector = models.detector(SampleIndex::new(first)).unwrap();
        assert!(format!("{detector:?}").starts_with("SileroDetector"));
        assert!(format!("{models:?}").contains("Parakeet"));
        let (segments, silent) = detect(detector, first, &audio);
        // Four sentences; the 0.2 s pause may or may not split two.
        assert!((3..=4).contains(&segments.len()), "{segments:?}");
        // 0.5 s of silence leads.
        assert!(segments[0].0 >= first + 4_000, "{segments:?}");
        // Every loud 30 ms frame is in a segment or past settled silence.
        let end = first + audio.len() as u64;
        assert!(silent + 2 * SETTLE_LAG.get() >= end, "{silent}");
        for (k, frame) in audio.chunks(480).enumerate() {
            // Mean power over 0.01 (an RMS of 0.1), in a whole frame.
            let energy: f32 = frame.iter().map(|s| s * s).sum();
            let at = first + k as u64 * 480;
            if energy > 4.8 {
                let covered = at >= silent
                    || segments
                        .iter()
                        .any(|&(from, to)| from <= at + 480 && at < to);
                assert!(covered, "loud frame at {at} marked silent; {segments:?}");
            }
        }
    }

    #[test]
    fn a_reset_in_a_pause_changes_nothing() {
        let Some((models, audio)) = models_and_fixture() else {
            return;
        };
        let (plain, plain_silent) = detect(models.detector(SampleIndex::ZERO).unwrap(), 0, &audio);
        let mut resetting = models.detector(SampleIndex::ZERO).unwrap();
        resetting.reset_after = SampleCount::new(8_000);
        let (reset, reset_silent) = detect(resetting, 0, &audio);
        assert_eq!(plain.len(), reset.len(), "{plain:?} vs {reset:?}");
        // The same speech, starts possibly widened to cover what a reset
        // hid.
        for (a, b) in plain.iter().zip(&reset) {
            assert!(b.0 <= a.0 + 2 * WINDOW as u64, "{plain:?} vs {reset:?}");
            assert!(a.0 <= b.0 + BACKDATE.get(), "{plain:?} vs {reset:?}");
            assert!(
                a.1.abs_diff(b.1) <= 2 * WINDOW as u64,
                "{plain:?} vs {reset:?}"
            );
        }
        assert!(plain_silent.abs_diff(reset_silent) <= 2 * WINDOW as u64);
    }

    fn tokens(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|&t| t.to_owned()).collect()
    }

    /// The words as (text, from, to) in samples.
    fn spans(words: &[TimedWord]) -> Vec<(&str, u64, u64)> {
        words
            .iter()
            .map(|w| (w.text.as_str(), w.from.get(), w.to.get()))
            .collect()
    }

    #[test]
    fn words_are_grouped_from_parakeets_tokens() {
        // As Parakeet gives them for "Today we look at the sea. When".
        let t = tokens(&[
            " Toda", "y", " we", " look", " at", " the", " se", "a", ".", " W", "hen",
        ]);
        let starts = [
            0.4, 0.72, 0.96, 1.2, 1.44, 1.6, 1.76, 2.08, 2.24, 2.48, 2.64,
        ];
        let durations = [
            0.32, 0.24, 0.24, 0.24, 0.16, 0.16, 0.32, 0.16, 0.24, 0.16, 0.08,
        ];
        let len = SampleCount::new(48_000);
        let words = words_of(&t, &starts, Some(&durations), len);
        assert_eq!(
            spans(&words),
            [
                ("Today", 6_400, 15_360),
                ("we", 15_360, 19_200),
                ("look", 19_200, 23_040),
                ("at", 23_040, 25_600),
                ("the", 25_600, 28_160),
                ("sea.", 28_160, 39_680),
                ("When", 39_680, 43_520),
            ]
        );
        // Without durations, each word runs to the next, the last to the
        // end of the audio.
        let words = words_of(&t[..4], &starts[..4], None, len);
        assert_eq!(
            spans(&words),
            [
                ("Today", 6_400, 15_360),
                ("we", 15_360, 19_200),
                ("look", 19_200, 48_000)
            ]
        );
    }

    #[test]
    fn words_stay_in_the_audio_in_order() {
        let len = SampleCount::new(16_000);
        // A first token without a space starts a word; a token of only
        // space is skipped; times before the start, past the end, or going
        // back are kept in place.
        let t = tokens(&["Hi", " ", " there", " now", " end"]);
        let starts = [-0.5, 0.1, 0.2, 0.05, 2.0];
        let durations = [0.7, 0.0, 0.3, 0.1, 1.0];
        let words = words_of(&t, &starts, Some(&durations), len);
        assert_eq!(
            spans(&words),
            [
                ("Hi", 0, 3_200),
                ("there", 3_200, 8_000),
                ("now", 8_000, 8_000),
                ("end", 16_000, 16_000),
            ]
        );
        // A time that isn't a number is the start.
        let words = words_of(&tokens(&[" a", " b"]), &[f32::NAN, 0.5], None, len);
        assert_eq!(spans(&words), [("a", 0, 8_000), ("b", 8_000, 16_000)]);
        // Times that don't match the tokens give no words.
        assert!(words_of(&t, &starts[..4], None, len).is_empty());
        assert!(words_of(&t, &starts, Some(&durations[..2]), len).is_empty());
        assert!(words_of(&[], &[], None, len).is_empty());
        // A huge time is the end of the audio.
        assert_eq!(at_seconds(f32::MAX), None);
        let words = words_of(&tokens(&[" x"]), &[f32::MAX], None, len);
        assert_eq!(spans(&words), [("x", 16_000, 16_000)]);
        // Seconds to the nearest sample, whatever f32 makes of them.
        assert_eq!(at_seconds(0.72), Some(SampleCount::new(11_520)));
        assert_eq!(at_seconds(0.0), Some(SampleCount::ZERO));
        assert_eq!(at_seconds(-1.0), None);
    }

    #[test]
    fn reset_after_is_an_hour_and_backdate_matches_sherpa() {
        assert_eq!(RESET_AFTER.get(), 57_600_000);
        // sherpa-onnx: start = tail - 2 * window - min_speech_samples.
        assert_eq!(BACKDATE.get(), 5_024);
        assert_eq!(RESET_PAUSE.get(), 16_000);
        assert!(SETTLE_LAG.get() > BACKDATE.get() + 2 * WINDOW as u64);
    }
}
