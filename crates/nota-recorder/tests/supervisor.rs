//! The engine supervisor against `nota-fake-engine`, a test double that
//! speaks the protocol and misbehaves on request: crashes, hangs, garbage,
//! wrong replies, a wrong version, a killed process.

// Test code throughout: clippy allows unwraps and panics in it.
#![cfg(test)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use nota_core::messages::AudioChunk;
use nota_core::{Clock, SampleCount, SampleIndex, SampleRate, SessionTime, SystemClock, TrackId};
use nota_recorder::engine::{
    AudioRefused, EngineCommand, EngineConfig, EngineEvent, EngineStatus, EngineStderr,
    EngineSupervisor, OfflineReason,
};

const TRACK: TrackId = TrackId::new(0);
/// 100 ms at 16 kHz, as the recorder sends it.
const CHUNK: u64 = 1_600;

fn fake(args: &[&str]) -> EngineConfig {
    let mut config = EngineConfig::new(EngineCommand {
        program: PathBuf::from(env!("CARGO_BIN_EXE_nota-fake-engine")),
        args: args.iter().map(OsString::from).collect(),
        stderr: EngineStderr::Null,
    });
    config.initial_backoff = Duration::from_millis(20);
    config.max_backoff = Duration::from_millis(200);
    config.start_timeout = Duration::from_secs(10);
    config.request_timeout = Duration::from_secs(10);
    config
}

fn on_track(track: TrackId, k: u64) -> AudioChunk {
    let c = chunk(k);
    AudioChunk::new(
        track,
        c.range().start(),
        SampleRate::SPEECH,
        c.samples().to_vec(),
    )
    .unwrap()
}

/// Chunk `k` on `track`, with a sample the fake engine dies on.
fn poisoned(track: TrackId, k: u64) -> AudioChunk {
    let mut samples = chunk(k).samples().to_vec();
    samples[0] = 1_002;
    AudioChunk::new(
        track,
        SampleIndex::new(k * CHUNK),
        SampleRate::SPEECH,
        samples,
    )
    .unwrap()
}

fn chunk(k: u64) -> AudioChunk {
    let samples = (0..CHUNK)
        .map(|i| i16::try_from(i % 1_000).unwrap())
        .collect();
    AudioChunk::new(
        TRACK,
        SampleIndex::new(k * CHUNK),
        SampleRate::SPEECH,
        samples,
    )
    .unwrap()
}

/// Events as they arrive, with the session time each was seen.
struct Events {
    rx: Receiver<EngineEvent>,
    clock: Arc<SystemClock>,
    seen: Vec<(SessionTime, EngineEvent)>,
}

impl Events {
    /// Waits for an event matching `want`, failing after `within`.
    fn until(&mut self, within: Duration, want: impl Fn(&EngineEvent) -> bool) -> EngineEvent {
        let deadline = self.clock.now().checked_add(within).unwrap();
        loop {
            let left = deadline
                .checked_duration_since(self.clock.now())
                .unwrap_or_else(|| panic!("timed out; saw {:#?}", self.seen));
            let event = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("timed out; saw {:#?}", self.seen));
            self.seen.push((self.clock.now(), event.clone()));
            if want(&event) {
                return event;
            }
        }
    }

    fn confirmed_to(&mut self, end: u64, within: Duration) {
        self.until(
            within,
            |e| matches!(e, EngineEvent::Confirmed { up_to, .. } if up_to.get() >= end),
        );
    }

    fn online(&mut self) -> u32 {
        match self.until(Duration::from_secs(10), |e| {
            matches!(e, EngineEvent::Status(EngineStatus::Online { .. }))
        }) {
            EngineEvent::Status(EngineStatus::Online { pid }) => pid,
            _ => unreachable!(),
        }
    }

    fn offline(&mut self, within: Duration) -> OfflineReason {
        match self.until(within, |e| {
            matches!(e, EngineEvent::Status(EngineStatus::Offline(_)))
        }) {
            EngineEvent::Status(EngineStatus::Offline(reason)) => reason,
            _ => unreachable!(),
        }
    }

    /// The transcripts seen, as `(start, end)`.
    fn transcripts(&self) -> Vec<(u64, u64)> {
        self.seen
            .iter()
            .filter_map(|(_, e)| match e {
                EngineEvent::Transcript(t) => {
                    assert_eq!(
                        t.text,
                        format!("{}-{}", t.range.start().get(), t.range.end().get())
                    );
                    Some((t.range.start().get(), t.range.end().get()))
                }
                _ => None,
            })
            .collect()
    }

    /// Asserts the transcripts cover `[0, end)` exactly once, in order.
    fn assert_tiles(&self, end: u64) {
        let mut next = 0;
        for (from, to) in self.transcripts() {
            assert_eq!(
                from,
                next,
                "text missing or repeated: {:?}",
                self.transcripts()
            );
            next = to;
        }
        assert_eq!(next, end, "{:?}", self.transcripts());
    }
}

