//! The chunks whose question found the decider down, and when clean-up
//! gives up asking about one.
//!
//! **A down decider and a bad chunk look alike.** A run ends at the first
//! question the decider is unavailable for, and the next run asks the
//! first chunk not yet proposed again. That's the same chunk whether the
//! decider is down for every question (a stopped server, a usage limit) or
//! fails only on this one (a prompt the server always errors on, a
//! request that always times out). So a chunk is given up on only with
//! evidence that the decider works: it answered another question, in this
//! session or another, in the run the chunk first failed in or since.
//!
//! **What counts.** A chunk that fails is a suspect, and a later run asks
//! it after every other chunk, so the rest of the session is asked first
//! and can show the decider works. Each failure while that evidence
//! stands counts; a failure without it leaves the count at one. At
//! [`GIVE_UP`] counted failures the chunk is given up on: its candidates
//! are stored with no answer, and the run carries on. Once evidence
//! stands it stays, so a chunk that is the last one left still reaches
//! the limit; an outage after that evidence can cost a suspect chunk its
//! answer, but no chunk that failed only during the outage.
//!
//! **Kept in memory.** The suspects live as long as the runner's clean-up,
//! so closing nota starts every count again. A run stopped by a recording
//! counts nothing and keeps the counts. A chunk that gets an answer, or a
//! session whose clean-up ends, is forgotten.

use std::collections::BTreeMap;

use nota_core::SessionId;
use nota_store::ChunkKey;

/// How many runs in a row a chunk's question may find the decider down,
/// while it answers other questions, before clean-up gives up on it:
/// enough that one slow request or a server restart doesn't cost a
/// chunk its answer.
pub(crate) const GIVE_UP: u32 = 3;

/// One chunk whose question found the decider down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Suspect {
    /// The failures counted, from 1.
    failures: u32,
    /// Whether the decider has answered another question in the run this
    /// chunk first failed in, or since.
    vouched: bool,
}

/// What clean-up does after a chunk's question found the decider down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failed {
    /// Ends the run, to be tried again later.
    TryAgain,
    /// Gives up on the chunk and carries on with the next.
    GiveUp,
}

/// The suspect chunks of every session.
#[derive(Debug, Default)]
pub(crate) struct Suspects {
    /// Each suspect, by session and chunk.
    by_chunk: BTreeMap<(SessionId, ChunkKey), Suspect>,
}

impl Suspects {
    /// Whether `chunk` of `session` is a suspect, to be asked after the
    /// rest.
    pub(crate) fn has(&self, session: SessionId, chunk: ChunkKey) -> bool {
        self.by_chunk.contains_key(&(session, chunk))
    }

    /// Notes that the decider answered a question: every suspect now has
    /// the evidence that it works.
    pub(crate) fn answered(&mut self) {
        for suspect in self.by_chunk.values_mut() {
            suspect.vouched = true;
        }
    }

    /// Notes that `chunk` of `session` found the decider down, in a run
    /// that has had an answer already if `answered_before`, and says what
    /// to do next. A chunk given up on is forgotten.
    pub(crate) fn failed(
        &mut self,
        session: SessionId,
        chunk: ChunkKey,
        answered_before: bool,
    ) -> Failed {
        let key = (session, chunk);
        let suspect = self.by_chunk.entry(key).or_insert(Suspect {
            failures: 0,
            vouched: false,
        });
        suspect.vouched |= answered_before;
        // Without evidence the decider works, every failure may be the
        // decider's, so the count stays at one.
        suspect.failures = if suspect.vouched {
            suspect.failures.saturating_add(1)
        } else {
            1
        };
        if suspect.failures < GIVE_UP {
            return Failed::TryAgain;
        }
        self.by_chunk.remove(&key);
        Failed::GiveUp
    }

    /// Forgets `chunk` of `session`: its question was settled.
    pub(crate) fn settled(&mut self, session: SessionId, chunk: ChunkKey) {
        self.by_chunk.remove(&(session, chunk));
    }

    /// Forgets every chunk of `session`: its clean-up has ended.
    pub(crate) fn forget(&mut self, session: SessionId) {
        self.by_chunk.retain(|(s, _), _| *s != session);
    }
}

#[cfg(test)]
mod tests {
    use nota_core::{SampleIndex, TrackId};

    use super::*;

    /// The session the tests use.
    const S1: SessionId = SessionId::new(1);

    /// The chunk starting at sample `start` of track 0.
    fn chunk(start: u64) -> ChunkKey {
        ChunkKey {
            track: TrackId::new(0),
            start: SampleIndex::new(start),
        }
    }

    /// A decider down for every question never costs a chunk its answer,
    /// however many runs fail on it.
    #[test]
    fn a_decider_down_for_everything_gives_up_on_nothing() {
        let mut suspects = Suspects::default();
        let ends: Vec<Failed> = (0..10)
            .map(|_| suspects.failed(S1, chunk(0), false))
            .collect();
        assert_eq!(ends, [Failed::TryAgain; 10]);
        assert!(suspects.has(S1, chunk(0)));
    }

    /// A chunk that fails after the decider answered others in its run,
    /// and on the next runs with nothing else to ask, is given up on at
    /// the third.
    #[test]
    fn a_chunk_failing_while_the_decider_answers_is_given_up_on() {
        let mut suspects = Suspects::default();
        assert_eq!(suspects.failed(S1, chunk(0), true), Failed::TryAgain);
        assert_eq!(suspects.failed(S1, chunk(0), false), Failed::TryAgain);
        assert_eq!(suspects.failed(S1, chunk(0), false), Failed::GiveUp);
        // Given up on, it's forgotten.
        assert!(!suspects.has(S1, chunk(0)));
    }

    /// Failures during an outage count as one: the count starts once the
    /// decider answers something.
    #[test]
    fn failures_before_any_answer_count_as_one() {
        let mut suspects = Suspects::default();
        for _ in 0..5 {
            assert_eq!(suspects.failed(S1, chunk(0), false), Failed::TryAgain);
        }
        // Another session's question is answered.
        suspects.answered();
        assert_eq!(suspects.failed(S1, chunk(0), false), Failed::TryAgain);
        assert_eq!(suspects.failed(S1, chunk(0), false), Failed::GiveUp);
    }

    /// A chunk whose question is settled starts again from nothing if it
    /// fails later.
    #[test]
    fn a_settled_chunk_is_forgotten() {
        let mut suspects = Suspects::default();
        suspects.failed(S1, chunk(0), true);
        suspects.failed(S1, chunk(0), true);
        suspects.settled(S1, chunk(0));
        assert!(!suspects.has(S1, chunk(0)));
        assert_eq!(suspects.failed(S1, chunk(0), true), Failed::TryAgain);
    }

    /// A session whose clean-up ended keeps no suspects, and another
    /// session's are kept.
    #[test]
    fn forgetting_a_session_keeps_the_others() {
        let mut suspects = Suspects::default();
        let s2 = SessionId::new(2);
        suspects.failed(S1, chunk(0), false);
        suspects.failed(s2, chunk(0), false);
        suspects.forget(S1);
        assert!(!suspects.has(S1, chunk(0)));
        assert!(suspects.has(s2, chunk(0)));
    }
}
