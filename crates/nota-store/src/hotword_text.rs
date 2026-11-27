//! The hotword pass's text: each final-pass chunk heard again by a beam
//! search that boosts the session's terms, after the final pass.
//!
//! **One row per final-pass chunk it decoded,** named by the chunk's track
//! and first sample (a foreign key to `final_text`) and covering exactly
//! the chunk's samples, so clean-up compares each chunk's two texts word by
//! word. Its text is null where the beam heard nothing. Each row records
//! the boost it ran at.
//!
//! **A record, like the final text.** Once stored, a row is never changed,
//! deleted or replaced (the schema's triggers, version 13). Text and
//! progress are committed together, so a pass stopped at any point carries
//! on from its progress and adds each row once; a write retried after its
//! answer was lost stores nothing twice.
//!
//! **What the engine left out** ([`LeftOut`]): how many terms the pass
//! last gave its engine, and those the engine said it can't boost. It's
//! what the engine said last, not a record: each report replaces the one
//! before (version 15).

use std::collections::BTreeMap;

use nota_core::{SampleIndex, SampleRange, SessionId, TrackId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::final_text::{parse_sample, sample};
use crate::segments::{parse_track, session_exists};
use crate::{Store, StoreError, session_key};

/// What the hotword pass heard in one final-pass chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct HotwordText {
    /// The chunk's track.
    pub track: TrackId,
    /// The chunk's samples: exactly its `final_text` row's.
    pub range: SampleRange,
    /// What the boosted beam heard; `None` if nothing.
    pub text: Option<String>,
    /// The boost it ran at (sherpa-onnx's `hotwords_score`), above 0.
    pub boost: f64,
}

/// What the hotword pass's engine said of the terms it was given: how
/// many it was given, and those it left out, unboosted. There are never
/// more left out than given; only [`LeftOut::new`] makes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeftOut {
    given: usize,
    terms: Vec<String>,
}

impl LeftOut {
    /// `terms` left out of `given` terms, or `None` if that's more terms
    /// than were given, or one of them is empty.
    #[must_use]
    pub fn new(given: usize, terms: Vec<String>) -> Option<Self> {
        let fits = terms.len() <= given && terms.iter().all(|term| !term.is_empty());
        fits.then_some(Self { given, terms })
    }

    /// How many terms the engine was given.
    #[must_use]
    pub const fn given(&self) -> usize {
        self.given
    }

    /// The terms it left out, in the order they were given.
    #[must_use]
    pub fn terms(&self) -> &[String] {
        &self.terms
    }
}

