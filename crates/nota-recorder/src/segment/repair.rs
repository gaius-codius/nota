//! Repairing a committed row whose file is missing or doesn't prove it,
//! from the journals that still hold its audio.
//!
//! A repair puts a file under the row's name that proves the row, and
//! changes nothing else: the row stays as committed. It runs only with
//! proof, and never loses a file. The steps, each a type only the step
//! before can lead to:
//!
//! 1. [`prove`]: the journals hold every sample of the row's track, epoch
//!    and range, and the audio rebuilt from them is the row's: its digest
//!    is the row's audio digest, or, for a row from before audio digests,
//!    the rebuilt FLAC's SHA-256 is the row's. That's a [`VerifiedRepair`].
//! 2. The row's name is cleared ([`Cleared`]): either nothing is there
//!    ([`confirm_absent`]), or the file there is kept ([`preserve`]):
//!    fsync'd, renamed to a name nothing has (`<name>.mismatched`, or with
//!    `.1`, `.2`, … after it), and the directory fsync'd. It's never
//!    deleted or renamed over.
//! 3. [`install`]: the rebuilt file is published under the row's name as
//!    any segment is (temp file, fsync, rename, directory fsync).
//!
//! A crash between any two steps leaves the old state or the repaired one:
//! the row's journals are kept until the row claims its samples, so the
//! next run finds the row's file missing (or still mismatched) and repairs
//! again; a file kept aside stays kept, and a second preserve picks another
//! free name. Salvage removes a leftover temp file as for any segment.

use std::io;
use std::path::{Path, PathBuf};

use nota_store::{AudioDigest, SegmentRow};
use sha2::{Digest, Sha256};

use super::plan::PlannedSegment;
use super::publish::{StepError, TempSegment};
use super::segment_file_name;
use crate::fs::Fs;

/// What's appended to a row's file name to keep a file that didn't prove
/// it aside.
pub(super) const MISMATCHED: &str = ".mismatched";

/// A segment rebuilt from journals, proven to hold exactly a row's audio.
/// Only [`prove`] makes one.
#[derive(Debug)]
pub(super) struct VerifiedRepair {
    row: SegmentRow,
    flac: Vec<u8>,
    audio: AudioDigest,
}

impl VerifiedRepair {
    /// The row it repairs.
    pub(super) const fn row(&self) -> &SegmentRow {
        &self.row
    }
}

/// `segment`, encoded as `flac` with audio digest `audio`, as a repair of
/// `row`, if it proves the row: the same track, epoch and range, and the
/// row's audio digest, or for a row without one, the row's SHA-256.
pub(super) fn prove(
    row: &SegmentRow,
    segment: &PlannedSegment,
    flac: Vec<u8>,
    audio: AudioDigest,
) -> Option<VerifiedRepair> {
    let same_place = segment.track == row.track()
        && segment.epoch == row.epoch()
        && segment.range == row.range();
    let same_audio = match row.audio() {
        Some(digest) => digest == audio,
        None => <[u8; 32]>::from(Sha256::digest(&flac)) == *row.sha256().as_bytes(),
    };
    (same_place && same_audio).then_some(VerifiedRepair {
        row: *row,
        flac,
        audio,
    })
}

/// The row's name, cleared for its rebuilt file. Only [`confirm_absent`]
/// and [`preserve`] make one.
#[derive(Debug)]
pub(super) enum Cleared {
    /// Nothing was under the name.
    Absent,
    /// The file under the name is durably kept under this one.
    Preserved(PathBuf),
}

/// Where `row`'s file is in `dir`.
fn path_of(dir: &Path, row: &SegmentRow) -> PathBuf {
    dir.join(segment_file_name(row.track(), row.range()))
}

/// [`Cleared::Absent`] if nothing is under `row`'s name in `dir`, as
/// listed now; `None` if something is.
///
/// # Errors
///
/// Any I/O error listing the directory.
pub(super) fn confirm_absent<S: Fs>(
    fs: &S,
    dir: &Path,
    row: &SegmentRow,
) -> io::Result<Option<Cleared>> {
    let path = path_of(dir, row);
    Ok((!fs.list(dir)?.contains(&path)).then_some(Cleared::Absent))
}

