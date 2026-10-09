//! A pass over audio already published: what the final pass runs after a
//! recording stops.
//!
//! [`transcribe`] reads each track's published segments in order, from
//! where an earlier run got to, and feeds them to an engine under the
//! [`EngineSupervisor`], so every request has its timeout and a hung or
//! crashed engine is restarted and resumes from the last confirmed sample,
//! as on the live path. What the engine confirms goes to a [`PassSink`]
//! with the text it gave for it, one call per confirmation, so whatever
//! the sink has committed is a point the pass can carry on from:
//! - text comes only once the engine has confirmed it (the supervisor
//!   holds it until then), so a sample's text reaches the sink once;
//! - audio the engine couldn't transcribe (it kept failing on it) reaches
//!   the sink as skipped, so the pass still moves past it;
//! - each track's runs of samples (consecutive segments in one epoch) are
//!   flushed at their end, so a chunk never spans a gap or a reopened
//!   stream.
//!
//! Audio is sent in frames that end on whole multiples of
//! [`PassConfig::frame`] (or a segment's end), and never more than
//! [`PassConfig::in_flight`] ahead of what's confirmed, so a long session
//! isn't read into memory at once and the engine never falls behind. A
//! pass resumed from a confirmed point sends the same frames from there as
//! one that never stopped.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use nota_core::messages::{AudioChunk, Transcript};
use nota_core::{Clock, SampleCount, SampleIndex, SampleRange, SampleRate, TrackId};
use nota_store::SegmentRow;

use super::{EngineCommand, EngineConfig, EngineEvent, EngineStatus, EngineSupervisor};
use crate::segment::ReadSegmentError;

/// One track's published audio, and where the pass starts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackAudio {
    /// The track.
    pub track: TrackId,
    /// Its published segments, in sample order.
    pub segments: Vec<SegmentRow>,
    /// Every sample before this is done already.
    pub from: SampleIndex,
}

/// Where a pass's results go.
pub trait PassSink {
    /// Why the sink failed.
    type Error;

    /// The engine has dealt with every sample of `track` before `up_to`:
    /// `texts` is what it heard since the last call for the track, and
    /// `skipped` the audio it couldn't transcribe. Commit them together:
    /// the pass may stop after any call, and starts again from what was
    /// committed.
    ///
    /// # Errors
    ///
    /// If they couldn't be committed; the pass stops.
    fn confirmed(
        &mut self,
        track: TrackId,
        up_to: SampleIndex,
        texts: Vec<Transcript>,
        skipped: Vec<SampleRange>,
    ) -> Result<(), Self::Error>;
}

/// How a pass runs.
#[derive(Debug, Clone)]
pub struct PassConfig {
    /// The engine, and how it's supervised.
    pub engine: EngineConfig,
    /// Frames end on multiples of this.
    pub frame: SampleCount,
    /// The most audio sent ahead of what's confirmed.
    pub in_flight: SampleCount,
    /// How long the pass waits with nothing confirmed (the engine down,
    /// or failing to start) before it gives up.
    pub stall: Duration,
}

impl PassConfig {
    /// The final pass with the engine `command` (which cuts at 25 s):
    /// frames of 1 s, up to 60 s ahead, and a pass given up after 10
    /// minutes without progress. Each request may take 60 s; a track is
    /// flushed only at the end of each of its runs, never for want of
    /// audio; 25 s (one chunk) is skipped when engines keep failing on it;
    /// and a stop waits at most 2 s for the engine's last answer, which
    /// the pass, stopping, wouldn't use.
    #[must_use]
    pub fn final_pass(command: EngineCommand) -> Self {
        let mut engine = EngineConfig::new(command);
        engine.request_timeout = Duration::from_secs(60);
        engine.idle_flush = Duration::from_hours(24);
        engine.poison_skip = SampleCount::new(25 * 16_000);
        engine.shutdown_wait = Duration::from_secs(2);
        Self {
            engine,
            frame: SampleCount::new(16_000),
            in_flight: SampleCount::new(60 * 16_000),
            stall: Duration::from_mins(10),
        }
    }
}

