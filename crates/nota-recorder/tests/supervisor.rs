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
    AudioRefused, EngineCommand, EngineConfig, EngineEvent, EngineStatus, EngineSupervisor,
    OfflineReason,
};

const TRACK: TrackId = TrackId::new(0);
/// 100 ms at 16 kHz, as the recorder sends it.
const CHUNK: u64 = 1_600;

fn fake(args: &[&str]) -> EngineConfig {
    let mut config = EngineConfig::new(EngineCommand {
        program: PathBuf::from(env!("CARGO_BIN_EXE_nota-fake-engine")),
        args: args.iter().map(OsString::from).collect(),
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
        // Seen already, while pausing.
        let seen = self
            .seen
            .iter()
            .any(|(_, e)| matches!(e, EngineEvent::Confirmed { up_to, .. } if up_to.get() >= end));
        if seen {
            return;
        }
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

    /// Waits `time`, keeping the events that come meanwhile.
    fn pause(&mut self, time: Duration) {
        let deadline = self.clock.now().checked_add(time).unwrap();
        while let Some(left) = deadline.checked_duration_since(self.clock.now()) {
            match self.rx.recv_timeout(left) {
                Ok(event) => self.seen.push((self.clock.now(), event)),
                Err(_) => break,
            }
        }
    }

    /// The ranges reported skipped, as `(start, end)`.
    fn skipped(&self) -> Vec<(u64, u64)> {
        self.seen
            .iter()
            .filter_map(|(_, e)| match e {
                EngineEvent::Skipped { range, .. } => {
                    Some((range.start().get(), range.end().get()))
                }
                _ => None,
            })
            .collect()
    }

    /// The offline reasons seen, in order.
    fn offline_reasons(&self) -> Vec<OfflineReason> {
        self.seen
            .iter()
            .filter_map(|(_, e)| match e {
                EngineEvent::Status(EngineStatus::Offline(reason)) => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    /// Asserts the transcripts and skipped ranges together cover
    /// `[0, end)` exactly once.
    fn assert_tiles_with_skips(&self, end: u64) {
        let mut ranges = self.transcripts();
        ranges.extend(self.skipped());
        ranges.sort_unstable();
        let mut next = 0;
        for (from, to) in &ranges {
            assert_eq!(*from, next, "missing or repeated: {ranges:?}");
            next = *to;
        }
        assert_eq!(next, end, "{ranges:?}");
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
    assert!(matches!(reason, OfflineReason::Exited { .. }), "{reason:?}");
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
    // Still hung: killed again, and again, but four hangs aren't enough for
    // its audio to be taken for what hangs it.
    assert_eq!(events.offline(Duration::from_secs(10)), OfflineReason::Hung);
    assert_eq!(events.offline(Duration::from_secs(10)), OfflineReason::Hung);
    assert_eq!(events.offline(Duration::from_secs(10)), OfflineReason::Hung);
    // A skip would come with the fourth failure; the fifth is 500 ms off.
    events.pause(Duration::from_millis(200));
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
    let OfflineReason::Exited {
        status: Some(status),
        ..
    } = reason
    else {
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
    let OfflineReason::Exited {
        status: Some(status),
        ..
    } = reason
    else {
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

/// Acceptance (GAI-179): failures raise the backoff past the 10 s budget,
/// but once an engine says hello the next restart comes within 2 s, so an
/// engine killed before it confirms anything still resumes text in time.
#[test]
fn a_kill_after_hello_restarts_soon_however_high_the_backoff() {
    let mut config = fake(&["echo", "--every", "100"]);
    // Uncapped, three kills in a row would wait 3 s, 6 s and then 12 s.
    config.initial_backoff = Duration::from_secs(3);
    config.max_backoff = Duration::from_secs(30);
    let (mut supervisor, mut events) = start(config);
    let mut pid = events.online();
    for _ in 0..2 {
        sigkill(pid);
        events.offline(Duration::from_secs(10));
        pid = events.online();
    }
    // Audio the engine holds, unconfirmed, when it's killed.
    send(&mut supervisor, 0..3);
    sigkill(pid);
    let killed_at = events.clock.now();
    events.offline(Duration::from_secs(10));
    events.online();
    supervisor.flush(TRACK);
    events.until(Duration::from_secs(15), |e| {
        matches!(e, EngineEvent::Transcript(_))
    });
    let took = events
        .clock
        .now()
        .checked_duration_since(killed_at)
        .unwrap();
    assert!(
        took < Duration::from_secs(10),
        "text resumed after {took:?}"
    );
    events.confirmed_to(3 * CHUNK, Duration::from_secs(5));
    events.assert_tiles(3 * CHUNK);
}

/// A running engine slower than real time is restarted once it falls
/// `max_unconfirmed` behind, and the oldest audio dropped, rather than its
/// backlog growing without limit.
#[test]
fn an_engine_that_falls_too_far_behind_is_restarted() {
    let mut config = fake(&["echo", "--delay-ms", "50"]);
    config.max_unconfirmed = SampleCount::new(6 * CHUNK);
    let (mut supervisor, mut events) = start(config);
    events.online();
    send(&mut supervisor, 0..30);
    supervisor.flush(TRACK);
    events.confirmed_to(30 * CHUNK, Duration::from_secs(20));
    assert!(
        events.offline_reasons().contains(&OfflineReason::Behind),
        "{:#?}",
        events.seen
    );
    assert!(!events.skipped().is_empty());
    // Nothing more than the limit was ever held: every gap is accounted
    // for, and no text is repeated.
    events.assert_tiles_with_skips(30 * CHUNK);
}

/// An engine holding exactly the limit isn't behind; one sample more is.
#[test]
fn behind_means_more_than_the_limit() {
    let mut config = fake(&["echo", "--every", "100"]);
    config.max_unconfirmed = SampleCount::new(4 * CHUNK);
    let (mut supervisor, mut events) = start(config);
    events.online();
    send(&mut supervisor, 0..4);
    supervisor.flush(TRACK);
    events.confirmed_to(4 * CHUNK, Duration::from_secs(5));
    assert!(events.offline_reasons().is_empty(), "{:#?}", events.seen);
    send(&mut supervisor, 4..9);
    assert_eq!(
        events.offline(Duration::from_secs(5)),
        OfflineReason::Behind
    );
}

/// Falling behind says nothing about the audio: a quiet track whose
/// first unconfirmed sample stands still while another track falls behind
/// again and again isn't taken for poison.
#[test]
fn falling_behind_is_not_poison() {
    let other = TrackId::new(1);
    let mut config = fake(&["echo", "--every", "100"]);
    config.max_unconfirmed = SampleCount::new(4 * CHUNK);
    let (mut supervisor, mut events) = start(config);
    events.online();
    supervisor.send_audio(on_track(other, 0)).unwrap();
    let mut next = 0;
    for _ in 0..4 {
        send(&mut supervisor, next..next + 5);
        next += 5;
        assert_eq!(
            events.offline(Duration::from_secs(5)),
            OfflineReason::Behind
        );
        events.online();
    }
    events.pause(Duration::from_millis(200));
    let skipped_other = events
        .seen
        .iter()
        .any(|(_, e)| matches!(e, EngineEvent::Skipped { track, .. } if *track == other));
    assert!(!skipped_other, "{:#?}", events.seen);
}

/// A new engine starts with room to catch up: audio that piled up to the
/// limit while it loaded doesn't put it behind on the next chunk, which
/// would restart it before it could confirm anything, every time.
#[test]
fn an_engine_that_loads_slowly_is_not_taken_for_behind() {
    // Loading takes 1 s, in which 10 chunks arrive: more than the limit.
    // Like the real engine it confirms only every few chunks, and decoding
    // takes a while (though it's twice as fast as real time), so the next
    // chunk always arrives before a new engine's first confirmation.
    let mut config = fake(&[
        "echo",
        "--every",
        "3",
        "--delay-ms",
        "150",
        "--hello-delay-ms",
        "1000",
    ]);
    config.max_unconfirmed = SampleCount::new(8 * CHUNK);
    let (mut supervisor, mut events) = start(config);
    // Audio in real time, 100 ms a chunk, for 3 s.
    for k in 0..30 {
        send(&mut supervisor, k..k + 1);
        events.pause(Duration::from_millis(100));
    }
    supervisor.flush(TRACK);
    events.confirmed_to(30 * CHUNK, Duration::from_secs(10));
    assert!(events.offline_reasons().is_empty(), "{:#?}", events.seen);
    // What piled up past half the limit was dropped; the rest is text.
    assert!(!events.skipped().is_empty());
    events.assert_tiles_with_skips(30 * CHUNK);
}

/// Audio that hangs every engine it's given is skipped after five hangs
/// in a row on it, rather than keeping the engine offline for good.
#[test]
fn audio_that_keeps_hanging_the_engine_is_skipped() {
    let mut config = fake(&["echo", "--poison", "1002", "--on-poison", "hang"]);
    config.request_timeout = Duration::from_millis(300);
    let (mut supervisor, mut events) = start(config);
    events.online();
    send(&mut supervisor, 0..2);
    supervisor.send_audio(poisoned(TRACK, 2)).unwrap();
    send(&mut supervisor, 3..5);
    let skipped = events.until(Duration::from_secs(20), |e| {
        matches!(e, EngineEvent::Skipped { .. })
    });
    let EngineEvent::Skipped { range, .. } = skipped else {
        unreachable!()
    };
    assert_eq!(
        (range.start().get(), range.end().get()),
        (2 * CHUNK, 5 * CHUNK)
    );
    assert_eq!(
        events.offline_reasons(),
        vec![OfflineReason::Hung; 5],
        "{:#?}",
        events.seen
    );
    send(&mut supervisor, 5..7);
    events.confirmed_to(7 * CHUNK, Duration::from_secs(10));
    events.assert_tiles_with_skips(7 * CHUNK);
}

/// Text alone isn't progress: an engine that sends transcripts but never
/// confirms them is hung after the request timeout, however often it
/// writes.
#[test]
fn an_engine_that_never_confirms_is_hung() {
    let mut config = fake(&["text-only", "--delay-ms", "150"]);
    config.request_timeout = Duration::from_millis(500);
    let (mut supervisor, mut events) = start(config);
    events.online();
    let sent_at = events.clock.now();
    // Twenty answers, 150 ms apart: 3 s of steady text.
    send(&mut supervisor, 0..20);
    assert_eq!(events.offline(Duration::from_secs(10)), OfflineReason::Hung);
    let took = events.clock.now().checked_duration_since(sent_at).unwrap();
    assert!(took < Duration::from_millis(1_500), "hung after {took:?}");
    // Unconfirmed text is never passed on.
    assert!(events.transcripts().is_empty(), "{:#?}", events.seen);
}

/// Shutting down flushes every track, so the tail's text isn't lost.
#[test]
fn shutdown_flushes_what_the_engine_holds() {
    let other = TrackId::new(1);
    let (mut supervisor, mut events) = start(fake(&["echo", "--every", "100"]));
    events.online();
    send(&mut supervisor, 0..3);
    supervisor.send_audio(on_track(other, 0)).unwrap();
    supervisor.shutdown();
    events.confirmed_to(3 * CHUNK, Duration::from_secs(5));
    events.until(
        Duration::from_secs(5),
        |e| matches!(e, EngineEvent::Confirmed { track, up_to } if *track == other && up_to.get() == CHUNK),
    );
    // Each track's held audio, as one transcript.
    assert_eq!(events.transcripts(), [(0, 3 * CHUNK), (0, CHUNK)]);
    assert!(events.skipped().is_empty(), "{:#?}", events.seen);
}

/// Audio the engine never confirmed by shutdown is reported skipped.
#[test]
fn shutdown_reports_what_was_never_transcribed() {
    let (mut supervisor, mut events) = start(fake(&["hang"]));
    events.online();
    send(&mut supervisor, 0..3);
    supervisor.shutdown();
    // Drain what's left: the channel closes with the supervisor.
    while let Ok(event) = events.rx.recv_timeout(Duration::from_secs(5)) {
        events.seen.push((events.clock.now(), event));
    }
    assert_eq!(events.skipped(), [(0, 3 * CHUNK)]);

    // And with no engine running at all.
    let mut config = fake(&[]);
    config.command.program = PathBuf::from("/nonexistent/nota-engine");
    let (mut supervisor, mut events) = start(config);
    events.offline(Duration::from_secs(10));
    send(&mut supervisor, 0..2);
    supervisor.shutdown();
    while let Ok(event) = events.rx.recv_timeout(Duration::from_secs(5)) {
        events.seen.push((events.clock.now(), event));
    }
    assert_eq!(events.skipped(), [(0, 2 * CHUNK)]);
}

/// An engine that exits says why: the end of its stderr is kept.
#[test]
fn an_exit_reports_the_end_of_stderr() {
    let mut config = fake(&[]);
    config.command.program = PathBuf::from("/bin/sh");
    config.command.args = [
        "-c",
        "echo loading >&2; echo 'Error: no model at /m' >&2; exit 1",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let (_supervisor, mut events) = start(config);
    let reason = events.offline(Duration::from_secs(10));
    let OfflineReason::Exited {
        status: Some(status),
        stderr,
    } = &reason
    else {
        panic!("{reason:?}")
    };
    assert_eq!(status.code(), Some(1));
    assert_eq!(stderr, "loading\nError: no model at /m");
    assert_eq!(
        reason.to_string(),
        "exited (exit status: 1): Error: no model at /m"
    );
}
