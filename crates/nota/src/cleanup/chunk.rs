//! One chunk of clean-up: the candidates the lookup offered in it, the
//! decider's answers, the gate on each yes, and the outcome each ends in.
//!
//! **The chunk is its words.** The lookup and the gate read the chunk's
//! words joined by single spaces, which is how every later revision shows
//! a changed chunk, so a byte range in that text names words by position.
//! A range is widened to the whole words it touches: the gate's
//! boundaries are the bench's, so a span can sit inside a hyphenated word
//! ("up" in "follow-up"), and the change keeps the rest of the word.
//!
//! **Every candidate ends in exactly one outcome** ([`Decided`]): a no,
//! or no answer, is rejected and logged; a yes the gate refuses is to be
//! ranked as a card; a yes the gate lets through is applied, unless the
//! text wouldn't change or its words are already reviewed or changed,
//! which are rejected and logged. Fixes the gate places in the same words
//! become one change.

use std::ops::Range;

use nota_checks::{Decision as GateDecision, Offered, Proposal, Rejection, gate};
use nota_core::LogProb;
use nota_core::decide::{Answer, Decision as Said, Probability};
use nota_store::{
    ChunkKey, Decided, DecidedBy, Found, NewProposal, Origin, Proposed, Refusal, WordSpan,
};

#[cfg(test)]
mod tests;

/// A chunk of revision 0 as clean-up reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chunk {
    /// Which chunk it is.
    key: ChunkKey,
    /// Its words joined by single spaces.
    text: String,
    /// Where each word is in `text`, in bytes, by position.
    spans: Vec<Range<usize>>,
    /// Each word's log-probability, by position.
    log_probs: Vec<Option<LogProb>>,
}

impl Chunk {
    /// The chunk `key` with `words` (text and log-probability each), or
    /// `None` if it has no words: there's nothing to name a change by.
    pub(crate) fn new<'w>(
        key: ChunkKey,
        words: impl IntoIterator<Item = (&'w str, Option<LogProb>)>,
    ) -> Option<Self> {
        let mut text = String::new();
        let mut spans = Vec::new();
        let mut log_probs = Vec::new();
        for (word, log_prob) in words {
            if !spans.is_empty() {
                text.push(' ');
            }
            let start = text.len();
            text.push_str(word);
            spans.push(start..text.len());
            log_probs.push(log_prob);
        }
        (!spans.is_empty()).then_some(Self {
            key,
            text,
            spans,
            log_probs,
        })
    }

    /// Which chunk it is.
    pub(crate) const fn key(&self) -> ChunkKey {
        self.key
    }

    /// Its words joined by single spaces: what the lookup and gate read.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The words `range` (bytes in [`Chunk::text`]) touches, or `None` if
    /// it touches none (it lies in the spaces between words).
    pub(crate) fn words(&self, range: &Range<usize>) -> Option<WordSpan> {
        let touches = |span: &Range<usize>| span.start < range.end && range.start < span.end;
        let first = self.spans.iter().position(touches)?;
        let last = self.spans.iter().rposition(touches)?;
        WordSpan::new(position(first), position(last))
    }

    /// Where `words` are in [`Chunk::text`], in bytes.
    fn bytes(&self, words: WordSpan) -> Range<usize> {
        let at = |n: u32| usize::try_from(n).unwrap_or(usize::MAX);
        let start = self.spans.get(at(words.first())).map_or(0, |s| s.start);
        let end = self
            .spans
            .get(at(words.last()))
            .map_or(self.text.len(), |s| s.end);
        start..end
    }

    /// The lowest known log-probability among `words`.
    fn lowest_log_prob(&self, words: WordSpan) -> Option<LogProb> {
        let at = |n: u32| usize::try_from(n).unwrap_or(usize::MAX);
        self.log_probs
            .get(at(words.first())..=at(words.last()))
            .into_iter()
            .flatten()
            .flatten()
            .copied()
            .min_by(|a, b| a.get().total_cmp(&b.get()))
    }

