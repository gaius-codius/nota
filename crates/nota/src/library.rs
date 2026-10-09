//! Where sessions live, on disk and in the library database, and
//! salvaging them at start.
//!
//! Every session has a directory under the data directory,
//! `sessions/<number>/`, holding its audio directory (`audio/`: journals,
//! segments, marks), and a row in the library database
//! (`library.db` in the data directory) with its segment rows. Numbers
//! count up from 1 and are never reused: a new session takes the number
//! after the highest on disk or in the database.
//!
//! **Recording never waits on the database.** A new session's row is
//! added by the publisher, before its first segment row (or by the saver
//! of live text, marks and notes, if that writes first), so a database
//! that can't be opened holds up publishing (the journals stay on disk)
//! and never recording. The session's row is also kept in its directory
//! (`sessions/<number>/session.txt`, see [`kept`]) once its tracks have
//! started. The next start adds any session directory the database doesn't
//! have, with the row kept there if it has one, importing its M1
//! per-session store (`sessions/<number>/nota.db`) if it has one, and
//! salvages it.
//!
//! **Findings.** Each start also checks every row of every session by name
//! (the recorder's integrity scan), so a row whose file is gone is found
//! even with no journals left, and copies each session's findings file
//! into the database's findings index. The file, in the session's audio
//! directory, is the record, and what Home reads, so findings recorded
//! since the start show; the index is as of the last start, for queries
//! across the library.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nota_core::{SampleCount, SampleRate, SessionId, TrackId, WallTime};
use nota_recorder::fs::{Fs, StdFs};
use nota_recorder::journal::JournalId;
use nota_recorder::segment::{
    Depth, DurableSegment, SegmentLength, SegmentStore, needs_salvage, read_findings, salvage, scan,
};
use nota_recorder::session::{SessionDir, SessionStore};
use nota_store::{NewSession, RowKey, SegmentRow, SessionState, StoreError, Writer};

/// Names under the data directory, and in a session's directory.
const SESSIONS: &str = "sessions";
const LIBRARY_DB: &str = "library.db";
const AUDIO: &str = "audio";
/// M1's per-session store.
const PER_SESSION_STORE: &str = "nota.db";

/// The sessions under one data directory.
#[derive(Debug, Clone)]
pub(crate) struct Library {
    root: PathBuf,
    db: Writer,
}

/// One session's place on disk.
#[derive(Debug, Clone)]
pub(crate) struct SessionPaths {
    pub(crate) id: SessionId,
    pub(crate) dir: PathBuf,
}

impl SessionPaths {
    /// The session's audio directory.
    pub(crate) fn audio(&self) -> PathBuf {
        self.dir.join(AUDIO)
    }

    /// Keeps `session`, this session's row, in its directory, so it can be
    /// adopted with it if the library database never takes it.
    pub(crate) fn keep(&self, session: &NewSession) -> io::Result<()> {
        kept::write(&StdFs, &self.dir, session)
    }

    /// Where an M1 session kept its own store.
    pub(crate) fn per_session_store(&self) -> PathBuf {
        self.dir.join(PER_SESSION_STORE)
    }
}

/// What salvage did to one session at start.
#[derive(Debug)]
pub(crate) enum Salvaged {
    /// It had journals left, and salvage published them. Also the journals
    /// it set aside as damaged, under their new names (kept, not deleted).
    Done(SessionId, Vec<PathBuf>),
    /// Salvage ran, but left journals it couldn't publish yet (unreadable,
    /// undeletable, or not set aside; held back by a name their segment
    /// can't use, or by a segment that doesn't match its row); the next
    /// start tries again. Also the journals it set aside as damaged, as in
    /// `Done`.
    Left(SessionId, Vec<PathBuf>),
    /// Another nota is using it; it's left alone.
    InUse(SessionId),
    /// Salvage failed, or couldn't start because the session couldn't be
    /// added to the library database; the journals are still there for the
    /// next start.
    Failed(SessionId, String),
}

/// A session as Home lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) id: SessionId,
    /// Its title, if the library database has one.
    pub(crate) title: Option<String>,
    /// When it started, if the library database knows.
    pub(crate) started_at: Option<WallTime>,
    /// How much audio its longest track has published, if the library
    /// database could be read.
    pub(crate) recorded: Option<Duration>,
    /// Whether it needs the user, and why.
    pub(crate) needs: Needs,
}

