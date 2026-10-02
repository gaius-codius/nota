//! One track's audio that the engine hasn't confirmed yet: what to resend
//! after a restart, and the checks on the engine's replies about it.
//!
//! Invariants:
//! - entries are in the order the recorder sent them, and every sample in
//!   them is unconfirmed;
//! - text is passed on only once the engine confirms past its end, so a
//!   transcript that the engine dies before confirming is dropped and
//!   redone after the restart, never delivered twice;
//! - a reply that doesn't fit what was sent (text that doesn't start at
//!   the first unconfirmed sample, a confirmation that goes back, or past
//!   the audio sent, or into a gap between streams) is a protocol
//!   violation.

use std::collections::VecDeque;

use nota_core::messages::{AudioChunk, ToEngine, Transcript};
use nota_core::protocol::MAX_AUDIO_SAMPLES;
use nota_core::{SampleCount, SampleIndex, SampleRange, SessionTime, TrackId};

/// One thing to send for the track.
#[derive(Debug)]
enum Entry {
    Audio {
        chunk: AudioChunk,
        /// When the current engine was sent it; `None` if not yet.
        sent_at: Option<SessionTime>,
    },
    /// The end of a stream: the audio after it doesn't follow on.
    Flush { sent: bool },
}

/// A reply about this track that doesn't fit what was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Violation(pub(super) &'static str);

/// The unconfirmed audio of one track.
#[derive(Debug)]
pub(super) struct Replay {
    track: TrackId,
    entries: VecDeque<Entry>,
    /// Transcripts waiting for their confirmation, in order.
    held: Vec<Transcript>,
    /// The sample after the last audio pushed.
    next: Option<SampleIndex>,
    /// Samples in `entries`.
    unconfirmed: u64,
    /// Audio has been pushed since the last flush, so the engine may hold
    /// an open stream even when everything is confirmed.
    open: bool,
    /// When the latest audio was pushed.
    last_audio: Option<SessionTime>,
    /// The first unconfirmed sample when the engine last failed, and how
    /// many failures in a row it has stood still for.
    stuck_at: Option<SampleIndex>,
    strikes: u32,
}

impl Replay {
    pub(super) const fn new(track: TrackId) -> Self {
        Self {
            track,
            entries: VecDeque::new(),
            held: Vec::new(),
            next: None,
            unconfirmed: 0,
            open: false,
            last_audio: None,
            stuck_at: None,
            strikes: 0,
        }
    }

    /// Adds audio. The caller has checked it doesn't start before the end
    /// of the audio pushed before; if it starts after, the stream before is flushed
    /// first. Audio longer than one frame can carry is split.
    pub(super) fn push_audio(&mut self, chunk: &AudioChunk, now: SessionTime) {
        if chunk.samples().is_empty() {
            return;
        }
        self.last_audio = Some(now);
        if self.next.is_some_and(|next| chunk.range().start() > next) {
            self.push_flush();
        }
        let mut at = chunk.range().start();
        for part in chunk.samples().chunks(MAX_AUDIO_SAMPLES) {
            let Some(piece) = AudioChunk::new(self.track, at, chunk.rate(), part.to_vec()) else {
                // Can't happen: the whole chunk's range didn't overflow.
                break;
            };
            at = piece.range().end();
            self.unconfirmed += piece.range().len().get();
            self.entries.push_back(Entry::Audio {
                chunk: piece,
                sent_at: None,
            });
        }
        self.next = Some(chunk.range().end());
        self.open = true;
    }

    /// Asks the engine to transcribe what it holds now.
    pub(super) fn push_flush(&mut self) {
        if self.open {
            self.entries.push_back(Entry::Flush { sent: false });
            self.open = false;
        }
    }

    /// When the latest audio came, if the stream it's in is still open.
    pub(super) fn idle_since(&self) -> Option<SessionTime> {
        self.last_audio.filter(|_| self.open)
    }

    /// Notes that the engine failed. Returns whether it has now failed
    /// `limit` times in a row on this track's audio: sent to it each time,
    /// with the first unconfirmed sample never moving.
    pub(super) fn note_failure(&mut self, limit: u32) -> bool {
        let first = self
            .first_unconfirmed()
            .filter(|_| self.oldest_sent().is_some());
        if first.is_none() || first != self.stuck_at {
            self.strikes = 0;
        }
        self.stuck_at = first;
        if first.is_some() {
            self.strikes += 1;
        }
        let poisoned = self.strikes >= limit;
        if poisoned {
            self.strikes = 0;
        }
        poisoned
    }

