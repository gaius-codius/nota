//! Drift arithmetic and the meter, unit by unit, then whole simulated
//! recordings: a device off by a steady rate, read through the meter and a
//! timeline it retimes, must stay within 20 ms of session time.
//!
//! The properties:
//! - with no drift, durations and counts are exactly the nominal rate's
//! - sample → time → sample is exact at every rate and drift allowed
//! - a recording at any drift within the limit stays within 20 ms

use proptest::prelude::*;

use super::*;
use crate::epoch::TrackTimeline;
use crate::ids::TrackId;

const SPEECH: SampleRate = SampleRate::SPEECH;

/// The most a drifting device's audio may be mapped from when it was
/// captured: GAI-315's acceptance criterion.
const WITHIN: Duration = Duration::from_millis(20); // check-bound

fn ppm(ppm: i32) -> Drift {
    Drift::from_ppb(ppm * 1_000).unwrap()
}

fn t(nanos: u64) -> SessionTime {
    SessionTime::from_nanos(nanos)
}

fn s(index: u64) -> SampleIndex {
    SampleIndex::new(index)
}

/// Drift is refused beyond 1000 ppm either way.
#[test]
fn drift_is_bounded_either_way() {
    assert_eq!(Drift::from_ppb(1_000_000).map(Drift::ppb), Some(1_000_000));
    assert_eq!(
        Drift::from_ppb(-1_000_000).map(Drift::ppb),
        Some(-1_000_000)
    );
    assert_eq!(Drift::from_ppb(1_000_001), None);
    assert_eq!(Drift::from_ppb(-1_000_001), None);
    assert_eq!(Drift::default(), Drift::ZERO);
}

/// A fast device's samples each take less time, a slow one's more.
#[test]
fn drift_changes_how_long_samples_last() {
    let second = SampleCount::new(16_000);
    // 16,000 samples from a device 100 ppm fast take 1 s / 1.0001.
    assert_eq!(
        ppm(100).duration_of(second, SPEECH),
        Some(Duration::from_nanos(999_900_010))
    );
    assert_eq!(
        ppm(-100).duration_of(second, SPEECH),
        Some(Duration::from_nanos(1_000_100_011))
    );
    assert_eq!(
        ppm(100).count_within(Duration::from_secs(1), SPEECH),
        Some(SampleCount::new(16_001))
    );
    assert_eq!(
        ppm(-100).count_within(Duration::from_secs(1), SPEECH),
        Some(SampleCount::new(15_998))
    );
}

/// Durations too long for session time, and elapsed times too long to
/// count, are `None`, not wrapped.
#[test]
fn drift_arithmetic_overflow_is_none() {
    assert_eq!(
        ppm(-1_000).duration_of(SampleCount::new(u64::MAX), SPEECH),
        None
    );
    let max = SampleRate::new(SampleRate::MAX_HZ).unwrap();
    assert_eq!(ppm(1_000).count_within(Duration::MAX, max), None);
}

/// The drift measured from what a device delivered against the time it
/// took, held within the limit.
#[test]
fn drift_is_measured_from_samples_and_time() {
    let hour = Duration::from_secs(3_600);
    assert_eq!(
        Drift::measured(SampleCount::new(57_605_760), hour, SPEECH),
        Some(ppm(100))
    );
    assert_eq!(
        Drift::measured(SampleCount::new(57_594_240), hour, SPEECH),
        Some(ppm(-100))
    );
    assert_eq!(
        Drift::measured(SampleCount::new(32_000), Duration::from_secs(1), SPEECH),
        Some(Drift(Drift::MAX_PPB))
    );
    assert_eq!(
        Drift::measured(SampleCount::new(1), Duration::ZERO, SPEECH),
        None
    );
}

/// A timeline with one epoch from zero, at `drift`.
fn epoch_at(drift: Drift) -> TrackTimeline {
    let mut timeline = TrackTimeline::new(TrackId::new(0));
    timeline
        .open_epoch_drifting(SessionTime::ZERO, SampleIndex::ZERO, SPEECH, drift)
        .unwrap();
    timeline
}

/// A stamp at `at` nanoseconds, with no delay.
fn stamped(at: u64) -> Stamp {
    Stamp {
        at: t(at),
        delay: Duration::ZERO,
    }
}

