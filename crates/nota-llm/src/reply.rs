//! Reading a model's reply to a question into typed decisions: a chat
//! completion from llama-server or another OpenAI-compatible server, or a
//! bare answer from a CLI ([`parse_answer`]).
//!
//! The reply comes from outside nota, so nothing in it is trusted. It's
//! read here, once, into one [`Decision`] per candidate, or refused:
//! - The body is a chat completion: the first choice must have finished
//!   (`"stop"`, not cut short at its token limit) with a text message.
//! - That text must be `{"decisions": [...]}` and nothing else, each entry
//!   `{"id": <n>, "apply": <bool>}`, with every candidate's number exactly
//!   once. The request's schema should already make it so, but a schema
//!   can't forbid a repeated number; a model or server that strays from it
//!   is refused, never half read.
//! - No object in the body or the text gives a key twice: which of the two
//!   to believe is a guess (`{"apply": false, "apply": true}`).
//! - A refused reply is a failed decision on the whole question: no
//!   candidate in it is a yes.
//!
//! The answers come from that text alone. The token probabilities that
//! come with it (`logprobs`) give each answer's [`Probability`] of a yes,
//! the weight of `true` against `false` where the model chose; if they're
//! missing, or don't line up with the answers, every probability is left
//! out rather than guessed, and the answers stand.

use std::error::Error;
use std::fmt;

use nota_core::decide::{Answer, Decision, Probability};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

/// The longest reply body read, in bytes. A reply for the most candidates,
/// with five token probabilities per token, is about 100 KiB.
pub const MAX_REPLY_LEN: usize = 1 << 20;

/// Why a reply was refused.
#[derive(Debug)]
pub enum ReplyError {
    /// The body was longer than [`MAX_REPLY_LEN`]: its length.
    TooLong(usize),
    /// The body, or the message text inside it, isn't JSON, or gives a key
    /// twice in one object.
    NotJson(serde_json::Error),
    /// The body isn't a chat completion with a finished text message: the
    /// part that's missing or wrong.
    NotACompletion(&'static str),
    /// The model hit its token limit before it finished.
    CutShort,
    /// The message text isn't one decision per candidate: what's wrong
    /// with it.
    NotDecisions(&'static str),
    /// A decision was for a candidate number the question doesn't have.
    UnknownCandidate(i128),
    /// Two decisions were for the same candidate number.
    Repeated(u32),
    /// No decision was given for this candidate number.
    Missing(u32),
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong(len) => {
                write!(f, "the reply is {len} bytes, more than {MAX_REPLY_LEN}")
            }
            Self::NotJson(e) => write!(f, "the reply isn't JSON: {e}"),
            Self::NotACompletion(what) => write!(f, "the reply isn't a finished answer: {what}"),
            Self::CutShort => f.write_str("the reply was cut short at its token limit"),
            Self::NotDecisions(what) => write!(f, "the answer isn't one per candidate: {what}"),
            Self::UnknownCandidate(id) => write!(
                f,
                "the answer names candidate {id}, which wasn't asked about"
            ),
            Self::Repeated(id) => write!(f, "the answer gives candidate {id} twice"),
            Self::Missing(id) => write!(f, "the answer leaves out candidate {id}"),
        }
    }
}

impl Error for ReplyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotJson(e) => Some(e),
            _ => None,
        }
    }
}

/// The decisions in `body`, a reply to a question about `candidates`
/// candidates: the first for candidate 1, and so on.
///
/// # Errors
///
/// If `body` isn't a finished chat completion whose text gives every
/// candidate exactly one yes or no; see [`ReplyError`].
pub fn parse(body: &[u8], candidates: usize) -> Result<Vec<Decision>, ReplyError> {
    if body.len() > MAX_REPLY_LEN {
        return Err(ReplyError::TooLong(body.len()));
    }
    let Strict(reply) = serde_json::from_slice(body).map_err(ReplyError::NotJson)?;
    let choice = reply
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or(ReplyError::NotACompletion("no choices"))?;
    match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") => {}
        Some("length") => return Err(ReplyError::CutShort),
        _ => return Err(ReplyError::NotACompletion("it didn't stop")),
    }
    let text = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .ok_or(ReplyError::NotACompletion("no message text"))?;
    let given = answers(text, candidates)?;
    let yes = probabilities(choice.get("logprobs"), &given);
    in_order(given.into_iter().zip(yes), candidates)
}

