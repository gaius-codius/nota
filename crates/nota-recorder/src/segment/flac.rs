//! Encoding a segment's samples as FLAC.
//!
//! Segments are published as FLAC files. The encoder is `flacenc`, which is pure Rust: no native
//! code, so the recorder keeps building without a C toolchain and nothing outside Rust's safety
//! guarantees runs next to the capture path. The samples come in as the journal's frames (slices
//! of mono 16-bit PCM) and are fed to the encoder one block at a time, so the audio is never
//! copied into one buffer.
//!
//! # The audio digest
//!
//! A FLAC file's bytes depend on the encoder and its settings, so its
//! SHA-256 proves a file only while those stay the same. A segment row also
//! keeps a digest of the audio itself ([`AudioDigest`]): the SHA-256 of its
//! canonical PCM, which is, in order, all integers little-endian:
//!
//! | Bytes | Field |
//! |---|---|
//! | 16 | the tag `nota audio pcm 1` |
//! | 4 | the sample rate, in Hz |
//! | 4 | the track |
//! | 8 | the first sample |
//! | 8 | the sample after the last |
//! | 2 × each sample | the samples, mono 16-bit signed |
//!
//! A file proves a row with a digest if it decodes (claxon, pure Rust too)
//! to mono 16-bit audio whose canonical PCM, at the file's rate and the
//! row's track and range, has the row's digest.

use std::fmt;

use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::config::Encoder;
use flacenc::error::{SourceError, Verify};
use flacenc::source::{Fill, Source};
use nota_core::{SampleRange, SampleRate, TrackId};
use nota_store::AudioDigest;
use sha2::{Digest, Sha256};

/// Samples per FLAC block. Fixed, so the output depends only on the audio and the rate.
const BLOCK_SIZE: usize = 4096;

/// Why a segment couldn't be encoded.
#[derive(Debug)]
pub enum FlacError {
    /// No samples.
    Empty,
    /// The encoder refused (its message).
    Encode(String),
}

impl fmt::Display for FlacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a segment with no samples can't be encoded as FLAC"),
            Self::Encode(message) => write!(f, "the FLAC encoder refused the segment: {message}"),
        }
    }
}

impl std::error::Error for FlacError {}

/// Consecutive runs of samples, read out block by block.
struct Runs<'a> {
    rate: u32,
    pieces: &'a [&'a [i16]],
    /// Index of the piece being read.
    piece: usize,
    /// Samples of that piece already read.
    offset: usize,
    total: usize,
    block: Vec<i32>,
}

impl<'a> Runs<'a> {
    fn new(rate: SampleRate, pieces: &'a [&'a [i16]], total: usize) -> Self {
        Self {
            rate: rate.hz(),
            pieces,
            piece: 0,
            offset: 0,
            total,
            block: Vec::with_capacity(BLOCK_SIZE),
        }
    }
}

impl Source for Runs<'_> {
    fn channels(&self) -> usize {
        1
    }

    fn bits_per_sample(&self) -> usize {
        16
    }

    fn sample_rate(&self) -> usize {
        // A u32 always fits in usize on the targets nota supports; saturate rather than panic.
        usize::try_from(self.rate).unwrap_or(usize::MAX)
    }

    fn read_samples<F: Fill>(
        &mut self,
        block_size: usize,
        dest: &mut F,
    ) -> Result<usize, SourceError> {
        self.block.clear();
        let want = block_size.min(BLOCK_SIZE);
        while self.block.len() < want {
            let Some(piece) = self.pieces.get(self.piece) else {
                break;
            };
            let rest = piece.get(self.offset..).unwrap_or(&[]);
            if rest.is_empty() {
                self.piece += 1;
                self.offset = 0;
                continue;
            }
            let take = rest.len().min(want - self.block.len());
            self.block
                .extend(rest.iter().take(take).map(|&s| i32::from(s)));
            self.offset += take;
        }
        if self.block.is_empty() {
            return Ok(0);
        }
        dest.fill_interleaved(&self.block)?;
        Ok(self.block.len())
    }

    fn len_hint(&self) -> Option<usize> {
        Some(self.total)
    }
}

