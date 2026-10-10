//! Epoch rows: each epoch's anchor, so a track's samples can be placed in
//! session time once its journals are gone.
//!
//! The recorder keeps an epoch's anchor in its journals' headers, and
//! copies it here with each segment it publishes, before the segment's
//! row, so a committed segment's epoch is timed here, drift included. Segments from
//! journals that didn't keep it (an older nota's), and rows committed
//! before it was kept, have no epoch row. An epoch's anchor never changes:
//! a different one for an epoch already stored is refused.

use nota_core::{
    Drift, EpochAnchor, EpochId, SampleIndex, SampleRate, SessionId, SessionTime, TrackId,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::segments::{Inserted, parse_track, session_exists};
use crate::{Store, StoreError, session_key};

/// An epoch row as SQLite holds it: track, epoch, first sample, rate,
/// anchor, drift.
type RawEpoch = (i64, i64, i64, i64, Option<i64>, i64);

impl Store {
    /// Stores the anchor of `track`'s epoch in `session`, in its own
    /// transaction, committed before returning. The same anchor again is
    /// [`Inserted::AlreadyPresent`].
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library;
    /// [`StoreError::EpochConflict`] if the epoch is stored with another
    /// anchor; [`StoreError::OutOfRange`] if a number doesn't fit SQLite's
    /// integer; [`StoreError::Corrupt`] if the stored row doesn't parse;
    /// [`StoreError::Sqlite`] for any other SQLite failure.
    pub fn insert_epoch(
        &mut self,
        session: SessionId,
        track: TrackId,
        anchor: &EpochAnchor,
    ) -> Result<Inserted, StoreError> {
        let key = session_key(session)?;
        let first = i64::try_from(anchor.first_sample.get()).map_err(|_| StoreError::OutOfRange)?;
        let start = i64::try_from(anchor.start.as_nanos()).map_err(|_| StoreError::OutOfRange)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !session_exists(&tx, key)? {
            return Err(StoreError::NoSession(session));
        }
        let stored = tx
            .query_row(
                "SELECT track, epoch, first_sample, rate, anchor_ns, drift_ppb FROM epoch \
                 WHERE session_id = ?1 AND track = ?2 AND epoch = ?3",
                params![key, i64::from(track.get()), i64::from(anchor.id.get())],
                raw_from_row,
            )
            .optional()?;
        if let Some(raw) = stored {
            let (_, existing) = parse_epoch(raw)?;
            return if existing == Some(*anchor) {
                Ok(Inserted::AlreadyPresent)
            } else {
                Err(StoreError::EpochConflict {
                    track,
                    epoch: anchor.id,
                })
            };
        }
        tx.execute(
            "INSERT INTO epoch \
             (session_id, track, epoch, first_sample, rate, anchor_ns, drift_ppb) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                key,
                i64::from(track.get()),
                i64::from(anchor.id.get()),
                first,
                i64::from(anchor.rate.hz()),
                start,
                i64::from(anchor.drift.ppb())
            ],
        )?;
        tx.commit()?;
        Ok(Inserted::New)
    }

    /// Every timed epoch of `session`, ordered by track then epoch: each
    /// track's anchors, oldest first. Rows with no anchor are left out.
    ///
    /// # Errors
    ///
    /// [`StoreError::OutOfRange`] if the session's number doesn't fit
    /// SQLite's integer; [`StoreError::Corrupt`] if a row doesn't parse;
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn epochs(&self, session: SessionId) -> Result<Vec<(TrackId, EpochAnchor)>, StoreError> {
        let key = session_key(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT track, epoch, first_sample, rate, anchor_ns, drift_ppb FROM epoch \
             WHERE session_id = ?1 ORDER BY track, epoch",
        )?;
        let raws = stmt
            .query_map([key], raw_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut epochs = Vec::new();
        for raw in raws {
            if let (track, Some(anchor)) = parse_epoch(raw)? {
                epochs.push((track, anchor));
            }
        }
        Ok(epochs)
    }
}

/// The columns of an epoch row.
fn raw_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEpoch> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

/// A stored epoch row's track and anchor; no anchor if it has none.
fn parse_epoch(
    (track, epoch, first, rate, anchor, drift): RawEpoch,
) -> Result<(TrackId, Option<EpochAnchor>), StoreError> {
    let corrupt = |what: &str| StoreError::Corrupt(format!("epoch row: {what}"));
    let track = parse_track(track)?;
    let id = u32::try_from(epoch)
        .map(EpochId::new)
        .map_err(|_| corrupt("an epoch that isn't a u32"))?;
    let first_sample = u64::try_from(first)
        .map(SampleIndex::new)
        .map_err(|_| corrupt("a negative first sample"))?;
    let rate = u32::try_from(rate)
        .ok()
        .and_then(SampleRate::new)
        .ok_or_else(|| corrupt("a rate out of range"))?;
    let drift = i32::try_from(drift)
        .ok()
        .and_then(Drift::from_ppb)
        .ok_or_else(|| corrupt("a drift out of range"))?;
    let Some(anchor) = anchor else {
        return Ok((track, None));
    };
    let start = u64::try_from(anchor)
        .map(SessionTime::from_nanos)
        .map_err(|_| corrupt("a negative anchor"))?;
    Ok((
        track,
        Some(EpochAnchor {
            id,
            start,
            first_sample,
            rate,
            drift,
        }),
    ))
}

#[cfg(test)]
mod tests;
