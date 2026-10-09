//! Coverage-guided fuzz target for the engine protocol's reader.
//!
//! Reads arbitrary bytes as a stream of frames in each direction and
//! asserts the reader's contract on every frame it accepts. A violated
//! assertion panics, which AFL records as a crash.

use nota_core::messages::{FromEngine, ToEngine};
use nota_core::protocol::{Frame, FrameReader, WireMessage, encode};

/// Reads `data` as frames of `M` until the end or the first error. Each
/// frame accepted re-encodes to exactly the bytes it was read from.
fn read_all<M: WireMessage + std::fmt::Debug>(data: &[u8], check: impl Fn(&Frame<M>)) {
    let mut input = data;
    loop {
        let before = input;
        let mut reader = FrameReader::new(&mut input);
        match reader.read_frame::<M>() {
            Ok(Some(frame)) => {
                let used = before.len() - input.len();
                let encoded = encode(&frame).unwrap_or_default();
                assert_eq!(encoded, before[..used], "{frame:?} isn't canonical");
                check(&frame);
            }
            Ok(None) | Err(_) => return,
        }
    }
}

/// A transcript's words lie inside its range, in order, and none is
/// without text.
fn check_from_engine(frame: &Frame<FromEngine>) {
    let Frame::Message(FromEngine::Transcript(t)) = frame else {
        return;
    };
    assert!(!t.range().is_empty(), "a transcript over no samples");
    let mut from = t.range().start();
    for word in t.words() {
        assert!(!word.text().is_empty(), "a word without text");
        assert!(word.range().start() >= from, "a word out of order");
        assert!(word.range().end() <= t.range().end(), "a word past its end");
        from = word.range().end();
    }
}

fn main() {
    afl::fuzz!(|data: &[u8]| {
        read_all::<FromEngine>(data, check_from_engine);
        read_all::<ToEngine>(data, |_| {});
    });
}
