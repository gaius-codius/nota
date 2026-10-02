//! Coverage-guided fuzz target for the journal reader.
//!
//! Feeds arbitrary bytes to `read_journal` and asserts the reader's contract
//! on whatever it returns. A violated assertion panics, which AFL records as
//! a crash.

use std::collections::BTreeMap;

use nota_recorder::journal::format::{FRAME_HEADER_LEN, HEADER_LEN, MAX_FRAME_SAMPLES};
use nota_recorder::journal::{ReadEnd, read_journal};

fn check(data: &[u8]) {
    let read = read_journal(data);
    assert!(read.valid_len() <= data.len(), "valid_len past the input");

    if read.header().is_none() {
        assert!(read.frames().is_empty(), "frames without a header");
        assert_eq!(read.valid_len(), 0, "valid_len without a header");
        return;
    }

    let mut expected_len = HEADER_LEN;
    let mut ends = BTreeMap::new();
    for (index, frame) in read.frames().iter().enumerate() {
        assert_eq!(frame.seq(), index as u64, "sequence numbers not 0,1,2,...");
        let samples = frame.samples().len();
        assert_eq!(
            samples as u64,
            frame.range().len().get(),
            "sample count differs from range length"
        );
        assert!(
            (1..=MAX_FRAME_SAMPLES as usize).contains(&samples),
            "frame length out of bounds"
        );
        if let Some(previous_end) = ends.insert(frame.track(), frame.range().end()) {
            assert_eq!(
                frame.range().start(),
                previous_end,
                "track range is not contiguous"
            );
        }
        // The frame's bytes in the input carry a CRC that matches them:
        // checked here independently of the reader.
        let bytes = &data[expected_len..expected_len + FRAME_HEADER_LEN + 2 * samples];
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&bytes[..28]);
        hasher.update(&bytes[FRAME_HEADER_LEN..]);
        let stored = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
        assert_eq!(hasher.finalize(), stored, "returned a frame whose CRC fails");
        let decoded: Vec<i16> = bytes[FRAME_HEADER_LEN..]
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(decoded, frame.samples(), "samples differ from the input");
        expected_len += FRAME_HEADER_LEN + 2 * samples;
    }
    assert_eq!(read.valid_len(), expected_len, "valid_len does not add up");

    if read.end() == ReadEnd::Complete {
        assert_eq!(read.valid_len(), data.len(), "Complete before end of input");
    }

    let again = read_journal(&data[..read.valid_len()]);
    assert_eq!(
        again.frames(),
        read.frames(),
        "re-reading the valid prefix differs"
    );
    assert_eq!(
        again.end(),
        ReadEnd::Complete,
        "valid prefix is not Complete"
    );
}

fn main() {
    afl::fuzz!(|data: &[u8]| {
        check(data);
    });
}
