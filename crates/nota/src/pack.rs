//! `nota pack`: builds term packs ([`nota_store::packs`]) from text.
//!
//! - `nota pack build DIR --out FILE` builds a pack (shipped, as a rule)
//!   from the committed lists in `DIR`: its `pack.toml` (the manifest, and
//!   for each source the lists it gives) and those lists, which must be
//!   files in `DIR` itself. They're all it reads, so the same lists give
//!   the same file from the same build of nota.
//! - `nota pack glossary FILE --out FILE --author NAME` builds a user's own
//!   pack from a plain-text glossary.
//! - `nota pack notice FILE` writes a pack's credits to stdout, for a
//!   NOTICE file.
//!
//! **`pack.toml`** holds `name`, `version`, `language` and `kind`
//! (`shipped` or `own`), and one `[[source]]` table per source: `id`,
//! `name`, `licence`, `attribution`, an optional `url`, and `lists`, the
//! list files (by name, in the directory) whose rows it gives.
//!
//! **A list** is UTF-8 text, one row per line, fields split by tabs: a term
//! and its review status (`reviewed` or `draft`) make a term row; a third
//! field makes the row an alias of that term. Blank lines and lines
//! starting `#` are skipped. Every row of a list has its source's id and
//! licence.
//!
//! **A glossary** is UTF-8 text, one term per line, with any aliases after
//! it, each after a tab: `GLP-1<TAB>gee el pee one<TAB>glp one`. Spaces at
//! either end of a field are dropped, empty fields skipped, and blank lines
//! and lines starting `#` skipped. Its rows are the author's: their source
//! is the author, their licence the author's own, and they're drafts, so
//! they show as the author's, not as reviewed rows.

use std::fmt::Write as _;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use nota_store::packs::{AliasRow, Manifest, PackKind, Review, Source, TermRow};
use nota_store::{Pack, PackError};

/// The largest list, glossary or manifest read, in bytes. The pharmacy
/// pack's lists are well under 1 MiB each (the repository's file limit).
const MAX_FILE: u64 = 8 * 1024 * 1024;

/// A glossary pack's source id, and the licence of its rows.
const AUTHOR: &str = "author";
const OWN_LICENCE: &str = "the author's own";

/// What `nota pack` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PackCommand {
    /// Build the shipped pack in a directory of lists.
    Build {
        /// The directory holding `pack.toml`.
        dir: PathBuf,
        /// The pack file to write.
        out: PathBuf,
    },
    /// Build a user's own pack from a glossary.
    Glossary {
        /// The glossary file.
        file: PathBuf,
        /// The pack file to write.
        out: PathBuf,
        /// The manifest the pack gets.
        about: GlossaryAbout,
    },
    /// Write a pack's credits to stdout.
    Notice {
        /// The pack file.
        pack: PathBuf,
    },
}

/// What a glossary pack's manifest says beyond its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlossaryAbout {
    /// The pack's name.
    pub(crate) name: String,
    /// Its version.
    pub(crate) version: String,
    /// Its language.
    pub(crate) language: String,
    /// Who wrote the glossary: the source of its rows.
    pub(crate) author: String,
}

/// `nota pack`'s arguments, after the word `pack`.
pub(crate) fn parse(args: &[std::ffi::OsString]) -> Result<PackCommand, String> {
    let word = |i: usize| args.get(i).and_then(|a| a.to_str());
    let path = |i: usize| {
        args.get(i)
            .map(PathBuf::from)
            .ok_or_else(|| "a path is missing".to_owned())
    };
    match word(0) {
        Some("build") => {
            let dir = path(1)?;
            let flags = crate::pairs(args.get(2..).unwrap_or_default())?;
            let mut out = None;
            for (flag, value) in flags {
                match flag {
                    "--out" => out = Some(PathBuf::from(value)),
                    _ => return Err(format!("unknown option {flag}")),
                }
            }
            Ok(PackCommand::Build {
                dir,
                out: out.ok_or("--out is required")?,
            })
        }
        Some("glossary") => parse_glossary(path(1)?, args.get(2..).unwrap_or_default()),
        Some("notice") if args.len() == 2 => Ok(PackCommand::Notice { pack: path(1)? }),
        _ => Err("pack takes build, glossary or notice".to_owned()),
    }
}

