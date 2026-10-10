//! The recorder loop, driven by a synthetic backend that sends audio from
//! its own thread, as an audio server's callback does.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRate, SessionId, SessionTime,
    TrackId, TrackTimeline,
};

use std::error::Error as _;

use super::*;
use crate::fs::fake::{FakeFile, FakeFs, FakeLock, FakeSyncer};
use crate::fs::{FileSyncer, Fs, FsFile, Synced};
use crate::journal::{JournalId, read_journal};
use crate::segment::SegmentLength;
use crate::session::SessionDir;

const MIC: TrackId = TrackId::new(0);
const SESSION: SessionId = SessionId::new(1);

fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

fn dir() -> PathBuf {
    PathBuf::from("/session")
}

fn samples(from: u64, len: u64) -> Vec<i16> {
    (from..from + len)
        .map(|i| i16::try_from(i % 30_000).unwrap())
        .collect()
}

#[derive(Debug, Clone)]
enum Step {
    Audio(Vec<i16>),
    /// Audio the stream stamped: its first sample was captured at this
    /// time on the clock that stops during suspend.
    TimedAudio(Vec<i16>, Duration),
    Notice(CaptureNotice),
    Fail(CaptureError),
    /// Moves the session clock on.
    Advance(Duration),
    /// Moves the session clock on as a suspend that long would.
    Suspend(Duration),
}

/// Sends its script from a thread of its own, then signals `sent`. When
/// stopped, it waits for that thread and sends `last` as it goes, like a
/// final callback racing the stop, then reports a failure, as cpal does
/// when a stream on a named `PipeWire` node is torn down.
#[derive(Debug)]
struct Synthetic {
    script: Vec<Step>,
    last: Vec<i16>,
    sent: mpsc::Sender<()>,
    clock: Arc<FakeClock>,
}

#[derive(Debug)]
struct SyntheticStream {
    events: CaptureSender,
    last: Vec<i16>,
    feeder: Option<thread::JoinHandle<()>>,
}

impl Drop for SyntheticStream {
    fn drop(&mut self) {
        if let Some(feeder) = self.feeder.take() {
            feeder.join().unwrap();
        }
        self.events.audio(&self.last);
        self.events
            .failed(CaptureError::Backend("torn down".into()));
    }
}

impl CaptureBackend for Synthetic {
    type Stream = SyntheticStream;

    fn start(
        &self,
        source: &Source,
        start_rate: SampleRate,
        events: CaptureSender,
    ) -> Result<SyntheticStream, CaptureError> {
        assert_eq!(*source, Source::Microphone);
        assert_eq!(start_rate, rate());
        let script = self.script.clone();
        let sent = self.sent.clone();
        let clock = Arc::clone(&self.clock);
        let feeder_events = events.clone();
        let feeder = thread::spawn(move || {
            for step in script {
                match step {
                    Step::Audio(s) => feeder_events.audio(&s),
                    Step::TimedAudio(s, stamp) => feeder_events.audio_captured(&s, stamp),
                    Step::Notice(n) => feeder_events.notice(n),
                    Step::Fail(e) => feeder_events.failed(e),
                    Step::Advance(by) => clock.advance(by),
                    Step::Suspend(by) => clock.suspend(by),
                }
            }
            sent.send(()).unwrap();
        });
        Ok(SyntheticStream {
            events,
            last: self.last.clone(),
            feeder: Some(feeder),
        })
    }
}

/// A backend that can't open its stream.
#[derive(Debug)]
struct Unavailable;

impl CaptureBackend for Unavailable {
    type Stream = ();

    fn start(&self, source: &Source, _: SampleRate, _: CaptureSender) -> Result<(), CaptureError> {
        Err(CaptureError::DeviceNotAvailable(source.clone()))
    }
}

struct Run<S: Fs> {
    writer: SessionWriter<S>,
    timeline: TrackTimeline,
    result: Result<(), RecordError>,
    reported: Vec<RecorderEvent>,
}

/// Records `script` and `last` from a [`Synthetic`] backend on a recorder
/// thread, with windows of 1,000 samples at 1 kHz, in epoch 0 from session
/// time zero. `while_running` runs once the script is sent, before the
/// capture stops.
fn run<S: Fs + Clone + 'static>(
    fs: &S,
    script: Vec<Step>,
    last: Vec<i16>,
    while_running: impl FnOnce(&FakeClock),
) -> Run<S> {
    run_in(fs, epoch_zero(), script, last, while_running)
}

/// [`run`], with `timeline` as the track's.
fn run_in<S: Fs + Clone + 'static>(
    fs: &S,
    mut timeline: TrackTimeline,
    script: Vec<Step>,
    last: Vec<i16>,
    while_running: impl FnOnce(&FakeClock),
) -> Run<S> {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let dyn_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    let session = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, Arc::clone(&dyn_clock)).unwrap();
    writer
        .start_track(MIC, timeline.current().unwrap())
        .unwrap();
    let (sent, script_sent) = mpsc::channel();
    let backend = Synthetic {
        script,
        last,
        sent,
        clock: Arc::clone(&clock),
    };
    let (capture, events) = start(&backend, MIC, &Source::Microphone, rate(), &dyn_clock).unwrap();
    let (finished, done) = mpsc::channel();
    thread::spawn(move || {
        let mut reported = Vec::new();
        let result = record_track(&mut writer, &mut timeline, &events, &mut |e| {
            reported.push(e);
        });
        let _ = finished.send(Run {
            writer,
            timeline,
            result,
            reported,
        });
    });
    script_sent.recv().unwrap();
    while_running(&clock);
    drop(capture);
    // A recorder that never returns fails the test rather than hanging it.
    done.recv_timeout(Duration::from_secs(10)).unwrap()
}

/// The finished journals' audio, in id order: each journal's range must
/// start where the previous one ended.
fn journaled(fs: &FakeFs, journals: &[FinishedJournal]) -> Vec<i16> {
    let mut out = Vec::new();
    for journal in journals {
        assert_eq!(journal.session(), SESSION);
        let bytes = fs.read(&dir().join(journal.id().file_name())).unwrap();
        let (range, audio) = read_journal(&bytes).audio().unwrap();
        assert_eq!(range.start().get(), out.len() as u64, "{journal:?}");
        out.extend(audio);
    }
    out
}

/// `MIC`'s timeline with epoch 0 open at session time zero, at [`rate`].
fn epoch_zero() -> TrackTimeline {
    let mut timeline = TrackTimeline::new(MIC);
    timeline
        .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
        .unwrap();
    timeline
}