/// Whether a session needs the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Needs {
    /// Nothing: its audio is published and its rows match their files.
    Nothing,
    /// Another nota holds it: it's being recorded.
    InUse,
    /// The user must act, for the reason given.
    Attention(String),
}

impl Library {
    /// The sessions under `root`, which is created if it's missing. The
    /// library database isn't opened until it's used.
    pub(crate) fn open(root: &Path) -> io::Result<Self> {
        ensure_dir(&root.join(SESSIONS))?;
        Ok(Self {
            root: root.to_path_buf(),
            db: Writer::new(&root.join(LIBRARY_DB)),
        })
    }

    /// The library database's file.
    pub(crate) fn db_path(&self) -> PathBuf {
        self.root.join(LIBRARY_DB)
    }

    /// The library database.
    pub(crate) const fn db(&self) -> &Writer {
        &self.db
    }

    fn sessions(&self) -> PathBuf {
        self.root.join(SESSIONS)
    }

    /// Every session on disk, in number order.
    pub(crate) fn existing(&self) -> io::Result<Vec<SessionPaths>> {
        let mut found: Vec<SessionPaths> = StdFs
            .list(&self.sessions())?
            .into_iter()
            .filter_map(|dir| {
                let name = dir.file_name()?.to_str()?;
                let id: u64 = name.parse().ok()?;
                // Only the name a session is given: not `07` or `+7`.
                if id.to_string() != name {
                    return None;
                }
                Some(SessionPaths {
                    id: SessionId::new(id),
                    dir,
                })
            })
            .collect();
        found.sort_by_key(|s| s.id.get());
        Ok(found)
    }

    /// Adds `session` to the library database if it isn't there, with the
    /// row kept in its directory and its per-session store's rows if it has
    /// them. A session directory that can't be listed, or whose kept row is
    /// there but can't be read, isn't added, so the next start can still
    /// import them. A kept row that doesn't parse never will: the session is
    /// added with its number alone rather than holding up its audio.
    fn adopt(&self, session: &SessionPaths) -> Result<(), String> {
        // Known already: nothing on disk is read again.
        if self
            .db
            .with(|db| db.session(session.id))
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Ok(());
        }
        let store = session.per_session_store();
        let there = StdFs
            .list(&session.dir)
            .map_err(|e| format!("listing {}: {e}", session.dir.display()))?;
        let per_session = there.contains(&store).then_some(store.as_path());
        let row = kept::read(&StdFs, &session.dir, session.id)
            .map_err(|e| format!("reading its title and tracks: {e}"))?
            .unwrap_or_else(|| NewSession::bare(session.id));
        self.db
            .with(|db| db.adopt_session(&row, per_session))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Adds every session on disk to the library database, and salvages
    /// every one left with journals, as after a crash or a stop that
    /// couldn't publish everything. A session the database still says is
    /// recording, which no one is, is marked stopped. A session another
    /// nota holds is left alone, in the database as on disk, and so is one
    /// with nothing in it yet (another nota may have just made it).
    ///
    /// A session that can't be added isn't salvaged (salvage would read no
    /// rows for it): if it has journals, that's reported. The next start
    /// tries again. A database that can't be opened is tried once, not
    /// once for each session.
    pub(crate) fn salvage_all(&self, length: SegmentLength) -> io::Result<Vec<Salvaged>> {
        let sessions = self.existing()?;
        if let Err(e) = self.db.with(|_| Ok(())) {
            return Ok(sessions
                .iter()
                .filter(|s| {
                    needs_salvage(&SessionDir::new(s.id, StdFs, &s.audio())).unwrap_or(false)
                })
                .map(|s| {
                    Salvaged::Failed(s.id, format!("the library database can't be opened: {e}"))
                })
                .collect());
        }
        Ok(sessions
            .iter()
            .filter_map(|session| self.salvage_one(session, length))
            .collect())
    }

