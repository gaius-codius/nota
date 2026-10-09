//! Property tests for the protocol decoder, which reads untrusted bytes from
//! the other process.
//!
//! The contract, checked on every input:
//! - encoding then decoding gives back the same frames, and a stream of
//!   frames reads back in order and then ends cleanly;
//! - the reader never panics, whatever the bytes;
//! - every frame it accepts is canonical: re-encoding it gives exactly the
//!   bytes it was read from, so no two byte strings mean the same message;
//! - a stream cut short gives the whole frames before the cut, then
//!   [`ReadError::Truncated`] (or a clean end if the cut is on a boundary).

use proptest::prelude::*;

use super::*;

fn any_rate() -> impl Strategy<Value = SampleRate> {
    prop_oneof![
        Just(SampleRate::SPEECH),
        (1..=SampleRate::MAX_HZ).prop_map(|hz| SampleRate::new(hz).unwrap()),
    ]
}

fn any_chunk() -> impl Strategy<Value = AudioChunk> {
    (
        any::<u32>(),
        any::<u64>(),
        any_rate(),
        prop::collection::vec(any::<i16>(), 0..64),
    )
        .prop_filter_map("end overflows", |(track, first, rate, samples)| {
            AudioChunk::new(TrackId::new(track), SampleIndex::new(first), rate, samples)
        })
}

/// A transcript with up to 6 words, cut at sorted points inside its range
/// (so some touch, and some are instants).
fn any_transcript() -> impl Strategy<Value = Transcript> {
    (
        any::<u32>(),
        any::<u64>(),
        any::<u64>(),
        ".{0,40}",
        prop::collection::vec((any::<u64>(), any::<u64>(), "\\PC{1,8}"), 0..6),
    )
        .prop_filter("a transcript covers some audio", |(_, a, b, _, _)| a != b)
        .prop_map(|(track, a, b, text, words)| {
            let (lo, hi) = (a.min(b), a.max(b));
            let span = u128::from(hi - lo) + 1;
            let at = |x: u64| lo + u64::try_from(u128::from(x) % span).unwrap();
            let mut points: Vec<u64> = words.iter().flat_map(|&(x, y, _)| [at(x), at(y)]).collect();
            points.sort_unstable();
            let words = points
                .chunks(2)
                .zip(words)
                .map(|(pair, (_, _, word))| {
                    let range =
                        SampleRange::new(SampleIndex::new(pair[0]), SampleIndex::new(pair[1]))
                            .unwrap();
                    HeardWord::new(word, range).unwrap()
                })
                .collect();
            let range = SampleRange::new(SampleIndex::new(lo), SampleIndex::new(hi)).unwrap();
            Transcript::new(TrackId::new(track), range, text)
                .unwrap()
                .with_words(words)
                .unwrap()
        })
}

fn to_engine() -> impl Strategy<Value = Frame<ToEngine>> {
    prop_oneof![
        any::<u16>().prop_map(|v| Frame::Hello(ProtocolVersion::new(v))),
        any_chunk().prop_map(|chunk| Frame::Message(ToEngine::Audio(chunk))),
        any::<u32>().prop_map(|track| Frame::Message(ToEngine::Flush {
            track: TrackId::new(track)
        })),
    ]
}

fn from_engine() -> impl Strategy<Value = Frame<FromEngine>> {
    prop_oneof![
        any::<u16>().prop_map(|v| Frame::Hello(ProtocolVersion::new(v))),
        any_transcript().prop_map(|t| Frame::Message(FromEngine::Transcript(t))),
        (any::<u32>(), any::<u64>()).prop_map(|(track, up_to)| Frame::Message(
            FromEngine::Confirmed {
                track: TrackId::new(track),
                up_to: SampleIndex::new(up_to),
            }
        )),
    ]
}

/// Encodes `frames` into one stream, with where each frame ends.
fn stream<M: WireMessage>(frames: &[Frame<M>]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::new();
    let mut ends = Vec::new();
    for frame in frames {
        bytes.extend(encode(frame).unwrap());
        ends.push(bytes.len());
    }
    (bytes, ends)
}

/// The frames read, the bytes each one was read from, and how the read
/// ended.
type ReadAll<M> = (Vec<Frame<M>>, Vec<Vec<u8>>, Result<(), ReadError>);

/// Reads `bytes` to the end or the first error.
fn read_all<M: WireMessage>(bytes: &[u8]) -> ReadAll<M> {
    let mut input = bytes;
    let mut frames = Vec::new();
    let mut raw = Vec::new();
    loop {
        let before = input.len();
        let mut reader = FrameReader::new(&mut input);
        match reader.read_frame::<M>() {
            Ok(Some(frame)) => {
                let used = before - input.len();
                let start = bytes.len() - before;
                raw.push(bytes[start..start + used].to_vec());
                frames.push(frame);
            }
            Ok(None) => return (frames, raw, Ok(())),
            Err(err) => return (frames, raw, Err(err)),
        }
    }
}

