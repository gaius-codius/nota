//! The detectors run on the recorder thread: what they raise and clear
//! reaches the recorder's events with its track and session time. Time is
//! a 1 kHz sample clock, so a sample is a millisecond.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use nota_core::{EpochId, FakeClock, SessionId};

use super::*;
use crate::fs::fake::FakeFs;
use crate::segment::SegmentLength;
use crate::session::SessionDir;

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

/// How long a test waits for the recorder: far more than it needs.
const WAIT: Duration = Duration::from_secs(10);

/// What [`record_tracks`] reported, each with its track.
type Reported = Vec<(Option<TrackId>, RecorderEvent)>;

/// A detector's report: its track, what it noticed, raised or cleared,
/// and when.
type Detection = (TrackId, Condition, WarningState, SessionTime);

fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

/// `n` samples of a steady tone: never zero, never quiet for long.
fn tone(n: usize) -> Vec<i16> {
    vec![1_000; n]
}

fn silence(n: usize) -> Vec<i16> {
    vec![0; n]
}

/// A writer with each of `started` recording from sample zero.
fn writer_with(started: &[TrackId]) -> SessionWriter<FakeFs> {
    let fs = FakeFs::with_dirs([PathBuf::from("/session")]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SessionId::new(1), fs, &PathBuf::from("/session"))
        .lock()
        .unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, clock).unwrap();
    for &track in started {
        let epoch = writer.test_epoch(track, EpochId::new(0), SampleIndex::ZERO);
        writer.start_track(track, &epoch).unwrap();
    }
    writer
}

/// A timeline for `track`, its first epoch at session time zero.
fn timeline(track: TrackId) -> TrackTimeline {
    let mut timeline = TrackTimeline::new(track);
    timeline
        .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
        .unwrap();
    timeline
}

/// A receiver for `tracks` on `queue`, reading `clock`, as a test builds
/// it by hand.
fn receiver(tracks: &[TrackId], queue: Arc<Queue>, clock: &Arc<FakeClock>) -> CaptureReceiver {
    CaptureReceiver {
        events: queue,
        rate: rate(),
        tracks: test_tracks(tracks),
        clock: Arc::<FakeClock>::clone(clock),
        thresholds: BTreeMap::new(),
    }
}

/// Records the events `send` queues for `tracks`, each started on the
/// writer and timed from zero, until they all end. `hook` sees each event
/// as it's reported, and may move `clock` on while the recorder is busy.
fn record(
    tracks: &[TrackId],
    clock: &Arc<FakeClock>,
    send: impl FnOnce(&QueueSender),
    hook: &mut dyn FnMut(&RecorderEvent),
) -> Reported {
    let mut writer = writer_with(tracks);
    let mut timelines: Vec<TrackTimeline> = tracks.iter().map(|&t| timeline(t)).collect();
    let (tx, queue) = test_channel();
    send(&tx);
    let events = receiver(tracks, queue, clock);
    let mut reported = Vec::new();
    record_tracks(&mut writer, &mut timelines, &events, &mut |track, event| {
        hook(&event);
        reported.push((track, event));
    })
    .unwrap();
    reported
}

/// What the detectors reported, in order.
fn detections(reported: &Reported) -> Vec<Detection> {
    reported
        .iter()
        .filter_map(|(track, event)| match (track, event) {
            (
                Some(track),
                RecorderEvent::Detected {
                    condition,
                    state,
                    at,
                },
            ) => Some((*track, *condition, *state, *at)),
            _ => None,
        })
        .collect()
}

/// A backend that keeps each stream's sender for the test to send with,
/// as an audio server's callback would.
#[derive(Default)]
struct Senders(Mutex<Vec<CaptureSender>>);

impl Senders {
    /// The sender of the stream started first.
    fn first(&self) -> CaptureSender {
        self.0.lock().unwrap()[0].clone()
    }
}

impl CaptureBackend for Senders {
    type Stream = ();

