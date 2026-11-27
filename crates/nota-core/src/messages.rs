//! Messages between the recorder and the speech engine, as plain Rust types.
//!
//! The engine child (`nota engine asr`) gets audio and returns text, both
//! located by sample, never by session time: the recorder maps samples to
//! session time through the track's epochs, so the engine needn't know about
//! them. How these are put on the wire is [`crate::protocol`]'s business.

use crate::ids::TrackId;
use crate::log_prob::LogProb;
use crate::time::{SampleCount, SampleIndex, SampleRange, SampleRate};

/// A version of the engine protocol. Both sides check it before anything
/// else, so a mismatched engine is refused rather than misread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion(u16);

impl ProtocolVersion {
    /// The version these messages are.
    pub const CURRENT: Self = Self(4);

    /// The version before a boosting engine said which hotwords it left
    /// out.
    pub const WITHOUT_LEFT_OUT: Self = Self(3);

    /// The version before the recorder could give the engine hotwords.
    pub const WITHOUT_HOTWORDS: Self = Self(2);

    /// The version before words carried their log-probabilities.
    pub const WITHOUT_LOG_PROBS: Self = Self(1);

    /// The version before transcripts carried their words' samples.
    pub const WITHOUT_WORDS: Self = Self(0);

    /// The version numbered `version`, as read from the other side.
    #[must_use]
    pub const fn new(version: u16) -> Self {
        Self(version)
    }

    /// The version's number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// From the recorder to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToEngine {
    /// The next audio for a track.
    Audio(AudioChunk),
    /// Transcribe what's buffered for a track now, without waiting for a
    /// pause (the track stopped or its epoch ended).
    Flush {
        /// The track to flush.
        track: TrackId,
    },
    /// The terms a hotword-boosted engine boosts, for every chunk from now
    /// on: the hotword pass sends them first to each engine it starts. An
    /// engine that doesn't boost ignores them.
    Hotwords(Vec<Hotword>),
}

/// The most hotwords one [`ToEngine::Hotwords`] carries: more than a whole
/// pack (the pharmacy pack has about 3,900 terms), and with each at
/// [`MAX_HOTWORD_LEN`] the message still fits a frame.
pub const MAX_HOTWORDS: usize = 10_000;

/// The longest hotword, in bytes of UTF-8. Terms are words or short
/// phrases; this bounds the message and the engine's work on each.
pub const MAX_HOTWORD_LEN: usize = 100;

/// A term the engine can be given as a hotword: the text sherpa-onnx's
/// per-stream hotwords format can't misread. That format splits phrases at
/// `/` and newlines, reads a word starting with `:` as a boost score, and
/// passes the text through a C string, which a NUL would cut (the pinned
/// crate panics on one). So a hotword is 1 to [`MAX_HOTWORD_LEN`] bytes of
/// words separated by single spaces, holding no `/`, no control character
/// and no word starting with `:`. Only [`Hotword::new`] makes one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hotword(String);

impl Hotword {
    /// `text` as a hotword.
    ///
    /// # Errors
    ///
    /// [`BadHotword`] if it breaks one of the rules in [`Hotword`]'s docs.
    pub fn new(text: &str) -> Result<Self, BadHotword> {
        if text.is_empty() || text.len() > MAX_HOTWORD_LEN {
            return Err(BadHotword::Length);
        }
        if text.chars().any(|c| c == '/' || c.is_control()) {
            return Err(BadHotword::Character);
        }
        let words: Vec<&str> = text.split(' ').collect();
        if words
            .iter()
            .any(|w| w.is_empty() || w.chars().any(char::is_whitespace))
        {
            return Err(BadHotword::Spacing);
        }
        if words.iter().any(|w| w.starts_with(':')) {
            return Err(BadHotword::ScoreLike);
        }
        Ok(Self(text.to_owned()))
    }

    /// The term's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a term can't be a [`Hotword`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadHotword {
    /// It's empty or longer than [`MAX_HOTWORD_LEN`] bytes.
    Length,
    /// It holds a `/` or a control character (a newline or a NUL among
    /// them), which the per-stream format would misread.
    Character,
    /// Its words aren't separated by single spaces: a leading, trailing or
    /// doubled space, or other whitespace.
    Spacing,
    /// A word starts with `:`, which the format reads as a boost score.
    ScoreLike,
}