    /// Forgets the failures counted against this track.
    pub(super) fn clear_strikes(&mut self) {
        self.strikes = 0;
        self.stuck_at = None;
    }

    /// Drops at least `count` samples of the oldest audio, in whole
    /// entries, unsent: the engines keep failing on it. Returns the ranges
    /// dropped.
    pub(super) fn skip(&mut self, count: SampleCount) -> Vec<SampleRange> {
        let mut dropped: Vec<SampleRange> = Vec::new();
        let mut left = count.get();
        while left > 0 {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            if let Entry::Audio { chunk, .. } = entry {
                let range = chunk.range();
                self.unconfirmed -= range.len().get();
                left = left.saturating_sub(range.len().get());
                join(&mut dropped, range);
            }
        }
        if !dropped.is_empty() {
            self.held.clear();
        }
        dropped
    }

    /// Everything not yet sent to the current engine, in order, marked as
    /// sent at `now`.
    pub(super) fn take_unsent(&mut self, now: SessionTime) -> Vec<ToEngine> {
        let mut out = Vec::new();
        for entry in &mut self.entries {
            match entry {
                Entry::Audio { chunk, sent_at } if sent_at.is_none() => {
                    *sent_at = Some(now);
                    out.push(ToEngine::Audio(chunk.clone()));
                }
                Entry::Flush { sent } if !*sent => {
                    *sent = true;
                    out.push(ToEngine::Flush { track: self.track });
                }
                Entry::Audio { .. } | Entry::Flush { .. } => {}
            }
        }
        out
    }

    /// The engine is gone: nothing has been sent to the next one, and text
    /// it didn't confirm is dropped.
    pub(super) fn reset_sent(&mut self) {
        self.held.clear();
        for entry in &mut self.entries {
            match entry {
                Entry::Audio { sent_at, .. } => *sent_at = None,
                Entry::Flush { sent } => *sent = false,
            }
        }
    }

    /// When the oldest audio the current engine hasn't confirmed was sent.
    pub(super) fn oldest_sent(&self) -> Option<SessionTime> {
        self.entries.iter().find_map(|entry| match entry {
            Entry::Audio { sent_at, .. } => *sent_at,
            Entry::Flush { .. } => None,
        })
    }

    /// The first unconfirmed sample.
    fn first_unconfirmed(&self) -> Option<SampleIndex> {
        self.entries.iter().find_map(|entry| match entry {
            Entry::Audio { chunk, .. } => Some(chunk.range().start()),
            Entry::Flush { .. } => None,
        })
    }

    /// The end of the stream the engine is working on: the run of sent
    /// audio from the first unconfirmed sample that follows on without a
    /// gap, a flush, or audio not yet sent. Replies can't reach past it.
    fn sent_run_end(&self) -> Option<SampleIndex> {
        let mut end = None;
        for entry in self
            .entries
            .iter()
            .skip_while(|e| matches!(e, Entry::Flush { .. }))
        {
            match entry {
                Entry::Audio {
                    chunk,
                    sent_at: Some(_),
                } if end.is_none_or(|end| end == chunk.range().start()) => {
                    end = Some(chunk.range().end());
                }
                Entry::Audio { .. } | Entry::Flush { .. } => break,
            }
        }
        end
    }

    /// Text from the engine. It's held until confirmed.
    pub(super) fn on_transcript(&mut self, transcript: Transcript) -> Result<(), Violation> {
        let expected = self
            .held
            .last()
            .map(|t| t.range.end())
            .or_else(|| self.first_unconfirmed())
            .ok_or(Violation("text for audio not sent"))?;
        if transcript.range.start() != expected {
            return Err(Violation(
                "text doesn't start at the first unconfirmed sample",
            ));
        }
        let past = self
            .sent_run_end()
            .is_none_or(|end| transcript.range.end() > end);
        if transcript.range.is_empty() || past {
            return Err(Violation("text past the audio sent"));
        }
        self.held.push(transcript);
        Ok(())
    }

    /// The engine confirms everything before `up_to`. Returns the text that
    /// is now final, in order.
    pub(super) fn on_confirmed(
        &mut self,
        up_to: SampleIndex,
    ) -> Result<Vec<Transcript>, Violation> {
        let first = self
            .first_unconfirmed()
            .ok_or(Violation("confirmed audio not sent"))?;
        if up_to <= first {
            return Err(Violation("confirmed position goes back"));
        }
        if self.sent_run_end().is_none_or(|end| up_to > end) {
            return Err(Violation("confirmed past the audio sent"));
        }
        if self.held.iter().any(|t| t.range.end() > up_to) {
            return Err(Violation("text runs past its confirmation"));
        }
        self.confirm(up_to);
        Ok(std::mem::take(&mut self.held))
    }

