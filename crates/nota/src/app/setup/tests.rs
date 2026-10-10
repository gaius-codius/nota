use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use nota_core::recorder::Level;
use nota_core::{FakeClock, SessionTime};
use nota_recorder::capture::{CaptureError, CaptureSender, Device};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::app::tests::TestDir;

/// How long a test waits for the preview's thread.
const WAIT: Duration = Duration::from_secs(10);

/// A backend that opens no real stream: each source it's asked to start
/// is sent on a channel, so a test sees what the preview listens to.
struct Opened(Sender<Capture>);

impl CaptureBackend for Opened {
    type Stream = ();

    fn start(
        &self,
        source: &Capture,
        _rate: nota_core::SampleRate,
        _events: CaptureSender,
    ) -> Result<(), CaptureError> {
        let _ = self.0.send(source.clone());
        Ok(())
    }
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(FakeClock::new(SessionTime::ZERO))
}

/// A screen with two outputs and two inputs, the first of each the default.
fn screen() -> nota_tui::Setup {
    let device = |name: &str, description: &str| nota_tui::Device {
        name: name.to_owned(),
        description: description.to_owned(),
    };
    let mut setup = nota_tui::Setup::new("9 Oct, 14:05", "parakeet", Theme::default());
    setup.set_devices(nota_tui::Devices {
        outputs: vec![
            device("alsa.speakers", "Speakers"),
            device("bluez.headphones", "Headphones"),
        ],
        inputs: vec![device("usb.seiren", "Seiren Mini")],
        default_output: Some("alsa.speakers".to_owned()),
        default_input: Some("usb.seiren".to_owned()),
    });
    setup
}

/// The screen's rows as text.
fn rows(setup: &mut nota_tui::Setup) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(72, 20)).unwrap();
    terminal.draw(|frame| setup.draw(frame)).unwrap();
    let buf = terminal.backend().buffer();
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

/// The row of the screen that starts with `name` after its marker.
fn row_of(rows: &[String], name: &str) -> String {
    rows.iter()
        .find(|row| row.contains(name))
        .unwrap_or_else(|| panic!("no row names {name}: {rows:?}"))
        .clone()
}

fn meters(data: &Path) -> (Meters, Receiver<Capture>) {
    let (opened, sources) = channel();
    let (preview, heard) = Preview::start(Opened(opened), RATE, clock()).unwrap();
    (
        Meters::new(preview, heard, data.to_path_buf(), clock()),
        sources,
    )
}

/// A level the preview heard moves the meter of its own source, and not
/// the other's.
#[test]
fn a_level_moves_its_sources_meter() {
    let mut setup = screen();
    for _ in 0..8 {
        apply(
            &mut setup,
            PreviewEvent::Level {
                track: SYSTEM,
                level: Level::FULL_SCALE,
            },
        );
    }
    let rows = rows(&mut setup);
    assert!(row_of(&rows, "System audio").contains("████████"));
    assert!(row_of(&rows, "Microphone").contains("▁▁▁▁▁▁▁▁"));
    for _ in 0..8 {
        apply(
            &mut setup,
            PreviewEvent::Level {
                track: MIC,
                level: Level::FULL_SCALE,
            },
        );
    }
    assert!(row_of(&rows_of(&mut setup), "Microphone").contains("████████"));
}

fn rows_of(setup: &mut nota_tui::Setup) -> Vec<String> {
    rows(setup)
}

/// A stream that fails says so on its own row.
#[test]
fn a_failed_stream_says_there_is_no_signal() {
    let mut setup = screen();
    apply(
        &mut setup,
        PreviewEvent::Failed {
            track: MIC,
            error: CaptureError::DeviceNotAvailable(Capture::Microphone),
        },
    );
    let rows = rows(&mut setup);
    assert!(row_of(&rows, "Microphone").contains("⚠ no signal"));
    assert!(!row_of(&rows, "System audio").contains("⚠"));
}

/// The devices the preview reports are what the screen names and lists.
#[test]
fn the_devices_reported_are_named_on_the_screen() {
    let mut setup = nota_tui::Setup::new("T", "parakeet", Theme::default());
    let device = |name: &str, description: &str| Device {
        name: name.to_owned(),
        description: description.to_owned(),
    };
    apply(
        &mut setup,
        PreviewEvent::Devices(Devices {
            outputs: vec![device("alsa.speakers", "Speakers")],
            inputs: vec![device("usb.seiren", "Seiren Mini")],
            default_output: Some("alsa.speakers".to_owned()),
            default_input: Some("usb.seiren".to_owned()),
        }),
    );
    let rows = rows(&mut setup);
    assert!(row_of(&rows, "System audio").contains("Speakers · default"));
    assert!(row_of(&rows, "Microphone").contains("Seiren Mini · default"));
}