    /// The text of `words` with `fixes` (byte ranges inside them, not
    /// overlapping, and what replaces each) in place.
    fn rewritten(&self, words: WordSpan, fixes: &[(Range<usize>, &str)]) -> String {
        let outer = self.bytes(words);
        let mut sorted: Vec<&(Range<usize>, &str)> = fixes.iter().collect();
        sorted.sort_by_key(|(range, _)| range.start);
        let mut out = String::new();
        let mut at = outer.start;
        for (range, replacement) in sorted {
            out.push_str(self.text.get(at..range.start).unwrap_or_default());
            out.push_str(replacement);
            at = range.end;
        }
        out.push_str(self.text.get(at..outer.end).unwrap_or_default());
        out
    }
}

/// A word position, from a count that a chunk's words never exceed.
fn position(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A candidate the lookup offered in a chunk, with where its term came
/// from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Offer {
    /// The heard words, as the lookup gave them.
    pub(crate) window: String,
    /// Where they are in the chunk's text, in bytes.
    pub(crate) span: Range<usize>,
    /// The chunk's words they touch.
    pub(crate) words: WordSpan,
    /// The term offered.
    pub(crate) term: String,
    /// How the lookup found it.
    pub(crate) found: Found,
    /// Where the term came from.
    pub(crate) origin: Origin,
}

impl Offer {
    /// The candidate as the gate's offered list holds it.
    fn offered(&self) -> Offered {
        Offered {
            window: self.window.clone(),
            term: self.term.clone(),
        }
    }
}

/// What the decider gave on a chunk's question.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Answers {
    /// One decision per candidate asked about, in order.
    Given(Vec<Said>),
    /// No answer, for the reason given: a reply that couldn't be read, or
    /// a question that couldn't be asked.
    None(String),
}

/// What a chunk's proposals record beyond each candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Context {
    /// The decider that was asked.
    pub(crate) decided_by: DecidedBy,
    /// The thresholds in force.
    pub(crate) thresholds: String,
}

/// The gate's verdict on one yes, as the outcome needs it.
#[derive(Debug, Clone, PartialEq)]
enum Verdict {
    /// Placed at `range`, with this C1 score.
    Placed {
        /// Where in the chunk's text, in bytes.
        range: Range<usize>,
        /// The text that replaces it.
        replacement: String,
        /// C1's score.
        similarity: f64,
    },
    /// Refused by `rule`, with C1's score if it was reached.
    Refused {
        /// The rule.
        rule: Refusal,
        /// C1's score.
        similarity: Option<f64>,
    },
}

impl Verdict {
    /// The gate's `decision`, as clean-up keeps it.
    fn of(decision: &GateDecision) -> Self {
        let similarity = decision.similarity().map(nota_checks::Similarity::value);
        match (decision.applied(), decision.rejection()) {
            (Some(fix), _) => Self::Placed {
                range: fix.range(),
                replacement: fix.replacement().to_owned(),
                similarity: fix.similarity().value(),
            },
            (None, Some(rule)) => Self::Refused {
                rule: refusal(rule),
                similarity,
            },
            // A decision is always one or the other.
            (None, None) => Self::Refused {
                rule: Refusal::NotOffered,
                similarity,
            },
        }
    }
}

/// The gate's rule, as the store names it.
const fn refusal(rule: Rejection) -> Refusal {
    match rule {
        Rejection::EmptySpan => Refusal::EmptySpan,
        Rejection::SpanNotFound => Refusal::SpanNotFound,
        Rejection::TooLong => Refusal::TooLong,
        Rejection::Dissimilar => Refusal::Dissimilar,
        Rejection::NotOffered => Refusal::NotOffered,
        Rejection::WindowElsewhere => Refusal::WindowElsewhere,
        Rejection::AlreadyChanged => Refusal::AlreadyChanged,
    }
}

