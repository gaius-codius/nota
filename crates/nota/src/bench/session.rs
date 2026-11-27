//! A session's candidates, as a clean-up run of it would read them: its
//! final text, its packs from those installed, and the proposals its own
//! runs made, which C3's savings are counted from.
//!
//! The library is only read. It's opened through nota-store, which brings
//! an older schema up to date as the app would; a missing library is
//! refused first, so the bench never makes one.

use std::path::Path;
use std::sync::Arc;

use nota_checks::English;
use nota_core::{Clock, SessionId};

use super::Candidates;
use crate::cleanup::sources::Installed;
use crate::cleanup::{Precheck, Screen, Sources, offering, read, saved};
use crate::library::LIBRARY_DB;

/// The candidates the lookup offers in `session`'s final text, in the
/// library under the data directory `data`, with its packs from those
/// installed there and C3 as `precheck`. `clock` is what a decider from
/// the same sources would be timed by; none is made here.
///
/// # Errors
///
/// The reason, for the user, if there's no library, it can't be read,
/// the packs can't be read, the session has no final text, or the term
/// lookup can't start.
pub(crate) fn from_session(
    data: &Path,
    session: SessionId,
    precheck: Precheck,
    clock: Arc<dyn Clock>,
) -> Result<Candidates, String> {
    let path = data.join(LIBRARY_DB);
    // Opening a missing library would make an empty one.
    if !path.is_file() {
        return Err(format!("there's no library at {}", path.display()));
    }
    let n = session.get();
    let db = nota_store::Store::open(&path)
        .map_err(|e| format!("the library {} can't be opened: {e}", path.display()))?;
    let packs = Installed::new(data, clock).packs()?;
    let read = read(&db, session, &packs).map_err(|e| format!("session {n} can't be read: {e}"))?;
    if read.lines.is_empty() {
        return Err(format!("session {n} has no final text to bench"));
    }
    let english = English::new().map_err(|e| format!("the term lookup can't start: {e}"))?;
    Ok(Candidates {
        source: format!("session {n}"),
        offering: offering(&english, &read.terms, &read.lines),
        screen: Screen::new(precheck, &read.lines),
        precheck,
        saved: Some(saved(&read.proposals)),
    })
}
