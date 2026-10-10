//! The journal's bytes on disk, and the reader that parses them back.
//!
//! All integers are little-endian. A journal holds one track's audio from
//! one epoch: a header, then frames.
//!
//! | Bytes | Header field |
//! |---|---|
//! | 0..8 | magic, `NOTAJRNL` |
//! | 8..10 | format version, 3 |
//! | 10..14 | sample rate in hertz |
//! | 14..22 | journal id ([`JournalId`]), also in the file name |
//! | 22..26 | track |
//! | 26..30 | the track's epoch |
//! | 30..38 | the epoch's first sample |
//! | 38..46 | the epoch's start, in session-time nanoseconds |
//! | 46..50 | CRC-32 of bytes 0..46 |
//!
//! Bytes 26..46 are the epoch's [`EpochAnchor`]. Every journal of an epoch
//! carries it, and the header is fsync'd before the first frame, so the
//! epoch's timing is on disk no later than its first sample. Salvage can
//! then time the audio of any journal it finds.
//!
//! | Bytes | Frame field |
//! |---|---|
//! | 0..4 | magic, `NJFR` |
//! | 4..12 | sequence number: 0 for the first frame, then +1 |
//! | 12..16 | track, the header's |
//! | 16..24 | the track's sample index of the first sample |
//! | 24..28 | number of samples, 1 to [`MAX_FRAME_SAMPLES`] |
//! | 28..32 | CRC-32 of bytes 0..28 and the samples |
//! | 32.. | the samples: mono 16-bit signed PCM |
//!
//! The reader runs during salvage on files that may be torn or corrupt, so
//! it treats its input as untrusted. It stops at the first frame that isn't
//! valid and returns only the frames before it. Valid means a complete frame
//! with the right magic, a length in range, a matching CRC, the next
//! sequence number, the header's track, and, after the first frame, a first
//! sample that continues the last frame exactly. So the audio it returns is
//! sample-continuous.
//!
//! A journal never holds a frame from before its epoch's first sample.
//!
//! Version 2 is version 3 without bytes 30..46: its CRC is at 30..34, and
//! its epoch's timing wasn't kept. The reader still reads it, with no
//! anchor, so a journal left by an older nota is salvaged. Version 1 (no
//! id, track or epoch in the header, several tracks per file) was only ever
//! written by tests; the reader refuses it.

use std::fmt;

use nota_core::{
    EpochAnchor, EpochId, SampleCount, SampleIndex, SampleRange, SampleRate, SessionTime, TrackId,
};

use super::JournalId;

/// The journal file's magic number.
const FILE_MAGIC: [u8; 8] = *b"NOTAJRNL";
/// The format this code writes.
const VERSION: u16 = 3;
/// The older format the reader still reads, whose header has no anchor.
const UNTIMED_VERSION: u16 = 2;
/// Bytes in the file header: the longest header any version has, so a
/// file no longer than this holds no frame.
pub const HEADER_LEN: usize = 50;
/// Bytes of the header the CRC covers.
const HEADER_FIELDS: usize = 46;
/// Bytes in a version 2 header, and of it the CRC covers.
const UNTIMED_HEADER_LEN: usize = 34;
const UNTIMED_HEADER_FIELDS: usize = 30;

/// Each frame's magic number.
const FRAME_MAGIC: [u8; 4] = *b"NJFR";
/// Bytes in a frame before its samples.
pub const FRAME_HEADER_LEN: usize = 32;
/// The most samples one frame holds: about half a second at 16 kHz. The
/// writer splits longer runs; the reader refuses longer frames, so a corrupt
/// length can't make it look far ahead.
pub const MAX_FRAME_SAMPLES: u32 = 8_192;
/// Bytes per sample.
const SAMPLE_BYTES: usize = 2;

/// The journal's header: what the file is, whose audio it holds, and how
/// that audio is timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalHeader {
    id: JournalId,
    track: TrackId,
    epoch: EpochId,
    rate: SampleRate,
    /// The epoch's start and first sample; `None` in a version 2 journal.
    timing: Option<(SessionTime, SampleIndex)>,
}

impl JournalHeader {
    /// The header of journal `id`, holding `track`'s audio from the epoch
    /// `anchor` times, at its rate.
    #[must_use]
    pub const fn new(id: JournalId, track: TrackId, anchor: EpochAnchor) -> Self {
        Self {
            id,
            track,
            epoch: anchor.id,
            rate: anchor.rate,
            timing: Some((anchor.start, anchor.first_sample)),
        }
    }