    /// Drops everything before `up_to`, splitting the audio entry it falls
    /// in, and any sent flush markers left at the front.
    fn confirm(&mut self, up_to: SampleIndex) {
        while let Some(front) = self.entries.front_mut() {
            match front {
                Entry::Flush { sent: true } => {}
                Entry::Flush { sent: false } => break,
                Entry::Audio { chunk, .. } => {
                    let range = chunk.range();
                    if range.start() >= up_to {
                        break;
                    }
                    if range.end() > up_to {
                        let Some(kept) = range.end().checked_count_since(up_to) else {
                            break;
                        };
                        let skip = chunk.samples().len() - usize::try_from(kept.get()).unwrap_or(0);
                        let rest = chunk.samples()[skip..].to_vec();
                        if let Some(rest) = AudioChunk::new(self.track, up_to, chunk.rate(), rest) {
                            self.unconfirmed -= range.len().get() - kept.get();
                            *chunk = rest;
                        }
                        break;
                    }
                    self.unconfirmed -= range.len().get();
                }
            }
            self.entries.pop_front();
        }
    }

    /// Drops the oldest audio until at most `keep` samples are left, while
    /// no engine is running. Returns the ranges dropped, never transcribed.
    pub(super) fn trim(&mut self, keep: SampleCount) -> Vec<SampleRange> {
        let mut dropped: Vec<SampleRange> = Vec::new();
        while self.unconfirmed > keep.get() {
            let Some(front) = self.entries.pop_front() else {
                break;
            };
            if let Entry::Audio { chunk, .. } = front {
                self.unconfirmed -= chunk.range().len().get();
                join(&mut dropped, chunk.range());
            }
        }
        if !dropped.is_empty() {
            self.held.clear();
        }
        dropped
    }

    #[cfg(test)]
    fn unconfirmed(&self) -> u64 {
        self.unconfirmed
    }
}

/// Adds `range` to `ranges`, merging it into the last if they touch.
fn join(ranges: &mut Vec<SampleRange>, range: SampleRange) {
    if let Some(last) = ranges.last_mut()
        && last.end() == range.start()
        && let Some(joined) = SampleRange::new(last.start(), range.end())
    {
        *last = joined;
        return;
    }
    ranges.push(range);
}

#[cfg(test)]
mod tests {
    use nota_core::SampleRate;

    use super::*;

    const TRACK: TrackId = TrackId::new(1);

    fn chunk(from: u64, len: usize) -> AudioChunk {
        let samples = (0..len).map(|i| i16::try_from(i % 100).unwrap()).collect();
        AudioChunk::new(TRACK, SampleIndex::new(from), SampleRate::SPEECH, samples).unwrap()
    }

    fn at(n: u64) -> SampleIndex {
        SampleIndex::new(n)
    }

    fn t(from: u64, to: u64) -> Transcript {
        Transcript {
            track: TRACK,
            range: SampleRange::new(at(from), at(to)).unwrap(),
            text: format!("{from}-{to}"),
        }
    }

    fn sent(replay: &mut Replay) -> Vec<ToEngine> {
        replay.take_unsent(SessionTime::ZERO)
    }

    fn starts(frames: &[ToEngine]) -> Vec<Option<u64>> {
        frames
            .iter()
            .map(|f| match f {
                ToEngine::Audio(c) => Some(c.range().start().get()),
                ToEngine::Flush { .. } => None,
            })
            .collect()
    }

    #[test]
    fn text_is_released_only_when_confirmed() {
        let mut replay = Replay::new(TRACK);
        replay.push_audio(&chunk(0, 10), SessionTime::ZERO);
        replay.push_audio(&chunk(10, 10), SessionTime::ZERO);
        sent(&mut replay);
        replay.on_transcript(t(0, 15)).unwrap();
        assert_eq!(replay.on_confirmed(at(15)).unwrap(), [t(0, 15)]);
        assert_eq!(replay.unconfirmed(), 5);
        assert_eq!(replay.first_unconfirmed(), Some(at(15)));
        // Confirmed silence releases nothing.
        assert_eq!(replay.on_confirmed(at(20)).unwrap(), []);
        assert_eq!(replay.unconfirmed(), 0);
        assert_eq!(replay.oldest_sent(), None);
    }

