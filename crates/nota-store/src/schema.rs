//! The library database's schema.
//!
//! One database, `library.db` in the data directory, holds every session.
//! Every row that belongs to a session names it (`session_id`, a foreign key
//! to `session`), and every row that belongs to one track of a session names
//! that too (`track`): segments, epochs, utterances and timeline events. Two
//! sessions can hold rows at the same track and samples without one ever
//! claiming the other's, and a row that names another row (a revision's
//! text and a proposal name words of a final-text chunk) names one of its
//! own session's: the foreign key includes the session.
//!
//! Tables, and who fills them:
//!
//! | Table | Holds | Filled by |
//! |---|---|---|
//! | `session` | number, title, language, state, when it started (V3) | `nota record`, and adopting a session found on disk |
//! | `track` | each track's kind and source | `nota record`, and adopting a session found on disk with its row kept |
//! | `segment` | each published segment: its track, epoch, samples, SHA-256, decoded-audio digest (V6) | the recorder's publish step and salvage |
//! | `epoch` | each epoch's first sample, rate, session-time anchor and drift (V7) ([`Store::insert_epoch`](crate::Store::insert_epoch)) | the recorder's publish step, before each segment row |
//! | `utterance`, `word` | the heard text, as the engine confirmed it, with word times; never changed (V4's triggers) | `nota record`'s live text ([`crate::transcript`]) |
//! | `revision`, `revision_text` | the displayed text: revision 0 is the final pass's text, each later one a new row holding only what it changes, as runs of a chunk's words; never changed (V8) | revision 0 with the first final-text row, later ones by term clean-up ([`crate::revisions`]) |
//! | `proposal` | a proposed fix to a run of a chunk's words, with the revision, model, pack and thresholds it came from (V8) | term clean-up |
//! | `heard_revision`, `heard_revision_text`, `heard_proposal` | the heard text's revisions and proposals from before V8, kept as they were and never added to | nothing since V8 |
//! | `mark`, `note` | marks and notes made while recording, in session time | `nota record`, as each is made ([`crate::annotations`]) |
//! | `job` | work queued after a stop: its kind, state, progress and what it waits for (V5) | the job queue ([`crate::jobs`]) |
//! | `final_text`, `final_word`, `final_progress` | the final pass's text, by track and sample, beside the heard text and never in it, never changed (V8); how far it has got on each track (V5) | the final pass ([`crate::final_text`]) |
//! | `event` | the timeline: warnings raised and cleared, device changes, the transcriber's state, gaps | `nota record`, as each is sent to the screen ([`crate::timeline`]) |
//! | `finding` | an index of each session's findings file: rows whose file didn't prove them, and rows that don't parse (V6) | the app, from the findings files ([`Store::index_findings`](crate::Store::index_findings)) |
//! | `session_pack` | the term packs a session chose, by name and version, in order; never changed (V9) | Setup, when the session starts ([`Store::record_session_packs`](crate::Store::record_session_packs)) |
//! | `learned_pair` | each heard span → term the user accepted or rejected, with its session and when; never changed (V9) | the correction card ([`crate::learned`]) |
//!
//! Session times are nanoseconds from the session's start
//! ([`SessionTime`](nota_core::SessionTime)); samples are indices at the
//! track's rate. Tables a later package fills are created here, so filling
//! them doesn't change the schema; a package that finds a column missing
//! adds a migration ([`crate::migrate`]).
//!
//! # Segment files without a row
//!
//! A segment file whose row is missing (a crash between the file's rename
//! and its row's commit, or a row lost with a damaged database) is **kept**.
//! Nothing deletes a segment file because its row is missing: the file may
//! be the only copy of its audio, since its journal is deleted once the
//! file's row is committed, and a lost database loses that row afterwards.
//! A segment file is deleted only with proof that its audio is held
//! elsewhere: decoded audio covered by committed rows whose files match
//! their hashes. Deleting a session removes its files whatever they hold.

