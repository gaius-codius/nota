//! A fixture's candidates, for a bench run without a session: a JSON file
//! of terms and chunks, `{"terms": ["amlodipine"], "chunks": ["start amylo
//! diping daily"]}`.
//!
//! Each chunk is on track 0, 30 s after the one before; its words are
//! split at single spaces and have no log-probabilities, so C3, when it's
//! on, is a plain pre-check. Each term is English, with no aliases. Every
//! candidate the lookup offers is asked about: a fixture has no learned
//! pairs to bar any.

use std::collections::BTreeMap;
use std::path::Path;

use nota_checks::{English, Term};
use nota_core::{SampleIndex, TrackId};
use nota_store::ChunkKey;
use serde_json::Value;

use super::{Candidates, read_text};
use crate::cleanup::chunk::Chunk;
use crate::cleanup::{Precheck, Screen, offering_in};

/// The largest fixture read, in bytes: 10 000 chunks of a few hundred
/// words each fit well inside it.
const MAX_FIXTURE: u64 = 32 * 1024 * 1024;

/// The most terms, and the most chunks, a fixture may hold: more than any
/// recording's final text, and a bound on what one run asks.
const MAX_ITEMS: usize = 10_000;

/// Where each chunk starts after the one before, in samples: 30 s at the
/// final pass's 16 kHz.
const CHUNK_SAMPLES: u64 = 480_000;

/// The track a fixture's chunks are on.
const TRACK: TrackId = TrackId::new(0);

/// What a fixture file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fixture {
    /// The terms the lookup offers, in order.
    pub(crate) terms: Vec<String>,
    /// The chunks' text, in order.
    pub(crate) chunks: Vec<String>,
}

/// The candidates the lookup offers in the fixture at `path`, with C3 as
/// `precheck`.
///
/// # Errors
///
/// The reason, for the user, if the file can't be read or isn't a
/// fixture, or the term lookup can't start.
pub(crate) fn from_fixture(path: &Path, precheck: Precheck) -> Result<Candidates, String> {
    let fixture = parse(&read_text(path, MAX_FIXTURE)?)
        .map_err(|e| format!("{}: not a bench fixture: {e}", path.display()))?;
    let english = English::new().map_err(|e| format!("the term lookup can't start: {e}"))?;
    let vocab: Vec<Term> = fixture
        .terms
        .iter()
        .map(|term| Term {
            term: term.clone(),
            aliases: Vec::new(),
            language: "en".to_owned(),
        })
        .collect();
    let chunks = chunks(&fixture.chunks);
    let offering = offering_in(&english, &vocab, &BTreeMap::new(), &|_, _| true, chunks);
    Ok(Candidates {
        source: format!("fixture {}", path.display()),
        offering,
        screen: Screen::new(precheck, &[]),
        precheck,
        saved: None,
    })
}

/// `texts` as chunks on [`TRACK`], [`CHUNK_SAMPLES`] apart. A blank text
/// keeps its place but is no chunk: it has nothing to offer, and as the
/// next chunk's context it would make that chunk's question unaskable.
fn chunks(texts: &[String]) -> Vec<Chunk> {
    (0_u64..)
        .zip(texts)
        .filter(|(_, text)| !text.trim().is_empty())
        .filter_map(|(i, text)| {
            let key = ChunkKey {
                track: TRACK,
                start: SampleIndex::new(i.saturating_mul(CHUNK_SAMPLES)),
            };
            Chunk::new(key, text.split(' ').map(|word| (word, None)))
        })
        .collect()
}

/// The fixture in `text`: an object with exactly `terms` and `chunks`,
/// each a list of at most [`MAX_ITEMS`] strings.
///
/// # Errors
///
/// What's wrong with its shape, lowercase.
pub(crate) fn parse(text: &str) -> Result<Fixture, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Value::Object(object) = value else {
        return Err("not a JSON object".to_owned());
    };
    if let Some(key) = object
        .keys()
        .find(|k| !matches!(k.as_str(), "terms" | "chunks"))
    {
        return Err(format!("unknown key {key}"));
    }
    Ok(Fixture {
        terms: strings(object.get("terms"), "terms")?,
        chunks: strings(object.get("chunks"), "chunks")?,
    })
}

/// The list of strings `value`, the field `name`.
fn strings(value: Option<&Value>, name: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Err(format!("{name} is missing"));
    };
    let Value::Array(items) = value else {
        return Err(format!("{name} isn't a list"));
    };
    if items.len() > MAX_ITEMS {
        return Err(format!("{name} has over {MAX_ITEMS} items"));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{name} holds something other than text"))
        })
        .collect()
}
