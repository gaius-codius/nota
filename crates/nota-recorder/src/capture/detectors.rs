//! The detectors [`record_tracks`](super::record_tracks) runs for each
//! track: [`Levels`] on the samples it records (digital zeros, quiet) and
//! [`Stall`] on the count of samples its stream has delivered.
//!
//! This module only wires the pure detectors to the recorder's events. It
//! gives what they report the track and the session time it belongs to:
//! a [`Change`] is at a sample, so it's timed by the track's timeline; a
//! stall is at the session time the delivered count last moved.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nota_core::recorder::WarningState;
use nota_core::{Clock, SampleCount, SampleIndex, SampleRate, SessionTime, TrackId, TrackTimeline};

use super::{CaptureEvent, CaptureReceiver, RecorderEvent};
use crate::detect::{Change, Condition, Levels, Stall, Stalled, Thresholds};

/// Where a detector's events go: the recorder's `report`, with the track.
type Report<'a> = &'a mut dyn FnMut(Option<TrackId>, RecorderEvent);

/// The detectors of every track [`record_tracks`](super::record_tracks)
/// records.
pub(super) struct Detectors {
    /// The session clock, for a stall's checks and for a change the
    /// timeline can't place.
    clock: Arc<dyn Clock>,
    /// Each track's detectors.
    tracks: BTreeMap<TrackId, TrackDetectors>,
}

/// One track's detectors.
struct TrackDetectors {
    /// What the track's levels are held to.
    thresholds: Thresholds,
    /// Made with the track's first audio, once the rate is known to be the
    /// writer's.
    levels: Option<Levels>,
    /// The stalled detector.
    stall: Stall,
    /// Whether the stream has started. A stream still opening (cpal can
    /// take 4 s) delivers nothing, and isn't stalled.
    armed: bool,
}

impl Detectors {
    /// The detectors for each track `events` was prepared for, with the
    /// thresholds it holds for them.
    pub(super) fn new(events: &CaptureReceiver) -> Self {
        let tracks = events
            .tracks()
            .into_iter()
            .map(|track| {
                let thresholds = events
                    .thresholds
                    .get(&track)
                    .copied()
                    .unwrap_or(Thresholds::MICROPHONE);
                let detectors = TrackDetectors {
                    thresholds,
                    levels: None,
                    stall: Stall::new(thresholds.stalled),
                    armed: false,
                };
                (track, detectors)
            })
            .collect();
        Self {
            clock: Arc::clone(&events.clock),
            tracks,
        }
    }

    /// Detectors that watch no track, for a test that drives
    /// [`handle`](super::handle) by hand.
    #[cfg(test)]
    pub(super) fn unwatched() -> Self {
        Self {
            clock: Arc::new(nota_core::FakeClock::new(SessionTime::ZERO)),
            tracks: BTreeMap::new(),
        }
    }

    /// Notes `event`, just received from `track`'s queue, before anything
    /// is done with it: a [`CaptureEvent::Started`] starts the track's
    /// stall watch.
    pub(super) fn note(&mut self, track: TrackId, event: &CaptureEvent) {
        if let (CaptureEvent::Started, Some(detectors)) = (event, self.tracks.get_mut(&track)) {
            detectors.armed = true;
        }
    }

    /// Feeds `track`'s detectors the `samples` just recorded from sample
    /// `first`, at `rate`, and reports what they raise or clear.
    pub(super) fn audio(
        &mut self,
        track: TrackId,
        rate: SampleRate,
        first: SampleIndex,
        samples: &[i16],
        timeline: &TrackTimeline,
        report: Report<'_>,
    ) {
        // Samples past the last sample number weren't recorded (the writer
        // refused them), and the detector can't number them.
        let len = SampleCount::new(samples.len() as u64);
        let Some(detectors) = self
            .tracks
            .get_mut(&track)
            .filter(|_| first.checked_add(len).is_some())
        else {
            return;
        };
        let thresholds = detectors.thresholds;
        let levels = detectors
            .levels
            .get_or_insert_with(|| Levels::new(rate, &thresholds));
        let changes = levels.push(first, samples);
        self.report_changes(track, changes, timeline, report);
    }

    /// Checks every armed track in `live` for a stall: its delivered count
    /// is read from the stream's thread, so a recorder that's behind on its
    /// queue doesn't stall a track whose audio is waiting there.
    pub(super) fn check_stalls(
        &mut self,
        events: &CaptureReceiver,
        live: &BTreeSet<TrackId>,
        report: Report<'_>,
    ) {
        let now = self.clock.now();
        let asleep = self.clock.suspended();
        for (&track, detectors) in &mut self.tracks {
            if !detectors.armed || !live.contains(&track) {
                continue;
            }
            let Some(progress) = events.progress(track) else {
                continue;
            };
            let stalled = detectors.stall.check(progress.now().delivered, now, asleep);
            report_stalled(track, stalled, report);
        }
    }

    /// Ends `track`'s detectors, its stream having stopped, failed or never
    /// started: what they raised is cleared, `next` being the sample after
    /// the last it recorded.
    pub(super) fn end(
        &mut self,
        track: TrackId,
        next: Option<SampleIndex>,
        timeline: Option<&TrackTimeline>,
        report: Report<'_>,
    ) {
        let now = self.clock.now();
        let Some(detectors) = self.tracks.get_mut(&track) else {
            return;
        };
        detectors.armed = false;
        let stalled = detectors.stall.end(now);
        let changes = match (detectors.levels.as_mut(), next) {
            (Some(levels), Some(next)) => levels.end(next),
            _ => Vec::new(),
        };
        report_stalled(track, stalled, report);
        // Without a timeline no audio was recorded, so there are no changes.
        if let Some(timeline) = timeline {
            self.report_changes(track, changes, timeline, report);
        }
    }

    /// Reports `changes` of `track`'s levels, each at the session time its
    /// sample plays at, or now if the timeline can't say.
    fn report_changes(
        &self,
        track: TrackId,
        changes: Vec<Change>,
        timeline: &TrackTimeline,
        report: Report<'_>,
    ) {
        for change in changes {
            let at = timeline
                .time_of(change.at)
                .unwrap_or_else(|| self.clock.now());
            detected(track, change.condition, change.state, at, report);
        }
    }
}

/// Reports a stall's raise or clear, if there is one.
fn report_stalled(track: TrackId, stalled: Option<Stalled>, report: Report<'_>) {
    if let Some(Stalled { state, at }) = stalled {
        detected(track, Condition::Stalled, state, at, report);
    }
}

/// Reports that `condition` was raised or cleared on `track`.
fn detected(
    track: TrackId,
    condition: Condition,
    state: WarningState,
    at: SessionTime,
    report: Report<'_>,
) {
    report(
        Some(track),
        RecorderEvent::Detected {
            condition,
            state,
            at,
        },
    );
}
