use super::*;

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn engine(list: &[&str]) -> Result<ModelPaths, String> {
    engine_cutting(list).map(|(paths, _)| paths)
}

fn engine_cutting(list: &[&str]) -> Result<(ModelPaths, ChunkerConfig), String> {
    match parse(&args(list))? {
        Command::EngineAsr(paths, chunking) => Ok((paths, chunking)),
        Command::Record(_) | Command::App(_) | Command::EngineFake(_) => Err("record".into()),
    }
}

fn record(list: &[&str]) -> Result<RecordArgs, String> {
    match parse(&args(list))? {
        Command::Record(args) => Ok(args),
        Command::EngineAsr(..) | Command::App(_) | Command::EngineFake(_) => Err("engine".into()),
    }
}

fn app(list: &[&str]) -> Result<RecordArgs, String> {
    match parse(&args(list))? {
        Command::App(args) => Ok(args),
        Command::EngineAsr(..) | Command::Record(_) | Command::EngineFake(_) => {
            Err("not the app".into())
        }
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
fn the_engine_cuts_for_the_live_pass_unless_told_the_final_one() {
    let base = ["engine", "asr", "--vad", "v", "--parakeet", "p"];
    let cut = |pass: &[&str]| engine_cutting(&[&base[..], pass].concat()).map(|(_, c)| c);
    let live = ChunkerConfig::live(SampleRate::SPEECH);
    let final_pass = ChunkerConfig::final_pass(SampleRate::SPEECH);
    assert_eq!(cut(&[]), Ok(live));
    assert_eq!(cut(&["--pass", "live"]), Ok(live));
    assert_eq!(cut(&["--pass", "final"]), Ok(final_pass));
    assert!(cut(&["--pass", "fast"]).is_err());
    let fake = |list: &[&str]| match parse(&args(list)) {
        Ok(Command::EngineFake(chunking)) => Ok(chunking),
        Ok(_) => Err("not the fake".to_owned()),
        Err(e) => Err(e),
    };
    assert_eq!(fake(&["engine", "fake"]), Ok(live));
    assert_eq!(fake(&["engine", "fake", "--pass", "final"]), Ok(final_pass));
    assert!(fake(&["engine", "fake", "--threads", "2"]).is_err());
}

#[test]
fn the_fake_engine_runs_only_when_asked() {
    assert!(!record(&["record", "--data", "/d"]).unwrap().fake_engine);
    assert!(
        record(&["record", "--data", "/d", "--fake-engine", "yes"])
            .unwrap()
            .fake_engine
    );
    assert!(
        app(&["--data", "/d", "--fake-engine", "yes"])
            .unwrap()
            .fake_engine
    );
}

#[test]
fn refuses_bad_arguments() {
    for bad in [
        &["engine"][..],
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
        &["--loud", "1"],
        &["--data"],
        // Setup's choices and the latency log are `nota record`'s.
        &["--title", "t"],
        &["--mic", "m"],
        &["--system", "s"],
        &["--data", "/d", "--latency-log", "l"],
        &["--parakeet", "p"],
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
    assert_eq!(
        r.start,
        recorder::Command::Start(Setup {
            title: "Pharmacy workshop".into(),
            mic: Some(Input::Device("usb-mic".into())),
            system: Some(Input::Device("speakers.monitor".into())),
        })
    );
    assert_eq!(r.data, PathBuf::from("/d"));
    assert_eq!(r.models, Some((PathBuf::from("p"), PathBuf::from("v"))));
    assert!(!r.tone);
}

#[test]
fn record_logs_latency_only_when_asked() {
    let r = record(&["record", "--data", "/d", "--latency-log", "/tmp/l.tsv"]).unwrap();
    assert_eq!(r.latency_log, Some(PathBuf::from("/tmp/l.tsv")));
    assert_eq!(
        record(&["record", "--data", "/d"]).unwrap().latency_log,
        None
    );
}

#[test]
fn record_defaults_to_the_default_devices_and_no_engine() {
    let r = record(&["record", "--data", "/d"]).unwrap();
    assert_eq!(
        r.start,
        recorder::Command::Start(Setup {
            title: "Recording".into(),
            mic: Some(Input::Default),
            system: Some(Input::Default),
        })
    );
    assert_eq!(r.models, None);
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

#[test]
fn no_command_opens_home() {
    let home = app(&[]).unwrap();
    assert!(home.data.is_absolute());
    assert_eq!(home.models, None);
    let home = app(&["--data", "/d", "--parakeet", "p", "--vad", "v"]).unwrap();
    assert_eq!(home.data, PathBuf::from("/d"));
    assert_eq!(home.models, Some((PathBuf::from("p"), PathBuf::from("v"))));
    assert_eq!(home.latency_log, None);
}
