//! Marks and notes made while recording, each stored as it's made, in
//! session time.

use nota_core::recorder::{Mark, Note};
use nota_core::{SessionId, SessionTime};
use rusqlite::params;

use crate::segments::session_exists;
use crate::{Store, StoreError, session_key};

/// A mark or a note, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    /// A mark (◆).
    Mark(Mark),
    /// A note (◇).
    Note(Note),
}

impl Annotation {
    /// The moment it's pinned to.
    #[must_use]
    pub const fn at(&self) -> SessionTime {
        match self {
            Self::Mark(mark) => mark.at,
            Self::Note(note) => note.at(),
        }
    }
}

fn nanos(at: SessionTime) -> Result<i64, StoreError> {
    i64::try_from(at.as_nanos()).map_err(|_| StoreError::OutOfRange)
}

impl Store {
    /// Stores `annotation` as one of the session's, committed before
    /// returning.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library,
    /// [`StoreError::OutOfRange`] if its time doesn't fit SQLite's integer,
    /// and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn add_annotation(
        &mut self,
        session: SessionId,
        annotation: &Annotation,
    ) -> Result<(), StoreError> {
        let key = session_key(session)?;
        let at = nanos(annotation.at())?;
        if !session_exists(&self.conn, key)? {
            return Err(StoreError::NoSession(session));
        }
        match annotation {
            Annotation::Mark(_) => self.conn.execute(
                "INSERT INTO mark (session_id, at_ns) VALUES (?1, ?2)",
                params![key, at],
            )?,
            Annotation::Note(note) => self.conn.execute(
                "INSERT INTO note (session_id, at_ns, text) VALUES (?1, ?2, ?3)",
                params![key, at, note.text()],
            )?,
        };
        Ok(())
    }

    /// The session's marks and notes, in session order; at one moment,
    /// marks before notes, each in the order they were made.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse (a negative time, a
    /// blank note), and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn annotations(&self, session: SessionId) -> Result<Vec<Annotation>, StoreError> {
        let key = session_key(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT at_ns, NULL, 0, id FROM mark WHERE session_id = ?1 \
             UNION ALL SELECT at_ns, text, 1, id FROM note WHERE session_id = ?1 \
             ORDER BY 1, 3, 4",
        )?;
        let raws = stmt
            .query_map([key], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter()
            .map(|(at, text)| {
                let at = u64::try_from(at)
                    .map(SessionTime::from_nanos)
                    .map_err(|_| StoreError::Corrupt(format!("annotation at {at} ns")))?;
                match text {
                    None => Ok(Annotation::Mark(Mark { at })),
                    Some(text) => Note::new(at, &text)
                        .map(Annotation::Note)
                        .ok_or_else(|| StoreError::Corrupt("a blank note".to_owned())),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
