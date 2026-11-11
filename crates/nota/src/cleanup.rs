//! Term clean-up: the job after the final pass that fixes misheard
//! specialist terms in its text, as tracked changes.
//!
//! For each chunk of the final text (revision 0), in session order:
//! **propose** (C4's lookup offers terms from the session's packs and the
//! learned pairs), **decide** (the decider answers yes or no on each),
//! **gate** (C2 checks every yes), and store every candidate as a proposal
//! with its outcome ([`chunk`]). A chunk's proposals are committed
//! together. At the end, the yeses the gate refused are ranked as "to
//! check" cards up to the cap, and the applied fixes become one new
//! revision, together in one transaction ([`cards`]).
//!
//! **No fix without the model's yes**: only a yes reaches the gate, and
//! only the gate places a fix.
//!
//! **Safe to run again.** A candidate the session has a proposal for
//! already (the same chunk, offered words, heard span and term) isn't
//! proposed again, and neither is one whose words a review or an applied
//! fix holds. So a run stopped by a recording, or killed, carries on from
//! the chunks it committed and ends as it would have, and a run after a
//! pack update only adds what's new.
//!
//! **The decider.** With none set up, the job waits for one; one that
//! can't be started, or keeps failing, ends the run as an engine failure
//! (it's tried again later, the reason shown); a question it gives no
//! readable answer to logs its candidates as rejected. Told to stop
//! between questions, the job returns at once, which drops the decider
//! and the server it keeps loaded.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use nota_checks::{
    Bar, Candidate as Found, English, KEEP, KEEP_RARE, Language, Lookup, MAX_CANDIDATES, Matched,
    SIMILARITY_MIN, Term,
};
use nota_core::decide::{Candidate, DecideError, Decider, Question};
use nota_core::{SampleIndex, SampleRate, SessionId, TrackId};
use nota_store::{
    ChunkKey, DecidedBy, FinalLine, JobEnd, JobKind, JobState, Origin, Outcome, Pack, Proposal,
    RevisionNumber, SessionTerms, StoreError, Wait, WordSpan, Writer,
};

use crate::jobs::Running;

pub(crate) mod cards;
pub(crate) mod chunk;
#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod sources;

use chunk::{Answers, Chunk, Context, Offer};

#[cfg(test)]
mod tests;

/// The decider a run would use, as the settings leave it now.
#[cfg_attr(
    all(not(unix), not(test)),
    expect(
        dead_code,
        reason = "only unix reads a decider's settings, so elsewhere none is set up"
    )
)]
pub(crate) enum Setup {
    /// None is set up: the job waits for one.
    NotSetUp,
    /// One is set up but can't be used, for the reason given.
    Unavailable(String),
    /// One to ask, and what to record as having answered.
    Ready(Box<dyn Decider + Send>, DecidedBy),
}

/// Where a run finds its decider and the installed packs.
pub(crate) trait Sources: Send {
    /// The decider, as the settings leave it now.
    fn decider(&self) -> Setup;

    /// The packs installed on this machine.
    ///
    /// # Errors
    ///
    /// The reason, for the user, if they can't be read.
    fn packs(&self) -> Result<Vec<Pack>, String>;
}

/// Runs term clean-up jobs.
pub(crate) struct CleanUp {
    /// Where the decider and packs come from.
    sources: Box<dyn Sources>,
    /// The final pass's sample rate, which times the recording for the
    /// card cap.
    rate: SampleRate,
    /// The English rules, built at the first run: unpacking the word list
    /// takes a while, and most nota sessions never need it.
    english: Option<Arc<English>>,
}

impl CleanUp {
    /// Clean-up taking its decider and packs from `sources`, over text at
    /// `rate`.
    pub(crate) fn new(sources: Box<dyn Sources>, rate: SampleRate) -> Self {
        Self {
            sources,
            rate,
            english: None,
        }
    }

    /// What a clean-up job of `session` waits for now, if anything: its
    /// session's final pass, then a decider.
    pub(crate) fn lacks(&self, db: &Writer, session: SessionId) -> Option<Wait> {
        match final_pass(db, session) {
            Ok(Some(JobState::Done) | None) => {}
            // A failed final pass, or one that can't be read, is reported
            // by the run, without waiting for a decider first.
            Ok(Some(JobState::Failed(_))) | Err(_) => return None,
            Ok(Some(_)) => return Some(Wait::FinalText),
        }
        matches!(self.sources.decider(), Setup::NotSetUp).then_some(Wait::Decider)
    }

