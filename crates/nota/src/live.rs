//! What the screen shows while recording, worked out from what the
//! recorder and the engine report: levels, bytes recorded, and the text,
//! each track's placed in session time through that track's epochs.
//!
//! [`Live`] does no I/O: it says what to send where, and the live thread
//! (in `record`) sends it. So each track's audio reaches the engine in the
//! order it was recorded, and its text is placed through the epochs the
//! recorder had opened by then: an epoch is always reported before the
//! audio recorded in it, and text only ever follows its audio.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use nota_core::messages::{AudioChunk, Transcript};
use nota_core::recorder::{Event, Level};
use nota_core::{SessionTime, TrackId, TrackTimeline, Utterance, Word};
use nota_recorder::capture::{CaptureNotice, RecorderEvent};
use nota_recorder::engine::EngineEvent;

use crate::inhibit::Slept;

/// How often each track's level is sent: well within the screen's 250 ms.
const LEVEL_EVERY: Duration = Duration::from_millis(100);

/// How far apart two tracks' first audio after a sleep can be and still be
/// the same sleep: each stream wakes on its own, within a buffer or two.
const SAME_SLEEP: Duration = Duration::from_secs(2);

/// What to do about one report.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Actions {
    /// Audio for the engine.
    pub(crate) transcribe: Option<AudioChunk>,
    /// A track whose epoch ended: the engine should transcribe what it
    /// holds of it now, so text doesn't run across the gap.
    pub(crate) flush: Option<TrackId>,
    /// Events for the screen.
    pub(crate) updates: Vec<Event>,
    /// Text the engine heard, with its words, placed in session time, to
    /// store. The screen gets the text (without the words) in `updates`.
    pub(crate) heard: Option<(Utterance, Vec<Word>)>,
}

/// A track's level since it was last sent.
#[derive(Debug, Default)]
struct Meter {
    peak: u16,
    sent_at: Option<SessionTime>,
}

/// The live view's state: a timeline following each track's epochs, and
/// each track's level meter.
#[derive(Debug)]
pub(crate) struct Live {
    followers: BTreeMap<TrackId, TrackTimeline>,
    meters: BTreeMap<TrackId, Meter>,
    /// Every sample captured, on every track, for the footer's size. The
    /// samples a broken journal dropped are counted too: nothing reports
    /// how many there were. The summary after the stop says a journal
    /// broke; the screen doesn't show warnings yet.
    recorded_samples: u64,
    /// The tracks whose stream reported a suspend and hasn't sent audio
    /// since.
    sleeping: BTreeSet<TrackId>,
    /// The sleeps seen so far, oldest first.
    slept: Vec<Slept>,
}

impl Live {
    /// Following `timelines`, as the recorder starts with them, and each
    /// track that joins later from its first epoch.
    pub(crate) fn new(timelines: &[TrackTimeline]) -> Self {
        Self {
            followers: timelines.iter().map(|t| (t.track(), t.clone())).collect(),
            meters: BTreeMap::new(),
            recorded_samples: 0,
            sleeping: BTreeSet::new(),
            slept: Vec::new(),
        }
    }

    /// The sleeps seen so far, oldest first, one for each time the machine
    /// slept.
    pub(crate) fn slept(&self) -> &[Slept] {
        &self.slept
    }

    /// What to do about something the recorder reported about `track`.
    pub(crate) fn recorder(&mut self, track: Option<TrackId>, event: RecorderEvent) -> Actions {
        let mut actions = Actions::default();
        match (track, event) {
            (_, RecorderEvent::Audio(chunk)) => {
                self.recorded_samples = self
                    .recorded_samples
                    .saturating_add(chunk.range().len().get());
                actions.updates.extend(self.woke_with(&chunk));
                if let Some(level) = self.level(&chunk) {
                    actions.updates.push(level);
                    // Two bytes a sample, as the journals hold it.
                    actions
                        .updates
                        .push(Event::Recorded(self.recorded_samples.saturating_mul(2)));
                }
                actions.transcribe = Some(chunk);
            }
            (Some(track), RecorderEvent::Epoch(epoch)) => {
                // A follower that refuses is out of step; its text is then
                // placed by the epochs it has, which is the best it knows.
                if let Some(follower) = self.followers.get_mut(&track) {
                    let _ = follower.follow(&epoch);
                    actions.flush = Some(track);
                } else {
                    // The track joined: its first epoch, numbered above
                    // any a resumed session used, and nothing yet to flush.
                    self.followers
                        .insert(track, TrackTimeline::following(track, &epoch));
                }
            }
            // The first audio after it says when the sleep ended.
            (Some(track), RecorderEvent::Capture(CaptureNotice::Suspended)) => {
                self.sleeping.insert(track);
            }
            // The stream stopped: what the engine holds of it won't grow.
            (Some(track), RecorderEvent::CaptureFailed(_)) => actions.flush = Some(track),
            _ => {}
        }
        actions
    }

