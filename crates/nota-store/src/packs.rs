//! Term packs: the vocabulary a session's clean-up looks terms up in.
//!
//! A pack is a separate, read-only SQLite file, apart from the library
//! database. It holds a manifest (its name, version, language, whether it's
//! shipped or someone's own, and each source with its licence and
//! attribution text) and its rows: terms, and the spoken-form aliases of
//! each ("gee el pee one" for GLP-1). Every row names its source, its
//! licence and its review status. Several sources may give the same term
//! or alias: each gives its own row.
//!
//! [`Pack::new`] is the one way to make a [`Pack`], and it checks every
//! rule below, so a pack that exists is a valid one; [`Pack::open`] reads a
//! file through the same checks, so a file edited or damaged outside nota
//! is refused rather than half read. A file whose tables aren't the
//! format's own (a view in place of one, say) is refused before any row is
//! read from it.
//!
//! **What is offered.** From a [`PackKind::Shipped`] pack, only `reviewed`
//! rows: a term none of whose rows is reviewed isn't offered, nor is a
//! draft alias. From a [`PackKind::Own`] pack (a user's glossary) every row
//! is offered, marked unreviewed unless it's reviewed: the author's word is
//! what the session chose. [`Pack::offered`] gives the plain term list the
//! lookup (C4) takes.
//!
//! **The file.** The format version is SQLite's `user_version`, and the
//! `application_id` says the file is a nota pack, so a library database
//! or any other SQLite file is refused. Tables a reader doesn't know are
//! ignored: one-line facts can come later as a table of their own without
//! a new format version. A pack is built in memory and written whole by
//! SQLite's `VACUUM INTO`, which writes pages in order with no journal, so
//! the same rows give the same bytes from the same SQLite. It's a build
//! output, not a recording or the library database, so it doesn't go
//! through the durable-write layer: a build that fails leaves a file to
//! delete and run again, and nothing is lost.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};

/// The pack format this code writes and reads.
pub const FORMAT: i64 = 1;

/// A pack's `application_id`: "nota" in ASCII.
const APPLICATION_ID: i32 = 0x6e6f_7461;

/// The page size a pack is written with, set so the file doesn't depend
/// on SQLite's compiled default.
const PAGE_SIZE: i64 = 4096;

/// The most term and alias rows a pack holds, together. Far above any real
/// pack (the pharmacy pack has a few thousand). Reading a file from outside
/// stops at this many rows in each table, before the rows are checked; past
/// that, what reading costs is bounded by the file's size.
pub const MAX_ROWS: usize = 1_000_000;

/// The longest name, version, language, term, alias, source id or licence,
/// in characters. A term is a word or a short phrase.
pub const MAX_TEXT: usize = 200;

/// The longest attribution text or source address, in characters: a
/// licence's notice runs to a few sentences.
pub const MAX_LONG_TEXT: usize = 4000;

/// The pack format's tables. Their checks repeat [`Pack::new`]'s, so a row
/// the code would refuse can't be stored either.
const SCHEMA: &str = "
CREATE TABLE manifest (
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    language TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('shipped', 'own'))
) STRICT;

CREATE TABLE source (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    licence TEXT NOT NULL CHECK (licence <> ''),
    attribution TEXT NOT NULL CHECK (attribution <> ''),
    url TEXT
) STRICT;

CREATE TABLE term (
    term TEXT NOT NULL CHECK (term <> ''),
    source TEXT NOT NULL REFERENCES source(id),
    licence TEXT NOT NULL CHECK (licence <> ''),
    review TEXT NOT NULL CHECK (review IN ('reviewed', 'draft')),
    PRIMARY KEY (term, source)
) STRICT;

CREATE TABLE alias (
    term TEXT NOT NULL,
    alias TEXT NOT NULL CHECK (alias <> ''),
    source TEXT NOT NULL REFERENCES source(id),
    licence TEXT NOT NULL CHECK (licence <> ''),
    review TEXT NOT NULL CHECK (review IN ('reviewed', 'draft')),
    PRIMARY KEY (term, alias, source)
) STRICT;
";

