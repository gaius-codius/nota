//! The three detectors that notice a track's capture going wrong. No audio
//! server says when a device goes away or a sink stops playing, so nota
//! watches each track's audio itself:
//! - **Stalled** ([`Stall`]): no samples arriving, as when a device is
//!   removed or a stream stops.
//! - **Digital zeros** ([`Levels`]): nothing but exact zeros, as when
//!   nothing is playing, a sink is suspended, or (later, on macOS) the
//!   permission to record was refused. A working microphone always hears
//!   some noise, so its samples are never all exactly zero for long.
//! - **Quiet** ([`Levels`]): nothing above the noise floor learned from
//!   the track: a pause, which is only worth a warning after about 30 s.
//!
//! Each detector raises its [`Condition`] once it has held for the time
//! its [`Thresholds`] state, and clears it when it ends. A raise is
//! reported when the threshold is reached, at the moment the condition
//! began: the first exact zero, the first quiet frame, the last samples
//! seen arriving. A clear is reported at the moment it ended. Nothing is
//! cleared that wasn't raised, so a pause shorter than its threshold
//! reports nothing.
//!
//! The detectors only count: they read no clock and do no I/O. The
//! recorder feeds [`Levels`] each track's samples as it records them, and
//! [`Stall`] the track's delivered count with the session time, and puts
//! what they report on the timeline.

use std::collections::VecDeque;
use std::time::Duration;

use nota_core::recorder::WarningState;
use nota_core::{SampleCount, SampleIndex, SampleRate, SessionTime};

/// What a detector noticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// No samples have arrived for a while.
    Stalled,
    /// Every sample for a while was exactly zero.
    DigitalZeros,
    /// Nothing for a while rose above the track's noise floor.
    Quiet,
}

/// How long each condition must hold before it's raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// How long with no samples arriving makes a track stalled.
    pub stalled: Duration,
    /// How long of nothing but exact zeros raises digital zeros.
    pub zeros: Duration,
    /// How long with nothing above the noise floor raises quiet.
    pub quiet: Duration,
}

/// How long with no samples arriving makes a track stalled: 2 s, the time
/// the Recording screen's "mic not responding" expects (UI spec,
/// "Warnings"). A running stream delivers a buffer every few tens of
/// milliseconds, and a route change's quiet moment lasts well under a
/// second, so 2 s is a stream that has stopped.
pub const STALLED_AFTER: Duration = Duration::from_secs(2);

/// How long of exact zeros on a microphone raises digital zeros: 5 s, as
/// the screen's "mic muted" expects. A live microphone's noise is never
/// exactly zero for that long; a muted or refused one is.
pub const MIC_ZEROS_AFTER: Duration = Duration::from_secs(5);

/// How long of exact zeros on the system audio raises digital zeros: 30 s,
/// as the screen's "nothing playing" expects. A sink that plays nothing
/// gives exact zeros, which between videos or slides is normal for a
/// while.
pub const SYSTEM_ZEROS_AFTER: Duration = Duration::from_secs(30);

/// How long with nothing above the noise floor raises quiet: 30 s
/// (research synthesis, "Disk, sleep and device loss"). A lecture's
/// pauses are shorter; a stretch that long is worth a look.
pub const QUIET_AFTER: Duration = Duration::from_secs(30);

impl Thresholds {
    /// A microphone's thresholds.
    pub const MICROPHONE: Self = Self {
        stalled: STALLED_AFTER,
        zeros: MIC_ZEROS_AFTER,
        quiet: QUIET_AFTER,
    };

    /// The system audio's thresholds.
    pub const SYSTEM_AUDIO: Self = Self {
        stalled: STALLED_AFTER,
        zeros: SYSTEM_ZEROS_AFTER,
        quiet: QUIET_AFTER,
    };
}

/// A condition [`Levels`] raised or cleared, at a sample of the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// What it's about.
    pub condition: Condition,
    /// Whether it's raised or cleared.
    pub state: WarningState,
    /// Where it began, if raised, or ended, if cleared: the first sample
    /// of the run, or the first after it.
    pub at: SampleIndex,
}

/// How long a frame lasts, in which the quiet detector measures the level:
/// 50 ms, short enough to find the gaps between words, which set the noise
/// floor, and long enough for a level that one sample's noise doesn't
/// swing.
const FRAME: Duration = Duration::from_millis(50);

/// How many frames make one block of the noise floor's history: 20, a
/// second's worth.
const FRAMES_PER_BLOCK: usize = 20;

/// How many blocks the noise floor remembers: 10, so the floor is the
/// quietest frame of about the last 10 s. Speech has gaps between words
/// far more often than that, so the floor stays the room's noise while
/// someone speaks, and follows a room that gets noisier within 10 s.
const FLOOR_BLOCKS: usize = 10;

/// How far above the noise floor a frame must be to count as sound, as a
/// ratio of mean squares: 10, that is 10 dB. The floor is the quietest
/// frame, so the room's own noise sits a few decibels above it; speech
/// sits 15 dB or more above, even from the back of a room.
const QUIET_MARGIN: u64 = 10;

