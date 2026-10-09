//! Reading a published segment's audio back, for the final pass.
//!
//! A row says its file is durable, and its SHA-256 says what the file
//! holds: the file is read whole, checked against the row's hash, and
//! decoded, and the samples must be exactly the row's, at the rate the
//! caller expects. A file that fails any check gives no audio.

use std::fmt;
use std::io;
use std::path::Path;

use nota_core::SampleRate;
use nota_store::SegmentRow;
use sha2::{Digest, Sha256};

use super::{flac, segment_file_name};
use crate::fs::Fs;

/// Why a published segment's audio couldn't be read back.
#[derive(Debug)]
pub enum ReadSegmentError {
    /// The file couldn't be read.
    Io(io::Error),
    /// The file isn't what its row says was published: its hash differs.
    Hash,
    /// The file doesn't decode as mono 16-bit FLAC (why).
    Flac(String),
    /// It decodes, but at another rate than expected.
    Rate(SampleRate),
    /// It decodes to another number of samples than its row covers.
    Length {
        /// The row's.
        expected: u64,
        /// The file's.
        found: u64,
    },
}

impl fmt::Display for ReadSegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "couldn't be read: {e}"),
            Self::Hash => write!(f, "doesn't match its row's SHA-256"),
            Self::Flac(why) => write!(f, "doesn't decode: {why}"),
            Self::Rate(rate) => write!(f, "is at {} Hz", rate.hz()),
            Self::Length { expected, found } => {
                write!(f, "holds {found} samples, its row {expected}")
            }
        }
    }
}

impl std::error::Error for ReadSegmentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// The samples of `row`'s segment file in the session directory `dir`,
/// which holds them at `rate`: one per sample of the row's range.
///
/// # Errors
///
/// [`ReadSegmentError`] if the file can't be read, doesn't match its
/// row's hash, doesn't decode, or holds other audio than its row says.
pub fn read_segment<S: Fs>(
    fs: &S,
    dir: &Path,
    row: &SegmentRow,
    rate: SampleRate,
) -> Result<Vec<i16>, ReadSegmentError> {
    let path = dir.join(segment_file_name(row.track(), row.range()));
    let bytes = fs.read(&path).map_err(ReadSegmentError::Io)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    if &digest != row.sha256().as_bytes() {
        return Err(ReadSegmentError::Hash);
    }
    let (found_rate, samples) = flac::decode(&bytes).map_err(ReadSegmentError::Flac)?;
    if found_rate != rate {
        return Err(ReadSegmentError::Rate(found_rate));
    }
    let found = samples.len() as u64;
    let expected = row.range().len().get();
    if found != expected {
        return Err(ReadSegmentError::Length { expected, found });
    }
    Ok(samples)
}

#[cfg(test)]
mod tests;
