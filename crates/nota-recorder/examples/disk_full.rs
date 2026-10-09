//! The workload for `scripts/disk-full.sh`: a recording that fills a real
//! disk.
//!
//! `disk_full <dir> [--no-ballast] [--checks]` keeps an 8 MiB ballast in
//! `<dir>`,
//! then records two tracks of 16 kHz noise into `<dir>/session` as fast as
//! it can, as `nota record` does: through a watched [`StdFs`], fsyncs on a
//! thread per track, rows committed to SQLite at `<dir>/library.db`, and
//! the disk monitor. The monitor checks once at the start, so the write
//! that meets the full disk is what notices it; with `--checks` it checks
//! every 50 ms too, so a check may notice first. It publishes each finished
//! journal as it goes, keeping what fails for the next try, as the
//! publisher does: recording in real time, the publisher keeps up, so no
//! more than a window a track waits for it. The ballast is scaled down
//! from nota's 256 MB, but kept larger than what SQLite's write-ahead log
//! grows to before its first checkpoint (about 4 MB). Once the disk is full
//! it stops, as `nota record` does, and checks:
//! - the ballast was freed;
//! - the recording lost nothing: every sample it took is in a published
//!   segment whose row is committed, decoded and compared;
//! - nothing is left for salvage: no journals, no temp files.
//!
//! It prints what filled the disk and what was published, and exits
//! non-zero if a check fails. With `--no-ballast` it keeps none, and only
//! reports what became of the recording. Keep `<dir>` on a small
//! filesystem of its own: it's filled.

use std::collections::BTreeMap;
use std::error::Error;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRate, SessionId, SessionTime,
    TrackId,
};
use nota_recorder::disk::{
    BALLAST_FILE_NAME, Ballast, DiskMonitor, DiskReport, DiskWatch, Freed, Full, MonitorConfig,
    Usage,
};
use nota_recorder::fs::{Fs, StdFs};
use nota_recorder::segment::{SegmentLength, publish_journals, segment_file_name};
use nota_recorder::session::{FinishedJournal, SessionDir, SessionStore, SessionWriter, Syncing};
use nota_store::{NewSession, Store};

type Res<T> = Result<T, Box<dyn Error>>;

const SESSION: SessionId = SessionId::new(1);
const TRACKS: [TrackId; 2] = [TrackId::new(0), TrackId::new(1)];
/// Samples a track takes per round: a tenth of a second.
const CHUNK: u64 = 1_600;
/// The ballast: 8 MiB.
const BALLAST: u64 = 8 << 20;

/// Two-second windows, so segments publish often.
fn length() -> Res<SegmentLength> {
    SegmentLength::new(SampleCount::new(32_000)).ok_or_else(|| "bad length".into())
}

/// Noise, so FLAC can't shrink it: distinct per track and position.
fn sample(track: TrackId, index: u64) -> i16 {
    let mut x = index
        .wrapping_add(u64::from(track.get()) << 40)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 29;
    i16::from_le_bytes([x.to_le_bytes()[0], x.to_le_bytes()[1]])
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flags: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let result = match args.first() {
        Some(dir)
            if flags
                .iter()
                .all(|f| ["--no-ballast", "--checks"].contains(f)) =>
        {
            run(
                Path::new(dir),
                !flags.contains(&"--no-ballast"),
                if flags.contains(&"--checks") {
                    Duration::from_millis(50)
                } else {
                    Duration::from_secs(3_600)
                },
            )
        }
        _ => Err("usage: disk_full <dir> [--no-ballast] [--checks]".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Nothing more can be done if stderr is gone.
            let _ = writeln!(io::stderr(), "disk_full: {e}");
            ExitCode::FAILURE
        }
    }
}

fn say(line: &str) -> Res<()> {
    writeln!(io::stdout(), "{line}")?;
    Ok(())
}

/// How the recording went, for the checks.
struct Recorded {
    /// The samples each track took.
    took: SampleIndex,
    rounds: u64,
    /// Appends and syncs that lost audio.
    failures: Vec<String>,
    /// Publish runs that failed (and were tried again).
    errors: Vec<String>,
    /// Journals still unpublished at the end.
    left: usize,
    finishing: Option<String>,
}