    /// The header of a version 2 journal: `track`'s audio from `epoch` at
    /// `rate`, with no anchor. Only tests write one, as an older nota did.
    #[cfg(test)]
    pub(crate) const fn untimed(
        id: JournalId,
        track: TrackId,
        epoch: EpochId,
        rate: SampleRate,
    ) -> Self {
        Self {
            id,
            track,
            epoch,
            rate,
            timing: None,
        }
    }

    /// How the journal's epoch is timed, or `None` for a version 2
    /// journal, which didn't keep it.
    #[must_use]
    pub const fn anchor(self) -> Option<EpochAnchor> {
        match self.timing {
            Some((start, first_sample)) => Some(EpochAnchor {
                id: self.epoch,
                start,
                first_sample,
                rate: self.rate,
            }),
            None => None,
        }
    }

    /// How many bytes the header takes on disk.
    #[must_use]
    pub const fn encoded_len(self) -> usize {
        if self.timing.is_some() {
            HEADER_LEN
        } else {
            UNTIMED_HEADER_LEN
        }
    }

    /// The journal's id.
    #[must_use]
    pub const fn id(self) -> JournalId {
        self.id
    }

    /// The track every frame belongs to.
    #[must_use]
    pub const fn track(self) -> TrackId {
        self.track
    }

    /// The track's epoch the audio was captured in.
    #[must_use]
    pub const fn epoch(self) -> EpochId {
        self.epoch
    }

    /// The sampling rate.
    #[must_use]
    pub const fn rate(self) -> SampleRate {
        self.rate
    }
}

/// One valid frame: a run of samples from one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    seq: u64,
    track: TrackId,
    range: SampleRange,
    samples: Vec<i16>,
}

impl Frame {
    /// The frame's sequence number, counting from 0 in the file.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// The track the samples belong to: always the header's.
    #[must_use]
    pub const fn track(&self) -> TrackId {
        self.track
    }

    /// The samples' positions in the track.
    #[must_use]
    pub const fn range(&self) -> SampleRange {
        self.range
    }

    /// The samples. There are exactly `range().len()` of them.
    #[must_use]
    pub fn samples(&self) -> &[i16] {
        &self.samples
    }
}

/// Why the reader stopped. For logs and diagnostics only: it doesn't
/// reliably tell a torn write from corruption. A frame's length is read
/// before its CRC can vouch for it, so a corrupt length can look like a torn
/// tail, and a torn tail that reads back as zeros looks invalid. Salvage
/// treats both the same: keep the frames before `valid_len`, drop the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadEnd {
    /// The file ended exactly after a valid frame (or the header).
    Complete,
    /// The file ended partway through the header or a frame: a torn write.
    /// The incomplete part starts at `offset`.
    Incomplete {
        /// Where the incomplete header or frame starts.
        offset: usize,
    },
    /// The header or a frame at `offset` is invalid.
    Invalid {
        /// Where the invalid header or frame starts.
        offset: usize,
        /// What's wrong with it.
        reason: Invalid,
    },
}

/// What made a header or frame invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    /// The file header's magic, version, rate or CRC is wrong.
    Header,
    /// The frame belongs to another track than the header's.
    Track {
        /// The header's track.
        expected: TrackId,
        /// The frame's.
        found: TrackId,
    },
    /// The frame doesn't start with the frame magic.
    Magic,
    /// The frame's length is zero or above [`MAX_FRAME_SAMPLES`], or its
    /// samples would run past the largest sample index.
    Length,
    /// The frame's CRC doesn't match its contents.
    Crc,
    /// The frame's sequence number isn't the next one.
    Sequence {
        /// The sequence number due.
        expected: u64,
        /// The one in the frame.
        found: u64,
    },
    /// The frame starts before its epoch's first sample, which the header
    /// gives.
    BeforeEpoch {
        /// The epoch's first sample.
        epoch_start: SampleIndex,
        /// The frame's first sample.
        found: SampleIndex,
    },
    /// The frame doesn't continue the track where the last frame ended.
    Discontinuous {
        /// The track.
        track: TrackId,
        /// The sample index due.
        expected: SampleIndex,
        /// The one in the frame.
        found: SampleIndex,
    },
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header => f.write_str("bad journal header"),
            Self::Track { expected, found } => write!(
                f,
                "a frame of track {} in a journal of track {}",
                found.get(),
                expected.get()
            ),
            Self::Magic => f.write_str("bad frame magic"),
            Self::Length => f.write_str("frame length out of range"),
            Self::Crc => f.write_str("frame CRC mismatch"),
            Self::Sequence { expected, found } => {
                write!(f, "frame {found} where frame {expected} was due")
            }
            Self::BeforeEpoch { epoch_start, found } => write!(
                f,
                "a frame at sample {} before its epoch's first sample {}",
                found.get(),
                epoch_start.get()
            ),
            Self::Discontinuous {
                track,
                expected,
                found,
            } => write!(
                f,
                "track {} jumps from sample {} to {}",
                track.get(),
                expected.get(),
                found.get()
            ),
        }
    }
}