impl std::fmt::Display for BadHotword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Length => "a hotword must be 1 to 100 bytes",
            Self::Character => "a hotword can't hold a slash or a control character",
            Self::Spacing => "a hotword's words must be separated by single spaces",
            Self::ScoreLike => "a hotword's words can't start with a colon",
        })
    }
}

impl std::error::Error for BadHotword {}

/// From the engine to the recorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromEngine {
    /// Text for a run of a track's samples.
    Transcript(Transcript),
    /// Every sample of `track` before `up_to` has been transcribed. After a
    /// restart, the recorder resends audio from here.
    Confirmed {
        /// The track.
        track: TrackId,
        /// The first sample not yet confirmed.
        up_to: SampleIndex,
    },
    /// The terms of the last [`ToEngine::Hotwords`] that a boosting engine
    /// left out, each as it was given: it can't boost them, so no chunk
    /// from now on favours them. Sent once for each list a boosting
    /// engine is given, with no terms if it boosts them all; an engine
    /// that doesn't boost sends none. At most [`MAX_HOTWORDS`].
    LeftOut(Vec<Hotword>),
}

/// A run of one track's audio: 16-bit mono PCM, contiguous from the start of
/// [`AudioChunk::range`]. The range always holds exactly as many samples as
/// the audio. The recorder ends a chunk at every epoch boundary, so one
/// chunk never spans a reopened stream or a change of rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioChunk {
    track: TrackId,
    range: SampleRange,
    rate: SampleRate,
    samples: Vec<i16>,
}

impl AudioChunk {
    /// `samples` from `track`, the first of them at `first_sample`. `None`
    /// if the last sample's index would overflow.
    #[must_use]
    pub fn new(
        track: TrackId,
        first_sample: SampleIndex,
        rate: SampleRate,
        samples: Vec<i16>,
    ) -> Option<Self> {
        let len = SampleCount::new(u64::try_from(samples.len()).ok()?);
        let range = SampleRange::starting_at(first_sample, len)?;
        Some(Self {
            track,
            range,
            rate,
            samples,
        })
    }

    /// The track the audio is from.
    #[must_use]
    pub const fn track(&self) -> TrackId {
        self.track
    }

    /// The samples the chunk covers.
    #[must_use]
    pub const fn range(&self) -> SampleRange {
        self.range
    }

    /// The sampling rate of the audio.
    #[must_use]
    pub const fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The audio itself.
    #[must_use]
    pub fn samples(&self) -> &[i16] {
        &self.samples
    }
}

/// One word the engine heard, the samples it was said in, and how sure
/// the engine was of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeardWord {
    text: String,
    range: SampleRange,
    log_prob: Option<LogProb>,
}

impl HeardWord {
    /// `text`, said in `range`, with no log-probability. `None` if `text`
    /// is empty: a word is something heard. The range may be empty, for a
    /// word the model placed at an instant.
    #[must_use]
    pub fn new(text: String, range: SampleRange) -> Option<Self> {
        (!text.is_empty()).then_some(Self {
            text,
            range,
            log_prob: None,
        })
    }

    /// The word with `log_prob` as its log-probability: its tokens'
    /// lowest, or `None` if the engine didn't give one.
    #[must_use]
    pub fn with_log_prob(self, log_prob: Option<LogProb>) -> Self {
        Self { log_prob, ..self }
    }

    /// The word.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The samples it was said in.
    #[must_use]
    pub const fn range(&self) -> SampleRange {
        self.range
    }

    /// The lowest log-probability of its tokens, punctuation left out;
    /// `None` if unknown.
    #[must_use]
    pub const fn log_prob(&self) -> Option<LogProb> {
        self.log_prob
    }
}

/// Text the engine heard in a run of one track's samples. The run is never
/// empty. Its words, if the engine gave them, lie inside the run, in
/// order, and don't overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    track: TrackId,
    range: SampleRange,
    text: String,
    words: Vec<HeardWord>,
}