/// Version 1: the per-session store M1 kept at `sessions/<n>/nota.db`, with
/// `user_version` 1 and `journal_mode=WAL`, exactly as M1 created it. Its
/// rows name no session. Kept to say what [`crate::migrate`] imports, and
/// so tests can make one.
pub const V1: &str = "
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

/// Version 2: the first library schema. (Version 1 was the per-session
/// store, `sessions/<n>/nota.db`, which [`crate::migrate`] imports.)
pub(crate) const V2: &str = "
CREATE TABLE session (
    id INTEGER PRIMARY KEY CHECK (id >= 0),
    title TEXT,
    language TEXT,
    state TEXT NOT NULL
) STRICT;

CREATE TABLE track (
    session_id INTEGER NOT NULL REFERENCES session(id),
    track INTEGER NOT NULL CHECK (track BETWEEN 0 AND 4294967295),
    kind TEXT NOT NULL,
    source TEXT,
    PRIMARY KEY (session_id, track)
) STRICT;

CREATE TABLE segment (
    session_id INTEGER NOT NULL REFERENCES session(id),
    track INTEGER NOT NULL CHECK (track BETWEEN 0 AND 4294967295),
    epoch INTEGER NOT NULL CHECK (epoch BETWEEN 0 AND 4294967295),
    start_sample INTEGER NOT NULL,
    end_sample INTEGER NOT NULL,
    sha256 BLOB NOT NULL,
    language TEXT,
    PRIMARY KEY (session_id, track, start_sample),
    CHECK (start_sample >= 0 AND end_sample > start_sample),
    CHECK (length(sha256) = 32)
) STRICT;

CREATE TABLE epoch (
    session_id INTEGER NOT NULL REFERENCES session(id),
    track INTEGER NOT NULL CHECK (track BETWEEN 0 AND 4294967295),
    epoch INTEGER NOT NULL CHECK (epoch BETWEEN 0 AND 4294967295),
    first_sample INTEGER NOT NULL CHECK (first_sample >= 0),
    rate INTEGER NOT NULL CHECK (rate > 0),
    anchor_ns INTEGER CHECK (anchor_ns >= 0),
    PRIMARY KEY (session_id, track, epoch)
) STRICT;

CREATE TABLE utterance (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES session(id),
    track INTEGER NOT NULL CHECK (track BETWEEN 0 AND 4294967295),
    start_ns INTEGER NOT NULL CHECK (start_ns >= 0),
    end_ns INTEGER NOT NULL CHECK (end_ns >= start_ns),
    text TEXT NOT NULL,
    engine TEXT NOT NULL,
    model TEXT NOT NULL,
    UNIQUE (session_id, id)
) STRICT;
CREATE INDEX utterance_by_time ON utterance (session_id, start_ns);

CREATE TABLE word (
    utterance_id INTEGER NOT NULL REFERENCES utterance(id),
    position INTEGER NOT NULL CHECK (position >= 0),
    text TEXT NOT NULL,
    start_ns INTEGER NOT NULL CHECK (start_ns >= 0),
    end_ns INTEGER NOT NULL CHECK (end_ns >= start_ns),
    PRIMARY KEY (utterance_id, position)
) STRICT;

CREATE TABLE revision (
    session_id INTEGER NOT NULL REFERENCES session(id),
    number INTEGER NOT NULL CHECK (number >= 0),
    parent INTEGER CHECK (parent < number),
    PRIMARY KEY (session_id, number),
    FOREIGN KEY (session_id, parent) REFERENCES revision(session_id, number)
) STRICT;

CREATE TABLE revision_text (
    session_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    utterance_id INTEGER NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (session_id, revision, utterance_id),
    FOREIGN KEY (session_id, revision) REFERENCES revision(session_id, number),
    FOREIGN KEY (session_id, utterance_id) REFERENCES utterance(session_id, id)
) STRICT;

CREATE TABLE proposal (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    utterance_id INTEGER NOT NULL,
    heard TEXT NOT NULL,
    replacement TEXT NOT NULL,
    source TEXT NOT NULL,
    model TEXT,
    pack_version TEXT,
    thresholds TEXT,
    state TEXT NOT NULL,
    FOREIGN KEY (session_id, revision) REFERENCES revision(session_id, number),
    FOREIGN KEY (session_id, utterance_id) REFERENCES utterance(session_id, id)
) STRICT;