    /// Adds `session` to the library database, salvages it if it has
    /// journals left, checks its rows by name, and indexes its findings.
    /// `None` if there was nothing to report.
    ///
    /// The session is locked first, so a session another nota is
    /// recording is never adopted or marked stopped under it.
    fn salvage_one(&self, session: &SessionPaths, length: SegmentLength) -> Option<Salvaged> {
        // Nothing in it yet, as `create` leaves it, unless the database
        // holds rows for it: then its files are gone, and the scan says so.
        if is_empty(session) && !self.has_rows(session.id) {
            return None;
        }
        let dir = SessionDir::new(session.id, StdFs, &session.audio());
        // An audio directory that can't be listed has nothing to salvage now.
        let journals = needs_salvage(&dir).unwrap_or(false);
        let lock = match dir.lock() {
            Ok(lock) => lock,
            Err(e) => {
                return journals.then(|| match e.kind() {
                    io::ErrorKind::WouldBlock => Salvaged::InUse(session.id),
                    _ => Salvaged::Failed(session.id, e.to_string()),
                });
            }
        };
        if let Err(e) = self.adopt(session) {
            return journals.then(|| {
                Salvaged::Failed(
                    session.id,
                    format!("adding it to the library database: {e}"),
                )
            });
        }
        // No one is recording it: whatever the database says, it stopped.
        // If this fails, the next start tries again.
        let _stopped = self.db.with(|db| db.stop_recording(session.id));
        let mut bound = SessionStore::new(lock, self.db.clone());
        let salvaged = journals.then(|| match salvage(&mut bound, length) {
            Ok(published) => {
                let aside = published.quarantined().to_vec();
                if needs_salvage(&dir).unwrap_or(true) {
                    Salvaged::Left(session.id, aside)
                } else {
                    Salvaged::Done(session.id, aside)
                }
            }
            Err(e) => Salvaged::Failed(session.id, e.to_string()),
        });
        // Salvage checks only the rows its journals overlap: a row whose
        // file is gone with no journals left is found here. Both record
        // what they find in the findings file, which is then indexed. Each
        // is tried again at the next start if it fails.
        let _scanned = scan(&mut bound, Depth::Names);
        if let Ok(findings) = read_findings(&dir) {
            let _indexed = self
                .db
                .with(|db| db.index_findings(session.id, &findings.indexed()));
        }
        salvaged
    }

    /// Whether the library database holds segment rows for `id`, or can't
    /// say (a row that doesn't parse).
    fn has_rows(&self, id: SessionId) -> bool {
        match self.db.with(|db| db.segments(id)) {
            Ok(rows) => !rows.is_empty(),
            Err(StoreError::CorruptRow { .. }) => true,
            Err(_) => false,
        }
    }

    /// Every session, as Home lists it, in number order: each one in the
    /// library database with its title and the audio its rows hold, and
    /// each one on disk that the database doesn't have (or every one, if
    /// the database can't be read: that's said as each one's attention).
    /// Audio is counted at `rate`, the rate every track records at.
    pub(crate) fn listing(&self, rate: SampleRate) -> io::Result<Vec<Listed>> {
        let on_disk = self.existing()?;
        let in_db = self.db.with(|db| {
            db.sessions()?
                .into_iter()
                .map(|session| {
                    // A session with a row that doesn't parse is listed
                    // without its length; its findings file names the row.
                    let rows = match db.segments(session.id) {
                        Ok(rows) => Some(rows),
                        Err(StoreError::CorruptRow { .. }) => None,
                        Err(e) => return Err(e),
                    };
                    Ok((session.id, session.title, session.started_at, rows))
                })
                .collect::<Result<Vec<_>, StoreError>>()
        });
        let mut listed: BTreeMap<u64, Listed> = BTreeMap::new();
        let db_error = match in_db {
            Ok(sessions) => {
                for (id, title, started_at, rows) in sessions {
                    let recorded = rows.as_deref().and_then(|rows| longest_track(rows, rate));
                    listed.insert(
                        id.get(),
                        Listed {
                            id,
                            title,
                            started_at,
                            recorded,
                            needs: Needs::Nothing,
                        },
                    );
                }
                None
            }
            Err(e) => Some(format!("the library database can't be read: {e}")),
        };
        for session in &on_disk {
            // One the database holds is listed whatever its directory has.
            if is_empty(session) && !listed.contains_key(&session.id.get()) {
                continue;
            }
            let needs = match (&db_error, needs(session)) {
                (_, needs @ (Needs::InUse | Needs::Attention(_))) => needs,
                (Some(e), Needs::Nothing) => Needs::Attention(e.clone()),
                (None, Needs::Nothing) => Needs::Nothing,
            };
            listed
                .entry(session.id.get())
                .and_modify(|listed| listed.needs = needs.clone())
                .or_insert(Listed {
                    id: session.id,
                    title: None,
                    started_at: None,
                    recorded: None,
                    needs,
                });
        }
        Ok(listed.into_values().collect())
    }