impl Transcript {
    /// `text`, heard in `range` of `track`, without words. `None` if the
    /// range is empty: text is always heard in some audio.
    #[must_use]
    pub fn new(track: TrackId, range: SampleRange, text: String) -> Option<Self> {
        if range.is_empty() {
            return None;
        }
        Some(Self {
            track,
            range,
            text,
            words: Vec::new(),
        })
    }

    /// The transcript with `words` as its words, if each lies inside its
    /// range and starts no earlier than the one before ends; otherwise
    /// the transcript back, unchanged.
    ///
    /// # Errors
    ///
    /// The transcript as it was, if the words don't fit it.
    pub fn with_words(self, words: Vec<HeardWord>) -> Result<Self, Self> {
        let mut from = self.range.start();
        for word in &words {
            if word.range.start() < from || word.range.end() > self.range.end() {
                return Err(self);
            }
            from = word.range.end();
        }
        Ok(Self { words, ..self })
    }

    /// The track the speech is from.
    #[must_use]
    pub const fn track(&self) -> TrackId {
        self.track
    }

    /// The samples the text covers.
    #[must_use]
    pub const fn range(&self) -> SampleRange {
        self.range
    }

    /// What the engine heard, before any correction.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Its words, in order; empty if the engine didn't give them.
    #[must_use]
    pub fn words(&self) -> &[HeardWord] {
        &self.words
    }

