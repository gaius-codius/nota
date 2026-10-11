//! Playing a moment: which samples are heard (within a segment, across a
//! segment boundary, across a gap between epochs, at the recording's
//! edges), when playing is refused, and that it stops when asked. The output
//! is a fake that keeps what it was given.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::config::Encoder;
use flacenc::error::Verify;
use nota_core::{
    Drift, EpochAnchor, EpochError, EpochId, FakeClock, SampleIndex, SampleRange, SampleRate,
    SessionId, SessionTime, TrackId, TrackTimeline,
};
use nota_store::{NewSession, Sha256Digest};
use sha2::{Digest, Sha256};

use super::plan::{Piece, plan};
use super::*;
use crate::fs::fake::FakeFs;
use crate::fs::{Fs, FsFile};
use crate::segment::{SegmentLength, segment_file_name};
use crate::session::SessionWriter;
use crate::test_dir::TestDir;

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);
const SESSION: SessionId = SessionId::new(1);

/// A low rate keeps the tests small: a second is 1,000 samples, so the
/// window is 5,000 samples.
fn rate() -> SampleRate {
    SampleRate::new(1_000).unwrap()
}

/// `seconds` into the session.
fn secs(seconds: u64) -> SessionTime {
    SessionTime::from_nanos(seconds * 1_000_000_000)
}

/// The sample at `index` of `track`: never zero (so silence can't pass for
/// it), and different on each track.
fn value(track: TrackId, index: u64) -> i16 {
    i16::try_from(index % 9_000 + 1 + u64::from(track.get()) * 10_000).unwrap()
}

/// The samples `from..to` of `track`, as `value` makes them.
fn values(track: TrackId, from: u64, to: u64) -> Vec<i16> {
    (from..to).map(|i| value(track, i)).collect()
}

/// An anchor for epoch `id`: sample `first_sample` plays at `start` seconds.
fn anchor(id: u32, start: u64, first_sample: u64) -> EpochAnchor {
    EpochAnchor {
        id: EpochId::new(id),
        start: secs(start),
        first_sample: SampleIndex::new(first_sample),
        rate: rate(),
        drift: Drift::ZERO,
    }
}

/// A session on a fake filesystem, with its rows in a real library.
struct Scene {
    fs: FakeFs,
    store: Store,
    session: SessionDir<FakeFs>,
    _dir: TestDir,
}

impl Scene {
    fn new(name: &str) -> Self {
        let dir = TestDir::new(name);
        let mut store = Store::open(&dir.0.join("library.db")).unwrap();
        store.create_session(&NewSession::bare(SESSION)).unwrap();
        let fs = FakeFs::new();
        fs.create_dir(Path::new("/s")).unwrap();
        let session = SessionDir::new(SESSION, fs.clone(), Path::new("/s"));
        Self {
            fs,
            store,
            session,
            _dir: dir,
        }
    }

    /// Stores `anchor` as an epoch of `track`.
    fn epoch(&mut self, track: TrackId, anchor: &EpochAnchor) {
        self.store.insert_epoch(SESSION, track, anchor).unwrap();
    }

    /// Publishes samples `from..to` of `track`, recorded in `epoch`, as a
    /// segment: its file and its row.
    fn segment(&mut self, track: TrackId, epoch: u32, from: u64, to: u64) {
        let bytes = encode(&values(track, from, to));
        let range = SampleRange::new(SampleIndex::new(from), SampleIndex::new(to)).unwrap();
        let path = Path::new("/s").join(segment_file_name(track, range));
        self.fs.create(&path).unwrap().write_all(&bytes).unwrap();
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let row =
            SegmentRow::new(track, EpochId::new(epoch), range, Sha256Digest::new(digest)).unwrap();
        self.store.insert_segment(SESSION, &row).unwrap();
    }

    /// Plays the moment at `at` seconds of `track` on `output`.
    fn play(
        &self,
        output: &FakeOutput,
        track: TrackId,
        at: u64,
    ) -> Result<Playback<FakeFs, FakeStream>, PlaybackError> {
        play_moment(&self.session, &self.store, track, secs(at), output)
    }

