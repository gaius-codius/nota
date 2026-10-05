//! Heard speech placed in session time, so text from several tracks merges
//! into one transcript.
//!
//! The engine locates text by sample, per track ([`Transcript`]). Each
//! track's samples map to session time through its own [`TrackTimeline`],
//! epoch by epoch, so placing a transcript needs the timeline of its track
//! as it stood when the audio was recorded. Once placed, utterances from any
//! track compare by when they were spoken: sorting them is the merge.

use crate::epoch::TrackTimeline;
use crate::ids::TrackId;
use crate::messages::Transcript;
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
        if transcript.track() != timeline.track() {
            return None;
        }
        let (start, end) = timeline.span_of(transcript.range())?;
        Some(Self {
            start,
            end,
            track: transcript.track(),
            text: transcript.into_text(),
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

#[cfg(test)]
mod tests;