/// Every accepted frame re-encodes to exactly the bytes it came from.
fn assert_canonical<M: WireMessage + fmt::Debug>(frames: &[Frame<M>], raw: &[Vec<u8>]) {
    for (frame, bytes) in frames.iter().zip(raw) {
        assert_eq!(&encode(frame).unwrap(), bytes, "{frame:?}");
    }
}

proptest! {
    #[test]
    fn to_engine_round_trips(frames in prop::collection::vec(to_engine(), 0..8)) {
        let (bytes, _) = stream(&frames);
        let (read, raw, end) = read_all::<ToEngine>(&bytes);
        prop_assert!(end.is_ok());
        prop_assert_eq!(&read, &frames);
        assert_canonical(&read, &raw);
    }

    #[test]
    fn from_engine_round_trips(frames in prop::collection::vec(from_engine(), 0..8)) {
        let (bytes, _) = stream(&frames);
        let (read, raw, end) = read_all::<FromEngine>(&bytes);
        prop_assert!(end.is_ok());
        prop_assert_eq!(&read, &frames);
        assert_canonical(&read, &raw);
    }

    /// Plain garbage: no panic, and whatever is accepted is canonical.
    #[test]
    fn garbage_is_never_misread(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let (read, raw, _) = read_all::<FromEngine>(&bytes);
        assert_canonical(&read, &raw);
        let (read, raw, _) = read_all::<ToEngine>(&bytes);
        assert_canonical(&read, &raw);
    }

    /// Garbage behind a plausible prefix, so the decoder's field checks are
    /// reached rather than just the length check.
    #[test]
    fn garbage_bodies_are_never_misread(
        tag in prop_oneof![Just(0x00_u8), Just(0x01), Just(0x02), Just(0x81), Just(0x82), any::<u8>()],
        body in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut bytes = u32::try_from(body.len() + 1).unwrap().to_le_bytes().to_vec();
        bytes.push(tag);
        bytes.extend(&body);
        let (read, raw, _) = read_all::<FromEngine>(&bytes);
        assert_canonical(&read, &raw);
        let (read, raw, _) = read_all::<ToEngine>(&bytes);
        assert_canonical(&read, &raw);
    }

    /// One flipped bit anywhere: no panic, frames before it are intact, and
    /// whatever is accepted is canonical.
    #[test]
    fn a_flipped_bit_is_never_misread(
        frames in prop::collection::vec(from_engine(), 1..6),
        at in any::<prop::sample::Index>(),
        bit in 0_u8..8,
    ) {
        let (mut bytes, ends) = stream(&frames);
        let at = at.index(bytes.len());
        bytes[at] ^= 1 << bit;
        let (read, raw, _) = read_all::<FromEngine>(&bytes);
        assert_canonical(&read, &raw);
        let intact = ends.iter().filter(|&&end| end <= at).count();
        prop_assert!(read.len() >= intact);
        prop_assert_eq!(&read[..intact], &frames[..intact]);
    }

    #[test]
    fn a_cut_stream_gives_the_whole_frames_then_truncated(
        frames in prop::collection::vec(to_engine(), 1..6),
        cut in any::<prop::sample::Index>(),
    ) {
        let (bytes, ends) = stream(&frames);
        let cut = cut.index(bytes.len() + 1);
        let (read, _, end) = read_all::<ToEngine>(&bytes[..cut]);
        let whole = ends.iter().filter(|&&end| end <= cut).count();
        prop_assert_eq!(&read, &frames[..whole].to_vec());
        let on_boundary = cut == 0 || ends.contains(&cut);
        if on_boundary {
            prop_assert!(end.is_ok());
        } else {
            prop_assert!(matches!(end, Err(ReadError::Truncated)), "{end:?}");
        }
    }

    /// The reader copes with a stream that hands over one byte at a time,
    /// as a pipe may.
    #[test]
    fn short_reads_are_reassembled(frames in prop::collection::vec(from_engine(), 0..6)) {
        let (bytes, _) = stream(&frames);
        let mut reader = FrameReader::new(OneByte(&bytes));
        let mut read = Vec::new();
        while let Some(frame) = reader.read_frame::<FromEngine>().unwrap() {
            read.push(frame);
        }
        prop_assert_eq!(read, frames);
    }
}

/// A reader that returns at most one byte per call, and an `Interrupted`
/// error before each byte.
struct OneByte<'a>(&'a [u8]);

impl Read for OneByte<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        thread_local!(static TICK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
        if TICK.with(|t| t.replace(!t.get())) {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        match (self.0.split_first(), buf.first_mut()) {
            (Some((&byte, rest)), Some(slot)) => {
                *slot = byte;
                self.0 = rest;
                Ok(1)
            }
            _ => Ok(0),
        }
    }
}