    /// The samples a play at `at` seconds of `track` hands the output.
    fn heard(&self, track: TrackId, at: u64) -> Vec<i16> {
        let output = FakeOutput::default();
        self.play(&output, track, at).unwrap().stop();
        output.played()
    }
}

/// Mono 16-bit `samples` at the test rate as a FLAC file, written the way
/// `segment::flac` writes one (fixed blocks).
fn encode(samples: &[i16]) -> Vec<u8> {
    let config = Encoder::default().into_verified().unwrap();
    let source = flacenc::source::MemSource::from_samples(
        &samples.iter().map(|&s| i32::from(s)).collect::<Vec<_>>(),
        1,
        16,
        usize::try_from(rate().hz()).unwrap(),
    );
    let stream = flacenc::encode_with_fixed_block_size(&config, source, 4_096).unwrap();
    let mut sink = ByteSink::new();
    stream.write(&mut sink).unwrap();
    sink.as_slice().to_vec()
}

/// An output that keeps what it was given, and shows when its stream is
/// dropped.
#[derive(Default)]
struct FakeOutput {
    /// What each start was given, with the rate.
    started: Mutex<Vec<(Vec<i16>, SampleRate)>>,
    /// Set when a stream it made is dropped.
    stopped: Arc<AtomicBool>,
    /// What its streams answer to `finished`; set by a test.
    finished: Arc<AtomicBool>,
    /// The error to refuse with, if it should refuse.
    refuse: Option<OutputError>,
}

impl FakeOutput {
    /// The samples of the one start.
    fn played(&self) -> Vec<i16> {
        let started = self.started.lock().unwrap();
        let [(samples, _)] = &started[..] else {
            panic!("expected one start, got {}", started.len());
        };
        samples.clone()
    }
}

/// A stream that says when it was dropped.
#[derive(Debug)]
struct FakeStream {
    stopped: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

impl OutputStream for FakeStream {
    fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl Output for FakeOutput {
    type Stream = FakeStream;

    fn start(&self, samples: &[i16], rate: SampleRate) -> Result<FakeStream, OutputError> {
        if let Some(error) = &self.refuse {
            return Err(error.clone());
        }
        self.started.lock().unwrap().push((samples.to_vec(), rate));
        Ok(FakeStream {
            stopped: Arc::clone(&self.stopped),
            finished: Arc::clone(&self.finished),
        })
    }
}

/// Inside one segment, the samples played are the five seconds around the
/// time: 2.5 s either side.
#[test]
fn a_moment_within_one_segment_plays_the_samples_around_its_time() {
    let mut scene = Scene::new("within-one");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 12_000);

    assert_eq!(scene.heard(MIC, 6), values(MIC, 3_500, 8_500));
}

/// A moment that crosses from one segment into the next plays on without a
/// seam: no sample twice, none missing.
#[test]
fn a_moment_across_a_segment_boundary_has_no_seam() {
    let mut scene = Scene::new("across-boundary");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 6_000);
    scene.segment(MIC, 0, 6_000, 12_000);

    // The window, 3,500..8,500, has the boundary at 6,000 in the middle.
    assert_eq!(scene.heard(MIC, 6), values(MIC, 3_500, 8_500));
}

/// A moment reaching over three segments reads the one between whole.
#[test]
fn a_moment_over_three_segments_reads_the_middle_one_whole() {
    let mut scene = Scene::new("three-segments");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 3_000);
    scene.segment(MIC, 0, 3_000, 4_000);
    scene.segment(MIC, 0, 4_000, 10_000);

    assert_eq!(scene.heard(MIC, 5), values(MIC, 2_500, 7_500));
}