    /// Makes a new session's directories, numbered after every session on
    /// disk and, if the database can be read, every session in it.
    pub(crate) fn create(&self) -> io::Result<SessionPaths> {
        let on_disk = self.existing()?.last().map_or(0, |last| last.id.get());
        // A database that can't be read now could hold a number the disk
        // doesn't show only if a session's directory was deleted by hand:
        // nota doesn't delete sessions yet. Rows bound to such a number are
        // refused (`NewSessionRows`), and its journals stay on disk.
        let in_db = self
            .db
            .with(|db| db.sessions())
            .ok()
            .and_then(|sessions| sessions.last().map(|s| s.id.get()))
            .unwrap_or(0);
        let mut next = on_disk.max(in_db).checked_add(1).ok_or_else(no_number)?;
        // Another nota may take the same number first: try the next.
        for _ in 0..16 {
            let dir = self.sessions().join(next.to_string());
            match StdFs.create_dir(&dir) {
                Ok(()) => {
                    StdFs.sync_dir(&self.sessions())?;
                    let paths = SessionPaths {
                        id: SessionId::new(next),
                        dir,
                    };
                    StdFs.create_dir(&paths.audio())?;
                    StdFs.sync_dir(&paths.dir)?;
                    return Ok(paths);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    next = next.checked_add(1).ok_or_else(no_number)?;
                }
                Err(e) => return Err(e),
            }
        }
        Err(no_number())
    }
}

/// Whether `session`'s directory shows it needs the user: rows whose files
/// didn't prove them (or that didn't parse) when they were last checked,
/// still unresolved, or journals left unpublished. Journals another nota is
/// still writing are a recording, not a problem.
fn needs(session: &SessionPaths) -> Needs {
    let dir = SessionDir::new(session.id, StdFs, &session.audio());
    match read_findings(&dir).map(|f| f.unresolved()) {
        Ok(0) => {}
        Ok(n) => {
            let (rows, verb, files) = if n == 1 {
                ("segment", "doesn't", "its file brings it")
            } else {
                ("segments", "don't", "their files bring them")
            };
            // With journals left, salvage keeps their audio and repairs what
            // they hold; with none, only the user can bring the files back.
            return Needs::Attention(if needs_salvage(&dir).unwrap_or(true) {
                format!("{n} {rows} {verb} match the library · nothing is deleted")
            } else {
                format!("{n} {rows} missing or changed · only {files} back")
            });
        }
        Err(e) => return Needs::Attention(e.to_string()),
    }
    match set_aside(&session.audio()) {
        Ok(0) => {}
        Ok(n) => {
            let journals = if n == 1 { "journal" } else { "journals" };
            return Needs::Attention(format!(
                "{n} damaged {journals} set aside · audio salvage couldn't read is kept"
            ));
        }
        Err(e) => return Needs::Attention(format!("its audio can't be checked: {e}")),
    }
    match needs_salvage(&dir) {
        Ok(false) => Needs::Nothing,
        Ok(true) => match dir.lock() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Needs::InUse,
            _ => {
                Needs::Attention("audio still to save · nota tries again when it starts".to_owned())
            }
        },
        Err(e) => Needs::Attention(format!("its audio can't be checked: {e}")),
    }
}

/// How many journals salvage has set aside in `audio` as damaged: renamed
/// to a journal's name with `.unreadable` after it. What salvage could
/// read of them is published; the rest may hold audio it couldn't.
fn set_aside(audio: &Path) -> io::Result<usize> {
    Ok(StdFs
        .list(audio)?
        .iter()
        .filter_map(|path| path.file_name()?.to_str()?.strip_suffix(".unreadable"))
        .filter(|journal| JournalId::from_file_name(std::ffi::OsStr::new(journal)).is_some())
        .count())
}

