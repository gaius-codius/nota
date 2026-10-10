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
    Notice(CaptureNotice),
    Fail(CaptureError),
    /// Moves the session clock on.
    Advance(Duration),
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
                    Step::Notice(n) => feeder_events.notice(n),
                    Step::Fail(e) => feeder_events.failed(e),
                    Step::Advance(by) => clock.advance(by),
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
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
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
    };
    assert!(matches!(rx.next(Duration::from_millis(1)), Received::Idle));
    let sender = CaptureSender {
        events: tx.clone(),
        track: MIC,
        progress: Progress::new(SampleIndex::ZERO, SampleIndex::ZERO),
        clock: Arc::new(FakeClock::new(SessionTime::ZERO)),
        stopping: Arc::new(AtomicBool::new(false)),
        began: Arc::new(AtomicBool::new(false)),
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
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    let (tx, rx) = test_channel();
    tx.send(MIC, CaptureEvent::Audio(samples(0, 10)));
    let other = SampleRate::new(rate().hz() * 2).unwrap();
    let events = CaptureReceiver {
        events: rx,
        rate: other,
        tracks: test_tracks(&[MIC]),
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
        run.writer.epoch(MIC),
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
        run.writer.epoch(MIC),
        Some((EpochId::new(2), SampleIndex::new(100)))
    );
    let journals = all_journals(run.writer, &run.reported);
    assert_eq!(journal_epochs(&fs, &journals), [(0, 0, 100), (2, 100, 200)]);
}

#[test]
fn other_notices_leave_the_epoch_alone() {
    let fs = FakeFs::with_dirs([dir()]);
    let script = vec![
        Step::Audio(samples(0, 100)),
        Step::Advance(secs(1)),
        Step::Notice(CaptureNotice::RouteChanged),
        Step::Notice(CaptureNotice::Warning("x".into())),
        Step::Audio(samples(100, 100)),
    ];
    let run = run(&fs, script, Vec::new(), |_| {});
    run.result.unwrap();
    assert_eq!(run.timeline.epochs().len(), 1);
    assert_eq!(
        run.writer.epoch(MIC),
        Some((EpochId::new(0), SampleIndex::ZERO))
    );
    assert!(epochs_reported(&run.reported).is_empty());
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
        run.writer.epoch(MIC),
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
        run.writer.epoch(MIC),
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
        .start_track(MIC, epoch, SampleIndex::new(at))
        .unwrap();
    let (tx, rx) = test_channel();
    tx.send(MIC, CaptureEvent::Audio(samples(0, 10)));
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC]),
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
        began: Arc::new(AtomicBool::new(false)),
        rate: rate(),
    };
    sender.notice(CaptureNotice::Overrun);
    // The recorder gets to it later.
    clock.advance(secs(5));
    let rx = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC]),
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
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
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
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
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
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
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
    assert!(failed > 0);
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
    writer.start_track(MIC, EpochId::new(0), last).unwrap();
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
