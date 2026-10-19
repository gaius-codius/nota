//! Timing a stream's buffers from `PipeWire`'s first settled stamp.
//!
//! cpal stamps each buffer with when `PipeWire` captured it: the graph
//! cycle's time less the stream's delay. A stream's first buffers carry no
//! delay, as `PipeWire` hasn't measured one yet, so their stamps are their
//! cycles' times, when their audio ended; the first buffer with a delay
//! may have one shorter than itself, as the graph's quantum changes under
//! it. Only once a buffer's delay is at least its length (within
//! [`SETTLED_WITHIN`]) does its stamp say when its audio began, and a
//! real capture's stamps hold steady from there: they put the stream's
//! first sample a quantum earlier than its first buffer, taken as ending
//! at its cycle, would.
//!
//! So [`Settling`] holds a stream's first buffers back until one is
//! settled, and dates them back from it: the buffers held run on, one
//! after another, to the settled buffer's stamp. A track's first epoch
//! opens at the first of them. Each buffer keeps its cycle's time: it's
//! sent with a delay that much longer, so the drift meter, which reads
//! the cycles, sees no difference.
//!
//! A stream started with the session can have its first audio dated
//! before the session clock's start: on a private `PipeWire`'s null sink,
//! the settled stamp put a monitor's first sample 22 ms before it
//! (GAI-491). Session time has nothing before its start, so that audio
//! is placed at the start, late by as much: the held buffers run on from
//! it, and a buffer dated by its own stamp ([`stamped`]) starts there.
//! Each still keeps its cycle.
//!
//! A run of buffers with no delay after the stream had one is held and
//! dated the same way, should `PipeWire` measure the delay again (it
//! might after the stream follows a new device; that hasn't been seen),
//! so a reopening's first audio would be dated as a stream's first is.
//! The sender sends what's held before a reopening, as it came before
//! the stretch the stream missed, and holds the reopening's first buffers
//! as a stream's first are ([`Settling::reopen`]).
//!
//! A stream that settles on no delay within [`HOLD_LIMIT`] or
//! [`HOLD_BUFFERS`], or ends, or sends a buffer cpal couldn't stamp, has
//! what it held sent as each buffer's own stamp places it ([`stamped`]),
//! and its buffers pass from then on, until it has a delay and loses it
//! again.

use std::time::Duration;

use nota_core::{SampleCount, SampleRate};

/// The most audio held back waiting for a settled stamp. A real capture
/// settles on its seventh buffer, after about 150 ms.
const HOLD_LIMIT: Duration = Duration::from_millis(500);

/// How far short of its buffer's length a delay can be and still be
/// settled: the graph's quantum, resampled to the stream's rate, comes as
/// a whole number of samples (683 for a quantum of 42.667 ms at 16 kHz,
/// 42.688 ms), so a delay of one quantum can fall short of its buffer by
/// part of a sample.
const SETTLED_WITHIN: Duration = Duration::from_millis(1);

/// The most buffers held back: past it, as past [`HOLD_LIMIT`], what's
/// held is sent. Room for them is made with the sender, off the stream's
/// real-time thread; a quantum of 256 at 48 kHz fills it in about 340 ms.
const HOLD_BUFFERS: usize = 64;

/// When a stream's buffer was delivered and stamped, as `PipeWire`
/// through cpal times it: on `CLOCK_MONOTONIC` as the stream gives it
/// ([`CaptureSender::audio_delivered`](super::CaptureSender::audio_delivered)),
/// and as the session clock's elapsed time once the sender has placed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    /// The time of the graph cycle that delivered it.
    pub cycle: Duration,
    /// When it was captured: the cycle's time less the stream's delay.
    pub capture: Duration,
}

impl Delivery {
    /// How long before its cycle the buffer was stamped.
    fn delay(self) -> Duration {
        self.cycle.saturating_sub(self.capture)
    }
}

/// When a buffer's first sample was captured, and how long before its
/// cycle that was; `None` for a buffer sent unstamped.
pub(super) type Timed = Option<(Duration, Duration)>;

/// One stream's buffers, held back until a stamp settles; see the module
/// docs.
#[derive(Debug)]
pub(super) struct Settling {
    rate: SampleRate,
    /// The most samples held: [`HOLD_LIMIT`] at the stream's rate.
    limit: usize,
    /// The held buffers' samples, one after another.
    samples: Vec<i16>,
    /// Each held buffer's length and stamp, in order.
    held: Vec<(usize, Delivery)>,
    /// Whether buffers are being held.
    holding: bool,
    /// Whether the stream has had a delay since it last settled or gave
    /// up holding: a delay of none after it starts holding again.
    measured: bool,
}

impl Settling {
    /// A stream at `rate` about to deliver its first buffer, which is
    /// held. Its room for the most it holds is made here, off the stream's
    /// real-time thread, so holding never allocates.
    pub(super) fn new(rate: SampleRate) -> Self {
        let limit = SampleCount::started_within(HOLD_LIMIT, rate)
            .and_then(|count| usize::try_from(count.get()).ok())
            .unwrap_or(0);
        Self {
            rate,
            limit,
            samples: Vec::with_capacity(limit),
            held: Vec::with_capacity(HOLD_BUFFERS),
            holding: true,
            measured: false,
        }
    }

