//! Property tests for the reply parser. The contract:
//! - any bytes give decisions or an error, never a panic
//! - decisions, when given, number exactly the question's candidates
//! - a well-formed reply gives back exactly the answers in it, whatever
//!   order its entries come in
//! - a reply that repeats or leaves out a candidate is refused
//! - the token probabilities never change an answer, and any probability
//!   given is from 0 to 1

use nota_core::decide::{Answer, MAX_CANDIDATES};
use proptest::prelude::*;
use serde_json::{Value, json};

use super::*;

/// Any answers for one to [`MAX_CANDIDATES`] candidates.
fn any_answers() -> impl Strategy<Value = Vec<Answer>> {
    prop::collection::vec(
        prop_oneof![Just(Answer::Yes), Just(Answer::No)],
        1..=MAX_CANDIDATES,
    )
}

/// `answers` with their entries in a shuffled order: (candidate index,
/// answer) pairs.
fn any_shuffled_answers() -> impl Strategy<Value = Vec<(usize, Answer)>> {
    any_answers()
        .prop_map(|answers| answers.into_iter().enumerate().collect::<Vec<_>>())
        .prop_shuffle()
}

/// The message text giving `entries`, in their order.
fn text(entries: &[(usize, Answer)]) -> String {
    let decisions: Vec<Value> = entries
        .iter()
        .map(|&(index, answer)| json!({"id": index + 1, "apply": answer == Answer::Yes}))
        .collect();
    json!({ "decisions": decisions }).to_string()
}

/// A finished completion whose message is `content`, with `logprobs`.
fn completion(content: &str, logprobs: &Value) -> Vec<u8> {
    json!({"choices": [{
        "finish_reason": "stop",
        "message": {"role": "assistant", "content": content},
        "logprobs": logprobs
    }]})
    .to_string()
    .into_bytes()
}

/// Token probabilities for `entries` in which the model leaned on each
/// answer by `lean` (a log-probability gap, either way), but chose the
/// answer the entry gives.
fn logprobs(entries: &[(usize, Answer)], leans: &[f64]) -> Value {
    let tokens: Vec<Value> = entries
        .iter()
        .zip(leans.iter().cycle())
        .map(|(&(_, answer), &lean)| {
            let chosen = if answer == Answer::Yes {
                "true"
            } else {
                "false"
            };
            json!({
                "token": chosen,
                "logprob": -0.1,
                "top_logprobs": [
                    {"token": "true", "logprob": -1.0 + lean},
                    {"token": "false", "logprob": -1.0 - lean},
                    {"token": " true", "logprob": -9.0}
                ]
            })
        })
        .collect();
    json!({ "content": tokens })
}

proptest! {
    /// Any bytes are read or refused, never a panic; read, they number
    /// the candidates exactly.
    #[test]
    fn any_bytes_give_decisions_or_an_error(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        candidates in 1..=MAX_CANDIDATES,
    ) {
        if let Ok(decisions) = parse(&bytes, candidates) {
            prop_assert_eq!(decisions.len(), candidates);
        }
    }

    /// Any message text inside a finished completion is read or refused,
    /// never a panic; read, it numbers the candidates exactly.
    #[test]
    fn any_message_gives_decisions_or_an_error(
        content in ".{0,200}",
        candidates in 1..=MAX_CANDIDATES,
    ) {
        if let Ok(decisions) = parse(&completion(&content, &Value::Null), candidates) {
            prop_assert_eq!(decisions.len(), candidates);
        }
    }

    /// A well-formed reply gives back its answers by candidate, whatever
    /// order its entries are in, with no probabilities when it has none.
    #[test]
    fn a_well_formed_reply_gives_its_answers(entries in any_shuffled_answers()) {
        let decisions = parse(&completion(&text(&entries), &Value::Null), entries.len()).unwrap();
        for &(index, answer) in &entries {
            prop_assert_eq!(decisions[index], Decision { answer, yes: None });
        }
    }

    /// The token probabilities give each answer a probability from 0 to
    /// 1, and never change the answer, however hard they lean against it.
    #[test]
    fn probabilities_never_change_an_answer(
        entries in any_shuffled_answers(),
        leans in prop::collection::vec(-20.0_f64..20.0, 1..8),
    ) {
        let body = completion(&text(&entries), &logprobs(&entries, &leans));
        let decisions = parse(&body, entries.len()).unwrap();
        for &(index, answer) in &entries {
            let decision = decisions[index];
            prop_assert_eq!(decision.answer, answer);
            let yes = decision.yes.unwrap().get();
            prop_assert!((0.0..=1.0).contains(&yes), "{yes}");
        }
    }

    /// A reply that gives one candidate twice in place of another is
    /// refused.
    #[test]
    fn a_repeated_candidate_is_refused(
        entries in any_shuffled_answers().prop_filter("two or more", |e| e.len() > 1),
        from in any::<prop::sample::Index>(),
        to in any::<prop::sample::Index>(),
    ) {
        let mut entries = entries;
        let (from, to) = (from.index(entries.len()), to.index(entries.len()));
        prop_assume!(from != to);
        entries[to].0 = entries[from].0;
        let refused = parse(&completion(&text(&entries), &Value::Null), entries.len());
        prop_assert!(matches!(refused, Err(ReplyError::Repeated(_))), "{refused:?}");
    }

    /// A reply that leaves out a candidate is refused.
    #[test]
    fn a_missing_candidate_is_refused(
        entries in any_shuffled_answers(),
        gone in any::<prop::sample::Index>(),
    ) {
        let mut entries = entries;
        let gone = entries.remove(gone.index(entries.len())).0;
        let refused = parse(&completion(&text(&entries), &Value::Null), entries.len() + 1);
        let gone = u32::try_from(gone + 1).unwrap();
        prop_assert!(matches!(refused, Err(ReplyError::Missing(n)) if n == gone), "{refused:?}");
    }
}
