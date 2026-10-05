//! The channel between the capture streams and the recorder thread, and the
//! count of what waits in it.
//!
//! A stream's callback runs on the audio server's real-time thread, so in
//! the steady state sending must not allocate. [`Queue`] keeps its events
//! in a `VecDeque` with room reserved up front, and lends the callbacks the
//! sample buffers the recorder has finished with, so the callback only
//! copies samples into a buffer it already has. Only when the disk stalls
//! for longer than [`RESERVED_EVENTS`] buffers, or more buffers are out
//! than [`SPARE_BUFFERS`], does sending allocate. The queue then grows
//! rather than dropping audio, and keeps the room it grew.
//!
//! Each sender takes the lock for a copy and a push. The recorder holds it
//! only to pop an event or hand a buffer back, never across a write or an
//! fsync. (`std::sync::mpsc` takes a lock to wake a waiting receiver too.)
//!
//! [`Progress`] counts each track's samples sent but not yet appended to
//! its journal, so loss can be measured from the audio the server
//! delivered, not only from what the recorder wrote.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nota_core::{SampleCount, SampleIndex, TrackId};

use super::CaptureEvent;

/// Events the queue has room for before it grows: about 20 s of audio at
/// `PipeWire`'s usual quantum, far longer than an fsync.
pub(super) const RESERVED_EVENTS: usize = 1_024;

/// Buffers the queue keeps for the callbacks to fill again. A stream needs
/// two or three in the steady state; the rest, after a stall, are freed.
pub(super) const SPARE_BUFFERS: usize = 64;

/// What the senders and the recorder share.
#[derive(Debug)]
struct State {
    events: VecDeque<(TrackId, CaptureEvent)>,
    /// Buffers the recorder is done with.
    spare: Vec<Vec<i16>>,
    /// Senders still able to send. At none, an empty queue is closed.
    senders: usize,
    /// Buffers sending had to allocate, because none was spare.
    allocated: u64,
}

/// The channel itself: events in order, from every stream started into it.
#[derive(Debug)]
pub(super) struct Queue {
    state: Mutex<State>,
    /// Signalled whenever an event arrives or a sender leaves.
    changed: Condvar,
}

/// What [`Queue::next`] found.
#[derive(Debug)]
pub(super) enum Received {
    /// The next event, from this track.
    Event(TrackId, CaptureEvent),
    /// Nothing came in time.
    Idle,
    /// The queue is empty and every sender has left.
    Closed,
}

impl Queue {
    /// An empty queue and its first sender.
    pub(super) fn new() -> (Arc<Self>, QueueSender) {
        let queue = Arc::new(Self {
            state: Mutex::new(State {
                events: VecDeque::with_capacity(RESERVED_EVENTS),
                spare: Vec::with_capacity(SPARE_BUFFERS),
                senders: 1,
                allocated: 0,
            }),
            changed: Condvar::new(),
        });
        let sender = QueueSender(Arc::clone(&queue));
        (queue, sender)
    }

    /// The state, even if a thread panicked holding it: every change to it
    /// is a single push or pop, so it's never left half done.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next event, waiting at most `timeout` for one. Events sent before
    /// the last sender left still come first.
    pub(super) fn next(&self, timeout: Duration) -> Received {
        let state = self.lock();
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |s| s.events.is_empty() && s.senders > 0)
            .unwrap_or_else(PoisonError::into_inner);
        match state.events.pop_front() {
            Some((track, event)) => Received::Event(track, event),
            None if state.senders == 0 => Received::Closed,
            None => Received::Idle,
        }
    }

    /// Takes back a buffer the recorder is done with, for a callback to
    /// fill again. Beyond [`SPARE_BUFFERS`] it's freed, outside the lock.
    pub(super) fn recycle(&self, buffer: Vec<i16>) {
        let mut state = self.lock();
        if state.spare.len() < SPARE_BUFFERS {
            state.spare.push(buffer);
        } else {
            drop(state);
            drop(buffer);
        }
    }

    /// How many buffers sending has had to allocate so far.
    #[cfg(test)]
    pub(super) fn allocated(&self) -> u64 {
        self.lock().allocated
    }
}

/// A sender on a [`Queue`]. The queue closes once every one has been
/// dropped and its events taken.
#[derive(Debug)]
pub(super) struct QueueSender(Arc<Queue>);