/// What the reader recovered from a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRead {
    header: Option<JournalHeader>,
    frames: Vec<Frame>,
    valid_len: usize,
    end: ReadEnd,
}

impl JournalRead {
    /// The header, if it was complete and valid.
    #[must_use]
    pub const fn header(&self) -> Option<JournalHeader> {
        self.header
    }

    /// The valid frames, in file order.
    #[must_use]
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// How many bytes from the start are the header and valid frames.
    #[must_use]
    pub const fn valid_len(&self) -> usize {
        self.valid_len
    }

    /// Why the reader stopped.
    #[must_use]
    pub const fn end(&self) -> ReadEnd {
        self.end
    }

    /// The samples the valid frames cover, continuous by construction.
    /// `None` if there are no valid frames.
    #[must_use]
    pub fn range(&self) -> Option<SampleRange> {
        let first = self.frames.first()?;
        let last = self.frames.last()?;
        SampleRange::new(first.range.start(), last.range.end())
    }

    /// The recovered audio: its sample range and the samples, copied into
    /// one buffer. `None` if there are no valid frames. Salvage reads
    /// [`Self::frames`] instead, to avoid the copy.
    #[must_use]
    pub fn audio(&self) -> Option<(SampleRange, Vec<i16>)> {
        let range = self.range()?;
        let samples = self
            .frames
            .iter()
            .flat_map(|f| f.samples.iter().copied())
            .collect();
        Some((range, samples))
    }
}

/// Reads a journal from its bytes, up to the first thing that isn't valid.
/// Never panics, whatever the input.
#[must_use]
pub fn read_journal(bytes: &[u8]) -> JournalRead {
    let stop = |header, frames, valid_len, end| JournalRead {
        header,
        frames,
        valid_len,
        end,
    };

    let header = match parse_header(bytes) {
        Parsed::Frame(header, _) => header,
        Parsed::Incomplete => {
            return stop(None, Vec::new(), 0, ReadEnd::Incomplete { offset: 0 });
        }
        Parsed::Invalid(reason) => {
            return stop(None, Vec::new(), 0, ReadEnd::Invalid { offset: 0, reason });
        }
    };

    let mut frames = Vec::new();
    let mut offset = header.encoded_len();
    let mut next_seq = 0_u64;
    let mut next_sample = None;
    loop {
        let rest = &bytes[offset..];
        if rest.is_empty() {
            return stop(Some(header), frames, offset, ReadEnd::Complete);
        }
        match parse_frame(rest, next_seq, header, next_sample) {
            Parsed::Frame(frame, len) => {
                next_sample = Some(frame.range.end());
                frames.push(frame);
                offset += len;
                next_seq += 1;
            }
            Parsed::Incomplete => {
                return stop(Some(header), frames, offset, ReadEnd::Incomplete { offset });
            }
            Parsed::Invalid(reason) => {
                return stop(
                    Some(header),
                    frames,
                    offset,
                    ReadEnd::Invalid { offset, reason },
                );
            }
        }
    }
}

/// The sample ranges of the frames of `track` found anywhere in `bytes`
/// from `from` on, each checked on its own: the right magic, a length in
/// range, the whole frame present, a matching CRC and the track. Sequence
/// and continuity aren't checked, so this finds the frames after damage
/// that [`read_journal`] stops at. Salvage uses it to tell corruption,
/// which can leave fsync'd audio after it, from a torn tail. Never panics,
/// whatever the input.
#[must_use]
pub fn frames_after(bytes: &[u8], from: usize, track: TrackId) -> Vec<SampleRange> {
    let mut found = Vec::new();
    let mut offset = from;
    while let Some(rest) = bytes.get(offset..) {
        let Some(at) = rest
            .windows(FRAME_MAGIC.len())
            .position(|w| w == FRAME_MAGIC)
        else {
            break;
        };
        offset += at;
        match frame_alone(&bytes[offset..], track) {
            Some((range, len)) => {
                found.push(range);
                offset += len;
            }
            None => offset += 1,
        }
    }
    found
}

