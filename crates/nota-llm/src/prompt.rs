//! The words a decider sends a model, one set per language.
//!
//! The English prompt is the research bench's (`pick.py`, the per-candidate
//! yes or no), word for word, except that the subject is the session's
//! rather than always "an Australian pharmacy lecture". The model answers
//! by candidate number, in JSON that the request's schema holds it to.

use std::fmt::Write as _;

use nota_core::decide::Question;

/// The words for one language: what the model is told it's doing, and how
/// a question is put to it.
pub trait Prompt: Send + Sync + std::fmt::Debug {
    /// The system message: the task, and the shape of the answer.
    fn system(&self) -> String;

    /// The user message that asks `question`.
    fn user(&self, question: &Question) -> String;
}

/// The subject used when a session doesn't name one.
pub const DEFAULT_SUBJECT: &str = "a lecture";

/// The English prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct English {
    /// What the recording is, as it fits "a transcript of …": "an
    /// Australian pharmacy lecture".
    subject: String,
}

impl English {
    /// The English prompt for a recording of `subject` ("an Australian
    /// pharmacy lecture"), or of [`DEFAULT_SUBJECT`] if it's `None` or
    /// blank.
    #[must_use]
    pub fn new(subject: Option<&str>) -> Self {
        let subject = subject
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_SUBJECT);
        Self {
            subject: subject.to_owned(),
        }
    }
}

impl Prompt for English {
    fn system(&self) -> String {
        format!(
            "You check candidate corrections for a speech-recognition transcript of {}.\n\
             Each numbered candidate says: the recogniser wrote some words (\"heard\"), \
             and a term from a vocabulary might be what the speaker really said.\n\
             For each candidate answer apply=true only if, in this context, the heard words \
             are clearly a garbled version of that term. Most candidates are coincidences: \
             answer false for those. Real words that make sense in the sentence must stay \
             (false).\n\
             Return JSON {{\"decisions\":[{{\"id\":<number>,\"apply\":<true|false>}}]}} \
             with one entry per candidate.",
            self.subject
        )
    }

    fn user(&self, question: &Question) -> String {
        let mut user = String::new();
        if let Some(previous) = question.previous() {
            user.push_str("PREVIOUS CHUNK (context only):\n");
            user.push_str(previous);
            user.push_str("\n\n");
        }
        user.push_str("CURRENT CHUNK:\n");
        user.push_str(question.current());
        user.push_str("\n\nCANDIDATES:\n");
        for (no, candidate) in question.candidates() {
            // Writing to a String can't fail.
            let _ = write!(
                user,
                "{no}. heard \"{}\" -> {}",
                candidate.heard(),
                candidate.term()
            );
            if let Some(fact) = candidate.fact() {
                let _ = write!(user, "   [{fact}]");
            }
            user.push('\n');
        }
        user
    }
}

/// The prompt for a session in `language` (its tag, "en" or "en-AU"), about
/// `subject`; `None` for a language nota has no prompt in yet. A session
/// with no language set is taken as English.
#[must_use]
pub fn for_language(language: Option<&str>, subject: Option<&str>) -> Option<Box<dyn Prompt>> {
    let primary = language.map_or("en", |tag| tag.split(['-', '_']).next().unwrap_or(tag));
    primary
        .eq_ignore_ascii_case("en")
        .then(|| Box::new(English::new(subject)) as Box<dyn Prompt>)
}

#[cfg(test)]
mod tests {
    use nota_core::decide::Candidate;

    use super::*;

    /// The bench's system message, word for word, with its subject.
    const BENCH_SYSTEM: &str = "You check candidate corrections for a speech-recognition \
        transcript of an Australian pharmacy lecture.\nEach numbered candidate says: the \
        recogniser wrote some words (\"heard\"), and a term from a vocabulary might be what \
        the speaker really said.\nFor each candidate answer apply=true only if, in this \
        context, the heard words are clearly a garbled version of that term. Most candidates \
        are coincidences: answer false for those. Real words that make sense in the sentence \
        must stay (false).\nReturn JSON {\"decisions\":[{\"id\":<number>,\"apply\":<true|false>}]} \
        with one entry per candidate.";

    /// With the bench's subject, the system message is the bench's.
    #[test]
    fn the_system_message_is_the_bench_one() {
        let prompt = English::new(Some("an Australian pharmacy lecture"));
        assert_eq!(prompt.system(), BENCH_SYSTEM);
    }

    /// A missing or blank subject reads as "a lecture".
    #[test]
    fn a_missing_subject_is_a_lecture() {
        for subject in [None, Some(""), Some("  ")] {
            let system = English::new(subject).system();
            assert!(
                system.starts_with(
                    "You check candidate corrections for a speech-recognition \
                     transcript of a lecture.\n"
                ),
                "{subject:?}: {system}"
            );
        }
    }

    /// The user message lays out the chunks and numbered candidates as
    /// the bench did, with a fact only where the candidate has one.
    #[test]
    fn the_user_message_is_laid_out_as_the_bench_one() {
        let candidates = vec![
            Candidate::new(
                "amylo diping".to_owned(),
                "amlodipine".to_owned(),
                Some("calcium channel blocker".to_owned()),
            )
            .unwrap(),
            Candidate::new("clinician".to_owned(), "clonidine".to_owned(), None).unwrap(),
        ];
        let question = Question::new(
            Some("Good morning.".to_owned()),
            "Start amylo diping, says the clinician.".to_owned(),
            candidates,
        )
        .unwrap();
        assert_eq!(
            English::new(None).user(&question),
            "PREVIOUS CHUNK (context only):\nGood morning.\n\n\
             CURRENT CHUNK:\nStart amylo diping, says the clinician.\n\n\
             CANDIDATES:\n\
             1. heard \"amylo diping\" -> amlodipine   [calcium channel blocker]\n\
             2. heard \"clinician\" -> clonidine\n"
        );
    }

    /// The first chunk has no previous chunk, and no heading for one.
    #[test]
    fn the_first_chunk_has_no_previous_heading() {
        let only = Candidate::new("x".to_owned(), "y".to_owned(), None).unwrap();
        let question = Question::new(None, "Hello.".to_owned(), vec![only]).unwrap();
        assert_eq!(
            English::new(None).user(&question),
            "CURRENT CHUNK:\nHello.\n\nCANDIDATES:\n1. heard \"x\" -> y\n"
        );
    }

    /// English and untagged sessions get the English prompt; other
    /// languages get none yet.
    #[test]
    fn only_english_has_a_prompt() {
        let subject = Some("a chemistry workshop");
        for tag in [None, Some("en"), Some("EN"), Some("en-AU"), Some("en_GB")] {
            let prompt = for_language(tag, subject).unwrap();
            assert_eq!(prompt.system(), English::new(subject).system(), "{tag:?}");
        }
        for tag in ["fr", "de-DE", "", "eng"] {
            assert!(for_language(Some(tag), subject).is_none(), "{tag:?}");
        }
    }
}