/// Keeps the file under `row`'s name in `dir`: fsyncs it, renames it to
/// the first of `<name>.mismatched`, `<name>.mismatched.1`, … that nothing
/// in `dir` has, and fsyncs the directory. Nothing is deleted or replaced.
///
/// # Errors
///
/// Any I/O error; the file is then under its own name or the new one,
/// never lost.
pub(super) fn preserve<S: Fs>(fs: &S, dir: &Path, row: &SegmentRow) -> io::Result<Cleared> {
    let path = path_of(dir, row);
    fs.sync_file(&path)?;
    let aside = free_aside(&path, &fs.list(dir)?);
    fs.rename(&path, &aside)?;
    fs.sync_dir(dir)?;
    Ok(Cleared::Preserved(aside))
}

/// The first of `<path>.mismatched`, `<path>.mismatched.1`, … not in
/// `taken`.
fn free_aside(path: &Path, taken: &[PathBuf]) -> PathBuf {
    let base = {
        let mut name = path.as_os_str().to_os_string();
        name.push(MISMATCHED);
        PathBuf::from(name)
    };
    let mut aside = base.clone();
    let mut n = 0_u64;
    while taken.contains(&aside) {
        n += 1;
        let mut name = base.as_os_str().to_os_string();
        name.push(format!(".{n}"));
        aside = PathBuf::from(name);
    }
    aside
}

/// Whether `name` is a file kept aside by [`preserve`]: a segment's file
/// name with [`MISMATCHED`] after it, and maybe `.N`.
pub(super) fn is_kept_aside(name: &str) -> bool {
    let Some((segment, rest)) = name.split_once(MISMATCHED) else {
        return false;
    };
    let numbered = match rest.strip_prefix('.') {
        None => rest.is_empty(),
        Some(n) => n.parse::<u64>().is_ok_and(|v| v.to_string() == n),
    };
    numbered && super::segment_in_file_name(std::ffi::OsStr::new(segment)).is_some()
}

/// A row whose file a repair has put back. Only [`install`] makes one.
#[derive(Debug)]
pub(super) struct Repaired {
    row: SegmentRow,
    kept: Option<PathBuf>,
}

impl Repaired {
    /// The row, which its file now proves.
    pub(super) const fn row(&self) -> &SegmentRow {
        &self.row
    }

    /// Where the file that was under its name is kept, if there was one.
    pub(super) fn kept(&self) -> Option<&Path> {
        self.kept.as_deref()
    }
}

/// Step 3: publishes `repair`'s file under its row's name in `dir`, which
/// `cleared` says is free, durably.
///
/// # Errors
///
/// As the publish steps: [`StepError::Name`] if a name it needs can't be
/// used, else [`StepError::Io`]. The row's name then holds nothing, or the
/// rebuilt file not yet durably, and the next run repairs again.
pub(super) fn install<S: Fs>(
    fs: &S,
    dir: &Path,
    repair: VerifiedRepair,
    cleared: Cleared,
) -> Result<Repaired, StepError> {
    let VerifiedRepair { row, flac, audio } = repair;
    TempSegment::write(fs, dir, row.track(), row.epoch(), row.range(), &flac, audio)?
        .sync()
        .map_err(StepError::Io)?
        .rename(fs)?
        .sync_dir(fs)
        .map_err(StepError::Io)?;
    // The file proves the row; the row itself stays as committed, so the
    // durable segment's own row (whose SHA-256 may differ) isn't committed.
    Ok(Repaired {
        row,
        kept: match cleared {
            Cleared::Absent => None,
            Cleared::Preserved(aside) => Some(aside),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aside_names_are_free_and_recognised() {
        let path = Path::new("/s/seg-t0-000000000000.flac");
        assert_eq!(
            free_aside(path, &[]),
            Path::new("/s/seg-t0-000000000000.flac.mismatched")
        );
        let taken = [
            PathBuf::from("/s/seg-t0-000000000000.flac.mismatched"),
            PathBuf::from("/s/seg-t0-000000000000.flac.mismatched.1"),
        ];
        let next = free_aside(path, &taken);
        assert_eq!(next, Path::new("/s/seg-t0-000000000000.flac.mismatched.2"));
        for name in [
            "seg-t0-000000000000.flac.mismatched",
            "seg-t0-000000000000.flac.mismatched.2",
        ] {
            assert!(is_kept_aside(name), "{name}");
        }
        for name in [
            "seg-t0-000000000000.flac",
            "seg-t0-000000000000.flac.mismatched.",
            "seg-t0-000000000000.flac.mismatched.02",
            "seg-t0-000000000000.flac.mismatched.x",
            "seg-t0-0.flac.mismatched",
            "other.mismatched",
        ] {
            assert!(!is_kept_aside(name), "{name}");
        }
    }
}
