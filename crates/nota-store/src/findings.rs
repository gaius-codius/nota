//! The findings index: what each session's findings file says, so the
//! library can list the sessions that need the user without reading every
//! session's directory.
//!
//! The recorder keeps the findings in the session's directory (its
//! `segment::findings` module), written durably whether or not the library
//! database is there. That file is the record. This table is an index of
//! it: [`Store::index_findings`] replaces a session's entries with what its
//! file says, in one transaction.

use std::collections::BTreeMap;

use nota_core::{EpochId, SampleIndex, SampleRange, SessionId, TrackId};
use rusqlite::{TransactionBehavior, params};

use crate::segments::{AudioDigest, RowKey, SegmentRow, Sha256Digest, session_exists};
use crate::{Store, StoreError, parse_session, session_key};

/// What's wrong with a committed row's file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Problem {
    /// There's no file under the row's name in the session directory.
    Missing,
    /// The file isn't the row's: its SHA-256 isn't the row's, and if the
    /// row has an audio digest, its decoded audio isn't the row's either
    /// (or doesn't decode).
    HashMismatch,
    /// The file's SHA-256 is the row's, but its FLAC header doesn't declare
    /// the row's number of samples.
    LengthMismatch,
    /// There's something under the row's name, but reading it failed. The
    /// error may be transient.
    Unreadable(ReadFailure),
}

/// Why a row's file couldn't be read: the error's kind, as far as a finding
/// keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReadFailure {
    /// `EACCES` or `EPERM`.
    PermissionDenied,
    /// A directory is under the row's name.
    IsADirectory,
    /// Any other error (`EIO`, for one).
    Other,
}

/// Whether a finding has been dealt with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    /// Not yet. While a row's file fails the check, the row claims nothing
    /// and its samples stay in journals.
    Unresolved,
    /// A later check found the row proven by its file (or, for a row that
    /// didn't parse, found it parsing) without nota changing anything: the
    /// error was transient, or someone put the file back.
    SinceVerified,
    /// nota rebuilt the row's file from its journals, with proof that they
    /// hold exactly the row's audio. A file that was there and didn't match
    /// was kept, renamed aside, first.
    Repaired,
}

/// One entry of a session's findings, as the index holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexedFinding {
    /// A committed row whose file didn't prove it.
    Row {
        /// The row, as committed.
        row: SegmentRow,
        /// What was found under its name.
        problem: Problem,
        /// Whether it's been dealt with.
        status: Status,
    },
    /// A row that doesn't parse, by where it is.
    Unparsable {
        /// Its track and first sample, as stored.
        key: RowKey,
        /// Whether it's been dealt with.
        status: Status,
    },
}

impl IndexedFinding {
    /// Whether it's been dealt with.
    #[must_use]
    pub const fn status(&self) -> Status {
        match self {
            Self::Row { status, .. } | Self::Unparsable { status, .. } => *status,
        }
    }
}

const fn problem_text(problem: Problem) -> &'static str {
    match problem {
        Problem::Missing => "missing",
        Problem::HashMismatch => "hash mismatch",
        Problem::LengthMismatch => "length mismatch",
        Problem::Unreadable(ReadFailure::PermissionDenied) => "unreadable: permission denied",
        Problem::Unreadable(ReadFailure::IsADirectory) => "unreadable: a directory",
        Problem::Unreadable(ReadFailure::Other) => "unreadable",
    }
}

/// The problem an unparsable row is indexed under.
const UNPARSABLE: &str = "unparsable";

fn parse_problem(text: &str) -> Option<Problem> {
    [
        Problem::Missing,
        Problem::HashMismatch,
        Problem::LengthMismatch,
        Problem::Unreadable(ReadFailure::PermissionDenied),
        Problem::Unreadable(ReadFailure::IsADirectory),
        Problem::Unreadable(ReadFailure::Other),
    ]
    .into_iter()
    .find(|p| problem_text(*p) == text)
}

const fn status_text(status: Status) -> &'static str {
    match status {
        Status::Unresolved => "unresolved",
        Status::SinceVerified => "since verified",
        Status::Repaired => "repaired",
    }
}

fn parse_status(text: &str) -> Option<Status> {
    [Status::Unresolved, Status::SinceVerified, Status::Repaired]
        .into_iter()
        .find(|s| status_text(*s) == text)
}

/// An entry as SQLite holds it: track, start, end, epoch, SHA-256, audio
/// digest, problem, status.
type RawFinding = (
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    String,
    String,
);

fn corrupt(why: &str) -> StoreError {
    StoreError::Corrupt(format!("finding: {why}"))
}

