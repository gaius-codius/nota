//! Property tests for the journal reader, which reads untrusted bytes during
//! salvage: arbitrary truncation, bit flips, duplicated, reordered, foreign
//! and other-track frames, and plain garbage, under random headers.
//!
//! The reader's contract, checked on every input:
//! - it never panics;
//! - every frame it returns has a valid CRC: re-encoding the frames gives
//!   back exactly the input's first `valid_len` bytes, CRCs included;
//! - it never returns samples past the first invalid frame: on a damaged
//!   journal it returns exactly the frames before the damage.
//!
//! A failure proptest finds is saved in `proptest-regressions/` and replayed
//! on every run; commit that file with the fix.

use nota_core::{EpochId, SampleIndex, SampleRate, TrackId};
use proptest::prelude::*;

use super::format::{HEADER_LEN, encode_frame, encode_header};
use super::{Invalid, JournalHeader, JournalId, JournalRead, ReadEnd, read_journal};

/// A frame as the tests compare it: sequence, track, first sample, samples.
type FrameSpec = (u64, TrackId, u64, Vec<i16>);

/// A valid journal: its header, its bytes, the frames in it, where each
/// frame ends, and the sample after the last one.
#[derive(Debug, Clone)]
struct Journal {
    header: JournalHeader,
    bytes: Vec<u8>,
    frames: Vec<FrameSpec>,
    ends: Vec<usize>,
    next: u64,
}

impl Journal {
    /// Where frame `k` starts.
    fn start_of(&self, k: usize) -> usize {
        if k == 0 { HEADER_LEN } else { self.ends[k - 1] }
    }

    /// The bytes of frame `k`.
    fn frame_bytes(&self, k: usize) -> &[u8] {
        &self.bytes[self.start_of(k)..self.ends[k]]
    }

    /// The first sample of frame `k`, or where a frame after the last would
    /// start.
    fn first_of(&self, k: usize) -> u64 {
        self.frames.get(k).map_or(self.next, |f| f.2)
    }
}

/// Any header: any id, track and epoch, any valid rate.
fn header() -> impl Strategy<Value = JournalHeader> {
    (
        any::<u64>(),
        any::<u32>(),
        any::<u32>(),
        1..=SampleRate::MAX_HZ,
    )
        .prop_map(|(id, track, epoch, hz)| {
            let rate = SampleRate::new(hz).unwrap();
            JournalHeader::new(
                JournalId::new(id),
                TrackId::new(track),
                EpochId::new(epoch),
                rate,
            )
        })
}

/// A journal under a random header: up to 12 frames of its track, the
/// first starting anywhere and each continuing the last exactly.
fn journal() -> impl Strategy<Value = Journal> {
    (
        header(),
        prop::collection::vec(prop::collection::vec(any::<i16>(), 1..40), 0..12),
        0_u64..1_000_000_000,
    )
        .prop_map(|(header, runs, start)| {
            let track = header.track();
            let mut bytes = encode_header(header).to_vec();
            let mut next = start;
            let mut frames = Vec::new();
            let mut ends = Vec::new();
            for (seq, samples) in (0_u64..).zip(runs) {
                encode_frame(&mut bytes, seq, track, SampleIndex::new(next), &samples);
                let first = next;
                next += samples.len() as u64;
                frames.push((seq, track, first, samples));
                ends.push(bytes.len());
            }
            Journal {
                header,
                bytes,
                frames,
                ends,
                next,
            }
        })
}

fn specs(read: &JournalRead) -> Vec<FrameSpec> {
    read.frames()
        .iter()
        .map(|f| {
            (
                f.seq(),
                f.track(),
                f.range().start().get(),
                f.samples().to_vec(),
            )
        })
        .collect()
}

/// The contract that holds for any input at all.
fn check_contract(input: &[u8], read: &JournalRead) -> Result<(), TestCaseError> {
    prop_assert!(read.valid_len() <= input.len());
    let mut reencoded = Vec::new();
    if let Some(header) = read.header() {
        reencoded.extend_from_slice(&encode_header(header));
        for f in read.frames() {
            prop_assert_eq!(f.samples().len() as u64, f.range().len().get());
            encode_frame(
                &mut reencoded,
                f.seq(),
                f.track(),
                f.range().start(),
                f.samples(),
            );
        }
    } else {
        prop_assert!(read.frames().is_empty());
    }
    // Byte for byte, CRCs included: every frame returned is valid, and they
    // are exactly the input's valid prefix.
    prop_assert_eq!(&reencoded[..], &input[..read.valid_len()]);
    prop_assert_eq!(
        read.end() == ReadEnd::Complete,
        read.header().is_some() && read.valid_len() == input.len()
    );
    Ok(())
}