/// Feeds `meter` one buffer starting at sample `first`, captured at `at`.
fn read(meter: &mut DriftMeter, timeline: &TrackTimeline, first: u64, at: u64) -> Reading {
    meter.observe(timeline.current().unwrap(), s(first), stamped(at))
}

/// The sample a run settled at by [`settle_run`]: [`SETTLE`] in.
const SETTLED: u64 = 160_000;

/// When [`SETTLED`] plays, in nanoseconds.
const SETTLED_AT: u64 = 10_000_000_000;

/// Starts a run on `meter` at sample zero and time zero, and settles it at
/// [`SETTLED`], on time: from there it measures.
fn settle_run(meter: &mut DriftMeter, timeline: &TrackTimeline) {
    assert_eq!(read(meter, timeline, 0, 0), Reading::Steady);
    assert_eq!(read(meter, timeline, SETTLED, SETTLED_AT), Reading::Steady);
}

/// A run's first buffer only starts it, however far off: there's nothing
/// yet to say audio was lost before it, and the buffers after it are read
/// against it.
#[test]
fn a_run_s_first_buffer_only_starts_it() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    assert_eq!(read(&mut meter, &timeline, 0, 500_000_000), Reading::Steady);
    // Where the first buffer puts it, half a second behind the mapping.
    assert_eq!(
        read(&mut meter, &timeline, 1_600, 600_000_000),
        Reading::Steady
    );
    // Then 10 ms later than that: a loss, the whole way behind it.
    assert_eq!(
        read(&mut meter, &timeline, 3_200, 710_000_000),
        Reading::Lost {
            hole: Duration::from_millis(510)
        }
    );
}

/// A buffer [`LOSS_MIN`] later than the buffer before it puts it is a
/// loss; a nanosecond less is not.
#[test]
fn a_buffer_late_by_the_loss_minimum_is_a_loss() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    read(&mut meter, &timeline, 0, 0);
    let mut short = meter.clone();
    // Sample 1,600 plays at 100 ms.
    assert_eq!(
        read(&mut short, &timeline, 1_600, 109_999_999),
        Reading::Steady
    );
    assert_eq!(
        read(&mut meter, &timeline, 1_600, 110_000_000),
        Reading::Lost {
            hole: Duration::from_millis(10)
        }
    );
}

/// Once a buffer reads as lost, so does every one after it until the
/// meter restarts: a caller that waits to open its epoch still finds the
/// loss when it's ready.
#[test]
fn a_loss_reads_until_the_run_restarts() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    read(&mut meter, &timeline, 0, 0);
    read(&mut meter, &timeline, 1_600, 150_000_000);
    // On from there, with no jump of its own.
    assert_eq!(
        read(&mut meter, &timeline, 3_200, 250_000_000),
        Reading::Lost {
            hole: Duration::from_millis(50)
        }
    );
    meter.restart();
    assert_eq!(
        read(&mut meter, &timeline, 4_800, 350_000_000),
        Reading::Steady
    );
    assert_eq!(
        read(&mut meter, &timeline, 6_400, 450_000_000),
        Reading::Steady
    );
}

/// Stamps that slide away from the samples a millisecond a buffer, past
/// [`LOSS_MIN`] but never by that much at once, aren't a loss.
#[test]
fn a_slide_past_the_loss_minimum_is_not_a_loss() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    for n in 0..30 {
        let at = n * 101_000_000;
        assert_eq!(read(&mut meter, &timeline, n * 1_600, at), Reading::Steady);
    }
}

/// After a restart the next buffer starts a new run, and the drift
/// measured before is kept.
#[test]
fn a_restart_starts_a_new_run_and_keeps_the_drift() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    settle_run(&mut meter, &timeline);
    read(&mut meter, &timeline, 480_000, 30_000_000_000);
    assert_eq!(meter.measured(), Some(Drift::ZERO));
    meter.restart();
    // A second off would be a loss, but it only starts the new run.
    assert_eq!(
        read(&mut meter, &timeline, 500_000, 32_250_000_000),
        Reading::Steady
    );
    assert_eq!(meter.measured(), Some(Drift::ZERO));
}

