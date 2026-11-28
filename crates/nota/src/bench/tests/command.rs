//! `nota bench`'s command line, and its logs' stamp.

use std::ffi::OsString;

use nota_core::{SessionId, WallTime};

use super::*;
use crate::bench::command::{RunArgs, Source, stamp};
#[cfg(unix)]
use crate::bench::command::{engines, real::work_dir};
#[cfg(unix)]
use crate::bench::dispatch;
use crate::bench::{BenchCommand, parse};

/// `list` as arguments.
fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

/// A run over a session takes its engines in order, the session in the
/// library under `--data`, the pre-check and the log folder as given.
#[test]
fn a_session_run_takes_every_flag_given() {
    let given = args(&[
        "--engines",
        "codex,local,claude",
        "--session",
        "3",
        "--data",
        "/data",
        "--precheck",
        "on",
        "--log",
        "/logs",
    ]);
    assert_eq!(
        parse(&given),
        Ok(BenchCommand::Run(RunArgs {
            engines: vec![EngineName::Codex, EngineName::Local, EngineName::Claude],
            source: Source::Session {
                data: PathBuf::from("/data"),
                session: SessionId::new(3),
            },
            precheck: Precheck::On,
            log: PathBuf::from("/logs"),
        }))
    );
}

/// Without `--precheck` C3 is off, and without `--log` the logs go in
/// the data directory's `bench` folder.
#[test]
fn a_run_defaults_to_no_pre_check_and_logs_in_the_data_directory() {
    let given = args(&[
        "--engines",
        "qwen",
        "--fixture",
        "f.json",
        "--data",
        "/data",
    ]);
    assert_eq!(
        parse(&given),
        Ok(BenchCommand::Run(RunArgs {
            engines: vec![EngineName::Local],
            source: Source::Fixture(PathBuf::from("f.json")),
            precheck: Precheck::Off,
            log: PathBuf::from("/data/bench"),
        }))
    );
}

/// `--precheck off` turns C3 off by name, as its default leaves it.
#[test]
fn the_pre_check_can_be_turned_off_by_name() {
    let given = args(&[
        "--engines",
        "local",
        "--fixture",
        "f.json",
        "--precheck",
        "off",
        "--log",
        "/logs",
    ]);
    assert_eq!(
        parse(&given),
        Ok(BenchCommand::Run(RunArgs {
            engines: vec![EngineName::Local],
            source: Source::Fixture(PathBuf::from("f.json")),
            precheck: Precheck::Off,
            log: PathBuf::from("/logs"),
        }))
    );
}

/// The CLI engines `--engines` names are built in the order given, each
/// recorded as the program that answers. Building one starts nothing: the
/// CLI runs only when it's asked.
#[cfg(unix)]
#[test]
fn the_cli_engines_are_built_in_the_order_given() {
    let clock: Arc<dyn Clock> = Arc::new(clock());
    // The local engine is left out: it reads the user's real settings.
    let built = engines(&[EngineName::Codex, EngineName::Claude], &clock).unwrap();
    let names: Vec<_> = built
        .iter()
        .map(|engine| (engine.name.as_str(), engine.by.name.as_str()))
        .collect();
    assert_eq!(names, [("codex", "codex exec"), ("claude", "claude -p")]);
}

/// The CLIs run in nota's own `bench-cli` folder, not wherever the bench
/// was started.
#[cfg(unix)]
#[test]
fn the_clis_run_in_nota_s_bench_cli_folder() {
    let dir = work_dir().unwrap();
    assert!(
        dir.is_absolute() && dir.ends_with("nota/bench-cli"),
        "{}",
        dir.display()
    );
}

