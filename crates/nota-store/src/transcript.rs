//! The heard text, and the revisions of the text that's shown.
//!
//! **The heard text is the record.** Each utterance the engine confirmed is
//! stored once, as it was heard: its track, its span in session time, its
//! text, its words with their times, and the engine and model that heard
//! it. Nothing here changes or deletes one, and the schema's triggers
//! (version 4) refuse an UPDATE, DELETE or replacing INSERT whatever
//! connection tries.
//!
//! **The shown text is a revision.** Revision 0 is the heard text itself,
//! and has no rows of its own. Every later revision is a new row naming
//! its parent, with a `revision_text` row for each utterance whose text it
//! changes; an utterance it doesn't change reads as in its parent, back to
//! the heard text. What a revision changes is never changed or added to
//! afterwards: a correction is a new revision. An utterance stored after a
//! revision was made reads in it as heard, since the revision didn't change
//! it. (The final pass's utterances, M2's GAI-317, will need telling apart
//! from the live ones before revisions show them.)

use std::collections::BTreeMap;

use nota_core::{SessionId, SessionTime, TrackId, Utterance};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::segments::{parse_track, session_exists};
use crate::{Store, StoreError, session_key};

/// An utterance's number in the library. Only the store gives one out, so
/// a number always names an utterance that was stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtteranceId(i64);

impl UtteranceId {
    /// The number.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// A revision of a session's text. Revision 0 is the heard text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RevisionNumber(u32);

impl RevisionNumber {
    /// The heard text.
    pub const HEARD: Self = Self(0);

    /// The number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One word of an utterance, and when it was said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    text: String,
    start: SessionTime,
    end: SessionTime,
}

impl Word {
    /// `text`, said from `start` to `end`. `None` if it ends before it
    /// starts.
    #[must_use]
    pub fn new(text: String, start: SessionTime, end: SessionTime) -> Option<Self> {
        (start <= end).then_some(Self { text, start, end })
    }

    /// The word.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// When it started.
    #[must_use]
    pub const fn start(&self) -> SessionTime {
        self.start
    }

    /// When it ended.
    #[must_use]
    pub const fn end(&self) -> SessionTime {
        self.end
    }
}

/// An utterance as heard, and what heard it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heard {
    /// What was heard, on which track, and when.
    pub utterance: Utterance,
    /// The engine that heard it, e.g. `sherpa-onnx`.
    pub engine: String,
    /// Its model, e.g. `parakeet-tdt-0.6b-v3-int8`.
    pub model: String,
    /// Its words with their times, in order; empty if the engine didn't
    /// give them.
    pub words: Vec<Word>,
}

/// A stored utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUtterance {
    /// Its number.
    pub id: UtteranceId,
    /// What was heard.
    pub heard: Heard,
}

/// One utterance as a revision shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The utterance.
    pub utterance: UtteranceId,
    /// Its track.
    pub track: TrackId,
    /// When it started.
    pub start: SessionTime,
    /// When it ended.
    pub end: SessionTime,
    /// Its text in the revision.
    pub text: String,
}

/// A session time as SQLite holds it.
fn nanos(at: SessionTime) -> Result<i64, StoreError> {
    i64::try_from(at.as_nanos()).map_err(|_| StoreError::OutOfRange)
}

/// A stored session time.
fn parse_nanos(at: i64) -> Result<SessionTime, StoreError> {
    u64::try_from(at)
        .map(SessionTime::from_nanos)
        .map_err(|_| StoreError::Corrupt(format!("session time {at} is negative")))
}

type RawUtterance = (i64, i64, i64, i64, String, String, String);

fn parse_utterance(
    (id, track, start, end, text, engine, model): RawUtterance,
    words: Vec<Word>,
) -> Result<StoredUtterance, StoreError> {
    let utterance = Utterance::new(
        parse_track(track)?,
        parse_nanos(start)?,
        parse_nanos(end)?,
        text,
    )
    .ok_or_else(|| StoreError::Corrupt(format!("utterance {id} ends before it starts")))?;
    Ok(StoredUtterance {
        id: UtteranceId(id),
        heard: Heard {
            utterance,
            engine,
            model,
            words,
        },
    })
}

