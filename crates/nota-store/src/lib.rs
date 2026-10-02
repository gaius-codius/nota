//! nota's database.
//!
//! For now this holds one thing: a row for each audio segment the recorder
//! has published. A row is committed durably (`synchronous=FULL`, write-ahead
//! log) before the journal for that audio is deleted. The full schema comes
//! later.
//!
//! The native library is SQLite, built in through rusqlite's `bundled`
//! feature, so nothing needs installing on the machine. SQLite does its own
//! file I/O, so the database is the one durable write that doesn't go
//! through the recorder's filesystem layer; the recorder's crash tests use a
//! stand-in store, and the `LazyFS` runs check this one on a real filesystem.
//!
//! **One store per session.** Rows carry no session, so a store must hold
//! one session's rows only. (Salvage also ignores a row unless its segment
//! file is in the session's directory and matches it: its SHA-256, and the
//! number of samples its FLAC header declares.)

use std::fmt;
use std::path::Path;

use nota_core::{EpochId, SampleIndex, SampleRange, TrackId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// The schema version this code writes and understands.
const SCHEMA_VERSION: i64 = 1;

const CREATE_SCHEMA: &str = "
CREATE TABLE segment (
    track INTEGER NOT NULL,
    epoch INTEGER NOT NULL,
    start_sample INTEGER NOT NULL,
    end_sample INTEGER NOT NULL,
    sha256 BLOB NOT NULL,
    PRIMARY KEY (track, start_sample),
    CHECK (start_sample >= 0 AND end_sample > start_sample),
    CHECK (length(sha256) = 32)
) STRICT;
";

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

/// One published segment: a track's continuous, non-empty run of samples
/// within one epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRow {
    track: TrackId,
    epoch: EpochId,
    range: SampleRange,
    sha256: Sha256Digest,
}

impl SegmentRow {
    /// A segment row, or `None` if `range` is empty.
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
        })
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
}

/// What [`Store::insert_segment`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// The row was added.
    New,
    /// An identical row was already stored; nothing changed.
    AlreadyPresent,
}

