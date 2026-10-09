//! The final pass's text: the session's published audio transcribed again
//! after the stop, in longer chunks than the live pass's.
//!
//! **Beside the heard text, never in it.** The heard text (the live pass,
//! [`crate::transcript`]) is the record, and revisions show it; the final
//! pass's text has rows of its own, so nothing that reads utterances or
//! revisions sees the two passes mixed. Its rows are located as the engine
//! gives them, by track and sample: placing them in session time needs
//! each epoch's anchor, which isn't stored yet.
//!
//! **Each sample once.** A track's rows never overlap. With them, each
//! track's progress says how far the pass has got: every published sample
//! before it has been through the engine, with its text in a row (or none,
//! for silence). A row with no text is audio the engine couldn't
//! transcribe. Text and progress are committed together, so a pass stopped
//! at any point carries on from its progress and adds each row once.

use std::collections::BTreeMap;

use nota_core::{SampleIndex, SampleRange, SessionId, TrackId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::segments::{parse_track, session_exists};
use crate::{Store, StoreError, session_key};

/// The engine and model that heard a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeardBy {
    /// The engine, e.g. `sherpa-onnx`.
    pub engine: String,
    /// Its model, e.g. `parakeet-tdt-0.6b-v3-int8`.
    pub model: String,
}

/// One word of a final-pass text, and its samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalWord {
    /// The word.
    pub text: String,
    /// Where it was said.
    pub range: SampleRange,
}

/// A run of one track's samples, and what the final pass heard in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalText {
    /// The track.
    pub track: TrackId,
    /// The samples; never empty.
    pub range: SampleRange,
    /// What was heard; `None` if the engine couldn't transcribe it.
    pub text: Option<String>,
    /// Its words, in order; empty if the engine didn't give them.
    pub words: Vec<FinalWord>,
    /// What heard it.
    pub heard_by: HeardBy,
}

fn sample(at: SampleIndex) -> Result<i64, StoreError> {
    i64::try_from(at.get()).map_err(|_| StoreError::OutOfRange)
}

fn parse_sample(at: i64) -> Result<SampleIndex, StoreError> {
    u64::try_from(at)
        .map(SampleIndex::new)
        .map_err(|_| StoreError::Corrupt(format!("sample {at} is negative")))
}

fn parse_range(start: i64, end: i64) -> Result<SampleRange, StoreError> {
    SampleRange::new(parse_sample(start)?, parse_sample(end)?)
        .filter(|range| !range.is_empty())
        .ok_or_else(|| StoreError::Corrupt(format!("samples {start}..{end}")))
}