fn finished_reported(reported: &[RecorderEvent]) -> Vec<FinishedJournal> {
    reported
        .iter()
        .filter_map(|e| match e {
            RecorderEvent::Finished(j) => Some(
                j.iter()
                    .map(|j| FinishedJournal::new(j.session(), j.id()))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Every journal a run finished: those reported while it recorded, then
/// the rest as `writer` finishes.
fn all_journals<S: Fs>(
    writer: SessionWriter<S>,
    reported: &[RecorderEvent],
) -> Vec<FinishedJournal> {
    let mut all = finished_reported(reported);
    all.extend(writer.finish().unwrap());
    all
}

/// A file operation [`Watched`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileOp {
    Write,
    Sync,
}

/// A [`FakeFs`] that reports every file write and fsync, once done, so a
/// test can wait for the recorder thread to reach one.
#[derive(Debug, Clone)]
struct Watched {
    fs: FakeFs,
    ops: mpsc::Sender<FileOp>,
}

#[derive(Debug)]
struct WatchedFile {
    file: FakeFile,
    ops: mpsc::Sender<FileOp>,
}

/// A [`WatchedFile`]'s fsyncs from a sync thread, reported as its own.
#[derive(Debug)]
struct WatchedSyncer {
    syncer: FakeSyncer,
    ops: mpsc::Sender<FileOp>,
}

impl FsFile for WatchedFile {
    type Syncer = WatchedSyncer;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        let _ = self.ops.send(FileOp::Write);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<Synced> {
        let synced = self.file.sync()?;
        let _ = self.ops.send(FileOp::Sync);
        Ok(synced)
    }

    fn syncer(&self) -> io::Result<WatchedSyncer> {
        Ok(WatchedSyncer {
            syncer: self.file.syncer()?,
            ops: self.ops.clone(),
        })
    }
}

impl FileSyncer for WatchedSyncer {
    fn sync(&self) -> io::Result<Synced> {
        let synced = self.syncer.sync()?;
        let _ = self.ops.send(FileOp::Sync);
        Ok(synced)
    }
}

impl Fs for Watched {
    type File = WatchedFile;
    type Lock = FakeLock;

    fn create(&self, path: &Path) -> io::Result<WatchedFile> {
        Ok(WatchedFile {
            file: self.fs.create(path)?,
            ops: self.ops.clone(),
        })
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.fs.create_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.fs.rename(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.fs.sync_dir(dir)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.fs.remove(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.fs.read(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.fs.list(dir)
    }

    fn lock_dir(&self, dir: &Path) -> io::Result<FakeLock> {
        self.fs.lock_dir(dir)
    }
}

/// The next file operation on the recorder thread; fails the test after
/// 10 s without one.
fn next_op(ops: &mpsc::Receiver<FileOp>) -> FileOp {
    ops.recv_timeout(Duration::from_secs(10)).unwrap()
}

#[test]
fn synthetic_audio_is_journaled_in_order_and_rotates_at_windows() {
    let fs = FakeFs::with_dirs([dir()]);
    let mut script = Vec::new();
    let mut at = 0;
    for len in [700_u64, 1, 299, 450, 550, 500] {
        script.push(Step::Audio(samples(at, len)));
        at += len;
    }
    let run = run(&fs, script, samples(at, 100), |_| {});
    run.result.unwrap();
    let total = at + 100;
    assert_eq!(run.writer.next_sample(MIC), Some(SampleIndex::new(total)));

    // The two full windows were handed out while recording, in order; the
    // last journal ends with the writer.
    let during = finished_reported(&run.reported);
    let ids: Vec<u64> = during.iter().map(|j| j.id().get()).collect();
    assert_eq!(ids, [0, 1]);
    let rest = run.writer.finish().unwrap();
    let ids: Vec<u64> = rest.iter().map(|j| j.id().get()).collect();
    assert_eq!(ids, [2]);

    let all: Vec<FinishedJournal> = during.into_iter().chain(rest).collect();
    assert_eq!(journaled(&fs, &all), samples(0, total));
    assert_eq!(JournalId::FIRST, all[0].id());
}

#[test]
fn audio_sent_as_the_stream_stops_is_recorded_and_a_failure_ignored() {
    // `last`, and a failure, go out while the stream is being dropped:
    // before `Stopped`.
    let fs = FakeFs::with_dirs([dir()]);
    let run = run(
        &fs,
        vec![Step::Audio(samples(0, 10))],
        samples(10, 5),
        |_| {},
    );
    run.result.unwrap();
    assert_eq!(run.writer.next_sample(MIC), Some(SampleIndex::new(15)));
    let journals = run.writer.finish().unwrap();
    assert_eq!(journaled(&fs, &journals), samples(0, 15));
}

#[test]
fn notices_are_reported_and_recording_goes_on() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::Audio(samples(0, 10)),
        Step::Notice(CaptureNotice::Overrun),
        Step::Audio(samples(10, 10)),
        Step::Notice(CaptureNotice::RouteChanged),
        Step::Notice(CaptureNotice::Warning("watch lost".into())),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    let notices: Vec<&CaptureNotice> = run
        .reported
        .iter()
        .filter_map(|e| match e {
            RecorderEvent::Capture(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        [
            &CaptureNotice::Overrun,
            &CaptureNotice::RouteChanged,
            &CaptureNotice::Warning("watch lost".into())
        ]
    );
    let journals = all_journals(run.writer, &run.reported);
    assert_eq!(journaled(&fs, &journals), samples(0, 20));
}

#[test]
fn a_failed_stream_stops_recording_after_what_it_sent() {
    let fs = FakeFs::with_dirs([dir()]);
    let failure = CaptureError::DeviceNotAvailable(Source::Microphone);
    let script = vec![
        Step::Audio(samples(0, 10)),
        Step::Fail(failure.clone()),
        Step::Audio(samples(10, 10)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    match run.result {
        Err(RecordError::Capture(e)) => assert_eq!(e, failure),
        other => panic!("{other:?}"),
    }
    assert_eq!(run.writer.next_sample(MIC), Some(SampleIndex::new(10)));
    let journals = run.writer.finish().unwrap();
    assert_eq!(journaled(&fs, &journals), samples(0, 10));
}

#[test]
fn journals_are_synced_while_no_audio_arrives() {
    let fs = FakeFs::with_dirs([dir()]);
    let (tx, ops) = mpsc::channel();
    let watched = Watched {
        fs: fs.clone(),
        ops: tx,
    };
    let journal = dir().join(JournalId::FIRST.file_name());
    let written = || {
        fs.read(&journal)
            .ok()
            .and_then(|b| read_journal(&b).audio())
            .is_some_and(|(r, _)| r.end().get() == 300)
    };
    let run = run(
        &watched,
        vec![Step::Audio(samples(0, 300))],
        Vec::new(),
        |clock| {
            // Every sample written (the new journal's header is synced on
            // the way), and every report of it taken; the samples aren't
            // due an fsync yet, so nothing else happens until the clock
            // moves.
            while !written() {
                next_op(&ops);
            }
            while ops.recv_timeout(Duration::from_millis(300)).is_ok() {}
            // No more audio comes: only the recorder's idle check can sync.
            clock.advance(Duration::from_secs(2));
            assert_eq!(next_op(&ops), FileOp::Sync);
        },
    );
    run.result.unwrap();
    let durable = run.writer.durable(MIC).unwrap();
    assert_eq!(durable.end(), SampleIndex::new(300));
}

#[test]
fn a_journal_that_cant_be_replaced_is_reported_and_the_track_moves_on() {
    let fs = FakeFs::with_dirs([dir()]);
    // Lets the listing at open, the marks' write (five operations) and the
    // first journal's creation (four) through, then fails everything from
    // the first frame write on.
    fs.crash_after(10);
    let script = (0..5).map(|i| Step::Audio(samples(i * 10, 10))).collect();
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert!(
        run.reported
            .iter()
            .any(|e| matches!(e, RecorderEvent::JournalFailed(SessionError::Journal(_)))),
        "{:?}",
        run.reported
    );
    assert_eq!(run.writer.next_sample(MIC), Some(SampleIndex::new(50)));
}

#[test]
fn a_marks_write_that_fails_is_a_gap_and_the_track_moves_on() {
    let fs = FakeFs::with_dirs([dir()]);
    // Lets the listing at open through, then fails creating the marks'
    // temp file once: the first audio has no journal to go to.
    fs.fail_after(1, io::ErrorKind::PermissionDenied);
    let script = (0..3).map(|i| Step::Audio(samples(i * 10, 10))).collect();
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert!(
        run.reported
            .iter()
            .any(|e| matches!(e, RecorderEvent::JournalFailed(SessionError::Marks(_)))),
        "{:?}",
        run.reported
    );
    assert_eq!(run.writer.next_sample(MIC), Some(SampleIndex::new(30)));
    // The next audio reserved its id and started a journal at sample 10.
    let finished = run.writer.finish().unwrap();
    let bytes = fs.read(&dir().join(finished[0].id().file_name())).unwrap();
    let (range, _) = read_journal(&bytes).audio().unwrap();
    assert_eq!(range.start(), SampleIndex::new(10));
}

/// The same failure for want of space is tried again at once: the first
/// failure has freed the ballast, if the recording keeps one, so nothing
/// is lost.
#[test]
fn a_marks_write_that_meets_a_full_disk_once_is_tried_again() {
    let fs = FakeFs::with_dirs([dir()]);
    fs.fail_after(1, io::ErrorKind::StorageFull);
    let script = (0..3).map(|i| Step::Audio(samples(i * 10, 10))).collect();
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert!(
        !run.reported
            .iter()
            .any(|e| matches!(e, RecorderEvent::JournalFailed(_))),
        "{:?}",
        run.reported
    );
    let finished = run.writer.finish().unwrap();
    let bytes = fs.read(&dir().join(finished[0].id().file_name())).unwrap();
    let (range, _) = read_journal(&bytes).audio().unwrap();
    assert_eq!(
        (range.start(), range.end()),
        (SampleIndex::ZERO, SampleIndex::new(30))
    );
}

#[test]
fn recording_an_unstarted_track_is_an_error() {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs, &dir()).lock().unwrap();
    let mut writer = SessionWriter::open(
        &session,
        rate(),
        SegmentLength::new(SampleCount::new(1_000)).unwrap(),
        clock,
    )
    .unwrap();
    let (tx, rx) = test_channel();
    tx.send(MIC, CaptureEvent::Audio(samples(0, 10)));
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC]),
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        thresholds: BTreeMap::new(),
    };
    let mut timeline = epoch_zero();
    let result = record_track(&mut writer, &mut timeline, &events, &mut |_| {});
    assert!(matches!(
        result,
        Err(RecordError::Session(SessionError::UnknownTrack(MIC)))
    ));
}

#[test]
fn a_channel_with_no_senders_reads_as_stopped() {
    let (tx, rx) = test_channel();
    let rx = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC]),
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        thresholds: BTreeMap::new(),
    };
    assert!(matches!(rx.next(Duration::from_millis(1)), Received::Idle));
    let sender = CaptureSender {
        events: tx.clone(),
        track: MIC,
        progress: Progress::new(SampleIndex::ZERO, SampleIndex::ZERO),
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        stopping: Arc::new(AtomicBool::new(false)),
        failed: Arc::new(AtomicBool::new(false)),
        began: Arc::new(AtomicBool::new(false)),
        rerouted: Arc::new(AtomicBool::new(false)),
        asleep: Arc::new(AtomicU64::new(0)),
        rate: rate(),
    };
    sender.audio(&[]);
    assert!(matches!(rx.next(Duration::from_millis(1)), Received::Idle));
    drop(tx);
    assert!(matches!(rx.next(Duration::from_millis(1)), Received::Idle));
    drop(sender);
    assert!(matches!(
        rx.next(Duration::from_millis(1)),
        Received::Closed
    ));
}

#[test]
fn a_stream_that_cant_open_is_an_error() {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let err = start(
        &Unavailable,
        MIC,
        &Source::Device("nowhere".into()),
        rate(),
        &clock,
    )
    .unwrap_err();
    assert_eq!(
        err,
        CaptureError::DeviceNotAvailable(Source::Device("nowhere".into()))
    );
    assert_eq!(err.to_string(), "device nowhere isn't available");
}

#[test]
fn record_errors_show_and_chain_their_cause() {
    let capture = CaptureError::Backend("gone".into());
    let e = RecordError::Capture(capture.clone());
    assert_eq!(e.to_string(), capture.to_string());
    assert_eq!(e.source().unwrap().to_string(), capture.to_string());
    let session = SessionError::UnknownTrack(MIC);
    let message = session.to_string();
    let e = RecordError::Session(session);
    assert_eq!(e.to_string(), message);
    assert_eq!(e.source().unwrap().to_string(), message);
}

#[test]
fn capture_errors_and_sources_read_plainly() {
    let cases = [
        (
            CaptureError::HostUnavailable("x".into()),
            "the audio server isn't available: x",
        ),
        (
            CaptureError::DeviceNotAvailable(Source::SystemAudio),
            "the system audio isn't available",
        ),
        (
            CaptureError::DeviceNotAvailable(Source::Microphone),
            "the microphone isn't available",
        ),
        (
            CaptureError::UnsupportedConfig("x".into()),
            "the device can't capture as asked: x",
        ),
        (CaptureError::Backend("x".into()), "capture failed: x"),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
}

#[test]
fn a_stream_at_another_rate_than_the_journals_records_nothing() {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs, &dir()).lock().unwrap();
    let mut writer = SessionWriter::open(
        &session,
        rate(),
        SegmentLength::new(SampleCount::new(1_000)).unwrap(),
        clock,
    )
    .unwrap();
    writer
        .start_track(
            MIC,
            &writer.test_epoch(MIC, EpochId::new(0), SampleIndex::ZERO),
        )
        .unwrap();
    let (tx, rx) = test_channel();
    tx.send(MIC, CaptureEvent::Audio(samples(0, 10)));
    let other = SampleRate::new(rate().hz() * 2).unwrap();
    let events = CaptureReceiver {
        events: rx,
        rate: other,
        tracks: test_tracks(&[MIC]),
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        thresholds: BTreeMap::new(),
    };
    let mut timeline = epoch_zero();
    let result = record_track(&mut writer, &mut timeline, &events, &mut |_| {});
    let Err(error) = result else {
        panic!("recorded at the wrong rate");
    };
    assert!(matches!(
        error,
        RecordError::RateMismatch { capture, journal } if capture == other && journal == rate()
    ));
    assert!(error.to_string().contains("Hz"));
    assert_eq!(writer.next_sample(MIC), Some(SampleIndex::ZERO));
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn at(s: u64) -> SessionTime {
    SessionTime::ZERO.checked_add(secs(s)).unwrap()
}

/// What `reported` says about epochs, in order.
fn epochs_reported(reported: &[RecorderEvent]) -> Vec<&RecorderEvent> {
    reported
        .iter()
        .filter(|e| {
            matches!(
                e,
                RecorderEvent::Epoch(_)
                    | RecorderEvent::EpochRefused(_)
                    | RecorderEvent::Capture(CaptureNotice::Overrun)
            )
        })
        .collect()
}

/// Each finished journal's epoch and range, in id order.
fn journal_epochs(fs: &FakeFs, journals: &[FinishedJournal]) -> Vec<(u32, u64, u64)> {
    journals
        .iter()
        .map(|j| {
            let bytes = fs.read(&dir().join(j.id().file_name())).unwrap();
            let read = read_journal(&bytes);
            let range = read.range().unwrap();
            (
                read.header().unwrap().epoch().get(),
                range.start().get(),
                range.end().get(),
            )
        })
        .collect()
}

#[test]
fn an_overrun_starts_a_new_epoch_at_its_time_and_the_loss_is_a_gap() {
    let fs = FakeFs::with_dirs([dir()]);
    // Half a second of audio, then an overrun reported at 2 s: 1.5 s lost.
    let script = vec![
        Step::Audio(samples(0, 500)),
        Step::Advance(secs(2)),
        Step::Notice(CaptureNotice::Overrun),
        Step::Advance(secs(1)),
        Step::Audio(samples(500, 300)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();

    let epoch = run.timeline.current().copied().unwrap();
    assert_eq!(epoch.id(), EpochId::new(1));
    assert_eq!(epoch.start(), at(2));
    assert_eq!(epoch.first_sample(), SampleIndex::new(500));
    assert_eq!(epoch.rate(), rate());
    assert_eq!(run.timeline.epochs().len(), 2);
    assert!(matches!(
        epochs_reported(&run.reported)[..],
        [
            RecorderEvent::Capture(CaptureNotice::Overrun),
            RecorderEvent::Epoch(e)
        ] if *e == epoch
    ));

    // The samples after the overrun keep their own time; the loss is a gap.
    let half = SessionTime::from_nanos(500_000_000);
    assert_eq!(
        run.timeline.time_of(SampleIndex::new(499)),
        Some(SessionTime::from_nanos(499_000_000))
    );
    assert_eq!(run.timeline.time_of(SampleIndex::new(500)), Some(at(2)));
    let gaps: Vec<_> = run.timeline.gaps().collect();
    assert_eq!(gaps.len(), 1);
    assert_eq!((gaps[0].from(), gaps[0].to()), (half, at(2)));

    // The journals follow: the new epoch starts a new journal, and the
    // sample count runs on.
    assert_eq!(
        run.writer.epoch(MIC).map(|e| (e.id(), e.first_sample())),
        Some((EpochId::new(1), SampleIndex::new(500)))
    );
    assert_eq!(run.writer.next_sample(MIC), Some(SampleIndex::new(800)));
    let journals = all_journals(run.writer, &run.reported);
    assert_eq!(journal_epochs(&fs, &journals), [(0, 0, 500), (1, 500, 800)]);
    assert_eq!(journaled(&fs, &journals), samples(0, 800));
}

#[test]
fn overruns_with_no_audio_between_leave_an_empty_epoch() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::Audio(samples(0, 100)),
        Step::Advance(secs(1)),
        Step::Notice(CaptureNotice::Overrun),
        Step::Advance(secs(1)),
        Step::Notice(CaptureNotice::Overrun),
        Step::Audio(samples(100, 100)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    let starts: Vec<(u32, SessionTime, u64)> = run
        .timeline
        .epochs()
        .iter()
        .map(|e| (e.id().get(), e.start(), e.first_sample().get()))
        .collect();
    assert_eq!(
        starts,
        [(0, SessionTime::ZERO, 0), (1, at(1), 100), (2, at(2), 100)]
    );
    assert_eq!(
        run.writer.epoch(MIC).map(|e| (e.id(), e.first_sample())),
        Some((EpochId::new(2), SampleIndex::new(100)))
    );
    let journals = all_journals(run.writer, &run.reported);
    assert_eq!(journal_epochs(&fs, &journals), [(0, 0, 100), (2, 100, 200)]);
}

/// A warning is only reported: the stream carried on, so the epoch does.
#[test]
fn a_warning_leaves_the_epoch_alone() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::Audio(samples(0, 100)),
        Step::Advance(secs(1)),
        Step::Notice(CaptureNotice::Warning("x".into())),
        Step::Audio(samples(100, 100)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert_eq!(run.timeline.epochs().len(), 1);
    assert_eq!(
        run.writer.epoch(MIC).map(|e| (e.id(), e.first_sample())),
        Some((EpochId::new(0), SampleIndex::ZERO))
    );
    assert!(epochs_reported(&run.reported).is_empty());
}

/// What `reported` says about epochs, route changes and suspends, in
/// order, as (what, epoch id, start, first sample).
fn reopenings(reported: &[RecorderEvent]) -> Vec<(&'static str, u32, SessionTime, u64)> {
    reported
        .iter()
        .filter_map(|e| match e {
            RecorderEvent::Capture(CaptureNotice::RouteChanged) => {
                Some(("route", 0, SessionTime::ZERO, 0))
            }
            RecorderEvent::Capture(CaptureNotice::Suspended) => {
                Some(("suspend", 0, SessionTime::ZERO, 0))
            }
            RecorderEvent::Epoch(e) => {
                Some(("epoch", e.id().get(), e.start(), e.first_sample().get()))
            }
            _ => None,
        })
        .collect()
}

/// After a route change, the next audio opens a new epoch when it was
/// captured: the stretch while the stream moved is a gap.
#[test]
fn a_route_change_opens_an_epoch_at_the_next_audio() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::Audio(samples(0, 100)),
        Step::Advance(secs(1)),
        Step::Notice(CaptureNotice::RouteChanged),
        // The new device's first 100 ms of audio arrive at 3 s.
        Step::Advance(secs(2)),
        Step::Audio(samples(100, 100)),
        Step::Audio(samples(200, 100)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    let reopened = SessionTime::from_nanos(2_900_000_000);
    assert_eq!(
        reopenings(&run.reported),
        [
            ("route", 0, SessionTime::ZERO, 0),
            ("epoch", 1, reopened, 100)
        ]
    );
    assert_eq!(run.timeline.time_of(SampleIndex::new(100)), Some(reopened));
    let gaps: Vec<_> = run.timeline.gaps().map(|g| (g.from(), g.to())).collect();
    assert_eq!(gaps, [(SessionTime::from_nanos(100_000_000), reopened)]);
    let journals = all_journals(run.writer, &run.reported);
    assert_eq!(journal_epochs(&fs, &journals), [(0, 0, 100), (1, 100, 300)]);
}

/// After a suspend, the first audio opens a new epoch when it was
/// captured, and the suspend is reported.
#[test]
fn a_suspend_opens_an_epoch_at_the_audio_after_it() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::Audio(samples(0, 100)),
        Step::Advance(Duration::from_millis(100)),
        Step::Suspend(secs(60)),
        Step::Audio(samples(100, 100)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    // The clock reads 60.1 s; the 100 samples just delivered began 0.1 s
    // before.
    let resumed = at(60);
    assert_eq!(
        reopenings(&run.reported),
        [
            ("suspend", 0, SessionTime::ZERO, 0),
            ("epoch", 1, resumed, 100)
        ]
    );
    let gaps: Vec<_> = run.timeline.gaps().map(|g| (g.from(), g.to())).collect();
    assert_eq!(gaps, [(SessionTime::from_nanos(100_000_000), resumed)]);
    let journals = all_journals(run.writer, &run.reported);
    assert_eq!(journal_epochs(&fs, &journals), [(0, 0, 100), (1, 100, 200)]);
}

/// An overrun reported for the same stretch as a route change, just before
/// its audio, opens the one epoch: the reopening isn't refused, and the
/// audio is timed from the overrun.
#[test]
fn an_overrun_and_a_route_change_open_one_epoch_for_one_gap() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::Audio(samples(0, 100)),
        Step::Notice(CaptureNotice::RouteChanged),
        Step::Advance(secs(2)),
        Step::Notice(CaptureNotice::Overrun),
        Step::Audio(samples(100, 100)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert!(
        !run.reported
            .iter()
            .any(|e| matches!(e, RecorderEvent::EpochRefused(_))),
        "{:?}",
        run.reported
    );
    let starts: Vec<_> = run
        .timeline
        .epochs()
        .iter()
        .map(|e| (e.id().get(), e.start(), e.first_sample().get()))
        .collect();
    assert_eq!(starts, [(0, SessionTime::ZERO, 0), (1, at(2), 100)]);
}

/// An overrun reported as the stream moves, before a stall, leaves the
/// stall to the reopening's own epoch: the audio after it is timed when it
/// was captured, not from the overrun.
#[test]
fn a_stall_after_an_overrun_gets_its_own_epoch() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::Audio(samples(0, 100)),
        Step::Notice(CaptureNotice::RouteChanged),
        Step::Advance(secs(1)),
        Step::Notice(CaptureNotice::Overrun),
        // The new device's first audio comes 2 s later.
        Step::Advance(secs(2)),
        Step::Audio(samples(100, 100)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    let starts: Vec<_> = run
        .timeline
        .epochs()
        .iter()
        .map(|e| (e.id().get(), e.start(), e.first_sample().get()))
        .collect();
    assert_eq!(
        starts,
        [
            (0, SessionTime::ZERO, 0),
            (1, at(1), 100),
            (2, SessionTime::from_nanos(2_900_000_000), 100)
        ]
    );
}

/// A blip in the suspend count shorter than a real suspend, and a suspend
/// before the stream's first audio, open no epoch.
#[test]
fn only_a_suspend_while_the_stream_runs_opens_an_epoch() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::Suspend(secs(5)),
        Step::Audio(samples(0, 100)),
        Step::Suspend(Duration::from_millis(99)),
        Step::Audio(samples(100, 100)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert_eq!(run.timeline.epochs().len(), 1);
    assert!(
        reopenings(&run.reported).iter().all(|r| r.0 == "suspend"),
        "{:?}",
        reopenings(&run.reported)
    );
}

#[test]
fn an_epoch_the_timeline_refuses_is_reported_and_recording_goes_on() {
    let fs = FakeFs::with_dirs([dir()]);
    // Epoch 0 starts at 10 s, so its second of audio runs to 11 s: an
    // overrun reported at 0 s is far too early to be drift.
    let mut timeline = TrackTimeline::new(MIC);
    timeline
        .open_epoch(at(10), SampleIndex::ZERO, rate())
        .unwrap();
    let script = vec![
        Step::Audio(samples(0, 1_000)),
        Step::Notice(CaptureNotice::Overrun),
        Step::Audio(samples(1_000, 10)),
    ];
    let run = run_in(&fs, timeline, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert!(matches!(
        epochs_reported(&run.reported)[..],
        [
            RecorderEvent::Capture(CaptureNotice::Overrun),
            RecorderEvent::EpochRefused(EpochError::ImplausibleOverrun { .. })
        ]
    ));
    assert_eq!(run.timeline.epochs().len(), 1);
    assert_eq!(
        run.writer.epoch(MIC).map(|e| (e.id(), e.first_sample())),
        Some((EpochId::new(0), SampleIndex::ZERO))
    );
    assert_eq!(run.writer.next_sample(MIC), Some(SampleIndex::new(1_010)));
}

#[test]
fn a_journal_that_breaks_at_the_overrun_is_reported_and_the_epoch_still_moves() {
    let fs = FakeFs::with_dirs([dir()]);
    // Lets the listing at open, the marks' write (five operations), the
    // first journal's creation (four) and its frame through, then fails
    // everything from the fsync that ends it at the overrun.
    fs.crash_after(11);
    let script = vec![
        Step::Audio(samples(0, 10)),
        Step::Advance(secs(1)),
        Step::Notice(CaptureNotice::Overrun),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    // Nothing failed before the overrun; ending the old epoch's journal
    // did, after the track moved.
    let failed = run
        .reported
        .iter()
        .position(|e| matches!(e, RecorderEvent::JournalFailed(_)));
    let moved = run
        .reported
        .iter()
        .position(|e| matches!(e, RecorderEvent::Epoch(_)));
    assert!(
        matches!((moved, failed), (Some(m), Some(f)) if m < f),
        "{:?}",
        run.reported
    );
    assert_eq!(
        run.writer.epoch(MIC).map(|e| (e.id(), e.first_sample())),
        Some((EpochId::new(1), SampleIndex::new(10)))
    );
    assert_eq!(run.timeline.current().map(Epoch::id), Some(EpochId::new(1)));
}

/// A writer with `MIC` started in `epoch` at sample `at`, and a channel
/// holding some audio for it.
fn started_in(epoch: EpochId, at: u64) -> (FakeFs, SessionWriter<FakeFs>, CaptureReceiver) {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    let mut writer = SessionWriter::open(
        &session,
        rate(),
        SegmentLength::new(SampleCount::new(1_000)).unwrap(),
        clock,
    )
    .unwrap();
    writer
        .start_track(MIC, &writer.test_epoch(MIC, epoch, SampleIndex::new(at)))
        .unwrap();
    let (tx, rx) = test_channel();
    tx.send(MIC, CaptureEvent::Audio(samples(0, 10)));
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC]),
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        thresholds: BTreeMap::new(),
    };
    (fs, writer, events)
}

#[test]
fn a_timeline_out_of_step_with_the_writer_records_nothing() {
    let other_rate = SampleRate::new(rate().hz() * 2).unwrap();
    let mut at_other_rate = TrackTimeline::new(MIC);
    at_other_rate
        .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, other_rate)
        .unwrap();
    let mut other_track = TrackTimeline::new(TrackId::new(1));
    other_track
        .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
        .unwrap();
    let mut from_100 = TrackTimeline::new(MIC);
    from_100
        .open_epoch(SessionTime::ZERO, SampleIndex::new(100), rate())
        .unwrap();
    let cases = [
        // No epoch open.
        (EpochId::new(0), 0, TrackTimeline::new(MIC), "empty"),
        // Epoch 0, but the writer records epoch 1.
        (EpochId::new(1), 0, epoch_zero(), "epoch"),
        (EpochId::new(0), 0, at_other_rate, "rate"),
        // The same epoch, from another first sample, either way round.
        (EpochId::new(0), 100, epoch_zero(), "writer ahead"),
        (EpochId::new(0), 0, from_100, "timeline ahead"),
    ];
    for (epoch, at, mut timeline, case) in cases {
        let (_fs, mut writer, events) = started_in(epoch, at);
        let before = timeline.clone();
        let result = record_track(&mut writer, &mut timeline, &events, &mut |_| {});
        let Err(error) = result else {
            panic!("{case}: recorded out of step");
        };
        assert!(matches!(error, RecordError::TimelineMismatch), "{case}");
        assert!(error.to_string().contains("timeline"), "{case}");
        assert!(std::error::Error::source(&error).is_none(), "{case}");
        assert_eq!(
            writer.next_sample(MIC),
            Some(SampleIndex::new(at)),
            "{case}"
        );
        assert_eq!(timeline, before, "{case}");
    }
    // A timeline for a track the writer didn't start.
    let (_fs, mut writer, events) = started_in(EpochId::new(0), 0);
    let result = record_track(&mut writer, &mut other_track, &events, &mut |_| {});
    assert!(matches!(
        result,
        Err(RecordError::Session(SessionError::UnknownTrack(t))) if t == TrackId::new(1)
    ));
    assert_eq!(writer.next_sample(MIC), Some(SampleIndex::ZERO));
}

#[test]
fn notices_carry_the_time_they_were_reported() {
    let (tx, rx) = test_channel();
    let clock = Arc::new(FakeClock::new(at(7)));
    let sender = CaptureSender {
        events: tx,
        track: MIC,
        progress: Progress::new(SampleIndex::ZERO, SampleIndex::ZERO),
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        stopping: Arc::new(AtomicBool::new(false)),
        failed: Arc::new(AtomicBool::new(false)),
        began: Arc::new(AtomicBool::new(false)),
        rerouted: Arc::new(AtomicBool::new(false)),
        asleep: Arc::new(AtomicU64::new(0)),
        rate: rate(),
    };
    sender.notice(CaptureNotice::Overrun);
    // The recorder gets to it later.
    clock.advance(secs(5));
    let rx = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC]),
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        thresholds: BTreeMap::new(),
    };
    assert!(matches!(
        rx.next(Duration::from_millis(1)),
        Received::Event(MIC, CaptureEvent::Notice { notice: CaptureNotice::Overrun, at: t }) if t == at(7)
    ));
}

