//! The revisions of the text that's shown: the final pass's text, and the
//! changes made to it since.
//!
//! **Revision 0 is the final text.** A session's first final-text row
//! ([`crate::final_text`]) makes it, and it has no rows of its own. A
//! session with no final text has no revisions; what to show for one (its
//! heard text, [`crate::transcript`]) is the caller's choice.
//!
//! **Every later revision is its parent plus changes.** A change names a
//! chunk of the final text (its track and first sample) and a run of that
//! chunk's words by position, and gives the text that replaces them.
//! Positions are always the final text's own words, which never change, so
//! a change means the same words in every revision. Reading a revision
//! walks back from it to revision 0, and the nearest revision's changes
//! win: an older change is left out if it touches any word a nearer one
//! changed. Undoing a change is a new revision whose change puts the words
//! back. What a revision changes is never changed or added to afterwards;
//! the schema's triggers (version 8) refuse it whatever connection tries.
//!
//! **In session order.** A revision's lines are placed in session time by
//! their epoch's anchor: the epoch of the segment holding the chunk's first
//! sample (a chunk never spans two epochs). A chunk whose epoch has no
//! anchor (audio from journals that didn't keep one), or whose track's
//! anchors don't rebuild into a timeline, has no time; those come after
//! the timed ones, by track and sample.

use std::collections::BTreeMap;

use nota_core::{EpochAnchor, SampleIndex, SessionId, SessionTime, TrackId, TrackTimeline};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::final_text::{FinalText, FinalWord, sample, texts_in};
use crate::segments::parse_track;
use crate::{SegmentRow, Store, StoreError, session_key};

/// A revision of a session's text. Revision 0 is the final pass's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RevisionNumber(u32);

impl RevisionNumber {
    /// The final pass's text.
    pub const FINAL: Self = Self(0);

    /// The number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A chunk of the final pass's text: its track and first sample, as
/// stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkKey {
    /// The chunk's track.
    pub track: TrackId,
    /// Its first sample.
    pub start: SampleIndex,
}

impl FinalText {
    /// The chunk this text is stored as.
    #[must_use]
    pub const fn key(&self) -> ChunkKey {
        ChunkKey {
            track: self.track,
            start: self.range.start(),
        }
    }
}

/// A run of a chunk's words, by position: `first` to `last`, both
/// included, so never empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WordSpan {
    first: u32,
    last: u32,
}

impl WordSpan {
    /// The words from position `first` to `last`, both included. `None`
    /// if `last` comes before `first`.
    #[must_use]
    pub const fn new(first: u32, last: u32) -> Option<Self> {
        if last < first {
            None
        } else {
            Some(Self { first, last })
        }
    }

    /// The first word's position.
    #[must_use]
    pub const fn first(self) -> u32 {
        self.first
    }

    /// The last word's position.
    #[must_use]
    pub const fn last(self) -> u32 {
        self.last
    }

    /// Whether the two runs share a word.
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.first <= other.last && other.first <= self.last
    }
}

/// A change a new revision makes: `words` of `chunk` read as `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The chunk.
    pub chunk: ChunkKey,
    /// Its words that change.
    pub words: WordSpan,
    /// What they read as.
    pub text: String,
}

/// A change as a revision shows it, and the revision that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The chunk's words it changes.
    pub words: WordSpan,
    /// What they read as.
    pub text: String,
    /// The revision that made the change: the one read, or an ancestor.
    pub revision: RevisionNumber,
}

/// When a chunk was said, in session time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timed {
    /// When its first sample played.
    pub start: SessionTime,
    /// When its audio ended.
    pub end: SessionTime,
}

/// One chunk of the final text as a revision shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalLine {
    /// The chunk as the final pass stored it.
    pub heard: FinalText,
    /// When it was said; `None` if its epoch's anchor isn't stored.
    pub at: Option<Timed>,
    /// Its text in the revision: as stored if nothing changes it,
    /// otherwise its words, with each change's text in place of the words
    /// it changes, joined by single spaces. `None` for audio the engine
    /// couldn't transcribe.
    pub text: Option<String>,
    /// The changes the revision shows in it, by first word.
    pub changes: Vec<Applied>,
}

