use std::os::unix::process::ExitStatusExt;

use serde_json::json;

use super::*;

/// What a CLI that exited with `code` left on its stdout.
fn ran(code: i32, stdout: &str) -> Ran {
    Ran {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
    }
}

/// Claude's result without a structured answer, or that isn't a result,
/// is an answer that can't be used.
#[test]
fn a_claude_result_without_an_answer_cant_be_used() {
    let cases = [
        (
            r#"{"type":"result","is_error":false}"#,
            "there's no structured answer",
        ),
        (
            r#"{"type":"assistant","is_error":false}"#,
            "it isn't a result",
        ),
    ];
    for (stdout, what) in cases {
        let Err(CliError::Unreadable(Cli::Claude, ReplyError::NotACompletion(said))) =
            claude_answer(&ran(0, stdout), 1)
        else {
            panic!("{stdout}")
        };
        assert_eq!(said, what);
    }
}

/// Claude's error result, or one that doesn't say whether it failed, is
/// an error, with its status only if it's a status.
#[test]
fn a_claude_error_keeps_only_its_status() {
    let cases = [
        (
            r#"{"type":"result","is_error":true,"api_error_status":401}"#,
            Some(401),
        ),
        (
            r#"{"type":"result","is_error":true,"api_error_status":"x"}"#,
            None,
        ),
        (
            r#"{"type":"result","is_error":true,"api_error_status":70000}"#,
            None,
        ),
    ];
    for (stdout, status) in cases {
        let Err(CliError::Refused(Cli::Claude, got)) = claude_answer(&ran(1, stdout), 1) else {
            panic!("{stdout}")
        };
        assert_eq!(got, status, "{stdout}");
    }
}

/// A Claude that exits with an error and leaves no result failed; one
/// that exits cleanly with no result gave an answer that can't be used.
#[test]
fn claude_without_a_result_is_told_by_its_status() {
    for stdout in ["", "not json", r#"{"type":"other"}"#] {
        assert!(
            matches!(claude_answer(&ran(1, stdout), 1), Err(CliError::Exited(Cli::Claude, s)) if s.code() == Some(1)),
            "{stdout}"
        );
    }
    assert!(matches!(
        claude_answer(&ran(0, "not json"), 1),
        Err(CliError::Unreadable(Cli::Claude, ReplyError::NotJson(_)))
    ));
}

/// Codex's answer is read with the space around it trimmed; an answer
/// that isn't text can't be used.
#[test]
fn codex_answer_is_read_trimmed() {
    let answer = "\n {\"decisions\":[{\"id\":1,\"apply\":true}]}\n";
    let decisions = codex_answer(&ran(0, answer), 1).unwrap();
    assert_eq!(
        decisions.iter().map(|d| d.answer).collect::<Vec<_>>(),
        [nota_core::decide::Answer::Yes]
    );
    let not_text = Ran {
        status: ExitStatus::from_raw(0),
        stdout: vec![0xff, 0xfe],
    };
    assert!(matches!(
        codex_answer(&not_text, 1),
        Err(CliError::Unreadable(
            Cli::Codex,
            ReplyError::NotACompletion("the answer isn't text")
        ))
    ));
}

/// Only an answer that can't be used is a failed decision; every other
/// error makes the decider unavailable.
#[test]
fn only_an_unusable_answer_is_a_failed_decision() {
    let failed = [
        CliError::Unreadable(Cli::Codex, ReplyError::CutShort),
        CliError::TooLong(Cli::Codex),
    ];
    for e in failed {
        assert!(matches!(e.into_decide(), DecideError::Failed(_)));
    }
    let unavailable = [
        CliError::Missing(Cli::Codex, PathBuf::from("codex")),
        CliError::NoAnswer(Cli::Codex, Duration::from_secs(1)),
        CliError::Exited(Cli::Codex, ExitStatus::from_raw(256)),
        CliError::Refused(Cli::Claude, None),
        CliError::Io(Cli::Codex, io::Error::other("x")),
    ];
    for e in unavailable {
        assert!(matches!(e.into_decide(), DecideError::Unavailable(_)));
    }
}

/// A schema file is named by its whole path, though its folder was given
/// relative: Codex runs in that folder and would look for it under itself.
#[test]
fn a_schema_file_is_named_by_its_whole_path() {
    // The workspace's build folder, from the crate's, where tests run.
    let file = SchemaFile::write(Path::new("../../target"), &json!({})).unwrap();
    assert!(file.0.is_absolute(), "{}", file.0.display());
    assert!(file.0.exists(), "{}", file.0.display());
    let path = file.0.clone();
    drop(file);
    assert!(!path.exists(), "{} was left", path.display());
}

/// Claude's result that doesn't say whether it failed, or that gave up on
/// the answer's shape, is an answer that can't be used: a failed decision,
/// not a CLI that can't run.
#[test]
fn a_claude_result_unsure_or_out_of_shape_cant_be_used() {
    let cases = [
        (
            r#"{"type":"result","structured_output":{}}"#,
            ReplyError::NotACompletion("it doesn't say whether it failed"),
        ),
        (
            r#"{"type":"result","is_error":"false","structured_output":{}}"#,
            ReplyError::NotACompletion("it doesn't say whether it failed"),
        ),
        (
            r#"{"type":"result","is_error":true,"subtype":"error_max_structured_output_retries"}"#,
            ReplyError::NotDecisions("it gave up on the answer's shape"),
        ),
    ];
    for (stdout, expected) in cases {
        let Err(CliError::Unreadable(Cli::Claude, got)) = claude_answer(&ran(0, stdout), 1) else {
            panic!("{stdout}")
        };
        assert_eq!(got.to_string(), expected.to_string(), "{stdout}");
    }
}