/// Whose a pack is, which decides what of it is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PackKind {
    /// Built from open sources and shipped with nota: only its reviewed
    /// rows are offered.
    Shipped,
    /// A user's own, built from their glossary: every row is offered, and
    /// shown as theirs.
    Own,
}

impl PackKind {
    /// The kind as the file stores it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shipped => "shipped",
            Self::Own => "own",
        }
    }

    /// The kind stored as `text`, if it's one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "shipped" => Some(Self::Shipped),
            "own" => Some(Self::Own),
            _ => None,
        }
    }
}

/// Whether a person has checked a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Review {
    /// Checked by a reviewer: offered from any pack.
    Reviewed,
    /// Not checked yet: offered only from the user's own packs, as
    /// unreviewed.
    Draft,
}

impl Review {
    /// The status as the file stores it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reviewed => "reviewed",
            Self::Draft => "draft",
        }
    }

    /// The status stored as `text`, if it's one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "reviewed" => Some(Self::Reviewed),
            "draft" => Some(Self::Draft),
            _ => None,
        }
    }
}

/// Where a pack's rows come from, and on what terms.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Source {
    /// The short name rows give for it (`rxnorm`).
    pub id: String,
    /// Its full name (`RxNorm, US National Library of Medicine`).
    pub name: String,
    /// Its licence (`public domain`, `CC0-1.0`).
    pub licence: String,
    /// The text its licence asks credits and NOTICE files to carry.
    pub attribution: String,
    /// Where it was taken from, if it has an address.
    pub url: Option<String>,
}

/// A pack's manifest: what it is, and its sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The pack's name (`pharmacy`), which sessions record it by.
    pub name: String,
    /// Its version (`2026.11`), which sessions record with the name. A pack
    /// rebuilt with other rows needs a new one.
    pub version: String,
    /// The language of its terms (`en`).
    pub language: String,
    /// Shipped or the user's own.
    pub kind: PackKind,
    /// Its sources, every one its rows name.
    pub sources: Vec<Source>,
}

impl Manifest {
    /// The pack as a session records it.
    #[must_use]
    pub fn pack_ref(&self) -> PackRef {
        PackRef {
            name: self.name.clone(),
            version: self.version.clone(),
        }
    }

    /// The credits for the pack's sources: for each, in id order, its
    /// name, licence, address if it has one, and attribution text. This is
    /// the pack's part of a NOTICE file and of the screen that credits the
    /// packs.
    #[must_use]
    pub fn notice(&self) -> String {
        let mut lines = vec![format!(
            "{} {} ({})",
            self.name, self.version, self.language
        )];
        for source in &self.sources {
            lines.push(String::new());
            lines.push(source.name.clone());
            lines.push(format!("Licence: {}", source.licence));
            if let Some(url) = &source.url {
                lines.push(format!("From: {url}"));
            }
            lines.push(source.attribution.clone());
        }
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }
}

/// A pack as a session records it: its name and version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackRef {
    /// The manifest's name.
    pub name: String,
    /// The manifest's version.
    pub version: String,
}

/// A term from one source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermRow {
    /// The term as it's written (`amlodipine`, `GLP-1`).
    pub term: String,
    /// The id of the source it's from.
    pub source: String,
    /// Its licence, which is its source's.
    pub licence: String,
    /// Whether it has been checked.
    pub review: Review,
}

/// A spoken form or other alias of a term, from one source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AliasRow {
    /// The term it's an alias of, which the pack has a row for.
    pub term: String,
    /// The alias (`gee el pee one`).
    pub alias: String,
    /// The id of the source it's from.
    pub source: String,
    /// Its licence, which is its source's.
    pub licence: String,
    /// Whether it has been checked.
    pub review: Review,
}

