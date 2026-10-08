//! Several tracks captured into one receiver and recorded on one thread.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use nota_core::{
    Clock, EpochId, FakeClock, SampleCount, SampleIndex, SampleRate, SessionId, SessionTime,
    TrackId, TrackTimeline,
};

use super::*;
use crate::fs::Fs;
use crate::fs::fake::FakeFs;
use crate::journal::read_journal;
use crate::segment::SegmentLength;
use crate::session::{FinishedJournal, SessionDir, Syncing};

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);
const SESSION: SessionId = SessionId::new(1);

fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

fn dir() -> PathBuf {
    PathBuf::from("/session")
}

/// Distinct per track and position.
fn samples(track: TrackId, from: u64, len: u64) -> Vec<i16> {
    (from..from + len)
        .map(|i| i16::try_from((i + u64::from(track.get()) * 10_000) % 30_000).unwrap())
        .collect()
}

#[derive(Debug, Clone)]
enum Step {
    Audio(Vec<i16>),
    Overrun,
    Fail(CaptureError),
}

/// Each source gets its own script, sent from a thread of its own; `sent`
/// hears once per script.
#[derive(Debug)]
struct PerSource {
    scripts: BTreeMap<String, Vec<Step>>,
    sent: mpsc::Sender<()>,
}

#[derive(Debug)]
struct Feeder(Option<thread::JoinHandle<()>>);

impl Drop for Feeder {
    fn drop(&mut self) {
        if let Some(feeder) = self.0.take() {
            feeder.join().unwrap();
        }
    }
}

fn name(source: &Source) -> String {
    match source {
        Source::Device(name) => name.clone(),
        other => other.to_string(),
    }
}

impl CaptureBackend for PerSource {
    type Stream = Feeder;

    fn start(
        &self,
        source: &Source,
        _: SampleRate,
        events: CaptureSender,
    ) -> Result<Feeder, CaptureError> {
        let Some(script) = self.scripts.get(&name(source)).cloned() else {
            return Err(CaptureError::DeviceNotAvailable(source.clone()));
        };
        let sent = self.sent.clone();
        Ok(Feeder(Some(thread::spawn(move || {
            for step in script {
                match step {
                    Step::Audio(s) => events.audio(&s),
                    Step::Overrun => events.notice(CaptureNotice::Overrun),
                    Step::Fail(e) => events.failed(e),
                }
            }
            sent.send(()).unwrap();
        }))))
    }
}

struct Recorded {
    fs: FakeFs,
    writer: SessionWriter<FakeFs>,
    timelines: Vec<TrackTimeline>,
    result: Result<(), RecordError>,
    reported: Reported,
}

/// Records `mic` and `system` (each a script for its own stream) with the
/// clock standing at 10 s and both tracks' epoch 0 opened at zero, until
/// both scripts are sent and the captures dropped.
fn record_both(mic: Vec<Step>, system: Vec<Step>) -> Recorded {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::from_nanos(10_000_000_000)));
    let session = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, Arc::clone(&clock)).unwrap();
    let mut timelines = Vec::new();
    for track in [MIC, SYSTEM] {
        writer
            .start_track(track, EpochId::new(0), SampleIndex::ZERO)
            .unwrap();
        let mut timeline = TrackTimeline::new(track);
        timeline
            .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
            .unwrap();
        timelines.push(timeline);
    }
    let (sent, all_sent) = mpsc::channel();
    let backend = PerSource {
        scripts: BTreeMap::from([("mic".to_owned(), mic), ("system".to_owned(), system)]),
        sent,
    };
    let (captures, events) = start_tracks(
        &backend,
        &[
            (MIC, Source::Device("mic".into())),
            (SYSTEM, Source::Device("system".into())),
        ],
        rate(),
        &clock,
    );
    let captures: Vec<_> = captures.into_iter().map(Result::unwrap).collect();
    assert_eq!(events.tracks(), [MIC, SYSTEM]);
    assert_eq!(
        captures.iter().map(Capture::track).collect::<Vec<_>>(),
        [MIC, SYSTEM]
    );
    // Each stream's start is read from the clock as it's opened.
    for capture in &captures {
        assert_eq!(
            capture.started_at(),
            SessionTime::from_nanos(10_000_000_000)
        );
    }
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let mut reported = Vec::new();
        let result = record_tracks(&mut writer, &mut timelines, &events, &mut |t, e| {
            reported.push((t, e));
        });
        done.send((writer, timelines, result, reported)).unwrap();
    });
    all_sent.recv().unwrap();
    all_sent.recv().unwrap();
    drop(captures);
    let (writer, timelines, result, reported) =
        finished.recv_timeout(Duration::from_secs(10)).unwrap();
    Recorded {
        fs,
        writer,
        timelines,
        result,
        reported,
    }
}

