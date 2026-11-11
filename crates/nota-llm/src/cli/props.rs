//! Property tests for reading what a CLI wrote. The contract:
//! - any bytes, with any exit status, give decisions or an error, never a
//!   panic
//! - decisions, when given, number exactly the question's candidates
//! - a well-formed answer gives back exactly the answers in it, from
//!   Claude's result or Codex's bare answer alike

use std::os::unix::process::ExitStatusExt;

use nota_core::decide::{Answer, MAX_CANDIDATES};
use proptest::prelude::*;
use serde_json::json;

use super::*;

/// Any answers for one to [`MAX_CANDIDATES`] candidates.
fn any_answers() -> impl Strategy<Value = Vec<Answer>> {
    prop::collection::vec(
        prop_oneof![Just(Answer::Yes), Just(Answer::No)],
        1..=MAX_CANDIDATES,
    )
}

/// What a CLI that exited with `code` wrote.
fn ran(code: u8, stdout: Vec<u8>) -> Ran {
    Ran {
        status: ExitStatus::from_raw(i32::from(code) << 8),
        stdout,
    }
}

/// The answer `{"decisions": [...]}` that gives `answers`.
fn answer(answers: &[Answer]) -> Value {
    let decisions: Vec<Value> = answers
        .iter()
        .enumerate()
        .map(|(i, a)| json!({"id": i + 1, "apply": *a == Answer::Yes}))
        .collect();
    json!({ "decisions": decisions })
}

proptest! {
    /// Any bytes are read or refused, never a panic, and decisions read
    /// number the candidates.
    #[test]
    fn any_output_is_read_or_refused(
        stdout in prop::collection::vec(any::<u8>(), 0..512),
        code in any::<u8>(),
        candidates in 1..=MAX_CANDIDATES,
    ) {
        let ran = ran(code, stdout);
        for read in [claude_answer(&ran, candidates), codex_answer(&ran, candidates)] {
            if let Ok(decisions) = read {
                prop_assert_eq!(decisions.len(), candidates);
            }
        }
    }

    /// A well-formed answer gives back exactly its answers, from either
    /// CLI.
    #[test]
    fn a_well_formed_answer_is_read_exactly(answers in any_answers()) {
        let answer = answer(&answers);
        let claude = json!({"type": "result", "is_error": false, "structured_output": answer});
        let from_claude = claude_answer(&ran(0, claude.to_string().into_bytes()), answers.len());
        let from_codex = codex_answer(&ran(0, answer.to_string().into_bytes()), answers.len());
        for read in [from_claude, from_codex] {
            let read: Vec<Answer> = read.unwrap().iter().map(|d| d.answer).collect();
            prop_assert_eq!(&read, &answers);
        }
    }
}