/// Encodes mono 16-bit samples at `rate` as a complete FLAC file. `pieces` are consecutive runs
/// of samples (the segment's journal frames, possibly sliced), encoded in order as one stream,
/// without first copying them into one buffer. The output depends only on the concatenated
/// samples and the rate: the same audio split differently gives the same bytes.
pub(super) fn encode(rate: SampleRate, pieces: &[&[i16]]) -> Result<Vec<u8>, FlacError> {
    let total: usize = pieces.iter().map(|piece| piece.len()).sum();
    if total == 0 {
        return Err(FlacError::Empty);
    }
    let config = Encoder::default()
        .into_verified()
        .map_err(|(_, e)| FlacError::Encode(e.to_string()))?;
    let source = Runs::new(rate, pieces, total);
    let mut stream = flacenc::encode_with_fixed_block_size(&config, source, BLOCK_SIZE)
        .map_err(|e| FlacError::Encode(e.to_string()))?;
    // A fixed-blocksize stream declares its block size as both the minimum
    // and the maximum; only the last block may be shorter. flacenc records a
    // lone short block's size instead, which is invalid under 16 samples and
    // makes strict decoders refuse a short segment.
    stream
        .stream_info_mut()
        .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
        .map_err(|e| FlacError::Encode(e.to_string()))?;
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| FlacError::Encode(e.to_string()))?;
    Ok(sink.as_slice().to_vec())
}

/// The canonical PCM's tag (see the module docs).
const PCM_TAG: &[u8; 16] = b"nota audio pcm 1";

/// Hashes canonical PCM, a run of samples at a time.
struct Pcm(Sha256);

impl Pcm {
    fn new(rate: u32, track: TrackId, range: SampleRange) -> Self {
        let mut sha = Sha256::new();
        sha.update(PCM_TAG);
        sha.update(rate.to_le_bytes());
        sha.update(track.get().to_le_bytes());
        sha.update(range.start().get().to_le_bytes());
        sha.update(range.end().get().to_le_bytes());
        Self(sha)
    }

    fn add(&mut self, sample: i16) {
        self.0.update(sample.to_le_bytes());
    }

    fn finish(self) -> AudioDigest {
        AudioDigest::new(self.0.finalize().into())
    }
}

/// The digest of `range` of `track`'s audio at `rate`: `pieces`, its
/// samples in order (see the module docs).
pub(super) fn audio_digest(
    rate: SampleRate,
    track: TrackId,
    range: SampleRange,
    pieces: &[&[i16]],
) -> AudioDigest {
    let mut pcm = Pcm::new(rate.hz(), track, range);
    for &sample in pieces.iter().copied().flatten() {
        pcm.add(sample);
    }
    pcm.finish()
}

/// The digest of the audio the FLAC file `bytes` decodes to, as `range` of
/// `track`: `None` if it doesn't decode, isn't mono 16-bit, or doesn't hold
/// exactly `range`'s number of samples.
pub(super) fn decoded_audio_digest(
    bytes: &[u8],
    track: TrackId,
    range: SampleRange,
) -> Option<AudioDigest> {
    let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes)).ok()?;
    let info = reader.streaminfo();
    if info.channels != 1 || info.bits_per_sample != 16 {
        return None;
    }
    let mut pcm = Pcm::new(info.sample_rate, track, range);
    let mut count = 0_u64;
    let want = range.len().get();
    for sample in reader.samples() {
        count += 1;
        if count > want {
            return None;
        }
        pcm.add(i16::try_from(sample.ok()?).ok()?);
    }
    (count == want).then(|| pcm.finish())
}

/// The number of samples a FLAC file declares in its STREAMINFO block, which
/// the format requires to come first: `None` if `bytes` don't start that way,
/// or the count is unknown (zero). Only the header is read; the hash of the
/// whole file is what proves the audio.
pub(super) fn stream_len(bytes: &[u8]) -> Option<u64> {
    // "fLaC", then a metadata block header whose type (low 7 bits of its
    // first byte) is 0, STREAMINFO, with a 34-byte body.
    let (magic, rest) = bytes.split_first_chunk::<4>()?;
    let (block, info) = rest.split_first_chunk::<4>()?;
    if magic != b"fLaC" || block[0] & 0x7F != 0 || block[1..] != [0, 0, 34] {
        return None;
    }
    // In the body: block sizes (4 bytes) and frame sizes (6), then 64 bits
    // of rate (20), channels (3), bits per sample (5) and total samples (36).
    let packed = u64::from_be_bytes(*info.get(10..18)?.first_chunk::<8>()?);
    match packed & 0xF_FFFF_FFFF {
        0 => None,
        n => Some(n),
    }
}