/// Each track's audio across `journals`, in order, checking each journal
/// follows on from the last of its track.
fn by_track(fs: &FakeFs, journals: &[FinishedJournal]) -> BTreeMap<TrackId, Vec<i16>> {
    let mut out: BTreeMap<TrackId, Vec<i16>> = BTreeMap::new();
    for journal in journals {
        let bytes = fs.read(&dir().join(journal.id().file_name())).unwrap();
        let read = read_journal(&bytes);
        let track = read.header().unwrap().track();
        let (range, audio) = read.audio().unwrap();
        let held = out.entry(track).or_default();
        assert_eq!(range.start().get(), held.len() as u64, "{journal:?}");
        held.extend(audio);
    }
    out
}

/// Everything a run reported, with its track.
type Reported = Vec<(Option<TrackId>, RecorderEvent)>;

fn all_journals(run: Recorded) -> (FakeFs, Vec<FinishedJournal>, Reported) {
    let mut journals: Vec<FinishedJournal> = Vec::new();
    for (track, event) in &run.reported {
        if let RecorderEvent::Finished(j) = event {
            assert_eq!(*track, None);
            journals.extend(j.iter().map(|j| FinishedJournal::new(j.session(), j.id())));
        }
    }
    journals.extend(run.writer.finish().unwrap());
    (run.fs, journals, run.reported)
}

#[test]
fn two_tracks_record_into_their_own_journals() {
    let mic = vec![
        Step::Audio(samples(MIC, 0, 600)),
        Step::Audio(samples(MIC, 600, 700)),
    ];
    let system = vec![
        Step::Audio(samples(SYSTEM, 0, 250)),
        Step::Audio(samples(SYSTEM, 250, 250)),
        Step::Audio(samples(SYSTEM, 500, 900)),
    ];
    let run = record_both(mic, system);
    run.result.as_ref().unwrap();
    assert_eq!(run.writer.next_sample(MIC), Some(SampleIndex::new(1_300)));
    assert_eq!(
        run.writer.next_sample(SYSTEM),
        Some(SampleIndex::new(1_400))
    );
    let (fs, journals, reported) = all_journals(run);
    let held = by_track(&fs, &journals);
    assert_eq!(held[&MIC], samples(MIC, 0, 1_300));
    assert_eq!(held[&SYSTEM], samples(SYSTEM, 0, 1_400));

    // Each chunk is reported once, as it was appended, with its track and
    // where it lands in that track.
    let mut audio: BTreeMap<TrackId, Vec<i16>> = BTreeMap::new();
    for (track, event) in &reported {
        if let RecorderEvent::Audio(chunk) = event {
            assert_eq!(*track, Some(chunk.track()));
            assert_eq!(chunk.rate(), rate());
            let got = audio.entry(chunk.track()).or_default();
            assert_eq!(chunk.range().start().get(), got.len() as u64);
            got.extend_from_slice(chunk.samples());
        }
    }
    assert_eq!(audio, held);
}