/// A [`FakeFs`] whose fsyncs, once it's closed, wait until it's opened
/// again, reporting each one that starts waiting.
#[derive(Debug, Clone)]
struct Stalling {
    fs: FakeFs,
    gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    stalled: mpsc::Sender<()>,
}

impl Stalling {
    fn set_closed(&self, closed: bool) {
        *self.gate.0.lock().unwrap() = closed;
        self.gate.1.notify_all();
    }
}

#[derive(Debug)]
struct StallingFile {
    file: FakeFile,
    fs: Stalling,
}

impl Stalling {
    /// Waits while the gate is closed, reporting that it does.
    fn wait_open(&self) {
        let (closed, opened) = &*self.gate;
        let mut closed = closed.lock().unwrap();
        if *closed {
            let _ = self.stalled.send(());
        }
        while *closed {
            closed = opened.wait(closed).unwrap();
        }
    }
}

/// A [`StallingFile`]'s fsyncs from a sync thread, stalled as its own.
#[derive(Debug)]
struct StallingSyncer {
    syncer: FakeSyncer,
    fs: Stalling,
}

impl FsFile for StallingFile {
    type Syncer = StallingSyncer;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }

    fn sync(&mut self) -> io::Result<Synced> {
        self.fs.wait_open();
        self.file.sync()
    }

    fn syncer(&self) -> io::Result<StallingSyncer> {
        Ok(StallingSyncer {
            syncer: self.file.syncer()?,
            fs: self.fs.clone(),
        })
    }
}

