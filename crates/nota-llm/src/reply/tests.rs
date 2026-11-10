use serde_json::{Value, json};

use super::*;

/// A real reply from llama-server (build d2e5458, Qwen3.5-4B `Q4_K_M`) to
/// two invented candidates: "amylo diping" for amlodipine (yes), and
/// "clinician" for clonidine (no).
const LLAMA_REPLY: &[u8] = include_bytes!("llama_reply.json");

/// A finished completion whose message text is `content`.
fn completion(content: &str) -> Vec<u8> {
    reply_with(&json!({"finish_reason": "stop", "message": {"content": content}}))
}

/// A reply whose only choice is `choice`.
fn reply_with(choice: &Value) -> Vec<u8> {
    json!({ "choices": [choice] }).to_string().into_bytes()
}

/// The real reply gives the model's answers, with how likely a yes was
/// from its token probabilities.
#[test]
fn a_real_reply_gives_answers_and_probabilities() {
    let decisions = parse(LLAMA_REPLY, 2).unwrap();
    let answers: Vec<Answer> = decisions.iter().map(|d| d.answer).collect();
    assert_eq!(answers, [Answer::Yes, Answer::No]);
    let yes: Vec<f64> = decisions.iter().map(|d| d.yes.unwrap().get()).collect();
    // "true" at -0.0004 against "false" at -7.957: all but certain.
    assert!(yes[0] > 0.999, "{yes:?}");
    // "false" at -0.365 against "true" at -1.186: exp(-1.186) over both.
    assert!((yes[1] - 0.3056).abs() < 1e-3, "{yes:?}");
}

/// The same reply read as an answer to a question of another size is
/// refused: it names candidate 2 of 1, or leaves out candidate 3 of 3.
#[test]
fn a_reply_for_another_question_is_refused() {
    assert!(matches!(
        parse(LLAMA_REPLY, 1),
        Err(ReplyError::UnknownCandidate(2))
    ));
    assert!(matches!(parse(LLAMA_REPLY, 3), Err(ReplyError::Missing(3))));
}

/// A body over the limit is refused before it's read.
#[test]
fn a_body_over_the_limit_is_refused() {
    let body = vec![b' '; MAX_REPLY_LEN + 1];
    assert!(matches!(parse(&body, 1), Err(ReplyError::TooLong(n)) if n == MAX_REPLY_LEN + 1));
}

/// A body that isn't JSON, or a message text that isn't, is refused.
#[test]
fn text_that_isnt_json_is_refused() {
    assert!(matches!(
        parse(b"<html>busy</html>", 1),
        Err(ReplyError::NotJson(_))
    ));
    assert!(matches!(
        parse(&completion("Yes, apply it."), 1),
        Err(ReplyError::NotJson(_))
    ));
}

