//! Sessions and their tracks.
//!
//! A session's number is its directory's (`sessions/<n>/`), so the
//! database and the disk name it the same way.

use nota_core::{SessionId, TrackId};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::segments::{parse_track, session_exists};
use crate::{Store, StoreError, parse_session, session_key};

/// Where a session is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Being recorded, or a recording that never finished (a crash, a
    /// killed process): its journals may need salvage.
    Recording,
    /// Recording has stopped.
    Stopped,
}

impl SessionState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Stopped => "stopped",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "recording" => Ok(Self::Recording),
            "stopped" => Ok(Self::Stopped),
            other => Err(StoreError::Corrupt(format!(
                "unknown session state {other:?}"
            ))),
        }
    }
}

/// What a track records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    /// A microphone: the room.
    Microphone,
    /// The system's audio output: what the computer plays.
    System,
}

impl TrackKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::System => "system",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "microphone" => Ok(Self::Microphone),
            "system" => Ok(Self::System),
            other => Err(StoreError::Corrupt(format!("unknown track kind {other:?}"))),
        }
    }
}

/// One of a session's tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// The track's number in the session.
    pub track: TrackId,
    /// What it records.
    pub kind: TrackKind,
    /// The device or node it was recorded from, as the user would know it.
    pub source: Option<String>,
}

/// A session to add to the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    /// Its number, the same as its directory's.
    pub id: SessionId,
    /// Its title, if it has one.
    pub title: Option<String>,
    /// The language spoken, as a BCP 47 tag (`en`, `de-CH`), if known.
    pub language: Option<String>,
    /// Its tracks, each with a different number.
    pub tracks: Vec<Track>,
}

/// A session in the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Its number, the same as its directory's.
    pub id: SessionId,
    /// Its title, if it has one.
    pub title: Option<String>,
    /// The language spoken, as a BCP 47 tag, if known.
    pub language: Option<String>,
    /// Where it is in its life.
    pub state: SessionState,
}

type RawSession = (i64, Option<String>, Option<String>, String);

fn parse_session_row((id, title, language, state): RawSession) -> Result<Session, StoreError> {
    Ok(Session {
        id: parse_session(id)?,
        title,
        language,
        state: SessionState::parse(&state)?,
    })
}

fn raw_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSession> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

/// Adds the session's row, in `state`, inside the caller's transaction.
pub(crate) fn insert_session(
    conn: &rusqlite::Connection,
    id: SessionId,
    title: Option<&str>,
    language: Option<&str>,
    state: SessionState,
) -> Result<(), StoreError> {
    let key = session_key(id)?;
    if session_exists(conn, key)? {
        return Err(StoreError::SessionExists(id));
    }
    conn.execute(
        "INSERT INTO session (id, title, language, state) VALUES (?1, ?2, ?3, ?4)",
        params![key, title, language, state.as_str()],
    )?;
    Ok(())
}

impl Store {
    /// Adds a session and its tracks, in [`SessionState::Recording`], in
    /// one transaction committed before returning.
    ///
    /// # Errors
    ///
    /// [`StoreError::SessionExists`] if the session is there already,
    /// [`StoreError::OutOfRange`] if its number doesn't fit SQLite's
    /// integer, and [`StoreError::Sqlite`] for any other SQLite failure,
    /// including two tracks with one number.
    pub fn create_session(&mut self, new: &NewSession) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_session(
            &tx,
            new.id,
            new.title.as_deref(),
            new.language.as_deref(),
            SessionState::Recording,
        )?;
        let key = session_key(new.id)?;
        for track in &new.tracks {
            tx.execute(
                "INSERT INTO track (session_id, track, kind, source) VALUES (?1, ?2, ?3, ?4)",
                params![
                    key,
                    i64::from(track.track.get()),
                    track.kind.as_str(),
                    track.source.as_deref()
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The session numbered `id`, if it's in the library.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if its row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn session(&self, id: SessionId) -> Result<Option<Session>, StoreError> {
        self.conn
            .query_row(
                "SELECT id, title, language, state FROM session WHERE id = ?1",
                [session_key(id)?],
                raw_session,
            )
            .optional()?
            .map(parse_session_row)
            .transpose()
    }

    /// Every session in the library, in number order.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn sessions(&self) -> Result<Vec<Session>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, title, language, state FROM session ORDER BY id")?;
        let raws = stmt
            .query_map([], raw_session)?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter().map(parse_session_row).collect()
    }

    /// Moves the session to `state`, committed before returning.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if it isn't in the library, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn set_state(&mut self, id: SessionId, state: SessionState) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE session SET state = ?2 WHERE id = ?1",
            params![session_key(id)?, state.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::NoSession(id));
        }
        Ok(())
    }

    /// The session's tracks, in number order.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn tracks(&self, id: SessionId) -> Result<Vec<Track>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT track, kind, source FROM track WHERE session_id = ?1 ORDER BY track",
        )?;
        let raws = stmt
            .query_map([session_key(id)?], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter()
            .map(|(track, kind, source)| {
                Ok(Track {
                    track: parse_track(track)?,
                    kind: TrackKind::parse(&kind)?,
                    source,
                })
            })
            .collect()
    }
}