impl Clone for QueueSender {
    fn clone(&self) -> Self {
        self.0.lock().senders += 1;
        Self(Arc::clone(&self.0))
    }
}

impl Drop for QueueSender {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.senders = state.senders.saturating_sub(1);
        drop(state);
        self.0.changed.notify_all();
    }
}

impl QueueSender {
    /// Queues `event` from `track`.
    pub(super) fn send(&self, track: TrackId, event: CaptureEvent) {
        self.0.lock().events.push_back((track, event));
        self.0.changed.notify_one();
    }

    /// Queues a copy of `samples` from `track`, in a spare buffer if there
    /// is one.
    pub(super) fn audio(&self, track: TrackId, samples: &[i16]) {
        let mut state = self.0.lock();
        let mut buffer = if let Some(buffer) = state.spare.pop() {
            buffer
        } else {
            state.allocated += 1;
            Vec::with_capacity(samples.len())
        };
        buffer.clear();
        buffer.extend_from_slice(samples);
        state.events.push_back((track, CaptureEvent::Audio(buffer)));
        drop(state);
        self.0.changed.notify_one();
    }
}

/// How far one track's audio has got, readable from any thread while it
/// records: from [`CaptureReceiver::progress`](super::CaptureReceiver::progress).
#[derive(Debug, Clone)]
pub struct Progress(Arc<Counts>);

#[derive(Debug)]
struct Counts {
    /// Samples sent by the stream and not yet appended to a journal.
    queued: AtomicU64,
    /// The writer's next sample, once the recorder has appended up to it.
    captured: AtomicU64,
    /// The highest durable position seen on the track's journals.
    durable: AtomicU64,
}

/// Where a track's audio was, at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Positions {
    /// The end of the audio the server delivered: what the recorder
    /// captured, and what waits in the queue for it.
    pub delivered: SampleIndex,
    /// The end of what the recorder has appended to the track's journals.
    pub captured: SampleIndex,
    /// The end of what an fsync has made durable.
    pub durable: SampleIndex,
}

impl Positions {
    /// Delivered, and waiting for the recorder.
    #[must_use]
    pub const fn queued(&self) -> SampleCount {
        SampleCount::new(self.delivered.get().saturating_sub(self.captured.get()))
    }

    /// What a crash now would lose: delivered, and not yet durable.
    #[must_use]
    pub const fn at_risk(&self) -> SampleCount {
        SampleCount::new(self.delivered.get().saturating_sub(self.durable.get()))
    }
}

impl Progress {
    /// A track's progress, with the recorder at `captured` and `durable`
    /// and nothing queued.
    pub(super) fn new(captured: SampleIndex, durable: SampleIndex) -> Self {
        Self(Arc::new(Counts {
            queued: AtomicU64::new(0),
            captured: AtomicU64::new(captured.get()),
            durable: AtomicU64::new(durable.get()),
        }))
    }

    /// The track's positions now. Read while the recorder runs, `delivered`
    /// may for a moment count a buffer the recorder has just appended
    /// twice, but never misses one, and `durable` is never past
    /// `captured`.
    #[must_use]
    pub fn now(&self) -> Positions {
        // Durable, queued, then captured: the recorder moves captured on
        // before taking the samples off the queue count, and durable never
        // passes captured, so reading in this order can't undercount.
        let durable = self.0.durable.load(Ordering::SeqCst);
        let queued = self.0.queued.load(Ordering::SeqCst);
        let captured = self.0.captured.load(Ordering::SeqCst);
        Positions {
            delivered: SampleIndex::new(captured.saturating_add(queued)),
            captured: SampleIndex::new(captured),
            durable: SampleIndex::new(durable),
        }
    }

    /// The stream is sending `n` samples.
    pub(super) fn sent(&self, n: usize) {
        self.0.queued.fetch_add(n as u64, Ordering::SeqCst);
    }

