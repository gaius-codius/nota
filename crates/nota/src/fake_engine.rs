//! `nota engine fake`: the engine child with stand-in models, for nota's
//! end-to-end tests of the final pass without speech models. Built only
//! with the `fake-capture` feature, which nota's own tests turn on.
//!
//! It runs the real child loop and chunker ([`nota_engine::child::run`]):
//! a sample louder than [`LOUD`] is speech, and the text of a chunk is how
//! many samples it holds, so a test can check the text covers the audio,
//! with a word over the whole chunk and one at its end. The first word has
//! the log-probability [`COUNT_LOG_PROB`] and the second none, so a test
//! can see both reach the stored text.
//!
//! Boosted (`--pass hotword`, sent hotwords), it leaves out every term with
//! a character other than an ASCII letter or a space, as the real engine
//! leaves out a term its vocabulary can't split, and says which; its second
//! word is then the first term it kept instead of "samples", so a session
//! whose pack has a term close to "samples" gets a hotword candidate.

use std::io;

use nota_core::lifeline::{self, Tie};
use nota_core::messages::Hotword;
use nota_core::{LogProb, SampleCount, SampleIndex, SampleRange};
use nota_engine::EngineError;
use nota_engine::child::{Detector, Heard, Models, TimedWord, Transcriber, run as run_child};
use nota_engine::chunker::{ChunkerConfig, Labels};

/// The loudness above which a sample is speech.
const LOUD: f32 = 0.01;

/// The log-probability of each chunk's first word, its count.
pub(crate) const COUNT_LOG_PROB: f32 = -0.5;

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
    run_child(
        input,
        io::stdout().lock(),
        chunking,
        || Ok(Stand::default()),
    )
}

/// The stand-in models.
#[derive(Debug, Default)]
struct Stand {
    /// The first hotword it was given, if it was boosted.
    boosted: Option<String>,
}

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
            self.next = self.next.saturating_add(SampleCount::new(1));
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
    /// "N samples", N the chunk's length, as two words: "N" over the whole
    /// chunk, with [`COUNT_LOG_PROB`], and "samples" at its end, with
    /// none. Boosted, the second word is its hotword.
    fn transcribe(&mut self, audio: &[f32]) -> Heard {
        let len = SampleCount::new(audio.len() as u64);
        let word = |text: String, from, log_prob| TimedWord {
            text,
            from,
            to: len,
            log_prob,
        };
        let second = self.boosted.as_deref().unwrap_or("samples");
        Heard {
            text: format!("{} {second}", audio.len()),
            words: vec![
                word(
                    audio.len().to_string(),
                    SampleCount::ZERO,
                    LogProb::new(COUNT_LOG_PROB),
                ),
                word(second.to_owned(), len, None),
            ],
        }
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

    /// Leaves out the terms of `hotwords` it can't "split" (any character
    /// but an ASCII letter or a space) and keeps the first of the rest
    /// (none clears the boost), as a beam search would favour it.
    fn boost(&mut self, hotwords: &[Hotword]) -> Result<Option<Vec<Hotword>>, EngineError> {
        let splits = |hotword: &&Hotword| {
            hotword
                .as_str()
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == ' ')
        };
        let (kept, left_out): (Vec<&Hotword>, Vec<&Hotword>) = hotwords.iter().partition(splits);
        self.boosted = kept.first().map(|hotword| hotword.as_str().to_owned());
        Ok(Some(left_out.into_iter().cloned().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of `stand` for a chunk of three samples.
    fn heard(stand: &mut Stand) -> (String, Vec<String>) {
        let heard = stand.transcribe(&[0.5; 3]);
        (
            heard.text,
            heard.words.into_iter().map(|w| w.text).collect(),
        )
    }

    /// Unboosted, the second word is "samples"; boosted, it's the first
    /// hotword given, until a boost with none clears it.
    #[test]
    fn a_boosted_stand_in_hears_its_first_hotword() {
        let mut stand = Stand::default();
        assert_eq!(
            heard(&mut stand),
            (
                "3 samples".to_owned(),
                vec!["3".to_owned(), "samples".to_owned()]
            )
        );
        let hotwords = ["sampler", "amlodipine"].map(|t| Hotword::new(t).unwrap());
        assert_eq!(stand.boost(&hotwords).unwrap(), Some(Vec::new()));
        assert_eq!(
            heard(&mut stand),
            (
                "3 sampler".to_owned(),
                vec!["3".to_owned(), "sampler".to_owned()]
            )
        );
        stand.boost(&[]).unwrap();
        assert_eq!(heard(&mut stand).0, "3 samples");
    }

    /// A term with a character other than an ASCII letter or a space is
    /// left out and named, as given; the first term kept is boosted.
    #[test]
    fn a_stand_in_leaves_out_terms_it_cannot_split() {
        let mut stand = Stand::default();
        let hotwords = ["Mg2+", "sampler", "β-blocker"].map(|t| Hotword::new(t).unwrap());
        let left_out = stand.boost(&hotwords).unwrap();
        assert_eq!(
            left_out,
            Some(vec![hotwords[0].clone(), hotwords[2].clone()])
        );
        assert_eq!(heard(&mut stand).0, "3 sampler");
    }
}