/// A gap between epochs plays as silence of its length, between the audio
/// either side of it: what is heard matches the timeline.
#[test]
fn a_gap_between_epochs_plays_as_silence_of_its_length() {
    let mut scene = Scene::new("epoch-gap");
    // Epoch 0 plays samples 0..4,000 from 0 s to 4 s; epoch 1 reopens at
    // 6 s, so 2 s of the session were not recorded.
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.epoch(MIC, &anchor(1, 6, 4_000));
    scene.segment(MIC, 0, 0, 4_000);
    scene.segment(MIC, 1, 4_000, 9_000);

    // The window, 2.5 s..7.5 s: 1.5 s of epoch 0, the 2 s gap, 1.5 s of
    // epoch 1 (samples 4,000..5,500).
    let mut expected = values(MIC, 2_500, 4_000);
    expected.extend([0; 2_000]);
    expected.extend(values(MIC, 4_000, 5_500));
    assert_eq!(scene.heard(MIC, 5), expected);
}

/// A moment whose time falls inside the gap still plays the audio either
/// side of it.
#[test]
fn a_moment_inside_a_gap_plays_what_surrounds_the_gap() {
    let mut scene = Scene::new("inside-gap");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.epoch(MIC, &anchor(1, 5, 4_000));
    scene.segment(MIC, 0, 0, 4_000);
    scene.segment(MIC, 1, 4_000, 8_000);

    // At 4.5 s: 2 s..7 s is 2 s of epoch 0, the 1 s gap, 2 s of epoch 1.
    let mut expected = values(MIC, 2_000, 4_000);
    expected.extend([0; 1_000]);
    expected.extend(values(MIC, 4_000, 6_000));
    let output = FakeOutput::default();
    let at = SessionTime::from_nanos(4_500_000_000);
    play_moment(&scene.session, &scene.store, MIC, at, &output)
        .unwrap()
        .stop();
    assert_eq!(output.played(), expected);
}

/// Near the start the window is clamped to the recording, not shifted.
#[test]
fn a_moment_near_the_start_is_clamped_to_the_recording() {
    let mut scene = Scene::new("clamp-start");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);

    assert_eq!(scene.heard(MIC, 1), values(MIC, 0, 3_500));
}

/// Near the end the window is clamped to the recording's last sample.
#[test]
fn a_moment_near_the_end_is_clamped_to_the_recording() {
    let mut scene = Scene::new("clamp-end");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);

    assert_eq!(scene.heard(MIC, 9), values(MIC, 6_500, 10_000));
}

/// A track recorded later than the session's start has its audio at its
/// own session times, not at sample zero.
#[test]
fn audio_that_starts_late_in_the_session_plays_at_its_session_time() {
    let mut scene = Scene::new("late-start");
    scene.epoch(MIC, &anchor(0, 100, 0));
    scene.segment(MIC, 0, 0, 10_000);

    assert_eq!(scene.heard(MIC, 105), values(MIC, 2_500, 7_500));
}

/// The track asked for is the one heard, though another has audio at the
/// same time.
#[test]
fn only_the_track_asked_for_is_heard() {
    let mut scene = Scene::new("two-tracks");
    for track in [MIC, SYSTEM] {
        scene.epoch(track, &anchor(0, 0, 0));
        scene.segment(track, 0, 0, 10_000);
    }

    assert_eq!(scene.heard(SYSTEM, 5), values(SYSTEM, 2_500, 7_500));
    assert_eq!(scene.heard(MIC, 5), values(MIC, 2_500, 7_500));
}

/// A full window plays five seconds, at the rate of the epoch.
#[test]
fn a_full_moment_is_five_seconds_at_the_epochs_rate() {
    let mut scene = Scene::new("length");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);
    let output = FakeOutput::default();

    let playing = scene.play(&output, MIC, 5).unwrap();

    assert_eq!(playing.length(), Duration::from_secs(5));
    assert_eq!(output.started.lock().unwrap()[0].1, rate());
}

/// While a recording runs it holds the session's lock, and playing is
/// refused before anything is read or played.
#[test]
fn playing_is_refused_while_a_recording_runs() {
    let mut scene = Scene::new("recording");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);
    let output = FakeOutput::default();
    // The lock is let go at once: it's the writer that holds the session.
    let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
    let recording = SessionWriter::open(
        &scene.session.lock().unwrap(),
        rate(),
        SegmentLength::default_at(rate()),
        clock,
    )
    .unwrap();

    let refused = scene.play(&output, MIC, 5).unwrap_err();

    assert!(matches!(refused, PlaybackError::Busy), "{refused}");
    assert_eq!(
        refused.to_string(),
        "can't play now: the session is being recorded or repaired, or already playing"
    );
    assert!(output.started.lock().unwrap().is_empty());
    // Once the recording is over the same call plays.
    drop(recording);
    assert_eq!(scene.heard(MIC, 5), values(MIC, 2_500, 7_500));
}