    #[test]
    fn a_restart_resends_from_the_first_unconfirmed_sample() {
        let mut replay = Replay::new(TRACK);
        replay.push_audio(&chunk(0, 10), SessionTime::ZERO);
        replay.push_audio(&chunk(10, 10), SessionTime::ZERO);
        assert_eq!(starts(&sent(&mut replay)), [Some(0), Some(10)]);
        replay.on_confirmed(at(4)).unwrap();
        // Text the engine didn't confirm before dying is dropped.
        replay.on_transcript(t(4, 12)).unwrap();
        replay.reset_sent();
        let resent = sent(&mut replay);
        assert_eq!(starts(&resent), [Some(4), Some(10)]);
        let ToEngine::Audio(first) = &resent[0] else {
            panic!()
        };
        assert_eq!(first.samples(), &chunk(0, 10).samples()[4..]);
        // The redone text comes back once.
        replay.on_transcript(t(4, 20)).unwrap();
        assert_eq!(replay.on_confirmed(at(20)).unwrap(), [t(4, 20)]);
    }

    #[test]
    fn only_new_audio_is_sent_while_running() {
        let mut replay = Replay::new(TRACK);
        replay.push_audio(&chunk(0, 10), SessionTime::ZERO);
        assert_eq!(starts(&sent(&mut replay)), [Some(0)]);
        replay.push_audio(&chunk(10, 10), SessionTime::ZERO);
        replay.push_flush();
        replay.push_flush();
        assert_eq!(starts(&sent(&mut replay)), [Some(10), None]);
        assert!(sent(&mut replay).is_empty());
    }

    #[test]
    fn a_jump_flushes_the_stream_before_it() {
        let mut replay = Replay::new(TRACK);
        replay.push_audio(&chunk(0, 10), SessionTime::ZERO);
        replay.push_audio(&chunk(50, 10), SessionTime::ZERO);
        assert_eq!(replay.next, Some(at(60)));
        assert_eq!(starts(&sent(&mut replay)), [Some(0), None, Some(50)]);
        // Text or a confirmation reaching into the gap, or across it into
        // the next stream, is a violation.
        assert!(replay.on_confirmed(at(30)).is_err());
        assert!(replay.on_confirmed(at(55)).is_err());
        assert!(replay.on_transcript(t(0, 55)).is_err());
        assert!(replay.on_transcript(t(0, 12)).is_err());
        replay.on_confirmed(at(10)).unwrap();
        // The sent flush goes with the stream it ended; the next stream's
        // text starts at its own first sample.
        assert_eq!(replay.first_unconfirmed(), Some(at(50)));
        replay.on_transcript(t(50, 60)).unwrap();
        assert_eq!(replay.on_confirmed(at(60)).unwrap(), [t(50, 60)]);
        assert!(replay.entries.is_empty());
    }

    #[test]
    fn a_jump_after_everything_is_confirmed_still_flushes() {
        let mut replay = Replay::new(TRACK);
        replay.push_audio(&chunk(0, 10), SessionTime::ZERO);
        sent(&mut replay);
        replay.on_confirmed(at(10)).unwrap();
        // The engine may still hold the stream open at 10.
        replay.push_audio(&chunk(50, 10), SessionTime::ZERO);
        assert_eq!(starts(&sent(&mut replay)), [None, Some(50)]);
        // A flush with nothing pushed since the last one sends nothing.
        replay.push_flush();
        replay.push_flush();
        assert_eq!(starts(&sent(&mut replay)), [None]);
        assert!(sent(&mut replay).is_empty());
    }

    #[test]
    fn a_track_that_stands_still_through_failures_is_skipped() {
        let mut replay = Replay::new(TRACK);
        // Nothing sent: failures don't count against the track.
        replay.push_audio(&chunk(0, 10), SessionTime::ZERO);
        assert!(!replay.note_failure(2));
        assert!(!replay.note_failure(2));
        sent(&mut replay);
        assert!(!replay.note_failure(2));
        replay.reset_sent();
        // Progress resets the count.
        sent(&mut replay);
        replay.on_confirmed(at(5)).unwrap();
        assert!(!replay.note_failure(2));
        replay.reset_sent();
        sent(&mut replay);
        assert!(replay.note_failure(2));
        replay.reset_sent();
        replay.push_audio(&chunk(10, 10), SessionTime::ZERO);
        replay.push_audio(&chunk(20, 10), SessionTime::ZERO);
        // At least 8 samples from 5, in whole entries: 5..20.
        assert_eq!(
            replay.skip(SampleCount::new(8)),
            [SampleRange::new(at(5), at(20)).unwrap()]
        );
        assert_eq!(replay.first_unconfirmed(), Some(at(20)));
        assert_eq!(replay.unconfirmed(), 10);
    }