/// Until a run has settled the meter neither measures nor corrects,
/// however far the stamps stray short of a loss.
#[test]
fn nothing_is_measured_while_a_run_settles() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    meter.measured = Some(Drift::ZERO);
    read(&mut meter, &timeline, 0, 0);
    // 9 ms behind just short of SETTLE, reached a millisecond a buffer.
    for n in 1..=9 {
        let at = n * 1_000_000_000 + n * 1_000_000;
        let first = n * 16_000;
        let reading = read(&mut meter, &timeline, first, at);
        assert_eq!(reading, Reading::Steady, "{n}");
    }
    assert_eq!(meter.measured(), Some(Drift::ZERO));
    assert!(!meter.from_this_run);
}

/// The meter doesn't correct a mapping before it has watched
/// [`MIN_WINDOW`] of a settled run, however far it strays short of a
/// loss.
#[test]
fn no_retime_before_the_rate_is_known() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    settle_run(&mut meter, &timeline);
    // 9 ms behind at 9 s on: past the retime limit, short of a loss.
    assert_eq!(
        read(
            &mut meter,
            &timeline,
            SETTLED + 144_000,
            SETTLED_AT + 9_009_000_000
        ),
        Reading::Steady
    );
    assert_eq!(meter.measured(), None);
}

/// A run settles at its first buffer [`SETTLE`] in, and the difference
/// from the mapping it has reached then is its own: only a change from it
/// is drift.
#[test]
fn a_run_measures_from_where_it_settled() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    read(&mut meter, &timeline, 0, 0);
    // 8 ms behind at 10 s, reached by a millisecond a buffer.
    for n in 1..=8 {
        read(&mut meter, &timeline, n * 1_600, n * 101_000_000);
    }
    assert_eq!(
        read(&mut meter, &timeline, SETTLED, SETTLED_AT + 8_000_000),
        Reading::Steady
    );
    assert_eq!(
        meter.run.map(|r| r.phase),
        Some(Phase::Settled { base: 8_000_000 })
    );
    // 10 s on, still 8 ms behind: no drift, so nothing to correct.
    let later = SETTLED + 160_000;
    assert_eq!(
        read(&mut meter, &timeline, later, SETTLED_AT + 10_008_000_000),
        Reading::Steady
    );
    assert_eq!(meter.measured(), Some(Drift::ZERO));
}

/// A mapping exactly [`RETIME_AFTER`] off isn't retimed: only more is.
#[test]
fn a_mapping_off_by_the_retime_limit_is_left() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    settle_run(&mut meter, &timeline);
    // Sample 320,000 plays at 20 s; it was captured 4 ms later.
    let first = SETTLED + 160_000;
    assert_eq!(
        read(&mut meter, &timeline, first, SETTLED_AT + 10_004_000_000),
        Reading::Steady
    );
    assert!(meter.measured().is_some());
    assert_eq!(
        read(&mut meter, &timeline, first, SETTLED_AT + 10_004_000_001),
        Reading::Retime(Drift(-399_841 - 66_666))
    );
}

/// When sample `n` of a device off by `ppb` was truly captured.
fn true_ns(n: u64, ppb: i128) -> u64 {
    let hz = i128::from(SPEECH.hz()) * (1_000_000_000 + ppb);
    u64::try_from(i128::from(n) * 1_000_000_000_000_000_000 / hz).unwrap()
}

/// Reads sample `n` of a device off by `ppb` at its true capture time,
/// retiming `timeline` as the meter asks.
fn read_true(meter: &mut DriftMeter, timeline: &mut TrackTimeline, n: u64, ppb: i128) -> Reading {
    let reading = read(meter, timeline, n, true_ns(n, ppb));
    if let Reading::Retime(drift) = reading {
        timeline.retime(s(n), drift).unwrap();
    }
    reading
}

