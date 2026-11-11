//! Term clean-up's proposals: every candidate the lookup offered for a
//! session's final text, and what was decided on it.
//!
//! **One row per candidate, written once.** A proposal names a run of a
//! chunk's words in revision 0 (the final text, whose words never change),
//! the heard span and the term, and the text that would replace the
//! words. It records where the term came from, how the lookup found it,
//! the decider's answer and the gate's rule and score, so Review and
//! `nota bench` can say why each was applied or not
//! ([`Proposed`], [`Decided`]). What it records is never changed.
//!
//! **Its outcome moves once, at the run's end.** A proposal is stored
//! with what its chunk's decisions made of it: applied, rejected and
//! logged, or to be ranked (a yes the gate refused). The run's end
//! ([`Store::place_proposals`]) ranks those as "to check" cards or as over
//! the cap, and writes the applied ones' changes as one new revision, all
//! in one transaction, so a run killed before it leaves its proposals to
//! be placed by the next. A review, by the user, is set once
//! ([`Store::review_proposal`]). The schema's triggers (version 12) refuse
//! anything else.

use nota_core::decide::{Place, Probability};
use nota_core::{LogProb, SampleIndex, SessionId};
use rusqlite::{Connection, TransactionBehavior, params};

use crate::packs::PackDigest;
use crate::revisions::{Change, ChunkKey, RevisionNumber, WordSpan, insert_revision};
use crate::segments::parse_track;
use crate::transcript::{parse_log_prob, stored_log_prob};
use crate::{Store, StoreError, session_key};

/// The version of what a proposal row records, so a later reader can tell
/// how an older row was decided.
pub const LOG_VERSION: i64 = 1;

/// A proposal's number. Only the store gives one out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProposalId(i64);

impl ProposalId {
    /// The number.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Where a proposal's term came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A pack the session chose.
    Pack {
        /// The pack's name.
        name: String,
        /// Its version.
        version: String,
        /// Its digest, which tells this build from another of the same
        /// version.
        digest: PackDigest,
    },
    /// The learned pairs: the user accepted this term for a heard span,
    /// and no pack of the session's has it.
    Learned,
}

/// How the lookup found a proposal's term for its heard span.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Found {
    /// The span is one of the term's aliases.
    Alias,
    /// The span sounds and looks like the term.
    Scored {
        /// The weighted score, from 0 to 1.
        score: f64,
        /// How alike they sound, from 0 to 1.
        sounds: f64,
        /// Letter-trigram Jaccard, from 0 to 1.
        trigrams: f64,
        /// Jaro-Winkler, from 0 to 1.
        jaro_winkler: f64,
        /// Whether it cleared the lower bar for a nearly unknown word.
        rare: bool,
    },
}

/// The decider that answered, and its model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecidedBy {
    /// The decider (`llama-server`, `claude -p`).
    pub name: String,
    /// Its model.
    pub model: String,
    /// Where it runs.
    pub place: Place,
}

/// The rule the gate refused a yes by (C2's `Rejection`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Refusal {
    /// The span has no letters or digits.
    EmptySpan,
    /// The span isn't in the chunk on word boundaries.
    SpanNotFound,
    /// A side is longer than C1 compares.
    TooLong,
    /// C1 is below its threshold.
    Dissimilar,
    /// No offered term is inside the replacement.
    NotOffered,
    /// No candidate with the term shares a word with the span.
    WindowElsewhere,
    /// Each match of the span overlaps a fix already placed.
    AlreadyChanged,
}

impl Refusal {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EmptySpan => "empty-span",
            Self::SpanNotFound => "span-not-found",
            Self::TooLong => "too-long",
            Self::Dissimilar => "dissimilar",
            Self::NotOffered => "not-offered",
            Self::WindowElsewhere => "window-elsewhere",
            Self::AlreadyChanged => "already-changed",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "empty-span" => Self::EmptySpan,
            "span-not-found" => Self::SpanNotFound,
            "too-long" => Self::TooLong,
            "dissimilar" => Self::Dissimilar,
            "not-offered" => Self::NotOffered,
            "window-elsewhere" => Self::WindowElsewhere,
            "already-changed" => Self::AlreadyChanged,
            _ => return None,
        })
    }
}