    /// Runs clean-up over `session` until it's done or `running` says to
    /// stop.
    pub(crate) fn run(&mut self, db: &Writer, session: SessionId, running: &Running<'_>) -> JobEnd {
        match final_pass(db, session) {
            Ok(Some(JobState::Done) | None) => {}
            Ok(Some(JobState::Failed(why))) => {
                return JobEnd::Failed(format!("there's no final transcript to fix: {why}"));
            }
            Ok(Some(_)) => return JobEnd::Waiting(Some(Wait::FinalText)),
            Err(e) => return stored(&e),
        }
        let (decider, by) = match self.sources.decider() {
            Setup::NotSetUp => return JobEnd::Waiting(Some(Wait::Decider)),
            Setup::Unavailable(why) => return JobEnd::EngineDown(why),
            Setup::Ready(decider, by) => (decider, by),
        };
        let english = match self.english() {
            Ok(english) => english,
            Err(why) => return JobEnd::Failed(why),
        };
        let packs = match self.sources.packs() {
            Ok(packs) => packs,
            Err(why) => return JobEnd::Failed(why),
        };
        let found = db.with(|db| read(db, session, &packs));
        let read = match found {
            Ok(read) => read,
            Err(e) => return stored(&e),
        };
        let pass = Pass {
            db,
            session,
            english: &english,
            context: Context {
                decided_by: by,
                thresholds: thresholds(),
            },
            running,
        };
        pass.run(&read, decider, self.rate)
    }

    /// The English rules, built once.
    fn english(&mut self) -> Result<Arc<English>, String> {
        if let Some(english) = &self.english {
            return Ok(Arc::clone(english));
        }
        let english =
            Arc::new(English::new().map_err(|e| format!("the term lookup can't start: {e}"))?);
        self.english = Some(Arc::clone(&english));
        Ok(english)
    }
}

/// The state of `session`'s final pass, if it has one.
fn final_pass(db: &Writer, session: SessionId) -> Result<Option<JobState>, StoreError> {
    let jobs = db.with(|db| db.session_jobs(session))?;
    Ok(jobs
        .into_iter()
        .find(|job| job.kind == JobKind::FinalPass)
        .map(|job| job.state))
}

/// How a job ends on a store error: waiting for space if the disk is
/// full, failed otherwise.
fn stored(e: &StoreError) -> JobEnd {
    crate::final_pass::stored(e)
}

/// The thresholds a proposal records, as `name=value` pairs.
fn thresholds() -> String {
    format!(
        "similarity_min={SIMILARITY_MIN};keep={KEEP};keep_rare={KEEP_RARE};\
         max_candidates={MAX_CANDIDATES};card_every_s={}",
        cards::CARD_EVERY.as_secs()
    )
}

/// What a run reads before it starts.
struct Read {
    /// Revision 0, in session order.
    lines: Vec<FinalLine>,
    /// The session's vocabulary.
    terms: SessionTerms,
    /// The proposals made already.
    proposals: Vec<Proposal>,
}

/// Reads `session`'s final text, vocabulary (from `installed` packs) and
/// proposals. A session with no final text has no lines.
fn read(
    db: &nota_store::Store,
    session: SessionId,
    installed: &[Pack],
) -> Result<Read, StoreError> {
    let lines = match db.revision(session, RevisionNumber::FINAL) {
        Ok(lines) => lines,
        Err(StoreError::NoRevision(..)) => Vec::new(),
        Err(e) => return Err(e),
    };
    Ok(Read {
        lines,
        terms: db.session_terms(session, installed)?,
        proposals: db.proposals(session)?,
    })
}

/// The lookup's terms from a session's vocabulary, and where each came
/// from: a term two packs offer is one term, from the first pack in the
/// session's order, with both packs' aliases; a term only the learned
/// pairs give is a term of its own. A span the user accepted for a term
/// is one of the term's aliases.
fn vocabulary(terms: &SessionTerms, language: &str) -> (Vec<Term>, BTreeMap<String, Origin>) {
    let mut order: Vec<Term> = Vec::new();
    let mut origins: BTreeMap<String, Origin> = BTreeMap::new();
    for offered in &terms.terms {
        origins.entry(offered.term.clone()).or_insert_with(|| {
            let pack = &offered.pack;
            Origin::Pack {
                name: pack.name().to_owned(),
                version: pack.version().to_owned(),
                digest: pack.digest(),
            }
        });
        order.push(Term {
            term: offered.term.clone(),
            aliases: offered.aliases.clone(),
            language: offered.language.clone(),
        });
    }
    for (heard, term) in terms.learned.accepted() {
        origins.entry(term.to_owned()).or_insert(Origin::Learned);
        order.push(Term {
            term: term.to_owned(),
            aliases: vec![heard.to_owned()],
            language: language.to_owned(),
        });
    }
    (order, origins)
}

