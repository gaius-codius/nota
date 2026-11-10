//! Learned pairs: what the user chose on a heard span, kept for every later
//! session.
//!
//! When the user picks a term for a span the engine got wrong, or types one
//! (the correction card), the span and the term are an **accepted** pair:
//! next time that span is heard, the term is an exact match, checked before
//! any fuzzy lookup. When the user keeps what was heard instead, the span
//! and each term offered for it are a **rejected** pair: that term is never
//! offered for that span again.
//!
//! Pairs hold across the library, not one session: they're the user's own
//! vocabulary. A pair is never changed or deleted; a later choice on the
//! same span and term is a new row, and the newest decides. So a term the
//! user rejected and later typed in by hand is accepted from then on, and
//! one accepted and later rejected is offered no more.
//!
//! Spans and terms are matched by [`match_key`], which drops case,
//! punctuation and spacing, so "Endside," and "endside" are one span. The
//! lookup and the clean-up job ask [`Learned::allows`] before offering a
//! term for a span; nothing else needs to know the rule.

use std::collections::BTreeMap;

use nota_core::{SessionId, WallTime};
use rusqlite::{TransactionBehavior, params};

use crate::{Store, StoreError, parse_session, session_key};

/// What the user chose for a heard span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Verdict {
    /// The term is what was meant.
    Accepted,
    /// The term isn't what was meant: never offer it for the span again.
    Rejected,
}

impl Verdict {
    /// The verdict as stored.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    /// The verdict stored as `text`.
    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            other => Err(StoreError::Corrupt(format!("unknown verdict {other:?}"))),
        }
    }
}

/// The form spans and terms are matched in: lower case, with every run of
/// characters other than letters, digits and apostrophes made one space,
/// and none at either end. "GLP-1" and "glp 1" match; so do "Endside," and
/// "endside".
#[must_use]
pub fn match_key(text: &str) -> String {
    // Lower case first: some capitals lower to a letter and a mark ("İ" to
    // "i" and a combining dot), and splitting after that keeps a key the
    // same when it's keyed again.
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One choice the user made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearnedPair {
    /// The span as it was heard.
    pub heard: String,
    /// The term chosen for it, or rejected.
    pub term: String,
    /// Which.
    pub verdict: Verdict,
    /// The session it was made in.
    pub session: SessionId,
    /// When.
    pub at: WallTime,
}

/// Every pair the user has made, and what holds for each span and term.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Learned {
    /// Every pair, oldest first.
    history: Vec<LearnedPair>,
    /// For each span and term (as [`match_key`]s), the newest verdict and
    /// the term as last written.
    newest: BTreeMap<(String, String), (Verdict, String)>,
}

impl Learned {
    /// The pairs in `history`, oldest first.
    fn new(history: Vec<LearnedPair>) -> Self {
        let mut newest = BTreeMap::new();
        for pair in &history {
            newest.insert(
                (match_key(&pair.heard), match_key(&pair.term)),
                (pair.verdict, pair.term.clone()),
            );
        }
        Self { history, newest }
    }

    /// Every pair the user has made, oldest first.
    #[must_use]
    pub fn history(&self) -> &[LearnedPair] {
        &self.history
    }

    /// Whether `term` may be offered for the span `heard`: false if the
    /// newest choice on them rejected it.
    #[must_use]
    pub fn allows(&self, heard: &str, term: &str) -> bool {
        !matches!(
            self.newest.get(&(match_key(heard), match_key(term))),
            Some((Verdict::Rejected, _))
        )
    }

    /// The terms accepted for the span `heard`, as last written, in order
    /// of their keys: the exact matches the lookup checks first.
    #[must_use]
    pub fn accepted_for(&self, heard: &str) -> Vec<&str> {
        let heard = match_key(heard);
        self.newest
            .range((heard.clone(), String::new())..)
            .take_while(|((h, _), _)| *h == heard)
            .filter(|(_, (verdict, _))| *verdict == Verdict::Accepted)
            .map(|(_, (_, term))| term.as_str())
            .collect()
    }

    /// Every span and the term accepted for it, as [`match_key`] of the
    /// span and the term as last written, in order: what the lookup turns
    /// into exact matches.
    #[must_use]
    pub fn accepted(&self) -> Vec<(&str, &str)> {
        self.newest
            .iter()
            .filter(|(_, (verdict, _))| *verdict == Verdict::Accepted)
            .map(|((heard, _), (_, term))| (heard.as_str(), term.as_str()))
            .collect()
    }
}

impl Store {
    /// Records that in `session`, at `at`, the user gave `verdict` on
    /// `term` for the span `heard`. Committed before it returns.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoText`] if the span or the term has no letters or
    /// digits, [`StoreError::NoSession`] if the session isn't in the
    /// library, [`StoreError::OutOfRange`] if its number doesn't fit
    /// SQLite's integer, and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn learn(
        &mut self,
        session: SessionId,
        heard: &str,
        term: &str,
        verdict: Verdict,
        at: WallTime,
    ) -> Result<(), StoreError> {
        let (heard_key, term_key) = (match_key(heard), match_key(term));
        if heard_key.is_empty() || term_key.is_empty() {
            return Err(StoreError::NoText);
        }
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !crate::segments::session_exists(&tx, key)? {
            return Err(StoreError::NoSession(session));
        }
        tx.execute(
            "INSERT INTO learned_pair (session_id, heard, heard_key, term, term_key, verdict, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                key,
                heard,
                heard_key,
                term,
                term_key,
                verdict.as_str(),
                at.unix_seconds()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every learned pair in the library, and what holds for each span and
    /// term.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a row doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn learned(&self) -> Result<Learned, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT heard, term, verdict, session_id, at FROM learned_pair ORDER BY id")?;
        let raws = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let history = raws
            .into_iter()
            .map(|(heard, term, verdict, session, at)| {
                Ok(LearnedPair {
                    heard,
                    term,
                    verdict: Verdict::parse(&verdict)?,
                    session: parse_session(session)?,
                    at: WallTime::from_unix_seconds(at)
                        .ok_or_else(|| StoreError::Corrupt(format!("learned at {at}")))?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(Learned::new(history))
    }
}

#[cfg(test)]
mod tests;