/// What's stored for the track at `range`: `Some(true)` if a row exactly
/// as `text` (words and all) is there, `Some(false)` if a different row
/// overlaps it, `None` if nothing does.
fn stored_as(conn: &Connection, key: i64, text: &FinalText) -> Result<Option<bool>, StoreError> {
    let track = i64::from(text.track.get());
    let (start, end) = (sample(text.range.start())?, sample(text.range.end())?);
    let overlap: Option<(i64, i64, Option<String>, String, String)> = conn
        .query_row(
            "SELECT start_sample, end_sample, text, engine, model FROM final_text \
             WHERE session_id = ?1 AND track = ?2 AND start_sample < ?4 AND end_sample > ?3 \
             ORDER BY start_sample LIMIT 1",
            params![key, track, start, end],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((at, until, heard, engine, model)) = overlap else {
        return Ok(None);
    };
    let same = (at, until) == (start, end)
        && heard == text.text
        && engine == text.heard_by.engine
        && model == text.heard_by.model
        && words_of(conn, key, track, at)? == text.words;
    Ok(Some(same))
}

fn words_of(
    conn: &Connection,
    key: i64,
    track: i64,
    start: i64,
) -> Result<Vec<FinalWord>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT text, word_start, word_end FROM final_word \
         WHERE session_id = ?1 AND track = ?2 AND start_sample = ?3 ORDER BY position",
    )?;
    let raws = stmt
        .query_map(params![key, track, start], |r| {
            Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    raws.into_iter()
        .map(|(text, from, to)| {
            Ok(FinalWord {
                text,
                range: SampleRange::new(parse_sample(from)?, parse_sample(to)?)
                    .ok_or_else(|| StoreError::Corrupt(format!("word at {from}..{to}")))?,
            })
        })
        .collect()
}

impl Store {
    /// Stores the final pass's `texts` for `track`, and that the pass has
    /// got to `up_to` on it, in one transaction committed before
    /// returning. Progress never goes back: an `up_to` before the stored
    /// one leaves it. A text stored exactly so already (a write retried
    /// after its answer was lost) isn't stored twice.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library,
    /// [`StoreError::FinalOverlap`] if a text is on another track, ends
    /// past `up_to`, or overlaps a different text stored or given,
    /// [`StoreError::OutOfRange`] if a sample doesn't fit SQLite's integer,
    /// and [`StoreError::Sqlite`] for any SQLite failure. Nothing is
    /// stored then.
    pub fn add_final_text(
        &mut self,
        session: SessionId,
        track: TrackId,
        up_to: SampleIndex,
        texts: &[FinalText],
    ) -> Result<(), StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !session_exists(&tx, key)? {
            return Err(StoreError::NoSession(session));
        }
        for text in texts {
            let misplaced = text.track != track || text.range.end() > up_to;
            let overlap = || StoreError::FinalOverlap {
                track,
                start: text.range.start(),
            };
            if misplaced || text.range.is_empty() {
                return Err(overlap());
            }
            match stored_as(&tx, key, text)? {
                Some(true) => continue,
                Some(false) => return Err(overlap()),
                None => {}
            }
            let start = sample(text.range.start())?;
            tx.execute(
                "INSERT INTO final_text \
                 (session_id, track, start_sample, end_sample, text, engine, model) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    key,
                    i64::from(track.get()),
                    start,
                    sample(text.range.end())?,
                    text.text,
                    text.heard_by.engine,
                    text.heard_by.model
                ],
            )?;
            for (position, word) in (0_i64..).zip(&text.words) {
                tx.execute(
                    "INSERT INTO final_word \
                     (session_id, track, start_sample, position, text, word_start, word_end) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        key,
                        i64::from(track.get()),
                        start,
                        position,
                        word.text,
                        sample(word.range.start())?,
                        sample(word.range.end())?
                    ],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO final_progress (session_id, track, up_to) VALUES (?1, ?2, ?3) \
             ON CONFLICT (session_id, track) DO UPDATE SET up_to = max(up_to, excluded.up_to)",
            params![key, i64::from(track.get()), sample(up_to)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// How far the final pass has got on each of the session's tracks:
    /// every published sample before it is done. A track it hasn't started
    /// isn't there.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn final_progress(
        &self,
        session: SessionId,
    ) -> Result<BTreeMap<TrackId, SampleIndex>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT track, up_to FROM final_progress WHERE session_id = ?1")?;
        let raws = stmt
            .query_map([session_key(session)?], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter()
            .map(|(track, up_to)| Ok((parse_track(track)?, parse_sample(up_to)?)))
            .collect()
    }

    /// The final pass's text for the session, by track, then in sample
    /// order.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn final_texts(&self, session: SessionId) -> Result<Vec<FinalText>, StoreError> {
        let key = session_key(session)?;
        // One snapshot: a text comes with its words.
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare(
            "SELECT track, start_sample, end_sample, text, engine, model FROM final_text \
             WHERE session_id = ?1 ORDER BY track, start_sample",
        )?;
        let raws = stmt
            .query_map([key], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter()
            .map(|(track, start, end, text, engine, model)| {
                Ok(FinalText {
                    track: parse_track(track)?,
                    range: parse_range(start, end)?,
                    text,
                    words: words_of(&tx, key, track, start)?,
                    heard_by: HeardBy { engine, model },
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