impl FileSyncer for StallingSyncer {
    fn sync(&self) -> io::Result<Synced> {
        self.fs.wait_open();
        self.syncer.sync()
    }
}

impl Fs for Stalling {
    type File = StallingFile;
    type Lock = FakeLock;

    fn create(&self, path: &Path) -> io::Result<StallingFile> {
        Ok(StallingFile {
            file: self.fs.create(path)?,
            fs: self.clone(),
        })
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.fs.create_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.fs.rename(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.fs.sync_dir(dir)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.fs.remove(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.fs.read(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        self.fs.list(dir)
    }

    fn lock_dir(&self, dir: &Path) -> io::Result<FakeLock> {
        self.fs.lock_dir(dir)
    }
}

/// A backend that hands its stream's sender to the test, which then plays
/// the audio server's callback.
#[derive(Debug)]
struct Handed(mpsc::Sender<CaptureSender>);

impl CaptureBackend for Handed {
    type Stream = ();

    fn start(&self, _: &Source, _: SampleRate, events: CaptureSender) -> Result<(), CaptureError> {
        self.0.send(events).unwrap();
        Ok(())
    }
}

fn positions_at(delivered: u64, captured: u64, durable: u64) -> Positions {
    Positions {
        delivered: SampleIndex::new(delivered),
        captured: SampleIndex::new(captured),
        durable: SampleIndex::new(durable),
    }
}

#[test]
fn audio_queued_behind_a_stalled_fsync_counts_as_delivered_and_at_risk() {
    let (stalled, stalls) = mpsc::channel();
    let fs = Stalling {
        fs: FakeFs::with_dirs([dir()]),
        gate: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        stalled,
    };
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(10_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, Arc::clone(&clock)).unwrap();
    writer
        .start_track(
            MIC,
            &writer.test_epoch(MIC, EpochId::new(0), SampleIndex::ZERO),
        )
        .unwrap();
    let (handed, sender) = mpsc::channel();
    let (capture, events) =
        start(&Handed(handed), MIC, &Source::Microphone, rate(), &clock).unwrap();
    let stream = sender.recv().unwrap();
    let progress = events.progress(MIC).unwrap();
    assert!(events.progress(TrackId::new(9)).is_none());
    assert_eq!(progress.now(), positions_at(0, 0, 0));

    let (appended, appends) = mpsc::channel();
    let recorder = thread::spawn(move || {
        let mut timeline = epoch_zero();
        let result = record_track(&mut writer, &mut timeline, &events, &mut |e| {
            if let RecorderEvent::Audio(chunk) = e {
                appended.send(chunk.samples().len()).unwrap();
            }
        });
        (writer, result)
    });
    let next_append = || appends.recv_timeout(Duration::from_secs(10)).unwrap();

    // Ten samples, appended and not yet due an fsync.
    stream.audio(&samples(0, 10));
    assert_eq!(next_append(), 10);
    // Then 900 samples, more than the sync interval at 1 kHz: the fsync
    // their append makes stalls, so they're off the queue but not yet
    // captured, and still counted as queued.
    fs.set_closed(true);
    stream.audio(&samples(10, 900));
    stalls.recv_timeout(Duration::from_secs(10)).unwrap();
    let before = progress.now();
    assert_eq!(before, positions_at(910, 10, 0));
    assert_eq!(before.queued(), SampleCount::new(900));

    // While it stalls, the stream delivers three more buffers: the queue
    // grows, and so does what a crash now would lose.
    for i in 0..3 {
        stream.audio(&samples(910 + 100 * i, 100));
    }
    let stalled = progress.now();
    assert_eq!(stalled, positions_at(1_210, 10, 0));
    assert_eq!(stalled.queued(), SampleCount::new(1_200));
    assert_eq!(stalled.at_risk(), SampleCount::new(1_210));

    fs.set_closed(false);
    assert_eq!(next_append(), 900);
    for _ in 0..3 {
        assert_eq!(next_append(), 100);
    }
    drop(stream);
    drop(capture);
    let (writer, result) = recorder.join().unwrap();
    result.unwrap();
    let after = progress.now();
    assert_eq!(after.queued(), SampleCount::ZERO);
    assert_eq!(after.delivered, SampleIndex::new(1_210));
    // The stalled fsync went through, at the sync budget (850 samples).
    assert_eq!(after, positions_at(1_210, 1_210, 850));
    writer.finish().unwrap();
}

#[test]
fn buffers_are_reused_in_the_steady_state() {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, Arc::clone(&clock)).unwrap();
    writer
        .start_track(
            MIC,
            &writer.test_epoch(MIC, EpochId::new(0), SampleIndex::ZERO),
        )
        .unwrap();
    let (handed, sender) = mpsc::channel();
    let (capture, events) =
        start(&Handed(handed), MIC, &Source::Microphone, rate(), &clock).unwrap();
    let stream = sender.recv().unwrap();
    let queue = Arc::clone(&events.events);
    let (appended, appends) = mpsc::channel();
    let recorder = thread::spawn(move || {
        let mut timeline = epoch_zero();
        let mut chunks = Vec::new();
        let mut finished = Vec::new();
        let result = record_track(&mut writer, &mut timeline, &events, &mut |e| match e {
            RecorderEvent::Audio(chunk) => {
                chunks.push(chunk);
                appended.send(()).unwrap();
            }
            other => finished.push(other),
        });
        (writer, result, chunks, finished)
    });
    // One buffer at a time, each recorded before the next, as a stream
    // whose recorder keeps up. The recorder hands a buffer back just after
    // reporting its audio, so when the next one is sent the last may not
    // be back yet, but the one before it is: two buffers in all, reused
    // from then on.
    for i in 0..200 {
        stream.audio(&samples(i * 32, 32));
        appends.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    drop(stream);
    drop(capture);
    let (writer, result, chunks, finished) = recorder.join().unwrap();
    result.unwrap();
    let allocated = queue.allocated();
    assert!(
        (1..=2).contains(&allocated),
        "{allocated} buffers allocated"
    );
    // What was reported is the audio sent, though its buffers were reused.
    let reported: Vec<i16> = chunks.iter().flat_map(|c| c.samples().to_vec()).collect();
    assert_eq!(reported, samples(0, 200 * 32));
    let journals = all_journals(writer, &finished);
    assert_eq!(journaled(&fs, &journals), samples(0, 200 * 32));
}

#[test]
fn audio_a_broken_journal_couldnt_keep_is_never_counted_durable() {
    let fs = FakeFs::with_dirs([dir()]);
    // As in the test above: everything fails from the first frame write on,
    // so the journal breaks and no replacement can be made.
    fs.crash_after(10);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs, &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, Arc::clone(&clock)).unwrap();
    writer
        .start_track(
            MIC,
            &writer.test_epoch(MIC, EpochId::new(0), SampleIndex::ZERO),
        )
        .unwrap();
    let (handed, sender) = mpsc::channel();
    let (capture, events) =
        start(&Handed(handed), MIC, &Source::Microphone, rate(), &clock).unwrap();
    let stream = sender.recv().unwrap();
    let progress = events.progress(MIC).unwrap();
    let (appended, appends) = mpsc::channel();
    let recorder = thread::spawn(move || {
        let mut timeline = epoch_zero();
        let mut failed = 0;
        let result = record_track(&mut writer, &mut timeline, &events, &mut |e| match e {
            RecorderEvent::Audio(_) => appended.send(()).unwrap(),
            RecorderEvent::JournalFailed(_) => failed += 1,
            _ => {}
        });
        (writer, result, failed)
    });
    for i in 0..5 {
        stream.audio(&samples(i * 10, 10));
        appends.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    drop(stream);
    drop(capture);
    let (writer, result, failed) = recorder.join().unwrap();
    result.unwrap();
    assert!(failed > 0); // check-bound
    assert_eq!(writer.durable(MIC), None);
    // All 50 samples moved the track on, and none of them is on disk.
    let positions = progress.now();
    assert_eq!(positions, positions_at(50, 50, 0));
    assert_eq!(positions.at_risk(), SampleCount::new(50));
}

#[test]
fn a_stream_that_outlives_its_receiver_queues_nothing() {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let (handed, sender) = mpsc::channel();
    let (capture, events) =
        start(&Handed(handed), MIC, &Source::Microphone, rate(), &clock).unwrap();
    let stream = sender.recv().unwrap();
    let queue = Arc::clone(&events.events);
    stream.audio(&samples(0, 10));
    drop(events);
    stream.audio(&samples(10, 10));
    stream.notice(CaptureNotice::Overrun);
    drop(capture);
    assert!(matches!(queue.next(Duration::ZERO), Received::Idle));
}

#[test]
fn audio_the_writer_refuses_outright_stays_delivered() {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs, &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, Arc::clone(&clock)).unwrap();
    let last = SampleIndex::new(u64::MAX - 1);
    writer
        .start_track(MIC, &writer.test_epoch(MIC, EpochId::new(0), last))
        .unwrap();
    let mut timeline = TrackTimeline::new(MIC);
    timeline
        .open_epoch(SessionTime::ZERO, last, rate())
        .unwrap();
    let (tx, rx) = test_channel();
    let progress = Progress::new(SampleIndex::ZERO, SampleIndex::ZERO);
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: vec![(MIC, progress.clone())],
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        thresholds: BTreeMap::new(),
    };
    // Two samples where only one sample number is left: nothing is
    // recorded, so they stay counted as delivered and not captured.
    progress.sent(2);
    tx.send(MIC, CaptureEvent::Audio(vec![1, 2]));
    drop(tx);
    let result = record_tracks(
        &mut writer,
        std::slice::from_mut(&mut timeline),
        &events,
        &mut |_, _| {},
    );
    assert!(matches!(
        result,
        Err(RecordError::Session(SessionError::Overflow))
    ));
    assert_eq!(writer.next_sample(MIC), Some(last));
    let positions = progress.now();
    assert_eq!(positions.captured, last);
    assert_eq!(positions.queued(), SampleCount::new(1));
    assert_eq!(positions.delivered, SampleIndex::new(u64::MAX));
}

/// The epochs the timeline holds, as (id, start, first sample).
fn epoch_starts(timeline: &TrackTimeline) -> Vec<(u32, SessionTime, u64)> {
    timeline
        .epochs()
        .iter()
        .map(|e| (e.id().get(), e.start(), e.first_sample().get()))
        .collect()
}

/// The timeline's gaps, as (from, to).
fn gap_spans(timeline: &TrackTimeline) -> Vec<(SessionTime, SessionTime)> {
    timeline.gaps().map(|g| (g.from(), g.to())).collect()
}

/// `reported`'s epoch events, in order.
fn epoch_events(reported: &[RecorderEvent]) -> Vec<Epoch> {
    reported
        .iter()
        .filter_map(|e| match e {
            RecorderEvent::Epoch(epoch) => Some(*epoch),
            _ => None,
        })
        .collect()
}

/// How many times `reported` says the stream overran.
fn overruns_reported(reported: &[RecorderEvent]) -> usize {
    reported
        .iter()
        .filter(|e| matches!(e, RecorderEvent::Capture(CaptureNotice::Overrun)))
        .count()
}

/// With stamps exact, an overrun's epoch starts at the stamp of the audio
/// after the loss, not at the 2 s the overrun was reported: the tolerance
/// is zero.
#[test]
fn an_overruns_epoch_starts_when_the_audio_after_it_was_captured() {
    let fs = FakeFs::with_dirs([dir()]);
    let stamp = Duration::from_millis(1_800);
    let script = vec![
        Step::TimedAudio(samples(0, 500), Duration::ZERO),
        Step::Advance(secs(2)),
        Step::Notice(CaptureNotice::Overrun),
        // The server lost 1.3 s; the next buffer says when it was captured,
        // 0.2 s before the report.
        Step::TimedAudio(samples(500, 300), stamp),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();

    let epoch = run.timeline.current().copied().unwrap();
    assert_eq!(epoch.id(), EpochId::new(1));
    assert_eq!(epoch.start(), SessionTime::from_nanos(1_800_000_000));
    assert_eq!(epoch.first_sample(), SampleIndex::new(500));
    assert_eq!(
        gap_spans(&run.timeline),
        [(
            SessionTime::from_nanos(500_000_000),
            SessionTime::from_nanos(1_800_000_000)
        )]
    );
    assert!(matches!(
        epochs_reported(&run.reported)[..],
        [
            RecorderEvent::Capture(CaptureNotice::Overrun),
            RecorderEvent::Epoch(e)
        ] if *e == epoch
    ));
    let journals = all_journals(run.writer, &run.reported);
    assert_eq!(journal_epochs(&fs, &journals), [(0, 0, 500), (1, 500, 800)]);
    assert_eq!(journaled(&fs, &journals), samples(0, 800));
}

/// The server reports overruns of its whole graph: one whose next buffer
/// on this stream is on time lost nothing here.
#[test]
fn an_overrun_that_lost_nothing_on_this_stream_opens_no_epoch() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::TimedAudio(samples(0, 500), Duration::ZERO),
        // Reported a second late, as the server's graph overran elsewhere:
        // an epoch opened at the report would split on-time audio.
        Step::Advance(secs(1)),
        Step::Notice(CaptureNotice::Overrun),
        // Exactly where the first buffer's 500 samples end.
        Step::TimedAudio(samples(500, 300), Duration::from_millis(500)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert_eq!(epoch_starts(&run.timeline), [(0, SessionTime::ZERO, 0)]);
    assert!(gap_spans(&run.timeline).is_empty());
    assert_eq!(overruns_reported(&run.reported), 1);
    assert!(epoch_events(&run.reported).is_empty());
}

/// A stream whose overrun report was dropped (cpal can't always deliver
/// one) still shows the loss in its stamps.
#[test]
fn a_loss_with_no_overrun_reported_still_opens_an_epoch() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::TimedAudio(samples(0, 500), Duration::ZERO),
        // No notice: only the stamp says 1 s was lost.
        Step::TimedAudio(samples(500, 300), Duration::from_millis(1_500)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    let lost = SessionTime::from_nanos(1_500_000_000);
    assert_eq!(
        epoch_starts(&run.timeline),
        [(0, SessionTime::ZERO, 0), (1, lost, 500)]
    );
    assert_eq!(
        gap_spans(&run.timeline),
        [(SessionTime::from_nanos(500_000_000), lost)]
    );
    assert_eq!(overruns_reported(&run.reported), 0);
}

/// A script of 50-sample buffers, each stamped at its true capture time.
/// It tracks the true time itself, so a loss can skip some of it.
struct Buffers {
    /// What the stream sends, in order.
    script: Vec<Step>,
    /// The next buffer's first sample.
    sample: u64,
    /// When the next buffer was captured, in milliseconds.
    millis: u64,
    /// Every buffer's first sample and stamp, in order.
    stamps: Vec<(u64, Duration)>,
}

impl Buffers {
    /// The samples in each buffer: 50 ms at [`rate`].
    const LEN: u64 = 50;

    /// An empty script, at sample zero and time zero.
    fn new() -> Self {
        Self {
            script: Vec::new(),
            sample: 0,
            millis: 0,
            stamps: Vec::new(),
        }
    }

    /// Sends the next buffer, captured right after the last.
    fn on_time(&mut self) {
        let stamp = Duration::from_millis(self.millis);
        self.script
            .push(Step::TimedAudio(samples(self.sample, Self::LEN), stamp));
        self.stamps.push((self.sample, stamp));
        self.sample += Self::LEN;
        self.millis += Self::LEN;
    }

    /// Loses `millis` of audio before the next buffer, and reports an
    /// overrun for it.
    fn lose(&mut self, millis: u64) {
        self.script.push(Step::Notice(CaptureNotice::Overrun));
        self.millis += millis;
    }
}

/// Losses within one window of the first cost one epoch for the first and
/// one for all the rest, not one each: the next buffer after the window
/// opens it, so its epoch absorbs every loss since the first.
#[test]
fn a_burst_of_overruns_opens_one_epoch_then_one_for_the_rest() {
    let fs = FakeFs::with_dirs([dir()]);
    let mut buffers = Buffers::new();
    buffers.on_time();
    buffers.on_time();
    for _ in 0..5 {
        buffers.lose(30);
        buffers.on_time();
    }
    while buffers.millis <= 2_500 {
        buffers.on_time();
    }
    let run = run(&fs, buffers.script.clone(), Vec::new(), |_| {});
    run.result.unwrap();

    // The first lost buffer is the third sent, 100 ms in plus 30 lost.
    let first_loss = buffers.stamps[2];
    assert_eq!(first_loss, (100, Duration::from_millis(130)));
    // The first buffer a whole window after that one's epoch opened.
    let after = buffers
        .stamps
        .iter()
        .copied()
        .find(|(_, stamp)| *stamp >= first_loss.1 + BURST_WINDOW)
        .unwrap();
    let start = |(_, stamp): (u64, Duration)| SessionTime::ZERO.checked_add(stamp).unwrap();
    assert_eq!(
        epoch_starts(&run.timeline),
        [
            (0, SessionTime::ZERO, 0),
            (1, start(first_loss), first_loss.0),
            (2, start(after), after.0)
        ]
    );
    assert_eq!(epoch_events(&run.reported).len(), 2);
    assert_eq!(overruns_reported(&run.reported), 5);

    // The first loss is the gap before epoch 1; the other four, 120 ms,
    // are the whole gap between epochs 1 and 2.
    let ran = Duration::from_millis(after.0 - first_loss.0);
    let ended = start(first_loss).checked_add(ran).unwrap();
    assert_eq!(
        gap_spans(&run.timeline),
        [
            (
                SessionTime::from_nanos(100_000_000),
                SessionTime::from_nanos(130_000_000)
            ),
            (ended, start(after))
        ]
    );
    assert_eq!(
        start(after).checked_duration_since(ended),
        Some(Duration::from_millis(120))
    );

    // Journals also rotate at each window, so count where the epoch
    // changes: twice.
    let journals = all_journals(run.writer, &run.reported);
    let ids: Vec<u32> = journal_epochs(&fs, &journals)
        .iter()
        .map(|(epoch, _, _)| *epoch)
        .collect();
    let mut changes = ids.clone();
    changes.dedup();
    assert_eq!(changes, [0, 1, 2], "{ids:?}");
}

/// When the buffer holding the stream's `sample`th sample was captured, if
/// the device runs 400 ppm fast: its samples last a little less than the
/// nominal time each.
fn fast_stamp(sample: u64) -> Duration {
    let nanos = u128::from(sample) * 1_000_000_000_000_000_000 / (1_000 * 1_000_400_000);
    Duration::from_nanos(u64::try_from(nanos).unwrap())
}

/// A device 400 ppm fast is timed by the rate it ran at, so the recording
/// stays in step with the session clock, and a drift past the limit is
/// reported once.
#[test]
fn a_drifting_device_is_retimed_and_reported_past_the_limit() {
    let fs = FakeFs::with_dirs([dir()]);
    let total = 90_000;
    let script: Vec<Step> = (0..total / 100)
        .map(|n| Step::TimedAudio(samples(n * 100, 100), fast_stamp(n * 100)))
        .collect();
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();

    // Each correction follows straight on from the epoch before it.
    assert!(run.timeline.epochs().len() > 1); // check-bound
    assert!(gap_spans(&run.timeline).is_empty());
    assert!(!epoch_events(&run.reported).is_empty());
    let last = run.timeline.current().copied().unwrap();
    assert!((last.drift().ppb() - 400_000).abs() <= 2, "{last:?}"); // check-bound
    let drifted: Vec<Drift> = run
        .reported
        .iter()
        .filter_map(|e| match e {
            RecorderEvent::Capture(CaptureNotice::Drifted(d)) => Some(*d),
            _ => None,
        })
        .collect();
    let [reported] = drifted[..] else {
        panic!("{drifted:?}");
    };
    assert_eq!(reported, Drift::from_ppb(400_000).unwrap());

    // The last sample sits within 5 ms of when it was captured, where the
    // nominal rate puts it about 36 ms late.
    let end = total - 1;
    let true_nanos = i128::from(u64::try_from(fast_stamp(end).as_nanos()).unwrap());
    let timed = run.timeline.time_of(SampleIndex::new(end)).unwrap();
    let off = i128::from(timed.as_nanos()) - true_nanos;
    assert!(off.abs() <= 5_000_000, "{off} ns"); // check-bound
    let nominal = i128::from(end) * 1_000_000;
    assert!(nominal - true_nanos > 35_000_000, "{nominal} {true_nanos}"); // check-bound
}

/// A reopening whose first buffer is stamped starts its epoch at the
/// stamp, not at the time the buffer reached the recorder.
#[test]
fn a_stamped_reopening_is_timed_by_its_stamp() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::TimedAudio(samples(0, 500), Duration::ZERO),
        Step::Advance(secs(5)),
        Step::Notice(CaptureNotice::RouteChanged),
        // Captured at 3 s, though the clock reads 5 s by the time it
        // arrives.
        Step::TimedAudio(samples(500, 300), secs(3)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert_eq!(
        reopenings(&run.reported),
        [("route", 0, SessionTime::ZERO, 0), ("epoch", 1, at(3), 500)]
    );
    assert_eq!(
        gap_spans(&run.timeline),
        [(SessionTime::from_nanos(500_000_000), at(3))]
    );
}

/// A sender given a stamp the session clock can't place falls back to
/// timing the audio by the clock, as an unstamped stream's is.
#[test]
fn a_stamp_the_clock_cant_place_is_sent_unstamped() {
    let (tx, rx) = test_channel();
    let sender = CaptureSender {
        events: tx,
        track: MIC,
        progress: Progress::new(SampleIndex::ZERO, SampleIndex::ZERO),
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        stopping: Arc::new(AtomicBool::new(false)),
        failed: Arc::new(AtomicBool::new(false)),
        // Begun already, so each call queues only its audio.
        began: Arc::new(AtomicBool::new(true)),
        rerouted: Arc::new(AtomicBool::new(false)),
        asleep: Arc::new(AtomicU64::new(0)),
        rate: rate(),
    };
    let rx = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC]),
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        thresholds: BTreeMap::new(),
    };
    sender.audio_captured(&[1, 2], Duration::MAX);
    sender.audio_captured(&[3, 4], Duration::from_millis(7));
    let next = || rx.next(Duration::from_millis(1));
    assert!(matches!(
        next(),
        Received::Event(MIC, CaptureEvent::Audio(s)) if s == [1, 2]
    ));
    assert!(matches!(
        next(),
        Received::Event(MIC, CaptureEvent::TimedAudio { samples, at: t })
            if samples == [3, 4] && t == SessionTime::from_nanos(7_000_000)
    ));
}