/// What was decided on a candidate: the decider's answer, the gate's
/// verdict on a yes, and the outcome they make. Each variant is one of
/// the three outcomes, so a proposal has exactly one.
#[derive(Debug, Clone, PartialEq)]
pub enum Decided {
    /// A yes the gate let through: applied as a change.
    Applied {
        /// How likely the decider thought a yes was, if it said.
        yes: Option<Probability>,
        /// C1's score.
        similarity: f64,
    },
    /// A yes the gate let through whose replacement is the words as they
    /// are: there's nothing to change, so it's rejected and logged.
    Unchanged {
        /// How likely the decider thought a yes was, if it said.
        yes: Option<Probability>,
        /// C1's score.
        similarity: f64,
    },
    /// A yes the gate let through, but for words that are already
    /// reviewed or changed (it found an earlier occurrence than the
    /// lookup offered): rejected and logged, so a reviewed span is never
    /// changed.
    Covered {
        /// How likely the decider thought a yes was, if it said.
        yes: Option<Probability>,
        /// C1's score.
        similarity: f64,
    },
    /// A yes the gate refused: a "to check" card, if it ranks within the
    /// cap, or logged.
    Refused {
        /// How likely the decider thought a yes was, if it said.
        yes: Option<Probability>,
        /// The rule that refused it.
        rule: Refusal,
        /// C1's score, if the gate got as far as comparing.
        similarity: Option<f64>,
    },
    /// The decider said no: rejected and logged.
    No {
        /// How likely the decider thought a yes was, if it said.
        yes: Option<Probability>,
    },
    /// The decider gave no answer on it (a reply that couldn't be read,
    /// or a question that couldn't be asked), for the reason given: rejected
    /// and logged, never applied.
    NoAnswer(String),
}

/// What a proposal records about its candidate, apart from the decision.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposed {
    /// The chunk of the final text.
    pub chunk: ChunkKey,
    /// Its words the outcome is for: where an applied fix landed (which
    /// can be an earlier occurrence of the heard span than the one
    /// offered), with any other fix in the same words; otherwise where the
    /// lookup offered it.
    pub words: WordSpan,
    /// Its words where the lookup offered the term, which tell a
    /// candidate proposed already from a new one.
    pub offered: WordSpan,
    /// The heard span, as the lookup gave it.
    pub heard: String,
    /// The term offered for it.
    pub term: String,
    /// What the words would read as with the term in place.
    pub replacement: String,
    /// Where the term came from.
    pub origin: Origin,
    /// How the lookup found it.
    pub found: Found,
    /// The decider that was asked.
    pub decided_by: DecidedBy,
    /// The lowest log-probability among the words, if any is known.
    pub log_prob: Option<LogProb>,
    /// The thresholds in force, as `name=value` pairs.
    pub thresholds: String,
}

/// A proposal to store: what it records, and what was decided.
#[derive(Debug, Clone, PartialEq)]
pub struct NewProposal {
    /// What it records about its candidate.
    pub proposed: Proposed,
    /// What was decided.
    pub decided: Decided,
}

/// Where a proposal's outcome stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Applied; by the revision given, once the run's end has written it.
    Applied(Option<RevisionNumber>),
    /// Rejected and logged.
    Rejected,
    /// A yes the gate refused, not ranked yet.
    ToRank,
    /// A "to check" card.
    Card,
    /// A yes the gate refused that ranked past the card cap: logged.
    OverCap,
}

/// The user's review of a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reviewed {
    /// Kept: the fix, or the card's term.
    Accepted,
    /// Turned down.
    Rejected,
}

impl Reviewed {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

/// A stored proposal.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    /// Its number; proposals are numbered in the order they're stored.
    pub id: ProposalId,
    /// What it records.
    pub proposed: Proposed,
    /// What was decided.
    pub decided: Decided,
    /// Where its outcome stands.
    pub outcome: Outcome,
    /// The user's review, if there's been one.
    pub review: Option<Reviewed>,
}

