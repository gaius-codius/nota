//! Messages between the recorder and the speech engine, as plain Rust types.
//!
//! The engine child (`nota engine asr`) gets audio and returns text, both
//! located by sample, never by session time: the recorder maps samples to
//! session time through the track's epochs, so the engine needn't know about
//! them. How these are put on the wire is the engine protocol's business.

use crate::ids::TrackId;
use crate::time::{SampleCount, SampleIndex, SampleRange, SampleRate};

/// A version of the engine protocol. Both sides check it before anything
/// else, so a mismatched engine is refused rather than misread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion(u16);

impl ProtocolVersion {
    /// The version these messages are.
    pub const CURRENT: Self = Self(0);

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
}

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

/// Text the engine heard in a run of one track's samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// The track the speech is from.
    pub track: TrackId,
    /// The samples the text covers.
    pub range: SampleRange,
    /// What the engine heard, before any correction.
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn protocol_version() {
        assert_eq!(ProtocolVersion::CURRENT.get(), 0);
        assert_eq!(ProtocolVersion::new(3).get(), 3);
        assert_ne!(ProtocolVersion::new(1), ProtocolVersion::CURRENT);
    }
}
