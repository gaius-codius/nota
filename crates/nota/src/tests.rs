use super::*;

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn engine(list: &[&str]) -> Result<ModelPaths, String> {
    match parse(&args(list))? {
        Command::EngineAsr(paths) => Ok(paths),
        Command::Record(_) => Err("record".into()),
    }
}

fn record(list: &[&str]) -> Result<RecordArgs, String> {
    match parse(&args(list))? {
        Command::Record(args) => Ok(args),
        Command::EngineAsr(_) => Err("engine".into()),
    }
}

#[test]
fn parses_engine_asr() {
    let paths = engine(&[
        "engine",
        "asr",
        "--vad",
        "v.onnx",
        "--parakeet",
        "p",
        "--threads",
        "2",
    ])
    .unwrap();
    assert_eq!(paths.parakeet_dir, PathBuf::from("p"));
    assert_eq!(paths.vad_model, PathBuf::from("v.onnx"));
    assert_eq!(paths.threads, 2);
    let default = engine(&["engine", "asr", "--vad", "v", "--parakeet", "p"]).unwrap();
    assert_eq!(default.threads, 4);
}

#[test]
fn refuses_bad_arguments() {
    for bad in [
        &[][..],
        &["engine"],
        &["engine", "tts"],
        &["engine", "asr"],
        &["engine", "asr", "--vad", "v"],
        &["engine", "asr", "--vad", "v", "--parakeet"],
        &[
            "engine",
            "asr",
            "--vad",
            "v",
            "--parakeet",
            "p",
            "--threads",
            "0",
        ],
        &[
            "engine",
            "asr",
            "--vad",
            "v",
            "--parakeet",
            "p",
            "--loud",
            "1",
        ],
        &["record", "--loud", "1"],
        &["record", "--title"],
        &["record", "--parakeet", "p"],
        &["record", "--vad", "v"],
        &["recorder"],
    ] {
        assert!(parse(&args(bad)).is_err(), "{bad:?}");
    }
}

#[test]
fn parses_record() {
    let r = record(&[
        "record",
        "--title",
        "Pharmacy workshop",
        "--data",
        "/d",
        "--parakeet",
        "p",
        "--vad",
        "v",
        "--mic",
        "usb-mic",
        "--system",
        "speakers.monitor",
    ])
    .unwrap();
    assert_eq!(r.title, "Pharmacy workshop");
    assert_eq!(r.data, PathBuf::from("/d"));
    assert_eq!(r.models, Some((PathBuf::from("p"), PathBuf::from("v"))));
    assert_eq!(r.mic, Source::Device("usb-mic".into()));
    assert_eq!(r.system, Source::Device("speakers.monitor".into()));
    assert!(!r.tone);
}

#[test]
fn record_defaults_to_the_default_devices_and_no_engine() {
    let r = record(&["record", "--data", "/d"]).unwrap();
    assert_eq!(r.title, "Recording");
    assert_eq!(r.models, None);
    assert_eq!(r.mic, Source::Microphone);
    assert_eq!(r.system, Source::SystemAudio);
}

#[test]
fn a_relative_data_directory_is_made_absolute() {
    // The library makes missing directories by walking up to their parents,
    // which a one-part relative path (`notes`, `.`) has none of.
    for given in ["notes", ".", "a/b"] {
        let r = record(&["record", "--data", given]).unwrap();
        assert!(r.data.is_absolute(), "{given}: {}", r.data.display());
        assert!(
            r.data.ends_with(given.trim_start_matches('.')),
            "{}",
            r.data.display()
        );
    }
}