/// A term a session's lookup may offer, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Offered {
    /// The term as it's written.
    pub term: String,
    /// Its aliases that are offered, in order, without repeats.
    pub aliases: Vec<String>,
    /// The language of the pack it's from.
    pub language: String,
    /// The pack it's from.
    pub pack: PackRef,
    /// The ids of the sources its offered rows came from, in order.
    pub sources: Vec<String>,
    /// Whether every row it's offered from is reviewed. False for a user's
    /// own terms, which show as unreviewed.
    pub reviewed: bool,
}

/// Why a pack couldn't be built or read.
#[derive(Debug)]
pub enum PackError {
    /// SQLite reported an error. Nothing was written, or the file holds a
    /// partial pack to delete.
    Sqlite(rusqlite::Error),
    /// The path isn't valid UTF-8, which SQLite's `VACUUM INTO` needs.
    PathNotUtf8,
    /// The file isn't a nota pack: its `application_id` is another.
    NotAPack,
    /// The file is a nota pack in a format this code doesn't know.
    UnknownFormat(i64),
    /// A manifest field or a row's text is empty, too long, or has a line
    /// break, a tab or another control character in it.
    BadText {
        /// Which field.
        field: &'static str,
        /// The text, as given.
        text: String,
    },
    /// The manifest has no sources.
    NoSources,
    /// Two sources have the same id.
    DuplicateSource(String),
    /// A row names no source.
    MissingSource {
        /// The row's term.
        term: String,
    },
    /// A row has no licence.
    MissingLicence {
        /// The row's term.
        term: String,
    },
    /// A row names a source the manifest doesn't have.
    UnknownSource {
        /// The row's term.
        term: String,
        /// The source it names.
        source: String,
    },
    /// A row's licence isn't its source's.
    LicenceMismatch {
        /// The row's term.
        term: String,
        /// The row's source.
        source: String,
    },
    /// An alias names a term the pack has no row for.
    AliasWithoutTerm {
        /// The term it names.
        term: String,
        /// The alias.
        alias: String,
    },
    /// The same row is given twice with different review statuses.
    Conflict {
        /// The row's term.
        term: String,
        /// The row's source.
        source: String,
    },
    /// The pack has more than [`MAX_ROWS`] rows.
    TooManyRows,
    /// The file's manifest isn't one row, or a row holds a kind or review
    /// status that isn't one.
    Corrupt(String),
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "sqlite error: {e}"),
            Self::PathNotUtf8 => write!(f, "the pack's path is not valid UTF-8"),
            Self::NotAPack => write!(f, "the file is not a nota pack"),
            Self::UnknownFormat(v) => write!(f, "unknown pack format {v}"),
            Self::BadText { field, text } => write!(
                f,
                "the {field} {text:?} is empty, too long, or has a control character"
            ),
            Self::NoSources => write!(f, "the pack names no sources"),
            Self::DuplicateSource(id) => write!(f, "two sources are named {id:?}"),
            Self::MissingSource { term } => write!(f, "{term:?} has no source"),
            Self::MissingLicence { term } => write!(f, "{term:?} has no licence"),
            Self::UnknownSource { term, source } => {
                write!(f, "{term:?} names source {source:?}, which the pack lacks")
            }
            Self::LicenceMismatch { term, source } => {
                write!(f, "{term:?} has another licence than its source {source:?}")
            }
            Self::AliasWithoutTerm { term, alias } => {
                write!(f, "alias {alias:?} names {term:?}, which the pack lacks")
            }
            Self::Conflict { term, source } => write!(
                f,
                "{term:?} from {source:?} is given twice with different review statuses"
            ),
            Self::TooManyRows => write!(f, "the pack has more than {MAX_ROWS} rows"),
            Self::Corrupt(why) => write!(f, "the pack file is corrupt: {why}"),
        }
    }
}

impl std::error::Error for PackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for PackError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// A valid pack: its manifest and its rows, each list sorted and without
/// repeats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pack {
    manifest: Manifest,
    terms: Vec<TermRow>,
    aliases: Vec<AliasRow>,
}