#[test]
fn an_overrun_moves_only_its_own_track_to_a_new_epoch() {
    let mic = vec![
        Step::Audio(samples(MIC, 0, 100)),
        Step::Overrun,
        Step::Audio(samples(MIC, 100, 100)),
    ];
    let system = vec![Step::Audio(samples(SYSTEM, 0, 300))];
    let run = record_both(mic, system);
    run.result.as_ref().unwrap();
    let [mic, system] = &run.timelines[..] else {
        panic!("two timelines")
    };
    assert_eq!(mic.epochs().len(), 2);
    assert_eq!(
        mic.epochs()[1].start(),
        SessionTime::from_nanos(10_000_000_000)
    );
    assert_eq!(mic.epochs()[1].first_sample(), SampleIndex::new(100));
    assert_eq!(system.epochs().len(), 1);
    assert_eq!(
        run.writer.epoch(MIC),
        Some((EpochId::new(1), SampleIndex::new(100)))
    );
    assert_eq!(
        run.writer.epoch(SYSTEM),
        Some((EpochId::new(0), SampleIndex::ZERO))
    );
    let epochs: Vec<_> = run
        .reported
        .iter()
        .filter_map(|(t, e)| match e {
            RecorderEvent::Epoch(epoch) => Some((*t, epoch.id())),
            _ => None,
        })
        .collect();
    assert_eq!(epochs, [(Some(MIC), EpochId::new(1))]);
    let notices = run
        .reported
        .iter()
        .filter(|(t, e)| {
            *t == Some(MIC) && matches!(e, RecorderEvent::Capture(CaptureNotice::Overrun))
        })
        .count();
    assert_eq!(notices, 1);
    // The chunk after the overrun is reported from where the epoch starts.
    let after: Vec<_> = run
        .reported
        .iter()
        .filter_map(|(_, e)| match e {
            RecorderEvent::Audio(c) if c.track() == MIC => Some(c.range().start().get()),
            _ => None,
        })
        .collect();
    assert_eq!(after, [0, 100]);
}

#[test]
fn a_failed_stream_is_reported_and_the_other_records_on() {
    let failure = CaptureError::DeviceNotAvailable(Source::Device("mic".into()));
    let mic = vec![
        Step::Audio(samples(MIC, 0, 50)),
        Step::Fail(failure.clone()),
    ];
    let system = vec![
        Step::Audio(samples(SYSTEM, 0, 400)),
        Step::Audio(samples(SYSTEM, 400, 400)),
    ];
    let run = record_both(mic, system);
    run.result.as_ref().unwrap();
    let failures: Vec<_> = run
        .reported
        .iter()
        .filter_map(|(t, e)| match e {
            RecorderEvent::CaptureFailed(e) => Some((*t, e.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(failures, [(Some(MIC), failure)]);
    let (fs, journals, _) = all_journals(run);
    let held = by_track(&fs, &journals);
    assert_eq!(held[&MIC], samples(MIC, 0, 50));
    assert_eq!(held[&SYSTEM], samples(SYSTEM, 0, 800));
}

#[test]
fn a_stream_without_a_timeline_records_nothing() {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs, &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    let mut mic = TrackTimeline::new(MIC);
    mic.open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
        .unwrap();
    let (tx, rx) = test_channel();
    tx.send(MIC, CaptureEvent::Audio(samples(MIC, 0, 10)));
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC, SYSTEM]),
    };
    let result = record_tracks(
        &mut writer,
        std::slice::from_mut(&mut mic),
        &events,
        &mut |_, _| {},
    );
    assert!(matches!(
        result,
        Err(RecordError::Session(SessionError::UnknownTrack(SYSTEM)))
    ));
    assert_eq!(writer.next_sample(MIC), Some(SampleIndex::ZERO));
}

#[test]
fn a_closed_channel_ends_recording() {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs, &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, clock).unwrap();
    writer
        .start_track(MIC, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    let mut mic = TrackTimeline::new(MIC);
    mic.open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
        .unwrap();
    let (tx, rx) = test_channel();
    tx.send(MIC, CaptureEvent::Audio(samples(MIC, 0, 10)));
    drop(tx);
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC]),
    };
    let mut audio = 0;
    record_tracks(
        &mut writer,
        std::slice::from_mut(&mut mic),
        &events,
        &mut |_, e| {
            if matches!(e, RecorderEvent::Audio(_)) {
                audio += 1;
            }
        },
    )
    .unwrap();
    assert_eq!(audio, 1);
    assert_eq!(writer.next_sample(MIC), Some(SampleIndex::new(10)));
}