/// How `text` stands against what's stored: `Some(true)` if stored exactly
/// so, `Some(false)` if a different row is stored for its chunk, `None` if
/// none is.
fn stored_as(conn: &Connection, key: i64, text: &HotwordText) -> Result<Option<bool>, StoreError> {
    let stored: Option<(i64, Option<String>, f64)> = conn
        .query_row(
            "SELECT end_sample, text, boost FROM hotword_text \
             WHERE session_id = ?1 AND track = ?2 AND start_sample = ?3",
            params![
                key,
                i64::from(text.track.get()),
                sample(text.range.start())?
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let end = sample(text.range.end())?;
    Ok(stored.map(|(e, t, b)| e == end && t == text.text && b.to_bits() == text.boost.to_bits()))
}

/// Whether `text` covers exactly a final-pass chunk of session `key`.
fn fits_a_chunk(conn: &Connection, key: i64, text: &HotwordText) -> Result<bool, StoreError> {
    let end: Option<i64> = conn
        .query_row(
            "SELECT end_sample FROM final_text \
             WHERE session_id = ?1 AND track = ?2 AND start_sample = ?3",
            params![
                key,
                i64::from(text.track.get()),
                sample(text.range.start())?
            ],
            |r| r.get(0),
        )
        .optional()?;
    Ok(end == Some(sample(text.range.end())?))
}

impl Store {
    /// Stores the hotword pass's `texts` for `track`, and that the pass has
    /// got to `up_to` on it, in one transaction committed before
    /// returning. Progress never goes back. A text stored exactly so
    /// already isn't stored twice.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library,
    /// [`StoreError::HotwordMisplaced`] if a text is on another track, ends
    /// past `up_to`, doesn't cover exactly one final-pass chunk, has a
    /// boost that isn't above 0, or differs from the row stored for its
    /// chunk, [`StoreError::OutOfRange`] if a sample doesn't fit SQLite's
    /// integer, and [`StoreError::Sqlite`] for any SQLite failure. Nothing
    /// is stored then.
    pub fn add_hotword_text(
        &mut self,
        session: SessionId,
        track: TrackId,
        up_to: SampleIndex,
        texts: &[HotwordText],
    ) -> Result<(), StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !session_exists(&tx, key)? {
            return Err(StoreError::NoSession(session));
        }
        for text in texts {
            let misplaced = || StoreError::HotwordMisplaced {
                track,
                start: text.range.start(),
            };
            // A NaN or infinite boost is refused too.
            let bad_boost = !text.boost.is_finite() || text.boost <= 0.0;
            if text.track != track || text.range.end() > up_to || bad_boost {
                return Err(misplaced());
            }
            match stored_as(&tx, key, text)? {
                Some(true) => continue,
                Some(false) => return Err(misplaced()),
                None if !fits_a_chunk(&tx, key, text)? => return Err(misplaced()),
                None => {}
            }
            tx.execute(
                "INSERT INTO hotword_text \
                 (session_id, track, start_sample, end_sample, text, boost) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    key,
                    i64::from(track.get()),
                    sample(text.range.start())?,
                    sample(text.range.end())?,
                    text.text,
                    text.boost
                ],
            )?;
        }
        tx.execute(
            "INSERT INTO hotword_progress (session_id, track, up_to) VALUES (?1, ?2, ?3) \
             ON CONFLICT (session_id, track) DO UPDATE SET up_to = max(up_to, excluded.up_to)",
            params![key, i64::from(track.get()), sample(up_to)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// How far the hotword pass has got on each of the session's tracks:
    /// every final-pass chunk ending at or before it is done. A track it
    /// hasn't started isn't there.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn hotword_progress(
        &self,
        session: SessionId,
    ) -> Result<BTreeMap<TrackId, SampleIndex>, StoreError> {
        let key = session_key(session)?;
        let mut stmt = self
            .conn
            .prepare("SELECT track, up_to FROM hotword_progress WHERE session_id = ?1")?;
        let raws = stmt
            .query_map([key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter()
            .map(|(track, up_to)| Ok((parse_track(track)?, parse_sample(up_to)?)))
            .collect()
    }

    /// Stores what the session's hotword engine said it left out, in place
    /// of whatever it said before, in one transaction committed before
    /// returning.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library,
    /// [`StoreError::OutOfRange`] if a count doesn't fit SQLite's integer,
    /// and [`StoreError::Sqlite`] for any SQLite failure. Nothing changes
    /// then.
    pub fn set_hotword_left_out(
        &mut self,
        session: SessionId,
        left_out: &LeftOut,
    ) -> Result<(), StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !session_exists(&tx, key)? {
            return Err(StoreError::NoSession(session));
        }
        let given = i64::try_from(left_out.given).map_err(|_| StoreError::OutOfRange)?;
        // The terms first, since they name the count's row.
        tx.execute(
            "DELETE FROM hotword_left_out_term WHERE session_id = ?1",
            [key],
        )?;
        tx.execute(
            "INSERT INTO hotword_left_out (session_id, given) VALUES (?1, ?2) \
             ON CONFLICT (session_id) DO UPDATE SET given = excluded.given",
            params![key, given],
        )?;
        for (position, term) in left_out.terms.iter().enumerate() {
            let position = i64::try_from(position).map_err(|_| StoreError::OutOfRange)?;
            tx.execute(
                "INSERT INTO hotword_left_out_term (session_id, position, term) \
                 VALUES (?1, ?2, ?3)",
                params![key, position, term],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// What the session's hotword engine last said it left out; `None` if
    /// no engine has said.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if the rows don't make one (more terms than
    /// given, an empty term), and [`StoreError::Sqlite`] for any SQLite
    /// failure.
    pub fn hotword_left_out(&self, session: SessionId) -> Result<Option<LeftOut>, StoreError> {
        let key = session_key(session)?;
        let given: Option<i64> = self
            .conn
            .query_row(
                "SELECT given FROM hotword_left_out WHERE session_id = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(given) = given else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT term FROM hotword_left_out_term WHERE session_id = ?1 ORDER BY position",
        )?;
        let terms = stmt
            .query_map([key], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let corrupt = || StoreError::Corrupt(format!("{} terms left out of {given}", terms.len()));
        let given = usize::try_from(given).map_err(|_| corrupt())?;
        let found = LeftOut::new(given, terms.clone()).ok_or_else(corrupt)?;
        Ok(Some(found))
    }

    /// The hotword pass's texts for the session, by track and then sample.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn hotword_texts(&self, session: SessionId) -> Result<Vec<HotwordText>, StoreError> {
        let key = session_key(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT track, start_sample, end_sample, text, boost FROM hotword_text \
             WHERE session_id = ?1 ORDER BY track, start_sample",
        )?;
        let raws = stmt
            .query_map([key], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, f64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter()
            .map(|(track, from, to, text, boost)| {
                let range = SampleRange::new(parse_sample(from)?, parse_sample(to)?)
                    .ok_or_else(|| StoreError::Corrupt(format!("hotword text at {from}..{to}")))?;
                Ok(HotwordText {
                    track: parse_track(track)?,
                    range,
                    text,
                    boost,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
