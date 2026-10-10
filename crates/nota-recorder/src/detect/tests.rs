use std::path::PathBuf;

use super::*;

const RATE: SampleRate = match SampleRate::new(16_000) {
    Some(rate) => rate,
    None => panic!("16 kHz is a rate"),
};

/// Samples a second at [`RATE`].
const SECOND: u64 = 16_000;

/// How many samples a buffer holds as the tests feed them: 20 ms, a
/// `PipeWire` quantum's worth.
const BUFFER: usize = 320;

/// Feeds a track's audio to [`Levels`] buffer by buffer, as the recorder
/// does, and keeps what they report.
struct Track {
    levels: Levels,
    next: SampleIndex,
    changes: Vec<Change>,
}

impl Track {
    fn new(thresholds: &Thresholds) -> Self {
        Self {
            levels: Levels::new(RATE, thresholds),
            next: SampleIndex::ZERO,
            changes: Vec::new(),
        }
    }

    fn mic() -> Self {
        Self::new(&Thresholds::MICROPHONE)
    }

    /// Feeds `samples` in buffers of [`BUFFER`].
    fn play(&mut self, samples: &[i16]) {
        for buffer in samples.chunks(BUFFER) {
            let changes = self.levels.push(self.next, buffer);
            self.changes.extend(changes);
            self.next = self
                .next
                .saturating_add(SampleCount::new(buffer.len() as u64));
        }
    }

    /// Where the audio fed so far ends.
    const fn at(&self) -> SampleIndex {
        self.next
    }

    /// What was reported for `condition`, as (state, sample) pairs.
    fn of(&self, condition: Condition) -> Vec<(WarningState, u64)> {
        self.changes
            .iter()
            .filter(|c| c.condition == condition)
            .map(|c| (c.state, c.at.get()))
            .collect()
    }
}

/// How many samples `ms` milliseconds hold.
const fn len(ms: u64) -> usize {
    (ms * SECOND / 1_000) as usize
}

/// `ms` milliseconds of noise at about `rms`, the same every run.
#[expect(
    clippy::cast_possible_truncation,
    reason = "test signal: values stay well inside i16"
)]
fn noise(ms: u64, rms: f64) -> Vec<i16> {
    let mut state: u32 = 0x1234_5678;
    (0..len(ms))
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // Uniform in [-1, 1), whose RMS is 1/sqrt(3).
            let uniform = f64::from(state >> 8) / f64::from(1_u32 << 23) - 1.0;
            let value = (uniform * rms * 3_f64.sqrt()).round();
            // Never an exact zero, so noise never reads as digital zeros.
            if value == 0.0 { 1 } else { value as i16 }
        })
        .collect()
}

/// `ms` milliseconds of a 440 Hz tone at about `rms`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "test signal: values stay well inside i16"
)]
fn tone(ms: u64, rms: f64) -> Vec<i16> {
    (0..len(ms))
        .map(|i| {
            let phase = 2.0 * std::f64::consts::PI * 440.0 * f64::from(i as u32) / 16_000.0;
            (phase.sin() * rms * 2_f64.sqrt()).round() as i16
        })
        .collect()
}

/// `ms` milliseconds of exact zeros.
fn zeros(ms: u64) -> Vec<i16> {
    vec![0; len(ms)]
}

/// A room's noise at about -60 dBFS.
const ROOM: f64 = 33.0;

/// Speech, roughly: at about -26 dBFS.
const VOICE: f64 = 1_600.0;

/// "Speech": half a second of the room, then a second of tone, `n` times.
fn talking(n: usize) -> Vec<i16> {
    (0..n)
        .flat_map(|_| [noise(500, ROOM), tone(1_000, VOICE)].concat())
        .collect()
}

/// Exact zeros on a microphone are raised once they've lasted 5 s, at the
/// first zero, and not a buffer before.
#[test]
fn zeros_on_a_microphone_are_raised_after_five_seconds() {
    let mut mic = Track::mic();
    mic.play(&noise(1_000, ROOM));
    let first_zero = mic.at().get();
    // A buffer short of 5 s: nothing yet.
    mic.play(&zeros(4_980));
    assert_eq!(mic.of(Condition::DigitalZeros), []);
    mic.play(&zeros(20));
    assert_eq!(
        mic.of(Condition::DigitalZeros),
        [(WarningState::Raised, first_zero)]
    );
}

