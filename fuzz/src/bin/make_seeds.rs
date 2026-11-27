//! Writes the synthetic seeds for the fuzz targets: journals for
//! `journal_reader` into `fuzz/in/`, and engine-protocol frames for
//! `engine_protocol` into `fuzz/in-protocol/`. Run from the `fuzz/`
//! directory. Existing seed journals are left alone; the protocol seeds are
//! written afresh each run, so they always carry the current protocol
//! version.
//!
//! The writer names each journal after its id, so each seed is written in
//! `seeds.tmp/`, then copied into `in/` under the seed's name. The protocol
//! seeds are encoded with `nota_core::protocol::encode`. All file
//! operations go through the recorder's filesystem layer.

use std::io;
use std::path::Path;
use std::sync::Arc;

use nota_core::messages::{FromEngine, HeardWord, ProtocolVersion, Transcript};
use nota_core::protocol::{Frame, FrameReader, encode};
use nota_core::{
    Clock, Drift, EpochAnchor, EpochId, LogProb, SampleIndex, SampleRange, SampleRate, SessionTime,
    SystemClock, TrackId,
};
use nota_recorder::fs::{Fs, FsFile, StdFile, StdFs};
use nota_recorder::journal::format::{HEADER_LEN, MAX_FRAME_SAMPLES};
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
    protocol_seeds(&here.join("in-protocol"))
}

/// Writes the engine-protocol seeds in `dir`: the current version's
/// handshake, and a transcript of "hi yo" over samples 0..16 whose first
/// word has the log-probability −0.5 and whose second has none, so a run
/// reaches the word fields from the start.
fn protocol_seeds(dir: &Path) -> Result<()> {
    ensure_dir(dir)?;
    frame_seed(dir, "hello", &Frame::Hello(ProtocolVersion::CURRENT))?;

    let range = |start, end| {
        SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).ok_or("a reversed range")
    };
    let hi = HeardWord::new("hi".to_owned(), range(0, 8)?)
        .ok_or("a word without text")?
        .with_log_prob(Some(LogProb::new(-0.5).ok_or("not a log-probability")?));
    let yo = HeardWord::new("yo".to_owned(), range(8, 16)?).ok_or("a word without text")?;
    let transcript = Transcript::new(TrackId::new(0), range(0, 16)?, "hi yo".to_owned())
        .ok_or("a transcript over no samples")?
        .with_words(vec![hi, yo])
        .map_err(|_| "words outside the transcript")?;
    frame_seed(
        dir,
        "transcript",
        &Frame::Message(FromEngine::Transcript(transcript)),
    )
}

/// Writes `frame`, encoded, to `name` in `dir`, in place of anything
/// there. Fails if the protocol's reader doesn't read the bytes back as
/// exactly that one frame: AFL takes a seed the target refuses without a
/// word, so a bad seed fails here instead.
fn frame_seed(dir: &Path, name: &str, frame: &Frame<FromEngine>) -> Result<()> {
    let bytes = encode(frame).map_err(|e| format!("the {name} seed doesn't encode: {e}"))?;
    let mut input = bytes.as_slice();
    let read = FrameReader::new(&mut input)
        .read_frame::<FromEngine>()
        .map_err(|e| format!("the {name} seed doesn't read back: {e}"))?;
    if read.as_ref() != Some(frame) || !input.is_empty() {
        return Err(format!("the {name} seed doesn't read back as its frame").into());
    }
    let target = dir.join(name);
    match StdFs.remove(&target) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let mut out = StdFs.create(&target)?;
    out.write_all(&bytes)?;
    out.sync()?;
    StdFs.sync_dir(dir)?;
    Ok(())
}

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
    // A seed left by a run before version 4 would cut wrongly.
    if timed.get(8..10) != Some(&4_u16.to_le_bytes()[..]) {
        return Err(format!("{from} isn't a version 4 journal: delete in/ and run again").into());
    }
    let (Some(kept), Some(frames)) = (timed.get(..fields), timed.get(HEADER_LEN..)) else {
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