/// A mapping more than [`RETIME_AFTER`] behind the stream, beyond where
/// the run settled, is retimed at a lower drift than measured, to catch up
/// over [`SLEW`]; once that's run its course, the measured drift takes
/// over.
#[test]
fn a_mapping_behind_slews_then_lands_on_the_measured_drift() {
    let mut timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    let slow = -100_000;
    read_true(&mut meter, &mut timeline, 0, slow);
    // Settled at 10 s, a millisecond behind.
    read_true(&mut meter, &mut timeline, SETTLED, slow);
    // 10 s on, the device is a millisecond more behind: measured, not
    // corrected.
    assert_eq!(
        read_true(&mut meter, &mut timeline, SETTLED + 159_984, slow),
        Reading::Steady
    );
    assert_eq!(meter.measured(), Some(ppm(-100)));
    // 55 s in it's 5.5 ms behind, 4.5 ms more than when it settled (less
    // a nanosecond's rounding): slew 75 ppm lower to catch up in 60 s.
    assert_eq!(
        read_true(&mut meter, &mut timeline, 879_912, slow),
        Reading::Retime(Drift(-174_999))
    );
    // Until 60 s of samples later the correction runs on, and then the
    // measured drift takes over.
    let until = 879_912 + 960_000;
    assert_eq!(
        read_true(&mut meter, &mut timeline, until - 1, slow),
        Reading::Steady
    );
    assert_eq!(
        read_true(&mut meter, &mut timeline, until, slow),
        Reading::Retime(ppm(-100))
    );
    assert_eq!(
        read_true(&mut meter, &mut timeline, until + 1_600, slow),
        Reading::Steady
    );
    // A millisecond ahead of the stamps, as it settled.
    let mapped = timeline.time_of(s(until)).unwrap().as_nanos();
    let settled = true_ns(until, slow) - 1_000_000;
    assert!(mapped.abs_diff(settled) < 100_000, "{mapped} {settled}");
}

/// A mapping ahead of the stream is slewed the other way.
#[test]
fn a_mapping_ahead_slews_at_a_higher_drift() {
    let mut timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    let fast = 100_000;
    read_true(&mut meter, &mut timeline, 0, fast);
    // Sample 160,016 is the first captured 10 s in.
    read_true(&mut meter, &mut timeline, SETTLED + 16, fast);
    assert_eq!(
        read_true(&mut meter, &mut timeline, 880_088, fast),
        Reading::Retime(Drift(175_000))
    );
}

/// A drift past the limit, measured over a trusted window, is reported
/// once, with the drift measured.
#[test]
fn a_drift_past_the_limit_is_reported_once() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    settle_run(&mut meter, &timeline);
    // 300 ppm exactly over a minute isn't past it.
    let minute = SETTLED_AT + 60_000_000_000;
    read(&mut meter, &timeline, SETTLED + 960_288, minute);
    assert_eq!(meter.measured(), Some(ppm(300)));
    assert_eq!(meter.past_limit(), None);
    let mut meter = DriftMeter::new();
    settle_run(&mut meter, &timeline);
    // 960,324 samples in a minute is 337.5 ppm.
    read(&mut meter, &timeline, SETTLED + 960_324, minute);
    assert_eq!(meter.past_limit(), Some(Drift(337_500)));
    assert_eq!(meter.past_limit(), None);
}

/// A short window's estimate past the limit isn't reported: a quantum lost
/// unseen early in a run moves it that far, and a minute's run settles it.
#[test]
fn a_short_window_past_the_limit_is_not_reported() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    settle_run(&mut meter, &timeline);
    // 5 ms short over 10 s: 500 ppm slow, as one unseen loss makes it.
    read(
        &mut meter,
        &timeline,
        SETTLED + 159_920,
        SETTLED_AT + 10_000_000_000,
    );
    assert_eq!(meter.measured(), Some(ppm(-500)));
    assert_eq!(meter.past_limit(), None);
    // By a minute the same 5 ms is under 100 ppm.
    read(
        &mut meter,
        &timeline,
        SETTLED + 959_920,
        SETTLED_AT + 60_000_000_000,
    );
    assert_eq!(meter.measured().map(|d| d.ppb() / 1_000), Some(-83));
    assert_eq!(meter.past_limit(), None);
}