    /// Takes the next buffer, `samples`, stamped `stamp` if cpal could,
    /// and `send`s what's due: nothing while it's held, or the buffers
    /// held, dated, and then it.
    pub(super) fn buffer(
        &mut self,
        samples: &[i16],
        stamp: Option<Delivery>,
        send: &mut dyn FnMut(&[i16], Timed),
    ) {
        let Some(stamp) = stamp else {
            self.release(send);
            send(samples, None);
            return;
        };
        let delay = stamp.delay();
        if !self.holding {
            if !(delay.is_zero() && self.measured) {
                self.measured |= !delay.is_zero();
                send(samples, stamped(stamp, samples.len(), self.rate));
                return;
            }
            // `PipeWire` is measuring the delay again.
            self.holding = true;
        }
        if !delay.is_zero()
            && length(samples.len(), self.rate)
                .is_some_and(|length| delay + SETTLED_WITHIN >= length)
        {
            self.date_from(stamp, send);
            self.measured = true;
            send(samples, Some((stamp.capture, delay)));
        } else if self.samples.len() + samples.len() > self.limit || self.held.len() == HOLD_BUFFERS
        {
            self.release(send);
            send(samples, stamped(stamp, samples.len(), self.rate));
        } else {
            self.samples.extend_from_slice(samples);
            self.held.push((samples.len(), stamp));
        }
    }

    /// Sends what's held, each buffer as its own stamp places it, and
    /// stops holding: the stream is ending, or gave no settled stamp in
    /// time, or one cpal couldn't stamp.
    pub(super) fn release(&mut self, send: &mut dyn FnMut(&[i16], Timed)) {
        let mut from = 0;
        for &(len, stamp) in &self.held {
            let to = from + len;
            send(&self.samples[from..to], stamped(stamp, len, self.rate));
            from = to;
        }
        self.clear();
        self.measured = false;
    }

    /// Holds the buffers that follow, as a stream's first are: the stream
    /// resumes after a stretch it missed, on a new device or after a
    /// suspend.
    pub(super) fn reopen(&mut self) {
        self.holding = true;
    }

    /// Sends what's held, dated back from `settled`, the stamp of the
    /// buffer that follows it, and stops holding. If they can't be dated,
    /// as [`Self::release`].
    fn date_from(&mut self, settled: Delivery, send: &mut dyn FnMut(&[i16], Timed)) {
        let Some(start) = self.start(settled) else {
            self.release(send);
            return;
        };
        let mut from = 0;
        for &(len, stamp) in &self.held {
            let to = from + len;
            let timed = length(from, self.rate)
                .and_then(|offset| start.checked_add(offset))
                .map_or_else(
                    || stamped(stamp, len, self.rate),
                    |at| Some((at, stamp.cycle.saturating_sub(at))),
                );
            send(&self.samples[from..to], timed);
            from = to;
        }
        self.clear();
    }

    /// When the first held sample was captured: the samples held before
    /// `settled`'s buffer, back from its stamp, or earlier if that would
    /// put a held buffer's audio after the cycle that delivered it; but no
    /// earlier than the session clock's start (see the module docs).
    /// `None` only if a length overflows.
    fn start(&self, settled: Delivery) -> Option<Duration> {
        let back = settled
            .capture
            .saturating_sub(length(self.samples.len(), self.rate)?);
        let mut end = 0;
        self.held.iter().try_fold(back, |start, &(len, stamp)| {
            end += len;
            Some(start.min(stamp.cycle.saturating_sub(length(end, self.rate)?)))
        })
    }

    /// Stops holding, with nothing held.
    fn clear(&mut self) {
        self.samples.clear();
        self.held.clear();
        self.holding = false;
    }
}

/// How long `len` samples last at `rate`.
fn length(len: usize, rate: SampleRate) -> Option<Duration> {
    SampleCount::new(u64::try_from(len).ok()?).duration_at(rate)
}

/// When the first of `len` samples stamped `stamp` was captured, and how
/// long before its cycle that was, by that stamp alone: as cpal stamps
/// it, unless its delay is none. A capture stream's audio comes before
/// the cycle that delivers it, so a delay of none is one `PipeWire` hasn't
/// measured; the buffer is then taken as ending at its cycle, captured its
/// length before, or at the session clock's start if that's later (see
/// the module docs). `None` only if the length overflows.
fn stamped(stamp: Delivery, len: usize, rate: SampleRate) -> Timed {
    let delay = stamp.delay();
    if !delay.is_zero() {
        return Some((stamp.capture, delay));
    }
    let began = stamp.cycle.saturating_sub(length(len, rate)?);
    Some((began, stamp.cycle.saturating_sub(began)))
}

#[cfg(test)]
mod tests;