/// A track started first opens its epoch when its stream was asked to
/// start; with stamps, its audio starts when it was captured, and the
/// start-up before is a gap, not a loss found at its second buffer.
#[test]
fn a_started_track_s_stamped_audio_starts_when_it_was_captured() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        // 50 ms of start-up: the first buffer was captured at 50 ms.
        Step::TimedAudio(samples(0, 100), Duration::from_millis(50)),
        Step::TimedAudio(samples(100, 100), Duration::from_millis(150)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert_eq!(
        epoch_starts(&run.timeline),
        [
            (0, SessionTime::ZERO, 0),
            (1, SessionTime::from_nanos(50_000_000), 0)
        ]
    );
    assert_eq!(
        run.timeline.time_of(SampleIndex::new(100)),
        Some(SessionTime::from_nanos(150_000_000))
    );
    let journals = all_journals(run.writer, &run.reported);
    assert_eq!(journal_epochs(&fs, &journals), [(1, 0, 200)]);
}

/// The most a drifting device's audio may be mapped from when it was
/// captured, in nanoseconds: GAI-315's acceptance criterion.
const DRIFT_ERROR_LIMIT: u64 = 20_000_000; // check-bound

/// When sample `sample` of a device `ppb` parts per billion off was
/// captured, at 1 kHz.
fn stamp_at(sample: u64, ppb: i128) -> Duration {
    let nanos = i128::from(sample) * 1_000_000_000_000_000_000 / (1_000 * (1_000_000_000 + ppb));
    Duration::from_nanos(u64::try_from(nanos).unwrap())
}