    fn start(&self, _: &Source, _: SampleRate, events: CaptureSender) -> Result<(), CaptureError> {
        self.0.lock().unwrap().push(events);
        Ok(())
    }
}

/// What a recorder on its own thread has reported so far.
struct Seen {
    rx: mpsc::Receiver<(Option<TrackId>, RecorderEvent)>,
    all: Reported,
}

impl Seen {
    /// Takes in reports until one satisfies `found`.
    fn until(&mut self, found: impl Fn(&RecorderEvent) -> bool) {
        loop {
            let got = self.rx.recv_timeout(WAIT).unwrap();
            let hit = found(&got.1);
            self.all.push(got);
            if hit {
                return;
            }
        }
    }

    /// Everything reported, once the recorder has returned.
    fn rest(mut self) -> Reported {
        self.all.extend(self.rx.try_iter());
        self.all
    }
}

fn is_stall(event: &RecorderEvent, wanted: WarningState) -> bool {
    matches!(
        event,
        RecorderEvent::Detected { condition: Condition::Stalled, state, .. } if *state == wanted
    )
}

/// Zeros last their threshold and are raised at the first of them, then
/// cleared at the first sample after.
#[test]
fn zeros_are_raised_at_the_first_zero_and_cleared_at_the_first_sound() {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let reported = record(
        &[MIC],
        &clock,
        |tx| {
            tx.send(MIC, CaptureEvent::Audio(tone(100)));
            // The microphone's 5 s of exact zeros, from sample 100.
            tx.send(MIC, CaptureEvent::Audio(silence(5_000)));
            tx.send(MIC, CaptureEvent::Audio(tone(100)));
            tx.send(MIC, CaptureEvent::Stopped);
        },
        &mut |_| {},
    );
    assert_eq!(
        detections(&reported),
        [
            (MIC, Condition::DigitalZeros, WarningState::Raised, ms(100)),
            (
                MIC,
                Condition::DigitalZeros,
                WarningState::Cleared,
                ms(5_100)
            ),
        ]
    );
}