/// Makes the session's revision 0, the final text, if it hasn't one.
pub(crate) fn make_final_revision(conn: &Connection, key: i64) -> Result<(), StoreError> {
    conn.execute(
        // Not ON CONFLICT DO NOTHING: the never-replaced trigger fires
        // before the conflict is seen.
        "INSERT INTO revision (session_id, number, parent) SELECT ?1, 0, NULL \
         WHERE NOT EXISTS (SELECT 1 FROM revision WHERE session_id = ?1 AND number = 0)",
        [key],
    )?;
    Ok(())
}

/// Whether the session has revision `number`.
fn revision_exists(conn: &Connection, key: i64, number: u32) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM revision WHERE session_id = ?1 AND number = ?2",
            params![key, i64::from(number)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Whether the session's chunk has both ends of `words` among its words.
fn has_words(conn: &Connection, key: i64, change: &Change) -> Result<bool, StoreError> {
    let found: i64 = conn.query_row(
        "SELECT count(*) FROM final_word \
         WHERE session_id = ?1 AND track = ?2 AND start_sample = ?3 AND position IN (?4, ?5)",
        params![
            key,
            i64::from(change.chunk.track.get()),
            sample(change.chunk.start)?,
            i64::from(change.words.first),
            i64::from(change.words.last)
        ],
        |r| r.get(0),
    )?;
    let ends = if change.words.first == change.words.last {
        1
    } else {
        2
    };
    Ok(found == ends)
}

/// Revision `number` and its ancestors, nearest first. Each parent is
/// lower than its child (the schema checks), so the walk ends.
fn chain(conn: &Connection, key: i64, number: u32) -> Result<Vec<u32>, StoreError> {
    let mut chain = vec![number];
    let mut at = number;
    while let Some(parent) = conn
        .query_row(
            "SELECT parent FROM revision WHERE session_id = ?1 AND number = ?2",
            params![key, i64::from(at)],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
    {
        at = u32::try_from(parent)
            .map_err(|_| StoreError::Corrupt(format!("revision parent {parent}")))?;
        chain.push(at);
    }
    Ok(chain)
}

/// A `revision_text` row as SQLite holds it: track, first sample, first
/// and last word, text.
type RawChange = (i64, i64, i64, i64, String);

/// Revision `revision`'s own changes.
fn own_changes(
    conn: &Connection,
    key: i64,
    revision: u32,
) -> Result<Vec<(ChunkKey, Applied)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT track, start_sample, first_word, last_word, text FROM revision_text \
         WHERE session_id = ?1 AND revision = ?2",
    )?;
    let raws = stmt
        .query_map(params![key, i64::from(revision)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<Vec<RawChange>, _>>()?;
    raws.into_iter()
        .map(|raw| parse_change(raw, RevisionNumber(revision)))
        .collect()
}

/// A stored change.
fn parse_change(
    (track, start, first, last, text): RawChange,
    revision: RevisionNumber,
) -> Result<(ChunkKey, Applied), StoreError> {
    let corrupt = || {
        StoreError::Corrupt(format!(
            "revision {} changes words {first}..={last} at sample {start}",
            revision.0
        ))
    };
    let start = u64::try_from(start).map_err(|_| corrupt())?;
    let first = u32::try_from(first).map_err(|_| corrupt())?;
    let last = u32::try_from(last).map_err(|_| corrupt())?;
    let words = WordSpan::new(first, last).ok_or_else(corrupt)?;
    let chunk = ChunkKey {
        track: parse_track(track)?,
        start: SampleIndex::new(start),
    };
    Ok((
        chunk,
        Applied {
            words,
            text,
            revision,
        },
    ))
}

/// The changes `chain` shows, by chunk, each chunk's by first word: the
/// nearest revision's first, and an older one only where it touches no
/// word a nearer one changed.
fn shown_changes(
    conn: &Connection,
    key: i64,
    chain: &[u32],
) -> Result<BTreeMap<ChunkKey, Vec<Applied>>, StoreError> {
    let mut shown: BTreeMap<ChunkKey, Vec<Applied>> = BTreeMap::new();
    // Revision 0 is the final text, with no changes of its own.
    for &revision in chain.iter().filter(|&&n| n != 0) {
        let mut own = own_changes(conn, key, revision)?;
        // Within one revision changes never overlap (the schema checks), so
        // their order doesn't matter; sorted, the result doesn't either.
        own.sort_by_key(|(chunk, applied)| (*chunk, applied.words));
        for (chunk, applied) in own {
            let taken = shown.entry(chunk).or_default();
            if !taken.iter().any(|t| t.words.overlaps(applied.words)) {
                taken.push(applied);
            }
        }
    }
    for changes in shown.values_mut() {
        changes.sort_by_key(|c| c.words);
    }
    Ok(shown)
}

/// `words` with each of `changes` (sorted by first word, not
/// overlapping) in place of the words it changes, joined by single
/// spaces. A change that replaces words with nothing leaves no gap.
fn render(words: &[FinalWord], changes: &[Applied]) -> String {
    let mut pieces: Vec<&str> = Vec::with_capacity(words.len());
    let mut changes = changes.iter().peekable();
    let mut skip_to = 0_usize;
    for (position, word) in words.iter().enumerate() {
        if position < skip_to {
            continue;
        }
        let first = changes.peek().map(|c| c.words.first as usize);
        if let Some(change) = changes.next_if(|_| first == Some(position)) {
            pieces.push(&change.text);
            skip_to = change.words.last as usize + 1;
        } else {
            pieces.push(&word.text);
        }
    }
    pieces.retain(|p| !p.is_empty());
    pieces.join(" ")
}

/// Each track's timeline from the session's anchors. A track whose
/// anchors don't rebuild into one (a damaged row) has none, so its chunks
/// read untimed rather than misplaced.
fn timelines(anchors: Vec<(TrackId, EpochAnchor)>) -> BTreeMap<TrackId, TrackTimeline> {
    let mut by_track: BTreeMap<TrackId, Vec<EpochAnchor>> = BTreeMap::new();
    for (track, anchor) in anchors {
        by_track.entry(track).or_default().push(anchor);
    }
    by_track
        .into_iter()
        .filter_map(|(track, anchors)| {
            TrackTimeline::rebuild(track, anchors)
                .ok()
                .map(|timeline| (track, timeline))
        })
        .collect()
}

/// When `text` was said: its start and end under the epoch of the segment
/// holding its first sample, from `segments` (sorted by track, then start).
/// `None` if no segment holds it or that epoch's anchor isn't stored.
fn placed(
    text: &FinalText,
    segments: &[SegmentRow],
    timelines: &BTreeMap<TrackId, TrackTimeline>,
) -> Option<Timed> {
    let key = text.key();
    let after =
        segments.partition_point(|s| (s.track(), s.range().start()) <= (key.track, key.start));
    let segment = segments.get(after.checked_sub(1)?)?;
    if segment.track() != key.track || !segment.range().contains(key.start) {
        return None;
    }
    let epoch = timelines
        .get(&key.track)?
        .epochs()
        .iter()
        .find(|e| e.id() == segment.epoch())?;
    Some(Timed {
        start: epoch.time_of(text.range.start())?,
        end: epoch.time_of(text.range.end())?,
    })
}

/// `lines` in session order: the timed ones by start, end, track and first
/// sample, then the untimed ones by track and first sample.
fn in_session_order(lines: &mut [FinalLine]) {
    lines.sort_by_key(|line| {
        let key = line.heard.key();
        (line.at.is_none(), line.at, key.track, key.start)
    });
}

impl Store {
    /// The session's revisions, in number order: none before its first
    /// final-text row, then revision 0, the final text, and each made since.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a number doesn't parse, and
    /// [`StoreError::Sqlite`] for any SQLite failure.
    pub fn revisions(&self, session: SessionId) -> Result<Vec<RevisionNumber>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT number FROM revision WHERE session_id = ?1 ORDER BY number")?;
        let numbers = stmt
            .query_map([session_key(session)?], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        numbers
            .into_iter()
            .map(|n| {
                u32::try_from(n)
                    .map(RevisionNumber)
                    .map_err(|_| StoreError::Corrupt(format!("revision number {n}")))
            })
            .collect()
    }

    /// Adds a revision of the session's text: `parent` with `changes`.
    /// Gives the new revision's number, one past the session's highest.
    /// Committed before returning.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoRevision`] if `parent` isn't one of the session's
    /// revisions, [`StoreError::NoWords`] if a change names words its chunk
    /// doesn't have (or a chunk the session doesn't have),
    /// [`StoreError::OutOfRange`] if a number doesn't fit, and
    /// [`StoreError::Sqlite`] for any SQLite failure, including two changes
    /// that share a word. Nothing is added then.
    pub fn add_revision(
        &mut self,
        session: SessionId,
        parent: RevisionNumber,
        changes: &[Change],
    ) -> Result<RevisionNumber, StoreError> {
        let key = session_key(session)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !revision_exists(&tx, key, parent.0)? {
            return Err(StoreError::NoRevision(session, parent.0));
        }
        for change in changes {
            if !has_words(&tx, key, change)? {
                return Err(StoreError::NoWords {
                    session,
                    chunk: change.chunk,
                    words: change.words,
                });
            }
        }
        let highest: i64 = tx.query_row(
            "SELECT max(number) FROM revision WHERE session_id = ?1",
            [key],
            |r| r.get(0),
        )?;
        let number = u32::try_from(highest)
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or(StoreError::OutOfRange)?;
        tx.execute(
            "INSERT INTO revision (session_id, number, parent) VALUES (?1, ?2, ?3)",
            params![key, i64::from(number), i64::from(parent.0)],
        )?;
        for change in changes {
            tx.execute(
                "INSERT INTO revision_text \
                 (session_id, revision, track, start_sample, first_word, last_word, text) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    key,
                    i64::from(number),
                    i64::from(change.chunk.track.get()),
                    sample(change.chunk.start)?,
                    i64::from(change.words.first),
                    i64::from(change.words.last),
                    change.text
                ],
            )?;
        }
        tx.commit()?;
        Ok(RevisionNumber(number))
    }

    /// The session's text as revision `number` shows it, in session order
    /// (see the module's docs): each chunk of the final text with the
    /// changes the revision shows in it.
    ///
    /// # Errors
    ///
    /// [`StoreError::NoRevision`] if the session has no such revision,
    /// [`StoreError::Corrupt`] or [`StoreError::CorruptRow`] if a row
    /// doesn't parse, and [`StoreError::Sqlite`] for any SQLite failure.
    pub fn revision(
        &self,
        session: SessionId,
        number: RevisionNumber,
    ) -> Result<Vec<FinalLine>, StoreError> {
        let key = session_key(session)?;
        // One snapshot for the chain, its changes, the text and its times.
        let tx = self.conn.unchecked_transaction()?;
        if !revision_exists(&tx, key, number.0)? {
            return Err(StoreError::NoRevision(session, number.0));
        }
        let mut shown = shown_changes(&tx, key, &chain(&tx, key, number.0)?)?;
        let timelines = timelines(self.epochs(session)?);
        let segments = self.segments(session)?;
        let mut lines: Vec<FinalLine> = texts_in(&tx, key)?
            .into_iter()
            .map(|heard| {
                let changes = shown.remove(&heard.key()).unwrap_or_default();
                let text = match &heard.text {
                    Some(_) if !changes.is_empty() => Some(render(&heard.words, &changes)),
                    stored => stored.clone(),
                };
                FinalLine {
                    at: placed(&heard, &segments, &timelines),
                    heard,
                    text,
                    changes,
                }
            })
            .collect();
        in_session_order(&mut lines);
        Ok(lines)
    }
}

#[cfg(test)]
mod tests;