/// `samples` at `rate` encoded otherwise than [`encode`] does: blocks of
/// `block` samples. The same audio in other bytes, as another encoder or
/// its settings would write it.
#[cfg(test)]
pub(super) fn encode_otherwise(rate: SampleRate, samples: &[i16], block: usize) -> Vec<u8> {
    let config = Encoder::default().into_verified().unwrap();
    let source = flacenc::source::MemSource::from_samples(
        &samples.iter().map(|&s| i32::from(s)).collect::<Vec<_>>(),
        1,
        16,
        usize::try_from(rate.hz()).unwrap(),
    );
    let stream = flacenc::encode_with_fixed_block_size(&config, source, block).unwrap();
    let mut sink = ByteSink::new();
    stream.write(&mut sink).unwrap();
    sink.as_slice().to_vec()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn rate(hz: u32) -> SampleRate {
        SampleRate::new(hz).unwrap()
    }

    /// Deterministic, noisy-ish samples.
    fn samples(n: usize) -> Vec<i16> {
        let mut x: u32 = 12345;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let [_, _, hi, lo] = x.to_le_bytes();
                i16::from_le_bytes([hi, lo])
            })
            .collect()
    }

    fn split<'a>(data: &'a [i16], sizes: &[usize]) -> Vec<&'a [i16]> {
        let mut out = Vec::new();
        let mut at = 0;
        for &size in sizes {
            out.push(&data[at..at + size]);
            at += size;
        }
        out.push(&data[at..]);
        out
    }

    fn decode(bytes: Vec<u8>) -> (claxon::metadata::StreamInfo, Vec<i16>) {
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes)).unwrap();
        let info = reader.streaminfo();
        assert_eq!((info.min_block_size, info.max_block_size), (4096, 4096));
        let out = reader
            .samples()
            .map(|s| i16::try_from(s.unwrap()).unwrap())
            .collect();
        (info, out)
    }

    #[test]
    fn round_trips_at_block_boundaries() {
        for hz in [1_000, 16_000] {
            for total in [1, 15, 4095, 4096, 4097, 10_000] {
                let data = samples(total);
                let bytes = encode(rate(hz), &[&data]).unwrap();
                let (info, decoded) = decode(bytes.clone());
                assert_eq!(decoded, data, "hz {hz} total {total}");
                assert_eq!(info.channels, 1);
                assert_eq!(info.bits_per_sample, 16);
                assert_eq!(info.sample_rate, hz);
                assert_eq!(info.samples, Some(total as u64));
                assert_eq!(stream_len(&bytes), Some(total as u64));
            }
        }
        let data = samples(100);
        let (info, _) = decode(encode(SampleRate::SPEECH, &[&data]).unwrap());
        assert_eq!(info.sample_rate, 16_000);
    }

    #[test]
    fn splitting_does_not_change_the_bytes() {
        let data = samples(10_000);
        let whole = encode(rate(16_000), &[&data]).unwrap();
        let uneven = split(&data, &[1, 4095, 0, 3000]);
        assert_eq!(
            uneven.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [1, 4095, 0, 3000, 2904]
        );
        assert_eq!(encode(rate(16_000), &uneven).unwrap(), whole);
        let singles = split(&data, &[1; 50]);
        assert_eq!(singles.len(), 51);
        assert_eq!(encode(rate(16_000), &singles).unwrap(), whole);
        assert_eq!(decode(whole).1, data);
    }

    #[test]
    fn encoding_is_deterministic() {
        let data = samples(5_000);
        assert_eq!(
            encode(rate(16_000), &[&data]).unwrap(),
            encode(rate(16_000), &[&data]).unwrap()
        );
    }

    #[test]
    fn extreme_values_round_trip() {
        let alternating: Vec<i16> = (0..9_000)
            .map(|i| if i % 2 == 0 { i16::MIN } else { i16::MAX })
            .collect();
        let zeros = vec![0i16; 9_000];
        for data in [alternating, zeros] {
            let (_, decoded) = decode(encode(rate(16_000), &[&data]).unwrap());
            assert_eq!(decoded, data);
        }
    }

    #[test]
    fn no_samples_is_empty() {
        assert!(matches!(encode(rate(16_000), &[]), Err(FlacError::Empty)));
        let none: &[i16] = &[];
        assert!(matches!(
            encode(rate(16_000), &[none, none]),
            Err(FlacError::Empty)
        ));
    }

    #[test]
    fn errors_display() {
        assert!(!FlacError::Empty.to_string().is_empty());
        let shown = FlacError::Encode("boom".into()).to_string();
        assert!(shown.contains("boom"));
    }

    #[test]
    fn stream_len_refuses_what_isnt_a_flac_header() {
        let bytes = encode(rate(16_000), &[&samples(5_000)]).unwrap();
        assert_eq!(stream_len(&bytes), Some(5_000));
        assert_eq!(stream_len(&bytes[..25]), None);
        assert_eq!(stream_len(&bytes[..26]), Some(5_000));
        for at in [0, 3, 4, 5, 6, 7] {
            let mut bad = bytes.clone();
            bad[at] ^= 0x01;
            assert_eq!(stream_len(&bad), None, "byte {at}");
        }
        // The last-block flag (top bit) is fine.
        let mut last = bytes.clone();
        last[4] ^= 0x80;
        assert_eq!(stream_len(&last), Some(5_000));
        // An unknown count is no count.
        let mut unknown = bytes;
        unknown[21] &= 0xF0;
        unknown[22..26].fill(0);
        assert_eq!(stream_len(&unknown), None);
    }

    #[test]
    fn the_audio_digest_is_the_canonical_pcms_and_any_encoding_decodes_to_it() {
        let data = samples(10_000);
        let track = TrackId::new(1);
        let range = SampleRange::new(
            nota_core::SampleIndex::new(500),
            nota_core::SampleIndex::new(10_500),
        )
        .unwrap();
        let digest = audio_digest(rate(16_000), track, range, &[&data[..3], &data[3..]]);
        // As the module docs give it.
        let mut canonical = b"nota audio pcm 1".to_vec();
        canonical.extend_from_slice(&16_000_u32.to_le_bytes());
        canonical.extend_from_slice(&1_u32.to_le_bytes());
        canonical.extend_from_slice(&500_u64.to_le_bytes());
        canonical.extend_from_slice(&10_500_u64.to_le_bytes());
        for s in &data {
            canonical.extend_from_slice(&s.to_le_bytes());
        }
        assert_eq!(
            digest.as_bytes(),
            &<[u8; 32]>::from(Sha256::digest(&canonical))
        );

        let ours = encode(rate(16_000), &[&data]).unwrap();
        let other = encode_otherwise(rate(16_000), &data, 1_152);
        assert_ne!(ours, other);
        for bytes in [&ours, &other] {
            assert_eq!(decoded_audio_digest(bytes, track, range), Some(digest));
        }
        // Another track, range, rate or sample is another digest.
        let moved = SampleRange::new(
            nota_core::SampleIndex::new(501),
            nota_core::SampleIndex::new(10_501),
        )
        .unwrap();
        assert_ne!(
            decoded_audio_digest(&ours, TrackId::new(0), range),
            Some(digest)
        );
        assert_ne!(decoded_audio_digest(&ours, track, moved), Some(digest));
        assert_ne!(
            decoded_audio_digest(&encode(rate(8_000), &[&data]).unwrap(), track, range),
            Some(digest)
        );
        let mut changed = data;
        changed[9_999] ^= 1;
        assert_ne!(
            decoded_audio_digest(&encode(rate(16_000), &[&changed]).unwrap(), track, range),
            Some(digest)
        );
        // More or fewer samples than the range, or no FLAC at all: none.
        let short = SampleRange::new(
            nota_core::SampleIndex::new(500),
            nota_core::SampleIndex::new(10_499),
        )
        .unwrap();
        let long = SampleRange::new(
            nota_core::SampleIndex::new(500),
            nota_core::SampleIndex::new(10_501),
        )
        .unwrap();
        assert_eq!(decoded_audio_digest(&ours, track, short), None);
        assert_eq!(decoded_audio_digest(&ours, track, long), None);
        assert_eq!(decoded_audio_digest(b"not flac", track, range), None);
        assert_eq!(
            decoded_audio_digest(&ours[..ours.len() / 2], track, range),
            None
        );
    }

    proptest! {
        #[test]
        fn decoding_any_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            let range = SampleRange::new(
                nota_core::SampleIndex::new(0),
                nota_core::SampleIndex::new(10),
            )
            .unwrap();
            let mut flac = b"fLaC".to_vec();
            flac.extend_from_slice(&bytes);
            let _ = decoded_audio_digest(&flac, TrackId::new(0), range);
            let _ = decoded_audio_digest(&bytes, TrackId::new(0), range);
        }

        #[test]
        fn stream_len_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = stream_len(&bytes);
            let mut flac = b"fLaC\x00\x00\x00\x22".to_vec();
            flac.extend_from_slice(&bytes);
            let got = stream_len(&flac);
            if bytes.len() < 18 {
                prop_assert_eq!(got, None);
            } else {
                let packed = u64::from_be_bytes(bytes[10..18].try_into().unwrap());
                let n = packed & 0xF_FFFF_FFFF;
                prop_assert_eq!(got, (n != 0).then_some(n));
            }
        }
    }
}