impl Pack {
    /// A pack of `manifest`'s sources and these rows, checked. Its sources
    /// are sorted by id, and its rows sorted with exact repeats dropped,
    /// so the order they're given in changes nothing.
    ///
    /// # Errors
    ///
    /// [`PackError::BadText`] for an empty, long or control-character
    /// field; [`PackError::NoSources`] and [`PackError::DuplicateSource`]
    /// for a bad source list; for a row, [`PackError::MissingSource`],
    /// [`PackError::MissingLicence`], [`PackError::UnknownSource`],
    /// [`PackError::LicenceMismatch`], [`PackError::AliasWithoutTerm`] or
    /// [`PackError::Conflict`]; and [`PackError::TooManyRows`].
    pub fn new(
        mut manifest: Manifest,
        terms: Vec<TermRow>,
        aliases: Vec<AliasRow>,
    ) -> Result<Self, PackError> {
        if too_many(terms.len(), aliases.len(), MAX_ROWS) {
            return Err(PackError::TooManyRows);
        }
        check_manifest(&mut manifest)?;
        let licences: BTreeMap<&str, &str> = manifest
            .sources
            .iter()
            .map(|s| (s.id.as_str(), s.licence.as_str()))
            .collect();
        for row in &terms {
            check_row(&licences, &row.term, &row.source, &row.licence)?;
        }
        let terms = dedupe(
            terms,
            |r| (r.term.clone(), r.source.clone()),
            |r| (r.term.clone(), r.source.clone()),
        )?;
        let known: BTreeSet<&str> = terms.iter().map(|r| r.term.as_str()).collect();
        for row in &aliases {
            check_row(&licences, &row.term, &row.source, &row.licence)?;
            check_text("alias", &row.alias, MAX_TEXT)?;
            if !known.contains(row.term.as_str()) {
                return Err(PackError::AliasWithoutTerm {
                    term: row.term.clone(),
                    alias: row.alias.clone(),
                });
            }
        }
        let aliases = dedupe(
            aliases,
            |r| (r.term.clone(), r.alias.clone(), r.source.clone()),
            |r| (r.term.clone(), r.source.clone()),
        )?;
        Ok(Self {
            manifest,
            terms,
            aliases,
        })
    }

    /// The pack's manifest.
    #[must_use]
    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Its term rows, sorted by term and source.
    #[must_use]
    pub fn terms(&self) -> &[TermRow] {
        &self.terms
    }

    /// Its alias rows, sorted by term, alias and source.
    #[must_use]
    pub fn aliases(&self) -> &[AliasRow] {
        &self.aliases
    }

    /// The terms a session that chose this pack is offered, sorted by
    /// term: from a shipped pack, the reviewed ones with their reviewed
    /// aliases; from the user's own, every one with every alias.
    #[must_use]
    pub fn offered(&self) -> Vec<Offered> {
        let shipped = self.manifest.kind == PackKind::Shipped;
        let counts = |review: Review| !shipped || review == Review::Reviewed;
        let mut by_term: BTreeMap<&str, Offered> = BTreeMap::new();
        for row in self.terms.iter().filter(|r| counts(r.review)) {
            let entry = by_term.entry(&row.term).or_insert_with(|| Offered {
                term: row.term.clone(),
                aliases: Vec::new(),
                language: self.manifest.language.clone(),
                pack: self.manifest.pack_ref(),
                sources: Vec::new(),
                reviewed: true,
            });
            entry.sources.push(row.source.clone());
            entry.reviewed &= row.review == Review::Reviewed;
        }
        for row in self.aliases.iter().filter(|r| counts(r.review)) {
            if let Some(entry) = by_term.get_mut(row.term.as_str()) {
                entry.aliases.push(row.alias.clone());
                entry.sources.push(row.source.clone());
                entry.reviewed &= row.review == Review::Reviewed;
            }
        }
        by_term
            .into_values()
            .map(|mut offered| {
                // Rows are sorted by term, alias, source: one alias from
                // two sources comes twice in a row, and sources may repeat.
                offered.aliases.dedup();
                offered.sources.sort();
                offered.sources.dedup();
                offered
            })
            .collect()
    }

