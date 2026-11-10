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
//! - audio the engine couldn't transcribe (it kept failing on it), and a
//!   segment that couldn't be read back, reach the sink as skipped, so the
//!   pass still moves past them and the rest is done;
//! - each track's runs of samples (consecutive segments in one epoch) are
//!   flushed at their end, so a chunk never spans a gap or a reopened
//!   stream;
//! - why the engine is down reaches the sink as the supervisor reports it,
//!   and its coming back up, so whoever shows the pass can say why it
//!   stands still before it gives up;
//! - an engine that can't be started, or that exits before saying hello
//!   [`EXITS_BEFORE_HELLO`] times in a row, ends the pass at once: a retry
//!   within the pass won't help, and the pass would hold the job runner
//!   for its whole stall. One that hangs, or dies after saying hello, is
//!   restarted until the stall.
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

use super::{
    EngineCommand, EngineConfig, EngineEvent, EngineStatus, EngineSupervisor, OfflineReason,
};
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

    /// The engine is down for `why` (`Some`), in the words a stall now
    /// would end with, or back up after being down in this pass (`None`).
    /// Called each time the supervisor reports it down, and once when it
    /// comes back; never for an engine that hasn't been down in this pass.
    /// It's only shown: the pass carries on whatever the sink does with it.
    fn engine_down(&mut self, why: Option<&str>);
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
    /// How long the pass waits with nothing confirmed (the engine hung,
    /// or dying after it says hello) before it gives up. An engine that
    /// can't start ends the pass sooner ([`PassEnd::CantStart`]).
    pub stall: Duration,
}

impl PassConfig {
    /// The final pass with the engine `command` (which cuts at 25 s):
    /// frames of 1 s, up to 60 s ahead, and a pass given up after 10
    /// minutes without progress (sooner if the engine can't start). Each request may take 60 s; a track is
    /// flushed only at the end of each of its runs, never for want of
    /// audio; 25 s (one chunk) is skipped when engines keep failing on it;
    /// and the engine is killed at once when the pass ends: a pass that's
    /// done has nothing left to confirm, and one that's stopped (for a
    /// recording) wouldn't use what it confirmed.
    #[must_use]
    pub fn final_pass(command: EngineCommand) -> Self {
        let mut engine = EngineConfig::new(command);
        engine.request_timeout = Duration::from_secs(60);
        engine.idle_flush = Duration::from_hours(24);
        engine.poison_skip = SampleCount::new(25 * 16_000);
        engine.shutdown_wait = Duration::ZERO;
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
    /// down, if it said and hasn't come back up since.
    Stalled(Option<String>),
    /// The engine couldn't be spawned, or exited before saying hello
    /// [`EXITS_BEFORE_HELLO`] times in a row, so restarting it within the
    /// pass is no use; why it was down the last time. What the sink
    /// committed stays.
    CantStart(String),
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
            Self::Stalled(Some(why)) | Self::CantStart(why) => f.write_str(&stopped_working(why)),
            Self::Stalled(None) => write!(f, "the transcriber stopped answering"),
            Self::Sink(e) => write!(f, "{e}"),
            Self::Engine(e) => write!(f, "the transcriber couldn't be started: {e}"),
        }
    }
}

/// How a stall says why the engine was down, and how the sink hears it
/// while the pass runs, so the reason reads the same before and after.
fn stopped_working(why: &str) -> String {
    format!("the transcriber stopped working: {why}")
}

/// How often a pass that's waiting for the engine checks whether it's
/// asked to stop.
const POLL: Duration = Duration::from_millis(100);

/// How many engines in a row may exit before saying hello before the pass
/// ends: a model that doesn't load fails the same way each time, while one
/// odd exit may not. Three, as decided on #237.
pub const EXITS_BEFORE_HELLO: u32 = 3;

/// How the pass's engines have started, judged from the supervisor's
/// reports, so the pass can give up on an engine that can't start.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Starts {
    /// The engine running now has said hello.
    hello: bool,
    /// The engines in a row that exited before saying hello: since the
    /// last failure of any other kind.
    exits: u32,
}