/// How the lookup found a candidate, as a proposal records it.
fn found(matched: Matched) -> nota_store::Found {
    match matched {
        Matched::Alias => nota_store::Found::Alias,
        Matched::Scored(score) => nota_store::Found::Scored {
            score: score.total,
            sounds: score.sounds,
            trigrams: score.trigrams,
            jaro_winkler: score.jaro_winkler,
            rare: score.bar == Bar::Rare,
        },
    }
}

/// The candidates the lookup gave in `chunk`, as offers. The lookup's
/// spans are always inside the chunk's words.
fn offers(chunk: &Chunk, found_in: Vec<Found>, origins: &BTreeMap<String, Origin>) -> Vec<Offer> {
    found_in
        .into_iter()
        .filter_map(|c| {
            let words = chunk.words(&c.span())?;
            Some(Offer {
                window: c.window().to_owned(),
                span: c.span(),
                words,
                term: c.term().to_owned(),
                found: found(c.matched()),
                origin: origins.get(c.term()).cloned().unwrap_or(Origin::Learned),
            })
        })
        .collect()
}

/// What earlier runs hold in each chunk: the candidates proposed already,
/// and the words no new fix may change.
#[derive(Debug, Default)]
struct Held {
    /// Each proposal's chunk, offered words, heard span and term.
    proposed: BTreeSet<(ChunkKey, WordSpan, String, String)>,
    /// The words a review or an applied fix holds, by chunk.
    blocked: BTreeMap<ChunkKey, Vec<WordSpan>>,
}

impl Held {
    /// What `proposals` hold.
    fn new(proposals: &[Proposal]) -> Self {
        let mut held = Self::default();
        for p in proposals {
            let at = &p.proposed;
            held.proposed
                .insert((at.chunk, at.offered, at.heard.clone(), at.term.clone()));
            if p.reviewed() || matches!(p.outcome, Outcome::Applied(_)) {
                held.blocked.entry(at.chunk).or_default().push(at.words);
            }
        }
        held
    }

    /// The words held in `chunk`.
    fn blocked(&self, chunk: ChunkKey) -> &[WordSpan] {
        self.blocked.get(&chunk).map_or(&[], Vec::as_slice)
    }

    /// Whether `offer` in `chunk` is new: not proposed before, and in no
    /// held words.
    fn is_new(&self, chunk: ChunkKey, offer: &Offer) -> bool {
        let key = (chunk, offer.words, offer.window.clone(), offer.term.clone());
        !self.proposed.contains(&key)
            && !self.blocked(chunk).iter().any(|b| b.overlaps(offer.words))
    }
}

/// One run over a session.
struct Pass<'a> {
    /// The library.
    db: &'a Writer,
    /// The session.
    session: SessionId,
    /// The language's rules, for the lookup.
    english: &'a English,
    /// What every proposal records about the run.
    context: Context,
    /// The runner's stop, progress and engine notes.
    running: &'a Running<'a>,
}

/// How a chunk's question went.
enum Asked {
    /// Answered, or no answer to read: the chunk's proposals are stored.
    Settled {
        /// Whether the decider answered, rather than giving an answer
        /// that couldn't be read or not being asked.
        answered: bool,
    },
    /// The run must end here, as given.
    Ended(JobEnd),
}

