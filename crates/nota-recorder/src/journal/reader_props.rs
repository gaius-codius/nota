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
//! - it never returns a frame from before its epoch's first sample, which
//!   a timed header gives.
//!
//! And the scan salvage uses to look past damage ([`frames_after`]): it
//! never panics, finds every frame of a valid journal, and after damage
//! finds exactly the frames after the damaged one.
//!
//! A failure proptest finds is saved in `proptest-regressions/` and replayed
//! on every run; commit that file with the fix.

use nota_core::{Drift, EpochAnchor, EpochId, SampleIndex, SampleRate, SessionTime, TrackId};
use proptest::prelude::*;

use super::format::{HEADER_LEN, MAX_FRAME_SAMPLES, encode_frame, encode_header, frames_after};
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

/// The anchor of epoch `epoch` at `rate`, starting at session time zero and
/// sample zero, so any frame the strategies make falls inside the epoch.
fn anchor(epoch: u32, rate: SampleRate) -> EpochAnchor {
    EpochAnchor {
        id: EpochId::new(epoch),
        start: SessionTime::ZERO,
        first_sample: SampleIndex::ZERO,
        rate,
        drift: Drift::ZERO,
    }
}

/// Any header: any id, track and epoch, any valid rate and drift.
fn header() -> impl Strategy<Value = JournalHeader> {
    (
        any::<u64>(),
        any::<u32>(),
        any::<u32>(),
        1..=SampleRate::MAX_HZ,
        -Drift::MAX_PPB..=Drift::MAX_PPB,
    )
        .prop_map(|(id, track, epoch, hz, ppb)| {
            let rate = SampleRate::new(hz).unwrap();
            let anchor = EpochAnchor {
                drift: Drift::from_ppb(ppb).unwrap(),
                ..anchor(epoch, rate)
            };
            JournalHeader::new(JournalId::new(id), TrackId::new(track), anchor)
        })
}

/// `bytes`, a version 4 journal, as version 3 wrote it: the same fields
/// and frames, with no drift and the CRC at 46..50.
fn as_version_3(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes[..46].to_vec();
    out[8..10].copy_from_slice(&3_u16.to_le_bytes());
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&bytes[HEADER_LEN..]);
    out
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
            let mut bytes = encode_header(header);
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

/// The sample ranges of `frames`, as [`frames_after`] reports them.
fn ranges(frames: &[FrameSpec]) -> Vec<(u64, u64)> {
    frames
        .iter()
        .map(|f| (f.2, f.2 + f.3.len() as u64))
        .collect()
}

