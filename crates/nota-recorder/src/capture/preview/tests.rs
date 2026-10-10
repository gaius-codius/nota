//! The preview on a scripted backend: levels, failures, listening to other
//! sources, the device list, and stopping.

use std::sync::Mutex;

use nota_core::FakeClock;

use super::super::{CaptureSender, Device};
use super::*;

const MIC: TrackId = TrackId::new(0);
const SYSTEM: TrackId = TrackId::new(1);

/// How long a test waits for the preview thread: far longer than it needs.
const PATIENCE: Duration = Duration::from_secs(10);

/// What the scripted backend saw.
#[derive(Debug)]
enum Seen {
    /// A stream opened on the source; the test sends its audio through
    /// the sender.
    Started(Source, CaptureSender),
    /// The source's stream was dropped.
    Stopped(Source),
}

/// A backend whose streams only say when they open and close.
struct Scripted {
    seen: Sender<Seen>,
    /// Sources whose streams can't be opened.
    refused: Vec<Source>,
    /// What the audio server has now.
    devices: Arc<Mutex<Devices>>,
}

/// A stream of the scripted backend.
struct Open {
    source: Source,
    seen: Sender<Seen>,
}

impl Drop for Open {
    fn drop(&mut self) {
        // A test that has stopped looking doesn't need to hear it.
        let _ = self.seen.send(Seen::Stopped(self.source.clone()));
    }
}

impl CaptureBackend for Scripted {
    type Stream = Open;

    fn start(
        &self,
        source: &Source,
        _rate: SampleRate,
        events: CaptureSender,
    ) -> Result<Open, CaptureError> {
        if self.refused.contains(source) {
            return Err(CaptureError::DeviceNotAvailable(source.clone()));
        }
        let _ = self.seen.send(Seen::Started(source.clone(), events));
        Ok(Open {
            source: source.clone(),
            seen: self.seen.clone(),
        })
    }

