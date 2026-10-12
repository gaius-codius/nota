use super::*;

/// Nanoseconds on `CLOCK_MONOTONIC`.
fn at(nanos: u64) -> Duration {
    Duration::from_nanos(nanos)
}

fn stamp(cycle: u64, capture: u64) -> Delivery {
    Delivery {
        cycle: at(cycle),
        capture: at(capture),
    }
}

/// What a stream sent: each buffer's samples and timing, in order.
#[derive(Debug, Default)]
struct Sent(Vec<(Vec<i16>, Timed)>);

impl Sent {
    fn send(&mut self) -> impl FnMut(&[i16], Timed) + '_ {
        |samples, timed| self.0.push((samples.to_vec(), timed))
    }

    fn timings(&self) -> Vec<Timed> {
        self.0.iter().map(|(_, timed)| *timed).collect()
    }
}

/// Feeds `settling` a buffer of `len` samples, each its index from
/// `first`, stamped `stamp`.
fn feed(
    settling: &mut Settling,
    sent: &mut Sent,
    first: usize,
    len: usize,
    stamp: Option<Delivery>,
) {
    let samples: Vec<i16> = (first..first + len)
        .map(|i| i16::try_from(i % 30_000).unwrap())
        .collect();
    settling.buffer(&samples, stamp, &mut sent.send());
}

/// A stream's first eight buffers as `PipeWire` delivered them in a real
/// capture of a sink's monitor at 16 kHz: (length, cycle, cpal's capture
/// stamp). The first five carry no delay; the sixth, the first at a new
/// quantum, one shorter than itself; the seventh one quantum (42.667 ms)
/// for its 683 samples (42.688 ms), as every buffer after it does.
const FIRST_BUFFERS: [(usize, u64, u64); 8] = [
    (314, 301_781_183_538_089, 301_781_183_538_089),
    (341, 301_781_204_871_422, 301_781_204_871_422),
    (342, 301_781_226_204_755, 301_781_226_204_755),
    (341, 301_781_247_538_088, 301_781_247_538_088),
    (341, 301_781_268_871_421, 301_781_268_871_421),
    (683, 301_781_290_204_754, 301_781_268_871_421),
    (683, 301_781_332_871_420, 301_781_290_204_754),
    (682, 301_781_375_538_086, 301_781_332_871_420),
];

/// Feeds the real capture's first `count` buffers; returns the samples fed.
fn feed_first(settling: &mut Settling, sent: &mut Sent, count: usize) -> usize {
    FIRST_BUFFERS
        .iter()
        .take(count)
        .fold(0, |first, &(len, cycle, capture)| {
            feed(settling, sent, first, len, Some(stamp(cycle, capture)));
            first + len
        })
}

/// The first buffers are held until the seventh settles, then sent dated
/// back from its stamp: they run on to it, one after another, with every
/// sample, and the first starts a quantum before the first buffer, taken
/// as ending at its cycle, would.
#[test]
fn the_first_buffers_are_dated_back_from_the_first_settled_stamp() {
    let mut settling = Settling::new(SampleRate::SPEECH);
    let mut sent = Sent::default();
    let fed = feed_first(&mut settling, &mut sent, 6);
    assert!(sent.0.is_empty(), "sent before a stamp settled: {sent:?}");
    let (len, cycle, capture) = FIRST_BUFFERS[6];
    feed(
        &mut settling,
        &mut sent,
        fed,
        len,
        Some(stamp(cycle, capture)),
    );

    // 2,362 samples held, 147.625 ms, back from the seventh's stamp.
    let start = capture - 147_625_000;
    assert_eq!(sent.0.len(), 7);
    assert_eq!(
        sent.0[0].1,
        Some((at(start), at(FIRST_BUFFERS[0].1 - start)))
    );
    let ended_at_cycle = FIRST_BUFFERS[0].1 - 19_625_000;
    assert_eq!(ended_at_cycle - start, 21_333_335);
    let mut end = at(start);
    for ((samples, timed), &(len, cycle, _)) in sent.0.iter().zip(&FIRST_BUFFERS) {
        let (began, delay) = timed.unwrap();
        assert_eq!(samples.len(), len);
        assert_eq!(began + delay, at(cycle), "a buffer keeps its cycle");
        assert!(
            began.abs_diff(end) <= Duration::from_nanos(1),
            "{began:?} after {end:?}"
        );
        end = began + length(len, SampleRate::SPEECH).unwrap();
    }
    assert_eq!(sent.0[6].1, Some((at(capture), at(cycle - capture))));
    let samples: Vec<i16> = sent.0.iter().flat_map(|(s, _)| s.clone()).collect();
    assert_eq!(
        samples,
        (0..fed + len)
            .map(|i| i16::try_from(i).unwrap())
            .collect::<Vec<_>>()
    );

    // From there each buffer goes as it comes, with its stamp.
    let (len, cycle, capture) = FIRST_BUFFERS[7];
    feed(
        &mut settling,
        &mut sent,
        0,
        len,
        Some(stamp(cycle, capture)),
    );
    assert_eq!(sent.0.len(), 8);
    assert_eq!(sent.0[7].1, Some((at(capture), at(cycle - capture))));
}