fn scanned(bytes: &[u8], from: usize, track: TrackId) -> Vec<(u64, u64)> {
    frames_after(bytes, from, track)
        .iter()
        .map(|r| (r.start().get(), r.end().get()))
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
    /// A version 3 journal, whole, cut anywhere past its header or damaged
    /// anywhere in its frames, reads the same frames as the version 4
    /// journal it differs from only by its header: its frames start after
    /// its own, shorter, header.
    #[test]
    fn a_version_3_journal_reads_as_its_version_4_twin(
        j in journal(),
        at in any::<prop::sample::Index>(),
        flip in any::<bool>(),
    ) {
        let old = as_version_3(&j.bytes);
        let shift = HEADER_LEN - 50;
        let frames = j.bytes.len() - HEADER_LEN;
        let at = at.index(frames + 1);
        let (mut new, mut older) = (j.bytes.clone(), old);
        if flip && at < frames {
            new[HEADER_LEN + at] ^= 0x20;
            older[50 + at] ^= 0x20;
        } else {
            new.truncate(HEADER_LEN + at);
            older.truncate(50 + at);
        }
        let (read_new, read_old) = (read_journal(&new), read_journal(&older));
        prop_assert_eq!(specs(&read_old), specs(&read_new));
        prop_assert_eq!(read_old.valid_len() + shift, read_new.valid_len());
        let undrifted = j.header.anchor().map(|a| EpochAnchor { drift: Drift::ZERO, ..a });
        prop_assert_eq!(read_old.header().and_then(JournalHeader::anchor), undrifted);
    }

    #[test]
    fn valid_journals_read_back_whole(j in journal()) {
        let read = read_journal(&j.bytes);
        check_contract(&j.bytes, &read)?;
        prop_assert_eq!(specs(&read), j.frames);
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
        let mut input = encode_header(header);
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
        let header = JournalHeader::new(JournalId::new(id), TrackId::new(track), anchor(epoch, rate));
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

    /// Any anchor comes back as written, drift and all, in a version 4
    /// header, and a version 2 header comes back untimed; a frame before
    /// the epoch's first sample is never returned.
    #[test]
    fn anchors_round_trip_and_bound_the_frames(
        epoch in any::<u32>(),
        start in any::<u64>(),
        first in 1..u64::MAX / 2,
        before in 1..1_000u64,
        hz in 1..=SampleRate::MAX_HZ,
        ppb in -Drift::MAX_PPB..=Drift::MAX_PPB,
        untimed in any::<bool>(),
    ) {
        let rate = SampleRate::new(hz).unwrap();
        let anchor = EpochAnchor {
            id: EpochId::new(epoch),
            start: SessionTime::from_nanos(start),
            first_sample: SampleIndex::new(first),
            rate,
            drift: Drift::from_ppb(ppb).unwrap(),
        };
        let header = if untimed {
            JournalHeader::untimed(JournalId::FIRST, TrackId::new(0), anchor.id, rate)
        } else {
            JournalHeader::new(JournalId::FIRST, TrackId::new(0), anchor)
        };
        let mut bytes = encode_header(header);
        let read = read_journal(&bytes);
        prop_assert_eq!(read.header(), Some(header));
        prop_assert_eq!(
            read.header().and_then(JournalHeader::anchor),
            (!untimed).then_some(anchor)
        );
        let early = SampleIndex::new(first.saturating_sub(before));
        encode_frame(&mut bytes, 0, TrackId::new(0), early, &[1, 2, 3]);
        let read = read_journal(&bytes);
        check_contract(&bytes, &read)?;
        // Untimed, nothing bounds the frame; timed, it's before the epoch.
        prop_assert_eq!(read.frames().len(), usize::from(untimed));
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
        prop_assert_eq!(specs(&read), j.frames);
        // The scan finds no frame in the noise either, magic or not.
        prop_assert_eq!(scanned(&input, read.valid_len(), j.header.track()), Vec::new());
    }

    /// The scan finds every frame of a valid journal, in order.
    #[test]
    fn the_scan_finds_every_frame(j in journal()) {
        prop_assert_eq!(scanned(&j.bytes, HEADER_LEN, j.header.track()), ranges(&j.frames));
    }

    /// One flipped bit in a frame: the read stops there, and the scan from
    /// where it stopped finds exactly the frames after the damaged one.
    #[test]
    fn after_a_bit_flip_the_scan_finds_the_frames_past_it(
        j in journal(),
        at in any::<prop::sample::Index>(),
        bit in 0_u8..8,
    ) {
        prop_assume!(j.bytes.len() > HEADER_LEN);
        let at = HEADER_LEN + at.index(j.bytes.len() - HEADER_LEN);
        let mut input = j.bytes.clone();
        input[at] ^= 1 << bit;
        let read = read_journal(&input);
        let damaged = j.ends.iter().filter(|&&end| end <= at).count();
        prop_assert_eq!(
            scanned(&input, read.valid_len(), j.header.track()),
            ranges(&j.frames[damaged + 1..])
        );
    }

    /// Any bytes, from anywhere (past the end too): no panic, and only
    /// frames of a length in range.
    #[test]
    fn the_scan_of_any_bytes_finds_only_whole_frames(
        input in prop::collection::vec(any::<u8>(), 0..2_000),
        from in 0_usize..2_100,
        track in any::<u32>(),
    ) {
        for r in frames_after(&input, from, TrackId::new(track)) {
            prop_assert!((1..=u64::from(MAX_FRAME_SAMPLES)).contains(&r.len().get()));
        }
    }
}
