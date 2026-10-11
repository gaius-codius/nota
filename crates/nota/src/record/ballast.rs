//! The ballast, made as a recording ends: after its last segments are
//! published and its session is marked stopped, so nothing the recording
//! needs waits behind 256 MB of writes (on a slow disk ext4 holds every
//! fsync behind them, and a recording falls seconds behind its audio).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nota_recorder::disk::Ballast;
use nota_recorder::fs::{Fs, StdFs};

use crate::library::startup_in_use;

/// How many of the ballast's megabytes pass between looks for another
/// live recording: each look locks every session's audio directory.
const LOOK_EVERY: usize = 8;

/// Where the ballast goes and how big it is.
pub(super) struct BallastPlan {
    /// The data directory.
    pub(super) data: PathBuf,
    /// The ballast's size in bytes.
    pub(super) len: u64,
}

impl BallastPlan {
    /// The ballast of `len` bytes in the data directory `data`.
    pub(super) fn new(data: &Path, len: u64) -> Self {
        Self {
            data: data.to_path_buf(),
            len,
        }
    }

    /// Makes the ballast if there's none and `full` is false, giving it up
    /// when a signal asks nota to close or another recording starts or is
    /// found live. A note for the summary if it couldn't be made; none for
    /// no room, or when it was given up (the next recording to end makes
    /// it).
    pub(super) fn make(&self, full: bool) -> Option<String> {
        let asked = Arc::new(AtomicBool::new(false));
        let registered = ask_on_signals(&asked);
        let noted = self.make_on(&StdFs, &asked, full);
        for id in registered {
            signal_hook::low_level::unregister(id);
        }
        noted
    }

    /// [`BallastPlan::make`] on `fs`, given up when `asked` is set.
    fn make_on<S: Fs>(&self, fs: &S, asked: &AtomicBool, full: bool) -> Option<String> {
        let looks = AtomicUsize::new(0);
        let give_up = || {
            asked.load(Ordering::SeqCst)
                || looks
                    .fetch_add(1, Ordering::SeqCst)
                    .is_multiple_of(LOOK_EVERY)
                    && startup_in_use(fs, &self.data, &[]).unwrap_or(true)
        };
        // Not even started on a full disk, or while another recording is
        // live.
        if full || give_up() {
            return None;
        }
        match Ballast::keep(fs, &self.data, self.len, give_up) {
            Ok(_) => None,
            Err(e) => Some(format!(
                "the space nota keeps for a full disk couldn't be set aside: {e}"
            )),
        }
    }
}

/// Sets `asked` on SIGHUP, SIGTERM and SIGINT, which the recording's own
/// listener no longer handles by now; the ids to unregister.
fn ask_on_signals(asked: &Arc<AtomicBool>) -> Vec<signal_hook::SigId> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    [SIGHUP, SIGTERM, SIGINT]
        .into_iter()
        .filter_map(|signal| signal_hook::flag::register(signal, Arc::clone(asked)).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io;

    use nota_recorder::fs::fake::FakeFs;

    use super::*;

    fn plan() -> BallastPlan {
        BallastPlan {
            data: PathBuf::from("/data"),
            len: 4_096,
        }
    }

    /// A data directory with a sessions directory, as a library has.
    fn data() -> FakeFs {
        FakeFs::with_dirs(["/data", "/data/sessions"])
    }

    /// With nothing in the way, the ballast is made.
    #[test]
    fn the_ballast_is_made_when_nothing_is_in_the_way() {
        let fs = data();
        let noted = plan().make_on(&fs, &AtomicBool::new(false), false);
        assert_eq!(noted, None);
        assert_eq!(fs.paths(), [PathBuf::from("/data/ballast-4096")]);
    }

    /// Asked to close before it starts, nothing is written.
    #[test]
    fn a_signal_before_it_starts_makes_none() {
        let fs = data();
        let noted = plan().make_on(&fs, &AtomicBool::new(true), false);
        assert_eq!(noted, None);
        assert!(fs.paths().is_empty(), "{:?}", fs.paths());
    }

    /// A recording live in another session (its audio directory locked)
    /// keeps the stop from writing a ballast beside its journals.
    #[test]
    fn a_live_recording_elsewhere_makes_none() {
        let fs = data();
        fs.create_dir(Path::new("/data/sessions/2")).unwrap();
        fs.create_dir(Path::new("/data/sessions/2/audio")).unwrap();
        let _live = fs.lock_dir(Path::new("/data/sessions/2/audio")).unwrap();
        let noted = plan().make_on(&fs, &AtomicBool::new(false), false);
        assert_eq!(noted, None);
        assert!(fs.paths().iter().all(|p| !p.starts_with("/data/ballast")));
    }

    /// A ballast that can't be made says why, in the summary's words.
    #[test]
    fn a_ballast_that_cant_be_made_is_noted() {
        use nota_recorder::fs::fake::Fault;
        let fs = data();
        fs.fail_on(
            Path::new("/data/ballast-4096.tmp"),
            Fault::Create,
            io::ErrorKind::PermissionDenied,
        );
        let noted = plan().make_on(&fs, &AtomicBool::new(false), false).unwrap();
        assert!(
            noted.starts_with("the space nota keeps for a full disk couldn't be set aside: "),
            "{noted}"
        );
    }

    /// A full disk makes none, however clear the way is.
    #[test]
    fn a_full_disk_makes_none() {
        let fs = data();
        let noted = plan().make_on(&fs, &AtomicBool::new(false), true);
        assert_eq!(noted, None);
        assert!(fs.paths().is_empty(), "{:?}", fs.paths());
    }
}