fn parse_finding(
    (track, start, end, epoch, sha256, audio, problem, status): RawFinding,
) -> Result<IndexedFinding, StoreError> {
    let status = parse_status(&status).ok_or_else(|| corrupt("unknown status"))?;
    if problem == UNPARSABLE {
        return Ok(IndexedFinding::Unparsable {
            key: RowKey { track, start },
            status,
        });
    }
    let problem = parse_problem(&problem).ok_or_else(|| corrupt("unknown problem"))?;
    let number = |n: Option<i64>| n.and_then(|n| u64::try_from(n).ok());
    let range = SampleRange::new(
        SampleIndex::new(number(Some(start)).ok_or_else(|| corrupt("start"))?),
        SampleIndex::new(number(end).ok_or_else(|| corrupt("end"))?),
    )
    .ok_or_else(|| corrupt("range"))?;
    let small = |n: Option<i64>| n.and_then(|n| u32::try_from(n).ok());
    let hash = |h: Option<Vec<u8>>| h.and_then(|h| <[u8; 32]>::try_from(h).ok());
    let mut row = SegmentRow::new(
        TrackId::new(small(Some(track)).ok_or_else(|| corrupt("track"))?),
        EpochId::new(small(epoch).ok_or_else(|| corrupt("epoch"))?),
        range,
        Sha256Digest::new(hash(sha256).ok_or_else(|| corrupt("hash"))?),
    )
    .ok_or_else(|| corrupt("range"))?;
    if let Some(audio) = audio {
        let audio = hash(Some(audio)).ok_or_else(|| corrupt("audio digest"))?;
        row = row.with_audio(AudioDigest::new(audio));
    }
    Ok(IndexedFinding::Row {
        row,
        problem,
        status,
    })
}

fn to_i64(n: u64) -> Result<i64, StoreError> {
    i64::try_from(n).map_err(|_| StoreError::OutOfRange)
}

impl Store {
    /// Replaces `session`'s entries in the findings index with `findings`,
    /// what its findings file says, in one transaction.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library,
    /// [`StoreError::OutOfRange`] if a number doesn't fit SQLite's integer,
    /// and [`StoreError::Sqlite`] for any SQLite failure. Nothing changes
    /// then.
    pub fn index_findings(
        &mut self,
        session: SessionId,
        findings: &[IndexedFinding],
    ) -> Result<(), StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !session_exists(&tx, key)? {
            return Err(StoreError::NoSession(session));
        }
        tx.execute("DELETE FROM finding WHERE session_id = ?1", [key])?;
        for finding in findings {
            match finding {
                IndexedFinding::Row {
                    row,
                    problem,
                    status,
                } => tx.execute(
                    "INSERT INTO finding (session_id, track, start_sample, end_sample, epoch, \
                     sha256, audio_digest, problem, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        key,
                        i64::from(row.track().get()),
                        to_i64(row.range().start().get())?,
                        to_i64(row.range().end().get())?,
                        i64::from(row.epoch().get()),
                        row.sha256().as_bytes().as_slice(),
                        row.audio().as_ref().map(|a| a.as_bytes().as_slice()),
                        problem_text(*problem),
                        status_text(*status),
                    ],
                )?,
                IndexedFinding::Unparsable { key: row, status } => tx.execute(
                    "INSERT INTO finding (session_id, track, start_sample, problem, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![key, row.track, row.start, UNPARSABLE, status_text(*status)],
                )?,
            };
        }
        tx.commit()?;
        Ok(())
    }

    /// `session`'s entries in the findings index, by track then first
    /// sample.
    ///
    /// # Errors
    ///
    /// [`StoreError::OutOfRange`] if the session's number doesn't fit
    /// SQLite's integer, [`StoreError::Corrupt`] if an entry doesn't parse,
    /// and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn findings(&self, session: SessionId) -> Result<Vec<IndexedFinding>, StoreError> {
        let key = session_key(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT track, start_sample, end_sample, epoch, sha256, audio_digest, problem, \
             status FROM finding WHERE session_id = ?1 \
             ORDER BY track, start_sample, end_sample, problem",
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
                    r.get(7)?,
                ))
            })?
            .collect::<Result<Vec<RawFinding>, _>>()?;
        raws.into_iter().map(parse_finding).collect()
    }

    /// How many unresolved findings each session has in the index, for
    /// those with any.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a session number is negative, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn unresolved_findings(&self) -> Result<BTreeMap<SessionId, usize>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, count(*) FROM finding WHERE status = ?1 GROUP BY session_id",
        )?;
        let raws = stmt
            .query_map([status_text(Status::Unresolved)], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter()
            .map(|(session, n)| {
                Ok((
                    parse_session(session)?,
                    usize::try_from(n).map_err(|_| corrupt("count"))?,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