CREATE TABLE mark (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES session(id),
    at_ns INTEGER NOT NULL CHECK (at_ns >= 0)
) STRICT;
CREATE INDEX mark_by_time ON mark (session_id, at_ns);

CREATE TABLE note (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES session(id),
    at_ns INTEGER NOT NULL CHECK (at_ns >= 0),
    text TEXT NOT NULL
) STRICT;
CREATE INDEX note_by_time ON note (session_id, at_ns);

CREATE TABLE job (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES session(id),
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    detail TEXT
) STRICT;

CREATE TABLE event (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES session(id),
    track INTEGER CHECK (track BETWEEN 0 AND 4294967295),
    at_ns INTEGER NOT NULL CHECK (at_ns >= 0),
    kind TEXT NOT NULL,
    detail TEXT
) STRICT;
CREATE INDEX event_by_time ON event (session_id, at_ns);
";

/// Version 3: when each session started, as UTC seconds since 1970
/// ([`WallTime`](nota_core::WallTime)), for showing its date. Null for a
/// session from before version 3, and for one adopted from disk without its
/// row kept there: nothing says when those started.
pub(crate) const V3: &str = "
ALTER TABLE session ADD COLUMN started_at INTEGER CHECK (started_at >= 0);
";

/// Version 4: the heard text is append-only. Triggers refuse any UPDATE
/// or DELETE of an utterance, its words, a revision or a revision's text;
/// an INSERT that would replace one (`INSERT OR REPLACE` deletes the row
/// it replaces without firing delete triggers, unless a connection turns
/// recursive triggers on); and an INSERT that adds to one already made: a
/// word for any utterance but the newest (its words go in with it), text
/// for revision 0 (the heard text has none of its own), or text for any
/// revision but its session's newest. SQLite checks them on every
/// connection's writes. (They don't stop a connection that drops them:
/// nothing in nota does.)
///
/// Deleting a session (M5) will need a migration that lets its rows go
/// with it.
pub(crate) const V4: &str = "
CREATE TRIGGER utterance_never_updated BEFORE UPDATE ON utterance
BEGIN SELECT RAISE(ABORT, 'heard text is never changed'); END;
CREATE TRIGGER utterance_never_deleted BEFORE DELETE ON utterance
BEGIN SELECT RAISE(ABORT, 'heard text is never deleted'); END;
CREATE TRIGGER utterance_never_replaced BEFORE INSERT ON utterance
WHEN EXISTS (SELECT 1 FROM utterance WHERE id = NEW.id)
BEGIN SELECT RAISE(ABORT, 'heard text is never replaced'); END;

CREATE TRIGGER word_never_updated BEFORE UPDATE ON word
BEGIN SELECT RAISE(ABORT, 'heard text is never changed'); END;
CREATE TRIGGER word_never_deleted BEFORE DELETE ON word
BEGIN SELECT RAISE(ABORT, 'heard text is never deleted'); END;
CREATE TRIGGER word_never_replaced BEFORE INSERT ON word
WHEN EXISTS (
    SELECT 1 FROM word WHERE utterance_id = NEW.utterance_id AND position = NEW.position
)
BEGIN SELECT RAISE(ABORT, 'heard text is never replaced'); END;
CREATE TRIGGER word_only_with_its_utterance BEFORE INSERT ON word
WHEN NEW.utterance_id IS NOT (SELECT max(id) FROM utterance)
BEGIN SELECT RAISE(ABORT, 'heard text is never added to'); END;

CREATE TRIGGER revision_never_updated BEFORE UPDATE ON revision
BEGIN SELECT RAISE(ABORT, 'a revision is never changed'); END;
CREATE TRIGGER revision_never_deleted BEFORE DELETE ON revision
BEGIN SELECT RAISE(ABORT, 'a revision is never deleted'); END;
CREATE TRIGGER revision_never_replaced BEFORE INSERT ON revision
WHEN EXISTS (
    SELECT 1 FROM revision
    WHERE (session_id = NEW.session_id AND number = NEW.number) OR rowid = NEW.rowid
)
BEGIN SELECT RAISE(ABORT, 'a revision is never replaced'); END;

