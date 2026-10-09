//! The workload and the checker for `scripts/lazyfs-crash.sh`, which runs
//! the recorder's crash checks on a real filesystem through `LazyFS`.
//!
//! - `write <dir> <promises> [--stop-after N] [--no-publish]` records two
//!   tracks with live publishing into `<dir>/session`, with the library
//!   database at `<dir>/library.db`. With `--no-publish` it publishes
//!   nothing, leaving every finished journal for salvage.
//!   Every recorder filesystem operation that changes the disk (create,
//!   mkdir, write, fsync, rename, directory fsync, remove) is counted. The
//!   journals are fsync'd on a thread per track, as `nota record` does it,
//!   so the count's order can vary a little from run to run. After the
//!   Nth, it creates `<promises>.stopped` and every thread blocks forever
//!   before its next operation, so the script can SIGKILL it at that point;
//!   an operation another thread already had under way may still land. Whenever the recorder
//!   reports something durable (a track's durable position, committed
//!   rows), it appends a line to `<promises>` and fsyncs it. Keep
//!   `<promises>` off the filesystem under test. Without `--stop-after` it
//!   runs to the end and prints `ops <total>`.
//! - `prepare-repair <dir> <promises> missing|mismatched` makes committed
//!   rows with their journals retained, then removes or replaces one file.
//!   A mismatched case plants two occupied aside names and logs their hashes.
//! - `probe-rename <dir>` checks that the filesystem supports no-replace
//!   renames and refuses to overwrite an occupied name.
//! - `check <dir> <promises> [--recovered] [--end <file>] [--repair]
//!   [--expect-repair] [--stop-after N]` checks the invariants the in-memory
//!   crash tests check (`src/segment/tests.rs`): before salvage,
//!   every row has its file and every promised sample is in a row or a
//!   journal; after salvage, only segments and rows are left, holding every
//!   promised sample and every promised row; a second salvage changes
//!   nothing. With `--recovered`, it also requires that nothing is left to
//!   salvage, for a run after a crash that followed a completed salvage.
//!   With `--end`, it writes where the first salvage ended (each session
//!   file's name and hash, and the rows) to `<file>`, so the script can
//!   compare a salvage that was interrupted with one that wasn't. `--repair`
//!   admits broken rows before salvage when journals still hold their audio;
//!   all rows must prove their audio afterwards. `--expect-repair` requires
//!   the reference run to repair one row. `--stop-after` counts recovery's
//!   recorder operations and uses the same stop marker as `write`.
//!
//! SQLite's own I/O isn't counted: `--stop-after` crash points fall between
//! the recorder's operations, and the store commits in between them. The
//! script reaches inside SQLite's commits, and tears the recorder's writes,
//! by having `LazyFS` inject the fault and crash itself while `write` runs
//! without `--stop-after`; the writer's calls then fail, and the script
//! kills it. It crashes salvage the same way, while `check` runs on a copy
//! of what `write --no-publish` left.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRate, SessionId, SessionTime,
    TrackId,
};
use nota_recorder::fs::{FileSyncer, Fs, FsFile, StdFile, StdFs, StdLock, StdSyncer, Synced};
use nota_recorder::journal::{JournalId, read_journal};
use nota_recorder::segment::{
    FINDINGS_FILE_NAME, Published, SegmentLength, publish_journals, salvage, segment_file_name,
};
use nota_recorder::session::{MARKS_FILE_NAME, SessionDir, SessionStore, SessionWriter, Syncing};
use nota_store::{NewSession, SegmentRow, Store, StoreError};
use sha2::{Digest, Sha256};

type Res<T> = Result<T, Box<dyn Error>>;

/// The one session every run records; the store is private to it.
const SESSION: SessionId = SessionId::new(1);
const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);
const TRACKS: [(TrackId, u64); 2] = [(MIC, 0), (SYSTEM, 700)];
/// Rounds of audio on both tracks.
const STEPS: usize = 12;
/// Chunk sizes, in samples, cycled through: some longer than a window.
const SIZES: [u64; 5] = [250, 100, 400, 1_600, 50];

/// A low rate keeps it quick: a second is 1,000 samples.
fn rate() -> Res<SampleRate> {
    SampleRate::new(1_000).ok_or_else(|| "bad rate".into())
}

/// One and a half seconds per window, so journals sync partway through.
fn length() -> Res<SegmentLength> {
    SegmentLength::new(SampleCount::new(1_500)).ok_or_else(|| "bad length".into())
}

/// The sample a track holds at `index`: distinct per track and position.
/// The same formula as the in-memory crash tests.
fn sample(track: TrackId, index: u64) -> i16 {
    let v = index
        .wrapping_mul(31)
        .wrapping_add(u64::from(track.get()) * 7_919);
    i16::from_le_bytes([v.to_le_bytes()[0], v.to_le_bytes()[1]])
}

