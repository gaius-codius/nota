use nota_recorder::fs::fake::{FakeFs, Fault};
use proptest::prelude::*;

use super::*;

const DIR: &str = "/data";

fn dir() -> &'static Path {
    Path::new(DIR)
}

fn any_listen() -> impl Strategy<Value = Listen> {
    prop_oneof![
        Just(Listen::System),
        Just(Listen::Microphone),
        Just(Listen::Both)
    ]
}

fn any_input() -> impl Strategy<Value = Input> {
    prop_oneof![
        Just(Input::Default),
        any::<String>().prop_filter_map("an empty name is no device", |name| {
            (!name.is_empty()).then_some(Input::Device(name))
        }),
    ]
}

fn any_remembered() -> impl Strategy<Value = Remembered> {
    (any_listen(), any_input(), any_input()).prop_map(|(listen, system, microphone)| Remembered {
        listen,
        system,
        microphone,
    })
}

proptest! {
    #[test]
    fn what_is_written_reads_back_the_same(remembered in any_remembered()) {
        prop_assert_eq!(decode(encode(&remembered).as_bytes()), Ok(remembered));
    }

    #[test]
    fn any_bytes_parse_or_are_refused_without_a_panic(
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let _ = decode(&bytes);
        let mut with_header = format!("{HEADER}\n").into_bytes();
        with_header.extend(&bytes);
        let _ = decode(&with_header);
    }
}

/// The format reads as the module's docs show it, skipping a line this
/// version doesn't know.
#[test]
fn the_format_reads_as_documented() {
    let text = "nota setup 1\n\
                listen both\n\
                system default\n\
                microphone device alsa_input.usb-Seiren_Mini\n\
                later something a later version adds\n";
    assert_eq!(
        decode(text.as_bytes()),
        Ok(Remembered {
            listen: Listen::Both,
            system: Input::Default,
            microphone: Input::Device("alsa_input.usb-Seiren_Mini".to_owned()),
        })
    );
}

/// A line that's missing is the default: the system audio on the default
/// devices.
#[test]
fn a_missing_line_is_the_default() {
    assert_eq!(
        decode(b"nota setup 1\n"),
        Ok(Remembered {
            listen: Listen::System,
            system: Input::Default,
            microphone: Input::Default,
        })
    );
}

/// A file that's wrong in any way is refused, not half read.
#[test]
fn a_file_that_is_wrong_is_refused() {
    for text in [
        "",
        "nota setup 2\n",
        "nota setup 1\nlisten everything\n",
        "nota setup 1\nlisten both\nlisten both\n",
        "nota setup 1\nsystem device\n",
        "nota setup 1\nsystem device \\x\n",
        "nota setup 1\nmicrophone pinned mic\n",
    ] {
        assert!(decode(text.as_bytes()).is_err(), "{text:?}");
    }
}

/// A device named like the old display names stays a device: the choice
/// is kept, not a name to guess from.
#[test]
fn a_device_named_mic_is_not_the_default() {
    let remembered = Remembered {
        listen: Listen::Microphone,
        system: Input::Device("system audio".to_owned()),
        microphone: Input::Device("mic".to_owned()),
    };
    let fs = FakeFs::with_dirs([DIR]);
    write(&fs, dir(), &remembered).unwrap();
    assert_eq!(read(&fs, dir()).unwrap(), Some(remembered));
}

/// Nothing there is none, a newer write replaces an older, and a file
/// that can't be read is an error, not none.
#[test]
fn a_file_is_read_back_after_the_last_write() {
    let fs = FakeFs::with_dirs([DIR]);
    assert_eq!(read(&fs, dir()).unwrap(), None);
    let mut remembered = Remembered {
        listen: Listen::Both,
        system: Input::Default,
        microphone: Input::Default,
    };
    write(&fs, dir(), &remembered).unwrap();
    remembered.listen = Listen::System;
    write(&fs, dir(), &remembered).unwrap();
    assert_eq!(read(&fs, dir()).unwrap(), Some(remembered));
    fs.fail_on(
        &dir().join(FILE),
        Fault::Read,
        io::ErrorKind::PermissionDenied,
    );
    assert!(read(&fs, dir()).is_err());
}

/// A file longer than any nota writes is taken as none.
#[test]
fn a_file_longer_than_any_nota_writes_is_taken_as_none() {
    let fs = FakeFs::with_dirs([DIR]);
    let mut file = fs.create(&dir().join(FILE)).unwrap();
    let mut text = format!("{HEADER}\nsystem device ");
    text.push_str(&"a".repeat(MAX_LEN));
    file.write_all(text.as_bytes()).unwrap();
    drop(file);
    assert_eq!(read(&fs, dir()).unwrap(), None);
}

/// A source not listened to has no track.
#[test]
fn only_the_sources_listened_to_have_tracks() {
    let remembered = |listen| Remembered {
        listen,
        system: Input::Device("speakers".to_owned()),
        microphone: Input::Device("usb-mic".to_owned()),
    };
    let tracks = |listen| {
        let setup = remembered(listen).setup("T".to_owned());
        (setup.mic, setup.system)
    };
    let (mic, system) = (
        Some(Input::Device("usb-mic".to_owned())),
        Some(Input::Device("speakers".to_owned())),
    );
    assert_eq!(tracks(Listen::System), (None, system.clone()));
    assert_eq!(tracks(Listen::Microphone), (mic.clone(), None));
    assert_eq!(tracks(Listen::Both), (mic, system));
}