/// The decisions in `text`, a bare answer to a question about
/// `candidates` candidates (`{"decisions": [...]}`, as a CLI gives it),
/// with no probabilities: the first for candidate 1, and so on.
///
/// # Errors
///
/// If `text` is over [`MAX_REPLY_LEN`], or isn't one yes or no for every
/// candidate; see [`ReplyError`].
pub fn parse_answer(text: &str, candidates: usize) -> Result<Vec<Decision>, ReplyError> {
    if text.len() > MAX_REPLY_LEN {
        return Err(ReplyError::TooLong(text.len()));
    }
    let Strict(value) = serde_json::from_str(text).map_err(ReplyError::NotJson)?;
    answer_of(&value, candidates)
}

/// The decisions in `value`, an answer already read as JSON, with no
/// probabilities.
///
/// # Errors
///
/// If it isn't one yes or no for every candidate; see [`ReplyError`].
pub(crate) fn answer_of(value: &Value, candidates: usize) -> Result<Vec<Decision>, ReplyError> {
    let given = answers_in(value, candidates)?;
    in_order(given.into_iter().map(|g| (g, None)), candidates)
}

/// The decisions in `given`, each a candidate's index and answer with its
/// probability, put in candidate order.
fn in_order(
    given: impl Iterator<Item = ((usize, Answer), Option<Probability>)>,
    candidates: usize,
) -> Result<Vec<Decision>, ReplyError> {
    let mut decisions = vec![None; candidates];
    for ((index, answer), yes) in given {
        if let Some(slot) = decisions.get_mut(index) {
            *slot = Some(Decision { answer, yes });
        }
    }
    decisions
        .into_iter()
        .enumerate()
        .map(|(index, d)| d.ok_or_else(|| ReplyError::Missing(number(index))))
        .collect()
}

/// JSON read from `bytes` so that no object in it gives a key twice.
///
/// # Errors
///
/// If it isn't JSON, or an object gives a key twice.
// Only the CLI decider reads with it, and that is Unix only.
#[cfg(unix)]
pub(crate) fn strict_json(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    serde_json::from_slice(bytes).map(|Strict(value)| value)
}

/// Candidate `index`'s number (from 0, so one more).
fn number(index: usize) -> u32 {
    u32::try_from(index).map_or(u32::MAX, |i| i.saturating_add(1))
}

/// The answers in the message `text`, in the order it gave them, each with
/// its candidate's index (from 0). Every candidate of `candidates` has
/// exactly one.
fn answers(text: &str, candidates: usize) -> Result<Vec<(usize, Answer)>, ReplyError> {
    let Strict(value) = serde_json::from_str(text).map_err(ReplyError::NotJson)?;
    answers_in(&value, candidates)
}

/// The answers in `value`, the message text read as JSON, as [`answers`]
/// gives them.
fn answers_in(value: &Value, candidates: usize) -> Result<Vec<(usize, Answer)>, ReplyError> {
    let top = only_keys(value, &["decisions"]).ok_or(ReplyError::NotDecisions(
        "it isn't an object holding only \"decisions\"",
    ))?;
    let entries = top
        .get("decisions")
        .and_then(Value::as_array)
        .ok_or(ReplyError::NotDecisions("\"decisions\" isn't a list"))?;
    let mut seen = vec![false; candidates];
    let mut given = Vec::with_capacity(candidates.min(entries.len()));
    for entry in entries {
        let (index, answer) = entry_of(entry, candidates)?;
        let slot = seen
            .get_mut(index)
            .ok_or(ReplyError::Missing(number(index)))?;
        if *slot {
            return Err(ReplyError::Repeated(number(index)));
        }
        *slot = true;
        given.push((index, answer));
    }
    match seen.iter().position(|&s| !s) {
        Some(index) => Err(ReplyError::Missing(number(index))),
        None => Ok(given),
    }
}