/// The mean square below which a frame is always quiet, whatever the
/// floor: 270, about -66 dBFS, below what a microphone's own noise
/// reaches. So a floor learned from a nearly silent track doesn't make
/// its faintest hiss count as sound.
const ALWAYS_QUIET: u64 = 270;

/// The mean square above which a frame is never quiet, whatever the
/// floor: about 1 070 000, -30 dBFS. A steady loud sound, music say,
/// would otherwise teach the floor its own level and read as quiet.
const NEVER_QUIET: u64 = 1_070_000;

/// The digital-zeros and quiet detectors for one track, fed its samples in
/// order.
#[derive(Debug)]
pub struct Levels {
    /// How many samples of exact zeros raise digital zeros.
    zeros_after: SampleCount,
    /// How many samples of quiet frames raise quiet.
    quiet_after: SampleCount,
    /// How many samples a frame holds.
    frame_len: u64,
    /// The run of exact zeros the latest sample is in.
    zeros: Run,
    /// The run of quiet frames the latest frame is in.
    quiet: Run,
    /// The frame being measured.
    frame: Frame,
    /// The quietest frames lately.
    floor: NoiseFloor,
}

/// A run of samples a condition holds over.
#[derive(Debug, Default)]
struct Run {
    /// Where it began, while it holds.
    since: Option<SampleIndex>,
    /// Whether it has been raised.
    raised: bool,
}

impl Run {
    /// Notes that the condition holds from `at`, up to `end`; a raise at
    /// the run's start if that makes it last `after`.
    fn holds(
        &mut self,
        condition: Condition,
        at: SampleIndex,
        end: SampleIndex,
        after: SampleCount,
    ) -> Option<Change> {
        let since = *self.since.get_or_insert(at);
        if self.raised || end.saturating_count_since(since) < after {
            return None;
        }
        self.raised = true;
        Some(Change {
            condition,
            state: WarningState::Raised,
            at: since,
        })
    }

    /// Notes that the condition ended at `at`; a clear if it was raised.
    fn ends(&mut self, condition: Condition, at: SampleIndex) -> Option<Change> {
        self.since = None;
        std::mem::take(&mut self.raised).then_some(Change {
            condition,
            state: WarningState::Cleared,
            at,
        })
    }
}

/// The frame being measured: its samples' sum of squares.
#[derive(Debug, Default)]
struct Frame {
    /// Its first sample.
    start: Option<SampleIndex>,
    /// How many samples it has.
    len: u64,
    /// The sum of their squares. At 16 kHz a frame's 800 squares, each
    /// under 2^30, sum far below `u64::MAX`; it saturates rather than
    /// wrap at any rate.
    sum_squares: u64,
}

impl Levels {
    /// The detectors for a track captured at `rate`, raised after
    /// `thresholds`.
    #[must_use]
    pub fn new(rate: SampleRate, thresholds: &Thresholds) -> Self {
        let count = |after: Duration| {
            SampleCount::started_within(after, rate).unwrap_or(SampleCount::new(u64::MAX))
        };
        Self {
            zeros_after: count(thresholds.zeros),
            quiet_after: count(thresholds.quiet),
            frame_len: count(FRAME).get().max(1),
            zeros: Run::default(),
            quiet: Run::default(),
            frame: Frame::default(),
            floor: NoiseFloor::default(),
        }
    }

    /// Feeds the track's next `samples`, the first of them numbered
    /// `first`, and returns what that raised or cleared, in order.
    pub fn push(&mut self, first: SampleIndex, samples: &[i16]) -> Vec<Change> {
        let mut changes = Vec::new();
        for (sample, &value) in (first.get()..).map(SampleIndex::new).zip(samples) {
            changes.extend(self.zero_sample(sample, value));
            changes.extend(self.frame_sample(sample, value));
        }
        changes
    }