/// A moment that is playing owns the session until it stops: nothing can
/// start recording it, and the sound stops with the call.
#[test]
fn a_playing_moment_holds_the_session_until_it_stops() {
    let mut scene = Scene::new("holds");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);
    let output = FakeOutput::default();

    let playing = scene.play(&output, MIC, 5).unwrap();
    // Another space while the first plays is refused until the first stops.
    assert!(matches!(
        scene.play(&output, MIC, 6),
        Err(PlaybackError::Busy)
    ));
    assert_eq!(
        scene.session.lock().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(!output.stopped.load(Ordering::Acquire));

    playing.stop();

    assert!(output.stopped.load(Ordering::Acquire));
    assert!(scene.session.lock().is_ok());
}

/// Dropping the playback is stopping it: the card closing is enough.
#[test]
fn dropping_a_playback_stops_the_sound() {
    let mut scene = Scene::new("drop");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);
    let output = FakeOutput::default();

    drop(scene.play(&output, MIC, 5).unwrap());

    assert!(output.stopped.load(Ordering::Acquire));
}

/// An output that refuses ends the call with its reason, and the session
/// isn't left held.
#[test]
fn an_output_that_refuses_gives_its_reason_and_lets_the_session_go() {
    let mut scene = Scene::new("output-refuses");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);
    let output = FakeOutput {
        refuse: Some(OutputError::NoDevice),
        ..FakeOutput::default()
    };

    let refused = scene.play(&output, MIC, 5).unwrap_err();

    assert!(
        matches!(refused, PlaybackError::Output(OutputError::NoDevice)),
        "{refused}"
    );
    assert_eq!(
        refused.to_string(),
        "couldn't play it: there is no output device"
    );
    assert!(scene.session.lock().is_ok());
}

/// A time outside the recording is refused, on either side.
#[test]
fn a_time_outside_the_recording_is_refused() {
    let mut scene = Scene::new("outside");
    scene.epoch(MIC, &anchor(0, 10, 0));
    scene.segment(MIC, 0, 0, 5_000);
    let output = FakeOutput::default();

    for at in [9, 16] {
        let refused = scene.play(&output, MIC, at).unwrap_err();
        assert!(
            matches!(refused, PlaybackError::OutsideRecording { at: t } if t == secs(at)),
            "{refused}"
        );
    }
    assert!(output.started.lock().unwrap().is_empty());
}

/// A track with no published audio has nothing to play.
#[test]
fn a_track_without_audio_has_nothing_to_play() {
    let mut scene = Scene::new("no-audio");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);

    let refused = scene.play(&FakeOutput::default(), SYSTEM, 5).unwrap_err();

    assert!(
        matches!(refused, PlaybackError::NoAudio(t) if t == SYSTEM),
        "{refused}"
    );
}

/// Segments without an epoch to time them can't be placed on the session
/// clock.
#[test]
fn audio_without_epochs_cant_be_placed_in_time() {
    let mut scene = Scene::new("untimed");
    scene.segment(MIC, 0, 0, 10_000);

    let refused = scene.play(&FakeOutput::default(), MIC, 5).unwrap_err();

    assert!(matches!(refused, PlaybackError::NotTimed), "{refused}");
}

/// A hole between published segments inside the window is refused, naming
/// its first sample, rather than played as if it were silence.
#[test]
fn a_hole_between_segments_is_refused_not_played() {
    let mut scene = Scene::new("hole");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 3_000);
    scene.segment(MIC, 0, 4_000, 9_000);
    let output = FakeOutput::default();

    let refused = scene.play(&output, MIC, 5).unwrap_err();

    assert!(
        matches!(refused, PlaybackError::Missing { from } if from == SampleIndex::new(3_000)),
        "{refused}"
    );
    assert!(output.started.lock().unwrap().is_empty());
}