    fn devices(&self) -> Result<Devices, CaptureError> {
        Ok(self
            .devices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }
}

/// A preview on a scripted backend, and what the test watches it by.
struct Rig {
    preview: Preview,
    events: Receiver<PreviewEvent>,
    seen: Receiver<Seen>,
    clock: Arc<FakeClock>,
    devices: Arc<Mutex<Devices>>,
}

impl Rig {
    /// A rig whose backend can't open the `refused` sources.
    fn new(refused: Vec<Source>) -> Self {
        let (seen_tx, seen) = mpsc::channel();
        let devices = Arc::new(Mutex::new(Devices::default()));
        let clock = Arc::new(FakeClock::new(SessionTime::ZERO));
        let backend = Scripted {
            seen: seen_tx,
            refused,
            devices: Arc::clone(&devices),
        };
        let (preview, events) = Preview::start(
            backend,
            SampleRate::SPEECH,
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .unwrap();
        Self {
            preview,
            events,
            seen,
            clock,
            devices,
        }
    }

    /// The next thing the backend saw.
    fn seen(&self) -> Seen {
        self.seen.recv_timeout(PATIENCE).unwrap()
    }

    /// The sender of the stream the backend opened next, for `source`.
    fn started(&self, source: &Source) -> CaptureSender {
        match self.seen() {
            Seen::Started(opened, sender) if opened == *source => sender,
            other => panic!("expected {source:?} to start, saw {other:?}"),
        }
    }

    /// The next event that isn't the device list.
    fn next(&self) -> PreviewEvent {
        loop {
            match self.events.recv_timeout(PATIENCE).unwrap() {
                PreviewEvent::Devices(_) => {}
                event => return event,
            }
        }
    }

    /// The next event, the device list included.
    fn next_any(&self) -> PreviewEvent {
        self.events.recv_timeout(PATIENCE).unwrap()
    }
}

fn level(track: TrackId, peak: u16) -> PreviewEvent {
    PreviewEvent::Level {
        track,
        level: Level::from_peak(peak),
    }
}

/// A level is the peak of what the stream heard, by its magnitude.
#[test]
fn a_level_is_the_peak_of_what_the_stream_heard() {
    let rig = Rig::new(Vec::new());
    rig.preview.listen(&[(MIC, Source::Microphone)]);
    let sender = rig.started(&Source::Microphone);
    // The quietest-looking peak is the negative one: it's the magnitude
    // that counts.
    sender.audio(&[3, -700, 12]);
    assert_eq!(rig.next(), level(MIC, 700));
}

/// Each track's level is its own stream's.
#[test]
fn each_track_has_its_own_level() {
    let rig = Rig::new(Vec::new());
    rig.preview
        .listen(&[(MIC, Source::Microphone), (SYSTEM, Source::SystemAudio)]);
    let mic = rig.started(&Source::Microphone);
    let system = rig.started(&Source::SystemAudio);
    system.audio(&[9_000]);
    mic.audio(&[40]);
    let mut heard = [rig.next(), rig.next()];
    heard.sort_by_key(|event| match event {
        PreviewEvent::Level { track, .. } => track.get(),
        _ => u32::MAX,
    });
    assert_eq!(heard, [level(MIC, 40), level(SYSTEM, 9_000)]);
}

/// A source that can't be opened is told, with its track and why, and the
/// other stream's levels still come.
#[test]
fn a_source_that_cant_be_opened_is_told_and_the_other_carries_on() {
    let rig = Rig::new(vec![Source::Microphone]);
    rig.preview
        .listen(&[(MIC, Source::Microphone), (SYSTEM, Source::SystemAudio)]);
    let system = rig.started(&Source::SystemAudio);
    system.audio(&[500]);
    assert_eq!(
        rig.next(),
        PreviewEvent::Failed {
            track: MIC,
            error: CaptureError::DeviceNotAvailable(Source::Microphone),
        }
    );
    assert_eq!(rig.next(), level(SYSTEM, 500));
}

/// A stream that fails while it runs is told once, however many times it
/// says so, and the other track carries on.
#[test]
fn a_failing_stream_is_told_once() {
    let rig = Rig::new(Vec::new());
    rig.preview
        .listen(&[(MIC, Source::Microphone), (SYSTEM, Source::SystemAudio)]);
    let mic = rig.started(&Source::Microphone);
    let system = rig.started(&Source::SystemAudio);
    let gone = CaptureError::DeviceNotAvailable(Source::Microphone);
    mic.failed(gone.clone());
    mic.failed(gone.clone());
    // Queued behind both: a second Failed would come before this level.
    system.audio(&[77]);
    assert_eq!(
        rig.next(),
        PreviewEvent::Failed {
            track: MIC,
            error: gone
        }
    );
    assert_eq!(rig.next(), level(SYSTEM, 77));
}

/// Listening to other sources stops the old streams before it opens the
/// new, and the old ones' audio no longer reaches the screen.
#[test]
fn listening_to_other_sources_stops_the_old_streams_first() {
    let rig = Rig::new(Vec::new());
    rig.preview.listen(&[(MIC, Source::Microphone)]);
    let old = rig.started(&Source::Microphone);
    rig.preview
        .listen(&[(MIC, Source::Device("usb".to_owned()))]);
    assert!(matches!(rig.seen(), Seen::Stopped(Source::Microphone)));
    let new = rig.started(&Source::Device("usb".to_owned()));
    // What the stopped stream still sends goes nowhere; the new one's is
    // heard.
    old.audio(&[30_000]);
    new.audio(&[11]);
    assert_eq!(rig.next(), level(MIC, 11));
}

/// Listening to nothing stops every stream.
#[test]
fn listening_to_nothing_stops_the_streams() {
    let rig = Rig::new(Vec::new());
    rig.preview.listen(&[(MIC, Source::Microphone)]);
    let _opened = rig.started(&Source::Microphone);
    rig.preview.listen(&[]);
    assert!(matches!(rig.seen(), Seen::Stopped(Source::Microphone)));
}

/// The devices are told when first read, and again once they change and
/// two seconds have passed.
#[test]
fn the_devices_are_told_when_read_and_when_they_change() {
    let rig = Rig::new(Vec::new());
    assert_eq!(rig.next_any(), PreviewEvent::Devices(Devices::default()));
    let headset = Devices {
        outputs: vec![Device {
            name: "headset".to_owned(),
            description: "Headset".to_owned(),
        }],
        default_output: Some("headset".to_owned()),
        ..Devices::default()
    };
    rig.devices
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone_from(&headset);
    rig.clock.advance(DEVICES_EVERY);
    assert_eq!(rig.next_any(), PreviewEvent::Devices(headset));
}

/// Dropping the preview stops the streams, ends the thread and closes the
/// events.
#[test]
fn dropping_the_preview_stops_the_streams_and_the_thread() {
    let Rig {
        preview,
        events,
        seen,
        ..
    } = {
        let rig = Rig::new(Vec::new());
        rig.preview.listen(&[(MIC, Source::Microphone)]);
        let _opened = rig.started(&Source::Microphone);
        rig
    };
    // Joins the thread: what it stopped is already told.
    drop(preview);
    assert!(matches!(
        seen.try_recv(),
        Ok(Seen::Stopped(Source::Microphone))
    ));
    // What was told before the drop is still there to read; then the
    // channel is closed, not just quiet.
    while events.recv_timeout(PATIENCE).is_ok() {}
    assert!(matches!(
        events.recv_timeout(PATIENCE),
        Err(RecvTimeoutError::Disconnected)
    ));
}

/// A preview whose receiver has gone ends its thread, and with it its
/// streams, at the next thing it has to tell.
#[test]
fn a_preview_nobody_listens_to_ends() {
    let rig = Rig::new(Vec::new());
    rig.preview.listen(&[(MIC, Source::Microphone)]);
    let mic = rig.started(&Source::Microphone);
    let Rig {
        preview: _preview,
        events,
        seen,
        ..
    } = rig;
    drop(events);
    mic.audio(&[5]);
    // The preview is still held: only the lost receiver ended the thread.
    assert!(matches!(
        seen.recv_timeout(PATIENCE),
        Ok(Seen::Stopped(Source::Microphone))
    ));
}

fn at_ms(ms: u64) -> SessionTime {
    SessionTime::from_nanos(ms * 1_000_000)
}

/// The first audio of a track is told at once, as its peak.
#[test]
fn a_track_s_first_audio_gives_a_level() {
    let mut meters = Meters::default();
    assert_eq!(
        meters.hear(MIC, &[10, -20], at_ms(0)),
        Some(Level::from_peak(20))
    );
}

/// Between two levels of a track, what it hears is kept as a peak, and
/// the next is due exactly 100 ms after the last.
#[test]
fn a_level_is_due_every_100_ms_and_holds_the_peak_between() {
    let mut meters = Meters::default();
    assert!(meters.hear(MIC, &[1], at_ms(0)).is_some());
    assert_eq!(meters.hear(MIC, &[300], at_ms(10)), None);
    assert_eq!(meters.hear(MIC, &[200], at_ms(50)), None);
    assert_eq!(meters.hear(MIC, &[150], at_ms(99)), None);
    assert_eq!(
        meters.hear(MIC, &[50], at_ms(100)),
        Some(Level::from_peak(300))
    );
    // The peak starts again after a level.
    assert_eq!(meters.hear(MIC, &[7], at_ms(150)), None);
    assert_eq!(
        meters.hear(MIC, &[6], at_ms(200)),
        Some(Level::from_peak(7))
    );
}

/// One track's level doesn't hold another's back.
#[test]
fn each_track_is_paced_on_its_own() {
    let mut meters = Meters::default();
    assert!(meters.hear(MIC, &[1], at_ms(0)).is_some());
    assert!(meters.hear(SYSTEM, &[1], at_ms(10)).is_some());
    assert_eq!(meters.hear(MIC, &[1], at_ms(20)), None);
}

/// A clock that reads earlier than the last level (it can't) doesn't make
/// one due.
#[test]
fn an_earlier_time_is_not_due() {
    let mut meters = Meters::default();
    assert!(meters.hear(MIC, &[1], at_ms(500)).is_some());
    assert_eq!(meters.hear(MIC, &[1], at_ms(100)), None);
}

fn some_devices() -> Devices {
    Devices {
        default_input: Some("mic".to_owned()),
        ..Devices::default()
    }
}

/// The devices are due at the first look, then every two seconds, to the
/// nanosecond.
#[test]
fn the_devices_are_due_every_two_seconds() {
    let mut poll = DevicesPoll::default();
    assert!(poll.due(at_ms(0)));
    assert_eq!(
        poll.took(at_ms(0), Ok(some_devices())),
        Some(some_devices())
    );
    assert!(!poll.due(at_ms(1_999)));
    assert!(poll.due(at_ms(2_000)));
}

/// The same devices are not told again, different ones are, and a failed
/// read tells nothing and keeps what was known.
#[test]
fn only_a_change_is_told() {
    let mut poll = DevicesPoll::default();
    assert_eq!(
        poll.took(at_ms(0), Ok(some_devices())),
        Some(some_devices())
    );
    assert_eq!(poll.took(at_ms(2_000), Ok(some_devices())), None);
    let failed = Err(CaptureError::HostUnavailable("down".to_owned()));
    assert_eq!(poll.took(at_ms(4_000), failed), None);
    assert!(!poll.due(at_ms(5_000)), "a failed read counts as a read");
    assert_eq!(poll.took(at_ms(6_000), Ok(some_devices())), None);
    assert_eq!(
        poll.took(at_ms(8_000), Ok(Devices::default())),
        Some(Devices::default())
    );
}