impl Starts {
    /// Notes an engine that said hello: it started, so its failure,
    /// whatever it is, breaks the run of exits before hello.
    fn online(&mut self) {
        self.hello = true;
    }

    /// Notes the engine going down for `why`, and whether that ends the
    /// pass: it couldn't be spawned, or it's the last of
    /// [`EXITS_BEFORE_HELLO`] exits in a row before hello. Any other
    /// failure (a hang, a bad hello, an exit after hello) breaks the run
    /// of exits and is left to the stall.
    fn offline(&mut self, why: &OfflineReason) -> bool {
        let before_hello = !std::mem::replace(&mut self.hello, false);
        match why {
            OfflineReason::SpawnFailed(_) => true,
            OfflineReason::Exited { .. } if before_hello => {
                self.exits = self.exits.saturating_add(1);
                self.exits >= EXITS_BEFORE_HELLO
            }
            _ => {
                self.exits = 0;
                false
            }
        }
    }
}

/// Runs a pass over `tracks`, one after the other, reading each segment's
/// audio with `read` (16 kHz samples, one per sample of its row), until
/// every track is confirmed to its last published sample, `stop` returns
/// true, or it fails. Committed results go to `sink` as they come. A
/// segment `read` can't give is skipped: its samples go to the sink as
/// skipped once everything before them is confirmed.
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
    let mut starts = Starts::default();
    let mut texts: Vec<Transcript> = Vec::new();
    work.reverse();
    while let Some(feed) = work.last_mut() {
        if stop() {
            return PassEnd::Stopped;
        }
        feed.send(&mut engine, &mut read, config);
        while let Some(range) = feed.unread_due() {
            if let Err(e) = sink.confirmed(feed.track, range.end(), Vec::new(), vec![range]) {
                return PassEnd::Sink(e);
            }
            last_progress = clock.now();
            feed.confirmed(range.end());
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
            EngineEvent::Status(EngineStatus::Offline(reason)) => {
                let why = reason.to_string();
                sink.engine_down(Some(&stopped_working(&why)));
                // The sink has heard why first, as it does for every
                // failure, so it shows the reason the run then ends with.
                if starts.offline(&reason) {
                    return PassEnd::CantStart(why);
                }
                offline = Some(why);
                continue;
            }
            // Back up: the reason no longer applies, to the sink or to a
            // later stall. An engine that wasn't down in this pass says
            // nothing: whatever the sink kept from before is its own.
            EngineEvent::Status(EngineStatus::Online { .. }) => {
                starts.online();
                if offline.take().is_some() {
                    sink.engine_down(None);
                }
                continue;
            }
            EngineEvent::Confirmed { track, up_to } => (track, up_to, Vec::new()),
            // The skipped audio is the oldest unconfirmed, so everything
            // before its end is now dealt with.
            EngineEvent::Skipped { track, range } => (track, range.end(), vec![range]),
        };
        // The supervisor passes a track's text on only with the
        // confirmation that covers it, so all the track's text is this
        // confirmation's.
        let (mine, others): (Vec<Transcript>, Vec<Transcript>) = std::mem::take(&mut texts)
            .into_iter()
            .partition(|t| t.track() == track);
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
    /// Segments that couldn't be read, to be skipped once everything sent
    /// before them is confirmed, in order.
    unread: VecDeque<SampleRange>,
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
            unread: VecDeque::new(),
            confirmed: track.from,
            end,
        })
    }

    /// Whether every published sample is confirmed.
    fn done(&self) -> bool {
        self.segments.is_empty() && self.unread.is_empty() && self.confirmed >= self.end
    }

    /// The first unreadable segment's samples, taken, once nothing sent
    /// before them is still unconfirmed.
    fn unread_due(&mut self) -> Option<SampleRange> {
        let next = *self.unread.front()?;
        let clear = self
            .sent
            .front()
            .is_none_or(|sent| sent.start() >= next.end());
        clear.then(|| self.unread.pop_front()).flatten()
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
    ) {
        while self.in_flight() < config.in_flight {
            let Some(&row) = self.segments.front() else {
                return;
            };
            // A new run (after a gap, or in a new epoch): the one before it
            // ends here, once, before any of this segment is sent.
            let new_run = self.previous.is_some_and(|previous| {
                previous.range().end() != row.range().start() || previous.epoch() != row.epoch()
            });
            if new_run && self.audio.is_none() {
                engine.flush(self.track);
            }
            if self.audio.is_none() {
                let Ok(read) = read(&row) else {
                    self.skip_unread(engine, row);
                    continue;
                };
                self.audio = Some(read);
            }
            let Some(audio) = &self.audio else {
                continue;
            };
            let start = self.next.max(row.range().start());
            let frame = config.frame.get().max(1);
            let boundary = (start.get() / frame)
                .saturating_add(1)
                .saturating_mul(frame);
            // Never before `start`: a row overlapping the one before it
            // (the store refuses those) is passed over, not looped on.
            let end = SampleIndex::new(boundary).min(row.range().end()).max(start);
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
    }

    /// Passes over `row`, which couldn't be read: the run before it ends
    /// there, and its samples (from where the pass is) are skipped once
    /// everything before them is confirmed.
    fn skip_unread(&mut self, engine: &EngineSupervisor, row: SegmentRow) {
        engine.flush(self.track);
        if let Some(range) = SampleRange::new(self.next.max(row.range().start()), row.range().end())
            .filter(|range| !range.is_empty())
        {
            self.unread.push_back(range);
        }
        self.next = row.range().end();
        self.segments.pop_front();
        self.previous = Some(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited() -> OfflineReason {
        OfflineReason::Exited {
            status: None,
            stderr: "no model".into(),
        }
    }

    /// Whether each report, in order, ends the pass, an `Online` reading
    /// `None`.
    fn ends(reports: &[Option<OfflineReason>]) -> Vec<Option<bool>> {
        let mut starts = Starts::default();
        reports
            .iter()
            .map(|report| {
                if let Some(why) = report {
                    Some(starts.offline(why))
                } else {
                    starts.online();
                    None
                }
            })
            .collect()
    }

    /// An engine that can't be spawned ends the pass the first time.
    #[test]
    fn a_spawn_failure_ends_the_pass_at_once() {
        assert_eq!(
            ends(&[Some(OfflineReason::SpawnFailed("not found".into()))]),
            [Some(true)]
        );
    }

    /// The third engine in a row to exit before hello ends the pass; the
    /// first two don't.
    #[test]
    fn the_third_exit_before_hello_ends_the_pass() {
        assert_eq!(
            ends(&[Some(exited()), Some(exited()), Some(exited())]),
            [Some(false), Some(false), Some(true)]
        );
    }

    /// An engine that dies after each hello is never counted: it started,
    /// so the stall decides.
    #[test]
    fn exits_after_hello_never_end_the_pass() {
        let reports: Vec<Option<OfflineReason>> =
            (0..5).flat_map(|_| [None, Some(exited())]).collect();
        assert!(
            ends(&reports).iter().flatten().all(|ends| !ends),
            "{:?}",
            ends(&reports)
        );
    }

    /// A hello between exits starts the count again.
    #[test]
    fn a_hello_starts_the_count_again() {
        assert_eq!(
            ends(&[
                Some(exited()),
                Some(exited()),
                None,
                Some(exited()),
                Some(exited()),
                Some(exited()),
            ]),
            // The exit after the hello is the started engine's own; two
            // more before hello don't make three.
            [
                Some(false),
                Some(false),
                None,
                Some(false),
                Some(false),
                Some(false)
            ]
        );
    }

    /// Another failure before hello (no hello in time, a bad hello)
    /// breaks the run of exits, and isn't one itself.
    #[test]
    fn another_failure_breaks_the_run_of_exits() {
        for other in [
            OfflineReason::StartTimeout,
            OfflineReason::Protocol("message before hello".into()),
        ] {
            assert_eq!(
                ends(&[
                    Some(exited()),
                    Some(exited()),
                    Some(other.clone()),
                    Some(exited()),
                    Some(exited()),
                ]),
                [
                    Some(false),
                    Some(false),
                    Some(false),
                    Some(false),
                    Some(false)
                ],
                "{other:?}"
            );
        }
    }
}