/// Each asked candidate's answer: `Some` yes or no with its probability,
/// `None` (with the reason) when the decider gave none.
fn per_candidate(answers: &Answers, n: usize) -> Vec<Result<Said, String>> {
    match answers {
        // A list of the wrong length is no answer: the decider's types
        // never give one, so this is only a guard.
        Answers::Given(said) if said.len() == n => said.iter().copied().map(Ok).collect(),
        Answers::Given(said) => vec![Err(format!("{} answers for {n} candidates", said.len())); n],
        Answers::None(why) => vec![Err(why.clone()); n],
    }
}

/// Settles a chunk: the outcome of each of `asked` (the candidates put to
/// the decider, in order), given `answers`. `offered` is every candidate
/// the lookup offered in the chunk, which the gate checks a yes against;
/// `blocked` are words a review or an earlier fix holds, which no new fix
/// may change. Gives one proposal per asked candidate, in order.
pub(crate) fn settle(
    chunk: &Chunk,
    offered: &[Offer],
    asked: &[Offer],
    answers: &Answers,
    blocked: &[WordSpan],
    context: &Context,
) -> Vec<NewProposal> {
    let said = per_candidate(answers, asked.len());
    let verdicts = gate_yeses(chunk, offered, asked, &said);
    let fixes = Fixes::new(chunk, &verdicts, blocked);
    asked
        .iter()
        .zip(&said)
        .zip(&verdicts)
        .enumerate()
        .map(|(i, ((offer, said), verdict))| {
            let decided = decided(said, verdict.as_ref(), fixes.kept(i));
            let (words, replacement) = fixes.words_of(i).unwrap_or_else(|| {
                let text = chunk.rewritten(offer.words, &[(offer.span.clone(), &offer.term)]);
                (offer.words, text)
            });
            NewProposal {
                proposed: proposed(chunk, offer, words, replacement, context),
                decided,
            }
        })
        .collect()
}

/// The gate's verdict on each yes among `asked`, in order; `None` for a
/// candidate that isn't a yes.
fn gate_yeses(
    chunk: &Chunk,
    offered: &[Offer],
    asked: &[Offer],
    said: &[Result<Said, String>],
) -> Vec<Option<Verdict>> {
    let yes: Vec<usize> = said
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            matches!(
                s,
                Ok(Said {
                    answer: Answer::Yes,
                    ..
                })
            )
        })
        .map(|(i, _)| i)
        .collect();
    let proposals: Vec<Proposal> = yes
        .iter()
        .map(|&i| Proposal {
            heard: asked[i].window.clone(),
            replacement: asked[i].term.clone(),
        })
        .collect();
    let offered: Vec<Offered> = offered.iter().map(Offer::offered).collect();
    let gated = gate(chunk.text(), &offered, &proposals);
    let mut verdicts = vec![None; asked.len()];
    for (&i, decision) in yes.iter().zip(gated.decisions()) {
        verdicts[i] = Some(Verdict::of(decision));
    }
    verdicts
}

/// What a fix the gate placed comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kept {
    /// It changes the text: part of a change.
    Changes,
    /// Its replacement is the text that's there.
    Unchanged,
    /// Its words are held by a review or an earlier fix.
    Covered,
}

/// The fixes the gate placed in a chunk, as whole-word changes.
struct Fixes {
    /// For each asked candidate, what its placed fix comes to.
    kept: Vec<Option<Kept>>,
    /// For each asked candidate whose fix changes the text, its change's
    /// words and their new text.
    changes: Vec<Option<(WordSpan, String)>>,
}

impl Fixes {
    /// The placed fixes among `verdicts`, widened to words and grouped
    /// where they share one; any touching `blocked` is covered.
    fn new(chunk: &Chunk, verdicts: &[Option<Verdict>], blocked: &[WordSpan]) -> Self {
        let mut kept = vec![None; verdicts.len()];
        let mut placed: Vec<(usize, WordSpan, Range<usize>, &str)> = Vec::new();
        for (i, verdict) in verdicts.iter().enumerate() {
            let Some(Verdict::Placed {
                range, replacement, ..
            }) = verdict
            else {
                continue;
            };
            let Some(words) = chunk.words(range) else {
                kept[i] = Some(Kept::Unchanged);
                continue;
            };
            kept[i] = Some(if blocked.iter().any(|b| b.overlaps(words)) {
                Kept::Covered
            } else if chunk.text().get(range.clone()) == Some(replacement.as_str()) {
                Kept::Unchanged
            } else {
                placed.push((i, words, range.clone(), replacement));
                Kept::Changes
            });
        }
        let changes = group(chunk, placed, verdicts.len());
        Self { kept, changes }
    }