    /// Stops watching: the track's stream has ended, and its next sample
    /// would have been `next`. Whatever was raised is cleared there, since
    /// an ended stream is neither silent nor quiet. A part frame is
    /// dropped.
    pub fn end(&mut self, next: SampleIndex) -> Vec<Change> {
        self.frame = Frame::default();
        [
            self.zeros.ends(Condition::DigitalZeros, next),
            self.quiet.ends(Condition::Quiet, next),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Runs the digital-zeros detector over one sample.
    fn zero_sample(&mut self, sample: SampleIndex, value: i16) -> Option<Change> {
        if value == 0 {
            let end = sample.saturating_add(SampleCount::new(1));
            self.zeros
                .holds(Condition::DigitalZeros, sample, end, self.zeros_after)
        } else {
            self.zeros.ends(Condition::DigitalZeros, sample)
        }
    }

    /// Adds one sample to the frame being measured, and runs the quiet
    /// detector over the frame once it's whole.
    fn frame_sample(&mut self, sample: SampleIndex, value: i16) -> Option<Change> {
        let start = *self.frame.start.get_or_insert(sample);
        let magnitude = u64::from(value.unsigned_abs());
        self.frame.len += 1;
        self.frame.sum_squares = self.frame.sum_squares.saturating_add(magnitude * magnitude);
        if self.frame.len < self.frame_len {
            return None;
        }
        let frame = std::mem::take(&mut self.frame);
        let end = sample.saturating_add(SampleCount::new(1));
        if self.is_quiet(&frame) {
            self.quiet
                .holds(Condition::Quiet, start, end, self.quiet_after)
        } else {
            self.quiet.ends(Condition::Quiet, start)
        }
    }

    /// Whether `frame` has nothing above the noise floor, which learns
    /// from it first. A frame of exact zeros is quiet, and teaches the
    /// floor nothing: zeros are no measure of the room.
    fn is_quiet(&mut self, frame: &Frame) -> bool {
        if frame.sum_squares == 0 {
            return true;
        }
        let mean_square = frame.sum_squares / frame.len.max(1);
        self.floor.learn(mean_square);
        let floor = self.floor.level().unwrap_or(mean_square);
        mean_square <= ALWAYS_QUIET
            || (mean_square <= NEVER_QUIET && mean_square <= floor.saturating_mul(QUIET_MARGIN))
    }
}

/// The noise floor: the quietest frame of about the last
/// [`FLOOR_BLOCKS`] seconds, kept as the quietest of each second.
#[derive(Debug, Default)]
struct NoiseFloor {
    /// The quietest mean square of each whole block, oldest first.
    blocks: VecDeque<u64>,
    /// The quietest mean square of the block being filled.
    current: Option<u64>,
    /// How many frames the block being filled has.
    frames: usize,
}

impl NoiseFloor {
    /// Takes in one frame's mean square.
    fn learn(&mut self, mean_square: u64) {
        self.current = Some(self.current.map_or(mean_square, |q| q.min(mean_square)));
        self.frames += 1;
        if self.frames < FRAMES_PER_BLOCK {
            return;
        }
        if let Some(quietest) = self.current.take() {
            if self.blocks.len() == FLOOR_BLOCKS {
                self.blocks.pop_front();
            }
            self.blocks.push_back(quietest);
        }
        self.frames = 0;
    }

    /// The floor: the quietest frame remembered. `None` before any.
    fn level(&self) -> Option<u64> {
        self.blocks.iter().copied().chain(self.current).min()
    }
}

/// The stalled detector for one track: whether its delivered count has
/// moved lately.
#[derive(Debug)]
pub struct Stall {
    /// How long without movement makes it stalled.
    after: Duration,
    /// When the count was last seen to move, if it's watched yet.
    moved: Option<Moved>,
    /// Whether stalled is raised.
    raised: bool,
}

/// The last time a track's delivered count was seen to move.
#[derive(Debug, Clone, Copy)]
struct Moved {
    /// The count then.
    delivered: SampleIndex,
    /// The session time then.
    at: SessionTime,
    /// The clock's count of time spent suspended then.
    asleep: Duration,
}

/// A raise or clear of [`Condition::Stalled`], at a session time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stalled {
    /// Whether it's raised or cleared.
    pub state: WarningState,
    /// If raised, when the samples were last seen arriving; if cleared,
    /// when they were seen arriving again.
    pub at: SessionTime,
}

impl Stall {
    /// A detector that raises stalled after `after` without samples.
    #[must_use]
    pub const fn new(after: Duration) -> Self {
        Self {
            after,
            moved: None,
            raised: false,
        }
    }

    /// Checks the track's `delivered` count at session time `now`, with
    /// the clock's count of time spent suspended at `asleep`. The first
    /// check starts the watch. Time spent suspended doesn't count: every
    /// stream is quiet while the machine sleeps, and the sleep has a
    /// warning of its own. A count that moved clears a raised stall;
    /// one that hasn't since `after` raises it.
    pub fn check(
        &mut self,
        delivered: SampleIndex,
        now: SessionTime,
        asleep: Duration,
    ) -> Option<Stalled> {
        let seen = Moved {
            delivered,
            at: now,
            asleep,
        };
        let Some(moved) = self.moved else {
            self.moved = Some(seen);
            return None;
        };
        if delivered != moved.delivered {
            self.moved = Some(seen);
            return std::mem::take(&mut self.raised).then_some(Stalled {
                state: WarningState::Cleared,
                at: now,
            });
        }
        let waited = now.checked_duration_since(moved.at).unwrap_or_default();
        let slept = asleep.saturating_sub(moved.asleep);
        if self.raised || waited.saturating_sub(slept) < self.after {
            return None;
        }
        self.raised = true;
        Some(Stalled {
            state: WarningState::Raised,
            at: moved.at,
        })
    }

    /// Stops watching: the track's stream has ended. A raised stall is
    /// cleared at `now`, since an ended stream isn't stalled.
    pub fn end(&mut self, now: SessionTime) -> Option<Stalled> {
        self.moved = None;
        std::mem::take(&mut self.raised).then_some(Stalled {
            state: WarningState::Cleared,
            at: now,
        })
    }
}

#[cfg(test)]
mod tests;
