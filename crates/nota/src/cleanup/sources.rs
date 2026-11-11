//! Where the app's clean-up finds its decider and packs: the local decider
//! the settings describe ([`crate::settings`]), and the packs installed in
//! the data directory's `packs/` folder.
//!
//! The settings are read again at each run (and each look at the queue),
//! so a decider set up while nota is open is used from the next run on.
//! Until Setup picks a session's decider (#163), every session uses the
//! local one.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nota_core::Clock;
use nota_core::decide::Place;
use nota_llm::local::LocalDecider;
use nota_store::{DecidedBy, Pack};

use super::{Setup, Sources};
use crate::settings::{self, Environment};

/// The folder in the data directory that holds the installed packs.
pub(crate) const PACKS: &str = "packs";

/// The most pack files read from the folder: far more than anyone
/// installs, and a bound on what a run reads.
const MAX_PACKS: usize = 256;

/// The app's decider and packs.
pub(crate) struct Installed {
    /// The installed packs' folder.
    packs: PathBuf,
    /// The clock the decider times its server by.
    clock: Arc<dyn Clock>,
}

impl Installed {
    /// The decider from the user's settings, timed by `clock`, and the
    /// packs installed under the data directory `data`.
    pub(crate) fn new(data: &Path, clock: Arc<dyn Clock>) -> Self {
        Self {
            packs: data.join(PACKS),
            clock,
        }
    }
}

impl Sources for Installed {
    fn decider(&self) -> Setup {
        match settings::local_decider(&Environment::current()) {
            settings::LocalDecider::NotSetUp => Setup::NotSetUp,
            settings::LocalDecider::Unavailable(why) => {
                Setup::Unavailable(format!("the decider can't be used: {why}"))
            }
            settings::LocalDecider::Ready(config) => {
                let by = DecidedBy {
                    name: "llama-server".to_owned(),
                    model: model_name(&config.model),
                    place: Place::Local,
                };
                let prompt = Box::new(nota_llm::prompt::English::new(None));
                let decider = LocalDecider::new(*config, prompt, Arc::clone(&self.clock));
                Setup::Ready(Box::new(decider), by)
            }
        }
    }

    fn packs(&self) -> Result<Vec<Pack>, String> {
        installed(&self.packs)
            .map_err(|e| format!("the packs in {} can't be read: {e}", self.packs.display()))
    }
}

/// A model's name, for the record: its file's name without the folder.
fn model_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// The packs in `dir` (`*.pack` files, at most [`MAX_PACKS`]), in file
/// name order; none if the folder isn't there. A file that isn't a valid
/// pack is passed over: a session that chose it finds it missing, and
/// says so.
fn installed(dir: &Path) -> io::Result<Vec<Pack>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pack"))
        .take(MAX_PACKS)
        .collect();
    paths.sort();
    Ok(paths.iter().filter_map(|p| Pack::open(p).ok()).collect())
}