/// Digital zeros clear at the first sample that isn't zero.
#[test]
fn zeros_clear_at_the_first_sample_that_isnt_zero() {
    let mut mic = Track::mic();
    mic.play(&zeros(6_000));
    // The non-zero sample sits inside a buffer, not at its start.
    let mut back = zeros(5);
    let first_sound = mic.at().get() + back.len() as u64;
    back.extend(noise(1_000, ROOM));
    mic.play(&back);
    assert_eq!(
        mic.of(Condition::DigitalZeros),
        [
            (WarningState::Raised, 0),
            (WarningState::Cleared, first_sound)
        ]
    );
}

/// The system audio's exact zeros (nothing playing) wait 30 s, not the
/// microphone's 5 s.
#[test]
fn zeros_on_the_system_audio_wait_thirty_seconds() {
    let mut system = Track::new(&Thresholds::SYSTEM_AUDIO);
    system.play(&zeros(29_900));
    assert_eq!(system.of(Condition::DigitalZeros), []);
    system.play(&zeros(100));
    assert_eq!(
        system.of(Condition::DigitalZeros),
        [(WarningState::Raised, 0)]
    );
}

/// A run of zeros shorter than the threshold reports nothing, raised or
/// cleared, however often it comes.
#[test]
fn short_runs_of_zeros_report_nothing() {
    let mut mic = Track::mic();
    for _ in 0..10 {
        mic.play(&zeros(4_900));
        mic.play(&noise(100, ROOM));
    }
    assert_eq!(mic.of(Condition::DigitalZeros), []);
}

/// After speech, the room's noise alone is raised as quiet once it has
/// lasted 30 s, from the first frame without speech, and cleared at the
/// frame where speech comes back.
#[test]
fn the_room_alone_is_quiet_after_thirty_seconds() {
    let mut mic = Track::mic();
    mic.play(&talking(8));
    let pause = mic.at().get();
    // A buffer under 30 s: nothing yet.
    mic.play(&noise(29_940, ROOM));
    assert_eq!(mic.of(Condition::Quiet), []);
    mic.play(&noise(60, ROOM));
    assert_eq!(mic.of(Condition::Quiet), [(WarningState::Raised, pause)]);
    let back = mic.at().get();
    mic.play(&tone(1_000, VOICE));
    assert_eq!(
        mic.of(Condition::Quiet),
        [(WarningState::Raised, pause), (WarningState::Cleared, back)]
    );
}

/// Speech with pauses well under 30 s is never quiet, nor are its
/// pauses' exact zeros, if any, digital zeros.
#[test]
fn speech_with_pauses_raises_nothing() {
    let mut mic = Track::mic();
    for _ in 0..6 {
        mic.play(&talking(4));
        mic.play(&noise(20_000, ROOM));
        mic.play(&zeros(3_000));
    }
    assert_eq!(mic.changes, []);
}

/// A steady loud sound, music say, isn't quiet, though the floor learns
/// its level.
#[test]
fn a_steady_loud_sound_is_never_quiet() {
    let mut system = Track::new(&Thresholds::SYSTEM_AUDIO);
    system.play(&tone(90_000, 4_000.0));
    assert_eq!(system.changes, []);
}

/// A faint hiss on a nearly silent track is quiet, though it's well above
/// the floor that track would learn on its own.
#[test]
fn a_faint_hiss_is_quiet_whatever_the_floor() {
    let mut mic = Track::mic();
    // Near-silence teaches a floor far below the hiss.
    mic.play(&noise(10_000, 1.0));
    mic.play(&noise(31_000, 12.0));
    // Quiet from the start: the hiss never counted as sound.
    assert_eq!(mic.of(Condition::Quiet), [(WarningState::Raised, 0)]);
}