/// `nota pack glossary FILE`'s options. The name defaults to the file's
/// name without its extension, the version to `1` and the language to
/// `en`.
fn parse_glossary(file: PathBuf, args: &[std::ffi::OsString]) -> Result<PackCommand, String> {
    let (mut out, mut author, mut name, mut version, mut language) = (None, None, None, None, None);
    for (flag, value) in crate::pairs(args)? {
        let text = || {
            value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{flag} takes UTF-8 text"))
        };
        match flag {
            "--out" => out = Some(PathBuf::from(value)),
            "--author" => author = Some(text()?),
            "--name" => name = Some(text()?),
            "--version" => version = Some(text()?),
            "--language" => language = Some(text()?),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    let name = match name {
        Some(name) => name,
        None => file
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .ok_or("--name is required for this file name")?,
    };
    let about = GlossaryAbout {
        name,
        version: version.unwrap_or_else(|| "1".to_owned()),
        language: language.unwrap_or_else(|| "en".to_owned()),
        author: author.ok_or("--author is required")?,
    };
    Ok(PackCommand::Glossary {
        file,
        out: out.ok_or("--out is required")?,
        about,
    })
}

/// Runs `command`, writing to `stdout` what it prints.
pub(crate) fn run(command: &PackCommand, stdout: &mut dyn io::Write) -> Result<(), String> {
    match command {
        PackCommand::Build { dir, out } => {
            let pack = build_dir(dir)?;
            write(&pack, out)?;
            say(stdout, &summary(&pack))
        }
        PackCommand::Glossary { file, out, about } => {
            let pack = glossary(&read_text(file)?, about)
                .map_err(|e| format!("{}: {e}", file.display()))?;
            write(&pack, out)?;
            say(stdout, &summary(&pack))
        }
        PackCommand::Notice { pack } => {
            let pack = Pack::open(pack).map_err(|e| format!("{}: {e}", pack.display()))?;
            say(stdout, pack.manifest().notice().trim_end())
        }
    }
}

/// Writes `text` and a line break to `stdout`.
fn say(stdout: &mut dyn io::Write, text: &str) -> Result<(), String> {
    writeln!(stdout, "{text}").map_err(|e| format!("couldn't write to stdout: {e}"))
}

/// Writes `pack` to the new file `out`.
fn write(pack: &Pack, out: &Path) -> Result<(), String> {
    pack.write(out)
        .map_err(|e| format!("{}: {e}", out.display()))
}

/// The file at `path` as text, if it's UTF-8 and at most [`MAX_FILE`]
/// bytes.
fn read_text(path: &Path) -> Result<String, String> {
    let name = path.display();
    let file = std::fs::File::open(path).map_err(|e| format!("{name}: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{name}: {e}"))?;
    // The size first: a cut at the limit may fall inside a character.
    if bytes.len() as u64 > MAX_FILE {
        return Err(format!("{name}: over {MAX_FILE} bytes"));
    }
    String::from_utf8(bytes).map_err(|e| format!("{name}: not UTF-8: {e}"))
}

/// The shipped pack built from `dir`'s `pack.toml` and the lists it names.
pub(crate) fn build_dir(dir: &Path) -> Result<Pack, String> {
    let toml_path = dir.join("pack.toml");
    let (manifest, lists) = parse_manifest(&read_text(&toml_path)?)
        .map_err(|e| format!("{}: {e}", toml_path.display()))?;
    let (mut terms, mut aliases) = (Vec::new(), Vec::new());
    for (source, file) in &lists {
        let path = dir.join(file);
        let (t, a) = parse_list(&read_text(&path)?, source)
            .map_err(|e| format!("{}:{e}", path.display()))?;
        terms.extend(t);
        aliases.extend(a);
    }
    Pack::new(manifest, terms, aliases).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Whether `file` names a file in the pack's directory itself: one
/// plain part, not absolute and not `..`, so a build reads only its own
/// directory.
fn is_plain_name(file: &str) -> bool {
    let mut parts = Path::new(file).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

/// A manifest from `pack.toml`'s text, and each source's lists with the
/// source they belong to, in the order given.
fn parse_manifest(text: &str) -> Result<(Manifest, Vec<(Source, String)>), String> {
    let table: toml::Table = text.parse().map_err(|e| format!("not TOML: {e}"))?;
    let kind = string(&table, "kind")?;
    let kind = PackKind::parse(&kind).ok_or_else(|| format!("unknown kind {kind:?}"))?;
    let raw_sources = match table.get("source") {
        Some(toml::Value::Array(items)) => items.as_slice(),
        Some(_) => return Err("source must be a list of tables".to_owned()),
        None => &[],
    };
    let mut sources = Vec::new();
    let mut lists = Vec::new();
    for item in raw_sources {
        let toml::Value::Table(t) = item else {
            return Err("each source must be a table".to_owned());
        };
        let source = Source {
            id: string(t, "id")?,
            name: string(t, "name")?,
            licence: string(t, "licence")?,
            attribution: string(t, "attribution")?,
            url: t.get("url").map(|_| string(t, "url")).transpose()?,
        };
        let Some(toml::Value::Array(files)) = t.get("lists") else {
            return Err(format!("source {:?} needs lists", source.id));
        };
        for file in files {
            let file = file
                .as_str()
                .filter(|f| is_plain_name(f))
                .ok_or_else(|| format!("source {:?}'s lists must be file names", source.id))?;
            lists.push((source.clone(), file.to_owned()));
        }
        sources.push(source);
    }
    let manifest = Manifest {
        name: string(&table, "name")?,
        version: string(&table, "version")?,
        language: string(&table, "language")?,
        kind,
        sources,
    };
    Ok((manifest, lists))
}

/// `table`'s text value `key`.
fn string(table: &toml::Table, key: &str) -> Result<String, String> {
    match table.get(key) {
        Some(toml::Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("{key} must be text")),
        None => Err(format!("{key} is missing")),
    }
}

/// The term and alias rows in a list's `text`, every one from `source`. An
/// error starts with the line's number.
fn parse_list(text: &str, source: &Source) -> Result<(Vec<TermRow>, Vec<AliasRow>), String> {
    let (mut terms, mut aliases) = (Vec::new(), Vec::new());
    for (n, line) in (1_usize..).zip(text.lines()) {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let (term, review, alias) = match fields[..] {
            [term, review] => (term, review, None),
            [term, review, alias] => (term, review, Some(alias)),
            _ => return Err(format!("{n}: a row is a term, a status and maybe an alias")),
        };
        let review = Review::parse(review)
            .ok_or_else(|| format!("{n}: {review:?} is not reviewed or draft"))?;
        match alias {
            None => terms.push(TermRow {
                term: term.to_owned(),
                source: source.id.clone(),
                licence: source.licence.clone(),
                review,
            }),
            Some(alias) => aliases.push(AliasRow {
                term: term.to_owned(),
                alias: alias.to_owned(),
                source: source.id.clone(),
                licence: source.licence.clone(),
                review,
            }),
        }
    }
    Ok((terms, aliases))
}

/// The user's own pack from a glossary's `text`.
pub(crate) fn glossary(text: &str, about: &GlossaryAbout) -> Result<Pack, PackError> {
    let source = Source {
        id: AUTHOR.to_owned(),
        name: about.author.clone(),
        licence: OWN_LICENCE.to_owned(),
        attribution: format!("Written by {}.", about.author),
        url: None,
    };
    let row = |term: &str| TermRow {
        term: term.to_owned(),
        source: AUTHOR.to_owned(),
        licence: OWN_LICENCE.to_owned(),
        review: Review::Draft,
    };
    let alias_row = |term: &str, alias: &str| AliasRow {
        term: term.to_owned(),
        alias: alias.to_owned(),
        source: AUTHOR.to_owned(),
        licence: OWN_LICENCE.to_owned(),
        review: Review::Draft,
    };
    let (mut terms, mut aliases) = (Vec::new(), Vec::new());
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t').map(str::trim).filter(|f| !f.is_empty());
        let Some(term) = fields.next() else {
            continue;
        };
        terms.push(row(term));
        aliases.extend(fields.map(|alias| alias_row(term, alias)));
    }
    let manifest = Manifest {
        name: about.name.clone(),
        version: about.version.clone(),
        language: about.language.clone(),
        kind: PackKind::Own,
        sources: vec![source],
    };
    Pack::new(manifest, terms, aliases)
}

/// The pack's rows in a few words, for saying what a build made.
pub(crate) fn summary(pack: &Pack) -> String {
    let m = pack.manifest();
    let mut text = format!(
        "{} {}: {} term rows, {} alias rows, {} offered terms",
        m.name,
        m.version,
        pack.terms().len(),
        pack.aliases().len(),
        pack.offered().len()
    );
    for source in &m.sources {
        let _ = write!(text, "\n  {} ({})", source.name, source.licence);
    }
    text
}

#[cfg(test)]
mod tests;