/// A bench run over `fixture`'s candidates with the codex engine, logging
/// in `log`: what it printed, or why it failed.
#[cfg(unix)]
fn bench_run(fixture: &Path, log: &Path) -> Result<String, String> {
    let run = BenchCommand::Run(RunArgs {
        engines: vec![EngineName::Codex],
        source: Source::Fixture(fixture.to_path_buf()),
        precheck: Precheck::Off,
        log: log.to_path_buf(),
    });
    let mut out = Vec::new();
    dispatch(&run, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

/// A bench run that gets through writes each engine's log, prints where,
/// and succeeds. The fixture offers nothing, so the CLI is never started.
#[cfg(unix)]
#[test]
fn a_bench_run_that_gets_through_logs_each_engine_and_succeeds() {
    let dir = TestDir::new("bench-run");
    let fixture = dir.0.join("fixture.json");
    write_file(&fixture, "{\"terms\": [], \"chunks\": []}");
    let log = dir.0.join("logs");

    let out = bench_run(&fixture, &log).unwrap();

    let logs: Vec<PathBuf> = std::fs::read_dir(&log)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(
        out.ends_with(&format!("Logged codex: {}\n", logs[0].display())),
        "{out}"
    );
}

/// A bench run whose log folder can't be made fails, saying why.
#[cfg(unix)]
#[test]
fn a_bench_run_whose_log_folder_cant_be_made_fails() {
    let dir = TestDir::new("bench-no-logs");
    let fixture = dir.0.join("fixture.json");
    write_file(&fixture, "{\"terms\": [], \"chunks\": []}");
    // A file stands where the log folder's parent should be.
    let plain = dir.0.join("plain");
    write_file(&plain, "");
    let log = plain.join("logs");

    let why = bench_run(&fixture, &log).unwrap_err();

    assert!(
        why.starts_with(&format!("the log folder {} can't be made: ", log.display())),
        "{why}"
    );
}

/// `compare` takes the old log, then the new.
#[test]
fn compare_takes_the_old_log_then_the_new() {
    assert_eq!(
        parse(&args(&["compare", "a.jsonl", "b.jsonl"])),
        Ok(BenchCommand::Compare {
            old: PathBuf::from("a.jsonl"),
            new: PathBuf::from("b.jsonl"),
        })
    );
}

/// Each command line the bench can't run is refused with a message
/// saying why.
#[test]
fn a_bad_command_line_is_refused_with_its_reason() {
    let cases: [(&[&str], &str); 10] = [
        (
            &["--engines", "gemini", "--fixture", "f"],
            "unknown engine \"gemini\": --engines takes local, claude or codex",
        ),
        (
            &["--engines", "local,claude,local", "--fixture", "f"],
            "--engines names local twice",
        ),
        // Two names for one engine are still one engine twice.
        (
            &["--engines", "local,qwen", "--fixture", "f"],
            "--engines names local twice",
        ),
        (&["--fixture", "f"], "--engines is required"),
        (
            &[
                "--engines",
                "local",
                "--session",
                "1",
                "--fixture",
                "f",
                "--data",
                "/d",
            ],
            "--session and --fixture don't go together",
        ),
        (
            &["--engines", "local", "--data", "/d"],
            "bench needs --session or --fixture",
        ),
        (
            &["--engines", "local", "--session", "three", "--data", "/d"],
            "--session takes a session's number",
        ),
        (
            &["--engines", "local", "--fixture", "f", "--precheck", "yes"],
            "--precheck takes on or off",
        ),
        (
            &["compare", "a.jsonl"],
            "bench compare takes two logs: the old and the new",
        ),
        (
            &["compare", "a.jsonl", "b.jsonl", "c.jsonl"],
            "bench compare takes two logs: the old and the new",
        ),
    ];
    for (given, message) in cases {
        assert_eq!(parse(&args(given)), Err(message.to_owned()), "{given:?}");
    }
}

/// A log's stamp is the bench's start in UTC, to the second.
#[test]
fn a_logs_stamp_is_the_start_in_utc() {
    let at = WallTime::from_unix_seconds(1_762_765_200).unwrap();
    assert_eq!(stamp(at).as_deref(), Some("20251110-090000"));
}