/// How a pass ended.
#[derive(Debug)]
pub enum PassEnd<E> {
    /// Every track was confirmed to its last published sample.
    Done,
    /// `stop` asked it to stop; it carries on from what the sink has.
    Stopped,
    /// Nothing was confirmed for [`PassConfig::stall`]; why the engine was
    /// down, if it said.
    Stalled(Option<String>),
    /// A segment's audio couldn't be read.
    Segment(SegmentRow, ReadSegmentError),
    /// The sink failed.
    Sink(E),
    /// The supervisor couldn't be started.
    Engine(io::Error),
}

impl<E: fmt::Display> fmt::Display for PassEnd<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Done => write!(f, "done"),
            Self::Stopped => write!(f, "stopped"),
            Self::Stalled(Some(why)) => write!(f, "the transcriber stopped working: {why}"),
            Self::Stalled(None) => write!(f, "the transcriber stopped answering"),
            Self::Segment(row, e) => write!(
                f,
                "the segment of track {} from sample {} {e}",
                row.track().get(),
                row.range().start().get()
            ),
            Self::Sink(e) => write!(f, "{e}"),
            Self::Engine(e) => write!(f, "the transcriber couldn't be started: {e}"),
        }
    }
}

/// How often a pass that's waiting for the engine checks whether it's
/// asked to stop.
const POLL: Duration = Duration::from_millis(100);

/// Runs a pass over `tracks`, one after the other, reading each segment's
/// audio with `read` (16 kHz samples, one per sample of its row), until
/// every track is confirmed to its last published sample, `stop` returns
/// true, or it fails. Committed results go to `sink` as they come.
pub fn transcribe<K: PassSink>(
    tracks: &[TrackAudio],
    mut read: impl FnMut(&SegmentRow) -> Result<Vec<i16>, ReadSegmentError>,
    config: &PassConfig,
    clock: &Arc<dyn Clock>,
    sink: &mut K,
    stop: &dyn Fn() -> bool,
) -> PassEnd<K::Error> {
    let mut work: Vec<Feed> = tracks.iter().filter_map(Feed::new).collect();
    if work.is_empty() {
        return PassEnd::Done;
    }
    let (mut engine, events) =
        match EngineSupervisor::start(config.engine.clone(), Arc::clone(clock)) {
            Ok(started) => started,
            Err(e) => return PassEnd::Engine(e),
        };
    let mut last_progress = clock.now();
    let mut offline: Option<String> = None;
    let mut texts: Vec<Transcript> = Vec::new();
    work.reverse();
    while let Some(feed) = work.last_mut() {
        if stop() {
            return PassEnd::Stopped;
        }
        if let Err((row, e)) = feed.send(&mut engine, &mut read, config) {
            return PassEnd::Segment(row, e);
        }
        if feed.done() {
            work.pop();
            continue;
        }
        let event = match events.recv_timeout(POLL) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => {
                let waited = clock
                    .now()
                    .checked_duration_since(last_progress)
                    .unwrap_or_default();
                if waited >= config.stall {
                    return PassEnd::Stalled(offline);
                }
                continue;
            }
            // The supervisor's thread is gone (it panicked): nothing more
            // will be confirmed.
            Err(RecvTimeoutError::Disconnected) => return PassEnd::Stalled(offline),
        };
        let (track, up_to, skipped) = match event {
            EngineEvent::Transcript(text) => {
                texts.push(text);
                continue;
            }
            EngineEvent::Status(EngineStatus::Offline(why)) => {
                offline = Some(why.to_string());
                continue;
            }
            EngineEvent::Status(EngineStatus::Online { .. }) => continue,
            EngineEvent::Confirmed { track, up_to } => (track, up_to, Vec::new()),
            // The skipped audio is the oldest unconfirmed, so everything
            // before its end is now dealt with.
            EngineEvent::Skipped { track, range } => (track, range.end(), vec![range]),
        };
        let (mine, others): (Vec<Transcript>, Vec<Transcript>) = std::mem::take(&mut texts)
            .into_iter()
            .partition(|t| t.track() == track && t.range().end() <= up_to);
        texts = others;
        if let Err(e) = sink.confirmed(track, up_to, mine, skipped) {
            return PassEnd::Sink(e);
        }
        last_progress = clock.now();
        if feed.track == track {
            feed.confirmed(up_to);
        }
    }
    engine.shutdown();
    PassEnd::Done
}