    /// What the screen is told when `chunk` is the first audio of a track
    /// after the machine slept: the warning, once for the sleep however
    /// many tracks woke from it, and the track's new epoch and the gap
    /// before it. The epoch is the capture's own, opened before this audio
    /// ([`RecorderEvent::Epoch`]). If it didn't start after a gap (the
    /// timeline took the stretch as drift, or refused a new epoch), only
    /// the warning is sent. The warning isn't cleared: the sleep is over,
    /// and the screen decides how long to show it.
    fn woke_with(&mut self, chunk: &AudioChunk) -> Vec<Event> {
        let track = chunk.track();
        if !self.sleeping.remove(&track) {
            return Vec::new();
        }
        let Some(timeline) = self.followers.get(&track) else {
            return Vec::new();
        };
        let Some((resumed, _)) = timeline.span_of(chunk.range()) else {
            return Vec::new();
        };
        let epoch = timeline.epoch_of(chunk.range().start()).copied();
        let gap = epoch.and_then(|e| timeline.gaps().find(|gap| gap.to() == e.start()));
        let mut updates = Vec::new();
        if !self.same_sleep_as_last(resumed) {
            let slept = Slept { resumed, gap };
            updates.push(slept.warning());
            self.slept.push(slept);
        }
        if let (Some(epoch), Some(gap)) = (epoch, gap) {
            updates.push(Event::Epoch { track, epoch });
            updates.push(Event::Gap { track, gap });
        }
        updates
    }

    /// Whether a sleep that ended with audio at `resumed` is the last one
    /// seen, reached by another track.
    fn same_sleep_as_last(&self, resumed: SessionTime) -> bool {
        self.slept.last().is_some_and(|last| {
            let apart = resumed
                .checked_duration_since(last.resumed)
                .or_else(|| last.resumed.checked_duration_since(resumed));
            apart.is_some_and(|apart| apart <= SAME_SLEEP)
        })
    }

    /// What to do about something the engine reported.
    pub(crate) fn engine(&self, event: EngineEvent) -> Actions {
        let mut actions = Actions::default();
        if let EngineEvent::Transcript(transcript) = event
            && let Some((text, words)) = self.place(transcript)
        {
            actions.updates.push(Event::Text(text.clone()));
            actions.heard = Some((text, words));
        }
        actions
    }

    /// `transcript` and its words placed in session time.
    fn place(&self, transcript: Transcript) -> Option<(Utterance, Vec<Word>)> {
        let timeline = self.followers.get(&transcript.track())?;
        Utterance::place_with_words(transcript, timeline)
    }

    /// The track's level, if it's due: the peak since the last one, at the
    /// session time `chunk` ends.
    fn level(&mut self, chunk: &AudioChunk) -> Option<Event> {
        let track = chunk.track();
        let (_, end) = self.followers.get(&track)?.span_of(chunk.range())?;
        let meter = self.meters.entry(track).or_default();
        meter.peak = meter.peak.max(Level::of_samples(chunk.samples()).peak());
        let due = meter
            .sent_at
            .and_then(|at| at.checked_add(LEVEL_EVERY))
            .is_none_or(|next| end >= next);
        if !due {
            return None;
        }
        let level = Level::from_peak(meter.peak);
        meter.peak = 0;
        meter.sent_at = Some(end);
        Some(Event::Level {
            track,
            at: end,
            level,
        })
    }
}

#[cfg(test)]
mod tests;