    /// The recorder appended `n` samples, and the track's next sample is
    /// now `captured`.
    pub(super) fn appended(&self, n: usize, captured: SampleIndex) {
        self.0.captured.store(captured.get(), Ordering::SeqCst);
        let _ = self
            .0
            .queued
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |q| {
                Some(q.saturating_sub(n as u64))
            });
    }

    /// The track's journals are durable up to `durable`.
    pub(super) fn synced(&self, durable: SampleIndex) {
        self.0.durable.fetch_max(durable.get(), Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIC: TrackId = TrackId::new(0);

    fn audio(received: Received) -> Vec<i16> {
        match received {
            Received::Event(MIC, CaptureEvent::Audio(samples)) => samples,
            other => panic!("expected audio, got {other:?}"),
        }
    }

    #[test]
    fn events_come_in_order_and_then_the_queue_closes() {
        let (queue, sender) = Queue::new();
        let second = sender.clone();
        assert!(matches!(queue.next(Duration::ZERO), Received::Idle));
        sender.audio(MIC, &[1, 2]);
        second.send(MIC, CaptureEvent::Stopped);
        drop(sender);
        assert!(matches!(queue.next(Duration::ZERO), Received::Event(..)));
        drop(second);
        // What was sent before the last sender left still comes.
        assert!(matches!(
            queue.next(Duration::ZERO),
            Received::Event(MIC, CaptureEvent::Stopped)
        ));
        assert!(matches!(queue.next(Duration::ZERO), Received::Closed));
    }

    #[test]
    fn a_waiting_recorder_wakes_when_audio_arrives() {
        let (queue, sender) = Queue::new();
        let feeder = std::thread::spawn(move || sender.audio(MIC, &[7]));
        assert_eq!(audio(queue.next(Duration::from_secs(10))), [7]);
        feeder.join().unwrap();
        assert!(matches!(
            queue.next(Duration::from_secs(10)),
            Received::Closed
        ));
    }

    #[test]
    fn recycled_buffers_are_filled_again_without_allocating() {
        let (queue, sender) = Queue::new();
        sender.audio(MIC, &[1, 2, 3]);
        let first = audio(queue.next(Duration::ZERO));
        let at = first.as_ptr();
        queue.recycle(first);
        for round in 0..100_i16 {
            sender.audio(MIC, &[round, round]);
            let buffer = audio(queue.next(Duration::ZERO));
            assert_eq!(buffer, [round, round]);
            assert_eq!(buffer.as_ptr(), at, "round {round}");
            queue.recycle(buffer);
        }
        assert_eq!(queue.allocated(), 1);
    }

    #[test]
    fn only_so_many_spare_buffers_are_kept() {
        let (queue, sender) = Queue::new();
        let outstanding = SPARE_BUFFERS + 10;
        for _ in 0..outstanding {
            sender.audio(MIC, &[0]);
        }
        let buffers: Vec<_> = (0..outstanding)
            .map(|_| audio(queue.next(Duration::ZERO)))
            .collect();
        for buffer in buffers {
            queue.recycle(buffer);
        }
        assert_eq!(queue.lock().spare.len(), SPARE_BUFFERS);
        for _ in 0..outstanding {
            sender.audio(MIC, &[0]);
        }
        assert_eq!(queue.allocated(), u64::try_from(outstanding + 10).unwrap());
    }

    #[test]
    fn positions_count_the_queue_between_the_stream_and_the_journal() {
        let progress = Progress::new(SampleIndex::new(100), SampleIndex::new(40));
        let at = |delivered, captured, durable| Positions {
            delivered: SampleIndex::new(delivered),
            captured: SampleIndex::new(captured),
            durable: SampleIndex::new(durable),
        };
        assert_eq!(progress.now(), at(100, 100, 40));
        progress.sent(30);
        progress.sent(20);
        assert_eq!(progress.now(), at(150, 100, 40));
        assert_eq!(progress.now().queued(), SampleCount::new(50));
        assert_eq!(progress.now().at_risk(), SampleCount::new(110));
        progress.appended(30, SampleIndex::new(130));
        assert_eq!(progress.now(), at(150, 130, 40));
        progress.synced(SampleIndex::new(130));
        // An older durable position never moves it back.
        progress.synced(SampleIndex::new(90));
        assert_eq!(progress.now(), at(150, 130, 130));
        assert_eq!(progress.now().at_risk(), SampleCount::new(20));
        // More taken off than was counted can't wrap.
        progress.appended(1_000, SampleIndex::new(150));
        assert_eq!(progress.now(), at(150, 150, 130));
    }
}
