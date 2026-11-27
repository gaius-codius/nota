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
//! **C3, the similarity pre-check** (off unless the settings turn it on,
//! until test 3 decides whether it's adopted, #168): a candidate C1 scores
//! below the gate's minimum is rejected and logged without being asked
//! about, unless a low log-probability marks its span (its words among
//! the recording's lowest [`LOW_LOG_PROB_PERCENT`]), since a yes on it
//! can still make a card. A chunk whose every new candidate is skipped is
//! no question at all. The gate still checks each yes against every
//! candidate the lookup offered, so skipping changes no other decision;
//! what it does change is the cards: a skipped yes is no card, which
//! leaves room under the cap for another (the code checks spec, C3). A
//! session's words with no log-probability mark nothing, so there it's a
//! plain pre-check. A skipped candidate is a proposal like any other, so
//! a later run doesn't ask it either, with the pre-check on or off.
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
//!
//! **A chunk the decider always fails on.** After a question finds the
//! decider down, clean-up asks it a fixed question of its own; if that is
//! answered, the failure is the chunk's. Such a chunk is asked after the
//! rest on later runs, and at its [`suspects::GIVE_UP`]th such failure its
//! candidates are logged as given up on and the run carries on
//! ([`suspects`]). So one bad chunk doesn't keep a session from its
//! revision and cards, and a decider that is down gives up on nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use nota_checks::{
    Bar, Candidate as Found, English, KEEP, KEEP_RARE, LOW_LOG_PROB_PERCENT, Language, Lookup,
    LowLogProb, MAX_CANDIDATES, Matched, PreCheck, SIMILARITY_MIN, Signal, Skipped, Term, precheck,
};
use nota_core::decide::{Candidate, DecideError, Decider, Question};
use nota_core::{LogProb, SampleIndex, SampleRate, SessionId, TrackId};
use nota_store::{
    ChunkKey, DecidedBy, FinalLine, JobEnd, JobKind, JobState, NewProposal, Origin, Outcome, Pack,
    Proposal, RevisionNumber, SessionTerms, StoreError, Wait, WordSpan, Writer,
};

use crate::jobs::Running;

pub(crate) mod cards;
pub(crate) mod chunk;
#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod sources;
pub(crate) mod suspects;

use chunk::{Answers, Chunk, Context, Offer};
use suspects::{Failed, Suspects, probe};

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

/// Whether a run puts its candidates through C3, the similarity
/// pre-check, before asking about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Precheck {
    /// Every new candidate is asked about: what nota does until test 3
    /// says otherwise.
    #[default]
    Off,
    /// A candidate C1 scores below the gate's minimum, with nothing else
    /// marking its span, isn't.
    On,
}

/// Where a run finds its decider and the installed packs.
pub(crate) trait Sources: Send {
    /// The decider, as the settings leave it now.
    fn decider(&self) -> Setup;

    /// Whether the settings turn the similarity pre-check on now.
    fn precheck(&self) -> Precheck;

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
    /// The chunks whose question found the decider down, across runs.
    suspects: Suspects,
}