fn samples(track: TrackId, from: u64, len: u64) -> Vec<i16> {
    (from..from + len).map(|i| sample(track, i)).collect()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("write") => write_command(&args[1..]),
        Some("check") => check_command(&args[1..]),
        Some("probe-rename") => probe_rename_command(&args[1..]),
        Some("prepare-repair") => prepare_repair_command(&args[1..]),
        _ => Err(
            "usage: lazyfs_crash write <dir> <promises> [--stop-after N] [--no-publish] | \
                  check <dir> <promises> [--recovered] [--end <file>] [--repair] \
                  [--expect-repair] [--stop-after N] | \
                  prepare-repair <dir> <promises> missing|mismatched | probe-rename <dir>"
                .into(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Nothing more can be done if stderr is gone.
            let _ = writeln!(io::stderr(), "lazyfs_crash: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// The counting filesystem.

/// [`StdFs`], counting the operations that change the disk, and stopping
/// dead after the `stop_after`th.
#[derive(Debug, Clone)]
struct CountingFs {
    /// Operations completed so far.
    ops: Arc<AtomicUsize>,
    /// The operation after which execution stops.
    stop_after: Option<usize>,
    /// Set at the crash point: every thread stops before its next
    /// operation.
    halted: Arc<AtomicBool>,
    /// The marker the shell waits for before killing this process.
    marker: PathBuf,
}

#[derive(Debug)]
struct CountingFile {
    /// The file on the filesystem under test.
    file: StdFile,
    /// The shared counter and stop marker.
    fs: CountingFs,
}

/// Fsyncs a [`CountingFile`] from a sync thread, counted.
#[derive(Debug)]
struct CountingSyncer {
    /// The underlying file sync handle.
    syncer: StdSyncer,
    /// The shared counter and stop marker.
    fs: CountingFs,
}

impl CountingFs {
    /// Runs one operation and counts it; at the crash point, marks it and
    /// never returns. After the crash point, no operation runs.
    fn counted<T>(&self, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        if self.halted.load(Ordering::SeqCst) {
            block_forever();
        }
        let result = op();
        let done = self.ops.fetch_add(1, Ordering::SeqCst) + 1;
        if self.stop_after == Some(done) {
            self.halted.store(true, Ordering::SeqCst);
            stop_here(&self.marker)?;
        }
        result
    }

    fn total(&self) -> usize {
        self.ops.load(Ordering::SeqCst)
    }
}

/// Creates the marker, then blocks until the script kills the process.
fn stop_here(marker: &Path) -> io::Result<()> {
    let mut file = StdFs.create(marker)?;
    file.sync()?;
    if let Some(dir) = marker.parent() {
        StdFs.sync_dir(dir)?;
    }
    block_forever();
    Err(io::Error::other("the crash point returned"))
}

/// Blocks this thread until the process is killed.
fn block_forever() {
    let (keep, wait) = mpsc::channel::<()>();
    // Nothing ever sends, and `keep` lives until after `recv`.
    let _ = wait.recv();
    drop(keep);
}

impl Fs for CountingFs {
    type File = CountingFile;
    type Lock = StdLock;

    fn create(&self, path: &Path) -> io::Result<CountingFile> {
        let file = self.counted(|| StdFs.create(path))?;
        Ok(CountingFile {
            file,
            fs: self.clone(),
        })
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.counted(|| StdFs.create_dir(path))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.counted(|| StdFs.rename(from, to))
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.counted(|| StdFs.sync_dir(dir))
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.counted(|| StdFs.remove(path))
    }

    fn sync_file(&self, path: &Path) -> io::Result<()> {
        self.counted(|| StdFs.sync_file(path))
    }

    fn rename_new(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.counted(|| StdFs.rename_new(from, to))
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        StdFs.read(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        StdFs.list(dir)
    }

    fn lock_dir(&self, dir: &Path) -> io::Result<StdLock> {
        StdFs.lock_dir(dir)
    }
}

impl FsFile for CountingFile {
    type Syncer = CountingSyncer;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let file = &mut self.file;
        self.fs.counted(|| file.write_all(bytes))
    }

    fn sync(&mut self) -> io::Result<Synced> {
        let file = &mut self.file;
        self.fs.counted(|| file.sync())
    }

    fn syncer(&self) -> io::Result<CountingSyncer> {
        Ok(CountingSyncer {
            syncer: self.file.syncer()?,
            fs: self.fs.clone(),
        })
    }
}

impl FileSyncer for CountingSyncer {
    fn sync(&self) -> io::Result<Synced> {
        self.fs.counted(|| self.syncer.sync())
    }
}

// ---------------------------------------------------------------------------
// Promises: what the recorder told its caller, one line each.
//
//   start <track> <first sample>
//   durable <track> <end>
//   row <track> <epoch> <start> <end> <sha256 hex>

#[derive(Debug)]
struct PromiseLog {
    /// The log kept outside the filesystem under test.
    file: StdFile,
    /// The furthest sample each track has promised.
    durable: BTreeMap<TrackId, SampleIndex>,
}

impl PromiseLog {
    fn create(path: &Path) -> Res<Self> {
        Ok(Self {
            file: StdFs.create(path)?,
            durable: BTreeMap::new(),
        })
    }

    fn line(&mut self, line: &str) -> Res<()> {
        let mut text = line.to_owned();
        text.push('\n');
        self.file.write_all(text.as_bytes())?;
        self.file.sync()?;
        Ok(())
    }

    fn start(&mut self, track: TrackId, at: SampleIndex) -> Res<()> {
        self.durable.insert(track, at);
        self.line(&format!("start {} {}", track.get(), at.get()))
    }

    /// Records `end` as durable on `track`, if it's further than before.
    fn durable(&mut self, track: TrackId, end: SampleIndex) -> Res<()> {
        if self.durable.get(&track).is_some_and(|&at| at >= end) {
            return Ok(());
        }
        self.durable.insert(track, end);
        self.line(&format!("durable {} {}", track.get(), end.get()))
    }

    /// What the writer says is durable on `track` after a call that
    /// returned `ok`, as the in-memory tests' `Promised::note` reads it:
    /// between journals, a successful call ended the last one with a sync.
    fn note<S: Fs>(&mut self, writer: &SessionWriter<S>, track: TrackId, ok: bool) -> Res<()> {
        let end = match writer.durable(track) {
            Some(d) => Some(d.end()),
            None if ok => writer.next_sample(track),
            None => None,
        };
        match end {
            Some(end) => self.durable(track, end),
            None => Ok(()),
        }
    }

    fn rows(&mut self, rows: &[SegmentRow]) -> Res<()> {
        for row in rows {
            let r = row.range();
            let line = format!(
                "row {} {} {} {} {}",
                row.track().get(),
                row.epoch().get(),
                r.start().get(),
                r.end().get(),
                hex(row.sha256().as_bytes())
            );
            self.line(&line)?;
        }
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The promises read back.
#[derive(Debug, Default)]
struct Promised {
    /// The first sample recorded on each track.
    started: BTreeMap<TrackId, SampleIndex>,
    /// The furthest sample each track has promised.
    durable: BTreeMap<TrackId, SampleIndex>,
    /// (track, epoch, start, end, sha256 hex)
    rows: Vec<(u32, u64, u64, u64, String)>,
    /// The segment name selected for repair; only its aside files are expected.
    repair_file: Option<String>,
    /// Exact names and hashes of files that must never be replaced.
    kept: Vec<(String, String)>,
    /// The mismatched file must survive under its original or aside name.
    displaced: Option<(String, String)>,
}

fn read_promises(path: &Path) -> Res<Promised> {
    let text = String::from_utf8(std::fs::read(path)?)?;
    let mut promised = Promised::default();
    // Every line is written and fsync'd whole; a final line without its
    // newline can't be complete, so it's ignored.
    let complete = text.rsplit_once('\n').map_or("", |(done, _)| done);
    for line in complete.lines() {
        let words: Vec<&str> = line.split(' ').collect();
        let num = |i: usize| -> Res<u64> {
            Ok(words
                .get(i)
                .ok_or_else(|| format!("short line {line:?}"))?
                .parse()?)
        };
        let track = |i: usize| -> Res<TrackId> { Ok(TrackId::new(u32::try_from(num(i)?)?)) };
        match words.first().copied() {
            Some("start") => {
                let (t, at) = (track(1)?, SampleIndex::new(num(2)?));
                promised.started.insert(t, at);
                promised.durable.entry(t).or_insert(at);
            }
            Some("durable") => {
                let (t, end) = (track(1)?, SampleIndex::new(num(2)?));
                let at = promised.durable.entry(t).or_insert(end);
                *at = (*at).max(end);
            }
            Some("row") => {
                let sha = (*words.get(5).ok_or("short row line")?).to_owned();
                promised
                    .rows
                    .push((track(1)?.get(), num(2)?, num(3)?, num(4)?, sha));
            }
            Some("repair") if words.len() == 2 => promised.repair_file = Some(words[1].to_owned()),
            Some("keep" | "displaced") if words.len() == 3 => {
                let entry = (words[1].to_owned(), words[2].to_owned());
                if words[0] == "keep" {
                    promised.kept.push(entry);
                } else {
                    promised.displaced = Some(entry);
                }
            }
            _ => return Err(format!("bad promise line {line:?}").into()),
        }
    }
    Ok(promised)
}

// ---------------------------------------------------------------------------
// write

fn write_command(args: &[String]) -> Res<()> {
    let (Some(dir), Some(promises)) = (args.first(), args.get(1)) else {
        return Err("write needs <dir> <promises>".into());
    };
    let mut stop_after = None;
    let mut publish = true;
    let mut rest = args[2..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--stop-after" => {
                let n = rest.next().ok_or("write: --stop-after needs N")?;
                stop_after = Some(n.parse::<usize>()?);
            }
            "--no-publish" => publish = false,
            other => return Err(format!("write: unknown option {other}").into()),
        }
    }
    let dir = PathBuf::from(dir);
    let promises = PathBuf::from(promises);
    let mut marker = promises.clone().into_os_string();
    marker.push(".stopped");

    // Set-up, not counted: the promises, the session directory and the
    // store. The promises come first, so a crash inside the store's own
    // set-up (which the script injects) still leaves them to check.
    let mut log = PromiseLog::create(&promises)?;
    if let Some(parent) = promises.parent() {
        StdFs.sync_dir(parent)?;
    }
    let session = dir.join("session");
    StdFs.create_dir(&session)?;
    StdFs.sync_dir(&dir)?;
    let mut store = open_library(&dir)?;

    let fs = CountingFs {
        ops: Arc::new(AtomicUsize::new(0)),
        stop_after,
        halted: Arc::new(AtomicBool::new(false)),
        marker: PathBuf::from(marker),
    };
    record(&fs, &mut store, &session, &mut log, publish)?;
    writeln!(io::stdout(), "ops {}", fs.total())?;
    Ok(())
}

/// Records both tracks in real time, publishing finished journals as it
/// goes (unless `publish` is false); the same scenario as the in-memory
/// `record_into`, longer.
fn record(
    fs: &CountingFs,
    store: &mut Store,
    session: &Path,
    log: &mut PromiseLog,
    publish: bool,
) -> Res<()> {
    let (rate, length) = (rate()?, length()?);
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let dyn_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    let session = SessionDir::new(SESSION, fs.clone(), session).lock()?;
    let mut writer =
        SessionWriter::open(&session, rate, length, dyn_clock)?.with_syncing(Syncing::Threads);
    let mut store = SessionStore::new(session, store);
    for (track, at) in TRACKS {
        writer.start_track(track, EpochId::new(0), SampleIndex::new(at))?;
        log.start(track, SampleIndex::new(at))?;
    }
    for step in 0..STEPS {
        let len = SIZES[step % SIZES.len()];
        for (track, _) in TRACKS {
            let from = writer.next_sample(track).ok_or("track not started")?.get();
            let appended = writer.append(track, &samples(track, from, len));
            log.note(&writer, track, appended.is_ok())?;
            appended?;
        }
        clock.advance(
            SampleCount::new(len)
                .duration_at(rate)
                .ok_or("duration overflow")?,
        );
        let synced = writer.sync_if_due();
        for (track, _) in TRACKS {
            log.note(&writer, track, synced.is_ok())?;
        }
        synced?;
        let finished = writer.take_finished();
        if publish && !finished.is_empty() {
            let done = publish_journals(&mut store, length, &finished)?;
            log.rows(done.segments())?;
        }
    }
    let mut ends = Vec::new();
    for (track, _) in TRACKS {
        ends.push((track, writer.next_sample(track).ok_or("track not started")?));
    }
    let finished = writer.finish()?;
    for (track, end) in ends {
        log.durable(track, end)?;
    }
    if publish {
        let done = publish_journals(&mut store, length, &finished)?;
        log.rows(done.segments())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Repair fixtures: committed rows plus the journals that can rebuild them.

fn put_file(path: &Path, bytes: &[u8]) -> Res<()> {
    let mut file = StdFs.create(path)?;
    file.write_all(bytes)?;
    file.sync()?;
    Ok(())
}

/// Refuse a filesystem that cannot enforce the repair's no-replace rename.
fn probe_rename_command(args: &[String]) -> Res<()> {
    let [dir] = args else {
        return Err("probe-rename needs <dir>".into());
    };
    let dir = Path::new(dir);
    let from = dir.join("rename-source");
    let taken = dir.join("rename-taken");
    let free = dir.join("rename-free");
    put_file(&from, b"source")?;
    put_file(&taken, b"taken")?;
    match StdFs.rename_new(&from, &taken) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        _ => return Err("LazyFS must support RENAME_NOREPLACE and refuse occupied names".into()),
    }
    StdFs.rename_new(&from, &free)?;
    if StdFs.read(&taken)? != b"taken" || StdFs.read(&free)? != b"source" {
        return Err("LazyFS no-replace rename changed the wrong file".into());
    }
    StdFs.remove(&taken)?;
    StdFs.remove(&free)?;
    StdFs.sync_dir(dir)?;
    Ok(())
}

/// The fault planted before recovery starts.
#[derive(Debug, Clone, Copy)]
enum RepairScenario {
    Missing,
    Mismatched,
}

impl RepairScenario {
    fn parse(value: &str) -> Res<Self> {
        match value {
            "missing" => Ok(Self::Missing),
            "mismatched" => Ok(Self::Mismatched),
            _ => Err("repair scenario needs missing or mismatched".into()),
        }
    }
}

fn prepare_repair_command(args: &[String]) -> Res<()> {
    let [dir, promises, scenario] = args else {
        return Err("prepare-repair needs <dir> <promises> missing|mismatched".into());
    };
    let scenario = RepairScenario::parse(scenario)?;
    write_command(&[dir.clone(), promises.clone(), "--no-publish".to_owned()])?;
    prepare_repair(Path::new(dir), Path::new(promises), scenario)
}

fn prepare_repair(dir: &Path, promises: &Path, scenario: RepairScenario) -> Res<()> {
    let session = dir.join("session");
    let mut store = open_library(dir)?;
    let journals: Vec<_> = observe(&session, &store)?
        .files
        .into_iter()
        .filter(|(path, _)| is_journal(path))
        .collect();
    if journals.is_empty() {
        return Err("repair fixture has no journals".into());
    }
    let ours = SessionDir::new(SESSION, StdFs, &session).lock()?;
    salvage(&mut SessionStore::new(ours, &mut store), length()?)?;
    let rows = store.segments(SESSION)?;
    let row = rows.first().ok_or("repair fixture has no rows")?;
    // Restore all journals so the missing file's audio can be proved.
    for (path, bytes) in journals {
        put_file(&path, &bytes)?;
    }
    let mut log = repair_promises(promises, &rows)?;
    let name = segment_file_name(row.track(), row.range());
    log.line(&format!("repair {name}"))?;
    StdFs.remove(&session.join(&name))?;
    match scenario {
        RepairScenario::Missing => {}
        RepairScenario::Mismatched => plant_mismatch(&session, &name, &rows, &mut log)?,
    }
    StdFs.sync_dir(&session)?;
    Ok(())
}

fn repair_promises(path: &Path, rows: &[SegmentRow]) -> Res<PromiseLog> {
    StdFs.remove(path)?;
    let mut log = PromiseLog::create(path)?;
    for (track, at) in TRACKS {
        log.start(track, SampleIndex::new(at))?;
        let end = rows
            .iter()
            .filter(|r| r.track() == track)
            .map(|r| r.range().end())
            .max()
            .ok_or("track has no rows")?;
        log.durable(track, end)?;
    }
    log.rows(rows)?;
    if let Some(parent) = path.parent() {
        StdFs.sync_dir(parent)?;
    }
    Ok(log)
}

fn plant_mismatch(
    session: &Path,
    name: &str,
    rows: &[SegmentRow],
    log: &mut PromiseLog,
) -> Res<()> {
    let other = rows.get(1).ok_or("repair fixture needs another segment")?;
    let junk = StdFs.read(&session.join(segment_file_name(other.track(), other.range())))?;
    put_file(&session.join(name), &junk)?;
    log.line(&format!("displaced {name} {}", hex(&Sha256::digest(&junk))))?;
    // Occupied names make recovery use a later suffix without replacing them.
    for (suffix, bytes) in [
        (".mismatched", b"first kept file".as_slice()),
        (".mismatched.1", b"second kept file".as_slice()),
    ] {
        let kept = format!("{name}{suffix}");
        put_file(&session.join(&kept), bytes)?;
        log.line(&format!("keep {kept} {}", hex(&Sha256::digest(bytes))))?;
    }
    Ok(())
}

/// Preservation is checked before and after recovery, and after cache loss.
fn check_kept(session: &Path, promised: &Promised, seen: &Observed, repaired: bool) -> Res<()> {
    let matches = |name: &str, hash: &str| {
        seen.files
            .get(&session.join(name))
            .is_some_and(|bytes| hex(&Sha256::digest(bytes)) == hash)
    };
    for (name, hash) in &promised.kept {
        if !matches(name, hash) {
            return Err(format!("kept file {name} was lost or replaced").into());
        }
    }
    if let Some((name, hash)) = &promised.displaced {
        let aside = format!("{name}.mismatched.2");
        if !matches(&aside, hash) && (repaired || !matches(name, hash)) {
            return Err(format!("mismatched file {name} was lost or replaced").into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// check

/// The session's files and the store's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    /// Each session file and its bytes.
    files: BTreeMap<PathBuf, Vec<u8>>,
    /// The rows committed for this session.
    rows: Vec<SegmentRow>,
}

/// Opens the library database in `dir`, with the example's session in it.
/// A rerun finds the session already there, which is fine.
fn open_library(dir: &Path) -> Res<Store> {
    let mut store = Store::open(&dir.join("library.db"))?;
    match store.create_session(&NewSession {
        id: SESSION,
        title: None,
        language: None,
        started_at: None,
        tracks: vec![],
    }) {
        Ok(()) | Err(StoreError::SessionExists(_)) => Ok(store),
        Err(e) => Err(e.into()),
    }
}

fn observe(session: &Path, store: &Store) -> Res<Observed> {
    let mut files = BTreeMap::new();
    for path in StdFs.list(session)? {
        let bytes = StdFs.read(&path)?;
        files.insert(path, bytes);
    }
    Ok(Observed {
        files,
        rows: store.segments(SESSION)?,
    })
}

fn is_journal(path: &Path) -> bool {
    path.file_name()
        .and_then(JournalId::from_file_name)
        .is_some()
}

fn is_temp_segment(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("seg-") && n.ends_with(".flac.tmp"))
}

fn decode_flac(bytes: &[u8]) -> Res<(u32, Vec<i16>)> {
    let mut reader = claxon::FlacReader::new(io::Cursor::new(bytes))?;
    let hz = reader.streaminfo().sample_rate;
    let mut out = Vec::new();
    for s in reader.samples() {
        out.push(i16::try_from(s?)?);
    }
    Ok((hz, out))
}

/// Every row has its file, holding exactly the row's audio; returns the
/// samples the rows hold.
fn row_samples(session: &Path, seen: &Observed) -> Res<BTreeSet<(TrackId, u64)>> {
    let mut held = BTreeSet::new();
    for row in &seen.rows {
        let path = session.join(segment_file_name(row.track(), row.range()));
        let Some(bytes) = seen.files.get(&path) else {
            return Err(format!("a row without its file: {row:?}").into());
        };
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if &digest != row.sha256().as_bytes() {
            return Err(format!("{} doesn't match its row's hash", path.display()).into());
        }
        let (hz, got) =
            decode_flac(bytes).map_err(|e| format!("{} doesn't decode: {e}", path.display()))?;
        let r = row.range();
        if hz != rate()?.hz() || got != samples(row.track(), r.start().get(), r.len().get()) {
            return Err(format!("{} holds the wrong audio", path.display()).into());
        }
        for s in r.start().get()..r.end().get() {
            if !held.insert((row.track(), s)) {
                return Err(
                    format!("rows overlap at track {} sample {s}", row.track().get()).into(),
                );
            }
        }
    }
    Ok(held)
}

/// The samples valid journal frames hold, each checked against what was
/// recorded.
fn journal_samples(seen: &Observed) -> Res<BTreeSet<(TrackId, u64)>> {
    let mut held = BTreeSet::new();
    for (path, bytes) in &seen.files {
        if !is_journal(path) {
            continue;
        }
        for frame in read_journal(bytes).frames() {
            let r = frame.range();
            if frame.samples() != samples(frame.track(), r.start().get(), r.len().get()) {
                return Err(format!("{} misread at {r:?}", path.display()).into());
            }
            held.extend((r.start().get()..r.end().get()).map(|s| (frame.track(), s)));
        }
    }
    Ok(held)
}

fn check_durable(promised: &Promised, held: &BTreeSet<(TrackId, u64)>) -> Res<()> {
    for (&track, &start) in &promised.started {
        let end = promised.durable.get(&track).copied().unwrap_or(start);
        if let Some(s) = (start.get()..end.get()).find(|&s| !held.contains(&(track, s))) {
            return Err(format!(
                "track {} lost sample {s} (promised durable up to {})",
                track.get(),
                end.get()
            )
            .into());
        }
    }
    Ok(())
}

fn check_after(session: &Path, promised: &Promised, after: &Observed) -> Res<()> {
    if let Some(left) = after
        .files
        .keys()
        .find(|p| is_journal(p) || is_temp_segment(p))
    {
        return Err(format!("salvage left {}", left.display()).into());
    }
    let held = row_samples(session, after)?;
    check_durable(promised, &held)?;
    for (track, epoch, start, end, sha) in &promised.rows {
        let found = after.rows.iter().any(|r| {
            r.track().get() == *track
                && u64::from(r.epoch().get()) == *epoch
                && r.range().start().get() == *start
                && r.range().end().get() == *end
                && hex(r.sha256().as_bytes()) == *sha
        });
        if !found {
            return Err(format!(
                "a committed row disappeared: track {track} epoch {epoch} {start}..{end}"
            )
            .into());
        }
    }
    check_file_names(session, promised, after)
}

/// Only the planted repair can leave aside files after a crash and restart.
fn is_repair_aside(path: &Path, promised: &Promised) -> bool {
    let (Some(name), Some(repair)) = (
        path.file_name().and_then(|n| n.to_str()),
        &promised.repair_file,
    ) else {
        return false;
    };
    let base = format!("{repair}.mismatched");
    name == base
        || name
            .strip_prefix(&format!("{base}."))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn check_file_names(session: &Path, promised: &Promised, after: &Observed) -> Res<()> {
    let named: BTreeSet<PathBuf> = after
        .rows
        .iter()
        .map(|r| session.join(segment_file_name(r.track(), r.range())))
        .collect();
    // Marks and repair findings accompany the segments.
    let marks = session.join(MARKS_FILE_NAME);
    let findings = session.join(FINDINGS_FILE_NAME);
    if let Some(orphan) = after.files.keys().find(|p| {
        !named.contains(*p) && **p != marks && **p != findings && !is_repair_aside(p, promised)
    }) {
        return Err(format!("a file without a row: {}", orphan.display()).into());
    }
    Ok(())
}

/// Writes where salvage ended: one line per session file (its name and
/// hash), then one per row, in the promises' format.
fn write_end(path: &Path, session: &Path, after: &Observed) -> Res<()> {
    let mut text = String::new();
    for (file, bytes) in &after.files {
        let name = file.strip_prefix(session).unwrap_or(file);
        writeln!(
            text,
            "file {} {}",
            name.display(),
            hex(&Sha256::digest(bytes))
        )?;
    }
    for row in &after.rows {
        let r = row.range();
        writeln!(
            text,
            "row {} {} {} {} {}",
            row.track().get(),
            row.epoch().get(),
            r.start().get(),
            r.end().get(),
            hex(row.sha256().as_bytes())
        )?;
    }
    let mut file = StdFs.create(path)?;
    file.write_all(text.as_bytes())?;
    file.sync()?;
    Ok(())
}

/// Options read at the command boundary.
#[derive(Debug, Default)]
struct CheckOptions {
    /// Require a completed recovery to have survived another crash.
    recovered: bool,
    /// Admit a broken row before recovery if journals hold its audio.
    repair: bool,
    /// Require the reference run to repair exactly one row.
    expect_repair: bool,
    /// Stop after this recorder operation for the shell to kill the process.
    stop_after: Option<usize>,
    /// Write the recovered file names, hashes and rows here.
    end: Option<PathBuf>,
}

impl CheckOptions {
    fn parse(args: &[String]) -> Res<Self> {
        let mut options = Self::default();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            match arg.as_str() {
                "--recovered" => options.recovered = true,
                "--repair" => options.repair = true,
                "--expect-repair" => options.expect_repair = true,
                "--stop-after" => {
                    options.stop_after = Some(
                        rest.next()
                            .ok_or("check: --stop-after needs N")?
                            .parse::<usize>()?,
                    );
                }
                "--end" => {
                    options.end = Some(PathBuf::from(
                        rest.next().ok_or("check: --end needs <file>")?,
                    ));
                }
                other => return Err(format!("check: unknown option {other}").into()),
            }
        }
        Ok(options)
    }
}

fn check_before(
    session: &Path,
    promised: &Promised,
    before: &Observed,
    options: &CheckOptions,
) -> Res<()> {
    check_kept(session, promised, before, options.recovered)?;
    let mut proven = before.clone();
    if options.repair && !options.recovered {
        proven.rows.retain(|row| {
            let one = Observed {
                files: before.files.clone(),
                rows: vec![*row],
            };
            row_samples(session, &one).is_ok()
        });
    }
    let in_rows = row_samples(session, &proven)?;
    let in_journals = journal_samples(before)?;
    let held = in_rows.union(&in_journals).copied().collect();
    check_durable(promised, &held)?;
    if options.recovered
        && let Some(left) = before
            .files
            .keys()
            .find(|p| is_journal(p) || is_temp_segment(p))
    {
        return Err(format!(
            "a completed salvage didn't last: {} is back after the crash",
            left.display()
        )
        .into());
    }
    Ok(())
}

fn stopped_marker(promises: &Path) -> PathBuf {
    let mut marker = promises.as_os_str().to_os_string();
    marker.push(".stopped");
    PathBuf::from(marker)
}

fn check_command(args: &[String]) -> Res<()> {
    let (Some(dir), Some(promises_path)) = (args.first(), args.get(1)) else {
        return Err("check needs <dir> <promises>".into());
    };
    let options = CheckOptions::parse(&args[2..])?;
    check(Path::new(dir), Path::new(promises_path), &options)
}

fn check(dir: &Path, promises_path: &Path, options: &CheckOptions) -> Res<()> {
    let session = dir.join("session");
    let promised = read_promises(promises_path)?;
    let length = length()?;
    let mut store = open_library(dir)?;

    let before = observe(&session, &store)?;
    check_before(&session, &promised, &before, options)
        .map_err(|e| format!("before salvage: {e}"))?;
    let fs = CountingFs {
        ops: Arc::new(AtomicUsize::new(0)),
        stop_after: options.stop_after,
        halted: Arc::new(AtomicBool::new(false)),
        marker: stopped_marker(promises_path),
    };
    let ours = SessionDir::new(SESSION, fs.clone(), &session).lock()?;
    let first = salvage(&mut SessionStore::new(ours.clone(), &mut store), length)?;
    if options.expect_repair && first.repaired().len() != 1 {
        return Err(format!("expected one repair, got {}", first.repaired().len()).into());
    }
    let after = observe(&session, &store)?;
    check_kept(&session, &promised, &after, true)?;
    check_after(&session, &promised, &after).map_err(|e| format!("after salvage: {e}"))?;
    if let Some(end) = &options.end {
        write_end(end, &session, &after)?;
    }
    if options.recovered && after != before {
        return Err("a completed salvage didn't last: salvage changed the disk again".into());
    }

    let second = salvage(&mut SessionStore::new(ours, &mut store), length)?;
    if second != Published::default() || observe(&session, &store)? != after {
        return Err("a second salvage changed something".into());
    }
    writeln!(io::stdout(), "ops {}", fs.total())?;
    report_check(&promised, &after, &first)
}

fn report_check(promised: &Promised, after: &Observed, first: &Published) -> Res<()> {
    let durable: Vec<String> = promised
        .durable
        .iter()
        .map(|(t, end)| format!("t{}..{}", t.get(), end.get()))
        .collect();
    writeln!(
        io::stdout(),
        "ok: {} rows ({} promised), {} published by salvage, {} journals deleted, durable {}",
        after.rows.len(),
        promised.rows.len(),
        first.segments().len(),
        first.deleted().len(),
        durable.join(" ")
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removes a test's scratch files even if its assertions fail.
    #[derive(Debug)]
    struct Fixture {
        /// The scratch directory owned by this test.
        root: PathBuf,
        /// The recording directory passed to the checker.
        rec: PathBuf,
        /// The independent promises log.
        promises: PathBuf,
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding removes only its synthetic fixtures"
    )]
    fn remove_scratch(root: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(root)
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = remove_scratch(&self.root);
        }
    }

    /// Each test gets its own fixture; both harnesses can run tests in parallel.
    fn fixture(scenario: RepairScenario, test: &str) -> Res<Fixture> {
        let root =
            std::env::temp_dir().join(format!("nota-lazyfs-repair-{test}-{}", std::process::id()));
        if root.exists() {
            remove_scratch(&root)?;
        }
        StdFs.create_dir(&root)?;
        let rec = root.join("rec");
        let promises = root.join("promises");
        let fixture = Fixture {
            root,
            rec,
            promises,
        };
        StdFs.create_dir(&fixture.rec)?;
        write_command(&[
            fixture.rec.to_string_lossy().into_owned(),
            fixture.promises.to_string_lossy().into_owned(),
            "--no-publish".to_owned(),
        ])?;
        prepare_repair(&fixture.rec, &fixture.promises, scenario)?;
        Ok(fixture)
    }

    /// A planted fault must really require repair, then stay fixed on reopening.
    fn recovery(scenario: RepairScenario, test: &str) -> Res<()> {
        let fixture = fixture(scenario, test)?;
        let (rec, promises) = (&fixture.rec, &fixture.promises);
        // The ordinary check rejects the broken row, so this is a repair case.
        let error = check(rec, promises, &CheckOptions::default())
            .unwrap_err()
            .to_string();
        let expected = match scenario {
            RepairScenario::Missing => "before salvage: a row without its file",
            RepairScenario::Mismatched => "doesn't match its row's hash",
        };
        assert!(error.contains(expected), "wrong fixture failure: {error}");
        check(
            rec,
            promises,
            &CheckOptions {
                repair: true,
                expect_repair: true,
                ..CheckOptions::default()
            },
        )?;
        // Reopening must find the row proved and nothing left to salvage.
        check(
            rec,
            promises,
            &CheckOptions {
                recovered: true,
                ..CheckOptions::default()
            },
        )?;
        Ok(())
    }

    /// Journals must repair a committed row whose file is missing.
    #[test]
    fn missing_fixture_needs_one_repair_and_stays_fixed() -> Res<()> {
        recovery(RepairScenario::Missing, "missing")
    }

    /// Journals must repair a mismatched row without replacing occupied names.
    #[test]
    fn mismatched_fixture_needs_one_repair_and_stays_fixed() -> Res<()> {
        recovery(RepairScenario::Mismatched, "mismatched")
    }

    /// Independent hashes must reject replacement of either occupied aside name.
    #[test]
    fn preservation_promises_detect_replacement() -> Res<()> {
        let fixture = fixture(RepairScenario::Mismatched, "replacement")?;
        let rec = &fixture.rec;
        let session = rec.join("session");
        let promised = read_promises(&fixture.promises)?;
        let seen = observe(&session, &open_library(rec)?)?;
        check_kept(&session, &promised, &seen, false)?;
        for (name, _) in &promised.kept {
            // Replace each name separately so either overwritten file fails.
            let mut replaced = seen.clone();
            replaced
                .files
                .insert(session.join(name), b"replacement".to_vec());
            assert!(check_kept(&session, &promised, &replaced, false).is_err());
        }
        Ok(())
    }

    /// The old mismatched audio must survive the move to its aside name.
    #[test]
    fn preservation_promises_detect_lost_mismatched_audio() -> Res<()> {
        let fixture = fixture(RepairScenario::Mismatched, "lost")?;
        let rec = &fixture.rec;
        let session = rec.join("session");
        let promised = read_promises(&fixture.promises)?;
        let mut seen = observe(&session, &open_library(rec)?)?;
        check_kept(&session, &promised, &seen, false)?;
        let (name, _) = promised
            .displaced
            .as_ref()
            .ok_or("no displaced file promise")?;
        // Removing the only copy must fail even before repair runs.
        seen.files.remove(&session.join(name));
        assert!(check_kept(&session, &promised, &seen, false).is_err());
        Ok(())
    }
}