impl Proposal {
    /// Whether the user has reviewed it.
    #[must_use]
    pub const fn reviewed(&self) -> bool {
        self.review.is_some()
    }
}

/// What the end of a clean-up run writes, for [`Store::place_proposals`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placing {
    /// Yeses the gate refused that become cards.
    pub cards: Vec<ProposalId>,
    /// Yeses the gate refused that ranked past the cap.
    pub over_cap: Vec<ProposalId>,
    /// The changes the applied proposals make, none sharing a word.
    pub changes: Vec<Change>,
    /// The applied proposals those changes come from.
    pub applied: Vec<ProposalId>,
}

/// The columns [`Decided`] is stored in: answer, why there's no answer,
/// yes probability, gate, similarity, outcome and rule.
type DecidedColumns = (
    &'static str,
    Option<String>,
    Option<f64>,
    Option<&'static str>,
    Option<f64>,
    &'static str,
    Option<&'static str>,
);

/// How `decided` is stored.
fn decided_columns(decided: &Decided) -> DecidedColumns {
    let p = |yes: &Option<Probability>| yes.map(Probability::get);
    match decided {
        Decided::Applied { yes, similarity } => (
            "yes",
            None,
            p(yes),
            Some("applied"),
            Some(*similarity),
            "applied",
            None,
        ),
        Decided::Unchanged { yes, similarity } => (
            "yes",
            None,
            p(yes),
            Some("applied"),
            Some(*similarity),
            "rejected",
            Some("no-change"),
        ),
        Decided::Covered { yes, similarity } => (
            "yes",
            None,
            p(yes),
            Some("applied"),
            Some(*similarity),
            "rejected",
            Some("covered"),
        ),
        Decided::Refused {
            yes,
            rule,
            similarity,
        } => (
            "yes",
            None,
            p(yes),
            Some(rule.as_str()),
            *similarity,
            "to-rank",
            None,
        ),
        Decided::No { yes } => ("no", None, p(yes), None, None, "rejected", Some("no")),
        Decided::NoAnswer(why) => (
            "none",
            Some(why.clone()),
            None,
            None,
            None,
            "rejected",
            Some("no-answer"),
        ),
    }
}

/// The columns [`Found`] is stored in: how, score, sounds, trigrams,
/// Jaro-Winkler and bar.
type FoundColumns = (
    &'static str,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<&'static str>,
);

/// How `found` is stored.
fn found_columns(found: Found) -> FoundColumns {
    match found {
        Found::Alias => ("alias", None, None, None, None, None),
        Found::Scored {
            score,
            sounds,
            trigrams,
            jaro_winkler,
            rare,
        } => (
            "scored",
            Some(score),
            Some(sounds),
            Some(trigrams),
            Some(jaro_winkler),
            Some(if rare { "rare" } else { "usual" }),
        ),
    }
}

/// How a place is stored.
const fn place_str(place: Place) -> &'static str {
    match place {
        Place::Local => "local",
        Place::Cloud => "cloud",
    }
}