/// Why a store call failed.
#[derive(Debug)]
pub enum StoreError {
    /// SQLite reported an error.
    Sqlite(rusqlite::Error),
    /// A pragma didn't take: its name and what it read back.
    Pragma {
        /// The pragma's name.
        name: &'static str,
        /// The value it read back.
        found: String,
    },
    /// The database's schema version (`PRAGMA user_version`) isn't one this
    /// code knows.
    UnknownSchema(i64),
    /// A sample index doesn't fit SQLite's signed 64-bit integer.
    OutOfRange,
    /// A different row already holds this track and start sample, or some
    /// of the new row's samples.
    Conflict {
        /// The row that is stored.
        existing: SegmentRow,
    },
    /// A stored row doesn't parse: a negative or inverted range, a
    /// wrong-length hash, or a track or epoch outside `u32`.
    Corrupt(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "sqlite error: {e}"),
            Self::Pragma { name, found } => {
                write!(f, "pragma {name} did not take (read back {found:?})")
            }
            Self::UnknownSchema(v) => write!(f, "unknown database schema version {v}"),
            Self::OutOfRange => write!(f, "sample index does not fit a 64-bit signed integer"),
            Self::Conflict { existing } => write!(
                f,
                "a different segment already holds track {} from sample {}",
                existing.track.get(),
                existing.range.start().get()
            ),
            Self::Corrupt(why) => write!(f, "corrupt segment row: {why}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// A row as SQLite holds it: track, epoch, start, end, hash.
type RawRow = (i64, i64, i64, i64, Vec<u8>);

fn to_i64(sample: SampleIndex) -> Result<i64, StoreError> {
    i64::try_from(sample.get()).map_err(|_| StoreError::OutOfRange)
}

fn raw_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

/// Parses a stored row into typed values.
fn parse_row((track, epoch, start, end, hash): RawRow) -> Result<SegmentRow, StoreError> {
    let track = u32::try_from(track)
        .map(TrackId::new)
        .map_err(|_| StoreError::Corrupt(format!("track {track} is not a u32")))?;
    let epoch = u32::try_from(epoch)
        .map(EpochId::new)
        .map_err(|_| StoreError::Corrupt(format!("epoch {epoch} is not a u32")))?;
    let start = u64::try_from(start)
        .map(SampleIndex::new)
        .map_err(|_| StoreError::Corrupt(format!("start sample {start} is negative")))?;
    let end = u64::try_from(end)
        .map(SampleIndex::new)
        .map_err(|_| StoreError::Corrupt(format!("end sample {end} is negative")))?;
    let range = SampleRange::new(start, end)
        .ok_or_else(|| StoreError::Corrupt("range ends before it starts".to_owned()))?;
    let hash: [u8; 32] = hash
        .try_into()
        .map_err(|h: Vec<u8>| StoreError::Corrupt(format!("hash is {} bytes, not 32", h.len())))?;
    SegmentRow::new(track, epoch, range, Sha256Digest::new(hash))
        .ok_or_else(|| StoreError::Corrupt("range is empty".to_owned()))
}

/// The nota database.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the database at `path`.
    ///
    /// Sets `journal_mode=WAL` and `synchronous=FULL` and checks both took.
    /// A new database gets its schema; one at the current version is used
    /// as it is.
    ///
    /// # Errors
    ///
    /// [`StoreError::Pragma`] if a pragma didn't take,
    /// [`StoreError::UnknownSchema`] if the schema version isn't known, and
    /// [`StoreError::Sqlite`] for any other SQLite failure.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let mut conn = Connection::open(path)?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Pragma {
                name: "journal_mode",
                found: mode,
            });
        }
        conn.execute_batch("PRAGMA synchronous = FULL")?;
        let sync: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        if sync != 2 {
            return Err(StoreError::Pragma {
                name: "synchronous",
                found: sync.to_string(),
            });
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => {
                tx.execute_batch(CREATE_SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
            }
            SCHEMA_VERSION => drop(tx),
            other => return Err(StoreError::UnknownSchema(other)),
        }
        Ok(Self { conn })
    }

    /// Inserts a row in its own transaction, committed before returning.
    ///
    /// A row says its segment file is durable, and salvage deletes journals
    /// on its word, so only the recorder's publish step calls this, once the
    /// file is fsync'd under its final name.
    ///
    /// If a row with the same track and start sample is stored, an identical
    /// one is [`Inserted::AlreadyPresent`] and a different one is a
    /// [`StoreError::Conflict`]. So is a row of the same track whose range
    /// overlaps the new one's: segments of a track never share a sample.
    ///
    /// # Errors
    ///
    /// [`StoreError::OutOfRange`] if a sample index doesn't fit SQLite's
    /// integer, [`StoreError::Conflict`] as above, [`StoreError::Corrupt`] if
    /// the stored row can't be parsed, and [`StoreError::Sqlite`] for any
    /// other SQLite failure.
    pub fn insert_segment(&mut self, row: &SegmentRow) -> Result<Inserted, StoreError> {
        let start = to_i64(row.range.start())?;
        let end = to_i64(row.range.end())?;
        let track = i64::from(row.track.get());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                "SELECT track, epoch, start_sample, end_sample, sha256 \
                 FROM segment WHERE track = ?1 AND start_sample = ?2",
                params![track, start],
                raw_from_row,
            )
            .optional()?;
        if let Some(raw) = existing {
            let existing = parse_row(raw)?;
            return if existing == *row {
                Ok(Inserted::AlreadyPresent)
            } else {
                Err(StoreError::Conflict { existing })
            };
        }
        // Segments of one track never share a sample.
        let overlapping = tx
            .query_row(
                "SELECT track, epoch, start_sample, end_sample, sha256 \
                 FROM segment WHERE track = ?1 AND start_sample < ?3 AND end_sample > ?2 \
                 LIMIT 1",
                params![track, start, end],
                raw_from_row,
            )
            .optional()?;
        if let Some(raw) = overlapping {
            return Err(StoreError::Conflict {
                existing: parse_row(raw)?,
            });
        }
        tx.execute(
            "INSERT INTO segment (track, epoch, start_sample, end_sample, sha256) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                track,
                i64::from(row.epoch.get()),
                start,
                end,
                row.sha256.as_bytes().as_slice()
            ],
        )?;
        tx.commit()?;
        Ok(Inserted::New)
    }

    /// Every row, ordered by track then start sample.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a stored row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn segments(&self) -> Result<Vec<SegmentRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT track, epoch, start_sample, end_sample, sha256 \
             FROM segment ORDER BY track, start_sample",
        )?;
        let raws = stmt
            .query_map([], raw_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(parse_row).collect()
    }

    /// A pragma's current value on this connection, as text.
    #[cfg(test)]
    fn pragma_text(&self, name: &str) -> String {
        self.conn
            .query_row(&format!("PRAGMA {name}"), [], |r| {
                r.get::<_, rusqlite::types::Value>(0)
            })
            .map(|v| match v {
                rusqlite::types::Value::Text(t) => t,
                rusqlite::types::Value::Integer(i) => i.to_string(),
                other => format!("{other:?}"),
            })
            .unwrap()
    }
}

