//! Heard speech placed in session time, so text from several tracks merges
//! into one transcript.
//!
//! The engine locates text by sample, per track ([`Transcript`]). Each
//! track's samples map to session time through its own [`TrackTimeline`],
//! epoch by epoch, so placing a transcript needs the timeline of its track
//! as it stood when the audio was recorded. Once placed, utterances from any
//! track compare by when they were spoken: sorting them is the merge. A
//! transcript's words are placed the same way, each within its
//! utterance's span ([`Utterance::place_with_words`]).

use crate::epoch::TrackTimeline;
use crate::ids::TrackId;
use crate::messages::{HeardWord, Transcript};
use crate::time::SessionTime;

/// What one track heard, and when, in session time.
///
/// Utterances order by start, then end, then track, then text: sorted, they
/// read in the order they were spoken, whichever track they came from and
/// in whatever order they were transcribed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Utterance {
    start: SessionTime,
    end: SessionTime,
    track: TrackId,
    text: String,
}

impl Utterance {
    /// Places `transcript` in session time through its track's `timeline`.
    /// `None` if the timeline is another track's, or doesn't reach the
    /// transcript's samples (see [`TrackTimeline::span_of`]).
    #[must_use]
    pub fn place(transcript: Transcript, timeline: &TrackTimeline) -> Option<Self> {
        Self::place_with_words(transcript, timeline).map(|(utterance, _)| utterance)
    }

    /// Places `transcript` as [`Utterance::place`] does, and each of its
    /// words through the same timeline. Every word lies within the
    /// utterance's span, in order: each starts no earlier than the one
    /// before ended, and a word the timeline can't time (which needs a time
    /// past what a [`SessionTime`] holds) runs from there to the
    /// utterance's end.
    #[must_use]
    pub fn place_with_words(
        transcript: Transcript,
        timeline: &TrackTimeline,
    ) -> Option<(Self, Vec<Word>)> {
        if transcript.track() != timeline.track() {
            return None;
        }
        let (start, end) = timeline.span_of(transcript.range())?;
        let track = transcript.track();
        let (text, heard) = transcript.into_text_and_words();
        let utterance = Self {
            start,
            end,
            track,
            text,
        };
        let mut floor = utterance.start;
        let words = heard
            .iter()
            .map(|word| {
                let placed = utterance.place_word(word, timeline, floor);
                floor = placed.end;
                placed
            })
            .collect();
        Some((utterance, words))
    }

    /// `word` in session time, within the utterance's span and starting no
    /// earlier than `floor`, where the word before it ended.
    fn place_word(&self, word: &HeardWord, timeline: &TrackTimeline, floor: SessionTime) -> Word {
        let within = |at: SessionTime| at.clamp(floor, self.end);
        let range = word.range();
        let start = timeline.time_of(range.start()).map_or(floor, within);
        let end = if range.is_empty() {
            start
        } else {
            timeline
                .span_of(range)
                .map_or(self.end, |(_, end)| within(end))
                .max(start)
        };
        Word {
            text: word.text().to_owned(),
            start,
            end,
        }
    }

    /// `text`, heard on `track` from `start` to `end`, as stored once
    /// placed. `None` if it ends before it starts.
    #[must_use]
    pub fn new(track: TrackId, start: SessionTime, end: SessionTime, text: String) -> Option<Self> {
        if end < start {
            return None;
        }
        Some(Self {
            start,
            end,
            track,
            text,
        })
    }

    /// When the speech started.
    #[must_use]
    pub const fn start(&self) -> SessionTime {
        self.start
    }

    /// When the speech ended.
    #[must_use]
    pub const fn end(&self) -> SessionTime {
        self.end
    }

    /// The track it was heard on.
    #[must_use]
    pub const fn track(&self) -> TrackId {
        self.track
    }

    /// What was heard, before any correction.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The text, without the rest.
    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }
}

/// One word of an utterance, and when it was said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    text: String,
    start: SessionTime,
    end: SessionTime,
}

impl Word {
    /// `text`, said from `start` to `end`. `None` if it ends before it
    /// starts.
    #[must_use]
    pub fn new(text: String, start: SessionTime, end: SessionTime) -> Option<Self> {
        (start <= end).then_some(Self { text, start, end })
    }

    /// The word.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// When it started.
    #[must_use]
    pub const fn start(&self) -> SessionTime {
        self.start
    }

    /// When it ended.
    #[must_use]
    pub const fn end(&self) -> SessionTime {
        self.end
    }
}

#[cfg(test)]
mod tests;
