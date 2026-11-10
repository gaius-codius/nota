//! The decider: given the candidates code found in a stretch of the
//! transcript, a yes or a no on each.
//!
//! Code finds the candidates (a heard span and a term it might be a
//! misheard form of); the decider only answers, one numbered candidate at a
//! time, whether the heard words really are that term. Its answer is never
//! applied as it stands: the candidate gate (C2) checks every yes. A
//! decider is a local model or a cloud one, and says which ([`Place`]), so
//! the screens can mark cloud work with ☁.
//!
//! The types hold the shape fixed: a [`Question`] has one to
//! [`MAX_CANDIDATES`] candidates, each with bounded text, and a
//! [`Decisions`] has exactly one [`Decision`] per candidate of its
//! question, in order, or it can't be built. A decision's answer comes
//! from the model's reply alone; a [`Probability`], where the decider can
//! give one, sits beside it and never turns a no into a yes.

use std::error::Error;
use std::fmt;

/// The most candidates one question asks about. The bench's chunks had at
/// most 5; a chunk with more is split by the caller.
pub const MAX_CANDIDATES: usize = 32;

/// The longest heard span or term, in bytes. The bench's longest is 77
/// letters; anything this long is no single term.
pub const MAX_SPAN_LEN: usize = 200;

/// The longest fact given with a term, in bytes. The bench's longest is
/// 148.
pub const MAX_FACT_LEN: usize = 512;

/// The longest stretch of transcript (the current chunk, or the previous
/// one given for context), in bytes. The bench's 30 s chunks are at most
/// 535.
pub const MAX_CHUNK_LEN: usize = 4_096;

/// Where a decider runs: on this machine, or a cloud service (☁).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// On this machine; nothing leaves it.
    Local,
    /// A cloud service: the transcript text in the question leaves the
    /// machine.
    Cloud,
}

/// Why a [`Candidate`] or a [`Question`] couldn't be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionError {
    /// A text that must say something was empty, or only white space:
    /// the field it was for.
    Empty(&'static str),
    /// A text was longer than its limit: the field, and the limit in
    /// bytes.
    TooLong(&'static str, usize),
    /// A question had no candidates.
    NoCandidates,
    /// A question had more than [`MAX_CANDIDATES`] candidates: how many.
    TooManyCandidates(usize),
}

impl fmt::Display for QuestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(field) => write!(f, "the {field} is empty"),
            Self::TooLong(field, limit) => {
                write!(f, "the {field} is longer than {limit} bytes")
            }
            Self::NoCandidates => f.write_str("a question needs at least one candidate"),
            Self::TooManyCandidates(n) => {
                write!(
                    f,
                    "{n} candidates is more than {MAX_CANDIDATES} in one question"
                )
            }
        }
    }
}

impl Error for QuestionError {}

/// `text` as a field's value: not blank, and at most `limit` bytes.
fn bounded(text: String, field: &'static str, limit: usize) -> Result<String, QuestionError> {
    if text.trim().is_empty() {
        Err(QuestionError::Empty(field))
    } else if text.len() > limit {
        Err(QuestionError::TooLong(field, limit))
    } else {
        Ok(text)
    }
}

/// One candidate correction: the recogniser wrote `heard`, and `term` may
/// be what the speaker said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The words as the recogniser wrote them.
    heard: String,
    /// The term from a pack or a learned pair.
    term: String,
    /// A line about the term that helps tell it apart, if the pack has one.
    fact: Option<String>,
}

impl Candidate {
    /// The candidate that `heard` is a misheard form of `term`, with the
    /// pack's `fact` about the term, if any.
    ///
    /// # Errors
    ///
    /// If `heard` or `term` is blank or longer than [`MAX_SPAN_LEN`], or
    /// `fact` is blank or longer than [`MAX_FACT_LEN`].
    pub fn new(heard: String, term: String, fact: Option<String>) -> Result<Self, QuestionError> {
        Ok(Self {
            heard: bounded(heard, "heard span", MAX_SPAN_LEN)?,
            term: bounded(term, "term", MAX_SPAN_LEN)?,
            fact: fact
                .map(|fact| bounded(fact, "fact", MAX_FACT_LEN))
                .transpose()?,
        })
    }

    /// The words as the recogniser wrote them.
    #[must_use]
    pub fn heard(&self) -> &str {
        &self.heard
    }

    /// The term they may be a misheard form of.
    #[must_use]
    pub fn term(&self) -> &str {
        &self.term
    }

    /// The pack's line about the term, if it has one.
    #[must_use]
    pub fn fact(&self) -> Option<&str> {
        self.fact.as_deref()
    }
}

/// A candidate's number in its question: 1 for the first. The model
/// answers by these numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CandidateNo(u32);

impl CandidateNo {
    /// The candidate's number, from 1.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The number of the candidate at `index` (from 0).
    fn at(index: usize) -> Self {
        // A question has at most MAX_CANDIDATES, far below u32::MAX.
        Self(u32::try_from(index).map_or(u32::MAX, |i| i.saturating_add(1)))
    }
}

impl fmt::Display for CandidateNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What a decider is asked: the candidates found in one chunk of the
/// transcript, with that chunk and the one before it for context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The chunk before, for context only, unless this is the first.
    previous: Option<String>,
    /// The chunk the candidates were found in.
    current: String,
    /// One to [`MAX_CANDIDATES`] candidates, numbered from 1 in order.
    candidates: Vec<Candidate>,
}

