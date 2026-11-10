use super::*;

/// A candidate from the bench: "amylo diping" heard for amlodipine.
fn amlodipine() -> Candidate {
    Candidate::new("amylo diping".to_owned(), "amlodipine".to_owned(), None).unwrap()
}

/// A question about `n` copies of [`amlodipine`].
fn question(n: usize) -> Question {
    Question::new(
        None,
        "start on amylo diping".to_owned(),
        vec![amlodipine(); n],
    )
    .unwrap()
}

const YES: Decision = Decision {
    answer: Answer::Yes,
    yes: None,
};
const NO: Decision = Decision {
    answer: Answer::No,
    yes: None,
};

/// Each field refuses blank text, and says which field it was.
#[test]
fn blank_text_is_refused_by_field() {
    let blank = |heard: &str, term: &str, fact: Option<&str>| {
        Candidate::new(heard.to_owned(), term.to_owned(), fact.map(str::to_owned)).unwrap_err()
    };
    assert_eq!(
        blank(" ", "amlodipine", None),
        QuestionError::Empty("heard span")
    );
    assert_eq!(blank("amylo", "\t", None), QuestionError::Empty("term"));
    assert_eq!(
        blank("amylo", "amlodipine", Some("")),
        QuestionError::Empty("fact")
    );
    assert_eq!(
        Question::new(None, " \n".to_owned(), vec![amlodipine()]).unwrap_err(),
        QuestionError::Empty("current chunk")
    );
    assert_eq!(
        Question::new(Some(String::new()), "x".to_owned(), vec![amlodipine()]).unwrap_err(),
        QuestionError::Empty("previous chunk")
    );
}

/// Each limit lets its length through and refuses one byte more.
#[test]
fn text_is_refused_one_byte_past_its_limit() {
    let at = |n: usize| "a".repeat(n);
    assert!(Candidate::new(at(MAX_SPAN_LEN), at(MAX_SPAN_LEN), Some(at(MAX_FACT_LEN))).is_ok());
    assert_eq!(
        Candidate::new(at(MAX_SPAN_LEN + 1), "t".to_owned(), None).unwrap_err(),
        QuestionError::TooLong("heard span", MAX_SPAN_LEN)
    );
    assert_eq!(
        Candidate::new("h".to_owned(), at(MAX_SPAN_LEN + 1), None).unwrap_err(),
        QuestionError::TooLong("term", MAX_SPAN_LEN)
    );
    assert_eq!(
        Candidate::new("h".to_owned(), "t".to_owned(), Some(at(MAX_FACT_LEN + 1))).unwrap_err(),
        QuestionError::TooLong("fact", MAX_FACT_LEN)
    );
    assert!(
        Question::new(
            Some(at(MAX_CHUNK_LEN)),
            at(MAX_CHUNK_LEN),
            vec![amlodipine()]
        )
        .is_ok()
    );
    assert_eq!(
        Question::new(None, at(MAX_CHUNK_LEN + 1), vec![amlodipine()]).unwrap_err(),
        QuestionError::TooLong("current chunk", MAX_CHUNK_LEN)
    );
    assert_eq!(
        Question::new(
            Some(at(MAX_CHUNK_LEN + 1)),
            "x".to_owned(),
            vec![amlodipine()]
        )
        .unwrap_err(),
        QuestionError::TooLong("previous chunk", MAX_CHUNK_LEN)
    );
}

/// A question has from one to [`MAX_CANDIDATES`] candidates.
#[test]
fn a_question_has_one_to_the_most_candidates() {
    let ask = |n| Question::new(None, "x".to_owned(), vec![amlodipine(); n]);
    assert_eq!(ask(0).unwrap_err(), QuestionError::NoCandidates);
    assert_eq!(ask(1).unwrap().len(), 1);
    assert_eq!(ask(MAX_CANDIDATES).unwrap().len(), MAX_CANDIDATES);
    assert_eq!(
        ask(MAX_CANDIDATES + 1).unwrap_err(),
        QuestionError::TooManyCandidates(MAX_CANDIDATES + 1)
    );
    assert!(!ask(1).unwrap().is_empty());
}

/// Candidates are numbered from 1, in the order they were given.
#[test]
fn candidates_are_numbered_from_one_in_order() {
    let other = Candidate::new("hyper profen".to_owned(), "ibuprofen".to_owned(), None).unwrap();
    let q = Question::new(None, "x".to_owned(), vec![amlodipine(), other]).unwrap();
    let numbered: Vec<(u32, &str)> = q.candidates().map(|(no, c)| (no.get(), c.term())).collect();
    assert_eq!(numbered, [(1, "amlodipine"), (2, "ibuprofen")]);
}

/// Decisions are made only with one per candidate.
#[test]
fn decisions_need_one_per_candidate() {
    let q = question(2);
    assert_eq!(
        Decisions::new(&q, vec![YES]).unwrap_err(),
        WrongCount {
            decisions: 1,
            candidates: 2
        }
    );
    assert_eq!(
        Decisions::new(&q, vec![YES, NO, NO]).unwrap_err(),
        WrongCount {
            decisions: 3,
            candidates: 2
        }
    );
    let made = Decisions::new(&q, vec![YES, NO]).unwrap();
    let listed: Vec<(u32, Answer)> = made.iter().map(|(no, d)| (no.get(), d.answer)).collect();
    assert_eq!(listed, [(1, Answer::Yes), (2, Answer::No)]);
}

/// A decision is found by its candidate's number, and only for numbers
/// the question has.
#[test]
fn a_decision_is_found_by_its_candidates_number() {
    let q = question(2);
    let made = Decisions::new(&q, vec![YES, NO]).unwrap();
    let numbers: Vec<CandidateNo> = q.candidates().map(|(no, _)| no).collect();
    assert_eq!(made.get(numbers[0]), Some(&YES));
    assert_eq!(made.get(numbers[1]), Some(&NO));
    assert_eq!(made.get(CandidateNo(0)), None);
    assert_eq!(made.get(CandidateNo(3)), None);
}

/// A probability is a number from 0 to 1, ends included.
#[test]
fn a_probability_is_from_zero_to_one() {
    assert_eq!(Probability::new(0.0).map(Probability::get), Some(0.0));
    assert_eq!(Probability::new(1.0).map(Probability::get), Some(1.0));
    assert_eq!(Probability::new(0.25).map(Probability::get), Some(0.25));
    assert_eq!(Probability::new(-0.01), None);
    assert_eq!(Probability::new(1.01), None);
    assert_eq!(Probability::new(f64::NAN), None);
    assert_eq!(Probability::new(f64::INFINITY), None);
}

/// Errors say what went wrong in plain words, and keep their cause.
#[test]
fn errors_say_what_went_wrong() {
    assert_eq!(
        QuestionError::TooLong("term", 200).to_string(),
        "the term is longer than 200 bytes"
    );
    assert_eq!(
        QuestionError::TooManyCandidates(40).to_string(),
        "40 candidates is more than 32 in one question"
    );
    let cause = WrongCount {
        decisions: 1,
        candidates: 2,
    };
    let failed = DecideError::Failed(Box::new(cause));
    assert_eq!(
        failed.to_string(),
        "the decider's answer couldn't be read: 1 decisions for 2 candidates"
    );
    assert_eq!(
        failed.source().map(ToString::to_string),
        Some(cause.to_string())
    );
    assert!(DecideError::Stopped.source().is_none());
}
