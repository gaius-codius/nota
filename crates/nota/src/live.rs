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
use nota_core::recorder::{Cause, Event, Level, Warning};
use nota_core::{Epoch, Gap, SessionTime, TrackId, TrackTimeline, Utterance, Word};
use nota_recorder::capture::{CaptureNotice, RecorderEvent};
use nota_recorder::detect::Condition;
use nota_recorder::engine::EngineEvent;

use crate::inhibit::{Slept, Unrecorded};

/// How often each track's level is sent: well within the screen's 250 ms.
const LEVEL_EVERY: Duration = Duration::from_millis(100);

/// The warning's cause for what a detector noticed.
const fn cause_of(condition: Condition) -> Cause {
    match condition {
        Condition::Stalled => Cause::Stalled,
        Condition::DigitalZeros => Cause::DigitalZeros,
        Condition::Quiet => Cause::Quiet,
    }
}

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
    /// Where each track's audio so far ends, so a gap that ended before
    /// it isn't taken for a sleep's.
    heard_to: BTreeMap<TrackId, SessionTime>,
    /// The sleeps seen so far, oldest first.
    slept: Vec<Slept>,
    /// The tracks that woke from the last sleep in `slept`.
    woke_from_last: BTreeSet<TrackId>,
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
            heard_to: BTreeMap::new(),
            slept: Vec::new(),
            woke_from_last: BTreeSet::new(),
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
                self.heard_to_end_of(&chunk);
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
                    // An epoch that follows straight on (drift corrected)
                    // breaks nothing the engine is hearing.
                    let straight_on = follower
                        .current()
                        .and_then(|current| current.time_of(epoch.first_sample()))
                        == Some(epoch.start());
                    if follower.follow(&epoch).is_err() || !straight_on {
                        actions.flush = Some(track);
                    }
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
            (
                Some(track),
                RecorderEvent::Detected {
                    condition,
                    state,
                    at,
                },
            ) => actions.updates.push(Event::Warning(Warning {
                cause: cause_of(condition),
                track: Some(track),
                at,
                state,
            })),
            (Some(track), RecorderEvent::Device { change, at }) => {
                actions.updates.push(Event::Device { track, change, at });
            }
            _ => {}
        }
        actions
    }

    /// What the screen is told when `chunk` is the first audio of a track
    /// after the machine slept: the warning, once for the sleep however
    /// many tracks woke from it, and for each gap the track's timeline kept
    /// since its audio ended, the epoch after it and the gap. The epochs
    /// are the capture's own, opened before this audio
    /// ([`RecorderEvent::Epoch`]); there are several if the audio server
    /// reported an overrun on the way back, or a stream stalled on after
    /// the resume. If there is no gap that began where the track's audio
    /// ended (the timeline took the stretch as drift, or refused a new
    /// epoch, and an earlier gap is all there is), only the warning is
    /// sent. The warning isn't cleared: the sleep is over, and the screen
    /// decides how long to show it.
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
        let heard_to = self.heard_to.get(&track).copied();
        let gaps = timeline
            .epoch_of(chunk.range().start())
            .map(|epoch| gaps_before(timeline, epoch, heard_to))
            .unwrap_or_default();
        let unrecorded = gaps
            .first()
            .zip(gaps.last())
            .map(|(first, last)| Unrecorded {
                from: first.from(),
                to: last.to(),
            });
        let seen = Slept {
            resumed,
            unrecorded,
        };
        let mut updates = Vec::new();
        let last = self
            .slept
            .last_mut()
            .filter(|last| !self.woke_from_last.contains(&track) && last.same_sleep_as(&seen));
        if let Some(last) = last {
            *last = last.merged_with(&seen);
            self.woke_from_last.insert(track);
        } else {
            updates.push(seen.warning());
            self.slept.push(seen);
            self.woke_from_last = BTreeSet::from([track]);
        }
        for gap in gaps {
            let after = timeline.epochs().iter().find(|e| e.start() == gap.to());
            updates.extend(after.map(|&epoch| Event::Epoch { track, epoch }));
            updates.push(Event::Gap { track, gap });
        }
        updates
    }

    /// Notes where `chunk` ends, as its track's audio so far.
    fn heard_to_end_of(&mut self, chunk: &AudioChunk) {
        let track = chunk.track();
        let end = self
            .followers
            .get(&track)
            .and_then(|timeline| timeline.span_of(chunk.range()));
        if let Some((_, end)) = end {
            self.heard_to.insert(track, end);
        }
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

/// The gaps in `timeline` between the end of the track's audio so far
/// (`heard_to`) and the start of `epoch`, the epoch its first audio after a
/// sleep is in, oldest first: more than one if an epoch with no audio was
/// opened on the way back. Empty if the last of them doesn't end where
/// `epoch` starts, so nothing was missed before it. Without `heard_to`
/// (nothing was heard yet) only the one that leads into `epoch` counts.
fn gaps_before(timeline: &TrackTimeline, epoch: &Epoch, heard_to: Option<SessionTime>) -> Vec<Gap> {
    let mut gaps: Vec<Gap> = timeline
        .gaps()
        .filter(|gap| gap.to() <= epoch.start())
        .filter(|gap| heard_to.is_none_or(|end| gap.from() >= end))
        .collect();
    if heard_to.is_none() {
        gaps.drain(..gaps.len().saturating_sub(1));
    }
    if gaps.last().is_none_or(|last| last.to() != epoch.start()) {
        gaps.clear();
    }
    gaps
}

#[cfg(test)]
mod tests;