CREATE TRIGGER revision_text_never_updated BEFORE UPDATE ON revision_text
BEGIN SELECT RAISE(ABORT, 'a revision is never changed'); END;
CREATE TRIGGER revision_text_never_deleted BEFORE DELETE ON revision_text
BEGIN SELECT RAISE(ABORT, 'a revision is never deleted'); END;
CREATE TRIGGER revision_text_never_replaced BEFORE INSERT ON revision_text
WHEN EXISTS (
    SELECT 1 FROM revision_text
    WHERE session_id = NEW.session_id AND revision = NEW.revision
      AND utterance_id = NEW.utterance_id
)
BEGIN SELECT RAISE(ABORT, 'a revision is never replaced'); END;
CREATE TRIGGER revision_text_only_in_the_newest BEFORE INSERT ON revision_text
WHEN NEW.revision = 0
  OR NEW.revision IS NOT (SELECT max(number) FROM revision WHERE session_id = NEW.session_id)
BEGIN SELECT RAISE(ABORT, 'a revision is never added to'); END;
";

/// Version 5: the job queue and the final pass.
///
/// A job gains its progress (`progress` of `total`, in whatever units its
/// kind counts) and what a waiting job waits for (`waits_for`: `space`
/// after a full disk, `engine` without the speech models, `audio` while
/// the session still has journals to publish); a session has at most one
/// job of each kind. Sessions stopped before version 5 get no jobs from
/// it: their audio has no final pass unless one is queued for them.
///
/// The final pass's text is kept apart from the heard text: its own rows,
/// located by track and sample as the engine gives them, not in
/// `utterance`, so a revision never shows the two passes mixed. (Placing
/// it in session time needs each epoch's anchor, which the `epoch` table
/// holds for every segment published since journals kept it.) A row with no text is audio the engine couldn't transcribe.
/// `final_progress` says how far each track has got: every published
/// sample before `up_to` is covered, so a pass stopped partway resumes
/// there.
pub(crate) const V5: &str = "
ALTER TABLE job ADD COLUMN progress INTEGER NOT NULL DEFAULT 0 CHECK (progress >= 0);
ALTER TABLE job ADD COLUMN total INTEGER NOT NULL DEFAULT 0 CHECK (total >= 0);
ALTER TABLE job ADD COLUMN waits_for TEXT;
CREATE UNIQUE INDEX job_once ON job (session_id, kind);

CREATE TABLE final_text (
    session_id INTEGER NOT NULL REFERENCES session(id),
    track INTEGER NOT NULL CHECK (track BETWEEN 0 AND 4294967295),
    start_sample INTEGER NOT NULL CHECK (start_sample >= 0),
    end_sample INTEGER NOT NULL CHECK (end_sample > start_sample),
    text TEXT,
    engine TEXT NOT NULL,
    model TEXT NOT NULL,
    PRIMARY KEY (session_id, track, start_sample)
) STRICT;

CREATE TABLE final_word (
    session_id INTEGER NOT NULL,
    track INTEGER NOT NULL,
    start_sample INTEGER NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    text TEXT NOT NULL,
    word_start INTEGER NOT NULL CHECK (word_start >= 0),
    word_end INTEGER NOT NULL CHECK (word_end >= word_start),
    PRIMARY KEY (session_id, track, start_sample, position),
    FOREIGN KEY (session_id, track, start_sample)
        REFERENCES final_text(session_id, track, start_sample)
) STRICT;

CREATE TABLE final_progress (
    session_id INTEGER NOT NULL REFERENCES session(id),
    track INTEGER NOT NULL CHECK (track BETWEEN 0 AND 4294967295),
    up_to INTEGER NOT NULL CHECK (up_to >= 0),
    PRIMARY KEY (session_id, track)
) STRICT;
";

