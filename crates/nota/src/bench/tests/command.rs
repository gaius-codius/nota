//! `nota bench`'s command line, and its logs' stamp.

use std::ffi::OsString;

use nota_core::{SessionId, WallTime};

use super::*;
use crate::bench::command::{RunArgs, Source, stamp};
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