/// Stores one proposal for session `key`.
fn insert(conn: &Connection, key: i64, new: &NewProposal) -> Result<(), StoreError> {
    let p = &new.proposed;
    let (answer, no_answer, yes, gate, similarity, outcome, rule) = decided_columns(&new.decided);
    let (matched, score, sounds, trigrams, jaro_winkler, bar) = found_columns(p.found);
    let (pack_name, pack_version, pack_digest) = match &p.origin {
        Origin::Pack {
            name,
            version,
            digest,
        } => (
            Some(name.as_str()),
            Some(version.as_str()),
            Some(digest.as_bytes().as_slice()),
        ),
        Origin::Learned => (None, None, None),
    };
    conn.execute(
        "INSERT INTO proposal (session_id, revision, track, start_sample, first_word, last_word, \
         offered_first, offered_last, heard, term, replacement, source, pack_name, \
         pack_version, pack_digest, matched, score, sounds, trigrams, jaro_winkler, bar, \
         decider, decider_model, decider_place, answer, no_answer, yes_probability, gate, \
         similarity, log_prob, outcome, rule, thresholds, log_version) VALUES (?1, 0, ?2, ?3, \
         ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'lookup', ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, \
         ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32)",
        params![
            key,
            i64::from(p.chunk.track.get()),
            crate::final_text::sample(p.chunk.start)?,
            i64::from(p.words.first()),
            i64::from(p.words.last()),
            i64::from(p.offered.first()),
            i64::from(p.offered.last()),
            p.heard,
            p.term,
            p.replacement,
            pack_name,
            pack_version,
            pack_digest,
            matched,
            score,
            sounds,
            trigrams,
            jaro_winkler,
            bar,
            p.decided_by.name,
            p.decided_by.model,
            place_str(p.decided_by.place),
            answer,
            no_answer,
            yes,
            gate,
            similarity,
            stored_log_prob(p.log_prob),
            outcome,
            rule,
            p.thresholds,
            LOG_VERSION,
        ],
    )?;
    Ok(())
}

/// The columns read back for a proposal, in order.
const COLUMNS: &str = "id, track, start_sample, first_word, last_word, heard, term, \
    replacement, pack_name, pack_version, pack_digest, matched, score, sounds, trigrams, \
    jaro_winkler, bar, decider, decider_model, decider_place, answer, no_answer, \
    yes_probability, gate, similarity, log_prob, outcome, rule, applied_in, review, thresholds, \
    offered_first, offered_last";

/// A stored proposal's row, read column by column as [`COLUMNS`] lists
/// them, and parsed.
fn parse_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Proposal, StoreError>> {
    let id: i64 = r.get(0)?;
    let corrupt = |what: &str| StoreError::Corrupt(format!("proposal {id}: {what}"));
    let track: i64 = r.get(1)?;
    let start: i64 = r.get(2)?;
    let (first, last): (i64, i64) = (r.get(3)?, r.get(4)?);
    let (offered_first, offered_last): (i64, i64) = (r.get(31)?, r.get(32)?);
    let origin = match (
        r.get::<_, Option<String>>(8)?,
        r.get::<_, Option<String>>(9)?,
        r.get::<_, Option<Vec<u8>>>(10)?,
    ) {
        (Some(name), Some(version), Some(digest)) => match PackDigest::from_stored(&digest) {
            Some(digest) => Origin::Pack {
                name,
                version,
                digest,
            },
            None => return Ok(Err(corrupt("a pack digest that isn't 32 bytes"))),
        },
        (None, None, None) => Origin::Learned,
        _ => return Ok(Err(corrupt("part of a pack"))),
    };
    let found = match (
        r.get::<_, String>(11)?.as_str(),
        r.get::<_, Option<String>>(16)?,
    ) {
        ("alias", None) => Found::Alias,
        ("scored", Some(bar)) => Found::Scored {
            score: r.get(12)?,
            sounds: r.get(13)?,
            trigrams: r.get(14)?,
            jaro_winkler: r.get(15)?,
            rare: bar == "rare",
        },
        _ => return Ok(Err(corrupt("how its term was found"))),
    };
    let place = match r.get::<_, String>(19)?.as_str() {
        "local" => Place::Local,
        "cloud" => Place::Cloud,
        _ => return Ok(Err(corrupt("where its decider runs"))),
    };
    let raw = RawDecision {
        answer: r.get(20)?,
        no_answer: r.get(21)?,
        yes: r.get(22)?,
        gate: r.get(23)?,
        similarity: r.get(24)?,
        outcome: r.get(26)?,
        rule: r.get(27)?,
        applied_in: r.get(28)?,
    };
    let review = match r.get::<_, Option<String>>(29)?.as_deref() {
        None => None,
        Some("accepted") => Some(Reviewed::Accepted),
        Some("rejected") => Some(Reviewed::Rejected),
        Some(_) => return Ok(Err(corrupt("its review"))),
    };
    let parsed = (|| {
        let span = |first: i64, last: i64| {
            u32::try_from(first)
                .ok()
                .zip(u32::try_from(last).ok())
                .and_then(|(first, last)| WordSpan::new(first, last))
                .ok_or_else(|| corrupt("its words"))
        };
        let words = span(first, last)?;
        let offered = span(offered_first, offered_last)?;
        let start = u64::try_from(start).map_err(|_| corrupt("its chunk"))?;
        let (decided, outcome) = raw.parse().ok_or_else(|| corrupt("its decision"))?;
        Ok(Proposal {
            id: ProposalId(id),
            proposed: Proposed {
                chunk: ChunkKey {
                    track: parse_track(track)?,
                    start: SampleIndex::new(start),
                },
                words,
                offered,
                heard: r.get(5)?,
                term: r.get(6)?,
                replacement: r.get(7)?,
                origin,
                found,
                decided_by: DecidedBy {
                    name: r.get(17)?,
                    model: r.get(18)?,
                    place,
                },
                log_prob: parse_log_prob(r.get(25)?)?,
                thresholds: r.get(30)?,
            },
            decided,
            outcome,
            review,
        })
    })();
    Ok(parsed)
}