/// A room that gets noisier is quiet again at its new level once the
/// floor has learned it, about 10 s later.
#[test]
fn the_floor_follows_a_noisier_room() {
    let mut mic = Track::mic();
    mic.play(&talking(4));
    mic.play(&noise(5_000, ROOM));
    let louder = mic.at().get();
    // 20 dB up: sound, at first, against the old floor.
    mic.play(&noise(45_000, ROOM * 10.0));
    let [(WarningState::Raised, from)] = mic.of(Condition::Quiet)[..] else {
        panic!("{:?}", mic.changes);
    };
    let learned_ms = (from - louder) * 1_000 / SECOND;
    assert!(
        (9_000..=11_500).contains(&learned_ms),
        "learned after {learned_ms} ms"
    );
}

/// Exact zeros are quiet too, so a long silence raises both: digital
/// zeros first, quiet after 30 s. The screen chooses what to show.
#[test]
fn a_long_silence_raises_zeros_then_quiet() {
    let mut mic = Track::mic();
    mic.play(&zeros(30_000));
    assert_eq!(
        mic.changes,
        [
            Change {
                condition: Condition::DigitalZeros,
                state: WarningState::Raised,
                at: SampleIndex::ZERO,
            },
            Change {
                condition: Condition::Quiet,
                state: WarningState::Raised,
                at: SampleIndex::ZERO,
            },
        ]
    );
}

/// An ended stream clears what was raised, where its audio ended, and
/// nothing that wasn't.
#[test]
fn ending_clears_what_was_raised() {
    let mut mic = Track::mic();
    mic.play(&zeros(6_000));
    let end = mic.at();
    assert_eq!(
        mic.levels.end(end),
        [Change {
            condition: Condition::DigitalZeros,
            state: WarningState::Cleared,
            at: end,
        }]
    );
    let mut quiet = Track::mic();
    quiet.play(&noise(3_000, ROOM));
    assert_eq!(quiet.levels.end(quiet.at()), []);
}

/// A session time `ms` milliseconds in.
const fn ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

/// A stream that stops delivering is stalled after 2 s, from the last
/// time its samples were seen arriving, and not a check before.
#[test]
fn no_samples_for_two_seconds_is_a_stall() {
    let mut stall = Stall::new(STALLED_AFTER);
    let delivered = SampleIndex::new(320);
    assert_eq!(stall.check(SampleIndex::ZERO, ms(0), Duration::ZERO), None);
    assert_eq!(stall.check(delivered, ms(100), Duration::ZERO), None);
    assert_eq!(stall.check(delivered, ms(2_099), Duration::ZERO), None);
    assert_eq!(
        stall.check(delivered, ms(2_100), Duration::ZERO),
        Some(Stalled {
            state: WarningState::Raised,
            at: ms(100),
        })
    );
    // Raised once, however long it lasts.
    assert_eq!(stall.check(delivered, ms(9_000), Duration::ZERO), None);
}

/// A stall clears when samples arrive again, at the check that sees them.
#[test]
fn a_stall_clears_when_samples_arrive() {
    let mut stall = Stall::new(STALLED_AFTER);
    stall.check(SampleIndex::ZERO, ms(0), Duration::ZERO);
    stall.check(SampleIndex::ZERO, ms(3_000), Duration::ZERO);
    assert_eq!(
        stall.check(SampleIndex::new(1), ms(3_500), Duration::ZERO),
        Some(Stalled {
            state: WarningState::Cleared,
            at: ms(3_500),
        })
    );
    // Watched again from there.
    assert_eq!(
        stall.check(SampleIndex::new(1), ms(5_000), Duration::ZERO),
        None
    );
}

/// A stream that never delivers its first samples stalls too, counted
/// from the first check.
#[test]
fn a_stream_with_no_first_samples_stalls() {
    let mut stall = Stall::new(STALLED_AFTER);
    stall.check(SampleIndex::ZERO, ms(400), Duration::ZERO);
    assert_eq!(
        stall.check(SampleIndex::ZERO, ms(2_400), Duration::ZERO),
        Some(Stalled {
            state: WarningState::Raised,
            at: ms(400),
        })
    );
}

/// Time spent suspended isn't a stall: every stream is quiet while the
/// machine sleeps.
#[test]
fn a_suspend_is_not_a_stall() {
    let mut stall = Stall::new(STALLED_AFTER);
    stall.check(SampleIndex::ZERO, ms(1_000), Duration::from_secs(5));
    // 60 s later, 59 of them asleep: a second awake without samples.
    assert_eq!(
        stall.check(SampleIndex::ZERO, ms(61_000), Duration::from_secs(64)),
        None
    );
    assert_eq!(
        stall.check(SampleIndex::ZERO, ms(62_000), Duration::from_secs(64)),
        Some(Stalled {
            state: WarningState::Raised,
            at: ms(1_000),
        })
    );
}