    /// What candidate `i`'s placed fix comes to, if the gate placed one.
    fn kept(&self, i: usize) -> Option<Kept> {
        self.kept.get(i).copied().flatten()
    }

    /// Candidate `i`'s change's words and text, if its fix changes the
    /// text.
    fn words_of(&self, i: usize) -> Option<(WordSpan, String)> {
        self.changes.get(i).cloned().flatten()
    }
}

/// Groups `placed` fixes (candidate, words, range, replacement) whose
/// words overlap, and gives each candidate its group's words and text.
fn group(
    chunk: &Chunk,
    mut placed: Vec<(usize, WordSpan, Range<usize>, &str)>,
    n: usize,
) -> Vec<Option<(WordSpan, String)>> {
    placed.sort_by_key(|(_, words, ..)| *words);
    let mut out = vec![None; n];
    let mut start = 0;
    while start < placed.len() {
        let mut words = placed[start].1;
        let mut end = start + 1;
        while let Some(next) = placed.get(end).filter(|p| p.1.overlaps(words)) {
            words = WordSpan::new(words.first(), words.last().max(next.1.last())).unwrap_or(words);
            end += 1;
        }
        let fixes: Vec<(Range<usize>, &str)> = placed[start..end]
            .iter()
            .map(|(_, _, range, replacement)| (range.clone(), *replacement))
            .collect();
        let text = chunk.rewritten(words, &fixes);
        for (i, ..) in &placed[start..end] {
            out[*i] = Some((words, text.clone()));
        }
        start = end;
    }
    out
}

/// The decision on a candidate: its answer (or why there's none), the
/// gate's verdict on a yes, and what a placed fix comes to.
fn decided(said: &Result<Said, String>, verdict: Option<&Verdict>, kept: Option<Kept>) -> Decided {
    let said = match said {
        Ok(said) => said,
        Err(why) => return Decided::NoAnswer(why.clone()),
    };
    let yes: Option<Probability> = said.yes;
    match (said.answer, verdict, kept) {
        (Answer::No, ..) => Decided::No { yes },
        // The gate gives every yes a verdict; a yes without one is taken
        // as refused, so nothing is applied that the gate didn't place.
        (Answer::Yes, None, _) => Decided::Refused {
            yes,
            rule: Refusal::NotOffered,
            similarity: None,
        },
        (Answer::Yes, Some(Verdict::Refused { rule, similarity }), _) => Decided::Refused {
            yes,
            rule: *rule,
            similarity: *similarity,
        },
        (Answer::Yes, Some(Verdict::Placed { similarity, .. }), kept) => {
            let similarity = *similarity;
            match kept {
                Some(Kept::Changes) => Decided::Applied { yes, similarity },
                Some(Kept::Covered) => Decided::Covered { yes, similarity },
                Some(Kept::Unchanged) | None => Decided::Unchanged { yes, similarity },
            }
        }
    }
}

/// What a proposal records about `offer`, whose outcome is for `words`
/// of `chunk`, reading `replacement`.
fn proposed(
    chunk: &Chunk,
    offer: &Offer,
    words: WordSpan,
    replacement: String,
    context: &Context,
) -> Proposed {
    Proposed {
        chunk: chunk.key(),
        words,
        offered: offer.words,
        heard: offer.window.clone(),
        term: offer.term.clone(),
        replacement,
        origin: offer.origin.clone(),
        found: offer.found,
        decided_by: context.decided_by.clone(),
        log_prob: chunk.lowest_log_prob(offer.words),
        thresholds: context.thresholds.clone(),
    }
}