/// Version 6: each segment row's decoded-audio digest
/// ([`AudioDigest`](crate::AudioDigest)), null for rows committed before it
/// (their SHA-256 alone proves their file); and `finding`, an index of the
/// findings file each session's directory keeps, so the library can list
/// what needs the user without reading every directory. The file is the
/// record; the index is replaced from it, one session at a time.
///
/// A finding names its row by track and first sample, as stored, which a
/// row that doesn't parse still has; the rest of the row (end, epoch,
/// hashes) is null for one that doesn't.
pub(crate) const V6: &str = "
ALTER TABLE segment ADD COLUMN audio_digest BLOB
    CHECK (audio_digest IS NULL OR length(audio_digest) = 32);

CREATE TABLE finding (
    session_id INTEGER NOT NULL REFERENCES session(id),
    track INTEGER NOT NULL,
    start_sample INTEGER NOT NULL,
    end_sample INTEGER,
    epoch INTEGER,
    sha256 BLOB,
    audio_digest BLOB,
    problem TEXT NOT NULL,
    status TEXT NOT NULL
) STRICT;
CREATE INDEX finding_by_session ON finding (session_id, status);
";

/// Version 7: each epoch's drift (`epoch.drift_ppb`), how far its device's
/// clock ran from the session clock in parts per billion, which its
/// samples are timed by ([`Drift`](nota_core::Drift)). Rows stored before
/// it read as having none, as every epoch did then.
pub(crate) const V7: &str = "
ALTER TABLE epoch ADD COLUMN drift_ppb INTEGER NOT NULL DEFAULT 0
    CHECK (drift_ppb BETWEEN -1000000 AND 1000000);
";

/// Version 8: revisions and proposals on the final pass's text, which
/// becomes a record like the heard text.
///
/// **The heard text's revisions are kept, and closed.** Versions 2 to 7
/// revised the heard text: `revision`, `revision_text` and `proposal`
/// named utterances. Those tables become `heard_revision`,
/// `heard_revision_text` and `heard_proposal`, rows and all (SQLite's
/// rename carries their foreign keys with them), and triggers refuse any
/// INSERT, UPDATE or DELETE on them: they read as they did, through
/// [`Store::heard_revision`](crate::Store::heard_revision), and nothing is
/// added to them. V4's triggers on the old tables are dropped first, so
/// their names are free for the new tables.
///
/// **Revisions are of the final pass's text.** `revision` is as before; a
/// session's revision 0 is its final text, made with its first final-text
/// row (and here, for every session that has some already). A
/// `revision_text` row is one change: a run of a chunk's words, first to
/// last by position, and the text that replaces them. The chunk is named
/// by session, track and first sample, and both ends of the run are
/// foreign keys to `final_word`, so a change names words its own
/// session's chunk has. A `proposal` names its chunk and words the same
/// way. A revision's changes never overlap, and as with V4, revision 0
/// has none, and only the session's newest revision is added to.
///
/// **The final text is append-only.** Triggers refuse an UPDATE, DELETE
/// or replacing INSERT of a `final_text` or `final_word` row, and a word
/// for any chunk but the newest stored, or for one its track's progress
/// has passed (its words go in with it, before the progress that covers
/// it). The newest is the highest rowid: `final_text` has no INTEGER
/// PRIMARY KEY, and VACUUM may renumber such rowids, which nothing in nota
/// runs. `final_progress` isn't part of the record: it moves on as the
/// pass does.
///
/// **Replacing by rowid.** A table without an INTEGER PRIMARY KEY has a
/// rowid apart from its key, and an `INSERT OR REPLACE` naming a stored
/// rowid with a new key would delete that row without firing its delete
/// trigger. Every append-only table here with such a key refuses an
/// INSERT whose rowid is taken (an automatic rowid reads as -1 there, and
/// none is stored), and V4's `word_never_replaced` is made again so.
pub(crate) const V8: &str = "
DROP TRIGGER revision_never_updated;
DROP TRIGGER revision_never_deleted;
DROP TRIGGER revision_never_replaced;
DROP TRIGGER revision_text_never_updated;
DROP TRIGGER revision_text_never_deleted;
DROP TRIGGER revision_text_never_replaced;
DROP TRIGGER revision_text_only_in_the_newest;
ALTER TABLE revision RENAME TO heard_revision;
ALTER TABLE revision_text RENAME TO heard_revision_text;
ALTER TABLE proposal RENAME TO heard_proposal;

