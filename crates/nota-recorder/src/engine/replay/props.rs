//! Property tests for [`Replay`], which checks the engine's replies, the
//! untrusted half of the protocol. Random runs of pushes, flushes, sends,
//! engine restarts, skips and trims, with arbitrary replies mixed in among
//! a well-behaved engine's, against a model of what the engine was sent.
//!
//! Checked after every step:
//! - nothing panics;
//! - a reply is accepted exactly when it fits what the engine was sent
//!   (text from where its last text ended, or from the start of the stream
//!   it's working on, and not past that stream; a confirmation forward,
//!   within the stream, and not before the end of its text), and a reply
//!   refused leaves the replay as it was;
//! - a confirmation releases exactly the text accepted since the last one,
//!   so every sample's text is released at most once, and in order;
//! - every sample pushed is sent to each engine at most once, in order and
//!   unchanged, and to the next engine after a restart unless confirmed or
//!   dropped; a sample confirmed or dropped is never sent again;
//! - the count of unconfirmed samples is what's still held.
//!
//! A failure proptest finds is saved in `proptest-regressions/` and replayed
//! on every run; commit that file with the fix.

use std::collections::VecDeque;

use nota_core::messages::{AudioChunk, ToEngine, Transcript};
use nota_core::protocol::MAX_AUDIO_SAMPLES;
use nota_core::{SampleCount, SampleIndex, SampleRange, SampleRate, SessionTime, TrackId};
use proptest::prelude::*;

use super::Replay;

const TRACK: TrackId = TrackId::new(3);

/// The sample every index carries, so audio split or resent can be checked
/// against what was pushed.
fn sample(index: u64) -> i16 {
    i16::try_from(index % 1_000).unwrap()
}

fn chunk(start: u64, len: u64) -> AudioChunk {
    let samples = (start..start + len).map(sample).collect();
    AudioChunk::new(TRACK, SampleIndex::new(start), SampleRate::SPEECH, samples).unwrap()
}

fn text(start: u64, end: u64) -> Transcript {
    let range = SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap();
    Transcript::new(TRACK, range, format!("{start}-{end}")).unwrap()
}

/// Where an arbitrary reply is aimed: near a point that matters, so the
/// boundaries are tried, or anywhere.
#[derive(Debug, Clone, Copy)]
enum Anchor {
    /// The first sample of the stream the engine is working on.
    StreamStart,
    /// The end of that stream.
    StreamEnd,
    /// Where the engine's unconfirmed text ends.
    TextEnd,
    /// An absolute sample.
    At(u64),
}

#[derive(Debug, Clone)]
enum Step {
    /// Audio after a gap (usually none).
    Push {
        gap: u64,
        len: u64,
    },
    Flush,
    /// Sends what's unsent to the engine.
    Send,
    /// The engine dies.
    Restart,
    /// The engine dies and the oldest audio is skipped, as the supervisor
    /// does to audio that keeps killing it.
    Skip(u64),
    /// The engine dies and the audio is trimmed, as while none runs.
    Trim(u64),
    /// Text from near `anchor`, `len` long or ending near `end`.
    Text {
        anchor: Anchor,
        delta: i64,
        len: u64,
        end: Option<(Anchor, i64)>,
    },
    /// A confirmation near `anchor`.
    Confirm {
        anchor: Anchor,
        delta: i64,
    },
    /// A well-behaved engine's answer: text for part of its stream (from
    /// where its text ends), then the confirmation.
    Answer {
        cut: u64,
    },
}

fn anchor() -> impl Strategy<Value = Anchor> {
    prop_oneof![
        Just(Anchor::StreamStart),
        Just(Anchor::StreamEnd),
        Just(Anchor::TextEnd),
        (0_u64..400).prop_map(Anchor::At),
    ]
}