    /// The text, without the rest.
    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }

    /// The text and the words, without the rest.
    #[must_use]
    pub fn into_text_and_words(self) -> (String, Vec<HeardWord>) {
        (self.text, self.words)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A term of words separated by single spaces is a hotword, as given.
    #[test]
    fn a_plain_term_is_a_hotword() {
        let hotword = Hotword::new("varicose veins").unwrap();
        assert_eq!(hotword.as_str(), "varicose veins");
        assert!(Hotword::new(&"a".repeat(MAX_HOTWORD_LEN)).is_ok());
    }

    /// Each text the per-stream format would misread is refused, for the
    /// reason that names it.
    #[test]
    fn a_term_the_format_would_misread_is_refused() {
        let refused = [
            ("", BadHotword::Length),
            (&"a".repeat(MAX_HOTWORD_LEN + 1), BadHotword::Length),
            ("mmol/L", BadHotword::Character),
            ("amlo\0dipine", BadHotword::Character),
            ("amlo\ndipine", BadHotword::Character),
            ("amlo\tdipine", BadHotword::Character),
            (" amlodipine", BadHotword::Spacing),
            ("amlodipine ", BadHotword::Spacing),
            ("varicose  veins", BadHotword::Spacing),
            ("varicose\u{a0}veins", BadHotword::Spacing),
            ("dose :2.0", BadHotword::ScoreLike),
        ];
        let got: Vec<(String, Result<Hotword, BadHotword>)> = refused
            .iter()
            .map(|(text, _)| ((*text).to_owned(), Hotword::new(text)))
            .collect();
        let want: Vec<(String, Result<Hotword, BadHotword>)> = refused
            .iter()
            .map(|(text, why)| ((*text).to_owned(), Err(*why)))
            .collect();
        assert_eq!(got, want);
    }

    /// Each reason a term is refused reads as the rule it broke, with the
    /// length limit's real value.
    #[test]
    fn each_bad_hotword_says_which_rule() {
        let got = [
            BadHotword::Length,
            BadHotword::Character,
            BadHotword::Spacing,
            BadHotword::ScoreLike,
        ]
        .map(|why| why.to_string());
        assert_eq!(
            got,
            [
                "a hotword must be 1 to 100 bytes",
                "a hotword can't hold a slash or a control character",
                "a hotword's words must be separated by single spaces",
                "a hotword's words can't start with a colon",
            ]
        );
        assert_eq!(MAX_HOTWORD_LEN, 100);
    }

    #[test]
    fn chunk_range_matches_its_audio() {
        let chunk = AudioChunk::new(
            TrackId::new(1),
            SampleIndex::new(100),
            SampleRate::SPEECH,
            vec![0; 3],
        )
        .unwrap();
        assert_eq!(chunk.track(), TrackId::new(1));
        assert_eq!(chunk.rate(), SampleRate::SPEECH);
        assert_eq!(chunk.samples(), &[0, 0, 0]);
        assert_eq!(chunk.range().start(), SampleIndex::new(100));
        assert_eq!(chunk.range().end(), SampleIndex::new(103));
        assert_eq!(chunk.range().len(), SampleCount::new(3));
    }

    #[test]
    fn chunk_refuses_an_end_that_overflows() {
        let last = SampleIndex::new(u64::MAX);
        assert_eq!(
            AudioChunk::new(TrackId::new(0), last, SampleRate::SPEECH, vec![0]),
            None
        );
        let empty = AudioChunk::new(TrackId::new(0), last, SampleRate::SPEECH, Vec::new()).unwrap();
        assert!(empty.range().is_empty());
    }

    #[test]
    fn transcript_covers_some_audio() {
        let range = |a, b| SampleRange::new(SampleIndex::new(a), SampleIndex::new(b)).unwrap();
        let t = Transcript::new(TrackId::new(2), range(4, 9), "hello".to_owned()).unwrap();
        assert_eq!(t.track(), TrackId::new(2));
        assert_eq!(t.range(), range(4, 9));
        assert_eq!(t.text(), "hello");
        assert_eq!(t.into_text(), "hello");
        assert_eq!(
            Transcript::new(TrackId::new(2), range(4, 4), "x".to_owned()),
            None
        );
        assert!(Transcript::new(TrackId::new(2), range(4, 5), String::new()).is_some());
    }

    #[test]
    fn words_lie_inside_the_transcript_in_order() {
        let range = |a, b| SampleRange::new(SampleIndex::new(a), SampleIndex::new(b)).unwrap();
        let word = |a, b| HeardWord::new("w".to_owned(), range(a, b)).unwrap();
        let t = || Transcript::new(TrackId::new(1), range(10, 20), "w w".to_owned()).unwrap();
        assert_eq!(HeardWord::new(String::new(), range(10, 12)), None);
        assert_eq!(word(3, 4).text(), "w");
        assert_eq!(word(3, 4).range(), range(3, 4));
        assert_eq!(word(3, 4).log_prob(), None);
        let sure = LogProb::new(-0.25);
        assert_eq!(word(3, 4).with_log_prob(sure).log_prob(), sure);
        // Touching words, the first at the start, the last at the end, and
        // an instant between them.
        let words = vec![word(10, 12), word(12, 12), word(12, 20)];
        let with = t().with_words(words.clone()).unwrap();
        assert_eq!(with.words(), words.as_slice());
        assert_eq!(with.into_text_and_words(), ("w w".to_owned(), words));
        assert!(t().words().is_empty());
        assert!(t().with_words(Vec::new()).unwrap().words().is_empty());
        // Before the start, past the end, overlapping, out of order.
        for words in [
            vec![word(9, 12)],
            vec![word(18, 21)],
            vec![word(10, 13), word(12, 14)],
            vec![word(14, 15), word(11, 12)],
        ] {
            assert_eq!(t().with_words(words), Err(t()));
        }
    }

    #[test]
    fn protocol_version() {
        assert_eq!(ProtocolVersion::CURRENT.get(), 4);
        assert_eq!(ProtocolVersion::WITHOUT_LEFT_OUT.get(), 3);
        assert_eq!(ProtocolVersion::WITHOUT_HOTWORDS.get(), 2);
        assert_eq!(ProtocolVersion::WITHOUT_LOG_PROBS.get(), 1);
        assert_eq!(ProtocolVersion::WITHOUT_WORDS.get(), 0);
        assert_eq!(ProtocolVersion::new(4).get(), 4);
        assert_ne!(ProtocolVersion::WITHOUT_LEFT_OUT, ProtocolVersion::CURRENT);
        assert_ne!(ProtocolVersion::WITHOUT_HOTWORDS, ProtocolVersion::CURRENT);
        assert_ne!(ProtocolVersion::WITHOUT_WORDS, ProtocolVersion::CURRENT);
        assert_ne!(ProtocolVersion::WITHOUT_LOG_PROBS, ProtocolVersion::CURRENT);
    }
}