    /// Writes the pack to a new file at `out`. The same pack gives the same
    /// bytes from the same SQLite.
    ///
    /// # Errors
    ///
    /// [`PackError::PathNotUtf8`] if `out` isn't UTF-8, and
    /// [`PackError::Sqlite`] if SQLite fails, including when `out` is a
    /// file that isn't empty. A failure may leave a partial file at `out`.
    #[expect(
        clippy::disallowed_methods,
        reason = "the pack is built in memory, then written once to `out` by VACUUM INTO, which refuses a file that holds anything"
    )]
    pub fn write(&self, out: &Path) -> Result<(), PackError> {
        let out = out.to_str().ok_or(PackError::PathNotUtf8)?;
        let mut conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "page_size", PAGE_SIZE)?;
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
        conn.pragma_update(None, "user_version", FORMAT)?;
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA)?;
        self.insert(&tx)?;
        tx.commit()?;
        conn.execute("VACUUM INTO ?1", [out])?;
        Ok(())
    }

    /// Inserts the manifest and rows, in sorted order, inside `tx`.
    fn insert(&self, tx: &rusqlite::Transaction<'_>) -> Result<(), PackError> {
        let m = &self.manifest;
        tx.execute(
            "INSERT INTO manifest VALUES (?1, ?2, ?3, ?4)",
            params![m.name, m.version, m.language, m.kind.as_str()],
        )?;
        for s in &m.sources {
            tx.execute(
                "INSERT INTO source VALUES (?1, ?2, ?3, ?4, ?5)",
                params![s.id, s.name, s.licence, s.attribution, s.url],
            )?;
        }
        let mut stmt = tx.prepare("INSERT INTO term VALUES (?1, ?2, ?3, ?4)")?;
        for r in &self.terms {
            stmt.execute(params![r.term, r.source, r.licence, r.review.as_str()])?;
        }
        let mut stmt = tx.prepare("INSERT INTO alias VALUES (?1, ?2, ?3, ?4, ?5)")?;
        for r in &self.aliases {
            stmt.execute(params![
                r.term,
                r.alias,
                r.source,
                r.licence,
                r.review.as_str()
            ])?;
        }
        Ok(())
    }

    /// The pack in the file at `path`, read only, never through a symlink,
    /// and checked as [`Pack::new`] checks a pack.
    ///
    /// # Errors
    ///
    /// [`PackError::NotAPack`] for another kind of file,
    /// [`PackError::UnknownFormat`] for a format this code doesn't read,
    /// [`PackError::Corrupt`] for a manifest that isn't one row or a value
    /// that isn't one, any of [`Pack::new`]'s errors for rows that break its
    /// rules, and [`PackError::Sqlite`] if SQLite fails, including when
    /// there's no file.
    pub fn open(path: &Path) -> Result<Self, PackError> {
        let conn = crate::connect(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| match e {
            crate::StoreError::Sqlite(e) => PackError::Sqlite(e),
            other => PackError::Corrupt(other.to_string()),
        })?;
        let (id, format): (i32, i64) = conn.query_row(
            "SELECT (SELECT application_id FROM pragma_application_id), \
                    (SELECT user_version FROM pragma_user_version)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if id != APPLICATION_ID {
            return Err(PackError::NotAPack);
        }
        if format != FORMAT {
            return Err(PackError::UnknownFormat(format));
        }
        check_schema(&conn)?;
        let manifest = read_manifest(&conn)?;
        let terms = read_rows(
            &conn,
            "SELECT term, source, licence, review FROM term",
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?)),
        )?
        .into_iter()
        .map(|(term, source, licence, review)| {
            Ok(TermRow {
                term,
                source,
                licence,
                review: parse_review(&review)?,
            })
        })
        .collect::<Result<Vec<_>, PackError>>()?;
        let aliases = read_rows(
            &conn,
            "SELECT term, alias, source, licence, review FROM alias",
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )?
        .into_iter()
        .map(|(term, alias, source, licence, review)| {
            Ok(AliasRow {
                term,
                alias,
                source,
                licence,
                review: parse_review(&review)?,
            })
        })
        .collect::<Result<Vec<_>, PackError>>()?;
        Self::new(manifest, terms, aliases)
    }
}