/// A change is reported after the audio it was noticed in, so a screen has
/// the track's audio by then.
#[test]
fn a_change_follows_the_audio_it_was_seen_in() {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let reported = record(
        &[MIC],
        &clock,
        |tx| {
            tx.send(MIC, CaptureEvent::Audio(silence(5_000)));
            tx.send(MIC, CaptureEvent::Stopped);
        },
        &mut |_| {},
    );
    // Journals finishing in between aren't the point.
    let kinds: Vec<&str> = reported
        .iter()
        .filter_map(|(_, event)| match event {
            RecorderEvent::Audio(_) => Some("audio"),
            RecorderEvent::Detected { .. } => Some("detected"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["audio", "detected", "detected"]);
}

/// Zeros on one track say nothing about the other, and the event names the
/// track the zeros were on.
#[test]
fn zeros_on_one_track_are_reported_for_that_track_only() {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let reported = record(
        &[MIC, SYSTEM],
        &clock,
        |tx| {
            for track in [MIC, SYSTEM] {
                tx.send(track, CaptureEvent::Audio(tone(100)));
            }
            tx.send(SYSTEM, CaptureEvent::Audio(tone(5_000)));
            tx.send(MIC, CaptureEvent::Audio(silence(5_000)));
            tx.send(MIC, CaptureEvent::Audio(tone(100)));
            tx.send(SYSTEM, CaptureEvent::Audio(tone(100)));
            tx.send(MIC, CaptureEvent::Stopped);
            tx.send(SYSTEM, CaptureEvent::Stopped);
        },
        &mut |_| {},
    );
    assert_eq!(
        detections(&reported),
        [
            (MIC, Condition::DigitalZeros, WarningState::Raised, ms(100)),
            (
                MIC,
                Condition::DigitalZeros,
                WarningState::Cleared,
                ms(5_100)
            ),
        ]
    );
}

/// A stream that ends while its zeros are raised clears them, at the end
/// of what was recorded.
#[test]
fn zeros_still_raised_when_the_stream_stops_are_cleared() {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let reported = record(
        &[MIC],
        &clock,
        |tx| {
            tx.send(MIC, CaptureEvent::Audio(tone(100)));
            tx.send(MIC, CaptureEvent::Audio(silence(5_200)));
            tx.send(MIC, CaptureEvent::Stopped);
        },
        &mut |_| {},
    );
    assert_eq!(
        detections(&reported),
        [
            (MIC, Condition::DigitalZeros, WarningState::Raised, ms(100)),
            (
                MIC,
                Condition::DigitalZeros,
                WarningState::Cleared,
                ms(5_300)
            ),
        ]
    );
}

/// Audio then ten seconds of nothing, the clock moved on while the
/// recorder handled a device event, then the stream stops. `started` says
/// whether the stream confirmed its start.
fn silent_after_audio(started: bool) -> Vec<Detection> {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let moved = Arc::clone(&clock);
    let reported = record(
        &[MIC],
        &clock,
        |tx| {
            if started {
                tx.send(MIC, CaptureEvent::Started);
            }
            tx.send(MIC, CaptureEvent::Audio(tone(10)));
            tx.send(
                MIC,
                CaptureEvent::Device {
                    change: DeviceChange::Format,
                    at: SessionTime::ZERO,
                },
            );
            tx.send(MIC, CaptureEvent::Stopped);
        },
        &mut |event| {
            if matches!(event, RecorderEvent::Device { .. }) {
                moved.advance(Duration::from_secs(10));
            }
        },
    );
    detections(&reported)
}

/// A started stream whose samples stop arriving is stalled from when they
/// were last seen, and its end clears that.
#[test]
fn a_started_track_with_no_samples_stalls_and_the_stop_clears_it() {
    assert_eq!(
        silent_after_audio(true),
        [
            (MIC, Condition::Stalled, WarningState::Raised, ms(0)),
            (MIC, Condition::Stalled, WarningState::Cleared, ms(10_000)),
        ]
    );
}

/// A stream still opening delivers nothing for seconds (cpal waits 4 s for
/// a server that doesn't answer), and isn't stalled.
#[test]
fn a_track_that_has_not_confirmed_its_start_never_stalls() {
    assert_eq!(silent_after_audio(false), []);
}

/// The stall reads what the stream delivered, not what the recorder has
/// got to: audio waiting in the queue while the recorder is busy isn't a
/// stall.
#[test]
fn audio_waiting_in_the_queue_is_not_a_stall() {
    let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(10_000_000_000)));
    let dyn_clock: Arc<dyn Clock> = Arc::<FakeClock>::clone(&clock);
    let (starter, events) = prepare_tracks(&[(MIC, Source::Microphone)], rate(), &dyn_clock);
    let backend = Senders::default();
    let mut captures = Some(starter.start(&backend));
    let sender = backend.first();
    sender.audio(&tone(100));
    let mut writer = writer_with(&[]);
    let mut busy = false;
    let mut reported = Vec::new();
    record_tracks(&mut writer, &mut [], &events, &mut |track, event| {
        // The first thing reported, the joining track's epoch, is the
        // recorder busy: the stream goes on delivering for 3 s meanwhile.
        if !busy {
            busy = true;
            for _ in 0..3 {
                clock.advance(Duration::from_secs(1));
                sender.audio(&tone(100));
            }
            // Then the stream stops, so the recorder returns.
            drop(captures.take());
        }
        reported.push((track, event));
    })
    .unwrap();
    assert_eq!(detections(&reported), []);
    let audio = reported
        .iter()
        .filter(|(_, e)| matches!(e, RecorderEvent::Audio(_)))
        .count();
    assert_eq!(audio, 4);
}