/// The frame at the start of `bytes`, if it checks out on its own, and its
/// length in bytes. See [`frames_after`].
fn frame_alone(bytes: &[u8], track: TrackId) -> Option<(SampleRange, usize)> {
    let head = bytes.get(..FRAME_HEADER_LEN)?;
    if head[..4] != FRAME_MAGIC {
        return None;
    }
    let (found, first, len, crc) = (
        TrackId::new(u32::from_le_bytes(array(head, 12)?)),
        SampleIndex::new(u64::from_le_bytes(array(head, 16)?)),
        u32::from_le_bytes(array(head, 24)?),
        u32::from_le_bytes(array(head, 28)?),
    );
    if len == 0 || len > MAX_FRAME_SAMPLES || found != track {
        return None;
    }
    let total = FRAME_HEADER_LEN + usize::try_from(len).ok()? * SAMPLE_BYTES;
    let payload = bytes.get(FRAME_HEADER_LEN..total)?;
    if frame_crc(&head[..28], payload) != crc {
        return None;
    }
    let range = SampleRange::starting_at(first, SampleCount::new(u64::from(len)))?;
    Some((range, total))
}

/// The header at the start of `bytes`, of either version: incomplete if
/// the bytes end before the header its version says, invalid if its magic,
/// version, CRC or rate is wrong.
fn parse_header(bytes: &[u8]) -> Parsed<JournalHeader> {
    let (Some(magic), Some(version)) = (bytes.get(..8), array(bytes, 8).map(u16::from_le_bytes))
    else {
        // Too short to say which version: torn.
        return Parsed::Incomplete;
    };
    let (len, fields) = match version {
        VERSION => (HEADER_LEN, HEADER_FIELDS),
        UNTIMED_VERSION => (UNTIMED_HEADER_LEN, UNTIMED_HEADER_FIELDS),
        _ => return Parsed::Invalid(Invalid::Header),
    };
    if magic != FILE_MAGIC {
        return Parsed::Invalid(Invalid::Header);
    }
    let Some(header) = bytes.get(..len) else {
        return Parsed::Incomplete;
    };
    match header_fields(header, fields, version == VERSION) {
        Some(parsed) => Parsed::Frame(parsed, len),
        None => Parsed::Invalid(Invalid::Header),
    }
}

/// The fields of a complete `header` whose CRC follows its first `fields`
/// bytes, with the anchor's timing if it's `timed`; `None` if the CRC or
/// the rate is wrong.
fn header_fields(header: &[u8], fields: usize, timed: bool) -> Option<JournalHeader> {
    if u32::from_le_bytes(array(header, fields)?) != crc32fast::hash(header.get(..fields)?) {
        return None;
    }
    let timing = if timed {
        Some((
            SessionTime::from_nanos(u64::from_le_bytes(array(header, 38)?)),
            SampleIndex::new(u64::from_le_bytes(array(header, 30)?)),
        ))
    } else {
        None
    };
    Some(JournalHeader {
        rate: SampleRate::new(u32::from_le_bytes(array(header, 10)?))?,
        id: JournalId::new(u64::from_le_bytes(array(header, 14)?)),
        track: TrackId::new(u32::from_le_bytes(array(header, 22)?)),
        epoch: EpochId::new(u32::from_le_bytes(array(header, 26)?)),
        timing,
    })
}

/// What parsing a header or a frame found.
enum Parsed<T> {
    /// A valid header or frame, and its length in bytes.
    Frame(T, usize),
    /// The bytes end partway through it.
    Incomplete,
    /// It isn't valid, for this reason.
    Invalid(Invalid),
}