#[cfg(test)]
mod test_dir {
    use std::path::PathBuf;

    /// A fresh directory under the system temp dir, removed when dropped.
    #[derive(Debug)]
    pub(crate) struct TestDir(pub(crate) PathBuf);

    impl TestDir {
        #[expect(
            clippy::disallowed_methods,
            reason = "test scaffolding outside the recorder's write path"
        )]
        pub(crate) fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("nota-store-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TestDir {
        #[expect(
            clippy::disallowed_methods,
            reason = "test scaffolding outside the recorder's write path"
        )]
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_dir::TestDir;
    use super::*;

    fn row(track: u32, epoch: u32, start: u64, end: u64, hash: u8) -> SegmentRow {
        SegmentRow::new(
            TrackId::new(track),
            EpochId::new(epoch),
            SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap(),
            Sha256Digest::new([hash; 32]),
        )
        .unwrap()
    }

    fn open(dir: &TestDir) -> Store {
        Store::open(&dir.0.join("nota.db")).unwrap()
    }

    #[test]
    fn insert_then_read_and_persist() {
        let dir = TestDir::new("persist");
        let r = row(1, 0, 0, 480, 7);
        {
            let mut store = open(&dir);
            assert_eq!(store.insert_segment(&r).unwrap(), Inserted::New);
            assert_eq!(store.segments().unwrap(), vec![r]);
        }
        assert_eq!(open(&dir).segments().unwrap(), vec![r]);
    }

    #[test]
    fn pragmas_are_set() {
        let dir = TestDir::new("pragmas");
        let store = open(&dir);
        assert_eq!(store.pragma_text("journal_mode"), "wal");
        assert_eq!(store.pragma_text("synchronous"), "2");
        assert_eq!(store.pragma_text("user_version"), "1");
    }

    #[test]
    fn same_row_twice_is_stored_once() {
        let dir = TestDir::new("twice");
        let mut store = open(&dir);
        let r = row(1, 0, 0, 480, 7);
        assert_eq!(store.insert_segment(&r).unwrap(), Inserted::New);
        assert_eq!(store.insert_segment(&r).unwrap(), Inserted::AlreadyPresent);
        assert_eq!(store.segments().unwrap(), vec![r]);
    }

    #[test]
    fn different_row_at_same_start_conflicts() {
        let dir = TestDir::new("conflict");
        let mut store = open(&dir);
        let r = row(1, 0, 100, 580, 7);
        store.insert_segment(&r).unwrap();
        for other in [
            row(1, 0, 100, 581, 7),
            row(1, 1, 100, 580, 7),
            row(1, 0, 100, 580, 8),
        ] {
            match store.insert_segment(&other) {
                Err(StoreError::Conflict { existing }) => assert_eq!(existing, r),
                got => panic!("expected conflict, got {got:?}"),
            }
        }
        assert_eq!(store.segments().unwrap(), vec![r]);
    }

    #[test]
    fn overlapping_rows_of_a_track_conflict() {
        let dir = TestDir::new("overlap");
        let mut store = open(&dir);
        let r = row(1, 0, 100, 200, 7);
        store.insert_segment(&r).unwrap();
        for other in [
            row(1, 0, 50, 101, 1),
            row(1, 0, 199, 300, 1),
            row(1, 0, 120, 130, 1),
            row(1, 0, 0, 1_000, 1),
        ] {
            match store.insert_segment(&other) {
                Err(StoreError::Conflict { existing }) => assert_eq!(existing, r),
                got => panic!("expected conflict for {other:?}, got {got:?}"),
            }
        }
        // Touching is fine, and so is another track.
        for fine in [
            row(1, 0, 0, 100, 2),
            row(1, 0, 200, 300, 3),
            row(2, 0, 150, 160, 4),
        ] {
            assert_eq!(store.insert_segment(&fine).unwrap(), Inserted::New);
        }
        assert_eq!(store.segments().unwrap().len(), 4);
    }

    #[test]
    fn rows_are_ordered_by_track_then_start() {
        let dir = TestDir::new("order");
        let mut store = open(&dir);
        let rows = [
            row(2, 0, 0, 10, 1),
            row(1, 0, 20, 30, 2),
            row(1, 0, 0, 10, 3),
            row(2, 0, 10, 20, 4),
        ];
        for r in &rows {
            assert_eq!(store.insert_segment(r).unwrap(), Inserted::New);
        }
        assert_eq!(
            store.segments().unwrap(),
            vec![rows[2], rows[1], rows[0], rows[3]]
        );
    }

    #[test]
    fn empty_range_is_not_a_row() {
        let s = SampleIndex::new(5);
        let empty = SampleRange::new(s, s).unwrap();
        assert!(
            SegmentRow::new(
                TrackId::new(0),
                EpochId::new(0),
                empty,
                Sha256Digest::new([0; 32])
            )
            .is_none()
        );
    }

    #[test]
    fn samples_beyond_i64_are_out_of_range() {
        let dir = TestDir::new("range");
        let mut store = open(&dir);
        let big = u64::try_from(i64::MAX).unwrap() + 1;
        assert!(matches!(
            store.insert_segment(&row(1, 0, big, big + 5, 1)),
            Err(StoreError::OutOfRange)
        ));
        assert!(matches!(
            store.insert_segment(&row(1, 0, 0, big, 1)),
            Err(StoreError::OutOfRange)
        ));
        assert_eq!(store.segments().unwrap(), vec![]);
    }

    #[test]
    fn unknown_schema_version_is_refused() {
        let dir = TestDir::new("schema");
        let path = dir.0.join("nota.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 7")
            .unwrap();
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::UnknownSchema(7))
        ));
    }

    #[test]
    fn corrupt_rows_are_reported() {
        let cases: [(i64, i64, i64, i64, &str); 4] = [
            (-1, 0, 0, 10, "track"),
            (1 << 33, 0, 0, 10, "track"),
            (1, -5, 0, 10, "epoch"),
            (1, 1 << 40, 0, 10, "epoch"),
        ];
        for (i, (track, epoch, start, end, what)) in cases.into_iter().enumerate() {
            let dir = TestDir::new(&format!("corrupt{i}"));
            let store = open(&dir);
            store
                .conn
                .execute(
                    "INSERT INTO segment VALUES (?1, ?2, ?3, ?4, zeroblob(32))",
                    params![track, epoch, start, end],
                )
                .unwrap();
            match store.segments() {
                Err(StoreError::Corrupt(msg)) => assert!(msg.contains(what), "{msg}"),
                got => panic!("expected corrupt, got {got:?}"),
            }
        }
    }

    #[test]
    fn checks_stop_bad_ranges_and_hashes() {
        let dir = TestDir::new("checks");
        let store = open(&dir);
        for sql in [
            "INSERT INTO segment VALUES (1, 0, 10, 10, zeroblob(32))",
            "INSERT INTO segment VALUES (1, 0, -1, 10, zeroblob(32))",
            "INSERT INTO segment VALUES (1, 0, 0, 10, zeroblob(31))",
        ] {
            assert!(store.conn.execute(sql, []).is_err(), "{sql}");
        }
    }

    #[test]
    fn error_display_is_specific() {
        use std::error::Error as _;
        let existing = row(3, 0, 42, 50, 1);
        let cases: [(StoreError, &str); 6] = [
            (StoreError::Sqlite(rusqlite::Error::InvalidQuery), "sqlite"),
            (
                StoreError::Pragma {
                    name: "journal_mode",
                    found: "delete".to_owned(),
                },
                "journal_mode",
            ),
            (StoreError::UnknownSchema(7), "7"),
            (StoreError::OutOfRange, "64-bit"),
            (StoreError::Conflict { existing }, "42"),
            (StoreError::Corrupt("bad hash".to_owned()), "bad hash"),
        ];
        for (err, needle) in cases {
            let text = err.to_string();
            assert!(text.contains(needle), "{text}");
        }
        assert!(
            StoreError::Sqlite(rusqlite::Error::InvalidQuery)
                .source()
                .is_some()
        );
        assert!(StoreError::OutOfRange.source().is_none());
    }
}