CREATE TRIGGER heard_revision_never_added BEFORE INSERT ON heard_revision
BEGIN SELECT RAISE(ABORT, 'the heard text is never revised again'); END;
CREATE TRIGGER heard_revision_never_updated BEFORE UPDATE ON heard_revision
BEGIN SELECT RAISE(ABORT, 'a revision is never changed'); END;
CREATE TRIGGER heard_revision_never_deleted BEFORE DELETE ON heard_revision
BEGIN SELECT RAISE(ABORT, 'a revision is never deleted'); END;
CREATE TRIGGER heard_revision_text_never_added BEFORE INSERT ON heard_revision_text
BEGIN SELECT RAISE(ABORT, 'the heard text is never revised again'); END;
CREATE TRIGGER heard_revision_text_never_updated BEFORE UPDATE ON heard_revision_text
BEGIN SELECT RAISE(ABORT, 'a revision is never changed'); END;
CREATE TRIGGER heard_revision_text_never_deleted BEFORE DELETE ON heard_revision_text
BEGIN SELECT RAISE(ABORT, 'a revision is never deleted'); END;
CREATE TRIGGER heard_proposal_never_added BEFORE INSERT ON heard_proposal
BEGIN SELECT RAISE(ABORT, 'the heard text is never proposed for again'); END;
CREATE TRIGGER heard_proposal_never_updated BEFORE UPDATE ON heard_proposal
BEGIN SELECT RAISE(ABORT, 'a heard proposal is never changed'); END;
CREATE TRIGGER heard_proposal_never_deleted BEFORE DELETE ON heard_proposal
BEGIN SELECT RAISE(ABORT, 'a heard proposal is never deleted'); END;

CREATE TABLE revision (
    session_id INTEGER NOT NULL REFERENCES session(id),
    number INTEGER NOT NULL CHECK (number >= 0),
    parent INTEGER CHECK (parent < number),
    PRIMARY KEY (session_id, number),
    FOREIGN KEY (session_id, parent) REFERENCES revision(session_id, number)
) STRICT;

CREATE TABLE revision_text (
    session_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    track INTEGER NOT NULL,
    start_sample INTEGER NOT NULL,
    first_word INTEGER NOT NULL,
    last_word INTEGER NOT NULL CHECK (last_word >= first_word),
    text TEXT NOT NULL,
    PRIMARY KEY (session_id, revision, track, start_sample, first_word),
    FOREIGN KEY (session_id, revision) REFERENCES revision(session_id, number),
    FOREIGN KEY (session_id, track, start_sample, first_word)
        REFERENCES final_word(session_id, track, start_sample, position),
    FOREIGN KEY (session_id, track, start_sample, last_word)
        REFERENCES final_word(session_id, track, start_sample, position)
) STRICT;

CREATE TABLE proposal (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    track INTEGER NOT NULL,
    start_sample INTEGER NOT NULL,
    first_word INTEGER NOT NULL,
    last_word INTEGER NOT NULL CHECK (last_word >= first_word),
    heard TEXT NOT NULL,
    replacement TEXT NOT NULL,
    source TEXT NOT NULL,
    model TEXT,
    pack_version TEXT,
    thresholds TEXT,
    state TEXT NOT NULL,
    FOREIGN KEY (session_id, revision) REFERENCES revision(session_id, number),
    FOREIGN KEY (session_id, track, start_sample, first_word)
        REFERENCES final_word(session_id, track, start_sample, position),
    FOREIGN KEY (session_id, track, start_sample, last_word)
        REFERENCES final_word(session_id, track, start_sample, position)
) STRICT;