/// A proposal's decision as its columns hold it.
struct RawDecision {
    /// `yes`, `no` or `none`.
    answer: String,
    /// Why there's no answer.
    no_answer: Option<String>,
    /// The yes probability.
    yes: Option<f64>,
    /// The gate's rule, for a yes.
    gate: Option<String>,
    /// C1's score.
    similarity: Option<f64>,
    /// Where the outcome stands.
    outcome: String,
    /// Why it was rejected.
    rule: Option<String>,
    /// The revision that applied it.
    applied_in: Option<i64>,
}

impl RawDecision {
    /// The decision and outcome, or `None` if the columns don't make one.
    fn parse(self) -> Option<(Decided, Outcome)> {
        let yes = match self.yes {
            Some(p) => Some(Probability::new(p)?),
            None => None,
        };
        let applied_in = match self.applied_in {
            Some(n) => Some(RevisionNumber::stored(u32::try_from(n).ok()?)),
            None => None,
        };
        let gate = self.gate.as_deref();
        let decided = match (self.answer.as_str(), gate, self.rule.as_deref()) {
            ("yes", Some("applied"), None) => Decided::Applied {
                yes,
                similarity: self.similarity?,
            },
            ("yes", Some("applied"), Some("no-change")) => Decided::Unchanged {
                yes,
                similarity: self.similarity?,
            },
            ("yes", Some("applied"), Some("covered")) => Decided::Covered {
                yes,
                similarity: self.similarity?,
            },
            ("yes", Some(rule), None) => Decided::Refused {
                yes,
                rule: Refusal::parse(rule)?,
                similarity: self.similarity,
            },
            ("no", None, Some("no")) => Decided::No { yes },
            ("none", None, Some("no-answer")) => Decided::NoAnswer(self.no_answer?),
            _ => return None,
        };
        let outcome = match (&decided, self.outcome.as_str()) {
            (Decided::Applied { .. }, "applied") => Outcome::Applied(applied_in),
            (Decided::Refused { .. }, "to-rank") => Outcome::ToRank,
            (Decided::Refused { .. }, "card") => Outcome::Card,
            (Decided::Refused { .. }, "over-cap") => Outcome::OverCap,
            (_, "rejected") => Outcome::Rejected,
            _ => return None,
        };
        Some((decided, outcome))
    }
}

/// Session `key`'s proposals, in number order.
fn read_all(conn: &Connection, key: i64) -> Result<Vec<Proposal>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM proposal WHERE session_id = ?1 ORDER BY id"
    ))?;
    let rows = stmt
        .query_map([key], parse_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().collect()
}