/// A reply cut off at its token limit is refused as cut short, even if
/// what it holds would read.
#[test]
fn a_reply_cut_short_is_refused() {
    let choice = json!({
        "finish_reason": "length",
        "message": {"content": r#"{"decisions":[{"id":1,"apply":true}]}"#}
    });
    assert!(matches!(
        parse(&reply_with(&choice), 1),
        Err(ReplyError::CutShort)
    ));
}

/// A body that isn't a finished completion with text says what it lacks.
#[test]
fn a_body_that_isnt_a_finished_completion_is_refused() {
    let fine = r#"{"decisions":[{"id":1,"apply":true}]}"#;
    let cases = [
        (json!({"error": {"message": "busy"}}), "no choices"),
        (json!({"choices": []}), "no choices"),
        (
            json!({"choices": [{"message": {"content": fine}}]}),
            "it didn't stop",
        ),
        (
            json!({"choices": [{"finish_reason": "tool_calls", "message": {"content": fine}}]}),
            "it didn't stop",
        ),
        (
            json!({"choices": [{"finish_reason": "stop", "message": {}}]}),
            "no message text",
        ),
        (
            json!({"choices": [{"finish_reason": "stop", "message": {"content": 7}}]}),
            "no message text",
        ),
    ];
    for (body, lacks) in cases {
        let refused = parse(body.to_string().as_bytes(), 1);
        assert!(
            matches!(refused, Err(ReplyError::NotACompletion(what)) if what == lacks),
            "{body}: {refused:?}"
        );
    }
}

/// Message text in any other shape than one entry of a number and a yes
/// or no per candidate is refused, never half read.
#[test]
fn text_of_another_shape_is_refused() {
    let cases = [
        r#"[{"id":1,"apply":true}]"#,
        r#"{"decisions":[{"id":1,"apply":true}],"note":"sure"}"#,
        r#"{"decisions":{"id":1,"apply":true}}"#,
        r#"{"decisions":[{"id":1}]}"#,
        r#"{"decisions":[{"id":1,"apply":true,"why":"sounds alike"}]}"#,
        r#"{"decisions":[{"id":"1","apply":true}]}"#,
        r#"{"decisions":[{"id":1.0,"apply":true}]}"#,
        r#"{"decisions":[{"id":1,"apply":"yes"}]}"#,
        r#"{"decisions":[{"id":1,"apply":1}]}"#,
        r#"{"decisions":[7]}"#,
    ];
    for content in cases {
        let refused = parse(&completion(content), 1);
        assert!(
            matches!(refused, Err(ReplyError::NotDecisions(_))),
            "{content}: {refused:?}"
        );
    }
}

/// A number the question doesn't have is refused: zero, negative, past
/// the end, or too big for any integer type.
#[test]
fn an_unknown_candidate_is_refused() {
    for (id, named) in [
        ("0", 0),
        ("-1", -1),
        ("3", 3),
        ("18446744073709551615", i128::from(u64::MAX)),
    ] {
        let content =
            format!(r#"{{"decisions":[{{"id":{id},"apply":true}},{{"id":1,"apply":true}}]}}"#);
        let refused = parse(&completion(&content), 2);
        assert!(
            matches!(refused, Err(ReplyError::UnknownCandidate(n)) if n == named),
            "{id}: {refused:?}"
        );
    }
}

/// A candidate given twice is refused, even when both say the same.
#[test]
fn a_candidate_given_twice_is_refused() {
    let content = r#"{"decisions":[{"id":1,"apply":false},{"id":1,"apply":false}]}"#;
    assert!(matches!(
        parse(&completion(content), 2),
        Err(ReplyError::Repeated(1))
    ));
}

/// A key given twice in one object is refused, wherever it is: the last
/// one would otherwise win, turning a no into a yes.
#[test]
fn a_key_given_twice_is_refused() {
    let twice = |refused: Result<Vec<Decision>, ReplyError>| match refused {
        Err(ReplyError::NotJson(e)) => e.to_string().contains("is given twice"),
        _ => false,
    };
    let apply = r#"{"decisions":[{"id":1,"apply":false,"apply":true}]}"#;
    assert!(twice(parse(&completion(apply), 1)));
    let decisions = r#"{"decisions":[{"id":1,"apply":false}],"decisions":[{"id":1,"apply":true}]}"#;
    assert!(twice(parse(&completion(decisions), 1)));
    let one = r#"{\"decisions\":[{\"id\":1,\"apply\":true}]}"#;
    let body = format!(
        r#"{{"choices":[{{"finish_reason":"length","finish_reason":"stop","message":{{"content":"{one}"}}}}]}}"#
    );
    assert!(twice(parse(body.as_bytes(), 1)));
    // The same keys once each read as before.
    let plain =
        format!(r#"{{"choices":[{{"finish_reason":"stop","message":{{"content":"{one}"}}}}]}}"#);
    let decisions = parse(plain.as_bytes(), 1).unwrap();
    assert_eq!(decisions[0].answer, Answer::Yes);
}

/// An empty list leaves out the first candidate.
#[test]
fn no_decisions_leaves_out_the_first() {
    let refused = parse(&completion(r#"{"decisions":[]}"#), 2);
    assert!(
        matches!(refused, Err(ReplyError::Missing(1))),
        "{refused:?}"
    );
}

/// Token probabilities that don't line up with the answers leave every
/// probability out, and the answers stand.
#[test]
fn probabilities_that_dont_line_up_are_left_out() {
    let content = r#"{"decisions":[{"id":1,"apply":true},{"id":2,"apply":false}]}"#;
    let token = |t: &str| json!({"token": t, "logprob": -0.1, "top_logprobs": [{"token": t, "logprob": -0.1}]});
    let cases = [
        // One boolean token for two answers.
        json!({"content": [token("true")]}),
        // The tokens say the opposite of the text.
        json!({"content": [token("false"), token("false")]}),
        // Not a list of tokens.
        json!({"content": "true false"}),
        json!(null),
    ];
    for logprobs in cases {
        let choice =
            json!({"finish_reason": "stop", "message": {"content": content}, "logprobs": logprobs});
        let decisions = parse(&reply_with(&choice), 2).unwrap();
        let expected = [
            Decision {
                answer: Answer::Yes,
                yes: None,
            },
            Decision {
                answer: Answer::No,
                yes: None,
            },
        ];
        assert_eq!(decisions, expected, "{logprobs}");
    }
}

/// A token whose alternatives hold neither `true` nor `false`, or whose
/// weight isn't a number, has no probability; the others keep theirs.
#[test]
fn a_token_without_weights_has_no_probability() {
    let content = r#"{"decisions":[{"id":1,"apply":true},{"id":2,"apply":true}]}"#;
    let logprobs = json!({"content": [
        {"token": "true", "logprob": -0.1, "top_logprobs": [
            {"token": "true", "logprob": -0.1}, {"token": "false", "logprob": -2.4}
        ]},
        {"token": "true", "logprob": -0.1, "top_logprobs": [{"token": "maybe", "logprob": -0.1}]}
    ]});
    let choice =
        json!({"finish_reason": "stop", "message": {"content": content}, "logprobs": logprobs});
    let decisions = parse(&reply_with(&choice), 2).unwrap();
    let first = decisions[0].yes.unwrap().get();
    let expected = (-0.1_f64).exp() / ((-0.1_f64).exp() + (-2.4_f64).exp());
    assert!((first - expected).abs() < 1e-12, "{first}");
    assert_eq!(decisions[1].yes, None);

    let broken = json!({"content": [
        {"token": "true", "top_logprobs": [{"token": "true", "logprob": "high"}]}
    ]});
    let content = r#"{"decisions":[{"id":1,"apply":true}]}"#;
    let choice =
        json!({"finish_reason": "stop", "message": {"content": content}, "logprobs": broken});
    assert_eq!(parse(&reply_with(&choice), 1).unwrap()[0].yes, None);
}

/// Errors say what's wrong in plain words, and a JSON error keeps its
/// cause.
#[test]
fn errors_say_whats_wrong() {
    assert_eq!(
        ReplyError::Missing(3).to_string(),
        "the answer leaves out candidate 3"
    );
    assert_eq!(
        ReplyError::Repeated(2).to_string(),
        "the answer gives candidate 2 twice"
    );
    assert_eq!(
        ReplyError::UnknownCandidate(9).to_string(),
        "the answer names candidate 9, which wasn't asked about"
    );
    assert_eq!(
        ReplyError::CutShort.to_string(),
        "the reply was cut short at its token limit"
    );
    let Err(not_json) = parse(b"{", 1) else {
        panic!("read a lone brace")
    };
    assert!(not_json.source().is_some());
    assert!(ReplyError::CutShort.source().is_none());
}

/// A body as long as the limit is read, not refused as too long.
#[test]
fn a_body_at_the_limit_is_read() {
    let body = vec![b' '; MAX_REPLY_LEN];
    assert!(matches!(parse(&body, 1), Err(ReplyError::NotJson(_))));
}

/// A weight too large to add up gives no probability, not a false 0.
#[test]
fn weights_too_large_to_add_give_no_probability() {
    let token = json!({"token": "false", "logprob": -0.1, "top_logprobs": [
        {"token": "true", "logprob": -1.0},
        {"token": "false", "logprob": 1000.0}
    ]});
    let choice = json!({
        "finish_reason": "stop",
        "message": {"content": r#"{"decisions":[{"id":1,"apply":false}]}"#},
        "logprobs": {"content": [token]}
    });
    let decisions = parse(&reply_with(&choice), 1).unwrap();
    assert_eq!(
        decisions[0],
        Decision {
            answer: Answer::No,
            yes: None
        }
    );
}