/// An offset from an anchor: mostly on it or one either side, where the
/// boundaries are.
fn delta() -> impl Strategy<Value = i64> {
    prop_oneof![Just(0_i64), Just(-1), Just(1), -3_i64..=3]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (prop_oneof![4 => Just(0_u64), 1 => 1_u64..20], 1_u64..40)
            .prop_map(|(gap, len)| Step::Push { gap, len }),
        1 => Just(Step::Flush),
        3 => Just(Step::Send),
        1 => Just(Step::Restart),
        1 => (0_u64..60).prop_map(Step::Skip),
        1 => (0_u64..80).prop_map(Step::Trim),
        2 => (
            anchor(),
            delta(),
            1_u64..30,
            prop::option::of((anchor(), delta())),
        )
            .prop_map(|(anchor, delta, len, end)| Step::Text { anchor, delta, len, end }),
        2 => (anchor(), delta()).prop_map(|(anchor, delta)| Step::Confirm { anchor, delta }),
        3 => any::<u64>().prop_map(|cut| Step::Answer { cut }),
    ]
}

/// Where each sample is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    NotPushed,
    /// Pushed, not yet sent to the engine running now.
    Pushed,
    /// Sent to the engine running now, not confirmed.
    Sent,
    Confirmed,
    Dropped,
}

/// One frame the engine has been sent and not yet answered.
#[derive(Debug, Clone, Copy)]
enum Frame {
    Audio(u64, u64),
    Flush,
}

/// The test's own account of the run.
#[derive(Debug, Default)]
struct Model {
    held: Vec<Held>,
    /// The sample after the last one pushed.
    next: u64,
    /// What the engine running now has and hasn't answered.
    engine: VecDeque<Frame>,
    /// Where the next audio sent to it must start, unless a flush comes
    /// first: the engine takes each stream's audio only without gaps.
    follows: Option<u64>,
    /// Text accepted and not yet released.
    text: Vec<Transcript>,
    /// Where the last text released ended.
    released_to: u64,
}

impl Model {
    fn at(&self, index: u64) -> Held {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.held.get(i).copied())
            .unwrap_or(Held::NotPushed)
    }

    fn set(&mut self, index: u64, to: Held) {
        let i = usize::try_from(index).unwrap();
        if self.held.len() <= i {
            self.held.resize(i + 1, Held::NotPushed);
        }
        self.held[i] = to;
    }

    /// The stream the engine is working on: after any flushes it has been
    /// sent, the audio that follows on without a gap.
    fn stream(&self) -> Option<(u64, u64)> {
        let mut frames = self
            .engine
            .iter()
            .skip_while(|f| matches!(f, Frame::Flush))
            .peekable();
        let Some(&&Frame::Audio(start, _)) = frames.peek() else {
            return None;
        };
        let mut end = start;
        for frame in frames {
            match *frame {
                Frame::Audio(from, to) if from == end => end = to,
                _ => break,
            }
        }
        Some((start, end))
    }

    fn text_end(&self) -> Option<u64> {
        self.text.last().map(|t| t.range().end().get())
    }

    /// Whether the engine may send text for `[start, end)`.
    fn text_fits(&self, start: u64, end: u64) -> bool {
        self.stream()
            .is_some_and(|(from, to)| start == self.text_end().unwrap_or(from) && end <= to)
    }

    /// Whether the engine may confirm up to `up_to`.
    fn confirm_fits(&self, up_to: u64) -> bool {
        self.stream().is_some_and(|(from, to)| {
            up_to > from && up_to <= to && self.text_end().is_none_or(|end| end <= up_to)
        })
    }

    fn resolve(&self, anchor: Anchor, delta: i64) -> u64 {
        let base = match anchor {
            Anchor::StreamStart => self.stream().map_or(0, |s| s.0),
            Anchor::StreamEnd => self.stream().map_or(self.next, |s| s.1),
            Anchor::TextEnd => self
                .text_end()
                .or_else(|| self.stream().map(|s| s.0))
                .unwrap_or(0),
            Anchor::At(at) => at,
        };
        base.saturating_add_signed(delta)
    }

    /// The engine has died: nothing is sent to the next one yet.
    fn restart(&mut self) {
        self.engine.clear();
        self.follows = None;
        self.text.clear();
        for held in &mut self.held {
            if *held == Held::Sent {
                *held = Held::Pushed;
            }
        }
    }

    fn unconfirmed(&self) -> u64 {
        let held = self
            .held
            .iter()
            .filter(|h| matches!(h, Held::Pushed | Held::Sent))
            .count();
        u64::try_from(held).unwrap()
    }
}

