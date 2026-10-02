//! Writes the synthetic seed journals for the fuzz target into `fuzz/in/`.
//! Run from the `fuzz/` directory. Existing seed files are left alone.

use std::path::Path;
use std::sync::Arc;

use nota_core::{Clock, SampleIndex, SampleRate, SystemClock, TrackId};
use nota_recorder::fs::StdFs;
use nota_recorder::journal::JournalWriter;
use nota_recorder::journal::format::MAX_FRAME_SAMPLES;

fn tone(len: usize, step: i16) -> Vec<i16> {
    (0..len).map(|i| (i as i16).wrapping_mul(step)).collect()
}

fn writer(
    path: &Path,
) -> Result<JournalWriter<nota_recorder::fs::StdFile>, Box<dyn std::error::Error>> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::start()?);
    Ok(JournalWriter::create(
        &StdFs,
        path,
        SampleRate::SPEECH,
        clock,
    )?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new("in");
    std::fs::create_dir_all(dir)?;
    let mic = TrackId::new(0);
    let sys = TrackId::new(1);

    let path = dir.join("empty.journal");
    if !path.exists() {
        let mut w = writer(&path)?;
        w.sync()?;
    }

    let path = dir.join("one_track.journal");
    if !path.exists() {
        let mut w = writer(&path)?;
        w.start_track(mic, SampleIndex::new(0))?;
        for len in [10, 160, 1] {
            w.append(mic, &tone(len, 7))?;
        }
        w.sync()?;
    }

    let path = dir.join("two_tracks.journal");
    if !path.exists() {
        let mut w = writer(&path)?;
        w.start_track(mic, SampleIndex::new(0))?;
        w.start_track(sys, SampleIndex::new(480))?;
        for round in 0..3 {
            w.append(mic, &tone(64 + round, 3))?;
            w.append(sys, &tone(32 + round, 5))?;
        }
        w.sync()?;
    }

    let path = dir.join("split_append.journal");
    if !path.exists() {
        let mut w = writer(&path)?;
        w.start_track(mic, SampleIndex::new(0))?;
        w.append(mic, &tone(MAX_FRAME_SAMPLES as usize + 500, 11))?;
        w.sync()?;
    }
    Ok(())
}