/// One track's audio on its way to the engine.
#[derive(Debug)]
struct Feed {
    track: TrackId,
    /// The segments still to send, the one being sent first.
    segments: VecDeque<SegmentRow>,
    /// The audio of the segment being sent, once read.
    audio: Option<Vec<i16>>,
    /// The next sample to send.
    next: SampleIndex,
    /// The segment last sent from, to tell where a run ends.
    previous: Option<SegmentRow>,
    /// Sent and not yet confirmed, in order.
    sent: VecDeque<SampleRange>,
    /// Everything before this is confirmed.
    confirmed: SampleIndex,
    /// The last published sample's end.
    end: SampleIndex,
}

impl Feed {
    /// The audio of `track` left to do, if any.
    fn new(track: &TrackAudio) -> Option<Self> {
        let segments: VecDeque<SegmentRow> = track
            .segments
            .iter()
            .filter(|row| row.range().end() > track.from)
            .copied()
            .collect();
        let end = segments.back()?.range().end();
        Some(Self {
            track: track.track,
            next: track.from.max(segments.front()?.range().start()),
            segments,
            audio: None,
            previous: None,
            sent: VecDeque::new(),
            confirmed: track.from,
            end,
        })
    }

    /// Whether every published sample is confirmed.
    fn done(&self) -> bool {
        self.segments.is_empty() && self.confirmed >= self.end
    }

    fn in_flight(&self) -> SampleCount {
        self.sent
            .iter()
            .fold(SampleCount::ZERO, |n, range| n.saturating_add(range.len()))
    }

    fn confirmed(&mut self, up_to: SampleIndex) {
        self.confirmed = self.confirmed.max(up_to);
        while let Some(front) = self.sent.front_mut() {
            if front.end() <= up_to {
                self.sent.pop_front();
            } else {
                if let Some(rest) = SampleRange::new(front.start().max(up_to), front.end()) {
                    *front = rest;
                }
                break;
            }
        }
    }

    /// Sends frames until [`PassConfig::in_flight`] is reached or the
    /// track is all sent, flushing at the end of each run.
    fn send(
        &mut self,
        engine: &mut EngineSupervisor,
        read: &mut impl FnMut(&SegmentRow) -> Result<Vec<i16>, ReadSegmentError>,
        config: &PassConfig,
    ) -> Result<(), (SegmentRow, ReadSegmentError)> {
        while self.in_flight() < config.in_flight {
            let Some(&row) = self.segments.front() else {
                return Ok(());
            };
            // A new run (after a gap, or in a new epoch): the one before it
            // ends here, once, before any of this segment is sent.
            let new_run = self.previous.is_some_and(|previous| {
                previous.range().end() != row.range().start() || previous.epoch() != row.epoch()
            });
            if new_run && self.audio.is_none() {
                engine.flush(self.track);
            }
            let audio = match &self.audio {
                Some(audio) => audio,
                None => self.audio.insert(read(&row).map_err(|e| (row, e))?),
            };
            let start = self.next.max(row.range().start());
            let frame = config.frame.get().max(1);
            let boundary = (start.get() / frame)
                .saturating_add(1)
                .saturating_mul(frame);
            let end = SampleIndex::new(boundary).min(row.range().end());
            let at = |sample: SampleIndex| {
                usize::try_from(sample.saturating_count_since(row.range().start()).get())
                    .unwrap_or(usize::MAX)
                    .min(audio.len())
            };
            let samples = audio[at(start)..at(end)].to_vec();
            if let Some(chunk) = AudioChunk::new(self.track, start, SampleRate::SPEECH, samples)
                .filter(|chunk| !chunk.range().is_empty())
            {
                let range = chunk.range();
                // Refused only if it overlaps what was sent (it can't) or
                // the supervisor has stopped, which the events will show.
                let _ = engine.send_audio(chunk);
                self.sent.push_back(range);
            }
            self.next = end;
            if end >= row.range().end() {
                self.segments.pop_front();
                self.audio = None;
                self.previous = Some(row);
                if self.segments.is_empty() {
                    engine.flush(self.track);
                }
            }
        }
        Ok(())
    }
}