impl Pass<'_> {
    /// Proposes, decides and gates each chunk, then places the run's
    /// proposals.
    fn run(&self, read: &Read, decider: Box<dyn Decider + Send>, rate: SampleRate) -> JobEnd {
        if let Some(end) = self.ask_each(read, decider) {
            return end;
        }
        // A stop now leaves the placing to the next run, which places what
        // this one committed.
        if (self.running.stop)() {
            return JobEnd::Waiting(None);
        }
        self.place(&read.lines, rate)
    }

    /// Proposes, decides and gates each chunk in turn, storing each one's
    /// proposals, and drops the decider once done. Gives how the run ends
    /// if it must end before placing.
    fn ask_each(&self, read: &Read, mut decider: Box<dyn Decider + Send>) -> Option<JobEnd> {
        let (terms, origins) = vocabulary(&read.terms, self.english.code());
        let lookup = Lookup::new(self.english, &terms);
        let allows = |heard: &str, term: &str| read.terms.allows(heard, term);
        let held = Held::new(&read.proposals);
        let chunks: Vec<Chunk> = read.lines.iter().filter_map(chunk_of).collect();
        let total = u64::try_from(chunks.len()).unwrap_or(u64::MAX);
        let mut answered = false;
        for (i, chunk) in chunks.iter().enumerate() {
            let done = u64::try_from(i).unwrap_or(u64::MAX);
            (self.running.progress)(nota_store::Progress { done, total });
            let offered = offers(chunk, lookup.find(chunk.text(), &allows), &origins);
            let asked: Vec<Offer> = offered
                .iter()
                .filter(|o| held.is_new(chunk.key(), o))
                .cloned()
                .collect();
            if asked.is_empty() {
                continue;
            }
            // Checked before each question too: the decider sees the stop
            // only while it answers, and returning drops it.
            if (self.running.stop)() {
                return Some(JobEnd::Waiting(None));
            }
            let previous = i.checked_sub(1).and_then(|p| chunks.get(p));
            let ask = Ask {
                chunk,
                previous,
                offered: &offered,
                asked: &asked,
                blocked: held.blocked(chunk.key()),
            };
            match self.ask(&ask, &mut *decider) {
                Asked::Settled { answered: true } if !answered => {
                    // The decider works: any reason shown for an earlier
                    // failure goes.
                    (self.running.engine_down)(None);
                    answered = true;
                }
                Asked::Settled { .. } => {}
                Asked::Ended(end) => return Some(end),
            }
        }
        (self.running.progress)(nota_store::Progress { done: total, total });
        drop(decider);
        None
    }

    /// Asks the decider about `ask`'s candidates and stores what it
    /// makes of them.
    fn ask(&self, ask: &Ask<'_>, decider: &mut dyn Decider) -> Asked {
        let answers = match question(ask) {
            Ok(question) => match decider.decide(&question, self.running.stop) {
                Ok(decisions) => Answers::Given(decisions.iter().map(|(_, d)| *d).collect()),
                Err(DecideError::Stopped) => return Asked::Ended(JobEnd::Waiting(None)),
                Err(e @ DecideError::Unavailable(_)) => {
                    return Asked::Ended(JobEnd::EngineDown(e.to_string()));
                }
                Err(e @ DecideError::Failed(_)) => Answers::None(e.to_string()),
            },
            Err(why) => Answers::None(why),
        };
        let proposals = chunk::settle(
            ask.chunk,
            ask.offered,
            ask.asked,
            &answers,
            ask.blocked,
            &self.context,
        );
        match self
            .db
            .with(|db| db.add_proposals(self.session, &proposals))
        {
            Ok(()) => Asked::Settled {
                answered: matches!(answers, Answers::Given(_)),
            },
            Err(e) => Asked::Ended(stored(&e)),
        }
    }

    /// Ranks the cards and writes the applied fixes, from every proposal
    /// the session has.
    fn place(&self, lines: &[FinalLine], rate: SampleRate) -> JobEnd {
        let placed = self.db.with(|db| {
            let proposals = db.proposals(self.session)?;
            let placing = cards::placing(&proposals, recorded(lines, rate));
            db.place_proposals(self.session, &placing)
        });
        match placed {
            Ok(_) => JobEnd::Done,
            Err(e) => stored(&e),
        }
    }
}

/// One chunk's question, and what settling it needs.
struct Ask<'a> {
    /// The chunk.
    chunk: &'a Chunk,
    /// The chunk before it, for context.
    previous: Option<&'a Chunk>,
    /// Every candidate the lookup offered in it.
    offered: &'a [Offer],
    /// The new ones, which are asked about.
    asked: &'a [Offer],
    /// The words no new fix may change.
    blocked: &'a [WordSpan],
}

/// The decider's question for `ask`, or why it can't be asked (a span or
/// chunk longer than a question holds).
fn question(ask: &Ask<'_>) -> Result<Question, String> {
    let candidates = ask
        .asked
        .iter()
        .map(|o| Candidate::new(o.window.clone(), o.term.clone(), None))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("the question couldn't be asked: {e}"))?;
    Question::new(
        ask.previous.map(|c| c.text().to_owned()),
        ask.chunk.text().to_owned(),
        candidates,
    )
    .map_err(|e| format!("the question couldn't be asked: {e}"))
}

/// The chunk `line` holds, if its words are stored.
fn chunk_of(line: &FinalLine) -> Option<Chunk> {
    let words = line
        .heard
        .words
        .iter()
        .map(|w| (w.text.as_str(), w.log_prob));
    Chunk::new(line.heard.key(), words)
}

/// How long the recording is, for the card cap: the longest span of final
/// text on one track, at `rate`.
fn recorded(lines: &[FinalLine], rate: SampleRate) -> Duration {
    let mut spans: BTreeMap<TrackId, (SampleIndex, SampleIndex)> = BTreeMap::new();
    for line in lines {
        let range = line.heard.range;
        spans
            .entry(line.heard.track)
            .and_modify(|(start, end)| {
                *start = (*start).min(range.start());
                *end = (*end).max(range.end());
            })
            .or_insert((range.start(), range.end()));
    }
    spans
        .values()
        .filter_map(|(start, end)| end.saturating_count_since(*start).duration_at(rate))
        .max()
        .unwrap_or_default()
}