INSERT INTO revision (session_id, number, parent)
SELECT DISTINCT session_id, 0, NULL FROM final_text;

CREATE TRIGGER revision_never_updated BEFORE UPDATE ON revision
BEGIN SELECT RAISE(ABORT, 'a revision is never changed'); END;
CREATE TRIGGER revision_never_deleted BEFORE DELETE ON revision
BEGIN SELECT RAISE(ABORT, 'a revision is never deleted'); END;
CREATE TRIGGER revision_never_replaced BEFORE INSERT ON revision
WHEN EXISTS (
    SELECT 1 FROM revision
    WHERE (session_id = NEW.session_id AND number = NEW.number) OR rowid = NEW.rowid
)
BEGIN SELECT RAISE(ABORT, 'a revision is never replaced'); END;

CREATE TRIGGER revision_text_never_updated BEFORE UPDATE ON revision_text
BEGIN SELECT RAISE(ABORT, 'a revision is never changed'); END;
CREATE TRIGGER revision_text_never_deleted BEFORE DELETE ON revision_text
BEGIN SELECT RAISE(ABORT, 'a revision is never deleted'); END;
CREATE TRIGGER revision_text_never_overlaps BEFORE INSERT ON revision_text
WHEN EXISTS (
    SELECT 1 FROM revision_text
    WHERE session_id = NEW.session_id AND revision = NEW.revision
      AND track = NEW.track AND start_sample = NEW.start_sample
      AND first_word <= NEW.last_word AND last_word >= NEW.first_word
)
BEGIN SELECT RAISE(ABORT, 'a revision never changes a word twice'); END;
CREATE TRIGGER revision_text_never_replaced BEFORE INSERT ON revision_text
WHEN EXISTS (SELECT 1 FROM revision_text WHERE rowid = NEW.rowid)
BEGIN SELECT RAISE(ABORT, 'a revision is never replaced'); END;
CREATE TRIGGER revision_text_only_in_the_newest BEFORE INSERT ON revision_text
WHEN NEW.revision = 0
  OR NEW.revision IS NOT (SELECT max(number) FROM revision WHERE session_id = NEW.session_id)
BEGIN SELECT RAISE(ABORT, 'a revision is never added to'); END;

CREATE TRIGGER final_text_never_updated BEFORE UPDATE ON final_text
BEGIN SELECT RAISE(ABORT, 'final text is never changed'); END;
CREATE TRIGGER final_text_never_deleted BEFORE DELETE ON final_text
BEGIN SELECT RAISE(ABORT, 'final text is never deleted'); END;
CREATE TRIGGER final_text_never_replaced BEFORE INSERT ON final_text
WHEN EXISTS (
    SELECT 1 FROM final_text
    WHERE (session_id = NEW.session_id AND track = NEW.track
           AND start_sample = NEW.start_sample)
       OR rowid = NEW.rowid
)
BEGIN SELECT RAISE(ABORT, 'final text is never replaced'); END;

CREATE TRIGGER final_word_never_updated BEFORE UPDATE ON final_word
BEGIN SELECT RAISE(ABORT, 'final text is never changed'); END;
CREATE TRIGGER final_word_never_deleted BEFORE DELETE ON final_word
BEGIN SELECT RAISE(ABORT, 'final text is never deleted'); END;
CREATE TRIGGER final_word_never_replaced BEFORE INSERT ON final_word
WHEN EXISTS (
    SELECT 1 FROM final_word
    WHERE (session_id = NEW.session_id AND track = NEW.track
           AND start_sample = NEW.start_sample AND position = NEW.position)
       OR rowid = NEW.rowid
)
BEGIN SELECT RAISE(ABORT, 'final text is never replaced'); END;
CREATE TRIGGER final_word_only_with_its_text BEFORE INSERT ON final_word
WHEN NOT EXISTS (
    SELECT 1 FROM final_text
    WHERE rowid = (SELECT max(rowid) FROM final_text)
      AND session_id = NEW.session_id AND track = NEW.track
      AND start_sample = NEW.start_sample
)
  OR NEW.start_sample < coalesce(
    (SELECT up_to FROM final_progress
     WHERE session_id = NEW.session_id AND track = NEW.track),
    0
  )
