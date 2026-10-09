//! The library database's schema.
//!
//! One database, `library.db` in the data directory, holds every session.
//! Every row that belongs to a session names it (`session_id`, a foreign key
//! to `session`), and every row that belongs to one track of a session names
//! that too (`track`): segments, epochs, utterances and timeline events. Two
//! sessions can hold rows at the same track and samples without one ever
//! claiming the other's, and a row that names another row (a revision's
//! text and a proposal name an utterance) names one of its own session's:
//! the foreign key includes the session.
//!
//! Tables, and who fills them:
//!
//! | Table | Holds | Filled by |
//! |---|---|---|
//! | `session` | number, title, language, state, when it started (V3) | `nota record`, and adopting a session found on disk |
//! | `track` | each track's kind and source | `nota record`, and adopting a session found on disk with its row kept |
//! | `segment` | each published segment: its track, epoch, samples, SHA-256 | the recorder's publish step and salvage |
//! | `epoch` | each epoch's first sample, rate and session-time anchor | the epochs package |
//! | `utterance`, `word` | the heard text, as the engine confirmed it, with word times; never changed (V4's triggers) | `nota record`'s live text ([`crate::transcript`]) |
//! | `revision`, `revision_text` | the displayed text: revision 0 is the heard text, each later one a new row holding only what it changes; never changed | revision 0 with the first utterance, later ones by term clean-up |
//! | `proposal` | a proposed fix, with the revision, model, pack and thresholds it came from | term clean-up |
//! | `mark`, `note` | marks and notes made while recording, in session time | `nota record`, as each is made ([`crate::annotations`]) |
//! | `job` | work queued after a stop: its kind, state, progress and what it waits for (V5) | the job queue ([`crate::jobs`]) |
//! | `final_text`, `final_word`, `final_progress` | the final pass's text, by track and sample, beside the heard text and never in it; how far it has got on each track (V5) | the final pass ([`crate::final_text`]) |
//! | `event` | the timeline: device changes, warnings, gaps | the detectors |
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
    SELECT 1 FROM revision WHERE session_id = NEW.session_id AND number = NEW.number
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
/// it in session time needs each epoch's anchor, which nothing stores
/// yet.) A row with no text is audio the engine couldn't transcribe.
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

#[cfg(test)]
mod tests;