/// After a restart, the earlier run's drift stays until the new run is as
/// long as it, or a minute: a 10 s run's jitter doesn't replace an hour's
/// estimate.
#[test]
fn a_new_run_takes_over_only_once_it_is_long_enough() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    settle_run(&mut meter, &timeline);
    read(
        &mut meter,
        &timeline,
        SETTLED + 57_605_760,
        SETTLED_AT + 3_600_000_000_000,
    );
    assert_eq!(meter.measured(), Some(ppm(100)));
    meter.restart();
    // Sample 58,000,000 plays at 3,625 s, where the new run starts; it
    // settles 10 s later.
    let from = 3_625_000_000_000;
    read(&mut meter, &timeline, 58_000_000, from);
    read(&mut meter, &timeline, 58_160_000, from + SETTLED_AT);
    // 10 s of the settled run, 1 ms out: not yet.
    let settled = from + SETTLED_AT;
    read(&mut meter, &timeline, 58_320_000, settled + 10_001_000_000);
    assert_eq!(meter.measured(), Some(ppm(100)));
    // A minute of it: it takes over.
    read(&mut meter, &timeline, 59_120_000, settled + 60_000_000_000);
    assert_eq!(meter.measured(), Some(Drift::ZERO));
}

/// A drift at the limit means bad stamps, not a device: the meter doesn't
/// correct by it, however far the mapping strays.
#[test]
fn no_retime_at_the_drift_limit() {
    let timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    settle_run(&mut meter, &timeline);
    // Twice the samples the time allows: clamped at 1000 ppm.
    assert_eq!(
        read(
            &mut meter,
            &timeline,
            SETTLED + 320_000,
            SETTLED_AT + 10_000_000_000
        ),
        Reading::Steady
    );
    assert_eq!(meter.measured(), Some(Drift(Drift::MAX_PPB)));
    assert_eq!(
        read(
            &mut meter,
            &timeline,
            SETTLED + 640_000,
            SETTLED_AT + 20_000_000_000
        ),
        Reading::Steady
    );
}

/// One simulated recording: its worst difference between the timeline and
/// the true time of a buffer, how many epochs it took, and the drift it
/// measured.
struct Simulated {
    worst: Duration,
    epochs: usize,
    measured: Option<Drift>,
}

/// Records `length` of a device off by `drift_ppb` (exact, not bounded),
/// a buffer every `buffer` samples, each stamped at its true capture time
/// plus `jitter(n)` nanoseconds, through a meter and a timeline it
/// retimes.
fn simulate(
    drift_ppb: i64,
    length: Duration,
    buffer: u64,
    jitter: impl Fn(u64) -> i64,
) -> Simulated {
    record(drift_ppb, length, buffer, |n, true_ns| {
        stamped(true_ns.saturating_add_signed(jitter(n)))
    })
}

/// [`simulate`], with buffer number `n`, truly captured at `true_ns`,
/// stamped `stamp(n, true_ns)`. No audio is lost, so a loss read fails the
/// test.
fn record(
    drift_ppb: i64,
    length: Duration,
    buffer: u64,
    stamp: impl Fn(u64, u64) -> Stamp,
) -> Simulated {
    let mut timeline = epoch_at(Drift::ZERO);
    let mut meter = DriftMeter::new();
    let true_hz = i128::from(SPEECH.hz()) * (1_000_000_000 + i128::from(drift_ppb));
    let mut worst = Duration::ZERO;
    let mut first = 0u64;
    while let Ok(true_ns) = u64::try_from(i128::from(first) * 1_000_000_000_000_000_000 / true_hz)
        && Duration::from_nanos(true_ns) < length
    {
        let stamp = stamp(first / buffer, true_ns);
        match meter.observe(timeline.current().unwrap(), s(first), stamp) {
            Reading::Steady => {}
            Reading::Retime(drift) => {
                timeline.retime(s(first), drift).unwrap();
            }
            Reading::Lost { hole } => panic!("no audio was lost, yet {hole:?} at {first}"),
        }
        let mapped = timeline.time_of(s(first)).unwrap().as_nanos();
        worst = worst.max(Duration::from_nanos(mapped.abs_diff(true_ns)));
        first += buffer;
    }
    Simulated {
        worst,
        epochs: timeline.epochs().len(),
        measured: meter.measured(),
    }
}

/// How far a stream's cycle times stood from its samples after it started,
/// measured on a USB microphone (`PipeWire` 1.6, 16 kHz from a 48 kHz
/// graph): (seconds in, milliseconds off), held after the last.
const MIC_SETTLING: [(f64, f64); 9] = [
    (0.0, 0.0),
    (0.1, 2.2),
    (0.5, 6.6),
    (1.0, 10.7),
    (2.0, 14.0),
    (3.0, 14.0),
    (5.0, 12.4),
    (10.0, 11.6),
    (20.0, 11.3),
];