/// An ended stream clears a raised stall, and stops being watched.
#[test]
fn ending_a_stalled_stream_clears_it() {
    let mut stall = Stall::new(STALLED_AFTER);
    assert_eq!(stall.end(ms(0)), None);
    stall.check(SampleIndex::ZERO, ms(0), Duration::ZERO);
    stall.check(SampleIndex::ZERO, ms(2_000), Duration::ZERO);
    assert_eq!(
        stall.end(ms(2_500)),
        Some(Stalled {
            state: WarningState::Cleared,
            at: ms(2_500),
        })
    );
    // Not watched: the next check starts afresh.
    assert_eq!(
        stall.check(SampleIndex::ZERO, ms(9_000), Duration::ZERO),
        None
    );
}

/// The bench fixture's speech and pauses, from `scripts/fetch-test-models.sh`.
fn fixture() -> Option<Vec<i16>> {
    let root = std::env::var_os("NOTA_TEST_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/nota/test-models"))
        })?;
    let Ok(bytes) = std::fs::read(root.join("fixtures/invented-lecture.wav")) else {
        assert!(
            std::env::var_os("NOTA_REQUIRE_TEST_MODELS").is_none_or(|v| v != "1"),
            "NOTA_REQUIRE_TEST_MODELS=1 but no fixture (run scripts/fetch-test-models.sh)"
        );
        return None;
    };
    Some(wav_samples(&bytes))
}

/// The samples of a 16 kHz mono 16-bit WAV file's `data` chunk.
fn wav_samples(bytes: &[u8]) -> Vec<i16> {
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = &bytes[at + 8..(at + 8 + len).min(bytes.len())];
        if id == b"fmt " {
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let hz = u32::from_le_bytes(body[4..8].try_into().unwrap());
            assert_eq!((channels, hz), (1, 16_000), "the fixture's format");
        }
        if id == b"data" {
            return body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| i16::from_le_bytes(b))
                .collect();
        }
        at += 8 + len + len % 2;
    }
    panic!("no data chunk in the fixture");
}

/// The bench fixture's speech and pauses, looped for five minutes, raise
/// nothing on a microphone, at its own level or 20 dB down, as from the
/// back of a room.
#[test]
fn the_fixture_raises_nothing() {
    let Some(speech) = fixture() else {
        return;
    };
    // Its pauses are exact zeros, as the fixture is made: proof the zeros
    // detector was tested, not just fed sound.
    assert!(speech.windows(3_200).any(|w| w.iter().all(|&s| s == 0)));
    for divisor in [1, 10] {
        let quieter: Vec<i16> = speech.iter().map(|&s| s / divisor).collect();
        let mut mic = Track::mic();
        while mic.at().get() < 300 * SECOND {
            mic.play(&quieter);
        }
        assert_eq!(mic.changes, [], "at 1/{divisor} of its level");
    }
}

/// Samples numbered past the last sample number don't overflow the
/// detectors: the writer refuses them, and they can't be counted, so they
/// raise nothing.
#[test]
fn samples_past_the_last_number_dont_overflow() {
    let mut levels = Levels::new(RATE, &Thresholds::MICROPHONE);
    let changes = levels.push(SampleIndex::new(u64::MAX - 10), &zeros(6_000));
    assert_eq!(changes, []);
}

/// The floor keeps a block of history every 20 frames, a second's worth,
/// not every 21.
#[test]
fn the_floor_keeps_a_block_every_twenty_frames() {
    let mut floor = NoiseFloor::default();
    for _ in 0..FRAMES_PER_BLOCK - 1 {
        floor.learn(500);
    }
    assert_eq!((floor.blocks.len(), floor.current), (0, Some(500)));
    floor.learn(400);
    assert_eq!(
        (
            floor.blocks.iter().copied().collect::<Vec<_>>(),
            floor.current
        ),
        (vec![400], None)
    );
}
