//! Segment rows: one for each audio segment the recorder has published,
//! scoped by session.
//!
//! A row says its segment file is durable, and salvage deletes journals on
//! its word. Its key is the session, track and first sample; within a
//! session, segments of one track never share a sample. Another session's
//! rows never count: two sessions can hold rows at the same track and
//! samples.
//!
//! A row holds two proofs of what its file holds. The file's SHA-256 proves
//! the file is the one published, byte for byte. The digest of its decoded
//! audio ([`AudioDigest`], since version 6) proves the audio is, however
//! the file was encoded: FLAC allows many encodings of the same samples, so
//! a file re-encoded by another encoder, or other settings, has another
//! SHA-256 but the same audio digest. Rows from before version 6 have no
//! audio digest; only their SHA-256 proves their file.

use nota_core::{EpochId, SampleIndex, SampleRange, SessionId, TrackId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{Store, StoreError, session_key};

/// A segment file's SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// The digest with these 32 bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest's bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A digest of a segment's decoded audio: the SHA-256 of its canonical PCM
/// (the recorder's `segment` module defines it: its rate, track, sample
/// range and samples). Equal digests mean the same samples, whatever the
/// FLAC encoder or its settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioDigest([u8; 32]);

impl AudioDigest {
    /// The digest with these 32 bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest's bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Where a segment row is in its session: its track and first sample, the
/// table's key, as SQLite holds them. Raw, so it can name a row that
/// doesn't parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowKey {
    /// The row's track, as stored.
    pub track: i64,
    /// The row's first sample, as stored.
    pub start: i64,
}

/// One published segment: a track's continuous, non-empty run of samples
/// within one epoch. Which session it belongs to is given with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRow {
    track: TrackId,
    epoch: EpochId,
    range: SampleRange,
    sha256: Sha256Digest,
    audio: Option<AudioDigest>,
}

impl SegmentRow {
    /// A segment row with no audio digest, or `None` if `range` is empty.
    /// See [`Self::with_audio`].
    #[must_use]
    pub fn new(
        track: TrackId,
        epoch: EpochId,
        range: SampleRange,
        sha256: Sha256Digest,
    ) -> Option<Self> {
        if range.is_empty() {
            return None;
        }
        Some(Self {
            track,
            epoch,
            range,
            sha256,
            audio: None,
        })
    }

    /// This row, with `audio` as the digest of its decoded audio.
    #[must_use]
    pub const fn with_audio(self, audio: AudioDigest) -> Self {
        Self {
            audio: Some(audio),
            ..self
        }
    }

    /// The track.
    #[must_use]
    pub const fn track(&self) -> TrackId {
        self.track
    }

    /// The epoch the samples were recorded in.
    #[must_use]
    pub const fn epoch(&self) -> EpochId {
        self.epoch
    }

    /// The samples the segment holds.
    #[must_use]
    pub const fn range(&self) -> SampleRange {
        self.range
    }

    /// The SHA-256 of the segment file.
    #[must_use]
    pub const fn sha256(&self) -> Sha256Digest {
        self.sha256
    }

    /// The digest of the segment's decoded audio, if the row has one: every
    /// row committed since version 6 does.
    #[must_use]
    pub const fn audio(&self) -> Option<AudioDigest> {
        self.audio
    }
}

/// What [`Store::insert_segment`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// The row was added.
    New,
    /// An identical row was already stored; nothing changed.
    AlreadyPresent,
}

/// A row as SQLite holds it: track, epoch, start, end, hash, audio digest.
pub(crate) type RawRow = (i64, i64, i64, i64, Vec<u8>, Option<Vec<u8>>);

/// The columns [`raw_from_row`] reads, in its order.
pub(crate) const COLUMNS: &str = "track, epoch, start_sample, end_sample, sha256, audio_digest";

fn to_i64(sample: SampleIndex) -> Result<i64, StoreError> {
    i64::try_from(sample.get()).map_err(|_| StoreError::OutOfRange)
}

pub(crate) fn raw_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

/// A stored track number.
pub(crate) fn parse_track(track: i64) -> Result<TrackId, StoreError> {
    u32::try_from(track)
        .map(TrackId::new)
        .map_err(|_| StoreError::Corrupt(format!("track {track} is not a u32")))
}

/// Parses a stored row of `session` into typed values. A row that doesn't
/// parse is a [`StoreError::CorruptRow`] naming it.
pub(crate) fn parse_row(session: SessionId, raw: RawRow) -> Result<SegmentRow, StoreError> {
    let key = RowKey {
        track: raw.0,
        start: raw.2,
    };
    parse_fields(raw).map_err(|why| StoreError::CorruptRow { session, key, why })
}

/// [`parse_row`]'s work: the row, or why it doesn't parse.
fn parse_fields((track, epoch, start, end, hash, audio): RawRow) -> Result<SegmentRow, String> {
    let track = u32::try_from(track)
        .map(TrackId::new)
        .map_err(|_| format!("track {track} is not a u32"))?;
    let epoch = u32::try_from(epoch)
        .map(EpochId::new)
        .map_err(|_| format!("epoch {epoch} is not a u32"))?;
    let start = u64::try_from(start)
        .map(SampleIndex::new)
        .map_err(|_| format!("start sample {start} is negative"))?;
    let end = u64::try_from(end)
        .map(SampleIndex::new)
        .map_err(|_| format!("end sample {end} is negative"))?;
    let range =
        SampleRange::new(start, end).ok_or_else(|| "range ends before it starts".to_owned())?;
    let hash: [u8; 32] = hash
        .try_into()
        .map_err(|h: Vec<u8>| format!("hash is {} bytes, not 32", h.len()))?;
    let row = SegmentRow::new(track, epoch, range, Sha256Digest::new(hash))
        .ok_or_else(|| "range is empty".to_owned())?;
    match audio {
        None => Ok(row),
        Some(audio) => {
            let audio: [u8; 32] = audio
                .try_into()
                .map_err(|a: Vec<u8>| format!("audio digest is {} bytes, not 32", a.len()))?;
            Ok(row.with_audio(AudioDigest::new(audio)))
        }
    }
}

