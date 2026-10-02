//! The recorder loop, driven by a synthetic backend that sends audio from
//! its own thread, as an audio server's callback does.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use nota_core::{Clock, EpochId, FakeClock, SampleIndex, SampleRate, SessionId, SessionTime};

use std::error::Error as _;

use super::*;
use crate::fs::fake::{FakeFile, FakeFs};
use crate::fs::{Fs, FsFile, Synced};
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
        let feeder_events = events.clone();
        let feeder = thread::spawn(move || {
            for step in script {
                match step {
                    Step::Audio(s) => feeder_events.audio(&s),
                    Step::Notice(n) => feeder_events.notice(n),
                    Step::Fail(e) => feeder_events.failed(e),
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
    result: Result<(), RecordError>,
    reported: Vec<RecorderEvent>,
}

/// Records `script` and `last` from a [`Synthetic`] backend on a recorder
/// thread, with windows of 1,000 samples at 1 kHz. `while_running` runs once
/// the script is sent, before the capture stops.
fn run<S: Fs + Clone + 'static>(
    fs: &S,
    script: Vec<Step>,
    last: Vec<i16>,
    while_running: impl FnOnce(&FakeClock),
) -> Run<S> {
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let dyn_clock: Arc<dyn Clock> = Arc::clone(&clock) as Arc<dyn Clock>;
    let session = SessionDir::new(SESSION, fs.clone(), &dir());
    let length = SegmentLength::new(1_000).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, dyn_clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    let (sent, script_sent) = mpsc::channel();
    let backend = Synthetic { script, last, sent };
    let (capture, events) = start(&backend, &Source::Microphone, rate()).unwrap();
    let (finished, done) = mpsc::channel();
    thread::spawn(move || {
        let mut reported = Vec::new();
        let result = record_track(&mut writer, MIC, &events, &mut |e| reported.push(e));
        let _ = finished.send(Run {
            writer,
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

fn finished_reported(reported: &[RecorderEvent]) -> Vec<FinishedJournal> {
    reported
        .iter()
        .filter_map(|e| match e {
            RecorderEvent::Finished(j) => Some(j.clone()),
            _ => None,
        })
        .flatten()
        .collect()
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

impl FsFile for WatchedFile {
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
}

impl Fs for Watched {
    type File = WatchedFile;

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
    let journals = run.writer.finish().unwrap();
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
    // Lets the listing at open, the first journal's creation and a few
    // writes through, then fails everything.
    fs.crash_after(4);
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
fn recording_an_unstarted_track_is_an_error() {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs.clone(), &dir());
    let mut writer =
        SessionWriter::open(&session, rate(), SegmentLength::new(1_000).unwrap(), clock).unwrap();
    let (tx, rx) = mpsc::channel();
    tx.send(CaptureEvent::Audio(samples(0, 10))).unwrap();
    let result = record_track(&mut writer, MIC, &CaptureReceiver(rx), &mut |_| {});
    assert!(matches!(
        result,
        Err(RecordError::Session(SessionError::UnknownTrack(MIC)))
    ));
}

#[test]
fn a_channel_with_no_senders_reads_as_stopped() {
    let (tx, rx) = mpsc::channel();
    let rx = CaptureReceiver(rx);
    assert!(rx.next(Duration::from_millis(1)).is_none());
    let sender = CaptureSender {
        events: tx.clone(),
        stopping: Arc::new(AtomicBool::new(false)),
    };
    sender.audio(&[]);
    assert!(rx.next(Duration::from_millis(1)).is_none());
    drop(tx);
    assert!(rx.next(Duration::from_millis(1)).is_none());
    drop(sender);
    assert!(matches!(
        rx.next(Duration::from_millis(1)),
        Some(CaptureEvent::Stopped)
    ));
}

#[test]
fn a_stream_that_cant_open_is_an_error() {
    let err = start(&Unavailable, &Source::Device("nowhere".into()), rate()).unwrap_err();
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