    #[test]
    fn replies_that_dont_fit_are_violations() {
        let mut replay = Replay::new(TRACK);
        assert!(replay.on_confirmed(at(1)).is_err(), "nothing sent");
        assert!(replay.on_transcript(t(0, 1)).is_err(), "nothing sent");
        replay.push_audio(&chunk(100, 10), SessionTime::ZERO);
        assert!(replay.on_confirmed(at(105)).is_err(), "pushed but not sent");
        sent(&mut replay);
        // Audio pushed after the engine was last sent some isn't covered.
        replay.push_audio(&chunk(110, 10), SessionTime::ZERO);
        assert!(
            replay.on_confirmed(at(115)).is_err(),
            "confirmed audio not yet sent"
        );
        assert!(replay.on_confirmed(at(100)).is_err(), "goes nowhere");
        assert!(replay.on_confirmed(at(99)).is_err(), "goes back");
        assert!(replay.on_confirmed(at(111)).is_err(), "past the audio");
        assert!(replay.on_transcript(t(101, 105)).is_err(), "wrong start");
        assert!(replay.on_transcript(t(100, 111)).is_err(), "past the audio");
        assert!(replay.on_transcript(t(100, 100)).is_err(), "empty");
        replay.on_transcript(t(100, 104)).unwrap();
        replay.on_transcript(t(104, 108)).unwrap();
        assert!(
            replay.on_confirmed(at(106)).is_err(),
            "text past its confirmation"
        );
        assert_eq!(
            replay.on_confirmed(at(108)).unwrap(),
            [t(100, 104), t(104, 108)]
        );
    }

    #[test]
    fn long_audio_is_split_into_frames() {
        let mut replay = Replay::new(TRACK);
        replay.push_audio(&chunk(7, MAX_AUDIO_SAMPLES * 2 + 1), SessionTime::ZERO);
        let frames = sent(&mut replay);
        let lens: Vec<_> = frames
            .iter()
            .map(|f| match f {
                ToEngine::Audio(c) => c.samples().len(),
                ToEngine::Flush { .. } => 0,
            })
            .collect();
        assert_eq!(lens, [MAX_AUDIO_SAMPLES, MAX_AUDIO_SAMPLES, 1]);
        assert_eq!(replay.unconfirmed(), MAX_AUDIO_SAMPLES as u64 * 2 + 1);
    }

    #[test]
    fn trimming_drops_the_oldest_audio_and_says_what() {
        let mut replay = Replay::new(TRACK);
        replay.push_audio(&chunk(0, 10), SessionTime::ZERO);
        replay.push_audio(&chunk(10, 10), SessionTime::ZERO);
        replay.push_audio(&chunk(40, 10), SessionTime::ZERO);
        replay.push_audio(&chunk(50, 10), SessionTime::ZERO);
        assert_eq!(replay.trim(SampleCount::new(40)), []);
        let dropped = replay.trim(SampleCount::new(15));
        assert_eq!(
            dropped,
            [
                SampleRange::new(at(0), at(20)).unwrap(),
                SampleRange::new(at(40), at(50)).unwrap()
            ]
        );
        assert_eq!(replay.unconfirmed(), 10);
        assert_eq!(starts(&sent(&mut replay)), [Some(50)]);
    }

    #[test]
    fn oldest_sent_is_the_first_audio_still_unconfirmed() {
        let mut replay = Replay::new(TRACK);
        replay.push_audio(&chunk(0, 10), SessionTime::ZERO);
        replay.take_unsent(SessionTime::from_nanos(5));
        replay.push_audio(&chunk(10, 10), SessionTime::ZERO);
        replay.take_unsent(SessionTime::from_nanos(9));
        assert_eq!(replay.oldest_sent(), Some(SessionTime::from_nanos(5)));
        replay.on_confirmed(at(10)).unwrap();
        assert_eq!(replay.oldest_sent(), Some(SessionTime::from_nanos(9)));
        replay.reset_sent();
        assert_eq!(replay.oldest_sent(), None);
    }
}
