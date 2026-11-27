//! The chunks whose question found the decider down, and when clean-up
//! gives up asking about one.
//!
//! **A down decider and a bad chunk look alike.** A run ends at the first
//! question the decider is unavailable for, and the next run asks the
//! first chunk not yet proposed again. That's the same chunk whether the
//! decider is down for every question (a stopped server, a usage limit) or
//! fails only on this one (a prompt the server always errors on, a
//! request that always times out). So after each such failure clean-up
//! asks the decider a fixed question of its own, the [`probe`]: if the
//! decider answers it, the failure was the chunk's and it counts; if the
//! probe fails too, the decider is down and nothing counts.
//!
//! **What counts.** A chunk with a counted failure is a suspect, and a
//! later run asks it after every other chunk, so the rest of the session
//! is decided while it keeps failing. At [`GIVE_UP`] counted failures,
//! in a row of runs or within one, the chunk is given up on: its
//! candidates are stored with no answer, and the run carries on. A
//! decider that is down, or restarts, while a chunk is asked fails the
//! probe too, so an outage costs no chunk its answer.
//!
//! **Kept in memory.** The suspects live as long as the runner's clean-up,
//! so closing nota starts every count again, and so does setting up
//! another decider: its failures say nothing of the last one's. A run
//! stopped by a recording counts nothing and keeps the counts. A chunk
//! whose proposals are stored, or a session whose clean-up ends, is
//! forgotten.

use std::collections::BTreeMap;

use nota_core::SessionId;
use nota_core::decide::{Candidate, Question, QuestionError};
use nota_store::{ChunkKey, DecidedBy};

/// How many counted failures a chunk's question may have before clean-up
/// gives up on it: enough that one slow request doesn't cost a chunk its
/// answer.
pub(crate) const GIVE_UP: u32 = 3;

/// The words of the [`probe`] question: invented, with one candidate
/// that any decider can answer.
const PROBE: &str = "take one tablet of paracetamol each morning";

/// The question clean-up asks after a chunk's question found the decider
/// down, to tell a bad chunk from a down decider: any answer to it, even
/// one that can't be read, shows the decider works.
///
/// # Errors
///
/// None in practice: its words are well within a question's limits. A
/// question that can't be made counts nothing.
pub(crate) fn probe() -> Result<Question, QuestionError> {
    let candidate = Candidate::new("paracetamol".to_owned(), "paracetamol".to_owned(), None)?;
    Question::new(None, PROBE.to_owned(), vec![candidate])
}

/// What clean-up does after a chunk's failure counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failed {
    /// Ends the run, to be tried again later.
    TryAgain,
    /// Gives up on the chunk and carries on with the next.
    GiveUp,
}

/// The suspect chunks of every session, and the decider they failed with.
#[derive(Debug, Default)]
pub(crate) struct Suspects {
    /// Each suspect's counted failures, by session and chunk.
    by_chunk: BTreeMap<(SessionId, ChunkKey), u32>,
    /// The decider the counts are for.
    decider: Option<DecidedBy>,
}

impl Suspects {
    /// Notes the decider a run asks: one other than the counts are for
    /// starts every count again.
    pub(crate) fn asking(&mut self, decider: &DecidedBy) {
        if self.decider.as_ref() != Some(decider) {
            self.by_chunk.clear();
            self.decider = Some(decider.clone());
        }
    }

    /// Whether `chunk` of `session` is a suspect, to be asked after the
    /// rest.
    pub(crate) fn has(&self, session: SessionId, chunk: ChunkKey) -> bool {
        self.by_chunk.contains_key(&(session, chunk))
    }

    /// Counts a failure of `chunk` of `session`, which the probe showed
    /// was the chunk's, and says what to do next. A chunk given up on
    /// stays a suspect until its proposals are stored
    /// ([`Suspects::settled`]), so a store that fails keeps its count.
    pub(crate) fn failed(&mut self, session: SessionId, chunk: ChunkKey) -> Failed {
        let failures = self.by_chunk.entry((session, chunk)).or_insert(0);
        *failures = failures.saturating_add(1);
        if *failures < GIVE_UP {
            Failed::TryAgain
        } else {
            Failed::GiveUp
        }
    }

    /// Forgets `chunk` of `session`: its proposals are stored.
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
    use nota_core::decide::Place;
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

    /// A local decider running `model`.
    fn decider(model: &str) -> DecidedBy {
        DecidedBy {
            name: "llama-server".to_owned(),
            model: model.to_owned(),
            place: Place::Local,
        }
    }

    /// The third counted failure gives the chunk up, and it stays a
    /// suspect until it's settled.
    #[test]
    fn a_chunk_is_given_up_on_at_its_third_counted_failure() {
        let mut suspects = Suspects::default();
        let ends: Vec<Failed> = (0..3).map(|_| suspects.failed(S1, chunk(0))).collect();
        assert_eq!(ends, [Failed::TryAgain, Failed::TryAgain, Failed::GiveUp]);
        // Its give-up isn't stored yet: a failed store asks again, and
        // gives up again at once.
        assert!(suspects.has(S1, chunk(0)));
        assert_eq!(suspects.failed(S1, chunk(0)), Failed::GiveUp);
        suspects.settled(S1, chunk(0));
        assert!(!suspects.has(S1, chunk(0)));
    }

    /// A chunk whose question is settled starts again from nothing if it
    /// fails later.
    #[test]
    fn a_settled_chunk_is_forgotten() {
        let mut suspects = Suspects::default();
        suspects.failed(S1, chunk(0));
        suspects.failed(S1, chunk(0));
        suspects.settled(S1, chunk(0));
        assert_eq!(suspects.failed(S1, chunk(0)), Failed::TryAgain);
    }

    /// Another decider's failures start from nothing; the same decider's
    /// keep counting.
    #[test]
    fn another_decider_starts_the_counts_again() {
        let mut suspects = Suspects::default();
        suspects.asking(&decider("a.gguf"));
        suspects.failed(S1, chunk(0));
        suspects.failed(S1, chunk(0));
        suspects.asking(&decider("a.gguf"));
        assert!(suspects.has(S1, chunk(0)));
        suspects.asking(&decider("b.gguf"));
        assert!(!suspects.has(S1, chunk(0)));
        assert_eq!(suspects.failed(S1, chunk(0)), Failed::TryAgain);
    }

    /// A session whose clean-up ended keeps no suspects, and another
    /// session's are kept.
    #[test]
    fn forgetting_a_session_keeps_the_others() {
        let mut suspects = Suspects::default();
        let s2 = SessionId::new(2);
        suspects.failed(S1, chunk(0));
        suspects.failed(s2, chunk(0));
        suspects.forget(S1);
        assert!(!suspects.has(S1, chunk(0)));
        assert!(suspects.has(s2, chunk(0)));
    }

    /// The probe is a question a decider can be asked.
    #[test]
    fn the_probe_can_be_asked() {
        let probe = probe().unwrap();
        assert_eq!(probe.current(), PROBE);
        assert_eq!(probe.len(), 1);
    }
}