fn start(config: EngineConfig) -> (EngineSupervisor, Events) {
    let clock = Arc::new(SystemClock::start().unwrap());
    let (supervisor, rx) = EngineSupervisor::start(config, clock.clone()).unwrap();
    let events = Events {
        rx,
        clock,
        seen: Vec::new(),
    };
    (supervisor, events)
}

fn send(supervisor: &mut EngineSupervisor, chunks: std::ops::Range<u64>) {
    for k in chunks {
        supervisor.send_audio(chunk(k)).unwrap();
    }
}

fn sigkill(pid: u32) {
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

#[test]
fn a_good_engine_transcribes_everything_once() {
    let (mut supervisor, mut events) = start(fake(&["echo", "--every", "3"]));
    events.online();
    send(&mut supervisor, 0..20);
    supervisor.flush(TRACK);
    events.confirmed_to(20 * CHUNK, Duration::from_secs(10));
    events.assert_tiles(20 * CHUNK);
}

/// Acceptance (GAI-128): kill the engine mid-chunk; no audio is lost and
/// text resumes within 10 s.
#[test]
fn sigkill_mid_chunk_loses_nothing_and_resumes() {
    let (mut supervisor, mut events) = start(fake(&["echo", "--every", "5"]));
    let pid = events.online();
    // Seven chunks: the engine answers the first five and holds two.
    send(&mut supervisor, 0..7);
    events.confirmed_to(5 * CHUNK, Duration::from_secs(10));
    assert!(alive(pid));

    sigkill(pid);
    let killed_at = events.clock.now();
    // The recorder carries on regardless.
    send(&mut supervisor, 7..20);

    let reason = events.offline(Duration::from_secs(10));
    assert!(matches!(reason, OfflineReason::Exited(_)), "{reason:?}");
    let new_pid = events.online();
    assert_ne!(new_pid, pid);

    // The two held chunks are resent and answered with the next three.
    events.until(
        Duration::from_secs(10),
        |e| matches!(e, EngineEvent::Transcript(t) if t.range.start().get() == 5 * CHUNK),
    );
    let resumed_at = events.clock.now();
    let took = resumed_at.checked_duration_since(killed_at).unwrap();
    assert!(
        took < Duration::from_secs(10),
        "text resumed after {took:?}"
    );

    supervisor.flush(TRACK);
    events.confirmed_to(20 * CHUNK, Duration::from_secs(10));
    events.assert_tiles(20 * CHUNK);
}

/// Acceptance (GAI-128): a hung engine is killed and restarted after the
/// timeout, and the recorder is never held up by it.
#[test]
fn a_hung_engine_is_killed_and_restarted() {
    let mut config = fake(&["hang"]);
    config.request_timeout = Duration::from_millis(500);
    let (mut supervisor, mut events) = start(config);
    let pid = events.online();
    let before = events.clock.now();
    // Far more than a pipe holds: sending must still not block.
    send(&mut supervisor, 0..2_000);
    let took = events.clock.now().checked_duration_since(before).unwrap();
    assert!(took < Duration::from_secs(2), "sending took {took:?}");

    let reason = events.offline(Duration::from_secs(10));
    assert_eq!(reason, OfflineReason::Hung);
    let hung_after = events
        .seen
        .last()
        .unwrap()
        .0
        .checked_duration_since(before)
        .unwrap();
    assert!(hung_after >= Duration::from_millis(500), "{hung_after:?}");
    assert!(!alive(pid), "the hung engine was killed");
    let again = events.online();
    assert_ne!(again, pid);
    // Still hung: killed again, and again, but its audio isn't taken for
    // what hangs it.
    assert_eq!(events.offline(Duration::from_secs(10)), OfflineReason::Hung);
    assert_eq!(events.offline(Duration::from_secs(10)), OfflineReason::Hung);
    assert_eq!(events.offline(Duration::from_secs(10)), OfflineReason::Hung);
    assert!(
        !events
            .seen
            .iter()
            .any(|(_, e)| matches!(e, EngineEvent::Skipped { .. })),
        "{:#?}",
        events.seen
    );
}

/// A slow engine working through a backlog keeps replying, so it isn't
/// hung even though the last of the backlog waits longer than the timeout.
#[test]
fn a_slow_engine_with_a_backlog_is_not_hung() {
    let mut config = fake(&["echo", "--delay-ms", "100"]);
    config.request_timeout = Duration::from_millis(400);
    let (mut supervisor, mut events) = start(config);
    events.online();
    // A second of decoding queued at once.
    send(&mut supervisor, 0..10);
    events.confirmed_to(10 * CHUNK, Duration::from_secs(10));
    let offline = events
        .seen
        .iter()
        .any(|(_, e)| matches!(e, EngineEvent::Status(EngineStatus::Offline(_))));
    assert!(!offline, "{:#?}", events.seen);
    events.assert_tiles(10 * CHUNK);
}

#[test]
fn shutdown_passes_on_the_answer_to_a_last_flush() {
    let (mut supervisor, mut events) = start(fake(&["echo", "--every", "100"]));
    events.online();
    send(&mut supervisor, 0..3);
    supervisor.flush(TRACK);
    supervisor.shutdown();
    events.confirmed_to(3 * CHUNK, Duration::from_secs(5));
    events.assert_tiles(3 * CHUNK);
}

#[test]
fn an_engine_that_never_says_hello_is_restarted() {
    let mut config = fake(&["no-hello"]);
    config.start_timeout = Duration::from_millis(300);
    let (_supervisor, mut events) = start(config);
    assert_eq!(
        events.offline(Duration::from_secs(10)),
        OfflineReason::StartTimeout
    );
    assert_eq!(
        events.offline(Duration::from_secs(10)),
        OfflineReason::StartTimeout
    );
}

/// Acceptance (GAI-128): a malformed reply restarts the engine rather than
/// panicking, and transcription carries on from the last confirmed sample.
#[test]
fn a_malformed_reply_restarts_the_engine() {
    let (mut supervisor, mut events) = start(fake(&["garbage-after", "--after", "2"]));
    events.online();
    send(&mut supervisor, 0..5);
    // Each engine answers two chunks and then sends garbage: three engines
    // get through the five chunks.
    events.confirmed_to(5 * CHUNK, Duration::from_secs(20));
    let offline: Vec<_> = events
        .seen
        .iter()
        .filter_map(|(_, e)| match e {
            EngineEvent::Status(EngineStatus::Offline(reason)) => Some(reason.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(offline.len(), 2, "{offline:?}");
    for reason in offline {
        assert!(
            matches!(&reason, OfflineReason::Protocol(what) if what.contains("over")),
            "{reason:?}"
        );
    }
    events.assert_tiles(5 * CHUNK);
}

#[test]
fn a_reply_that_doesnt_fit_restarts_the_engine() {
    let (mut supervisor, mut events) = start(fake(&["bad-transcript"]));
    events.online();
    send(&mut supervisor, 0..1);
    assert_eq!(
        events.offline(Duration::from_secs(10)),
        OfflineReason::Protocol("confirmed past the audio sent".into())
    );
}

#[test]
fn a_wrong_version_is_refused() {
    let (_supervisor, mut events) = start(fake(&["wrong-version"]));
    let reason = events.offline(Duration::from_secs(10));
    assert!(
        matches!(reason, OfflineReason::Version(v) if v.get() == 1),
        "{reason:?}"
    );
}

#[test]
fn a_crash_is_reported_and_transcription_resumes() {
    let (mut supervisor, mut events) = start(fake(&["crash-after", "--after", "3"]));
    events.online();
    send(&mut supervisor, 0..6);
    let reason = events.offline(Duration::from_secs(10));
    let OfflineReason::Exited(Some(status)) = reason else {
        panic!("{reason:?}")
    };
    assert_eq!(status.code(), Some(101));
    events.confirmed_to(6 * CHUNK, Duration::from_secs(10));
    events.assert_tiles(6 * CHUNK);
}

/// An engine killed in the middle of writing a frame died; it didn't send
/// a bad reply.
#[test]
fn half_a_frame_then_exit_is_a_crash() {
    let (mut supervisor, mut events) = start(fake(&["torn-after", "--after", "1"]));
    events.online();
    send(&mut supervisor, 0..2);
    let reason = events.offline(Duration::from_secs(10));
    let OfflineReason::Exited(Some(status)) = reason else {
        panic!("{reason:?}")
    };
    assert_eq!(status.code(), Some(101));
}

#[test]
fn an_engine_that_cant_start_is_retried_and_audio_is_bounded() {
    let mut config = fake(&[]);
    config.command.program = PathBuf::from("/nonexistent/nota-engine");
    config.max_unconfirmed = SampleCount::new(5 * CHUNK);
    let (mut supervisor, mut events) = start(config);
    let first = events.offline(Duration::from_secs(10));
    assert!(matches!(first, OfflineReason::SpawnFailed(_)), "{first:?}");
    send(&mut supervisor, 0..8);
    let skipped = events.until(Duration::from_secs(10), |e| {
        matches!(e, EngineEvent::Skipped { .. })
    });
    let EngineEvent::Skipped { track, range } = skipped else {
        unreachable!()
    };
    assert_eq!(track, TRACK);
    assert_eq!(range.start().get(), 0);
    // Retried, with the wait doubling: three more failures come quickly.
    for _ in 0..3 {
        assert!(matches!(
            events.offline(Duration::from_secs(10)),
            OfflineReason::SpawnFailed(_)
        ));
    }
}

#[test]
fn overlapping_audio_is_refused_without_sending() {
    let (mut supervisor, mut events) = start(fake(&["echo"]));
    events.online();
    send(&mut supervisor, 0..2);
    let err = supervisor.send_audio(chunk(1)).unwrap_err();
    assert_eq!(
        err,
        AudioRefused::Overlaps {
            expected: SampleIndex::new(2 * CHUNK),
            got: SampleIndex::new(CHUNK),
        }
    );
    // A jump forward starts a new stream, which the engine accepts.
    send(&mut supervisor, 5..6);
    events.confirmed_to(6 * CHUNK, Duration::from_secs(10));
    let ranges = events.transcripts();
    assert_eq!(
        ranges,
        [(0, CHUNK), (CHUNK, 2 * CHUNK), (5 * CHUNK, 6 * CHUNK)]
    );
    let offline = events
        .seen
        .iter()
        .any(|(_, e)| matches!(e, EngineEvent::Status(EngineStatus::Offline(_))));
    assert!(!offline, "{:#?}", events.seen);
}

#[test]
fn audio_the_engine_cant_take_is_refused() {
    let (mut supervisor, _events) = start(fake(&["echo"]));
    let rate = SampleRate::new(48_000).unwrap();
    let chunk = AudioChunk::new(TRACK, SampleIndex::ZERO, rate, vec![0; 10]).unwrap();
    assert_eq!(supervisor.send_audio(chunk), Err(AudioRefused::Rate(rate)));
    // Nothing was queued: the track still starts at zero.
    supervisor.send_audio(self::chunk(0)).unwrap();
}

/// Audio that kills every engine it's given is skipped, up to one chunk's
/// worth, after three engines in a row die on it, rather than replayed
/// forever.
#[test]
fn audio_that_keeps_killing_the_engine_is_skipped() {
    // Only chunk 2 holds the sample value 1002 (`chunk` stays under 1000):
    // the fake dies on it.
    let (mut supervisor, mut events) = start(fake(&["echo", "--poison", "1002"]));
    events.online();
    send(&mut supervisor, 0..2);
    supervisor.send_audio(poisoned(TRACK, 2)).unwrap();
    send(&mut supervisor, 3..5);
    let skipped = events.until(Duration::from_secs(20), |e| {
        matches!(e, EngineEvent::Skipped { .. })
    });
    let EngineEvent::Skipped { track, range } = skipped else {
        unreachable!()
    };
    assert_eq!(track, TRACK);
    // Everything queued from the poison on, being under a chunk's worth.
    assert_eq!(
        (range.start().get(), range.end().get()),
        (2 * CHUNK, 5 * CHUNK)
    );
    // Transcription carries on after it.
    send(&mut supervisor, 5..7);
    events.confirmed_to(7 * CHUNK, Duration::from_secs(10));
    assert_eq!(
        events.transcripts(),
        [
            (0, CHUNK),
            (CHUNK, 2 * CHUNK),
            (5 * CHUNK, 6 * CHUNK),
            (6 * CHUNK, 7 * CHUNK)
        ]
    );
}

/// Poison on one track doesn't cost another track its audio.
#[test]
fn poison_on_one_track_spares_the_others() {
    let other = TrackId::new(1);
    let (mut supervisor, mut events) = start(fake(&["echo", "--poison", "1002"]));
    events.online();
    supervisor.send_audio(poisoned(TRACK, 0)).unwrap();
    for k in 0..3 {
        supervisor.send_audio(on_track(other, k)).unwrap();
    }
    events.until(Duration::from_secs(20), |e| {
        matches!(e, EngineEvent::Confirmed { track, up_to } if *track == other && up_to.get() == 3 * CHUNK)
    });
    for (_, event) in &events.seen {
        if let EngineEvent::Skipped { track, .. } = event {
            assert_eq!(*track, TRACK, "{:#?}", events.seen);
        }
    }
}

/// Audio that stops coming without a flush is flushed after a while, so
/// the engine isn't left holding it (and isn't then taken for hung).
#[test]
fn a_track_whose_audio_stops_is_flushed() {
    let mut config = fake(&["echo", "--every", "100"]);
    config.idle_flush = Duration::from_millis(200);
    config.request_timeout = Duration::from_millis(600);
    let (mut supervisor, mut events) = start(config);
    events.online();
    send(&mut supervisor, 0..3);
    events.confirmed_to(3 * CHUNK, Duration::from_secs(5));
    events.assert_tiles(3 * CHUNK);
    // The stream goes on after the flush.
    send(&mut supervisor, 3..4);
    events.confirmed_to(4 * CHUNK, Duration::from_secs(5));
    let offline = events
        .seen
        .iter()
        .any(|(_, e)| matches!(e, EngineEvent::Status(EngineStatus::Offline(_))));
    assert!(!offline, "{:#?}", events.seen);
}

#[test]
fn shutdown_stops_the_engine() {
    let (supervisor, mut events) = start(fake(&["echo"]));
    let pid = events.online();
    supervisor.shutdown();
    assert!(!alive(pid));
}

#[test]
fn shutdown_kills_an_engine_that_ignores_its_stdin() {
    let (supervisor, mut events) = start(fake(&["deaf"]));
    let pid = events.online();
    let before = events.clock.now();
    supervisor.shutdown();
    let took = events.clock.now().checked_duration_since(before).unwrap();
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(!alive(pid));
}

#[test]
fn dropping_the_handle_stops_the_engine() {
    let (supervisor, mut events) = start(fake(&["deaf"]));
    let pid = events.online();
    drop(supervisor);
    assert!(!alive(pid));
}