proptest! {
    #[test]
    fn valid_journals_read_back_whole(j in journal()) {
        let read = read_journal(&j.bytes);
        check_contract(&j.bytes, &read)?;
        prop_assert_eq!(specs(&read), j.frames.clone());
        prop_assert_eq!(read.end(), ReadEnd::Complete);
        prop_assert_eq!(read.header(), Some(j.header));
    }

    /// Cut anywhere: exactly the frames that end before the cut come back,
    /// and a cut inside a frame is reported as incomplete there.
    #[test]
    fn truncation_keeps_the_complete_frames(j in journal(), cut in any::<prop::sample::Index>()) {
        let cut = cut.index(j.bytes.len() + 1);
        let input = &j.bytes[..cut];
        let read = read_journal(input);
        check_contract(input, &read)?;
        let kept = j.ends.iter().filter(|&&end| end <= cut).count();
        if cut < HEADER_LEN {
            prop_assert_eq!(read.header(), None);
            prop_assert_eq!(read.end(), ReadEnd::Incomplete { offset: 0 });
        } else {
            prop_assert_eq!(specs(&read), j.frames[..kept].to_vec());
            let boundary = j.start_of(kept);
            let want = if boundary == cut {
                ReadEnd::Complete
            } else {
                ReadEnd::Incomplete { offset: boundary }
            };
            prop_assert_eq!(read.end(), want);
        }
    }

    /// One flipped bit anywhere: the frame holding it and everything after
    /// are dropped (CRC-32 catches every single-bit error).
    #[test]
    fn a_bit_flip_drops_its_frame_and_the_rest(
        j in journal(),
        at in any::<prop::sample::Index>(),
        bit in 0_u8..8,
    ) {
        let at = at.index(j.bytes.len());
        let mut input = j.bytes.clone();
        input[at] ^= 1 << bit;
        let read = read_journal(&input);
        check_contract(&input, &read)?;
        prop_assert_ne!(read.end(), ReadEnd::Complete);
        if at < HEADER_LEN {
            prop_assert_eq!(read.header(), None);
            prop_assert_eq!(read.end(), ReadEnd::Invalid { offset: 0, reason: Invalid::Header });
        } else {
            let damaged = j.ends.iter().filter(|&&end| end <= at).count();
            prop_assert_eq!(specs(&read), j.frames[..damaged].to_vec());
            prop_assert_eq!(read.valid_len(), j.start_of(damaged));
        }
    }

    /// A frame written twice: the copy is out of sequence.
    #[test]
    fn a_duplicated_frame_stops_the_read(j in journal(), k in any::<prop::sample::Index>()) {
        prop_assume!(!j.frames.is_empty());
        let k = k.index(j.frames.len());
        let mut input = j.bytes[..j.ends[k]].to_vec();
        input.extend_from_slice(j.frame_bytes(k));
        input.extend_from_slice(&j.bytes[j.ends[k]..]);
        let read = read_journal(&input);
        check_contract(&input, &read)?;
        prop_assert_eq!(specs(&read), j.frames[..=k].to_vec());
        let want = ReadEnd::Invalid {
            offset: j.ends[k],
            reason: Invalid::Sequence { expected: k as u64 + 1, found: k as u64 },
        };
        prop_assert_eq!(read.end(), want);
    }

    /// Two neighbouring frames swapped: the read stops before the pair.
    #[test]
    fn reordered_frames_stop_the_read(j in journal(), k in any::<prop::sample::Index>()) {
        prop_assume!(j.frames.len() >= 2);
        let k = k.index(j.frames.len() - 1);
        let mut input = j.bytes[..j.start_of(k)].to_vec();
        input.extend_from_slice(j.frame_bytes(k + 1));
        input.extend_from_slice(j.frame_bytes(k));
        input.extend_from_slice(&j.bytes[j.ends[k + 1]..]);
        let read = read_journal(&input);
        check_contract(&input, &read)?;
        prop_assert_eq!(specs(&read), j.frames[..k].to_vec());
        prop_assert!(
            matches!(read.end(), ReadEnd::Invalid { reason: Invalid::Sequence { .. }, offset } if offset == j.start_of(k)),
            "{:?}", read.end()
        );
    }

    /// Random bytes between two frames: nothing after them comes back.
    #[test]
    fn garbage_between_frames_stops_the_read(
        j in journal(),
        k in any::<prop::sample::Index>(),
        garbage in prop::collection::vec(any::<u8>(), 1..200),
    ) {
        let k = k.index(j.frames.len() + 1);
        let at = j.start_of(k);
        let mut input = j.bytes[..at].to_vec();
        input.extend_from_slice(&garbage);
        input.extend_from_slice(&j.bytes[at..]);
        let read = read_journal(&input);
        check_contract(&input, &read)?;
        prop_assert_eq!(specs(&read), j.frames[..k].to_vec());
        prop_assert_ne!(read.end(), ReadEnd::Complete);
    }

    /// A well-formed frame with a valid CRC that doesn't belong (from
    /// another journal, say): its sequence number gives it away.
    #[test]
    fn a_foreign_frame_stops_the_read(
        j in journal(),
        k in any::<prop::sample::Index>(),
        seq_skew in 1_u64..1_000,
        same_track in any::<bool>(),
        other_track in any::<u32>(),
        first in any::<u64>(),
        samples in prop::collection::vec(any::<i16>(), 1..40),
    ) {
        let k = k.index(j.frames.len() + 1);
        let at = j.start_of(k);
        let mut input = j.bytes[..at].to_vec();
        let seq = (k as u64).wrapping_add(seq_skew);
        let track = if same_track { j.header.track() } else { TrackId::new(other_track) };
        encode_frame(&mut input, seq, track, SampleIndex::new(first), &samples);
        input.extend_from_slice(&j.bytes[at..]);
        let read = read_journal(&input);
        check_contract(&input, &read)?;
        prop_assert_eq!(specs(&read), j.frames[..k].to_vec());
        prop_assert_ne!(read.end(), ReadEnd::Complete);
    }

    /// Any bytes at all.
    #[test]
    fn arbitrary_bytes_keep_the_contract(input in prop::collection::vec(any::<u8>(), 0..2_000)) {
        let read = read_journal(&input);
        check_contract(&input, &read)?;
    }

    /// A valid header followed by anything: the frame parser gets the noise.
    #[test]
    fn a_valid_header_then_noise_keeps_the_contract(
        header in header(),
        noise in prop::collection::vec(any::<u8>(), 0..2_000),
    ) {
        let mut input = encode_header(header).to_vec();
        input.extend_from_slice(&noise);
        let read = read_journal(&input);
        check_contract(&input, &read)?;
        prop_assert_eq!(read.header(), Some(header));
    }

    /// Every header field comes back as written.
    #[test]
    fn the_header_round_trips(
        id in any::<u64>(),
        track in any::<u32>(),
        epoch in any::<u32>(),
        hz in 1..=SampleRate::MAX_HZ,
    ) {
        let rate = SampleRate::new(hz).unwrap();
        let header = JournalHeader::new(
            JournalId::new(id),
            TrackId::new(track),
            EpochId::new(epoch),
            rate,
        );
        let bytes = encode_header(header);
        prop_assert_eq!(bytes.len(), HEADER_LEN);
        let read = read_journal(&bytes);
        check_contract(&bytes, &read)?;
        let got = read.header();
        prop_assert_eq!(got, Some(header));
        prop_assert_eq!(got.map(JournalHeader::id), Some(JournalId::new(id)));
        prop_assert_eq!(got.map(JournalHeader::track), Some(TrackId::new(track)));
        prop_assert_eq!(got.map(JournalHeader::epoch), Some(EpochId::new(epoch)));
        prop_assert_eq!(got.map(JournalHeader::rate), Some(rate));
        prop_assert_eq!((read.end(), read.valid_len()), (ReadEnd::Complete, HEADER_LEN));
    }

    /// A frame that is right in every way but its track: refused as
    /// another track's, and nothing after it comes back.
    #[test]
    fn a_frame_of_another_track_stops_the_read(
        j in journal(),
        k in any::<prop::sample::Index>(),
        other in any::<u32>(),
        samples in prop::collection::vec(any::<i16>(), 1..40),
    ) {
        let found = TrackId::new(other);
        prop_assume!(found != j.header.track());
        let k = k.index(j.frames.len() + 1);
        let at = j.start_of(k);
        let mut input = j.bytes[..at].to_vec();
        encode_frame(&mut input, k as u64, found, SampleIndex::new(j.first_of(k)), &samples);
        input.extend_from_slice(&j.bytes[at..]);
        let read = read_journal(&input);
        check_contract(&input, &read)?;
        prop_assert_eq!(specs(&read), j.frames[..k].to_vec());
        prop_assert_eq!(read.valid_len(), at);
        let want = ReadEnd::Invalid {
            offset: at,
            reason: Invalid::Track { expected: j.header.track(), found },
        };
        prop_assert_eq!(read.end(), want);
    }

    /// Noise that starts like a frame (right magic), so the reader gets past
    /// the first check.
    #[test]
    fn noise_with_frame_magic_keeps_the_contract(
        j in journal(),
        noise in prop::collection::vec(any::<u8>(), 0..200),
    ) {
        let mut input = j.bytes.clone();
        input.extend_from_slice(b"NJFR");
        input.extend_from_slice(&noise);
        let read = read_journal(&input);
        check_contract(&input, &read)?;
        prop_assert_eq!(specs(&read), j.frames.clone());
    }
}