/// Moves proposal `id` of session `key` from `from` to `to`, or fails with
/// [`StoreError::NoProposal`] if it isn't the session's, or isn't there.
fn move_outcome(
    conn: &Connection,
    key: i64,
    id: ProposalId,
    from: &str,
    to: &str,
) -> Result<(), StoreError> {
    let changed = conn.execute(
        "UPDATE proposal SET outcome = ?4 WHERE id = ?1 AND session_id = ?2 AND outcome = ?3",
        params![id.0, key, from, to],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::NoProposal(id.0))
    }
}

impl Store {
    /// Stores `proposals` for `session`, in order, in one transaction
    /// committed before returning: a chunk's proposals are stored together
    /// or not at all.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoWords`] if a proposal names words its chunk doesn't
    /// have (or a chunk the session doesn't have), [`StoreError::OutOfRange`]
    /// if a number doesn't fit, and [`StoreError::Sqlite`] for any SQLite
    /// failure. Nothing is stored then.
    pub fn add_proposals(
        &mut self,
        session: SessionId,
        proposals: &[NewProposal],
    ) -> Result<(), StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for new in proposals {
            for words in [new.proposed.words, new.proposed.offered] {
                let change = Change {
                    chunk: new.proposed.chunk,
                    words,
                    text: String::new(),
                };
                if !crate::revisions::has_words(&tx, key, &change)? {
                    return Err(StoreError::NoWords {
                        session,
                        chunk: change.chunk,
                        words: change.words,
                    });
                }
            }
            insert(&tx, key, new)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The session's proposals, in the order they were stored.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn proposals(&self, session: SessionId) -> Result<Vec<Proposal>, StoreError> {
        read_all(&self.conn, session_key(session)?)
    }

    /// Writes the end of a clean-up run, in one transaction committed
    /// before returning: `placing`'s cards and over-cap proposals take
    /// those outcomes, and its changes become a new revision whose parent
    /// is the session's newest, which applies its applied proposals. With
    /// no changes, no revision is made. Gives the new revision, if any.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoProposal`] if a card or over-cap proposal isn't the
    /// session's to rank, or an applied one isn't the session's, applied
    /// and not yet written; [`StoreError::NoRevision`] if the session has
    /// no revision to be the parent; [`StoreError::NoWords`] if a change
    /// names words its chunk doesn't have; and [`StoreError::Sqlite`] for
    /// any SQLite failure, including two changes that share a word.
    /// Nothing changes then.
    pub fn place_proposals(
        &mut self,
        session: SessionId,
        placing: &Placing,
    ) -> Result<Option<RevisionNumber>, StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for &id in &placing.cards {
            move_outcome(&tx, key, id, "to-rank", "card")?;
        }
        for &id in &placing.over_cap {
            move_outcome(&tx, key, id, "to-rank", "over-cap")?;
        }
        let revision = if placing.changes.is_empty() {
            None
        } else {
            let newest: Option<i64> = tx.query_row(
                "SELECT max(number) FROM revision WHERE session_id = ?1",
                [key],
                |r| r.get(0),
            )?;
            let parent = newest
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(StoreError::NoRevision(session, 0))?;
            Some(insert_revision(
                &tx,
                session,
                RevisionNumber::stored(parent),
                &placing.changes,
            )?)
        };
        for &id in &placing.applied {
            let changed = tx.execute(
                "UPDATE proposal SET applied_in = ?3 WHERE id = ?1 AND session_id = ?2 \
                 AND outcome = 'applied' AND applied_in IS NULL",
                params![id.0, key, revision.map(|r| i64::from(r.get()))],
            )?;
            if changed != 1 || revision.is_none() {
                return Err(StoreError::NoProposal(id.0));
            }
        }
        tx.commit()?;
        Ok(revision)
    }

    /// Records the user's review of proposal `id`. A proposal is reviewed
    /// once.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoProposal`] if there's no such proposal, and
    /// [`StoreError::Sqlite`] for any SQLite failure, including one
    /// reviewed already.
    pub fn review_proposal(&mut self, id: ProposalId, review: Reviewed) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE proposal SET review = ?2 WHERE id = ?1",
            params![id.0, review.as_str()],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::NoProposal(id.0))
        }
    }
}

#[cfg(test)]
mod tests;