fn run(dir: &Path, with_ballast: bool, interval: Duration) -> Res<()> {
    let session_dir = dir.join("session");
    StdFs.create_dir(&session_dir)?;
    StdFs.sync_dir(dir)?;
    let mut store = Store::open(&dir.join("library.db"))?;
    store.create_session(&NewSession {
        id: SESSION,
        title: None,
        language: None,
        tracks: vec![],
    })?;
    let watch = DiskWatch::new(StdFs);
    if with_ballast {
        let ballast = Ballast::keep(&StdFs, dir, BALLAST, || false)?
            .ok_or("no room for the ballast: give the filesystem more than 16 MiB")?;
        watch.hold(ballast);
    }
    let reports = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&reports);
    let monitor = DiskMonitor::spawn(
        Arc::clone(&watch),
        MonitorConfig {
            data_dir: dir.to_path_buf(),
            audio_dir: session_dir.clone(),
            usage: Usage::new(TRACKS.len(), SampleRate::SPEECH, length()?),
            // Already kept, or not wanted.
            ballast_len: u64::MAX / 4,
            interval,
        },
        move |report| {
            if let Ok(mut seen) = seen.lock() {
                seen.push(report);
            }
        },
    )?;
    let recorded = record(&watch, &session_dir, store)?;
    let summary = monitor.stop()?;
    let full = summary.full.ok_or("the monitor missed the full disk")?;
    report(&full, &recorded, &reports)?;
    if !with_ballast {
        return Ok(());
    }
    let problems = check(dir, &session_dir, &full, &recorded)?;
    if problems.is_empty() {
        say("ok: the ballast was freed, the segments finished, and no audio was lost")
    } else {
        Err(problems.join("\n").into())
    }
}

/// Records until the disk is full, publishing as it goes; then stops and
/// publishes the rest, as `nota record` does.
fn record(watch: &Arc<DiskWatch<StdFs>>, session_dir: &Path, store: Store) -> Res<Recorded> {
    let (rate, length) = (SampleRate::SPEECH, length()?);
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let dyn_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    let lock = SessionDir::new(SESSION, watch.fs(), session_dir).lock()?;
    let mut writer =
        SessionWriter::open(&lock, rate, length, dyn_clock)?.with_syncing(Syncing::Threads);
    let mut store = SessionStore::new(lock, store);
    let mut pending: Vec<FinishedJournal> = Vec::new();
    let mut errors = Vec::new();
    // As the publisher does: what's still on disk after a run is tried
    // again with the next.
    let mut publish = |pending: &mut Vec<FinishedJournal>| -> Res<()> {
        if pending.is_empty() {
            return Ok(());
        }
        if let Err(e) = publish_journals(&mut store, length, pending) {
            errors.push(e.to_string());
        }
        let there = StdFs.list(session_dir)?;
        pending.retain(|j| there.contains(&session_dir.join(j.id().file_name())));
        Ok(())
    };
    for track in TRACKS {
        writer.start_track(track, EpochId::new(0), SampleIndex::ZERO)?;
    }
    let mut failures = Vec::new();
    let mut rounds = 0_u64;
    while watch.full().is_none() {
        for track in TRACKS {
            let from = writer.next_sample(track).ok_or("track not started")?.get();
            let audio: Vec<i16> = (from..from + CHUNK).map(|i| sample(track, i)).collect();
            if let Err(e) = writer.append(track, &audio) {
                failures.push(e.to_string());
            }
        }
        clock.advance(
            SampleCount::new(CHUNK)
                .duration_at(rate)
                .ok_or("duration overflow")?,
        );
        if let Err(e) = writer.sync_if_due() {
            failures.push(e.to_string());
        }
        pending.extend(writer.take_finished());
        publish(&mut pending)?;
        rounds += 1;
        if rounds > 1_000_000 {
            return Err("the disk never filled".into());
        }
    }
    let took = writer.next_sample(TRACKS[0]).ok_or("track not started")?;
    let finishing = match writer.finish() {
        Ok(last) => {
            pending.extend(last);
            None
        }
        Err(e) => {
            let (e, last) = e.into_parts();
            pending.extend(last);
            Some(e.to_string())
        }
    };
    // The last batch, and once more, as the publisher finishes.
    publish(&mut pending)?;
    publish(&mut pending)?;
    Ok(Recorded {
        took,
        rounds,
        failures,
        errors,
        left: pending.len(),
        finishing,
    })
}