impl Question {
    /// The question about `candidates`, found in the chunk `current`, with
    /// the chunk before it, `previous`, for context.
    ///
    /// # Errors
    ///
    /// If either chunk is blank or longer than [`MAX_CHUNK_LEN`], or there
    /// are no candidates or more than [`MAX_CANDIDATES`].
    pub fn new(
        previous: Option<String>,
        current: String,
        candidates: Vec<Candidate>,
    ) -> Result<Self, QuestionError> {
        if candidates.is_empty() {
            return Err(QuestionError::NoCandidates);
        }
        if candidates.len() > MAX_CANDIDATES {
            return Err(QuestionError::TooManyCandidates(candidates.len()));
        }
        Ok(Self {
            previous: previous
                .map(|text| bounded(text, "previous chunk", MAX_CHUNK_LEN))
                .transpose()?,
            current: bounded(current, "current chunk", MAX_CHUNK_LEN)?,
            candidates,
        })
    }

    /// The chunk before the one the candidates are in, if there is one.
    #[must_use]
    pub fn previous(&self) -> Option<&str> {
        self.previous.as_deref()
    }

    /// The chunk the candidates were found in.
    #[must_use]
    pub fn current(&self) -> &str {
        &self.current
    }

    /// How many candidates it asks about: from 1 to [`MAX_CANDIDATES`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// Always false: a question has at least one candidate. Here because
    /// [`Question::len`] is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Each candidate with its number, in order.
    #[must_use]
    pub fn candidates(&self) -> impl ExactSizeIterator<Item = (CandidateNo, &Candidate)> {
        self.candidates
            .iter()
            .enumerate()
            .map(|(i, c)| (CandidateNo::at(i), c))
    }
}

/// A decider's answer on one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The heard words are a misheard form of the term: apply it, if the
    /// gate agrees.
    Yes,
    /// They aren't: leave them.
    No,
}

/// How likely the decider thought a yes was, from 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Probability(f64);

impl Probability {
    /// The probability `p`, or `None` unless it's a number from 0 to 1.
    #[must_use]
    pub fn new(p: f64) -> Option<Self> {
        (0.0..=1.0).contains(&p).then_some(Self(p))
    }

    /// Its value, from 0 to 1.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// The decision on one candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// The answer, as the model gave it.
    pub answer: Answer,
    /// How likely the model thought a yes was, where the decider can say
    /// (a local model's token probabilities). Cloud CLIs give none.
    pub yes: Option<Probability>,
}

/// The decisions on a question: exactly one per candidate, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Decisions(Vec<Decision>);

/// A list of decisions didn't match its question: how many there were,
/// and how many candidates it asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrongCount {
    /// How many decisions there were.
    pub decisions: usize,
    /// How many candidates the question had.
    pub candidates: usize,
}

impl fmt::Display for WrongCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} decisions for {} candidates",
            self.decisions, self.candidates
        )
    }
}

impl Error for WrongCount {}

impl Decisions {
    /// `decisions` as the answer to `question`: the first for its first
    /// candidate, and so on.
    ///
    /// # Errors
    ///
    /// If there isn't exactly one decision per candidate.
    pub fn new(question: &Question, decisions: Vec<Decision>) -> Result<Self, WrongCount> {
        if decisions.len() == question.len() {
            Ok(Self(decisions))
        } else {
            Err(WrongCount {
                decisions: decisions.len(),
                candidates: question.len(),
            })
        }
    }

    /// The decision on candidate `no`, if the question had one so
    /// numbered.
    #[must_use]
    pub fn get(&self, no: CandidateNo) -> Option<&Decision> {
        let index = usize::try_from(no.get()).ok()?.checked_sub(1)?;
        self.0.get(index)
    }

    /// Each decision with its candidate's number, in order.
    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (CandidateNo, &Decision)> {
        self.0
            .iter()
            .enumerate()
            .map(|(i, d)| (CandidateNo::at(i), d))
    }
}

/// Why a decider gave no decisions.
#[derive(Debug)]
pub enum DecideError {
    /// It was told to stop (a recording started, or nota is closing)
    /// before it had an answer. Nothing was decided and nothing runs on:
    /// ask again later.
    Stopped,
    /// It answered, but the answer couldn't be read as one yes or no per
    /// candidate (a malformed reply, a refusal, a reply cut short). A
    /// failed decision on this question: no candidate in it is a yes.
    /// Asking again gives the same answer, so the job carries on with the
    /// next question.
    Failed(Box<dyn Error + Send + Sync>),
    /// It couldn't be reached, or kept failing after the restarts it's
    /// allowed (a missing program, a server that crashes or hangs): the
    /// job fails with this reason.
    Unavailable(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for DecideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => f.write_str("the decider was stopped"),
            Self::Failed(e) => write!(f, "the decider's answer couldn't be read: {e}"),
            Self::Unavailable(e) => write!(f, "the decider isn't working: {e}"),
        }
    }
}

impl Error for DecideError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Stopped => None,
            Self::Failed(e) | Self::Unavailable(e) => Some(&**e),
        }
    }
}

/// Something that answers yes or no on candidates: a local model, or a
/// cloud one.
pub trait Decider {
    /// Where it runs, for the ☁ marker.
    fn place(&self) -> Place;

    /// The decisions on `question`'s candidates. `stop` is asked often
    /// while it works; once it says true, the decider stops what it's
    /// running and returns [`DecideError::Stopped`].
    ///
    /// # Errors
    ///
    /// [`DecideError::Stopped`] when `stop` said to,
    /// [`DecideError::Failed`] for an answer that can't be read, and
    /// [`DecideError::Unavailable`] when the decider can't run.
    fn decide(
        &mut self,
        question: &Question,
        stop: &dyn Fn() -> bool,
    ) -> Result<Decisions, DecideError>;
}

#[cfg(test)]
mod tests;
