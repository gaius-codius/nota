//! A session's vocabulary: the packs it chose and the learned pairs, as the
//! plain term list the lookup (C4) takes.
//!
//! A session records its packs once, when it starts, by each pack's name,
//! version and digest ([`PackRef`]), so a proposal made from its terms can
//! say where each came from. Reading its vocabulary later takes the packs
//! as they are installed and checks each recorded one is there with its
//! recorded name, version and digest: a pack upgraded since, or rebuilt
//! under the same version with other rows or as another kind, gives other
//! terms, so it's refused as missing, not quietly used.
//!
//! A session recorded before digests were (library version 9) has none
//! for its packs, and its rows are never changed, so its packs are matched
//! by name and version alone, as they were then ([`RecordedPack`]).

use rusqlite::{TransactionBehavior, params};

use nota_core::SessionId;

use crate::learned::Learned;
use crate::packs::{Offered, Pack, PackDigest, PackRef};
use crate::{Store, StoreError, session_key};

/// A pack as a session's record holds it: what [`PackRef`] holds, but
/// with no digest for a session recorded before digests were.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordedPack {
    /// The pack's name.
    pub name: String,
    /// Its version.
    pub version: String,
    /// Its digest, or `None` for a session recorded before digests were
    /// (library version 9).
    pub digest: Option<PackDigest>,
}

impl RecordedPack {
    /// Whether `pack` is the one recorded: the same name, version and
    /// digest, or for a record with no digest, the same name and version.
    #[must_use]
    pub fn is(&self, pack: &PackRef) -> bool {
        self.name == pack.name()
            && self.version == pack.version()
            && self.digest.is_none_or(|d| d == pack.digest())
    }
}

impl From<&PackRef> for RecordedPack {
    fn from(pack: &PackRef) -> Self {
        Self {
            name: pack.name().to_owned(),
            version: pack.version().to_owned(),
            digest: Some(pack.digest()),
        }
    }
}

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
    /// Records that `session` uses `packs`, in this order, none included. A
    /// session's packs are recorded once, and never changed.
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
            "SELECT count(*) FROM session_packs_chosen WHERE session_id = ?1",
            [key],
            |r| r.get(0),
        )?;
        if recorded > 0 {
            return Err(StoreError::PacksRecorded(session));
        }
        // Marks the choice made, so a choice of none is recorded too.
        tx.execute(
            "INSERT INTO session_packs_chosen (session_id) VALUES (?1)",
            [key],
        )?;
        for (position, pack) in (0_i64..).zip(packs) {
            tx.execute(
                "INSERT INTO session_pack (session_id, position, name, version, digest)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    key,
                    position,
                    pack.name(),
                    pack.version(),
                    pack.digest().as_bytes()
                ],
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
    /// [`StoreError::Corrupt`] if a stored digest isn't 32 bytes,
    /// [`StoreError::OutOfRange`] if the session's number doesn't fit
    /// SQLite's integer, and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn session_packs(&self, session: SessionId) -> Result<Vec<RecordedPack>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT name, version, digest FROM session_pack \
             WHERE session_id = ?1 ORDER BY position",
        )?;
        let raws = stmt
            .query_map([session_key(session)?], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raws.into_iter()
            .map(|(name, version, digest)| {
                let digest = digest
                    .map(|bytes| {
                        PackDigest::from_stored(&bytes).ok_or_else(|| {
                            StoreError::Corrupt(format!("a pack digest of {} bytes", bytes.len()))
                        })
                    })
                    .transpose()?;
                Ok(RecordedPack {
                    name,
                    version,
                    digest,
                })
            })
            .collect()
    }

    /// `session`'s vocabulary: the terms its recorded packs offer, taken
    /// from `installed` (the packs on the machine, any others among them
    /// ignored), and the library's learned pairs.
    ///
    /// # Errors
    ///
    /// [`StoreError::PackMissing`] if a pack the session recorded isn't in
    /// `installed` with its recorded name, version and digest
    /// ([`RecordedPack::is`]), and [`Store::session_packs`]'s and
    /// [`Store::learned`]'s errors.
    pub fn session_terms(
        &self,
        session: SessionId,
        installed: &[Pack],
    ) -> Result<SessionTerms, StoreError> {
        let mut terms = Vec::new();
        for chosen in self.session_packs(session)? {
            let pack = installed_as_recorded(&chosen, installed).ok_or_else(|| {
                StoreError::PackMissing {
                    session,
                    pack: chosen.clone(),
                }
            })?;
            terms.extend(pack.offered());
        }
        Ok(SessionTerms {
            terms,
            learned: self.learned()?,
        })
    }
}

/// The pack in `installed` that `chosen` recorded, if there's one. A
/// record with no digest that two installed builds with other digests
/// both match has none: which of them the session used can't be known,
/// and taking whichever comes first would hang on the order.
fn installed_as_recorded<'a>(chosen: &RecordedPack, installed: &'a [Pack]) -> Option<&'a Pack> {
    let mut matching = installed.iter().filter(|p| chosen.is(&p.pack_ref()));
    let first = matching.next()?;
    matching
        .all(|p| p.digest() == first.digest())
        .then_some(first)
}

#[cfg(test)]
mod tests;