#[test]
fn streams_that_cant_start_leave_the_others_running() {
    let (sent, _all_sent) = mpsc::channel();
    let backend = PerSource {
        scripts: BTreeMap::from([("system".to_owned(), Vec::new())]),
        sent,
    };
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let (started, events) = start_tracks(
        &backend,
        &[
            (MIC, Source::Device("mic".into())),
            (SYSTEM, Source::Device("system".into())),
            (SYSTEM, Source::Device("system".into())),
        ],
        rate(),
        &clock,
    );
    let outcomes: Vec<_> = started
        .iter()
        .map(|r| r.as_ref().map(Capture::track))
        .collect();
    assert_eq!(
        outcomes,
        [
            Err(&CaptureError::DeviceNotAvailable(Source::Device(
                "mic".into()
            ))),
            Ok(SYSTEM),
            Err(&CaptureError::Backend("track 1 was asked for twice".into())),
        ]
    );
    assert_eq!(events.tracks(), [SYSTEM]);
    assert_eq!(events.rate(), rate());
}

#[test]
fn a_track_that_fails_first_leaves_the_other_recording_until_it_stops() {
    // In this order on the channel: the mic fails, then the system audio
    // still arrives, then the system stream stops.
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, clock).unwrap();
    let mut timelines = Vec::new();
    for track in [MIC, SYSTEM] {
        writer
            .start_track(track, EpochId::new(0), SampleIndex::ZERO)
            .unwrap();
        let mut timeline = TrackTimeline::new(track);
        timeline
            .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
            .unwrap();
        timelines.push(timeline);
    }
    let (tx, rx) = test_channel();
    let failure = CaptureError::Backend("gone".into());
    tx.send(MIC, CaptureEvent::Failed(failure.clone()));
    tx.send(SYSTEM, CaptureEvent::Audio(samples(SYSTEM, 0, 30)));
    tx.send(MIC, CaptureEvent::Stopped);
    tx.send(SYSTEM, CaptureEvent::Audio(samples(SYSTEM, 30, 20)));
    tx.send(SYSTEM, CaptureEvent::Stopped);
    // Never seen: recording ended with the system stream.
    tx.send(SYSTEM, CaptureEvent::Audio(samples(SYSTEM, 50, 5)));
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC, SYSTEM]),
    };
    let mut failed = Vec::new();
    record_tracks(&mut writer, &mut timelines, &events, &mut |t, e| {
        if let RecorderEvent::CaptureFailed(e) = e {
            failed.push((t, e));
        }
    })
    .unwrap();
    assert_eq!(failed, [(Some(MIC), failure)]);
    assert_eq!(writer.next_sample(SYSTEM), Some(SampleIndex::new(50)));
    assert_eq!(writer.next_sample(MIC), Some(SampleIndex::ZERO));
    let journals = writer.finish().unwrap();
    assert_eq!(by_track(&fs, &journals)[&SYSTEM], samples(SYSTEM, 0, 50));
}