/// The replay as it stands, to show a refused reply changed nothing.
fn snapshot(replay: &Replay) -> String {
    format!("{replay:?}")
}

fn drop_ranges(model: &mut Model, dropped: &[SampleRange]) -> Result<(), TestCaseError> {
    let mut last = 0;
    for range in dropped {
        prop_assert!(range.start().get() >= last, "dropped out of order");
        for i in range.start().get()..range.end().get() {
            prop_assert_eq!(model.at(i), Held::Pushed, "dropped {} twice or unheld", i);
            model.set(i, Held::Dropped);
        }
        last = range.end().get();
    }
    Ok(())
}

fn send(replay: &mut Replay, model: &mut Model) -> Result<(), TestCaseError> {
    let mut last = None;
    for message in replay.take_unsent(SessionTime::ZERO) {
        match message {
            ToEngine::Audio(chunk) => {
                let range = chunk.range();
                prop_assert_eq!(chunk.track(), TRACK);
                prop_assert!(chunk.samples().len() <= MAX_AUDIO_SAMPLES);
                prop_assert!(
                    last.is_none_or(|end| range.start().get() >= end),
                    "audio sent out of order"
                );
                prop_assert!(
                    model.follows.is_none_or(|next| next == range.start().get()),
                    "a gap in a stream, with no flush before it"
                );
                model.follows = Some(range.end().get());
                for (i, &s) in (range.start().get()..range.end().get()).zip(chunk.samples()) {
                    prop_assert_eq!(model.at(i), Held::Pushed, "sample {} sent again", i);
                    prop_assert_eq!(s, sample(i), "sample {} changed", i);
                    model.set(i, Held::Sent);
                }
                last = Some(range.end().get());
                model
                    .engine
                    .push_back(Frame::Audio(range.start().get(), range.end().get()));
            }
            ToEngine::Flush { track } => {
                prop_assert_eq!(track, TRACK);
                model.follows = None;
                model.engine.push_back(Frame::Flush);
            }
        }
    }
    // Everything still held has now been sent to this engine.
    prop_assert!(!model.held.contains(&Held::Pushed), "audio left unsent");
    Ok(())
}

fn on_text(
    replay: &mut Replay,
    model: &mut Model,
    start: u64,
    end: u64,
) -> Result<(), TestCaseError> {
    let fits = model.text_fits(start, end);
    let before = snapshot(replay);
    match replay.on_transcript(text(start, end)) {
        Ok(()) => {
            prop_assert!(
                fits,
                "text {}..{} accepted; {:?}",
                start,
                end,
                model.stream()
            );
            model.text.push(text(start, end));
        }
        Err(violation) => {
            prop_assert!(
                !fits,
                "text {}..{} refused ({:?}); {:?}",
                start,
                end,
                violation,
                model.stream()
            );
            prop_assert_eq!(snapshot(replay), before, "a refused reply changed it");
        }
    }
    Ok(())
}

