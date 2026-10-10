use ratatui::crossterm::event::KeyModifiers;

use super::*;

fn press(setup: &mut Setup, code: KeyCode) -> Option<SetupAction> {
    setup.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A Setup screen as the app opens it, with the audio server's devices:
/// two outputs and two inputs, the first of each the default.
fn screen() -> Setup {
    let device = |name: &str, description: &str| Device {
        name: name.to_owned(),
        description: description.to_owned(),
    };
    let mut setup = Setup::new("9 Oct, 14:05", "parakeet", Theme::default());
    setup.set_devices(Devices {
        outputs: vec![
            device("alsa.speakers", "Speakers"),
            device("bluez.headphones", "Headphones"),
        ],
        inputs: vec![
            device("alsa.seiren", "Seiren Mini"),
            device("alsa.webcam", "Webcam"),
        ],
        default_output: Some("alsa.speakers".to_owned()),
        default_input: Some("alsa.seiren".to_owned()),
    });
    setup
}

/// `←→` on a source goes through the default and each named device, and
/// wraps: pinning is one step from "default", and "default" is one step
/// back.
#[test]
fn left_and_right_pin_a_device_and_default_returns_to_following() {
    let mut setup = screen();
    assert_eq!(setup.chosen().system, Input::Default);
    press(&mut setup, KeyCode::Right);
    assert_eq!(setup.chosen().system, Input::Device("alsa.speakers".into()));
    press(&mut setup, KeyCode::Right);
    assert_eq!(
        setup.chosen().system,
        Input::Device("bluez.headphones".into())
    );
    // Past the last device it wraps to the default.
    press(&mut setup, KeyCode::Right);
    assert_eq!(setup.chosen().system, Input::Default);
    press(&mut setup, KeyCode::Left);
    assert_eq!(
        setup.chosen().system,
        Input::Device("bluez.headphones".into())
    );
    // The other source's device wasn't touched.
    assert_eq!(setup.chosen().microphone, Input::Default);
}

/// `↓` picks the microphone, and `←→` then changes its device, not the
/// system audio's.
#[test]
fn arrows_change_the_chosen_sources_device() {
    let mut setup = screen();
    press(&mut setup, KeyCode::Down);
    assert_eq!(setup.chosen().listen, Listen::Microphone);
    press(&mut setup, KeyCode::Right);
    press(&mut setup, KeyCode::Right);
    assert_eq!(
        setup.chosen().microphone,
        Input::Device("alsa.webcam".into())
    );
    assert_eq!(setup.chosen().system, Input::Default);
}

/// Both has no device of its own: `←→` there changes nothing.
#[test]
fn both_has_no_device_to_change() {
    let mut setup = screen();
    press(&mut setup, KeyCode::Down);
    press(&mut setup, KeyCode::Down);
    assert_eq!(setup.chosen().listen, Listen::Both);
    press(&mut setup, KeyCode::Right);
    assert_eq!(setup.chosen().system, Input::Default);
    assert_eq!(setup.chosen().microphone, Input::Default);
}

/// The source rows don't wrap: `↑` on the first and `↓` on the last stay.
#[test]
fn the_sources_do_not_wrap() {
    let mut setup = screen();
    press(&mut setup, KeyCode::Up);
    assert_eq!(setup.chosen().listen, Listen::System);
    for _ in 0..5 {
        press(&mut setup, KeyCode::Down);
    }
    assert_eq!(setup.chosen().listen, Listen::Both);
}

/// A device pinned earlier that the audio server no longer has shows by
/// its name and can be chosen away from; once left, it isn't offered again.
#[test]
fn a_pinned_device_that_has_gone_can_be_left() {
    let mut setup = screen().remembering(
        Listen::System,
        Input::Device("usb.gone".into()),
        Input::Default,
    );
    assert_eq!(setup.device_label(Source::System), "usb.gone");
    press(&mut setup, KeyCode::Right);
    assert_eq!(setup.chosen().system, Input::Default);
    press(&mut setup, KeyCode::Left);
    assert_eq!(
        setup.chosen().system,
        Input::Device("bluez.headphones".into())
    );
}

/// `⏎` starts, `esc` goes back and Ctrl+C quits, from the title as well
/// as the sources.
#[test]
fn enter_starts_and_escape_goes_back() {
    let mut setup = screen();
    assert_eq!(press(&mut setup, KeyCode::Enter), Some(SetupAction::Start));
    assert_eq!(press(&mut setup, KeyCode::Esc), Some(SetupAction::Back));
    press(&mut setup, KeyCode::Tab);
    assert_eq!(press(&mut setup, KeyCode::Enter), Some(SetupAction::Start));
    assert_eq!(press(&mut setup, KeyCode::Esc), Some(SetupAction::Back));
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(setup.handle_key(ctrl_c), Some(SetupAction::Quit));
}

/// Letters type into the title only while it has the cursor: on the
/// sources they're not text, and `l` is `→`.
#[test]
fn typing_edits_the_title_only_on_the_title() {
    let mut setup = screen();
    press(&mut setup, KeyCode::Char('x'));
    assert_eq!(setup.chosen().title, "9 Oct, 14:05");
    press(&mut setup, KeyCode::Tab);
    for c in " lab".chars() {
        press(&mut setup, KeyCode::Char(c));
    }
    assert_eq!(setup.chosen().title, "9 Oct, 14:05 lab");
    press(&mut setup, KeyCode::Backspace);
    assert_eq!(setup.chosen().title, "9 Oct, 14:05 la");
    assert_eq!(setup.chosen().system, Input::Default);
}

/// A title cleared to nothing (or spaces) starts the recording under the
/// pre-filled one.
#[test]
fn a_cleared_title_is_the_prefilled_one() {
    let mut setup = screen();
    press(&mut setup, KeyCode::Tab);
    for _ in 0..20 {
        press(&mut setup, KeyCode::Backspace);
    }
    press(&mut setup, KeyCode::Char(' '));
    assert_eq!(setup.chosen().title, "9 Oct, 14:05");
}

/// A pasted title is one line, and only while the title has the cursor.
#[test]
fn a_paste_is_one_line_and_lands_on_the_title_only() {
    let mut setup = screen();
    setup.paste("ignored");
    assert_eq!(setup.chosen().title, "9 Oct, 14:05");
    press(&mut setup, KeyCode::Tab);
    setup.paste("\u{202e}x\ny\tz");
    assert_eq!(setup.chosen().title, "9 Oct, 14:05x y z");
}

/// The title stops growing at its limit.
#[test]
fn the_title_has_a_limit() {
    let mut setup = screen();
    press(&mut setup, KeyCode::Tab);
    setup.paste(&"a".repeat(500));
    assert_eq!(setup.chosen().title.chars().count(), MAX_TITLE_CHARS);
}

/// Each source is recorded when it's chosen or both are; the preview
/// listens to both whichever is chosen.
#[test]
fn every_source_is_previewed_whichever_is_chosen() {
    let mut setup = screen();
    assert!(Listen::System.records(Source::System));
    assert!(!Listen::System.records(Source::Microphone));
    assert!(Listen::Both.records(Source::Microphone));
    press(&mut setup, KeyCode::Down);
    assert_eq!(setup.tracks(), 1);
    assert_eq!(
        setup.inputs(),
        [
            (Source::System, Input::Default),
            (Source::Microphone, Input::Default)
        ]
    );
    press(&mut setup, KeyCode::Down);
    assert_eq!(setup.tracks(), 2);
}

/// A meter shows the latest eight levels, oldest first, and a failed
/// stream shows no bars until a level arrives.
#[test]
fn a_meter_shows_the_latest_levels() {
    let mut setup = screen();
    for peak in [100, 200, 300, 400, 500, 600, 700, 800, 900] {
        setup.set_level(Source::System, Level::from_peak(peak));
    }
    let peaks: Vec<_> = setup.meters[0]
        .bars
        .iter()
        .map(|level| level.map(Level::peak))
        .collect();
    assert_eq!(peaks, [200, 300, 400, 500, 600, 700, 800, 900].map(Some));
    setup.set_failed(Source::System);
    assert!(setup.meters[0].failed);
    setup.set_level(Source::System, Level::SILENT);
    assert!(!setup.meters[0].failed);
}

/// The space line shows under four hours, and not at or over it.
#[test]
fn the_space_line_shows_only_under_four_hours() {
    let mut setup = screen();
    let hours = |h: u64| Some(Duration::from_secs(h * 3_600));
    assert!(setup.space_line().is_none());
    setup.set_space(hours(4));
    assert!(setup.space_line().is_none());
    setup.set_space(Some(Duration::from_mins(4 * 60 - 1)));
    assert!(setup.space_line().is_some());
    setup.set_space(None);
    assert!(setup.space_line().is_none());
}