/// Says what filled the disk and what became of the recording.
fn report(full: &Full, recorded: &Recorded, reports: &Mutex<Vec<DiskReport>>) -> Res<()> {
    let filled = full
        .path
        .as_ref()
        .map_or_else(|| "a check".to_owned(), |p| p.display().to_string());
    say(&format!(
        "full: {filled}; ballast {:?}; {} rounds, {} samples a track",
        full.ballast,
        recorded.rounds,
        recorded.took.get()
    ))?;
    say(&format!(
        "{} journals left; finishing: {}; journal failures: {}",
        recorded.left,
        recorded.finishing.as_deref().unwrap_or("ok"),
        recorded.failures.len()
    ))?;
    for e in &recorded.errors {
        say(&format!("publish error (tried again): {e}"))?;
    }
    let warned = reports
        .lock()
        .map_err(|_| "a report thread panicked")?
        .iter()
        .filter(|r| matches!(r, DiskReport::Full(_)))
        .count();
    if warned == 1 {
        Ok(())
    } else {
        Err(format!("the full disk was reported {warned} times, not once").into())
    }
}

/// What went wrong, if anything: the ballast still there, audio lost, a
/// segment not finished.
fn check(dir: &Path, session_dir: &Path, full: &Full, recorded: &Recorded) -> Res<Vec<String>> {
    let mut problems = Vec::new();
    if full.ballast != Freed::Freed {
        problems.push(format!("the ballast wasn't freed: {:?}", full.ballast));
    }
    if StdFs.list(dir)?.contains(&dir.join(BALLAST_FILE_NAME)) {
        problems.push("the ballast is still there".to_owned());
    }
    if let Some(first) = recorded.failures.first() {
        problems.push(format!(
            "{} appends lost audio, first: {first}",
            recorded.failures.len()
        ));
    }
    if let Some(e) = &recorded.finishing {
        problems.push(format!("finishing failed: {e}"));
    }
    if recorded.left > 0 {
        problems.push(format!("{} journals weren't published", recorded.left));
    }
    let left: Vec<PathBuf> = StdFs
        .list(session_dir)?
        .into_iter()
        .filter(|p| {
            let journal = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("journal-"));
            journal || p.extension().is_some_and(|e| e == "tmp")
        })
        .collect();
    if !left.is_empty() {
        problems.push(format!("left for salvage: {left:?}"));
    }
    problems.extend(check_segments(dir, session_dir, recorded.took)?);
    Ok(problems)
}

/// Every sample each track took, from zero to `took`, in a committed row's
/// segment, read back from the disk and decoded.
fn check_segments(dir: &Path, session_dir: &Path, took: SampleIndex) -> Res<Vec<String>> {
    let rows = Store::open(&dir.join("library.db"))?.segments(SESSION)?;
    let mut problems = Vec::new();
    let mut next: BTreeMap<TrackId, u64> = TRACKS.iter().map(|&t| (t, 0)).collect();
    for row in &rows {
        let track = row.track();
        let range = row.range();
        let at = next.entry(track).or_insert(0);
        if range.start().get() != *at {
            problems.push(format!(
                "track {} jumps from {at} to {}",
                track.get(),
                range.start().get()
            ));
        }
        *at = range.end().get();
        let path = session_dir.join(segment_file_name(track, range));
        let bytes = StdFs.read(&path)?;
        let mut reader = claxon::FlacReader::new(io::Cursor::new(bytes))?;
        let decoded: Vec<i32> = reader.samples().collect::<Result<_, _>>()?;
        let expected: Vec<i32> = (range.start().get()..range.end().get())
            .map(|i| i32::from(sample(track, i)))
            .collect();
        if decoded != expected {
            problems.push(format!("{} doesn't hold its samples", path.display()));
        }
    }
    for (track, end) in next {
        if end != took.get() {
            problems.push(format!(
                "track {} published up to {end} of {}",
                track.get(),
                took.get()
            ));
        }
    }
    Ok(problems)
}
