//! Coverage-guided fuzz target for the journal reader.
//!
//! Feeds arbitrary bytes to `read_journal` and asserts the reader's contract
//! on whatever it returns. A violated assertion panics, which AFL records as
//! a crash.

use nota_recorder::journal::format::{
    FRAME_HEADER_LEN, HEADER_LEN, MAX_FRAME_SAMPLES, encode_header,
};
use nota_recorder::journal::{Invalid, JournalHeader, ReadEnd, read_journal};

/// `N` bytes of `data` from `at`.
fn field<const N: usize>(data: &[u8], at: usize) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&data[at..at + N]);
    out
}

/// The header's fields are the input's bytes, checked independently of the
/// reader: magic, version 4 (or 3, with no drift, or 2, with no anchor), a
/// CRC that matches, and the id, track, epoch, rate and anchor where the
/// layout puts them. Returns how many bytes the header took.
fn check_header(data: &[u8], header: JournalHeader) -> usize {
    let anchor = header.anchor();
    assert!(data.len() >= 10, "a header from too few bytes");
    let version = u16::from_le_bytes(field(data, 8));
    assert!((2..=4).contains(&version), "header version {version}");
    let len = match version {
        4 => HEADER_LEN,
        3 => 50,
        _ => 34,
    };
    assert_eq!(anchor.is_some(), version > 2, "an anchor only from 3 on");
    assert!(data.len() >= len, "a header from too few bytes");
    assert_eq!(&data[..8], b"NOTAJRNL", "header magic");
    assert_eq!(
        u32::from_le_bytes(field(data, len - 4)),
        crc32fast::hash(&data[..len - 4]),
        "returned a header whose CRC fails"
    );
    assert_eq!(
        header.rate().hz(),
        u32::from_le_bytes(field(data, 10)),
        "rate"
    );
    assert_eq!(header.id().get(), u64::from_le_bytes(field(data, 14)), "id");
    assert_eq!(
        header.track().get(),
        u32::from_le_bytes(field(data, 22)),
        "track"
    );
    assert_eq!(
        header.epoch().get(),
        u32::from_le_bytes(field(data, 26)),
        "epoch"
    );
    if let Some(anchor) = anchor {
        assert_eq!(anchor.id, header.epoch(), "anchor's epoch");
        assert_eq!(anchor.rate, header.rate(), "anchor's rate");
        assert_eq!(
            anchor.first_sample.get(),
            u64::from_le_bytes(field(data, 30)),
            "anchor's first sample"
        );
        assert_eq!(
            anchor.start.as_nanos(),
            u64::from_le_bytes(field(data, 38)),
            "anchor's start"
        );
        let drift = if version == 4 {
            i32::from_le_bytes(field(data, 46))
        } else {
            0
        };
        assert_eq!(anchor.drift.ppb(), drift, "anchor's drift");
        assert!(drift.unsigned_abs() <= 1_000_000, "a drift past the limit");
    }
    let encoded = encode_header(header);
    if version == 3 {
        // Written again, it's version 4, with no drift.
        assert_eq!(encoded[..8], data[..8], "header re-encodes");
        assert_eq!(encoded[10..46], data[10..46], "header re-encodes");
        assert_eq!(encoded[46..50], [0; 4], "header re-encodes");
    } else {
        assert_eq!(encoded, data[..len], "header re-encodes");
    }
    len
}

fn check(data: &[u8]) {
    let read = read_journal(data);
    assert!(read.valid_len() <= data.len(), "valid_len past the input");

    let Some(header) = read.header() else {
        assert!(read.frames().is_empty(), "frames without a header");
        assert_eq!(read.valid_len(), 0, "valid_len without a header");
        return;
    };

    let mut expected_len = check_header(data, header);
    let mut previous_end = None;
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
        // One track per journal: the header's, and continuous.
        assert_eq!(frame.track(), header.track(), "a frame of another track");
        // None before its epoch's first sample, where the header gives it.
        if let Some(anchor) = header.anchor() {
            assert!(
                frame.range().start() >= anchor.first_sample,
                "a frame before its epoch"
            );
        }
        if let Some(end) = previous_end {
            assert_eq!(frame.range().start(), end, "frames are not contiguous");
        }
        previous_end = Some(frame.range().end());
        // The frame's bytes in the input carry a CRC that matches them:
        // checked here independently of the reader.
        let bytes = &data[expected_len..expected_len + FRAME_HEADER_LEN + 2 * samples];
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&bytes[..28]);
        hasher.update(&bytes[FRAME_HEADER_LEN..]);
        let stored = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
        assert_eq!(
            hasher.finalize(),
            stored,
            "returned a frame whose CRC fails"
        );
        let decoded: Vec<i16> = bytes[FRAME_HEADER_LEN..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();
        assert_eq!(decoded, frame.samples(), "samples differ from the input");
        let track = u32::from_le_bytes(field(bytes, 12));
        assert_eq!(track, header.track().get(), "track differs from the input");
        expected_len += FRAME_HEADER_LEN + 2 * samples;
    }
    assert_eq!(read.valid_len(), expected_len, "valid_len does not add up");

    match read.end() {
        ReadEnd::Complete => {
            assert_eq!(read.valid_len(), data.len(), "Complete before end of input");
        }
        // A frame refused for its track really is another track's.
        ReadEnd::Invalid {
            offset,
            reason: Invalid::Track { expected, found },
        } => {
            assert_eq!(offset, read.valid_len(), "stopped somewhere else");
            assert_eq!(expected, header.track(), "expected another track");
            assert_ne!(found, expected, "refused its own track");
            assert_eq!(
                u32::from_le_bytes(field(data, offset + 12)),
                found.get(),
                "the refused frame's track"
            );
        }
        _ => {}
    }

    let again = read_journal(&data[..read.valid_len()]);
    assert_eq!(
        again.header(),
        read.header(),
        "re-reading changes the header"
    );
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
