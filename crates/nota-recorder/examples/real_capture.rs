//! The M1a exit criteria (T1, first half) run end to end with real
//! capture: the workload and the checker for `scripts/real-capture-crash.sh`
//! and `scripts/real-engine-kill.sh`.
//!
//! - `write <dir> <log> <ref> --source NODE --seconds S --segment-seconds K
//!   [--stop-after N]` captures one track from the `PipeWire` node `NODE` at
//!   16 kHz for `S` seconds into `<dir>/session`, with the store at
//!   `<dir>/nota.db`, publishing each finished journal on a publisher
//!   thread as it goes, in `K`-second segment windows. Every recorder
//!   operation that changes the disk, and every row commit, is counted.
//!   After the Nth, every thread stops before its next operation,
//!   `<log>.stopped` is created, and the script SIGKILLs the process.
//!   Operations run concurrently, as in the recorder, so the recorder's
//!   timing is real; an operation another thread already had under way may
//!   still land after the Nth, and is logged as late. Operations slower
//!   than 100 ms are logged with their time. Keep `K` small: the tap
//!   rereads a journal at each fsync. Each journal write is copied to
//!   `<ref>` (unsynced), the audio
//!   the recorder captured, to check the recovered audio against. `<log>`
//!   gets one fsync'd line for each journal fsync (when, captured, durable
//!   before and after, delivered), each committed row, each overrun or
//!   journal failure, and the stop. Keep `<log>` and `<ref>` off the filesystem
//!   under test.
//! - `salvage <dir> --segment-seconds K [--stop-after N]` runs salvage,
//!   counting its operations and stopping after the Nth as `write` does.
//! - `check <dir> <log> <ref> --segment-seconds K [--recovered yes]` checks
//!   the bounded-loss
//!   criteria: before salvage, every row has its file and every sample
//!   fsync'd is in a row or a journal; after it, only segments and rows are
//!   left, in order and without gaps from the first sample, holding at least
//!   everything fsync'd and exactly the audio captured, and every committed
//!   row; a second salvage changes nothing; the durable position was never
//!   more than the journal's sync interval (850 ms) behind the captured one,
//!   nor more than 1.1 s behind the audio delivered, nor behind the wall
//!   clock (all below); no audio was lost before the journal. It prints one `result` line
//!   with the measurements and a digest of the files and rows. With
//!   `--recovered yes`, for a disk that crashed again after a completed
//!   salvage, salvage must also find nothing left to do.
//! - `engine <dir> --source NODE --seconds S --engine PROGRAM --parakeet DIR
//!   --vad FILE --said TEXT --kill-after-ms MS --ready PATH` captures from
//!   `NODE` into journals and feeds every frame written to the real engine
//!   under its supervisor. Once the engine is online and capture has
//!   started it creates `PATH`, so the script can start the speech. `MS`
//!   after the first confirmed text it SIGKILLs the engine, then measures
//!   how long until the new engine's first text, and checks that the kill
//!   cut an utterance (that text starts before the audio sent at the
//!   kill), that no audio was lost before the journal or skipped, that the
//!   journals fed the engine without a gap, that everything sent was
//!   confirmed, and that the text reads as `TEXT`.
//!
//! "Captured" is what the recorder has written to the journal. "Delivered"
//! is that and the audio still queued between the stream and the recorder,
//! from the track's [`Progress`](nota_recorder::capture::Progress): what
//! the server handed over, and what a kill loses beyond durable. Behind
//! delivered is the bounded-loss rule, 1.1 s; behind the journal, durable
//! must stay within the sync interval. The lag is also measured against
//! the wall clock since the first frame was written (which slightly
//! undercounts the queue at the start), as a cross-check. Fsync times only
//! mean something on a real disk: tmpfs makes every fsync free.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::ffi::OsString;
    use std::fmt::{self, Write as _};
    use std::io::{self, Write as _};
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitCode};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, mpsc};
    use std::thread;
    use std::time::Duration;

    use nota_core::messages::AudioChunk;
    use nota_core::{
        Clock, EpochId, SampleCount, SampleIndex, SampleRate, SessionId, SessionTime, SystemClock,
        TrackId, TrackTimeline,
    };
    use nota_recorder::capture::{
        CaptureNotice, CaptureReceiver, PipeWireBackend, Progress, RecordError, RecorderEvent,
        Source, record_track, start,
    };
    use nota_recorder::engine::{
        EngineCommand, EngineConfig, EngineEvent, EngineStatus, EngineSupervisor,
    };
    use nota_recorder::fs::{Fs, FsFile, StdFile, StdFs, StdLock, Synced};
    use nota_recorder::journal::{JournalId, SYNC_INTERVAL, read_journal};
    use nota_recorder::segment::{
        DurableSegment, Published, SegmentLength, SegmentStore, publish_journals, salvage,
        segment_file_name,
    };
    use nota_recorder::session::{
        FinishedJournal, MARKS_FILE_NAME, SessionDir, SessionStore, SessionWriter,
    };
    use nota_store::{SegmentRow, Store};
    use sha2::{Digest, Sha256};

    type Res<T> = Result<T, Box<dyn Error + Send + Sync>>;

    const SESSION: SessionId = SessionId::new(1);
    const TRACK: TrackId = TrackId::new(0);
    const RATE: SampleRate = SampleRate::SPEECH;
    /// The bounded-loss rule, durable behind the audio delivered: 1.1 s at
    /// 16 kHz.
    const MAX_LAG: u64 = 17_600;
    /// An operation slower than this is logged.
    const SLOW: Duration = Duration::from_millis(100);
    /// The quietest peak that counts as audio playing: -70 dBFS.
    const MIN_PEAK: i16 = 10;

    pub(super) fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let result = match args.first().map(String::as_str) {
            Some("write") => write_command(&Args::parse(&args[1..])),
            Some("salvage") => salvage_command(&Args::parse(&args[1..])),
            Some("check") => check_command(&Args::parse(&args[1..])),
            Some("engine") => engine_command(&Args::parse(&args[1..])),
            _ => Err("usage: real_capture write|salvage|check|engine ... (see the source)".into()),
        };
        match result {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(e) => {
                // Nothing more can be done if stderr is gone.
                let _ = writeln!(io::stderr(), "real_capture: {e}");
                ExitCode::from(2)
            }
        }
    }

    /// Positional arguments, then `--flag value` pairs.
    #[derive(Debug, Default)]
    struct Args {
        positional: Vec<String>,
        flags: BTreeMap<String, String>,
        bad: Option<String>,
    }

    impl Args {
        fn parse(args: &[String]) -> Self {
            let mut parsed = Self::default();
            let mut it = args.iter();
            while let Some(arg) = it.next() {
                if let Some(flag) = arg.strip_prefix("--") {
                    match it.next() {
                        Some(value) => {
                            parsed.flags.insert(flag.to_owned(), value.clone());
                        }
                        None => parsed.bad = Some(format!("{arg} needs a value")),
                    }
                } else {
                    parsed.positional.push(arg.clone());
                }
            }
            parsed
        }

        fn path(&self, i: usize, what: &str) -> Res<PathBuf> {
            if let Some(bad) = &self.bad {
                return Err(bad.clone().into());
            }
            self.positional
                .get(i)
                .map(PathBuf::from)
                .ok_or_else(|| format!("missing {what}").into())
        }

        fn flag(&self, name: &str) -> Res<&str> {
            self.flags
                .get(name)
                .map(String::as_str)
                .ok_or_else(|| format!("missing --{name}").into())
        }

        fn number(&self, name: &str) -> Res<u64> {
            Ok(self.flag(name)?.parse()?)
        }

        fn optional(&self, name: &str) -> Res<Option<u64>> {
            Ok(match self.flags.get(name) {
                Some(v) => Some(v.parse()?),
                None => None,
            })
        }

        fn length(&self) -> Res<SegmentLength> {
            self.number("segment-seconds")?
                .checked_mul(u64::from(RATE.hz()))
                .and_then(|n| SegmentLength::new(SampleCount::new(n)))
                .ok_or_else(|| "--segment-seconds must be positive and not huge".into())
        }
    }

    fn ms(samples: u64) -> f64 {
        // Sample counts here are far below 2^52.
        #[expect(
            clippy::cast_precision_loss,
            reason = "sample counts in a measurement run are far below 2^52"
        )]
        let s = samples as f64;
        s * 1_000.0 / f64::from(RATE.hz())
    }

    fn nanos(t: SessionTime) -> u64 {
        t.as_nanos()
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the measurement runs for a fixed wall time while the recorder thread records"
    )]
    fn wait(duration: Duration) {
        thread::sleep(duration);
    }

    /// Blocks this thread until the process is killed.
    fn block_forever() {
        let (keep, wait) = mpsc::channel::<()>();
        // Nothing ever sends, and `keep` lives until after `recv`.
        let _ = wait.recv();
        drop(keep);
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

    fn class(path: &Path) -> &'static str {
        if is_journal(path) {
            "journal"
        } else if is_temp_segment(path) {
            "segment-tmp"
        } else if path.extension().is_some_and(|e| e == "flac") {
            "segment"
        } else {
            "dir"
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    // -----------------------------------------------------------------------
    // The tapped filesystem.

    /// The log `write` keeps, one fsync'd line each:
    ///
    ///   start <first sample>
    ///   first <t ns> <end of the first frame>
    ///   sync <t ns> <captured> <durable before> <durable after> <delivered>
    ///   row <epoch> <start> <end> <sha256 hex>
    ///   overrun <t ns>
    ///   journal-failed <t ns>
    ///   slow <kind> <class> <t ns> <took ns>
    ///   late <op> <kind> <class>
    ///   stop <op> <kind> <class> <t ns> <captured> <durable> <delivered>
    ///   end <t ns> <captured> <durable> <delivered>
    #[derive(Debug)]
    struct Log(StdFile);

    impl Log {
        fn line(&mut self, line: &str) -> io::Result<()> {
            let mut text = line.to_owned();
            text.push('\n');
            self.0.write_all(text.as_bytes())?;
            self.0.sync()?;
            Ok(())
        }
    }

    /// What [`TapFs`] does besides writing.
    #[derive(Debug)]
    struct Tap {
        clock: Arc<dyn Clock>,
        /// Set at the crash point: every thread stops before its next
        /// operation. Operations run concurrently, as in the recorder, so
        /// one already under way on another thread may still land; it's
        /// logged as late.
        halted: AtomicBool,
        ops: AtomicUsize,
        stop_after: Option<usize>,
        marker: Option<PathBuf>,
        /// The bytes written to each journal still on disk.
        journals: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
        /// The end of the audio fsync'd.
        durable: AtomicU64,
        /// The track's progress, for the audio delivered, once capture has
        /// started.
        progress: OnceLock<Progress>,
        anchored: AtomicBool,
        log: Option<Mutex<Log>>,
        /// Where each journal's writes are copied, and the copies.
        reference: Option<(PathBuf, Mutex<BTreeMap<PathBuf, StdFile>>)>,
        /// Each journal write, for the engine.
        frames: Option<Mutex<mpsc::Sender<Feed>>>,
    }

    /// [`StdFs`] with a [`Tap`].
    #[derive(Debug, Clone)]
    struct TapFs(Arc<Tap>);

    #[derive(Debug)]
    struct TapFile {
        file: StdFile,
        path: PathBuf,
        fs: TapFs,
    }

    fn poisoned<T>(_: T) -> io::Error {
        io::Error::other("a lock was poisoned")
    }

    impl Tap {
        fn new(clock: Arc<dyn Clock>) -> Self {
            Self {
                clock,
                halted: AtomicBool::new(false),
                ops: AtomicUsize::new(0),
                stop_after: None,
                marker: None,
                journals: Mutex::new(BTreeMap::new()),
                durable: AtomicU64::new(0),
                progress: OnceLock::new(),
                anchored: AtomicBool::new(false),
                log: None,
                reference: None,
                frames: None,
            }
        }

        fn log(&self, line: &str) -> io::Result<()> {
            match &self.log {
                Some(log) => log.lock().map_err(poisoned)?.line(line),
                None => Ok(()),
            }
        }

        /// Logs `event` with the time.
        fn note(&self, event: &str) -> io::Result<()> {
            let t = nanos(self.clock.now());
            self.log(&format!("{event} {t}"))
        }

        /// The end of the valid frames written to `path`.
        fn written_end(&self, path: &Path) -> io::Result<Option<u64>> {
            let journals = self.journals.lock().map_err(poisoned)?;
            Ok(journals
                .get(path)
                .and_then(|bytes| read_journal(bytes).range())
                .map(|r| r.end().get()))
        }

        /// The end of the audio the server delivered: captured, and queued
        /// for the recorder. At least `captured`, which the tap may see
        /// before the recorder's progress does.
        fn delivered(&self, captured: u64) -> u64 {
            self.progress
                .get()
                .map_or(0, |p| p.now().delivered.get())
                .max(captured)
        }

        /// The end of everything written: the furthest valid frame in any
        /// journal still on disk, or what's durable.
        fn captured(&self) -> io::Result<u64> {
            let journals = self.journals.lock().map_err(poisoned)?;
            let written = journals
                .values()
                .filter_map(|bytes| read_journal(bytes).range())
                .map(|r| r.end().get())
                .max()
                .unwrap_or(0);
            Ok(written.max(self.durable.load(Ordering::SeqCst)))
        }
    }

    impl TapFs {
        /// Runs one operation that changes the disk, and counts it; logs it
        /// if it took over 100 ms. At the crash point it logs it, marks it
        /// and never returns; so does an operation that ends after it.
        fn counted<T>(
            &self,
            kind: &str,
            path: &Path,
            op: impl FnOnce() -> io::Result<T>,
        ) -> io::Result<T> {
            if self.0.halted.load(Ordering::SeqCst) {
                block_forever();
            }
            let began = self.0.clock.now();
            let result = op();
            let ended = self.0.clock.now();
            let done = self.0.ops.fetch_add(1, Ordering::SeqCst) + 1;
            let took = ended.checked_duration_since(began).unwrap_or_default();
            if took > SLOW {
                self.0.log(&format!(
                    "slow {kind} {} {} {}",
                    class(path),
                    nanos(began),
                    took.as_nanos()
                ))?;
            }
            match self.0.stop_after {
                Some(n) if done == n => {
                    self.0.halted.store(true, Ordering::SeqCst);
                    self.stop(done, kind, path)?;
                }
                Some(n) if done > n => {
                    self.0.halted.store(true, Ordering::SeqCst);
                    self.0.log(&format!("late {done} {kind} {}", class(path)))?;
                }
                _ => return result,
            }
            block_forever();
            Err(io::Error::other("the crash point returned"))
        }

        fn stop(&self, op: usize, kind: &str, path: &Path) -> io::Result<()> {
            let t = nanos(self.0.clock.now());
            let captured = self.0.captured()?;
            let line = format!(
                "stop {op} {kind} {} {t} {captured} {} {}",
                class(path),
                self.0.durable.load(Ordering::SeqCst),
                self.0.delivered(captured)
            );
            self.0.log(&line)?;
            if let Some(marker) = &self.0.marker {
                let mut file = StdFs.create(marker)?;
                file.sync()?;
                if let Some(dir) = marker.parent() {
                    StdFs.sync_dir(dir)?;
                }
            }
            Ok(())
        }

        fn total(&self) -> usize {
            self.0.ops.load(Ordering::SeqCst)
        }

        /// Notes a journal write: the copy, the engine feed, the first
        /// frame's time.
        fn wrote(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.0
                .journals
                .lock()
                .map_err(poisoned)?
                .entry(path.to_owned())
                .or_default()
                .extend(bytes);
            if let Some((_, copies)) = &self.0.reference
                && let Some(copy) = copies.lock().map_err(poisoned)?.get_mut(path)
            {
                copy.write_all(bytes)?;
            }
            if let Some(frames) = &self.0.frames {
                // The engine side ending early isn't the recorder's problem.
                let _ = frames
                    .lock()
                    .map_err(poisoned)?
                    .send(Feed::Bytes(path.to_owned(), bytes.to_vec()));
            }
            if self.0.log.is_some()
                && !self.0.anchored.load(Ordering::SeqCst)
                && let Some(end) = self.0.written_end(path)?
            {
                self.0.anchored.store(true, Ordering::SeqCst);
                let t = nanos(self.0.clock.now());
                self.0.log(&format!("first {t} {end}"))?;
            }
            Ok(())
        }

        /// Notes a journal fsync: everything written to it is durable, and
        /// a journal is only written up to its sync before the next starts,
        /// so its end is also everything captured.
        fn synced(&self, path: &Path) -> io::Result<()> {
            if self.0.log.is_none() {
                return Ok(());
            }
            let Some(end) = self.0.written_end(path)? else {
                return Ok(());
            };
            let before = self.0.durable.fetch_max(end, Ordering::SeqCst);
            if end > before {
                let t = nanos(self.0.clock.now());
                let delivered = self.0.delivered(end);
                self.0
                    .log(&format!("sync {t} {end} {before} {end} {delivered}"))?;
            }
            Ok(())
        }
    }

    impl Fs for TapFs {
        type File = TapFile;
        type Lock = StdLock;

        fn create(&self, path: &Path) -> io::Result<TapFile> {
            let file = self.counted("create", path, || {
                let file = StdFs.create(path)?;
                if is_journal(path) {
                    if let Some((dir, copies)) = &self.0.reference
                        && let Some(name) = path.file_name()
                    {
                        let copy = StdFs.create(&dir.join(name))?;
                        copies
                            .lock()
                            .map_err(poisoned)?
                            .insert(path.to_owned(), copy);
                    }
                    self.0
                        .journals
                        .lock()
                        .map_err(poisoned)?
                        .insert(path.to_owned(), Vec::new());
                }
                Ok(file)
            })?;
            Ok(TapFile {
                file,
                path: path.to_owned(),
                fs: self.clone(),
            })
        }

        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.counted("mkdir", path, || StdFs.create_dir(path))
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.counted("rename", to, || StdFs.rename(from, to))
        }

        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.counted("dirsync", dir, || StdFs.sync_dir(dir))
        }

        fn remove(&self, path: &Path) -> io::Result<()> {
            self.counted("remove", path, || {
                StdFs.remove(path)?;
                self.0.journals.lock().map_err(poisoned)?.remove(path);
                Ok(())
            })
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

    impl FsFile for TapFile {
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            let (file, path, fs) = (&mut self.file, &self.path, &self.fs);
            fs.counted("write", path, || {
                file.write_all(bytes)?;
                if is_journal(path) {
                    fs.wrote(path, bytes)?;
                }
                Ok(())
            })
        }

        fn sync(&mut self) -> io::Result<Synced> {
            let (file, path, fs) = (&mut self.file, &self.path, &self.fs);
            fs.counted("fsync", path, || {
                let synced = file.sync()?;
                if is_journal(path) {
                    fs.synced(path)?;
                }
                Ok(synced)
            })
        }
    }

    /// The store, with each row commit counted as one operation, so a
    /// crash can fall between a row's commit and its journal's removal.
    #[derive(Debug)]
    struct CountedStore<'a> {
        store: &'a mut Store,
        fs: TapFs,
    }

    #[derive(Debug)]
    struct CommitError(String);

    impl fmt::Display for CommitError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl Error for CommitError {}

    impl SegmentStore for CountedStore<'_> {
        type Error = CommitError;

        fn rows(&mut self) -> Result<Vec<SegmentRow>, CommitError> {
            self.store
                .segments()
                .map_err(|e| CommitError(e.to_string()))
        }

        fn insert(&mut self, segment: &DurableSegment) -> Result<(), CommitError> {
            let store = &mut *self.store;
            self.fs
                .counted("commit", segment.path(), || {
                    store
                        .insert_segment(segment.row())
                        .map(|_| ())
                        .map_err(io::Error::other)
                })
                .map_err(|e| CommitError(e.to_string()))
        }
    }
    // -----------------------------------------------------------------------
    // write

    fn write_command(args: &Args) -> Res<bool> {
        let dir = args.path(0, "<dir>")?;
        let log_path = args.path(1, "<log>")?;
        let ref_dir = args.path(2, "<ref>")?;
        let source = Source::Device(args.flag("source")?.to_owned());
        let seconds = args.number("seconds")?;
        let length = args.length()?;
        let stop_after = args
            .optional("stop-after")?
            .map(usize::try_from)
            .transpose()?;
        let mut marker = log_path.clone().into_os_string();
        marker.push(".stopped");

        // Set-up, not counted: the session directory, the store, the log.
        let session_path = dir.join("session");
        StdFs.create_dir(&session_path)?;
        StdFs.sync_dir(&dir)?;
        StdFs.create_dir(&ref_dir)?;
        let store = Store::open(&dir.join("nota.db"))?;
        let log = Log(StdFs.create(&log_path)?);
        if let Some(parent) = log_path.parent() {
            StdFs.sync_dir(parent)?;
        }

        let clock: Arc<dyn Clock> =
            Arc::new(SystemClock::start().map_err(|_| "no monotonic clock")?);
        let mut tap = Tap::new(Arc::clone(&clock));
        tap.stop_after = stop_after;
        tap.marker = Some(PathBuf::from(marker));
        tap.log = Some(Mutex::new(log));
        tap.reference = Some((ref_dir, Mutex::new(BTreeMap::new())));
        let fs = TapFs(Arc::new(tap));

        let session = SessionDir::new(SESSION, fs.clone(), &session_path).lock()?;
        let mut writer = SessionWriter::open(&session, RATE, length, Arc::clone(&clock))?;
        writer.start_track(TRACK, EpochId::new(0), SampleIndex::ZERO)?;
        fs.0.log("start 0")?;

        // Publishing runs on its own thread, so the recorder never waits
        // for an encode.
        let (to_publish, finished) = mpsc::channel::<Vec<FinishedJournal>>();
        let publisher = {
            let fs = fs.clone();
            let session = session.clone();
            thread::spawn(move || -> Res<()> {
                let mut store = store;
                let counted = CountedStore {
                    store: &mut store,
                    fs: fs.clone(),
                };
                let mut bound = SessionStore::new(session, counted);
                for batch in finished {
                    let done = publish_journals(&mut bound, length, &batch)?;
                    for row in done.segments() {
                        fs.0.log(&row_line(row))?;
                    }
                }
                Ok(())
            })
        };

        let (capture, events) = start(&PipeWireBackend, TRACK, &source, RATE, &clock)?;
        if let Some(progress) = events.progress(TRACK) {
            let _ = fs.0.progress.set(progress);
        }
        let mut timeline = TrackTimeline::new(TRACK);
        timeline.open_epoch(clock.now(), SampleIndex::ZERO, RATE)?;
        let recorder = {
            let to_publish = to_publish.clone();
            let fs = fs.clone();
            thread::spawn(move || record(writer, timeline, &events, &to_publish, &fs))
        };
        wait(Duration::from_secs(seconds));
        drop(capture);
        let (writer, notices, failures, result, unlogged) = recorder
            .join()
            .map_err(|_| "the recorder thread panicked")?;
        result?;
        if let Some(e) = unlogged {
            return Err(e.into());
        }
        let last = writer.finish()?;
        let _ = to_publish.send(last);
        drop(to_publish);
        publisher
            .join()
            .map_err(|_| "the publisher thread panicked")??;
        let t = nanos(clock.now());
        let captured = fs.0.captured()?;
        fs.0.log(&format!(
            "end {t} {captured} {} {}",
            fs.0.durable.load(Ordering::SeqCst),
            fs.0.delivered(captured)
        ))?;
        let overruns = notices
            .iter()
            .filter(|n| **n == CaptureNotice::Overrun)
            .count();
        writeln!(
            io::stdout(),
            "ops {} overruns {overruns} journal-failures {failures}",
            fs.total()
        )?;
        Ok(failures == 0)
    }

    /// What the recorder thread hands back: the writer, the stream's
    /// notices, the journal failures, how recording ended, and the first
    /// failure to log a lost-audio event.
    type Recorded = (
        SessionWriter<TapFs>,
        Vec<CaptureNotice>,
        usize,
        Result<(), RecordError>,
        Option<io::Error>,
    );

    /// Records until the capture stops, handing finished journals to the
    /// publisher and logging audio lost before the journal.
    fn record(
        mut writer: SessionWriter<TapFs>,
        mut timeline: TrackTimeline,
        events: &CaptureReceiver,
        to_publish: &mpsc::Sender<Vec<FinishedJournal>>,
        fs: &TapFs,
    ) -> Recorded {
        let mut notices = Vec::new();
        let mut failures = 0_usize;
        let mut unlogged = None;
        let result = record_track(&mut writer, &mut timeline, events, &mut |e| {
            let logged = match e {
                RecorderEvent::Finished(j) => {
                    let _ = to_publish.send(j);
                    Ok(())
                }
                RecorderEvent::JournalFailed(_) => {
                    failures += 1;
                    fs.0.note("journal-failed")
                }
                // Rows carry their epoch; the checks compare them as is.
                // The audio is checked from the journals; one track's
                // stream failing ends `record_track` with that error.
                RecorderEvent::Epoch(_)
                | RecorderEvent::EpochRefused(_)
                | RecorderEvent::Audio(_)
                | RecorderEvent::CaptureFailed(_) => Ok(()),
                RecorderEvent::Capture(n) => {
                    let logged = if n == CaptureNotice::Overrun {
                        fs.0.note("overrun")
                    } else {
                        Ok(())
                    };
                    notices.push(n);
                    logged
                }
            };
            if let Err(e) = logged {
                unlogged.get_or_insert(e);
            }
        });
        (writer, notices, failures, result, unlogged)
    }

    fn row_line(row: &SegmentRow) -> String {
        let r = row.range();
        format!(
            "row {} {} {} {}",
            row.epoch().get(),
            r.start().get(),
            r.end().get(),
            hex(row.sha256().as_bytes())
        )
    }

    // -----------------------------------------------------------------------
    // salvage

    fn salvage_command(args: &Args) -> Res<bool> {
        let dir = args.path(0, "<dir>")?;
        let length = args.length()?;
        let clock: Arc<dyn Clock> =
            Arc::new(SystemClock::start().map_err(|_| "no monotonic clock")?);
        let mut tap = Tap::new(clock);
        tap.stop_after = args
            .optional("stop-after")?
            .map(usize::try_from)
            .transpose()?;
        if let Some(marker) = args.flags.get("marker") {
            tap.marker = Some(PathBuf::from(marker));
        }
        let fs = TapFs(Arc::new(tap));
        let mut store = Store::open(&dir.join("nota.db"))?;
        let session = SessionDir::new(SESSION, fs.clone(), &dir.join("session")).lock()?;
        let counted = CountedStore {
            store: &mut store,
            fs: fs.clone(),
        };
        let done = salvage(&mut SessionStore::new(session, counted), length)?;
        writeln!(
            io::stdout(),
            "ops {} published {} deleted {}",
            fs.total(),
            done.segments().len(),
            done.deleted().len()
        )?;
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // check

    #[derive(Debug, Default)]
    struct Promised {
        durable: u64,
        /// (epoch, start, end, sha256 hex)
        rows: Vec<(u64, u64, u64, String)>,
        /// (t, end of the first frame)
        first: Option<(u64, u64)>,
        /// (t, captured, durable before, delivered)
        syncs: Vec<(u64, u64, u64, u64)>,
        /// (op, kind, class, t, captured, durable, delivered)
        stop: Option<(u64, String, String, u64, u64, u64, u64)>,
        /// (t, captured, durable, delivered)
        end: Option<(u64, u64, u64, u64)>,
        overruns: usize,
        journal_failures: usize,
        /// (kind:class, t, took ns), slowest first after reading.
        slow: Vec<(String, u64, u64)>,
        late: usize,
    }

    impl Promised {
        /// Where the run ended (the crash point, or `none`), with what was
        /// captured and delivered then.
        fn at_end(&self) -> (String, u64, u64) {
            if let Some((op, kind, class, _, captured, _, delivered)) = &self.stop {
                (format!("{op}:{kind}:{class}"), *captured, *delivered)
            } else {
                let (captured, delivered) = self
                    .end
                    .map_or((0, 0), |(_, captured, _, delivered)| (captured, delivered));
                ("none".to_owned(), captured, delivered)
            }
        }
    }

    fn read_log(path: &Path) -> Res<Promised> {
        let text = String::from_utf8(StdFs.read(path)?)?;
        let mut p = Promised::default();
        // Every line is written and fsync'd whole; a final line without its
        // newline can't be complete, so it's ignored.
        let complete = text.rsplit_once('\n').map_or("", |(done, _)| done);
        for line in complete.lines() {
            let w: Vec<&str> = line.split(' ').collect();
            let n = |i: usize| -> Res<u64> {
                Ok(w.get(i)
                    .ok_or_else(|| format!("short line {line:?}"))?
                    .parse()?)
            };
            let s = |i: usize| -> Res<String> {
                Ok((*w.get(i).ok_or_else(|| format!("short line {line:?}"))?).to_owned())
            };
            match w.first().copied() {
                Some("start") => {}
                Some("first") => p.first = Some((n(1)?, n(2)?)),
                Some("sync") => {
                    p.syncs.push((n(1)?, n(2)?, n(3)?, n(5)?));
                    p.durable = p.durable.max(n(4)?);
                }
                Some("row") => p.rows.push((n(1)?, n(2)?, n(3)?, s(4)?)),
                Some("overrun") => p.overruns += 1,
                Some("slow") => p.slow.push((format!("{}:{}", s(1)?, s(2)?), n(3)?, n(4)?)),
                Some("late") => p.late += 1,
                Some("journal-failed") => p.journal_failures += 1,
                Some("stop") => {
                    p.stop = Some((n(1)?, s(2)?, s(3)?, n(4)?, n(5)?, n(6)?, n(7)?));
                }
                Some("end") => {
                    p.end = Some((n(1)?, n(2)?, n(3)?, n(4)?));
                    p.durable = p.durable.max(n(3)?);
                }
                _ => return Err(format!("bad log line {line:?}").into()),
            }
        }
        Ok(p)
    }

    /// The audio the recorder wrote, from the copies of its journals: one
    /// run from sample 0.
    fn reference(dir: &Path) -> Res<Vec<i16>> {
        let mut frames = BTreeMap::new();
        for path in StdFs.list(dir)? {
            for frame in read_journal(&StdFs.read(&path)?).frames() {
                frames.insert(frame.range().start().get(), frame.samples().to_vec());
            }
        }
        let mut audio = Vec::new();
        for (start, samples) in frames {
            if start != audio.len() as u64 {
                return Err(format!("the reference has a gap at {}", audio.len()).into());
            }
            audio.extend(samples);
        }
        Ok(audio)
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Observed {
        files: BTreeMap<PathBuf, Vec<u8>>,
        rows: Vec<SegmentRow>,
    }

    impl Observed {
        fn read(session: &Path, store: &Store) -> Res<Self> {
            let mut files = BTreeMap::new();
            for path in StdFs.list(session)? {
                let bytes = StdFs.read(&path)?;
                files.insert(path, bytes);
            }
            Ok(Self {
                files,
                rows: store.segments()?,
            })
        }

        /// A digest of every file's name and bytes and every row.
        fn digest(&self) -> String {
            let mut h = Sha256::new();
            for (path, bytes) in &self.files {
                h.update(
                    path.file_name()
                        .map(std::ffi::OsStr::as_encoded_bytes)
                        .unwrap_or_default(),
                );
                h.update(Sha256::digest(bytes));
            }
            for row in &self.rows {
                h.update(row_line(row).as_bytes());
            }
            hex(&h.finalize())[..16].to_owned()
        }
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

    fn slice(audio: &[i16], start: u64, end: u64) -> Option<&[i16]> {
        audio.get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
    }

    /// Every row has its file, holding exactly the row's audio as
    /// captured; returns the rows' ranges, sorted.
    fn row_ranges(session: &Path, seen: &Observed, audio: &[i16]) -> Res<Vec<(u64, u64)>> {
        let mut ranges = Vec::new();
        for row in &seen.rows {
            let path = session.join(segment_file_name(row.track(), row.range()));
            let Some(bytes) = seen.files.get(&path) else {
                return Err(format!("a row without its file: {row:?}").into());
            };
            let digest: [u8; 32] = Sha256::digest(bytes).into();
            if &digest != row.sha256().as_bytes() {
                return Err(format!("{} doesn't match its row's hash", path.display()).into());
            }
            let (hz, got) = decode_flac(bytes)
                .map_err(|e| format!("{} doesn't decode: {e}", path.display()))?;
            let (start, end) = (row.range().start().get(), row.range().end().get());
            if hz != RATE.hz() || Some(got.as_slice()) != slice(audio, start, end) {
                return Err(format!("{} holds other audio than captured", path.display()).into());
            }
            ranges.push((start, end));
        }
        ranges.sort_unstable();
        Ok(ranges)
    }

    /// The ranges valid journal frames hold, each checked against the
    /// audio captured.
    fn journal_ranges(seen: &Observed, audio: &[i16]) -> Res<Vec<(u64, u64)>> {
        let mut ranges = Vec::new();
        for (path, bytes) in &seen.files {
            if !is_journal(path) {
                continue;
            }
            for frame in read_journal(bytes).frames() {
                let (start, end) = (frame.range().start().get(), frame.range().end().get());
                if Some(frame.samples()) != slice(audio, start, end) {
                    return Err(format!("{} misread at {start}..{end}", path.display()).into());
                }
                ranges.push((start, end));
            }
        }
        Ok(ranges)
    }

    /// The end of the run the ranges cover from sample 0.
    fn covered_from_zero(mut ranges: Vec<(u64, u64)>) -> u64 {
        ranges.sort_unstable();
        let mut end = 0;
        for (s, e) in ranges {
            if s > end {
                break;
            }
            end = end.max(e);
        }
        end
    }

    /// The rows run from sample 0 without a gap or an overlap; returns
    /// their end.
    fn rows_in_order(ranges: &[(u64, u64)]) -> Res<u64> {
        let mut end = 0;
        for &(s, e) in ranges {
            if s != end {
                return Err(format!(
                    "the segments aren't continuous: one ends at {end}, the next starts at {s}"
                )
                .into());
            }
            end = e;
        }
        Ok(end)
    }

    fn check_command(args: &Args) -> Res<bool> {
        let dir = args.path(0, "<dir>")?;
        let promised = read_log(&args.path(1, "<log>")?)?;
        let audio = reference(&args.path(2, "<ref>")?)?;
        let length = args.length()?;
        let session = dir.join("session");
        let recovered = args.flags.get("recovered").is_some_and(|v| v == "yes");
        let mut store = Store::open(&dir.join("nota.db"))?;
        match run_checks(&session, &mut store, length, &promised, &audio, recovered) {
            Ok(line) => {
                writeln!(io::stdout(), "result ok {line}")?;
                Ok(true)
            }
            Err(e) => {
                writeln!(io::stdout(), "result FAIL {e}")?;
                Ok(false)
            }
        }
    }

    fn run_checks(
        session: &Path,
        store: &mut Store,
        length: SegmentLength,
        promised: &Promised,
        audio: &[i16],
        recovered: bool,
    ) -> Res<String> {
        // Before salvage: rows only with their files, and nothing fsync'd
        // missing from the rows and journals together.
        let before = Observed::read(session, store)?;
        let in_rows = row_ranges(session, &before, audio).map_err(|e| format!("before: {e}"))?;
        let mut held = journal_ranges(&before, audio).map_err(|e| format!("before: {e}"))?;
        held.extend(&in_rows);
        let on_disk = covered_from_zero(held);
        if on_disk < promised.durable {
            return Err(format!(
                "before: samples {on_disk}..{} were fsync'd but are gone",
                promised.durable
            )
            .into());
        }

        let ours = SessionDir::new(SESSION, StdFs, session).lock()?;
        let first = salvage(&mut SessionStore::new(ours.clone(), &mut *store), length)?;
        let after = Observed::read(session, store)?;
        if recovered && after != before {
            return Err("a completed salvage didn't last: salvage had to run again".into());
        }
        if let Some(left) = after
            .files
            .keys()
            .find(|p| is_journal(p) || is_temp_segment(p))
        {
            return Err(format!("salvage left {}", left.display()).into());
        }
        let ranges = row_ranges(session, &after, audio).map_err(|e| format!("after: {e}"))?;
        let recovered = rows_in_order(&ranges)?;
        if recovered < promised.durable {
            return Err(format!(
                "after: recovered up to {recovered}, but {} was fsync'd",
                promised.durable
            )
            .into());
        }
        rows_kept(session, promised, &after)?;

        let second = salvage(&mut SessionStore::new(ours, &mut *store), length)?;
        if second != Published::default() || Observed::read(session, store)? != after {
            return Err("a second salvage changed something".into());
        }

        let lag = Lag::of(promised);
        // The writer's own budget: the sync interval's worth of audio.
        let max_journal_lag = SampleCount::started_within(SYNC_INTERVAL, RATE)
            .ok_or("the sync interval overflows")?
            .get();
        if lag.max > max_journal_lag || lag.delivered_max > MAX_LAG || lag.wall_max > MAX_LAG {
            let behind = format!(
                "durable was {:.0} ms behind the journal (bound {:.0} ms), {:.0} ms behind \
                 the audio delivered and {:.0} ms behind the wall clock (bound {:.0} ms)",
                ms(lag.max),
                ms(max_journal_lag),
                ms(lag.delivered_max),
                ms(lag.wall_max),
                ms(MAX_LAG)
            );
            return Err((behind + &slow_ops(promised)).into());
        }
        if promised.overruns > 0 || promised.journal_failures > 0 {
            return Err(format!(
                "audio was lost before the journal: {} overruns, {} journal failures",
                promised.overruns, promised.journal_failures
            )
            .into());
        }
        // A second or more of silence means nothing was playing: the run
        // measured nothing.
        let peak = audio.iter().map(|s| s.saturating_abs()).max().unwrap_or(0);
        if audio.len() >= 16_000 && peak <= MIN_PEAK {
            return Err("the captured audio is silent".into());
        }
        let (stop, captured_at_end, delivered_at_end) = promised.at_end();
        Ok(format!(
            "stop={stop} delivered={delivered_at_end} captured={captured_at_end} durable={} \
             recovered={recovered} loss_ms={:.1} loss_delivered_ms={:.1} beyond_durable_ms={:.1} \
             lag_max_ms={:.1} delivered_lag_max_ms={:.1} wall_lag_max_ms={:.1} syncs={} rows={} \
             salvaged={} deleted={} late_ops={} slowest_op={} peak_dbfs={:.1} state={}",
            promised.durable,
            ms(captured_at_end.saturating_sub(recovered)),
            ms(delivered_at_end.saturating_sub(recovered)),
            ms(recovered.saturating_sub(promised.durable)),
            ms(lag.max),
            ms(lag.delivered_max),
            ms(lag.wall_max),
            promised.syncs.len(),
            after.rows.len(),
            first.segments().len(),
            first.deleted().len(),
            promised.late,
            promised
                .slow
                .iter()
                .max_by_key(|(_, _, took)| *took)
                .map_or_else(
                    || "none".to_owned(),
                    |(what, _, took)| format!("{what}:{}ms", took / 1_000_000)
                ),
            20.0 * (f64::from(peak.max(1)) / 32_768.0).log10(),
            after.digest()
        ))
    }

    /// The operations that took over 100 ms, for a failure message.
    fn slow_ops(p: &Promised) -> String {
        let t0 = p.first.map_or(0, |(t, _)| t);
        p.slow
            .iter()
            .fold(String::new(), |mut out, (what, t, took)| {
                let _ = write!(
                    out,
                    "; slow {what} at {:.3} s took {} ms",
                    ms(t.saturating_sub(t0) * u64::from(RATE.hz()) / 1_000_000_000) / 1_000.0,
                    took / 1_000_000
                );
                out
            })
    }

    /// Every committed row is still there, and every file has its row.
    fn rows_kept(session: &Path, promised: &Promised, after: &Observed) -> Res<()> {
        for (epoch, start, end, sha) in &promised.rows {
            let found = after.rows.iter().any(|r| {
                u64::from(r.epoch().get()) == *epoch
                    && r.range().start().get() == *start
                    && r.range().end().get() == *end
                    && hex(r.sha256().as_bytes()) == *sha
            });
            if !found {
                return Err(format!("a committed row disappeared: {start}..{end}").into());
            }
        }
        let named: Vec<PathBuf> = after
            .rows
            .iter()
            .map(|r| session.join(segment_file_name(r.track(), r.range())))
            .collect();
        // The session's marks are the one other file a recording leaves.
        let marks = session.join(MARKS_FILE_NAME);
        if let Some(orphan) = after
            .files
            .keys()
            .find(|p| !named.contains(*p) && **p != marks)
        {
            return Err(format!("a file without a row: {}", orphan.display()).into());
        }
        Ok(())
    }

    /// How far durable fell behind captured: just before each fsync, and at
    /// the stop.
    #[derive(Debug)]
    struct Lag {
        max: u64,
        /// Behind the audio delivered, which counts audio queued before the
        /// recorder too.
        delivered_max: u64,
        /// Behind the wall clock since the first frame.
        wall_max: u64,
    }

    impl Lag {
        fn of(p: &Promised) -> Self {
            let mut points: Vec<(u64, u64, u64, u64)> = p.syncs.clone();
            if let Some((_, _, _, t, captured, durable, delivered)) = &p.stop {
                points.push((*t, *captured, *durable, *delivered));
            }
            let max = points
                .iter()
                .map(|&(_, captured, durable, _)| captured.saturating_sub(durable))
                .max()
                .unwrap_or(0);
            let delivered_max = points
                .iter()
                .map(|&(_, _, durable, delivered)| delivered.saturating_sub(durable))
                .max()
                .unwrap_or(0);
            let wall_max = p.first.map_or(0, |(t0, end0)| {
                points
                    .iter()
                    .map(|&(t, _, durable, _)| {
                        let since = t.saturating_sub(t0);
                        let wall = end0 + since * u64::from(RATE.hz()) / 1_000_000_000;
                        wall.saturating_sub(durable)
                    })
                    .max()
                    .unwrap_or(0)
            });
            Self {
                max,
                delivered_max,
                wall_max,
            }
        }
    }

    // -----------------------------------------------------------------------
    // engine

    /// What the recorder side sends the engine feeder.
    #[derive(Debug)]
    enum Feed {
        Bytes(PathBuf, Vec<u8>),
        End,
    }

    fn engine_config(args: &Args) -> Res<EngineConfig> {
        let program = PathBuf::from(args.flag("engine")?);
        let args: Vec<OsString> = [
            "engine",
            "asr",
            "--parakeet",
            args.flag("parakeet")?,
            "--vad",
            args.flag("vad")?,
            "--threads",
            "2",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        Ok(EngineConfig::new(EngineCommand { program, args }))
    }

    /// Feeds each new journal frame to the engine, in order; returns the
    /// supervisor once the recorder has finished, after a flush.
    fn feeder(
        mut supervisor: EngineSupervisor,
        feed: &mpsc::Receiver<Feed>,
        sent: &AtomicU64,
    ) -> Res<EngineSupervisor> {
        let mut journals: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        while let Ok(Feed::Bytes(path, bytes)) = feed.recv() {
            let buf = journals.entry(path).or_default();
            buf.extend(bytes);
            for frame in read_journal(buf).frames() {
                let start = frame.range().start().get();
                let next = sent.load(Ordering::SeqCst);
                if start < next {
                    continue;
                }
                if start != next {
                    return Err(format!("the journals skip samples {next}..{start}").into());
                }
                let chunk =
                    AudioChunk::new(TRACK, frame.range().start(), RATE, frame.samples().to_vec())
                        .ok_or("a frame that isn't an audio chunk")?;
                supervisor
                    .send_audio(chunk)
                    .map_err(|e| format!("audio refused: {e:?}"))?;
                sent.store(frame.range().end().get(), Ordering::SeqCst);
            }
        }
        supervisor.flush(TRACK);
        Ok(supervisor)
    }

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace()
            .map(|w| {
                w.chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect::<String>()
                    .to_lowercase()
            })
            .filter(|w| !w.is_empty())
            .collect()
    }

    fn distance(a: &[String], b: &[String]) -> usize {
        let mut row: Vec<usize> = (0..=b.len()).collect();
        for (i, x) in a.iter().enumerate() {
            let mut prev = row[0];
            row[0] = i + 1;
            for (j, y) in b.iter().enumerate() {
                let here = (row[j + 1] + 1)
                    .min(row[j] + 1)
                    .min(prev + usize::from(x != y));
                prev = row[j + 1];
                row[j + 1] = here;
            }
        }
        row[b.len()]
    }

    fn since_ms(later: SessionTime, earlier: SessionTime) -> f64 {
        later
            .checked_duration_since(earlier)
            .map_or(f64::NAN, |d| d.as_secs_f64() * 1_000.0)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one measurement, start to finish, reads best in order"
    )]
    fn engine_command(args: &Args) -> Res<bool> {
        let dir = args.path(0, "<dir>")?;
        let source = Source::Device(args.flag("source")?.to_owned());
        let seconds = args.number("seconds")?;
        let kill_after = Duration::from_millis(args.number("kill-after-ms")?);
        let ready = PathBuf::from(args.flag("ready")?);
        let said = words(&String::from_utf8(
            StdFs.read(Path::new(args.flag("said")?))?,
        )?);
        let config = engine_config(args)?;

        let clock: Arc<dyn Clock> =
            Arc::new(SystemClock::start().map_err(|_| "no monotonic clock")?);
        let (supervisor, events) = EngineSupervisor::start(config, Arc::clone(&clock))?;
        let next = |within: Duration| -> Res<EngineEvent> {
            events
                .recv_timeout(within)
                .map_err(|_| "no engine event in time".into())
        };
        let mut pid = loop {
            if let EngineEvent::Status(EngineStatus::Online { pid }) =
                next(Duration::from_secs(60))?
            {
                break pid;
            }
        };

        let (feed_tx, feed_rx) = mpsc::channel();
        let mut tap = Tap::new(Arc::clone(&clock));
        tap.frames = Some(Mutex::new(feed_tx.clone()));
        let fs = TapFs(Arc::new(tap));
        let session = SessionDir::new(SESSION, fs, &dir).lock()?;
        // Short windows keep each journal small, as the feeder rereads it.
        let length =
            SegmentLength::new(SampleCount::new(10 * u64::from(RATE.hz()))).ok_or("bad length")?;
        let mut writer = SessionWriter::open(&session, RATE, length, Arc::clone(&clock))?;
        writer.start_track(TRACK, EpochId::new(0), SampleIndex::ZERO)?;

        let sent = Arc::new(AtomicU64::new(0));
        let feeding = {
            let sent = Arc::clone(&sent);
            thread::spawn(move || feeder(supervisor, &feed_rx, &sent))
        };
        let (capture, capture_events) = start(&PipeWireBackend, TRACK, &source, RATE, &clock)?;
        let mut timeline = TrackTimeline::new(TRACK);
        timeline.open_epoch(clock.now(), SampleIndex::ZERO, RATE)?;
        let recorder = thread::spawn(move || {
            // Audio lost before the journal: the engine never sees it either.
            let mut lost = 0_usize;
            let result = record_track(&mut writer, &mut timeline, &capture_events, &mut |e| {
                if matches!(
                    e,
                    RecorderEvent::JournalFailed(_)
                        | RecorderEvent::Capture(CaptureNotice::Overrun)
                ) {
                    lost += 1;
                }
            });
            (writer, result, lost)
        });
        let started = clock.now();
        let stop_at = started
            .checked_add(Duration::from_secs(seconds))
            .ok_or("bad duration")?;
        let mut marker = StdFs.create(&ready)?;
        marker.sync()?;

        let mut text = Vec::new();
        let mut first_text = None;
        let mut kill_due = None;
        let mut killed = None;
        let mut held_at_kill = 0;
        let mut sent_at_kill = 0;
        let mut resumed_from = None;
        let mut lost = 0;
        let mut offline_at = None;
        let mut online_again = None;
        let mut resumed = None;
        let mut skipped = 0_u64;
        let mut confirmed = 0_u64;
        let mut heard = false;
        let mut capturing = Some((capture, recorder));
        let mut supervisor = None;
        let mut feeding = Some(feeding);
        loop {
            let now = clock.now();
            if killed.is_none()
                && let Some(due) = kill_due
                && now >= due
            {
                sent_at_kill = sent.load(Ordering::SeqCst);
                held_at_kill = sent_at_kill.saturating_sub(confirmed);
                let status = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status()?;
                if !status.success() {
                    return Err("kill failed".into());
                }
                killed = Some(clock.now());
            }
            if now >= stop_at
                && let Some((capture, recorder)) = capturing.take()
            {
                drop(capture);
                let (writer, result, notices) = recorder
                    .join()
                    .map_err(|_| "the recorder thread panicked")?;
                result?;
                lost = notices;
                writer.finish()?;
                let _ = feed_tx.send(Feed::End);
                if let Some(f) = feeding.take() {
                    supervisor = Some(f.join().map_err(|_| "the feeder panicked")??);
                }
            }
            if supervisor.is_some() && confirmed >= sent.load(Ordering::SeqCst) {
                break;
            }
            if supervisor.is_some()
                && now
                    >= stop_at
                        .checked_add(Duration::from_secs(60))
                        .ok_or("bad duration")?
            {
                return Err("the engine never confirmed all the audio".into());
            }
            let event = match events.recv_timeout(Duration::from_millis(20)) {
                Ok(e) => e,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the supervisor went away".into());
                }
            };
            let at = clock.now();
            match event {
                EngineEvent::Transcript(t) => {
                    heard = true;
                    // Only text after the old engine is seen to go down:
                    // anything before that it sent before it died.
                    if offline_at.is_some() && resumed.is_none() {
                        resumed = Some(at);
                        resumed_from = Some(t.range().start().get());
                    }
                    text.push(t.into_text());
                }
                EngineEvent::Confirmed { up_to, .. } => {
                    confirmed = up_to.get();
                    if heard && first_text.is_none() {
                        first_text = Some(at);
                        kill_due = Some(at.checked_add(kill_after).ok_or("bad duration")?);
                    }
                }
                EngineEvent::Status(EngineStatus::Offline(_)) => {
                    if killed.is_some() && offline_at.is_none() {
                        offline_at = Some(at);
                    }
                }
                EngineEvent::Status(EngineStatus::Online { pid: new }) => {
                    pid = new;
                    if killed.is_some() && online_again.is_none() {
                        online_again = Some(at);
                    }
                }
                EngineEvent::Skipped { .. } => skipped += 1,
            }
        }
        if let Some(s) = supervisor {
            s.shutdown();
        }

        let total = sent.load(Ordering::SeqCst);
        let heard_words = words(&text.join(" "));
        let errors = distance(&heard_words, &said);
        let (Some(first_text), Some(killed)) = (first_text, killed) else {
            writeln!(io::stdout(), "result FAIL the engine was never killed")?;
            return Ok(false);
        };
        let took = resumed.map_or(f64::INFINITY, |r| since_ms(r, killed));
        // The first text after the restart covers audio sent before the
        // kill that the old engine hadn't confirmed: an utterance was cut.
        let cut = resumed_from.is_some_and(|from| from < sent_at_kill);
        let ok = resumed.is_some()
            && took < 10_000.0
            && cut
            && lost == 0
            && skipped == 0
            && confirmed >= total
            && errors * 10 <= said.len();
        writeln!(
            io::stdout(),
            "result {} kill_after_first_text_ms={:.0} held_at_kill_ms={:.0} \
             offline_ms={:.0} online_ms={:.0} text_resumed_ms={took:.0} cut_utterance={} \
             skipped={skipped} lost_before_journal={lost} confirmed={confirmed}/{total} \
             word_errors={errors}/{}",
            if ok { "ok" } else { "FAIL" },
            since_ms(killed, first_text),
            ms(held_at_kill),
            offline_at.map_or(f64::NAN, |t| since_ms(t, killed)),
            online_again.map_or(f64::NAN, |t| since_ms(t, killed)),
            if cut { "yes" } else { "no" },
            said.len()
        )?;
        Ok(ok)
    }
}
