//! The chat request a question becomes, in llama-server's
//! OpenAI-compatible form.
//!
//! The settings are the bench's (`pick.py`): temperature 0, thinking off,
//! and a JSON schema that llama-server turns into a grammar, so the reply
//! can only be one `{"id", "apply"}` entry per candidate, by number. The
//! schema can't stop a number being repeated or left out; the reply's
//! parser checks that. The request also asks for the five likeliest tokens
//! at each step, from which the parser reads how likely a yes was.

use nota_core::decide::Question;
use serde_json::{Value, json};

use crate::prompt::Prompt;

/// How many tokens the reply may use: the bench's 200, which fits its
/// five candidates, or more for a bigger question (each entry takes about
/// 12 tokens).
fn max_tokens(candidates: usize) -> usize {
    200_usize.max(40 + 16 * candidates)
}

/// The schema the reply must follow: `{"decisions": [...]}` with exactly
/// `candidates` entries, each a candidate's number and a yes or no.
fn schema(candidates: usize) -> Value {
    let ids: Vec<usize> = (1..=candidates).collect();
    json!({
        "type": "object",
        "properties": {
            "decisions": {
                "type": "array",
                "minItems": candidates,
                "maxItems": candidates,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer", "enum": ids},
                        "apply": {"type": "boolean"}
                    },
                    "required": ["id", "apply"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["decisions"],
        "additionalProperties": false
    })
}

/// The request body that puts `question` to the model in `prompt`'s words.
#[must_use]
pub fn body(prompt: &dyn Prompt, question: &Question) -> Vec<u8> {
    let request = json!({
        "temperature": 0,
        "max_tokens": max_tokens(question.len()),
        "logprobs": true,
        "top_logprobs": 5,
        "chat_template_kwargs": {"enable_thinking": false},
        "messages": [
            {"role": "system", "content": prompt.system()},
            {"role": "user", "content": prompt.user(question)}
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "decisions", "strict": true, "schema": schema(question.len())}
        }
    });
    request.to_string().into_bytes()
}

#[cfg(test)]
mod tests {
    use nota_core::decide::Candidate;

    use super::*;
    use crate::prompt::English;

    /// A question about `n` candidates.
    fn question(n: usize) -> Question {
        let one = Candidate::new("amylo diping".to_owned(), "amlodipine".to_owned(), None).unwrap();
        Question::new(None, "Start amylo diping.".to_owned(), vec![one; n]).unwrap()
    }

    /// The request carries the bench's settings, both messages, and a
    /// schema numbered for the question's candidates.
    #[test]
    fn the_request_has_the_bench_settings_and_a_numbered_schema() {
        let prompt = English::new(None);
        let q = question(3);
        let sent: Value = serde_json::from_slice(&body(&prompt, &q)).unwrap();
        assert_eq!(sent["temperature"], 0);
        assert_eq!(sent["max_tokens"], 200);
        assert_eq!(sent["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(sent["logprobs"], true);
        assert_eq!(sent["messages"][0]["content"], prompt.system());
        assert_eq!(sent["messages"][1]["content"], prompt.user(&q));
        let decisions =
            &sent["response_format"]["json_schema"]["schema"]["properties"]["decisions"];
        assert_eq!(decisions["minItems"], 3);
        assert_eq!(decisions["maxItems"], 3);
        assert_eq!(
            decisions["items"]["properties"]["id"]["enum"],
            json!([1, 2, 3])
        );
    }

    /// The schema names the candidate before its answer, as the bench's
    /// did: llama-server's grammar follows the schema's order, and a model
    /// made to answer first answers differently.
    #[test]
    fn the_schema_names_the_candidate_before_its_answer() {
        let sent = String::from_utf8(body(&English::new(None), &question(1))).unwrap();
        let items = &sent[sent.find(r#""items""#).unwrap()..];
        let (id, apply) = (
            items.find(r#""id""#).unwrap(),
            items.find(r#""apply""#).unwrap(),
        );
        assert!(id < apply, "{items}");
        assert!(sent.starts_with(r#"{"temperature":0,"#), "{sent}");
    }

    /// A question too big for the bench's 200 tokens is given more.
    #[test]
    fn a_big_question_is_given_more_tokens() {
        assert_eq!(max_tokens(1), 200);
        assert_eq!(max_tokens(10), 200);
        assert_eq!(max_tokens(11), 216);
        assert_eq!(max_tokens(32), 552);
    }
}
