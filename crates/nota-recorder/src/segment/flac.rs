//! Encoding a segment's samples as FLAC.
//!
//! Segments are published as FLAC files. The encoder is `flacenc`, which is pure Rust: no native
//! code, so the recorder keeps building without a C toolchain and nothing outside Rust's safety
//! guarantees runs next to the capture path. The samples come in as the journal's frames (slices
//! of mono 16-bit PCM) and are fed to the encoder one block at a time, so the audio is never
//! copied into one buffer.

use std::fmt;

use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::config::Encoder;
use flacenc::error::{SourceError, Verify};
use flacenc::source::{Fill, Source};
use nota_core::SampleRate;

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

#[cfg(test)]
mod tests {
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
                let (info, decoded) = decode(bytes);
                assert_eq!(decoded, data, "hz {hz} total {total}");
                assert_eq!(info.channels, 1);
                assert_eq!(info.bits_per_sample, 16);
                assert_eq!(info.sample_rate, hz);
                assert_eq!(info.samples, Some(total as u64));
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
}