/// The same for a USB DAC's monitor, which settled the other way.
const MONITOR_SETTLING: [(f64, f64); 8] = [
    (0.0, 0.0),
    (0.5, -3.2),
    (1.0, -7.7),
    (2.0, -11.7),
    (3.0, -11.3),
    (5.0, -11.0),
    (10.0, -10.8),
    (20.0, -10.6),
];

/// How far `shape` puts the stamps off at `ns` nanoseconds in, in
/// nanoseconds: straight lines between its points.
fn settling(shape: &[(f64, f64)], ns: u64) -> i64 {
    #[expect(clippy::cast_precision_loss, reason = "test arithmetic in seconds")]
    let secs = ns as f64 / 1e9;
    let ms = shape
        .windows(2)
        .find(|w| secs < w[1].0)
        .map_or(shape[shape.len() - 1].1, |w| {
            w[0].1 + (w[1].1 - w[0].1) * (secs - w[0].0) / (w[1].0 - w[0].0)
        });
    #[expect(clippy::cast_possible_truncation, reason = "a few milliseconds")]
    let off = (ms * 1e6).round() as i64;
    off
}

/// A stream settling as the hardware measured, 14 ms off within 2 s,
/// reads as neither a loss nor drift: the timeline keeps one epoch, and
/// the drift measured is the device's.
#[test]
fn a_stream_settling_as_measured_is_neither_lost_nor_drifting() {
    for shape in [&MIC_SETTLING[..], &MONITOR_SETTLING[..]] {
        let run = record(0, Duration::from_secs(600), 320, |_, true_ns| {
            stamped(true_ns.saturating_add_signed(settling(shape, true_ns)))
        });
        assert_eq!(run.epochs, 1, "{shape:?}");
        let measured = run.measured.map(Drift::ppb);
        assert!(
            measured.is_some_and(|ppb| ppb.abs() < 2_000),
            "{measured:?}"
        ); // check-bound
    }
}

/// A device 100 ppm off is still corrected while its stream settles as
/// measured, in one epoch to slew and one to land.
#[test]
fn a_drifting_stream_settling_as_measured_is_corrected() {
    for drift in [100_000, -100_000] {
        let run = record(drift, Duration::from_secs(600), 320, |_, true_ns| {
            stamped(true_ns.saturating_add_signed(settling(&MIC_SETTLING, true_ns)))
        });
        assert_eq!(run.epochs, 3, "{drift}");
        assert!(run.worst < WITHIN, "{drift}: {:?}", run.worst);
        let measured = run
            .measured
            .map(|d| d.ppb() - i32::try_from(drift).unwrap());
        assert!(
            measured.is_some_and(|ppb| ppb.abs() < 2_000),
            "{measured:?}"
        ); // check-bound
    }
}

/// The delays measured on the microphone at each quantum, in
/// microseconds: 1.5 quanta at 48 kHz.
const QUANTUM_DELAYS: [u64; 5] = [2_333, 4_000, 8_000, 32_000, 53_333];

/// A step in the stream's delay, as a quantum change makes, moves its
/// stamps but not its cycles: from any measured delay to any other, half a
/// minute in, it reads as neither a loss nor drift.
#[test]
fn a_step_in_the_delay_is_neither_lost_nor_drifting() {
    for before in QUANTUM_DELAYS {
        for after in QUANTUM_DELAYS {
            let delay = |true_ns| {
                let us = if true_ns < 30_000_000_000 {
                    before
                } else {
                    after
                };
                Duration::from_micros(us)
            };
            // The cycle comes a steady 60 ms after the capture.
            let run = record(0, Duration::from_secs(120), 320, |_, true_ns| Stamp {
                at: t(true_ns + 60_000_000 - u64::try_from(delay(true_ns).as_nanos()).unwrap()),
                delay: delay(true_ns),
            });
            assert_eq!(run.epochs, 1, "{before} µs to {after} µs");
        }
    }
}