/// The preview listens to both sources on the defaults at first, and to
/// the new device once `→` pins one: every meter moves whichever source
/// is chosen, and a new choice reopens its stream.
#[test]
fn the_preview_follows_the_choice() {
    let tmp = TestDir::new("preview-follows");
    let (mut meters, opened) = meters(&tmp.0);
    let mut setup = screen();
    meters.refresh(&mut setup);
    let first = [
        opened.recv_timeout(WAIT).unwrap(),
        opened.recv_timeout(WAIT).unwrap(),
    ];
    assert_eq!(first, [Capture::SystemAudio, Capture::Microphone]);
    // Nothing changed: the preview is left alone.
    meters.refresh(&mut setup);
    setup.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    meters.refresh(&mut setup);
    let then = [
        opened.recv_timeout(WAIT).unwrap(),
        opened.recv_timeout(WAIT).unwrap(),
    ];
    assert_eq!(
        then,
        [
            Capture::Device("alsa.speakers".to_owned()),
            Capture::Microphone
        ]
    );
}

/// Every file under `dir`, with its size, so a test can see that nothing
/// was written.
fn files(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                pending.push(entry.path());
            }
            found.push((entry.path(), meta.len()));
        }
    }
    found.sort();
    found
}

/// A preview creates no session and no journal: the data directory holds
/// the same files after it ran as before, with `sessions/` empty.
#[test]
fn a_preview_creates_no_session_and_no_journal() {
    let tmp = TestDir::new("preview-writes-nothing");
    let library = Library::open(&tmp.0).unwrap();
    let before = files(&tmp.0);
    let (mut meters, opened) = meters(&tmp.0);
    let mut setup = screen();
    meters.refresh(&mut setup);
    // Both streams were opened, so the preview ran.
    opened.recv_timeout(WAIT).unwrap();
    opened.recv_timeout(WAIT).unwrap();
    // The preview's streams close before anything else opens them.
    drop(meters);
    assert_eq!(files(&tmp.0), before);
    assert!(library.listing(RATE).unwrap().is_empty());
    assert!(
        std::fs::read_dir(tmp.0.join("sessions"))
            .unwrap()
            .next()
            .is_none()
    );
}

/// What `R` records with: nothing kept and no earlier recording is both
/// sources on the defaults; Setup's kept choice wins over the tracks of
/// the last recording, which still give its title.
#[test]
fn r_records_with_what_setup_kept_or_else_the_last_recording() {
    let tmp = TestDir::new("r-settings");
    let library = Library::open(&tmp.0).unwrap();
    assert_eq!(
        last_setup_for(&library, &tmp.0),
        Setup {
            title: "Recording".to_owned(),
            mic: Some(Input::Default),
            system: Some(Input::Default),
        }
    );
    let library = crate::app::tests::library_of_three(&tmp.0);
    // No file yet: the last recording's tracks, as before Setup existed.
    assert_eq!(
        last_setup_for(&library, &tmp.0),
        Setup {
            title: "Finishing".to_owned(),
            mic: Some(Input::Default),
            system: Some(Input::Default),
        }
    );
    let kept = Remembered {
        listen: nota_tui::Listen::Microphone,
        system: Input::Default,
        microphone: Input::Device("mic".to_owned()),
    };
    remembered::write(&StdFs, &tmp.0, &kept).unwrap();
    // The kept choice: only the microphone, pinned to a device named `mic`,
    // which the tracks' display names couldn't tell from the default.
    assert_eq!(
        last_setup_for(&library, &tmp.0),
        Setup {
            title: "Finishing".to_owned(),
            mic: Some(Input::Device("mic".to_owned())),
            system: None,
        }
    );
}

/// `⏎` keeps the choice for `R` and records it; a choice that can't be
/// kept still records, with a note.
#[test]
fn starting_keeps_the_choice_and_says_when_it_could_not() {
    let tmp = TestDir::new("start-keeps");
    let mut setup = screen();
    setup.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let choice = setup.chosen();
    let Done::Start(started, note) = start(&choice, &tmp.0) else {
        panic!("Setup didn't start");
    };
    assert_eq!(note, None);
    assert_eq!(
        started,
        Setup {
            title: "9 Oct, 14:05".to_owned(),
            mic: Some(Input::Default),
            system: None,
        }
    );
    assert_eq!(
        remembered::read(&StdFs, &tmp.0).unwrap().unwrap().listen,
        nota_tui::Listen::Microphone
    );
    // A data directory that isn't there can't keep anything.
    let Done::Start(started, note) = start(&choice, &tmp.0.join("gone")) else {
        panic!("Setup didn't start");
    };
    assert_eq!(started.mic, Some(Input::Default));
    assert!(note.unwrap().starts_with("the choices couldn't be kept"));
}