/// A settled stamp that would put a held buffer's audio after the cycle
/// that delivered it dates the buffers back from that cycle instead.
#[test]
fn no_held_audio_is_dated_after_its_cycle() {
    let mut settling = Settling::new(SampleRate::SPEECH);
    let mut sent = Sent::default();
    // 320 samples, 20 ms, delivered at 100 ms; then a settled buffer
    // stamped 125 ms, which would put the first from 105 ms to 125 ms.
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(100_000_000, 100_000_000)),
    );
    feed(
        &mut settling,
        &mut sent,
        320,
        320,
        Some(stamp(145_000_000, 125_000_000)),
    );
    assert_eq!(
        sent.timings(),
        [
            Some((at(80_000_000), at(20_000_000))),
            Some((at(125_000_000), at(20_000_000)))
        ]
    );
}

/// A stream that never has a delay is held for at most 500 ms, then sent
/// as each buffer's cycle places it, and from then on isn't held.
#[test]
fn a_stream_with_no_delay_is_held_no_longer_than_the_limit() {
    let mut settling = Settling::new(SampleRate::SPEECH);
    let mut sent = Sent::default();
    let cycle = |n: u64| 1_000_000_000 + n * 20_000_000;
    // 25 buffers of 20 ms make 500 ms.
    for n in 0..25 {
        feed(
            &mut settling,
            &mut sent,
            0,
            320,
            Some(stamp(cycle(n), cycle(n))),
        );
    }
    assert!(sent.0.is_empty());
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(cycle(25), cycle(25))),
    );
    let ended = |n| Some((at(cycle(n) - 20_000_000), at(20_000_000)));
    assert_eq!(sent.timings(), (0..26).map(ended).collect::<Vec<_>>());
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(cycle(26), cycle(26))),
    );
    assert_eq!(sent.0.len(), 27);
    assert_eq!(sent.0[26].1, ended(26));
}

/// A stream of short buffers holds no more of them than it made room for,
/// so holding never allocates: the next is sent with all it held.
#[test]
fn a_hold_sends_what_it_holds_once_its_room_is_full() {
    let mut settling = Settling::new(SampleRate::SPEECH);
    let mut sent = Sent::default();
    // 43 samples, about 2.7 ms: 64 make well under 500 ms.
    let cycle = |n: u64| 1_000_000_000 + n * 2_687_500;
    for n in 0..64 {
        feed(
            &mut settling,
            &mut sent,
            0,
            43,
            Some(stamp(cycle(n), cycle(n))),
        );
    }
    assert!(sent.0.is_empty());
    feed(
        &mut settling,
        &mut sent,
        0,
        43,
        Some(stamp(cycle(64), cycle(64))),
    );
    assert_eq!(sent.0.len(), 65);
}

/// A stream that settled and then loses its delay for longer than the
/// limit sends what it held, and then isn't held again until it has had a
/// delay once more.
#[test]
fn a_delay_lost_past_the_limit_is_not_held_again() {
    let mut settling = Settling::new(SampleRate::SPEECH);
    let mut sent = Sent::default();
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(100_000_000, 80_000_000)),
    );
    let cycle = |n: u64| 1_000_000_000 + n * 20_000_000;
    for n in 0..26 {
        feed(
            &mut settling,
            &mut sent,
            0,
            320,
            Some(stamp(cycle(n), cycle(n))),
        );
    }
    assert_eq!(sent.0.len(), 27);
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(cycle(26), cycle(26))),
    );
    assert_eq!(sent.0.len(), 28, "not held again");
}