fn on_confirmed(replay: &mut Replay, model: &mut Model, up_to: u64) -> Result<(), TestCaseError> {
    let fits = model.confirm_fits(up_to);
    let before = snapshot(replay);
    match replay.on_confirmed(SampleIndex::new(up_to)) {
        Ok(released) => {
            prop_assert!(fits, "confirmed {} accepted; {:?}", up_to, model.stream());
            // Exactly the text accepted since the last release, in order:
            // none of it was released before, or will be again.
            prop_assert_eq!(&released, &model.text);
            for t in &released {
                prop_assert!(t.range().start().get() >= model.released_to, "text again");
                model.released_to = t.range().end().get();
            }
            model.text.clear();
            for i in 0..up_to {
                match model.at(i) {
                    Held::Sent => model.set(i, Held::Confirmed),
                    Held::Pushed => prop_assert!(false, "confirmed {} before it was sent", i),
                    Held::NotPushed | Held::Confirmed | Held::Dropped => {}
                }
            }
            while let Some(&frame) = model.engine.front() {
                match frame {
                    Frame::Audio(_, end) if end <= up_to => {}
                    Frame::Audio(start, end) if start < up_to => {
                        model.engine[0] = Frame::Audio(up_to, end);
                        break;
                    }
                    Frame::Audio(..) => break,
                    Frame::Flush => {}
                }
                model.engine.pop_front();
            }
        }
        Err(violation) => {
            prop_assert!(
                !fits,
                "confirmed {} refused ({:?}); {:?}",
                up_to,
                violation,
                model.stream()
            );
            prop_assert_eq!(snapshot(replay), before, "a refused reply changed it");
        }
    }
    Ok(())
}

fn run(steps: &[Step]) -> Result<(), TestCaseError> {
    let mut replay = Replay::new(TRACK);
    let mut model = Model::default();
    for step in steps {
        match *step {
            Step::Push { gap, len } => {
                let start = model.next + gap;
                replay.push_audio(&chunk(start, len), SessionTime::ZERO);
                for i in start..start + len {
                    model.set(i, Held::Pushed);
                }
                model.next = start + len;
            }
            Step::Flush => replay.push_flush(),
            Step::Send => send(&mut replay, &mut model)?,
            Step::Restart => {
                replay.reset_sent();
                model.restart();
            }
            Step::Skip(count) => {
                replay.reset_sent();
                model.restart();
                let dropped = replay.skip(SampleCount::new(count));
                let total: u64 = dropped.iter().map(|r| r.len().get()).sum();
                prop_assert_eq!(total, count.min(model.unconfirmed()));
                drop_ranges(&mut model, &dropped)?;
            }
            Step::Trim(keep) => {
                replay.reset_sent();
                model.restart();
                let before = model.unconfirmed();
                let dropped = replay.trim(SampleCount::new(keep));
                drop_ranges(&mut model, &dropped)?;
                prop_assert_eq!(model.unconfirmed(), keep.min(before));
            }
            Step::Text {
                anchor,
                delta,
                len,
                end,
            } => {
                let start = model.resolve(anchor, delta);
                let end = end.map_or(start + len, |(at, delta)| model.resolve(at, delta));
                if end > start {
                    on_text(&mut replay, &mut model, start, end)?;
                }
            }
            Step::Confirm { anchor, delta } => {
                let up_to = model.resolve(anchor, delta);
                on_confirmed(&mut replay, &mut model, up_to)?;
            }
            Step::Answer { cut } => {
                if let Some((from, to)) = model.stream() {
                    let start = model.text_end().unwrap_or(from);
                    // Somewhere after the text, up to the stream's end.
                    let span = to - start;
                    let up_to = if span == 0 {
                        to
                    } else {
                        start + 1 + cut % span
                    };
                    if up_to > start {
                        on_text(&mut replay, &mut model, start, up_to)?;
                    }
                    on_confirmed(&mut replay, &mut model, up_to)?;
                }
            }
        }
        prop_assert_eq!(replay.unconfirmed().get(), model.unconfirmed());
    }
    Ok(())
}

proptest! {
    #[test]
    fn replies_are_checked_against_what_was_sent(
        steps in prop::collection::vec(step(), 0..80)
    ) {
        run(&steps)?;
    }
}