impl CleanUp {
    /// Clean-up taking its decider and packs from `sources`, over text at
    /// `rate`.
    pub(crate) fn new(sources: Box<dyn Sources>, rate: SampleRate) -> Self {
        Self {
            sources,
            rate,
            english: None,
            suspects: Suspects::default(),
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
        let precheck = self.sources.precheck();
        self.suspects.asking(&by);
        let mut pass = Pass {
            db,
            session,
            english: &english,
            context: Context {
                decided_by: by,
                thresholds: thresholds(precheck),
            },
            running,
            screen: Screen::new(precheck, &read.lines),
            suspects: &mut self.suspects,
            answered: false,
        };
        let end = pass.run(&read, decider, self.rate);
        // A session whose clean-up has ended asks the decider nothing more.
        if matches!(end, JobEnd::Done | JobEnd::Failed(_)) {
            self.suspects.forget(session);
        }
        end
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

/// The thresholds a proposal records, as `name=value` pairs: the gate's,
/// the lookup's, the card cap's, and whether the pre-check ran (with its
/// low log-probability share).
pub(crate) fn thresholds(precheck: Precheck) -> String {
    let precheck = match precheck {
        Precheck::Off => "precheck=off".to_owned(),
        Precheck::On => format!("precheck=on;low_log_prob_percent={LOW_LOG_PROB_PERCENT}"),
    };
    format!(
        "similarity_min={SIMILARITY_MIN};keep={KEEP};keep_rare={KEEP_RARE};\
         max_candidates={MAX_CANDIDATES};card_every_s={};{precheck}",
        cards::CARD_EVERY.as_secs()
    )
}

/// How a run sorts its new candidates before asking: C3 on or off, and
/// the recording's low log-probability band it reads signals from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Screen {
    /// Whether C3 runs.
    precheck: Precheck,
    /// The recording's lowest log-probabilities, over every word of its
    /// final text on both tracks; none if too few words have one.
    low: Option<LowLogProb>,
}

impl Screen {
    /// The screen for a run over `lines` with `precheck`.
    pub(crate) fn new(precheck: Precheck, lines: &[FinalLine]) -> Self {
        let known = lines
            .iter()
            .flat_map(|line| &line.heard.words)
            .filter_map(|word| word.log_prob)
            .map(LogProb::get);
        let low = match precheck {
            Precheck::Off => None,
            Precheck::On => LowLogProb::of(known),
        };
        Self { precheck, low }
    }

    /// `new` offers in `chunk`, split into those to ask about and those
    /// C3 skips (none while it's off), each in the order given.
    pub(crate) fn sort(
        &self,
        chunk: &Chunk,
        new: Vec<Offer>,
    ) -> (Vec<Offer>, Vec<(Offer, Skipped)>) {
        if self.precheck == Precheck::Off {
            return (new, Vec::new());
        }
        let mut asked = Vec::new();
        let mut skipped = Vec::new();
        for offer in new {
            match precheck(&offer.window, &offer.term, self.signal(chunk, &offer)) {
                PreCheck::Ask(_) => asked.push(offer),
                PreCheck::Skip(skip) => skipped.push((offer, skip)),
            }
        }
        (asked, skipped)
    }

    /// What marks `offer`'s span in `chunk` as suspect, if anything: one
    /// of its words in the recording's low band.
    fn signal(&self, chunk: &Chunk, offer: &Offer) -> Option<Signal> {
        let low = self.low?;
        let lowest = chunk.lowest_log_prob(offer.words)?;
        low.marks(lowest.get()).then_some(Signal::LowLogProb)
    }
}

/// What a run reads before it starts.
pub(crate) struct Read {
    /// Revision 0, in session order.
    pub(crate) lines: Vec<FinalLine>,
    /// The session's vocabulary.
    pub(crate) terms: SessionTerms,
    /// The proposals made already.
    pub(crate) proposals: Vec<Proposal>,
}

/// Reads `session`'s final text, vocabulary (from `installed` packs) and
/// proposals. A session with no final text has no lines.
pub(crate) fn read(
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

/// A chunk of the final text and every candidate the lookup offers in
/// it.
pub(crate) type Offering = (Chunk, Vec<Offer>);

/// The chunks of `lines` with their words stored, in session order, each
/// with what the lookup offers in it from the session's vocabulary
/// `terms`, in `english`.
pub(crate) fn offering(
    english: &English,
    terms: &SessionTerms,
    lines: &[FinalLine],
) -> Vec<Offering> {
    let (vocab, origins) = vocabulary(terms, english.code());
    let chunks = lines.iter().filter_map(chunk_of).collect();
    offering_in(
        english,
        &vocab,
        &origins,
        &|h, t| terms.allows(h, t),
        chunks,
    )
}

/// Each of `chunks` with what the lookup offers in it from `vocab`, as far
/// as `allows` lets it; `origins` says where each term came from.
pub(crate) fn offering_in(
    english: &English,
    vocab: &[Term],
    origins: &BTreeMap<String, Origin>,
    allows: &dyn Fn(&str, &str) -> bool,
    chunks: Vec<Chunk>,
) -> Vec<Offering> {
    let lookup = Lookup::new(english, vocab);
    chunks
        .into_iter()
        .map(|chunk| {
            let offered = offers(&chunk, lookup.find(chunk.text(), allows), origins);
            (chunk, offered)
        })
        .collect()
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

/// What C3 saved a session's runs so far, counted from its proposals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Saved {
    /// Candidates it skipped, each rejected and logged without a call.
    pub(crate) candidates: usize,
    /// Questions it saved: chunks whose every proposal it skipped, so no
    /// question was ever put about them. A chunk where it skipped some
    /// candidates but not all was still asked, about fewer.
    pub(crate) questions: usize,
}

/// What C3 saved the session whose proposals are `proposals`.
pub(crate) fn saved(proposals: &[Proposal]) -> Saved {
    let mut chunks: BTreeMap<ChunkKey, bool> = BTreeMap::new();
    let mut candidates = 0;
    for p in proposals {
        let skipped = matches!(p.decided, nota_store::Decided::Skipped { .. });
        candidates += usize::from(skipped);
        *chunks.entry(p.proposed.chunk).or_insert(true) &= skipped;
    }
    Saved {
        candidates,
        questions: chunks.values().filter(|all| **all).count(),
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
    /// Which candidates are asked about.
    screen: Screen,
    /// The chunks whose question found the decider down, across runs.
    suspects: &'a mut Suspects,
    /// Whether the decider has answered in this run, and the runner been
    /// told.
    answered: bool,
}

/// How a chunk's question went.
enum Asked {
    /// Answered, or no answer to read: the chunk's proposals are stored.
    Settled,
    /// The run must end here, as given.
    Ended(JobEnd),
}

/// What the probe after a chunk's failure showed.
enum Probed {
    /// The decider answered it: the failure was the chunk's.
    Works,
    /// It failed too: the decider is down.
    Down,
    /// A stop came while it was asked.
    Stopped,
}

impl Pass<'_> {
    /// Proposes, decides and gates each chunk, then places the run's
    /// proposals.
    fn run(&mut self, read: &Read, decider: Box<dyn Decider + Send>, rate: SampleRate) -> JobEnd {
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

    /// Proposes, decides and gates each chunk in turn, the suspects last,
    /// storing each one's proposals, and drops the decider once done.
    /// Gives how the run ends if it must end before placing.
    fn ask_each(&mut self, read: &Read, mut decider: Box<dyn Decider + Send>) -> Option<JobEnd> {
        let held = Held::new(&read.proposals);
        let offering = offering(self.english, &read.terms, &read.lines);
        let total = u64::try_from(offering.len()).unwrap_or(u64::MAX);
        for (n, i) in self.order(&offering).into_iter().enumerate() {
            let Some((chunk, offered)) = offering.get(i) else {
                continue;
            };
            let done = u64::try_from(n).unwrap_or(u64::MAX);
            (self.running.progress)(nota_store::Progress { done, total });
            let new: Vec<Offer> = offered
                .iter()
                .filter(|o| held.is_new(chunk.key(), o))
                .cloned()
                .collect();
            if new.is_empty() {
                continue;
            }
            let (asked, skipped) = self.screen.sort(chunk, new);
            // Checked before each question too: the decider sees the stop
            // only while it answers, and returning drops it.
            if (self.running.stop)() {
                return Some(JobEnd::Waiting(None));
            }
            let previous = i
                .checked_sub(1)
                .and_then(|p| offering.get(p))
                .map(|(c, _)| c);
            let ask = Ask {
                chunk,
                previous,
                offered,
                asked: &asked,
                skipped: &skipped,
                blocked: held.blocked(chunk.key()),
            };
            match self.ask(&ask, &mut *decider) {
                Asked::Settled => self.suspects.settled(self.session, chunk.key()),
                Asked::Ended(end) => return Some(end),
            }
        }
        (self.running.progress)(nota_store::Progress { done: total, total });
        drop(decider);
        None
    }

    /// The chunks' positions in the order to ask them: session order,
    /// with the suspects after the rest, so the rest are asked even while
    /// a suspect keeps failing, and can show the decider works.
    fn order(&self, offering: &[Offering]) -> Vec<usize> {
        let (suspect, rest): (Vec<usize>, Vec<usize>) = (0..offering.len())
            .partition(|&i| self.suspects.has(self.session, offering[i].0.key()));
        rest.into_iter().chain(suspect).collect()
    }

    /// Notes that the decider answered: the first time in a run, the
    /// runner is told, so any reason shown for an earlier failure goes and
    /// the clean-up jobs that wait on the decider are tried again soon.
    fn note_answer(&mut self) {
        if !self.answered {
            (self.running.answered)();
            self.answered = true;
        }
    }

    /// Asks the decider about `ask`'s candidates left after the
    /// pre-check, if any, and stores what it makes of them with the
    /// skipped ones, together: a run stopped after it neither asks nor
    /// skips any of them again.
    fn ask(&mut self, ask: &Ask<'_>, decider: &mut dyn Decider) -> Asked {
        let answers = if ask.asked.is_empty() {
            None
        } else {
            match self.answers(ask, decider) {
                Ok(answers) => Some(answers),
                Err(end) => return Asked::Ended(end),
            }
        };
        let proposals = proposals(ask, answers.as_ref(), &self.context);
        match self
            .db
            .with(|db| db.add_proposals(self.session, &proposals))
        {
            Ok(()) => Asked::Settled,
            Err(e) => Asked::Ended(stored(&e)),
        }
    }

    /// The decider's answers on `ask`'s candidates, or how the run ends if
    /// it must end here. A decider that's down on this chunk but answers
    /// the probe has the failure counted as the chunk's, and once the
    /// count says so, the chunk is given up on.
    fn answers(&mut self, ask: &Ask<'_>, decider: &mut dyn Decider) -> Result<Answers, JobEnd> {
        let question = match question(ask) {
            Ok(question) => question,
            Err(why) => return Ok(Answers::None(why)),
        };
        match decider.decide(&question, self.running.stop) {
            Ok(decisions) => {
                // Told before the proposals are stored: the decider works
                // even if the store fails.
                self.note_answer();
                Ok(Answers::Given(decisions.iter().map(|(_, d)| *d).collect()))
            }
            Err(DecideError::Stopped) => Err(JobEnd::Waiting(None)),
            Err(e @ DecideError::Unavailable(_)) => match self.failed(ask.chunk, decider) {
                Ok(Failed::GiveUp) => Ok(Answers::GaveUp(e.to_string())),
                Ok(Failed::TryAgain) => Err(JobEnd::EngineDown(e.to_string())),
                Err(end) => Err(end.unwrap_or(JobEnd::EngineDown(e.to_string()))),
            },
            Err(e @ DecideError::Failed(_)) => Ok(Answers::None(e.to_string())),
        }
    }

    /// After `chunk`'s question found the decider down: asks the probe,
    /// and if the decider answers it, counts the failure as the chunk's.
    /// Gives what the count says to do, or `Err` if nothing counted: with
    /// how the run ends if a stop came, or `None` for the decider being
    /// down.
    fn failed(
        &mut self,
        chunk: &Chunk,
        decider: &mut dyn Decider,
    ) -> Result<Failed, Option<JobEnd>> {
        match self.probe(decider) {
            Probed::Works => Ok(self.suspects.failed(self.session, chunk.key())),
            Probed::Down => Err(None),
            Probed::Stopped => Err(Some(JobEnd::Waiting(None))),
        }
    }

    /// Asks the decider the probe question. An answer that can't be read
    /// still shows it works; what it says is never stored.
    fn probe(&mut self, decider: &mut dyn Decider) -> Probed {
        let Ok(question) = probe() else {
            return Probed::Down;
        };
        match decider.decide(&question, self.running.stop) {
            Ok(_) | Err(DecideError::Failed(_)) => {
                self.note_answer();
                Probed::Works
            }
            Err(DecideError::Unavailable(_)) => Probed::Down,
            Err(DecideError::Stopped) => Probed::Stopped,
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
pub(crate) struct Ask<'a> {
    /// The chunk.
    pub(crate) chunk: &'a Chunk,
    /// The chunk before it, for context.
    pub(crate) previous: Option<&'a Chunk>,
    /// Every candidate the lookup offered in it.
    pub(crate) offered: &'a [Offer],
    /// The new ones the pre-check left, which are asked about.
    pub(crate) asked: &'a [Offer],
    /// The new ones it skipped, with C1's score on each.
    pub(crate) skipped: &'a [(Offer, Skipped)],
    /// The words no new fix may change.
    pub(crate) blocked: &'a [WordSpan],
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

/// What a chunk's candidates come to, before anything is stored.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Settled {
    /// One proposal per candidate asked about, in order, then one per
    /// candidate the pre-check skipped.
    pub(crate) proposals: Vec<NewProposal>,
    /// Whether a question was put to the decider: not when the pre-check
    /// skipped every candidate.
    pub(crate) asked: bool,
}

/// Asks `decider` once about `ask`'s candidates left after the pre-check,
/// if any, until `stop` says to stop, and settles them and the skipped
/// ones as `context` records; or how the asking must end here. Stores
/// nothing, and tries nothing again: a decider that's down ends it, where
/// a clean-up run probes and counts the failure. `nota bench` asks this
/// way.
pub(crate) fn settle_chunk(
    ask: &Ask<'_>,
    decider: &mut dyn Decider,
    stop: &dyn Fn() -> bool,
    context: &Context,
) -> Result<Settled, JobEnd> {
    let asked = !ask.asked.is_empty();
    let answers = if asked {
        Some(answer_once(ask, decider, stop)?)
    } else {
        None
    };
    Ok(Settled {
        proposals: proposals(ask, answers.as_ref(), context),
        asked,
    })
}

/// The proposals for `ask`'s chunk as `context` records them: its asked
/// candidates settled with `answers` (none when none were asked), then
/// its skipped ones.
fn proposals(ask: &Ask<'_>, answers: Option<&Answers>, context: &Context) -> Vec<NewProposal> {
    let mut proposals = match answers {
        Some(answers) => chunk::settle(
            ask.chunk,
            ask.offered,
            ask.asked,
            answers,
            ask.blocked,
            context,
        ),
        None => Vec::new(),
    };
    proposals.extend(
        ask.skipped
            .iter()
            .map(|(offer, skip)| chunk::skipped(ask.chunk, offer, *skip, context)),
    );
    proposals
}

/// The decider's answers on `ask`'s candidates, asked once, or how the
/// asking ends if it must end here: stopped, or the decider down.
fn answer_once(
    ask: &Ask<'_>,
    decider: &mut dyn Decider,
    stop: &dyn Fn() -> bool,
) -> Result<Answers, JobEnd> {
    let question = match question(ask) {
        Ok(question) => question,
        Err(why) => return Ok(Answers::None(why)),
    };
    match decider.decide(&question, stop) {
        Ok(decisions) => Ok(Answers::Given(decisions.iter().map(|(_, d)| *d).collect())),
        Err(DecideError::Stopped) => Err(JobEnd::Waiting(None)),
        Err(e @ DecideError::Unavailable(_)) => Err(JobEnd::EngineDown(e.to_string())),
        Err(e @ DecideError::Failed(_)) => Ok(Answers::None(e.to_string())),
    }
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