/// A stream that ends while buffers are held has them sent, each as its
/// own stamp places it, every sample in order.
#[test]
fn what_is_held_is_sent_as_the_stream_ends() {
    let mut settling = Settling::new(SampleRate::SPEECH);
    let mut sent = Sent::default();
    let fed = feed_first(&mut settling, &mut sent, 6);
    assert!(sent.0.is_empty());
    settling.release(&mut sent.send());
    let ends = FIRST_BUFFERS
        .iter()
        .take(6)
        .map(|&(len, cycle, capture)| stamped(stamp(cycle, capture), len, SampleRate::SPEECH));
    assert_eq!(sent.timings(), ends.collect::<Vec<_>>());
    let samples: Vec<i16> = sent.0.iter().flat_map(|(s, _)| s.clone()).collect();
    assert_eq!(
        samples,
        (0..fed)
            .map(|i| i16::try_from(i).unwrap())
            .collect::<Vec<_>>()
    );
    // Nothing is held any more.
    settling.release(&mut sent.send());
    assert_eq!(sent.0.len(), 6);
}

/// A buffer cpal couldn't stamp sends what's held first, then goes
/// unstamped.
#[test]
fn an_unstamped_buffer_sends_what_is_held_first() {
    let mut settling = Settling::new(SampleRate::SPEECH);
    let mut sent = Sent::default();
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(100_000_000, 100_000_000)),
    );
    feed(&mut settling, &mut sent, 320, 320, None);
    assert_eq!(
        sent.timings(),
        [Some((at(80_000_000), at(20_000_000))), None]
    );
    assert_eq!(sent.0[1].0[0], 320);
}

/// Buffers with no delay after the stream had one, as `PipeWire` measures
/// it again, are held and dated back from the next settled stamp too.
#[test]
fn a_delay_lost_again_is_settled_again() {
    let mut settling = Settling::new(SampleRate::SPEECH);
    let mut sent = Sent::default();
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(100_000_000, 80_000_000)),
    );
    assert_eq!(sent.timings(), [Some((at(80_000_000), at(20_000_000)))]);
    // A new device: two buffers with no delay, then one settled.
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(500_000_000, 500_000_000)),
    );
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(520_000_000, 520_000_000)),
    );
    assert_eq!(sent.0.len(), 1);
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(560_000_000, 500_000_000)),
    );
    assert_eq!(
        sent.timings()[1..],
        [
            Some((at(460_000_000), at(40_000_000))),
            Some((at(480_000_000), at(40_000_000))),
            Some((at(500_000_000), at(60_000_000)))
        ]
    );
}

/// A stream that gave up holding with no delay isn't held again while its
/// delay stays none: it never had one to lose. Once it has one, losing it
/// holds its buffers again.
#[test]
fn a_stream_that_gave_up_holding_is_held_again_only_after_a_delay() {
    let mut settling = Settling::new(SampleRate::SPEECH);
    let mut sent = Sent::default();
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(100_000_000, 100_000_000)),
    );
    settling.release(&mut sent.send());
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(120_000_000, 120_000_000)),
    );
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(140_000_000, 140_000_000)),
    );
    assert_eq!(sent.0.len(), 3);
    // A delay shorter than its buffer: sent, but not settled.
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(160_000_000, 150_000_000)),
    );
    assert_eq!(sent.0.len(), 4);
    feed(
        &mut settling,
        &mut sent,
        0,
        320,
        Some(stamp(180_000_000, 180_000_000)),
    );
    assert_eq!(sent.0.len(), 4, "a delay lost holds again");
}

/// The first buffer, stamped with its cycle's time, is placed as captured
/// its length before, so none of its audio is after the cycle that
/// delivered it.
#[test]
fn a_buffer_with_no_delay_ends_at_its_cycle() {
    let (len, cycle, capture) = FIRST_BUFFERS[0];
    // 314 samples at 16 kHz: 19.625 ms.
    assert_eq!(
        stamped(stamp(cycle, capture), len, SampleRate::SPEECH),
        Some((at(cycle - 19_625_000), at(19_625_000)))
    );
}

/// A buffer with a delay, `PipeWire`'s or cpal's own (the callback's time
/// less the buffer's length, before the driver starts), keeps its stamp,
/// shorter than the buffer or not.
#[test]
fn a_buffer_with_a_delay_keeps_its_stamp() {
    let (len, cycle, capture) = FIRST_BUFFERS[5];
    assert_eq!(
        stamped(stamp(cycle, capture), len, SampleRate::SPEECH),
        Some((at(capture), at(cycle - capture)))
    );
    assert_eq!(
        stamped(stamp(1_000_000_000, 980_000_000), 320, SampleRate::SPEECH),
        Some((at(980_000_000), Duration::from_millis(20)))
    );
}

/// A cycle that comes before its buffer's length can't be placed, and the
/// buffer is sent unstamped.
#[test]
fn a_cycle_earlier_than_its_buffer_is_unstamped() {
    assert_eq!(
        stamped(stamp(10_000_000, 10_000_000), 320, SampleRate::SPEECH),
        None
    );
}