/// A segment whose file is gone is reported as that segment's failure.
#[test]
fn a_segment_file_that_is_gone_is_refused() {
    let mut scene = Scene::new("file-gone");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);
    let range = SampleRange::new(SampleIndex::ZERO, SampleIndex::new(10_000)).unwrap();
    scene
        .fs
        .remove(&Path::new("/s").join(segment_file_name(MIC, range)))
        .unwrap();

    let refused = scene.play(&FakeOutput::default(), MIC, 5).unwrap_err();

    assert!(
        matches!(refused, PlaybackError::Segment(ReadSegmentError::Io(_))),
        "{refused}"
    );
}

/// The plan for a clock that runs fast: the window holds more samples than
/// the nominal rate, as the epoch's mapping says.
#[test]
fn a_drifting_epoch_puts_more_samples_in_the_window() {
    let drifting = EpochAnchor {
        drift: Drift::from_ppb(Drift::MAX_PPB).unwrap(),
        ..anchor(0, 0, 0)
    };
    let timeline = TrackTimeline::rebuild(MIC, [drifting]).unwrap();
    let recorded = SampleRange::new(SampleIndex::ZERO, SampleIndex::new(20_000)).unwrap();

    let planned = plan(&timeline, recorded, secs(5)).unwrap();

    // 0.1% fast: 1,001 samples a second. Sample 2,502 plays at 2.4995 s, the
    // one playing at the window's start; the first at or after 7.5 s is 7,508.
    let range = SampleRange::new(SampleIndex::new(2_502), SampleIndex::new(7_508)).unwrap();
    assert_eq!(planned.pieces, [Piece::Audio(range)]);
}

/// Epochs at different rates in one window can't be played as one stream.
#[test]
fn epochs_at_different_rates_in_one_window_are_refused() {
    let faster = EpochAnchor {
        rate: SampleRate::new(2_000).unwrap(),
        ..anchor(1, 4, 4_000)
    };
    let timeline = TrackTimeline::rebuild(MIC, [anchor(0, 0, 0), faster]).unwrap();
    let recorded = SampleRange::new(SampleIndex::ZERO, SampleIndex::new(9_000)).unwrap();

    let refused = plan(&timeline, recorded, secs(4)).unwrap_err();

    assert!(matches!(refused, PlaybackError::RateChanged), "{refused}");
}

/// A window that meets only a gap is all silence, at the rate of the
/// track's first epoch.
#[test]
fn a_window_inside_a_long_gap_is_all_silence() {
    let timeline = TrackTimeline::rebuild(MIC, [anchor(0, 0, 0), anchor(1, 60, 1_000)]).unwrap();
    let recorded = SampleRange::new(SampleIndex::ZERO, SampleIndex::new(2_000)).unwrap();

    let planned = plan(&timeline, recorded, secs(30)).unwrap();

    assert_eq!(planned.rate, rate());
    assert_eq!(planned.pieces, [Piece::Silence(Duration::from_secs(5))]);
}

/// A moment whose audio stops short of the next epoch (its last journal is
/// unsalvaged) is refused, not played short with the next epoch's audio
/// early.
#[test]
fn a_hole_at_the_end_of_an_epoch_is_refused_not_played() {
    let mut scene = Scene::new("trailing-hole");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.epoch(MIC, &anchor(1, 6, 4_000));
    scene.segment(MIC, 0, 0, 3_000);
    scene.segment(MIC, 1, 4_000, 9_000);
    let output = FakeOutput::default();

    let refused = scene.play(&output, MIC, 5).unwrap_err();

    assert!(
        matches!(refused, PlaybackError::Missing { from } if from == SampleIndex::new(3_000)),
        "{refused}"
    );
    assert!(output.started.lock().unwrap().is_empty());
}

