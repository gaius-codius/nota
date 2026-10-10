//! Golden tests for the Setup screen: the UI spec's `SetupM2` mockup, drawn
//! through `ratatui`'s `TestBackend` and compared with insta. The device
//! names are invented.
//!
//! The design is a draft. When the screen changes on purpose, update the
//! mockup and accept the new snapshot (`cargo insta review`) in the same
//! change.

#![cfg(test)]

use std::time::Duration;

use nota_core::recorder::{Input, Level};
use nota_tui::{Device, Devices, Listen, Setup, Source, Theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// The mockup's devices: speakers as the default output, a USB microphone
/// as the default input.
fn devices() -> Devices {
    let device = |name: &str, description: &str| Device {
        name: name.to_owned(),
        description: description.to_owned(),
    };
    Devices {
        outputs: vec![
            device("alsa.speakers", "Speakers"),
            device("bluez.headphones", "Headphones"),
        ],
        inputs: vec![device("usb.seiren", "Seiren Mini")],
        default_output: Some("alsa.speakers".to_owned()),
        default_input: Some("usb.seiren".to_owned()),
    }
}

/// The mockup's moment: the system audio chosen and moving, the
/// microphone quiet, a title pre-filled with the date and time.
fn mockup_setup() -> Setup {
    let mut setup = Setup::new(
        "9 Oct, 14:05",
        "parakeet, live and after the stop",
        Theme::default(),
    )
    .remembering(Listen::System, Input::Default, Input::Default);
    setup.set_devices(devices());
    for peak in [9_000, 1_500, 100, 3_000, 22_000, 14_000, 1_500, 100] {
        setup.set_level(Source::System, Level::from_peak(peak));
    }
    for _ in 0..8 {
        setup.set_level(Source::Microphone, Level::SILENT);
    }
    setup
}

fn draw(setup: &mut Setup, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| setup.draw(frame)).unwrap();
    terminal
}

/// The screen as the mockup has it, without the space line. It is drawn 72
/// columns wide rather than the mockup's 62, so the microphone's
/// `Seiren Mini · default` and the footer's hint fit whole.
#[test]
fn setup() {
    let terminal = draw(&mut mockup_setup(), 72, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// With under four hours of room, the space line shows under Engines.
#[test]
fn setup_with_the_space_line() {
    let mut setup = mockup_setup();
    setup.set_space(Some(Duration::from_mins(2 * 60 + 10)));
    let terminal = draw(&mut setup, 72, 20);
    insta::assert_snapshot!(terminal.backend());
}

/// Both sources chosen: both lines of plain words, a pinned microphone
/// named by its device, and the microphone's stream failed.
#[test]
fn setup_with_both_and_a_pinned_device() {
    let mut setup = Setup::new("Joinery · mortise and tenon", "parakeet", Theme::default())
        .remembering(
            Listen::Both,
            Input::Device("bluez.headphones".to_owned()),
            Input::Default,
        );
    setup.set_devices(devices());
    setup.set_failed(Source::Microphone);
    let terminal = draw(&mut setup, 70, 22);
    insta::assert_snapshot!(terminal.backend());
}

/// A terminal below the minimum asks for more room instead of a broken
/// layout.
#[test]
fn setup_too_small() {
    let terminal = draw(&mut mockup_setup(), 50, 12);
    insta::assert_snapshot!(terminal.backend());
}
