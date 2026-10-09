use std::path::Path;

use nota_core::{EpochId, SampleIndex, SampleRange, TrackId};
use nota_store::Sha256Digest;

use super::*;
use crate::fs::FsFile;
use crate::fs::fake::FakeFs;

const TRACK: TrackId = TrackId::new(1);

fn samples(n: u64) -> Vec<i16> {
    (0..n)
        .map(|i| i16::try_from(i % 3_000).unwrap() - 1_500)
        .collect()
}

/// Publishes `audio`, encoded at `rate`, as the segment of `TRACK` from
/// sample `first`, and gives its row.
fn publish(fs: &FakeFs, dir: &Path, first: u64, audio: &[i16], rate: SampleRate) -> SegmentRow {
    let bytes = flac::encode(rate, &[audio]).unwrap();
    let range = SampleRange::new(
        SampleIndex::new(first),
        SampleIndex::new(first + audio.len() as u64),
    )
    .unwrap();
    let mut file = fs
        .create(&dir.join(segment_file_name(TRACK, range)))
        .unwrap();
    file.write_all(&bytes).unwrap();
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    SegmentRow::new(TRACK, EpochId::new(0), range, Sha256Digest::new(digest)).unwrap()
}

fn dir(fs: &FakeFs) -> &'static Path {
    let dir = Path::new("/s");
    fs.create_dir(dir).unwrap();
    dir
}

#[test]
fn a_segment_reads_back_as_it_was_published() {
    let fs = FakeFs::new();
    let dir = dir(&fs);
    let audio = samples(10_000);
    let row = publish(&fs, dir, 4_800_000, &audio, SampleRate::SPEECH);
    assert_eq!(
        read_segment(&fs, dir, &row, SampleRate::SPEECH).unwrap(),
        audio
    );
}

#[test]
fn a_segment_that_isnt_what_its_row_says_gives_no_audio() {
    let fs = FakeFs::new();
    let dir = dir(&fs);
    let audio = samples(5_000);
    let row = publish(&fs, dir, 0, &audio, SampleRate::SPEECH);

    // Another hash.
    let other =
        SegmentRow::new(TRACK, row.epoch(), row.range(), Sha256Digest::new([7; 32])).unwrap();
    assert!(matches!(
        read_segment(&fs, dir, &other, SampleRate::SPEECH),
        Err(ReadSegmentError::Hash)
    ));
    // Another rate than the caller's.
    let fast = SampleRate::new(48_000).unwrap();
    assert!(matches!(
        read_segment(&fs, dir, &row, fast),
        Err(ReadSegmentError::Rate(r)) if r == SampleRate::SPEECH
    ));
    // A row longer than its file, under the same name and hash.
    let long = SegmentRow::new(
        TRACK,
        row.epoch(),
        SampleRange::new(SampleIndex::new(0), SampleIndex::new(5_001)).unwrap(),
        row.sha256(),
    )
    .unwrap();
    assert!(matches!(
        read_segment(&fs, dir, &long, SampleRate::SPEECH),
        Err(ReadSegmentError::Length {
            expected: 5_001,
            found: 5_000
        })
    ));
    // No file.
    let missing = SegmentRow::new(
        TRACK,
        row.epoch(),
        SampleRange::new(SampleIndex::new(9_000), SampleIndex::new(9_010)).unwrap(),
        row.sha256(),
    )
    .unwrap();
    assert!(matches!(
        read_segment(&fs, dir, &missing, SampleRate::SPEECH),
        Err(ReadSegmentError::Io(_))
    ));
}

#[test]
fn a_file_that_isnt_flac_gives_no_audio() {
    let fs = FakeFs::new();
    let dir = dir(&fs);
    let range = SampleRange::new(SampleIndex::new(0), SampleIndex::new(4)).unwrap();
    let bytes = b"not a flac file".to_vec();
    let mut file = fs
        .create(&dir.join(segment_file_name(TRACK, range)))
        .unwrap();
    file.write_all(&bytes).unwrap();
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    let row = SegmentRow::new(TRACK, EpochId::new(0), range, Sha256Digest::new(digest)).unwrap();
    let err = read_segment(&fs, dir, &row, SampleRate::SPEECH).unwrap_err();
    assert!(matches!(err, ReadSegmentError::Flac(_)), "{err}");
}

proptest::proptest! {
    /// Whatever a segment file holds, reading it gives its row's audio or
    /// an error, never a panic: here, a published segment with bytes
    /// overwritten, cut short or added to, under a row hashed to match.
    #[test]
    fn damaged_segment_files_are_errors_not_panics(
        len in 1_u64..9_000,
        edits in proptest::collection::vec((0_usize..20_000, proptest::num::u8::ANY), 0..8),
        cut in proptest::option::of(0_usize..20_000),
        extra in proptest::collection::vec(proptest::num::u8::ANY, 0..16),
    ) {
        let fs = FakeFs::new();
        let dir = dir(&fs);
        let audio = samples(len);
        let mut bytes = flac::encode(SampleRate::SPEECH, &[&audio]).unwrap();
        for (at, value) in edits {
            if let Some(byte) = bytes.get_mut(at) {
                *byte = value;
            }
        }
        if let Some(cut) = cut {
            bytes.truncate(cut);
        }
        bytes.extend(extra);
        let range = SampleRange::new(SampleIndex::new(0), SampleIndex::new(len)).unwrap();
        let mut file = fs.create(&dir.join(segment_file_name(TRACK, range))).unwrap();
        file.write_all(&bytes).unwrap();
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let row = SegmentRow::new(TRACK, EpochId::new(0), range, Sha256Digest::new(digest)).unwrap();
        if let Ok(read) = read_segment(&fs, dir, &row, SampleRate::SPEECH) {
            proptest::prop_assert_eq!(read.len() as u64, len);
        }
    }
}