/// Whether `terms` and `aliases` rows together are more than `max`.
const fn too_many(terms: usize, aliases: usize, max: usize) -> bool {
    terms.saturating_add(aliases) > max
}

/// Checks the manifest's fields and sources, and sorts the sources by id.
fn check_manifest(manifest: &mut Manifest) -> Result<(), PackError> {
    check_text("pack name", &manifest.name, MAX_TEXT)?;
    check_text("pack version", &manifest.version, MAX_TEXT)?;
    check_text("pack language", &manifest.language, MAX_TEXT)?;
    if manifest.sources.is_empty() {
        return Err(PackError::NoSources);
    }
    manifest.sources.sort();
    for pair in manifest.sources.windows(2) {
        if let [a, b] = pair
            && a.id == b.id
        {
            return Err(PackError::DuplicateSource(a.id.clone()));
        }
    }
    for s in &manifest.sources {
        check_text("source id", &s.id, MAX_TEXT)?;
        check_text("source name", &s.name, MAX_TEXT)?;
        check_text("source licence", &s.licence, MAX_TEXT)?;
        check_long_text("source attribution", &s.attribution)?;
        if let Some(url) = &s.url {
            check_text("source address", url, MAX_LONG_TEXT)?;
        }
    }
    Ok(())
}

/// Checks a row's term, source and licence against the manifest's
/// `licences`, keyed by source id.
fn check_row(
    licences: &BTreeMap<&str, &str>,
    term: &str,
    source: &str,
    licence: &str,
) -> Result<(), PackError> {
    check_text("term", term, MAX_TEXT)?;
    if source.is_empty() {
        return Err(PackError::MissingSource {
            term: term.to_owned(),
        });
    }
    if licence.is_empty() {
        return Err(PackError::MissingLicence {
            term: term.to_owned(),
        });
    }
    match licences.get(source) {
        None => Err(PackError::UnknownSource {
            term: term.to_owned(),
            source: source.to_owned(),
        }),
        Some(&theirs) if theirs != licence => Err(PackError::LicenceMismatch {
            term: term.to_owned(),
            source: source.to_owned(),
        }),
        Some(_) => Ok(()),
    }
}

/// Checks one line of text: not empty, at most `max` characters, no
/// control characters (line breaks and tabs included), and no space at
/// either end, which would make two spellings of one term.
fn check_text(field: &'static str, text: &str, max: usize) -> Result<(), PackError> {
    let bad = text.is_empty()
        || text.chars().count() > max
        || text.chars().any(char::is_control)
        || text.trim() != text;
    if bad {
        return Err(PackError::BadText {
            field,
            text: text.to_owned(),
        });
    }
    Ok(())
}

/// Checks a few lines of text, such as a licence's notice: not empty or
/// blank, at most [`MAX_LONG_TEXT`] characters, and no control characters
/// but line breaks.
fn check_long_text(field: &'static str, text: &str) -> Result<(), PackError> {
    let bad = text.trim().is_empty()
        || text.chars().count() > MAX_LONG_TEXT
        || text.chars().any(|c| c.is_control() && c != '\n');
    if bad {
        return Err(PackError::BadText {
            field,
            text: text.to_owned(),
        });
    }
    Ok(())
}

