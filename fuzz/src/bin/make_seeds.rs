//! Writes the synthetic seed journals for the fuzz target into `fuzz/in/`.
//! Run from the `fuzz/` directory. Existing seed files are left alone.
//!
//! The writer names each journal after its id, so each seed is written in
//! `seeds.tmp/`, then copied into `in/` under the seed's name. All file
//! operations go through the recorder's filesystem layer.

use std::io;
use std::path::Path;
use std::sync::Arc;

use nota_core::{
    Clock, Drift, EpochAnchor, EpochId, SampleIndex, SampleRate, SessionTime, SystemClock, TrackId,
};
use nota_recorder::fs::{Fs, FsFile, StdFile, StdFs};
use nota_recorder::journal::format::MAX_FRAME_SAMPLES;
use nota_recorder::journal::{JournalHeader, JournalId, JournalWriter};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// `len` samples of `i * step` for i = 0, 1, 2..., wrapping.
fn tone(len: usize, step: i16) -> Vec<i16> {
    std::iter::successors(Some(0_i16), |i| Some(i.wrapping_add(1)))
        .take(len)
        .map(|i| i.wrapping_mul(step))
        .collect()
}

/// Makes `dir` unless it's already there.
fn ensure_dir(dir: &Path) -> io::Result<()> {
    match StdFs.create_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// Writes the seed `name` in `dir` with `fill`, unless it already exists,
/// by way of the journal `header` names in `scratch`.
fn seed(
    dir: &Path,
    scratch: &Path,
    name: &str,
    header: JournalHeader,
    first: u64,
    fill: impl FnOnce(&mut JournalWriter<StdFile>) -> Result<()>,
) -> Result<()> {
    let target = dir.join(name);
    if target.exists() {
        return Ok(());
    }
    // A journal left over from an interrupted run would block the create.
    match StdFs.remove(&scratch.join(header.id().file_name())) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::start()?);
    let mut w = JournalWriter::create(&StdFs, scratch, header, SampleIndex::new(first), clock)?;
    fill(&mut w)?;
    let path = w.path().to_path_buf();
    w.finish()?;
    // The layer renames only within a directory: copy, then remove.
    let bytes = StdFs.read(&path)?;
    let mut out = StdFs.create(&target)?;
    out.write_all(&bytes)?;
    out.sync()?;
    StdFs.sync_dir(dir)?;
    StdFs.remove(&path)?;
    Ok(())
}

fn main() -> Result<()> {
    // The filesystem layer wants absolute paths.
    let here = std::env::current_dir()?;
    let (dir, scratch) = (&here.join("in"), &here.join("seeds.tmp"));
    ensure_dir(dir)?;
    ensure_dir(scratch)?;
    // Each journal's epoch starts at its first sample, `first`, a second
    // per epoch number into the session; a later epoch is a device 120 ppm
    // slow.
    let speech = |id, track, epoch: u32, first| {
        JournalHeader::new(
            JournalId::new(id),
            TrackId::new(track),
            EpochAnchor {
                id: EpochId::new(epoch),
                start: SessionTime::from_nanos(u64::from(epoch) * 1_000_000_000),
                first_sample: SampleIndex::new(first),
                rate: SampleRate::SPEECH,
                drift: if epoch > 0 {
                    Drift::from_ppb(-120_000).unwrap_or(Drift::ZERO)
                } else {
                    Drift::ZERO
                },
            },
        )
    };

    seed(dir, scratch, "empty.journal", speech(0, 0, 0, 0), 0, |_| {
        Ok(())
    })?;
    seed(
        dir,
        scratch,
        "one_track.journal",
        speech(0, 0, 0, 0),
        0,
        |w| {
            for len in [10, 160, 1] {
                w.append(&tone(len, 7))?;
            }
            Ok(())
        },
    )?;
    // Another track, a later epoch and journal, starting partway in.
    seed(
        dir,
        scratch,
        "later_journal.journal",
        speech(7, 1, 2, 480),
        480,
        |w| {
            for round in 0..3 {
                w.append(&tone(32 + round, 5))?;
            }
            Ok(())
        },
    )?;
    seed(
        dir,
        scratch,
        "split_append.journal",
        speech(1, 0, 0, 0),
        0,
        |w| {
            w.append(&tone(MAX_FRAME_SAMPLES as usize + 500, 11))?;
            Ok(())
        },
    )?;
    older_copy(dir, "one_track.journal", "untimed.journal", 2)?;
    older_copy(dir, "later_journal.journal", "undrifted.journal", 3)?;
    Ok(())
}

/// Bytes in a version 4 header.
const HEADER: usize = 54;

/// Writes the seed `name` in `dir` as the journal of `version` an older
/// nota wrote: the seed `from` with its header cut to the fields that
/// version had (version 3 has no drift, bytes 46..50 of version 4;
/// version 2 no anchor, bytes 30..50), and the CRC over the fields left.
/// The frames are as they were.
fn older_copy(dir: &Path, from: &str, name: &str, version: u16) -> Result<()> {
    let target = dir.join(name);
    if target.exists() {
        return Ok(());
    }
    let fields = match version {
        3 => 46,
        2 => 30,
        _ => return Err(format!("no older version {version}").into()),
    };
    let timed = StdFs.read(&dir.join(from))?;
    let (Some(kept), Some(frames)) = (timed.get(..fields), timed.get(HEADER..)) else {
        return Err(format!("{from} is shorter than a header").into());
    };
    let mut bytes = kept.to_vec();
    bytes[8..10].copy_from_slice(&version.to_le_bytes());
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes.extend_from_slice(frames);
    let mut out = StdFs.create(&target)?;
    out.write_all(&bytes)?;
    out.sync()?;
    StdFs.sync_dir(dir)?;
    Ok(())
}