/// Three hours of a device 100 ppm fast, and of one 100 ppm slow, recorded
/// through the stamped stream: every buffer maps to within 20 ms of when it
/// was captured, where the nominal rate would end over a second out.
#[test]
fn three_hours_at_100_ppm_record_within_20_ms() {
    let buffer = 5_000;
    let total = 3 * 3_600 * 1_000;
    for ppb in [100_000, -100_000] {
        let fs = FakeFs::with_dirs([dir()]);
        let script: Vec<Step> = (0..total / buffer)
            .map(|n| Step::TimedAudio(samples(n * buffer, buffer), stamp_at(n * buffer, ppb)))
            .collect();
        let run = run(&fs, script, Vec::new(), |_| {});
        run.result.unwrap();
        // Corrections follow straight on: nothing was lost.
        assert!(gap_spans(&run.timeline).is_empty(), "{ppb}");
        let worst = (0..total / buffer)
            .map(|n| {
                let sample = n * buffer;
                let timed = run.timeline.time_of(SampleIndex::new(sample)).unwrap();
                let captured = u64::try_from(stamp_at(sample, ppb).as_nanos()).unwrap();
                timed.as_nanos().abs_diff(captured)
            })
            .max()
            .unwrap();
        assert!(worst < DRIFT_ERROR_LIMIT, "{ppb}: {worst} ns");
        let end = total - 1;
        let nominal = end * 1_000_000;
        let captured = u64::try_from(stamp_at(end, ppb).as_nanos()).unwrap();
        assert!(nominal.abs_diff(captured) > 1_000_000_000, "{ppb}"); // check-bound
    }
}