/// `rows` sorted, with exact repeats dropped. Two rows with the same
/// `key` that still differ (in review status: the key holds everything
/// else) are a [`PackError::Conflict`], named by `name`.
fn dedupe<R: Ord + Clone, K: Ord>(
    mut rows: Vec<R>,
    key: impl Fn(&R) -> K,
    name: impl Fn(&R) -> (String, String),
) -> Result<Vec<R>, PackError> {
    rows.sort();
    rows.dedup();
    for pair in rows.windows(2) {
        if let [a, b] = pair
            && key(a) == key(b)
        {
            let (term, source) = name(a);
            return Err(PackError::Conflict { term, source });
        }
    }
    Ok(rows)
}

/// The file's one manifest row and its sources.
fn read_manifest(conn: &Connection) -> Result<Manifest, PackError> {
    let rows = read_rows(
        conn,
        "SELECT name, version, language, kind FROM manifest",
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?)),
    )?;
    let [(name, version, language, kind)] = <[_; 1]>::try_from(rows)
        .map_err(|rows| PackError::Corrupt(format!("{} manifest rows", rows.len())))?;
    let kind = PackKind::parse(&kind)
        .ok_or_else(|| PackError::Corrupt(format!("unknown pack kind {kind:?}")))?;
    let sources = read_rows(
        conn,
        "SELECT id, name, licence, attribution, url FROM source",
        |r| {
            Ok(Source {
                id: r.get(0)?,
                name: r.get(1)?,
                licence: r.get(2)?,
                attribution: r.get(3)?,
                url: r.get(4)?,
            })
        },
    )?;
    Ok(Manifest {
        name,
        version,
        language,
        kind,
        sources,
    })
}

/// The tables a pack's rows are read from.
const TABLES: [&str; 4] = ["alias", "manifest", "source", "term"];

/// Checks that the file's four tables are the format's own, as [`SCHEMA`]
/// makes them, before anything is read from them. A file from outside
/// could make one a view, or give it generated columns, and so run any
/// SQL, as long as it liked, at the first read; this refuses such a file
/// before it's read. (Tables it doesn't know are left alone: nothing reads
/// them.) Schema code from the file is also kept from calling functions
/// that aren't safe in a schema (`trusted_schema`).
#[expect(
    clippy::disallowed_methods,
    reason = "an empty in-memory database to compare the schema with; it writes no file"
)]
fn check_schema(conn: &Connection) -> Result<(), PackError> {
    conn.pragma_update(None, "trusted_schema", false)?;
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SCHEMA)?;
    let found = schema_of(conn)?;
    if found != schema_of(&expected)? {
        return Err(PackError::Corrupt(
            "its tables aren't the pack format's".to_owned(),
        ));
    }
    Ok(())
}

/// The type and SQL of each of [`TABLES`] in `conn`'s schema, in name
/// order; a table that's missing is missing from the list.
fn schema_of(conn: &Connection) -> Result<Vec<(String, String, Option<String>)>, PackError> {
    let mut stmt = conn.prepare(
        "SELECT name, type, sql FROM sqlite_schema \
         WHERE name IN (?1, ?2, ?3, ?4) ORDER BY name",
    )?;
    let rows = stmt
        .query_map(TABLES, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The rows `sql` selects, at most [`MAX_ROWS`] of them.
fn read_rows<T>(
    conn: &Connection,
    sql: &str,
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, PackError> {
    read_at_most(conn, sql, MAX_ROWS, map)
}

/// The rows `sql` selects, refused as [`PackError::TooManyRows`] if there
/// are more than `max`.
fn read_at_most<T>(
    conn: &Connection,
    sql: &str,
    max: usize,
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, PackError> {
    let limit = i64::try_from(max).unwrap_or(i64::MAX).saturating_add(1);
    let mut stmt = conn.prepare(&format!("{sql} LIMIT {limit}"))?;
    let rows = stmt.query_map([], map)?.collect::<Result<Vec<_>, _>>()?;
    if rows.len() > max {
        return Err(PackError::TooManyRows);
    }
    Ok(rows)
}

/// A stored review status.
fn parse_review(text: &str) -> Result<Review, PackError> {
    Review::parse(text).ok_or_else(|| PackError::Corrupt(format!("unknown review status {text:?}")))
}

#[cfg(test)]
mod tests;