/// Every word of the session's utterances, by utterance, in order.
fn words_of(conn: &Connection, key: i64) -> Result<BTreeMap<i64, Vec<Word>>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT w.utterance_id, w.text, w.start_ns, w.end_ns FROM word w \
         JOIN utterance u ON u.id = w.utterance_id \
         WHERE u.session_id = ?1 ORDER BY w.utterance_id, w.position",
    )?;
    let raws = stmt
        .query_map([key], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut words: BTreeMap<i64, Vec<Word>> = BTreeMap::new();
    for (utterance, text, start, end) in raws {
        let word = Word::new(text, parse_nanos(start)?, parse_nanos(end)?).ok_or_else(|| {
            StoreError::Corrupt(format!(
                "a word of utterance {utterance} ends before it starts"
            ))
        })?;
        words.entry(utterance).or_default().push(word);
    }
    Ok(words)
}

/// The utterance stored exactly as `heard` in the session, words and all,
/// if there is one.
fn same_utterance(conn: &Connection, key: i64, heard: &Heard) -> Result<Option<i64>, StoreError> {
    let u = &heard.utterance;
    let mut stmt = conn.prepare(
        "SELECT id FROM utterance WHERE session_id = ?1 AND track = ?2 \
         AND start_ns = ?3 AND end_ns = ?4 AND text = ?5 AND engine = ?6 AND model = ?7",
    )?;
    let ids = stmt
        .query_map(
            params![
                key,
                i64::from(u.track().get()),
                nanos(u.start())?,
                nanos(u.end())?,
                u.text(),
                heard.engine,
                heard.model
            ],
            |r| r.get::<_, i64>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Ok(None);
    }
    let mut words = words_of(conn, key)?;
    Ok(ids
        .into_iter()
        .find(|id| words.remove(id).unwrap_or_default() == heard.words))
}