fn parse_frame(
    bytes: &[u8],
    next_seq: u64,
    header: JournalHeader,
    next_sample: Option<SampleIndex>,
) -> Parsed<Frame> {
    let journal_track = header.track;
    let Some(head) = bytes.get(..FRAME_HEADER_LEN) else {
        // Too short for a header: torn, unless what's there already shows
        // it isn't a frame.
        return if bytes.len() >= 4 && bytes[..4] != FRAME_MAGIC {
            Parsed::Invalid(Invalid::Magic)
        } else {
            Parsed::Incomplete
        };
    };
    let (Some(seq), Some(track), Some(first), Some(len), Some(crc)) = (
        array(head, 4).map(u64::from_le_bytes),
        array(head, 12).map(u32::from_le_bytes),
        array(head, 16).map(u64::from_le_bytes),
        array(head, 24).map(u32::from_le_bytes),
        array(head, 28).map(u32::from_le_bytes),
    ) else {
        return Parsed::Incomplete;
    };
    if head[..4] != FRAME_MAGIC {
        return Parsed::Invalid(Invalid::Magic);
    }
    if len == 0 || len > MAX_FRAME_SAMPLES {
        return Parsed::Invalid(Invalid::Length);
    }
    // At most 8192 * 2 bytes past the header, so this can't overflow.
    let Some(total) = usize::try_from(len)
        .ok()
        .map(|len| FRAME_HEADER_LEN + len * SAMPLE_BYTES)
    else {
        return Parsed::Invalid(Invalid::Length);
    };
    let Some(payload) = bytes.get(FRAME_HEADER_LEN..total) else {
        return Parsed::Incomplete;
    };
    if frame_crc(&head[..28], payload) != crc {
        return Parsed::Invalid(Invalid::Crc);
    }
    if seq != next_seq {
        return Parsed::Invalid(Invalid::Sequence {
            expected: next_seq,
            found: seq,
        });
    }
    let track = TrackId::new(track);
    if track != journal_track {
        return Parsed::Invalid(Invalid::Track {
            expected: journal_track,
            found: track,
        });
    }
    let first = SampleIndex::new(first);
    let Some(range) = SampleRange::starting_at(first, SampleCount::new(u64::from(len))) else {
        return Parsed::Invalid(Invalid::Length);
    };
    if let Some((_, epoch_start)) = header.timing
        && first < epoch_start
    {
        return Parsed::Invalid(Invalid::BeforeEpoch {
            epoch_start,
            found: first,
        });
    }
    if let Some(expected) = next_sample
        && expected != first
    {
        return Parsed::Invalid(Invalid::Discontinuous {
            track,
            expected,
            found: first,
        });
    }
    let samples = payload
        .as_chunks::<SAMPLE_BYTES>()
        .0
        .iter()
        .map(|&pair| i16::from_le_bytes(pair))
        .collect();
    Parsed::Frame(
        Frame {
            seq,
            track,
            range,
            samples,
        },
        total,
    )
}

/// `N` bytes of `bytes` from `at`, or `None` if they run past the end.
fn array<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn frame_crc(head: &[u8], payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(head);
    hasher.update(payload);
    hasher.finalize()
}

/// The bytes of `header`: version 3, or version 2 for a header with no
/// anchor.
#[must_use]
pub fn encode_header(header: JournalHeader) -> Vec<u8> {
    let version = if header.timing.is_some() {
        VERSION
    } else {
        UNTIMED_VERSION
    };
    let mut out = Vec::with_capacity(header.encoded_len());
    out.extend_from_slice(&FILE_MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&header.rate.hz().to_le_bytes());
    out.extend_from_slice(&header.id.get().to_le_bytes());
    out.extend_from_slice(&header.track.get().to_le_bytes());
    out.extend_from_slice(&header.epoch.get().to_le_bytes());
    if let Some((start, first_sample)) = header.timing {
        out.extend_from_slice(&first_sample.get().to_le_bytes());
        out.extend_from_slice(&start.as_nanos().to_le_bytes());
    }
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Appends one frame to `out`. The caller keeps `samples` between 1 and
/// [`MAX_FRAME_SAMPLES`] long, and the sequence and positions continuous;
/// the writer does, and the tests use this to build bad journals on
/// purpose.
pub(crate) fn encode_frame(
    out: &mut Vec<u8>,
    seq: u64,
    track: TrackId,
    first: SampleIndex,
    samples: &[i16],
) {
    let start = out.len();
    out.extend_from_slice(&FRAME_MAGIC);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&track.get().to_le_bytes());
    out.extend_from_slice(&first.get().to_le_bytes());
    // The writer never passes more than MAX_FRAME_SAMPLES.
    let len = u32::try_from(samples.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&[0; 4]); // the CRC, filled in below
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    let crc = frame_crc(&out[start..start + 28], &out[start + FRAME_HEADER_LEN..]);
    out[start + 28..start + FRAME_HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
}