/// Three hours at +100 and −100 ppm map to session time within 20 ms, in
/// a few epochs.
#[test]
fn three_hours_at_100_ppm_stay_within_20_ms() {
    let three_hours = Duration::from_hours(3);
    for drift in [100_000, -100_000] {
        let run = simulate(drift, three_hours, 320, |_| 0);
        assert!(run.worst < WITHIN, "{drift}: {:?}", run.worst);
        // One epoch to slew, one to land.
        assert_eq!(run.epochs, 3, "{drift}");
    }
}

/// Without correcting, the same three hours would be a second out: the
/// test above measures the correction, not a device that barely drifts.
#[test]
fn three_hours_at_100_ppm_uncorrected_are_a_second_out() {
    let timeline = epoch_at(Drift::ZERO);
    // 16,001.6 samples a second for three hours.
    let samples = 3 * 3_600 * 160_016 / 10;
    let mapped = timeline.time_of(s(samples)).unwrap();
    assert_eq!(mapped, t(10_801_080_000_000));
}

/// A pseudo-random jitter of up to `max` nanoseconds either way, the same
/// for the same buffer.
fn jitter(max: i64) -> impl Fn(u64) -> i64 {
    move |n| {
        let mixed = n.wrapping_mul(6_364_136_223_846_793_005).rotate_left(29);
        i64::try_from(mixed % (2 * max.unsigned_abs() + 1)).unwrap_or(0) - max
    }
}

/// A millisecond of timestamp jitter either way still stays within 20 ms
/// over three hours.
#[test]
fn jittered_timestamps_stay_within_20_ms() {
    let three_hours = Duration::from_hours(3);
    for drift in [100_000, -100_000, 0] {
        let run = simulate(drift, three_hours, 320, jitter(1_000_000));
        assert!(run.worst < WITHIN, "{drift}: {:?}", run.worst);
        assert!(run.epochs < 20, "{drift}: {} epochs", run.epochs);
    }
}

fn any_rate() -> impl Strategy<Value = SampleRate> {
    prop_oneof![
        4 => Just(SPEECH),
        1 => Just(SampleRate::new(44_100).unwrap()),
        1 => (1..=SampleRate::MAX_HZ).prop_map(|hz| SampleRate::new(hz).unwrap()),
    ]
}

fn any_drift() -> impl Strategy<Value = Drift> {
    prop_oneof![
        Just(Drift::ZERO),
        Just(Drift(Drift::MAX_PPB)),
        Just(Drift(-Drift::MAX_PPB)),
        (-Drift::MAX_PPB..=Drift::MAX_PPB).prop_map(Drift),
    ]
}

proptest! {
    /// With no drift, durations and counts are the nominal rate's exactly.
    #[test]
    fn no_drift_is_the_nominal_rate(n in any::<u64>(), nanos in any::<u64>(), rate in any_rate()) {
        let count = SampleCount::new(n);
        prop_assert_eq!(Drift::ZERO.duration_of(count, rate), count.duration_at(rate));
        let elapsed = Duration::from_nanos(nanos);
        prop_assert_eq!(
            Drift::ZERO.count_within(elapsed, rate),
            SampleCount::started_within(elapsed, rate)
        );
    }

    /// Sample → time → sample is exact, and consecutive samples are at
    /// least a nanosecond apart, at every rate and drift.
    #[test]
    fn counts_round_trip_at_every_drift(
        n in 0..u64::MAX / 2_000_000,
        rate in any_rate(),
        drift in any_drift(),
    ) {
        let count = SampleCount::new(n);
        // Only counts whose next sample still has a session time.
        let next = drift.duration_of(SampleCount::new(n + 1), rate);
        prop_assume!(next.is_some());
        let d = drift.duration_of(count, rate).unwrap();
        prop_assert_eq!(drift.count_within(d, rate), Some(count));
        prop_assert!(next.unwrap() > d);
    }

    /// A recording at any steady drift within 800 ppm, with up to a
    /// millisecond of jitter, maps within 20 ms over an hour.
    #[test]
    fn any_steady_drift_stays_within_20_ms(
        drift in -800_000i64..=800_000,
        max_jitter in prop_oneof![Just(0i64), 0..=1_000_000i64],
    ) {
        let run = simulate(drift, Duration::from_secs(3_600), 1_600, jitter(max_jitter));
        prop_assert!(run.worst < WITHIN, "{:?}", run.worst);
    }
}