/// Whether the session has revision `number`.
fn revision_exists(conn: &Connection, key: i64, number: u32) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM revision WHERE session_id = ?1 AND number = ?2",
            params![key, i64::from(number)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// [`Store::utterances`]' work, on `conn`.
fn utterances_in(conn: &Connection, key: i64) -> Result<Vec<StoredUtterance>, StoreError> {
    let mut words = words_of(conn, key)?;
    let mut stmt = conn.prepare(
        "SELECT id, track, start_ns, end_ns, text, engine, model FROM utterance \
         WHERE session_id = ?1 ORDER BY start_ns, end_ns, track, id",
    )?;
    let raws = stmt
        .query_map([key], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<Result<Vec<RawUtterance>, _>>()?;
    raws.into_iter()
        .map(|raw| {
            let words = words.remove(&raw.0).unwrap_or_default();
            parse_utterance(raw, words)
        })
        .collect()
}

impl Store {
    /// Stores `heard` as one of the session's utterances, with its words,
    /// in one transaction committed before returning, and gives its
    /// number. The session's revision 0, the heard text, is added with its
    /// first utterance. An utterance stored exactly as `heard` already
    /// (the same track, span, text, engine, model and words: a write retried
    /// after its answer was lost) isn't stored twice; its number comes back.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library,
    /// [`StoreError::OutOfRange`] if a number or time doesn't fit SQLite's
    /// integer, and [`StoreError::Sqlite`] for any SQLite failure. Nothing
    /// is stored then.
    pub fn add_utterance(
        &mut self,
        session: SessionId,
        heard: &Heard,
    ) -> Result<UtteranceId, StoreError> {
        let key = session_key(session)?;
        let u = &heard.utterance;
        let (start, end) = (nanos(u.start())?, nanos(u.end())?);
        let words = heard
            .words
            .iter()
            .map(|w| Ok((w.text(), nanos(w.start)?, nanos(w.end)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !session_exists(&tx, key)? {
            return Err(StoreError::NoSession(session));
        }
        if let Some(id) = same_utterance(&tx, key, heard)? {
            return Ok(UtteranceId(id));
        }
        tx.execute(
            "INSERT INTO utterance (session_id, track, start_ns, end_ns, text, engine, model) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                key,
                i64::from(u.track().get()),
                start,
                end,
                u.text(),
                heard.engine,
                heard.model
            ],
        )?;
        let id = tx.last_insert_rowid();
        for (position, (text, start, end)) in (0_i64..).zip(words) {
            tx.execute(
                "INSERT INTO word (utterance_id, position, text, start_ns, end_ns) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, position, text, start, end],
            )?;
        }
        if !revision_exists(&tx, key, RevisionNumber::HEARD.0)? {
            tx.execute(
                "INSERT INTO revision (session_id, number, parent) VALUES (?1, 0, NULL)",
                [key],
            )?;
        }
        tx.commit()?;
        Ok(UtteranceId(id))
    }

    /// The session's utterances as heard, in session order: by start, then
    /// end, then track, then the order they were stored.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn utterances(&self, session: SessionId) -> Result<Vec<StoredUtterance>, StoreError> {
        let key = session_key(session)?;
        // One snapshot: another writer's utterance comes with its words or
        // not at all.
        let tx = self.conn.unchecked_transaction()?;
        utterances_in(&tx, key)
    }

    /// The session's revisions, in number order: none before its first
    /// utterance, then revision 0, the heard text, and each made since.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a number doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn revisions(&self, session: SessionId) -> Result<Vec<RevisionNumber>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT number FROM revision WHERE session_id = ?1 ORDER BY number")?;
        let numbers = stmt
            .query_map([session_key(session)?], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        numbers
            .into_iter()
            .map(|n| {
                u32::try_from(n)
                    .map(RevisionNumber)
                    .map_err(|_| StoreError::Corrupt(format!("revision number {n}")))
            })
            .collect()
    }

    /// Adds a revision of the session's text: `parent` with each of
    /// `changes`' utterances showing its new text. Gives the new
    /// revision's number, one past the session's highest. Committed before
    /// returning.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoRevision`] if `parent` isn't one of the session's
    /// revisions, [`StoreError::NoUtterance`] if a change names an
    /// utterance of another session, and [`StoreError::Sqlite`] for any
    /// SQLite failure, including two changes to one utterance. Nothing is
    /// added then.
    pub fn add_revision(
        &mut self,
        session: SessionId,
        parent: RevisionNumber,
        changes: &[(UtteranceId, String)],
    ) -> Result<RevisionNumber, StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !revision_exists(&tx, key, parent.0)? {
            return Err(StoreError::NoRevision(session, parent.0));
        }
        for (utterance, _) in changes {
            let ours = tx
                .query_row(
                    "SELECT 1 FROM utterance WHERE session_id = ?1 AND id = ?2",
                    params![key, utterance.0],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !ours {
                return Err(StoreError::NoUtterance(session, utterance.0));
            }
        }
        let highest: i64 = tx.query_row(
            "SELECT max(number) FROM revision WHERE session_id = ?1",
            [key],
            |r| r.get(0),
        )?;
        let number = u32::try_from(highest)
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or(StoreError::OutOfRange)?;
        tx.execute(
            "INSERT INTO revision (session_id, number, parent) VALUES (?1, ?2, ?3)",
            params![key, i64::from(number), i64::from(parent.0)],
        )?;
        for (utterance, text) in changes {
            tx.execute(
                "INSERT INTO revision_text (session_id, revision, utterance_id, text) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![key, i64::from(number), utterance.0, text],
            )?;
        }
        tx.commit()?;
        Ok(RevisionNumber(number))
    }

    /// The session's text as revision `number` shows it, in session order:
    /// each utterance's text from the nearest revision back to 0 that
    /// changes it, or as heard.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoRevision`] if the session has no such revision,
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn revision(
        &self,
        session: SessionId,
        number: RevisionNumber,
    ) -> Result<Vec<Line>, StoreError> {
        let key = session_key(session)?;
        // One snapshot for the chain, its text and the utterances.
        let tx = self.conn.unchecked_transaction()?;
        if !revision_exists(&tx, key, number.0)? {
            return Err(StoreError::NoRevision(session, number.0));
        }
        // The revision and its ancestors, nearest first. Each parent is
        // lower than its child (the schema checks), so the walk ends.
        let mut chain = vec![number.0];
        let mut at = number.0;
        while let Some(parent) = tx
            .query_row(
                "SELECT parent FROM revision WHERE session_id = ?1 AND number = ?2",
                params![key, i64::from(at)],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
        {
            at = u32::try_from(parent)
                .map_err(|_| StoreError::Corrupt(format!("revision parent {parent}")))?;
            chain.push(at);
        }
        let mut changed: BTreeMap<i64, String> = BTreeMap::new();
        let mut stmt = tx.prepare(
            "SELECT utterance_id, text FROM revision_text \
             WHERE session_id = ?1 AND revision = ?2",
        )?;
        // Revision 0 is the heard text, with no text of its own.
        for revision in chain.into_iter().filter(|&n| n != 0) {
            let rows = stmt
                .query_map(params![key, i64::from(revision)], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (utterance, text) in rows {
                // The nearest revision's text wins.
                changed.entry(utterance).or_insert(text);
            }
        }
        let lines = utterances_in(&tx, key)?
            .into_iter()
            .map(|stored| {
                let u = stored.heard.utterance;
                Line {
                    utterance: stored.id,
                    track: u.track(),
                    start: u.start(),
                    end: u.end(),
                    text: changed
                        .remove(&stored.id.0)
                        .unwrap_or_else(|| u.into_text()),
                }
            })
            .collect();
        Ok(lines)
    }
}

#[cfg(test)]
mod tests;