/// How long the longest track's audio in `rows` is, at `rate`. `None`
/// with no rows.
fn longest_track(rows: &[SegmentRow], rate: SampleRate) -> Option<Duration> {
    let mut per_track: BTreeMap<TrackId, SampleCount> = BTreeMap::new();
    for row in rows {
        let total = per_track.entry(row.track()).or_insert(SampleCount::new(0));
        *total = total.saturating_add(row.range().len());
    }
    per_track
        .into_values()
        .max_by_key(|count| count.get())
        .and_then(|count| count.duration_at(rate))
}

/// Whether `session` holds nothing yet: an empty audio directory and no
/// per-session store, as `create` leaves it, or with only its kept row.
/// One that can't be listed isn't taken for empty.
fn is_empty(session: &SessionPaths) -> bool {
    let kept = [kept::KEPT, kept::KEPT_PARTIAL].map(|name| session.dir.join(name));
    let only_audio = StdFs.list(&session.dir).is_ok_and(|there| {
        there.contains(&session.audio())
            && there
                .iter()
                .all(|path| *path == session.audio() || kept.contains(path))
    });
    only_audio
        && StdFs
            .list(&session.audio())
            .is_ok_and(|audio| audio.is_empty())
}

fn no_number() -> io::Error {
    io::Error::other("couldn't find a free session number")
}

/// A new session's rows in the library database, as the recorder's store:
/// the session's own row is added before anything else is read or written,
/// when the database is first there to take it. Clones share the row: it's
/// added once, by whichever writes first (the publisher, or the saver of
/// live text, marks and notes).
#[derive(Debug, Clone)]
pub(crate) struct NewSessionRows {
    id: SessionId,
    db: Writer,
    /// The session's row, until it's added.
    pending: Arc<Mutex<Option<NewSession>>>,
}

impl NewSessionRows {
    /// `session`'s rows in `db`. Does no I/O.
    pub(crate) fn new(db: Writer, session: NewSession) -> Self {
        Self {
            id: session.id,
            db,
            pending: Arc::new(Mutex::new(Some(session))),
        }
    }

    /// The library database.
    pub(crate) const fn db(&self) -> &Writer {
        &self.db
    }

    /// Adds the session's row if it isn't added yet. Refuses a call for any
    /// other session.
    pub(crate) fn added(&self, session: SessionId) -> Result<(), StoreError> {
        if session != self.id {
            return Err(StoreError::NoSession(session));
        }
        // Nothing panics while holding it; the row is added or it isn't.
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(new) = &*pending else {
            return Ok(());
        };
        self.db.with(|db| match db.create_session(new) {
            // Added by an earlier call whose answer was lost: the same
            // session, as long as it's still the same row.
            Err(StoreError::SessionExists(id)) => match db.session(id)? {
                Some(s) if s.title == new.title && s.state == SessionState::Recording => Ok(()),
                _ => Err(StoreError::SessionExists(id)),
            },
            done => done,
        })?;
        *pending = None;
        Ok(())
    }
}

impl SegmentStore for NewSessionRows {
    type Error = StoreError;

    fn rows(&mut self, session: SessionId) -> Result<Vec<SegmentRow>, StoreError> {
        self.added(session)?;
        self.db.rows(session)
    }

    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), StoreError> {
        self.added(session)?;
        self.db.insert(session, segment)
    }

    fn is_disk_full(error: &StoreError) -> bool {
        error.is_disk_full()
    }

    fn unparsable_row(error: &StoreError) -> Option<RowKey> {
        Writer::unparsable_row(error)
    }
}

/// Makes `dir` and any missing parents, each durably.
fn ensure_dir(dir: &Path) -> io::Result<()> {
    match StdFs.list(dir) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let parent = parent_of(dir)
        .ok_or_else(|| io::Error::other(format!("{} has no parent", dir.display())))?;
    ensure_dir(parent)?;
    match StdFs.create_dir(dir) {
        Ok(()) => StdFs.sync_dir(parent),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// The directory `dir` is in: for a relative path with one part, the
/// working directory. `None` for a root.
fn parent_of(dir: &Path) -> Option<&Path> {
    match dir.parent()? {
        p if p.as_os_str().is_empty() => Some(Path::new(".")),
        p => Some(p),
    }
}

mod kept;

#[cfg(test)]
mod tests;