#[test]
fn events_from_a_stream_that_never_started_are_dropped() {
    // A stream can report something before its start fails (cpal reports a
    // refused real-time promotion while the stream is still being built).
    // Its track was never started, and the others record on.
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, clock).unwrap();
    writer
        .start_track(SYSTEM, EpochId::new(0), SampleIndex::ZERO)
        .unwrap();
    let mut timeline = TrackTimeline::new(SYSTEM);
    timeline
        .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
        .unwrap();
    let mut timelines = vec![timeline];
    let (tx, rx) = test_channel();
    tx.send(
        MIC,
        CaptureEvent::Notice {
            notice: CaptureNotice::Warning("no rtkit".into()),
            at: SessionTime::ZERO,
        },
    );
    tx.send(SYSTEM, CaptureEvent::Audio(samples(SYSTEM, 0, 30)));
    tx.send(MIC, CaptureEvent::Audio(samples(MIC, 0, 10)));
    tx.send(
        MIC,
        CaptureEvent::Failed(CaptureError::Backend("late".into())),
    );
    tx.send(SYSTEM, CaptureEvent::Audio(samples(SYSTEM, 30, 20)));
    tx.send(SYSTEM, CaptureEvent::Stopped);
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[SYSTEM]),
    };
    let mut reported = Vec::new();
    record_tracks(&mut writer, &mut timelines, &events, &mut |t, _| {
        reported.push(t);
    })
    .unwrap();
    assert!(!reported.contains(&Some(MIC)), "{reported:?}");
    assert_eq!(writer.next_sample(SYSTEM), Some(SampleIndex::new(50)));
    let journals = writer.finish().unwrap();
    assert_eq!(by_track(&fs, &journals)[&SYSTEM], samples(SYSTEM, 0, 50));
}

/// Waits up to 10 s for the recorder to report an event `wanted` picks.
fn wait_for(
    seen: &mpsc::Receiver<(Option<TrackId>, RecorderEvent)>,
    wanted: impl Fn(Option<TrackId>, &RecorderEvent) -> bool,
) {
    loop {
        let (track, event) = seen.recv_timeout(Duration::from_secs(10)).unwrap();
        if wanted(track, &event) {
            return;
        }
    }
}

/// Whether `event` is `track`'s audio starting at `first`.
fn audio_from(track: TrackId, first: u64) -> impl Fn(Option<TrackId>, &RecorderEvent) -> bool {
    move |_, event| {
        matches!(event, RecorderEvent::Audio(chunk)
            if chunk.track() == track && chunk.range().start().get() == first)
    }
}

#[test]
fn a_stalled_fsync_on_one_track_never_holds_up_the_other() {
    let fs = FakeFs::with_dirs([dir()]);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(SessionTime::ZERO));
    let session = SessionDir::new(SESSION, fs.clone(), &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, clock)
        .unwrap()
        .with_syncing(Syncing::Threads);
    let mut timelines = Vec::new();
    for track in [MIC, SYSTEM] {
        writer
            .start_track(track, EpochId::new(0), SampleIndex::ZERO)
            .unwrap();
        let mut timeline = TrackTimeline::new(track);
        timeline
            .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
            .unwrap();
        timelines.push(timeline);
    }
    let (tx, rx) = test_channel();
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC, SYSTEM]),
    };
    let mic = events.progress(MIC).unwrap();
    let system = events.progress(SYSTEM).unwrap();
    let (report, seen) = mpsc::channel();
    let recorder = thread::spawn(move || {
        let mut handed_out = Vec::new();
        let result = record_tracks(&mut writer, &mut timelines, &events, &mut |t, e| {
            if let RecorderEvent::Finished(journals) = &e {
                handed_out.extend(
                    journals
                        .iter()
                        .map(|j| FinishedJournal::new(j.session(), j.id())),
                );
            }
            let _ = report.send((t, e));
        });
        (writer, result, handed_out)
    });

    // The mic's first journal exists once its audio is reported; from now
    // on its fsyncs hang.
    tx.audio(MIC, &samples(MIC, 0, 100));
    wait_for(&seen, audio_from(MIC, 0));
    let mic_journal = dir().join(crate::journal::JournalId::FIRST.file_name());
    assert!(fs.paths().contains(&mic_journal));
    let stall = fs.stall_syncs(&mic_journal);
    // A full sync budget (850 samples) starts its fsync, which hangs, and
    // the mic's audio after that waits in memory.
    tx.audio(MIC, &samples(MIC, 100, 800));
    assert!(stall.wait_for_held(1, Duration::from_secs(10)));
    tx.audio(MIC, &samples(MIC, 900, 50));

    // The system audio crosses three budgets and two windows meanwhile, and
    // both its journals are handed out, their last fsyncs done.
    for from in (0..2_600).step_by(100) {
        tx.audio(SYSTEM, &samples(SYSTEM, from, 100));
    }
    let handed_out = std::cell::Cell::new(0);
    wait_for(&seen, |_, e| {
        if let RecorderEvent::Finished(journals) = e {
            handed_out.set(handed_out.get() + journals.len());
        }
        handed_out.get() >= 2
    });
    // Once the next event is handled, the progress has caught up.
    tx.audio(SYSTEM, &samples(SYSTEM, 2_600, 1));
    wait_for(&seen, audio_from(SYSTEM, 2_600));
    let (mic_now, system_now) = (mic.now(), system.now());
    assert_eq!(system_now.durable, SampleIndex::new(2_000));
    // The recorder took all the mic's audio, though none is durable.
    assert_eq!(mic_now.captured, SampleIndex::new(950));
    assert_eq!(mic_now.durable, SampleIndex::ZERO);
    assert_eq!(mic_now.at_risk().get(), 950);
    assert!(system_now.at_risk().get() <= 850, "{system_now:?}");

    stall.release();
    tx.send(MIC, CaptureEvent::Stopped);
    tx.send(SYSTEM, CaptureEvent::Stopped);
    let (writer, result, mut journals) = recorder.join().unwrap();
    result.unwrap();
    journals.extend(writer.finish().unwrap());
    let audio = by_track(&fs, &journals);
    assert_eq!(audio[&MIC], samples(MIC, 0, 950));
    assert_eq!(audio[&SYSTEM], samples(SYSTEM, 0, 2_601));
}

