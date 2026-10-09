//! The library database's schema.
//!
//! One database, `library.db` in the data directory, holds every session.
//! Every row that belongs to a session names it (`session_id`, a foreign key
//! to `session`), and every row that belongs to one track of a session names
//! that too (`track`): segments, epochs, utterances and timeline events. Two
//! sessions can hold rows at the same track and samples without one ever
//! claiming the other's.
//!
//! Tables, and who fills them:
//!
//! | Table | Holds | Filled by |
//! |---|---|---|
//! | `session` | number, title, language, state | `nota record`, and adopting a session found on disk |
//! | `track` | each track's kind and source | `nota record` |
//! | `segment` | each published segment: its track, epoch, samples, SHA-256 | the recorder's publish step and salvage |
//! | `epoch` | each epoch's first sample, rate and session-time anchor | the epochs package |
//! | `utterance`, `word` | the heard text, as the engine confirmed it, with word times | the live-transcript package |
//! | `revision`, `revision_text` | the displayed text: revision 0 is the heard text, each later one a new row | the live-transcript package, then term clean-up |
//! | `proposal` | a proposed fix, with the revision, model, pack and thresholds it came from | term clean-up |
//! | `mark`, `note` | marks and notes made while recording, in session time | the marks-and-notes package |
//! | `job` | work queued after a stop | the job queue |
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
    model TEXT NOT NULL
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
    utterance_id INTEGER NOT NULL REFERENCES utterance(id),
    text TEXT NOT NULL,
    PRIMARY KEY (session_id, revision, utterance_id),
    FOREIGN KEY (session_id, revision) REFERENCES revision(session_id, number)
) STRICT;

CREATE TABLE proposal (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    utterance_id INTEGER NOT NULL REFERENCES utterance(id),
    heard TEXT NOT NULL,
    replacement TEXT NOT NULL,
    source TEXT NOT NULL,
    model TEXT,
    pack_version TEXT,
    thresholds TEXT,
    state TEXT NOT NULL,
    FOREIGN KEY (session_id, revision) REFERENCES revision(session_id, number)
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

#[cfg(test)]
mod tests;