/// A stalled track's samples arriving again clear it, at the time they
/// were seen. The recorder raises it from its idle checks, with no event
/// to wake it.
#[test]
fn a_stall_is_raised_while_idle_and_cleared_when_samples_return() {
    let clock = Arc::new(FakeClock::new(SessionTime::from_nanos(10_000_000_000)));
    let dyn_clock: Arc<dyn Clock> = Arc::<FakeClock>::clone(&clock);
    let (starter, events) = prepare_tracks(&[(MIC, Source::Microphone)], rate(), &dyn_clock);
    let backend = Senders::default();
    let mut writer = writer_with(&[]);
    let (tx, rx) = mpsc::channel();
    thread::scope(|scope| {
        let recorder = scope.spawn(move || {
            record_tracks(&mut writer, &mut [], &events, &mut |track, event| {
                let _ = tx.send((track, event));
            })
        });
        let captures = starter.start(&backend);
        let sender = backend.first();
        let mut seen = Seen {
            rx,
            all: Vec::new(),
        };
        sender.audio(&tone(100));
        // Once the recorder has the audio it has also seen the count move,
        // at 10 s.
        seen.until(|e| matches!(e, RecorderEvent::Audio(_)));
        clock.advance(Duration::from_secs(3));
        seen.until(|e| is_stall(e, WarningState::Raised));
        sender.audio(&tone(100));
        seen.until(|e| is_stall(e, WarningState::Cleared));
        drop(captures);
        recorder.join().unwrap().unwrap();
        assert_eq!(
            detections(&seen.rest()),
            [
                (MIC, Condition::Stalled, WarningState::Raised, ms(10_000)),
                (MIC, Condition::Stalled, WarningState::Cleared, ms(13_000)),
            ]
        );
    });
}

/// The stream's report that its device went away reaches the recorder's
/// events with the track and the session time it was noticed at.
#[test]
fn a_lost_device_is_reported_with_its_track_and_time() {
    let clock = Arc::new(FakeClock::new(ms(1_234)));
    let dyn_clock: Arc<dyn Clock> = Arc::<FakeClock>::clone(&clock);
    let (starter, events) = prepare_tracks(&[(MIC, Source::Microphone)], rate(), &dyn_clock);
    let backend = Senders::default();
    let captures = starter.start(&backend);
    backend.first().device(DeviceChange::Lost);
    drop(captures);
    let mut writer = writer_with(&[]);
    let mut devices = Vec::new();
    record_tracks(&mut writer, &mut [], &events, &mut |track, event| {
        if let RecorderEvent::Device { change, at } = event {
            devices.push((track, change, at));
        }
    })
    .unwrap();
    assert_eq!(devices, [(Some(MIC), DeviceChange::Lost, ms(1_234))]);
}

/// A stream that fails twice is reported failing once, and the audio it
/// still delivers afterwards, from a device it was moved to, isn't queued.
#[test]
fn a_stream_is_reported_failed_once_and_sends_no_audio_after() {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let dyn_clock: Arc<dyn Clock> = Arc::<FakeClock>::clone(&clock);
    let (starter, events) = prepare_tracks(&[(MIC, Source::Microphone)], rate(), &dyn_clock);
    let backend = Senders::default();
    let _captures = starter.start(&backend);
    let sender = backend.first();
    sender.failed(CaptureError::Backend("first".into()));
    sender.failed(CaptureError::Backend("second".into()));
    sender.audio(&tone(100));
    let mut writer = writer_with(&[]);
    let mut failures = Vec::new();
    let mut audio = 0;
    record_tracks(
        &mut writer,
        &mut [],
        &events,
        &mut |track, event| match event {
            RecorderEvent::CaptureFailed(error) => failures.push((track, error)),
            RecorderEvent::Audio(_) => audio += 1,
            _ => {}
        },
    )
    .unwrap();
    assert_eq!(
        failures,
        [(Some(MIC), CaptureError::Backend("first".into()))]
    );
    assert_eq!(audio, 0);
    assert_eq!(
        events.progress(MIC).map(|p| p.now().delivered),
        Some(SampleIndex::ZERO)
    );
}