/// A loss exactly a window after the last loss epoch opens its own: the
/// window is over.
#[test]
fn a_loss_a_whole_window_after_the_last_opens_its_own_epoch() {
    let fs = FakeFs::with_dirs([dir()]);
    let mut buffers = Buffers::new();
    buffers.on_time();
    buffers.on_time();
    // The first loss: an epoch at 130 ms.
    buffers.lose(30);
    buffers.on_time();
    while buffers.millis < 2_080 {
        buffers.on_time();
    }
    // The second, captured at 2,130 ms: exactly the window after.
    buffers.lose(50);
    buffers.on_time();
    buffers.on_time();
    let run = run(&fs, buffers.script.clone(), Vec::new(), |_| {});
    run.result.unwrap();
    let starts: Vec<(u32, SessionTime)> = epoch_starts(&run.timeline)
        .into_iter()
        .map(|(id, start, _)| (id, start))
        .collect();
    assert_eq!(
        starts,
        [
            (0, SessionTime::ZERO),
            (1, SessionTime::from_nanos(130_000_000)),
            (2, SessionTime::from_nanos(2_130_000_000)),
        ]
    );
}

/// An epoch opened after a loss maps its audio at the drift measured, not
/// at a correction's slewed drift, nor none.
#[test]
fn a_loss_epoch_keeps_the_measured_drift() {
    let fs = FakeFs::with_dirs([dir()]);
    // 400 ppm fast; 40 s in, while a correction is slewing, a second is
    // lost.
    let loss = 40_000;
    let script: Vec<Step> = (0..600)
        .map(|n| {
            let first = n * 100;
            let shift = if first >= loss {
                secs(1)
            } else {
                Duration::ZERO
            };
            Step::TimedAudio(samples(first, 100), fast_stamp(first) + shift)
        })
        .collect();
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    let opened = run
        .timeline
        .epochs()
        .iter()
        .find(|e| e.first_sample() == SampleIndex::new(loss))
        .copied()
        .unwrap();
    let before = run
        .timeline
        .epoch_of(SampleIndex::new(loss - 1))
        .copied()
        .unwrap();
    // The epoch before was slewing, off the measured drift.
    assert!((before.drift().ppb() - 400_000).abs() > 1_000, "{before:?}"); // check-bound
    assert!((opened.drift().ppb() - 400_000).abs() <= 2, "{opened:?}"); // check-bound
    assert_eq!(gap_spans(&run.timeline).len(), 1);
}