#[test]
fn the_other_tracks_due_fsync_runs_as_soon_as_a_stream_ends() {
    let fs = FakeFs::with_dirs([dir()]);
    let fake = Arc::new(FakeClock::new(SessionTime::ZERO));
    let clock: Arc<dyn Clock> = Arc::clone(&fake) as Arc<dyn Clock>;
    let session = SessionDir::new(SESSION, fs, &dir()).lock().unwrap();
    let length = SegmentLength::new(SampleCount::new(1_000)).unwrap();
    let mut writer = SessionWriter::open(&session, rate(), length, clock).unwrap();
    let mut timelines = Vec::new();
    for track in [MIC, SYSTEM] {
        writer
            .start_track(track, EpochId::new(0), SampleIndex::ZERO)
            .unwrap();
        let mut timeline = TrackTimeline::new(track);
        timeline
            .open_epoch(SessionTime::ZERO, SampleIndex::ZERO, rate())
            .unwrap();
        timelines.push(timeline);
    }
    let (tx, rx) = test_channel();
    tx.send(SYSTEM, CaptureEvent::Audio(samples(SYSTEM, 0, 30)));
    tx.send(
        MIC,
        CaptureEvent::Failed(CaptureError::Backend("gone".into())),
    );
    tx.send(SYSTEM, CaptureEvent::Stopped);
    let events = CaptureReceiver {
        events: rx,
        rate: rate(),
        tracks: test_tracks(&[MIC, SYSTEM]),
    };
    let system = events.progress(SYSTEM).unwrap();
    record_tracks(&mut writer, &mut timelines, &events, &mut |_, e| {
        // The system track's fsync falls due as the mic's stream ends.
        if matches!(e, RecorderEvent::CaptureFailed(_)) {
            fake.advance(crate::journal::SYNC_INTERVAL);
        }
    })
    .unwrap();
    // Synced then, not only once recording stopped.
    assert_eq!(system.now().durable, SampleIndex::new(30));
    assert_eq!(writer.durable(SYSTEM).unwrap().end(), SampleIndex::new(30));
    writer.finish().unwrap();
}