/// Audio from before the first epoch has no time, but the timed part after
/// it plays.
#[test]
fn audio_before_the_first_epoch_is_left_out_and_the_rest_plays() {
    let mut scene = Scene::new("untimed-start");
    // Samples 0..2,000 came from journals kept before epochs were.
    scene.epoch(MIC, &anchor(0, 0, 2_000));
    scene.segment(MIC, 0, 0, 2_000);
    scene.segment(MIC, 0, 2_000, 8_000);

    // Sample 2,000 plays at 0 s, so 3 s is sample 5,000.
    assert_eq!(scene.heard(MIC, 3), values(MIC, 2_500, 7_500));
}

/// When every sample is before the first epoch, nothing can be placed.
#[test]
fn audio_that_is_all_before_the_first_epoch_cant_be_placed() {
    let mut scene = Scene::new("all-untimed");
    scene.epoch(MIC, &anchor(0, 0, 20_000));
    scene.segment(MIC, 0, 0, 8_000);

    let refused = scene.play(&FakeOutput::default(), MIC, 3).unwrap_err();

    assert!(matches!(refused, PlaybackError::NotTimed), "{refused}");
}

/// The playback reports what its output stream reports.
#[test]
fn a_playback_is_finished_when_its_stream_is() {
    let mut scene = Scene::new("finished");
    scene.epoch(MIC, &anchor(0, 0, 0));
    scene.segment(MIC, 0, 0, 10_000);
    let output = FakeOutput::default();
    let playing = scene.play(&output, MIC, 5).unwrap();

    assert!(!playing.finished());
    output.finished.store(true, Ordering::Release);
    assert!(playing.finished());
}

/// The messages the card shows, one for each way playing can fail.
#[test]
fn every_failure_has_a_message_for_the_card() {
    let io = || io::Error::other("disk");
    let at = SessionTime::ZERO;
    let from = SampleIndex::ZERO;
    for (error, message) in [
        (
            PlaybackError::Session(io()),
            "couldn't open the session to play it: disk",
        ),
        (
            PlaybackError::Store(StoreError::OutOfRange),
            "couldn't read the session from the library: a number does not fit a 64-bit signed integer",
        ),
        (
            PlaybackError::Timeline(EpochError::TooManyEpochs),
            "the session's timing is not usable: the track has run out of epoch numbers",
        ),
        (PlaybackError::NoAudio(MIC), "track 0 has no saved audio"),
        (
            PlaybackError::NotTimed,
            "the session has no timing for this track's audio",
        ),
        (
            PlaybackError::OutsideRecording { at },
            "that time is outside the recording",
        ),
        (
            PlaybackError::Missing { from },
            "part of that moment's audio isn't saved",
        ),
        (
            PlaybackError::RateChanged,
            "the moment spans audio at different rates",
        ),
        (
            PlaybackError::TimeOverflow,
            "the moment's time is out of range",
        ),
        (
            PlaybackError::Segment(ReadSegmentError::Hash),
            "a saved segment can't be played: doesn't match its row's SHA-256",
        ),
        (
            PlaybackError::Output(OutputError::NoDevice),
            "couldn't play it: there is no output device",
        ),
    ] {
        assert_eq!(error.to_string(), message);
    }
    assert_eq!(
        OutputError::Unsupported("f64".into()).to_string(),
        "the output device refused the format: f64"
    );
    assert_eq!(
        OutputError::Backend("busy".into()).to_string(),
        "the audio system refused: busy"
    );
}

/// An error that wraps another gives it back as its source, so the cause
/// isn't lost; one that wraps nothing gives none.
#[test]
fn errors_keep_their_cause() {
    let has_source = |error: PlaybackError| std::error::Error::source(&error).is_some();
    assert!(has_source(PlaybackError::Session(io::Error::other("disk"))));
    assert!(has_source(PlaybackError::Store(StoreError::OutOfRange)));
    assert!(has_source(PlaybackError::Timeline(
        EpochError::TooManyEpochs
    )));
    assert!(has_source(PlaybackError::Segment(ReadSegmentError::Hash)));
    assert!(has_source(PlaybackError::Output(OutputError::NoDevice)));
    assert!(!has_source(PlaybackError::Busy));
    assert!(!has_source(PlaybackError::NotTimed));
}