/// One entry, `{"id": <n>, "apply": <bool>}`, as a candidate index (from
/// 0) and its answer.
fn entry_of(entry: &Value, candidates: usize) -> Result<(usize, Answer), ReplyError> {
    let fields = only_keys(entry, &["id", "apply"]).ok_or(ReplyError::NotDecisions(
        "an entry isn't an object holding only \"id\" and \"apply\"",
    ))?;
    let id = fields
        .get("id")
        .and_then(whole_number)
        .ok_or(ReplyError::NotDecisions(
            "an entry's id isn't a whole number",
        ))?;
    let apply = fields
        .get("apply")
        .and_then(Value::as_bool)
        .ok_or(ReplyError::NotDecisions(
            "an entry's apply isn't true or false",
        ))?;
    let index = usize::try_from(id)
        .ok()
        .and_then(|id| id.checked_sub(1))
        .filter(|&index| index < candidates)
        .ok_or(ReplyError::UnknownCandidate(id))?;
    Ok((index, if apply { Answer::Yes } else { Answer::No }))
}

/// `value` as a whole number, if it is one: written as an integer, not
/// `1.0`.
fn whole_number(value: &Value) -> Option<i128> {
    let number = value.as_number()?;
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
}

/// A JSON value read so that no object in it gives a key twice.
struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

/// Builds a [`Value`] as `serde_json`'s own does, except that a key given
/// twice is an error.
struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(Number::from_f64(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                // Without the key: a cloud model's reply may hold the
                // transcript's words, and none of them goes in a reason.
                return Err(de::Error::custom("a key is given twice"));
            }
            let Strict(value) = map.next_value()?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

/// `value` as an object, if it is one with exactly the keys `keys`.
fn only_keys<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Map<String, Value>> {
    let object = value.as_object()?;
    (object.len() == keys.len() && keys.iter().all(|k| object.contains_key(*k))).then_some(object)
}

/// The probability of a yes for each of the `given` answers, in their
/// order, from the choice's `logprobs`; all `None` if there are none or
/// they don't line up with the answers.
fn probabilities(logprobs: Option<&Value>, given: &[(usize, Answer)]) -> Vec<Option<Probability>> {
    read_probabilities(logprobs, given).unwrap_or_else(|| vec![None; given.len()])
}

/// The probabilities, if the tokens line up: the `true` and `false` tokens
/// the model chose, in order, are exactly the `given` answers.
fn read_probabilities(
    logprobs: Option<&Value>,
    given: &[(usize, Answer)],
) -> Option<Vec<Option<Probability>>> {
    let tokens = logprobs?.get("content")?.as_array()?;
    let mut chosen = Vec::with_capacity(given.len());
    for token in tokens {
        if let Some(answer) = token.get("token").and_then(Value::as_str).and_then(boolean) {
            chosen.push((answer, token));
        }
    }
    if chosen.len() != given.len() {
        return None;
    }
    chosen
        .iter()
        .zip(given)
        .map(|(&(answer, token), &(_, given))| (answer == given).then(|| yes_weight(token)))
        .collect()
}

/// The answer a token stands for, if it's `true` or `false` (spaces
/// around it allowed, as the tokenizer may join one on).
fn boolean(token: &str) -> Option<Answer> {
    match token.trim() {
        "true" => Some(Answer::Yes),
        "false" => Some(Answer::No),
        _ => None,
    }
}

/// How likely a yes was where the model chose `token`: the weight of the
/// `true` tokens among its likeliest, against `true` and `false` together.
/// `None` if neither is among them, or a weight isn't a number.
fn yes_weight(token: &Value) -> Option<Probability> {
    let (mut yes, mut no) = (0.0_f64, 0.0_f64);
    for alternative in token.get("top_logprobs")?.as_array()? {
        let Some(answer) = alternative
            .get("token")
            .and_then(Value::as_str)
            .and_then(boolean)
        else {
            continue;
        };
        let weight = alternative.get("logprob")?.as_f64()?.exp();
        match answer {
            Answer::Yes => yes += weight,
            Answer::No => no += weight,
        }
    }
    let total = yes + no;
    // No weight at all gives 0 / 0, which isn't a probability either.
    if total.is_finite() {
        Probability::new(yes / total)
    } else {
        None
    }
}

#[cfg(test)]
mod props;
#[cfg(test)]
mod tests;