/// Whether the session is in the `session` table.
pub(crate) fn session_exists(conn: &Connection, session: i64) -> Result<bool, StoreError> {
    Ok(conn
        .query_row("SELECT 1 FROM session WHERE id = ?1", [session], |_| Ok(()))
        .optional()?
        .is_some())
}

/// [`Store::insert_segment`]'s work, inside the caller's transaction.
pub(crate) fn insert(
    conn: &Connection,
    session: SessionId,
    row: &SegmentRow,
) -> Result<Inserted, StoreError> {
    let key = session_key(session)?;
    let start = to_i64(row.range.start())?;
    let end = to_i64(row.range.end())?;
    let track = i64::from(row.track.get());
    if !session_exists(conn, key)? {
        return Err(StoreError::NoSession(session));
    }
    let existing = conn
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM segment \
                 WHERE session_id = ?1 AND track = ?2 AND start_sample = ?3"
            ),
            params![key, track, start],
            raw_from_row,
        )
        .optional()?;
    if let Some(raw) = existing {
        let existing = parse_row(session, raw)?;
        return if existing == *row {
            Ok(Inserted::AlreadyPresent)
        } else {
            Err(StoreError::Conflict { existing })
        };
    }
    // Segments of one track in one session never share a sample.
    let overlapping = conn
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM segment WHERE session_id = ?1 AND track = ?2 \
                 AND start_sample < ?4 AND end_sample > ?3 LIMIT 1"
            ),
            params![key, track, start, end],
            raw_from_row,
        )
        .optional()?;
    if let Some(raw) = overlapping {
        return Err(StoreError::Conflict {
            existing: parse_row(session, raw)?,
        });
    }
    conn.execute(
        "INSERT INTO segment \
         (session_id, track, epoch, start_sample, end_sample, sha256, audio_digest) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            key,
            track,
            i64::from(row.epoch.get()),
            start,
            end,
            row.sha256.as_bytes().as_slice(),
            row.audio.as_ref().map(|a| a.as_bytes().as_slice()),
        ],
    )?;
    Ok(Inserted::New)
}

impl Store {
    /// Inserts `session`'s row in its own transaction, committed before
    /// returning.
    ///
    /// A row says its segment file is durable, and salvage deletes journals
    /// on its word, so only the recorder's publish step calls this, once the
    /// file is fsync'd under its final name.
    ///
    /// The type system can't hold callers to that here: the proof that a
    /// file is durable (the recorder's `DurableSegment`) is built in the
    /// recorder, which depends on this crate, so this one can't name it, and
    /// Rust has no visibility for "this crate and the recorder only". The
    /// recorder's `SegmentStore::insert` takes the proof and is the one
    /// caller outside tests and examples. Salvage limits the damage of a
    /// wrong row: it deletes a journal on a row's word only if the row's
    /// segment file exists and matches its hash and length. It can't tell
    /// whether that file is durable, though, so callers must still make it
    /// durable first.
    ///
    /// If a row of the session with the same track and start sample is
    /// stored, an identical one is [`Inserted::AlreadyPresent`] and a
    /// different one is a [`StoreError::Conflict`]. So is a row of the same
    /// session and track whose range overlaps the new one's: segments of a
    /// track never share a sample. Other sessions' rows don't count.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library,
    /// [`StoreError::OutOfRange`] if a number doesn't fit SQLite's integer,
    /// [`StoreError::Conflict`] as above, [`StoreError::CorruptRow`] if the
    /// stored row can't be parsed, and [`StoreError::Sqlite`] for any other
    /// SQLite failure.
    pub fn insert_segment(
        &mut self,
        session: SessionId,
        row: &SegmentRow,
    ) -> Result<Inserted, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = insert(&tx, session, row)?;
        if inserted == Inserted::New {
            tx.commit()?;
        }
        Ok(inserted)
    }

    /// Every row of `session`, ordered by track then start sample.
    ///
    /// # Errors
    ///
    /// [`StoreError::OutOfRange`] if the session's number doesn't fit
    /// SQLite's integer, [`StoreError::CorruptRow`], naming the first row in
    /// that order that doesn't parse, and [`StoreError::Sqlite`] for any
    /// SQLite failure. One row that doesn't parse fails the whole call: the
    /// schema's checks refuse such a row, so only an outside edit or a
    /// damaged database makes one, and leaving it out would let a segment be
    /// published over the samples it may hold.
    pub fn segments(&self, session: SessionId) -> Result<Vec<SegmentRow>, StoreError> {
        let key = session_key(session)?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM segment WHERE session_id = ?1 ORDER BY track, start_sample"
        ))?;
        let raws = stmt
            .query_map([key], raw_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter()
            .map(|raw| parse_row(session, raw))
            .collect()
    }
}