BEGIN SELECT RAISE(ABORT, 'final text is never added to'); END;

DROP TRIGGER word_never_replaced;
CREATE TRIGGER word_never_replaced BEFORE INSERT ON word
WHEN EXISTS (
    SELECT 1 FROM word
    WHERE (utterance_id = NEW.utterance_id AND position = NEW.position)
       OR rowid = NEW.rowid
)
BEGIN SELECT RAISE(ABORT, 'heard text is never replaced'); END;
";

/// Version 9: the packs each session chose, and the learned pairs.
///
/// **A session's packs** (`session_pack`) are named by the name and
/// version in each pack's manifest ([`crate::packs`]), in the order the
/// session chose them. They're recorded once, when the session starts, so
/// a proposal can say which pack, at which version, its term came from;
/// triggers refuse an UPDATE, DELETE or replacing INSERT of a row.
///
/// **Learned pairs** (`learned_pair`) are the user's choices on a heard
/// span: a term accepted for it, or a term rejected for it, with the
/// session it was made in and when (UTC seconds since 1970). `heard` is
/// the span as heard; `heard_key` and `term_key` are the forms pairs are
/// matched by ([`crate::learned::match_key`]). A pair is a record, never
/// changed: a later choice on the same span and term is a new row, and
/// the newest says what holds ([`crate::learned`]).
///
/// Deleting a session (M5) will need a migration that lets these rows go
/// with it, as V4's note says of the heard text.
pub(crate) const V9: &str = "
CREATE TABLE session_pack (
    session_id INTEGER NOT NULL REFERENCES session(id),
    position INTEGER NOT NULL CHECK (position >= 0),
    name TEXT NOT NULL CHECK (name <> ''),
    version TEXT NOT NULL CHECK (version <> ''),
    PRIMARY KEY (session_id, name),
    UNIQUE (session_id, position)
) STRICT;

CREATE TRIGGER session_pack_never_updated BEFORE UPDATE ON session_pack
BEGIN SELECT RAISE(ABORT, 'a session''s packs are never changed'); END;
CREATE TRIGGER session_pack_never_deleted BEFORE DELETE ON session_pack
BEGIN SELECT RAISE(ABORT, 'a session''s packs are never deleted'); END;
CREATE TRIGGER session_pack_never_replaced BEFORE INSERT ON session_pack
WHEN EXISTS (
    SELECT 1 FROM session_pack
    WHERE (session_id = NEW.session_id AND (name = NEW.name OR position = NEW.position))
       OR rowid = NEW.rowid
)
BEGIN SELECT RAISE(ABORT, 'a session''s packs are never replaced'); END;

CREATE TABLE learned_pair (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES session(id),
    heard TEXT NOT NULL,
    heard_key TEXT NOT NULL CHECK (heard_key <> ''),
    term TEXT NOT NULL,
    term_key TEXT NOT NULL CHECK (term_key <> ''),
    verdict TEXT NOT NULL CHECK (verdict IN ('accepted', 'rejected')),
    at INTEGER NOT NULL CHECK (at >= 0)
) STRICT;
CREATE INDEX learned_pair_by_heard ON learned_pair (heard_key, term_key);

CREATE TRIGGER learned_pair_never_updated BEFORE UPDATE ON learned_pair
BEGIN SELECT RAISE(ABORT, 'a learned pair is never changed'); END;
CREATE TRIGGER learned_pair_never_deleted BEFORE DELETE ON learned_pair
BEGIN SELECT RAISE(ABORT, 'a learned pair is never deleted'); END;
CREATE TRIGGER learned_pair_never_replaced BEFORE INSERT ON learned_pair
WHEN EXISTS (SELECT 1 FROM learned_pair WHERE id = NEW.id)
BEGIN SELECT RAISE(ABORT, 'a learned pair is never replaced'); END;
";

#[cfg(test)]
mod tests;
