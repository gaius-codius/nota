//! A session's vocabulary: the packs it chose and the learned pairs, as the
//! plain term list the lookup (C4) takes.
//!
//! A session records its packs once, when it starts, by each pack's name
//! and version ([`PackRef`]), so a proposal made from its terms can say
//! where each came from. Reading its vocabulary later takes the packs as
//! they are installed and checks each recorded one is there at its
//! recorded version: a pack upgraded since gives other terms, so it's
//! refused, not quietly used.

use rusqlite::{TransactionBehavior, params};

use nota_core::SessionId;

use crate::learned::Learned;
use crate::packs::{Offered, Pack, PackRef};
use crate::{Store, StoreError, session_key};

/// The terms a session's lookup may offer, and the user's learned pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTerms {
    /// The terms the session's packs offer, pack by pack in the order the
    /// session chose them, each pack's sorted by term. A term two packs
    /// have comes once from each, with its own origin.
    pub terms: Vec<Offered>,
    /// Every learned pair in the library: the exact matches, and the terms
    /// never to offer for a span.
    pub learned: Learned,
}

impl SessionTerms {
    /// Whether `term` may be offered for the span `heard`: false once the
    /// user rejected it for that span ([`Learned::allows`]).
    #[must_use]
    pub fn allows(&self, heard: &str, term: &str) -> bool {
        self.learned.allows(heard, term)
    }
}

impl Store {
    /// Records that `session` uses `packs`, in this order. A session's packs
    /// are recorded once, and never changed.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoSession`] if the session isn't in the library,
    /// [`StoreError::PacksRecorded`] if its packs are recorded already,
    /// [`StoreError::OutOfRange`] if its number doesn't fit SQLite's
    /// integer, and [`StoreError::Sqlite`] for any SQLite failure,
    /// including one pack named twice. Nothing is recorded then.
    pub fn record_session_packs(
        &mut self,
        session: SessionId,
        packs: &[PackRef],
    ) -> Result<(), StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !crate::segments::session_exists(&tx, key)? {
            return Err(StoreError::NoSession(session));
        }
        let recorded: i64 = tx.query_row(
            "SELECT count(*) FROM session_pack WHERE session_id = ?1",
            [key],
            |r| r.get(0),
        )?;
        if recorded > 0 {
            return Err(StoreError::PacksRecorded(session));
        }
        for (position, pack) in (0_i64..).zip(packs) {
            tx.execute(
                "INSERT INTO session_pack (session_id, position, name, version)
                 VALUES (?1, ?2, ?3, ?4)",
                params![key, position, pack.name, pack.version],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The packs `session` recorded, in the order it chose them; none for a
    /// session that chose none, or isn't in the library.
    ///
    /// # Errors
    ///
    /// [`StoreError::OutOfRange`] if the session's number doesn't fit
    /// SQLite's integer, and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn session_packs(&self, session: SessionId) -> Result<Vec<PackRef>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT name, version FROM session_pack WHERE session_id = ?1 ORDER BY position",
        )?;
        let packs = stmt
            .query_map([session_key(session)?], |r| {
                Ok(PackRef {
                    name: r.get(0)?,
                    version: r.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(packs)
    }

    /// `session`'s vocabulary: the terms its recorded packs offer, taken
    /// from `installed` (the packs on the machine, any others among them
    /// ignored), and the library's learned pairs.
    ///
    /// # Errors
    ///
    /// [`StoreError::PackMissing`] if a pack the session recorded isn't in
    /// `installed` at its recorded version, and [`Store::session_packs`]'s
    /// and [`Store::learned`]'s errors.
    pub fn session_terms(
        &self,
        session: SessionId,
        installed: &[Pack],
    ) -> Result<SessionTerms, StoreError> {
        let mut terms = Vec::new();
        for chosen in self.session_packs(session)? {
            let pack = installed
                .iter()
                .find(|p| p.manifest().pack_ref() == chosen)
                .ok_or_else(|| StoreError::PackMissing {
                    session,
                    pack: chosen.clone(),
                })?;
            terms.extend(pack.offered());
        }
        Ok(SessionTerms {
            terms,
            learned: self.learned()?,
        })
    }
}

#[cfg(test)]
mod tests;
