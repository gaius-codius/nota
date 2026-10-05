//! The engine child's tie to its recorder: the engine ends when the
//! recorder dies, even while it's busy.
//!
//! The engine runs in a process group of its own, so the only portable
//! sign that the recorder has died (SIGKILL, a crash) is its stdin closing,
//! and it reads stdin only between steps: not while it loads its models or
//! decodes a chunk. On Linux, [`tie_to_recorder`] closes that gap: it asks
//! the kernel to send the engine SIGKILL when its parent dies, then checks
//! that the parent is still the recorder that started it, in case the
//! recorder died before the call. The recorder names itself in
//! [`RECORDER_PID_VAR`] when it starts an engine.
//!
//! The kernel sends the signal when the *thread* that started the engine
//! ends, not the whole process, so the recorder starts its engines from a
//! thread that outlives them (the supervisor's).
//!
//! Elsewhere there's no tie, and stdin closing stays the only path.

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::ffi::OsStr;
use std::io;

/// The environment variable in which the recorder passes its process id to
/// the engines it starts.
pub const RECORDER_PID_VAR: &str = "NOTA_RECORDER_PID";

/// What [`tie_to_recorder`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Tie {
    /// The engine dies with its parent. If the recorder named itself, the
    /// parent is that recorder.
    Tied,
    /// The recorder that started the engine had already gone: the engine
    /// should exit now.
    Orphaned,
    /// Not on this platform, or the kernel refused (a sandbox's policy, say):
    /// the engine ends when its stdin closes.
    Unsupported,
}

/// Ties this process (the engine) to its parent (the recorder): the kernel
/// kills it when the parent dies. Call it first thing, before anything
/// slow. Off Linux and Android, or if the kernel refuses, it's
/// [`Tie::Unsupported`], and the engine still works: stdin closing ends it.
/// The parent is checked either way.
///
/// # Errors
///
/// If [`RECORDER_PID_VAR`] is set but isn't a process id.
pub fn tie_to_recorder() -> io::Result<Tie> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use rustix::process::{Signal, getppid, set_parent_process_death_signal};

        let tied = set_parent_process_death_signal(Some(Signal::KILL)).is_ok();
        // Read after the signal is set: a recorder that dies from here on
        // kills the engine, and one that died before has left it with
        // another parent.
        let parent = getppid().map(|pid| pid.as_raw_nonzero().get());
        check_parent(tied, std::env::var_os(RECORDER_PID_VAR).as_deref(), parent)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    Ok(Tie::Unsupported)
}

/// Whether the engine's `parent` is the recorder named by `named` (the
/// variable's value), if one is named, given whether the kernel took the
/// death signal (`tied`).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn check_parent(tied: bool, named: Option<&OsStr>, parent: Option<i32>) -> io::Result<Tie> {
    let untied = if tied { Tie::Tied } else { Tie::Unsupported };
    let Some(named) = named else {
        return Ok(untied);
    };
    let recorder = named
        .to_str()
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|&pid| pid > 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{RECORDER_PID_VAR} isn't a process id: {:?}",
                    named.to_string_lossy()
                ),
            )
        })?;
    Ok(if parent == Some(recorder) {
        untied
    } else {
        Tie::Orphaned
    })
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn a_parent_that_is_the_named_recorder_is_tied() {
        let tie = check_parent(true, Some(OsStr::new("4242")), Some(4242)).unwrap();
        assert_eq!(tie, Tie::Tied);
    }

    #[test]
    fn a_parent_that_isnt_the_named_recorder_is_orphaned() {
        // Reparented to init, to a subreaper, or with no parent at all.
        for parent in [Some(1), Some(4243), None] {
            let tie = check_parent(true, Some(OsStr::new("4242")), parent).unwrap();
            assert_eq!(tie, Tie::Orphaned, "{parent:?}");
        }
    }

    #[test]
    fn with_no_recorder_named_any_parent_is_tied() {
        assert_eq!(check_parent(true, None, Some(1)).unwrap(), Tie::Tied);
        assert_eq!(check_parent(true, None, None).unwrap(), Tie::Tied);
    }

    #[test]
    fn a_refused_death_signal_leaves_the_engine_untied_but_checked() {
        let named = Some(OsStr::new("4242"));
        assert_eq!(
            check_parent(false, named, Some(4242)).unwrap(),
            Tie::Unsupported
        );
        assert_eq!(
            check_parent(false, None, Some(1)).unwrap(),
            Tie::Unsupported
        );
        assert_eq!(check_parent(false, named, Some(1)).unwrap(), Tie::Orphaned);
    }

    #[test]
    fn a_named_recorder_that_isnt_a_process_id_is_refused() {
        for named in ["", "abc", "0", "-5", "42 ", "99999999999"] {
            let err = check_parent(true, Some(OsStr::new(named)), Some(42)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{named:?}");
            assert!(err.to_string().contains(RECORDER_PID_VAR), "{err}");
        }
    }

    /// The death signal is set, on the calling thread's task: the one the
    /// kernel checks when the parent dies.
    #[test]
    fn tying_sets_the_death_signal() {
        use rustix::process::{Signal, parent_process_death_signal};

        // On a thread of its own, so the test runner's threads are left
        // as they were.
        std::thread::spawn(|| {
            assert_eq!(parent_process_death_signal().unwrap(), None);
            // The test runner names no recorder, so it's tied to whatever
            // started it.
            assert_eq!(tie_to_recorder().unwrap(), Tie::Tied);
            assert_eq!(parent_process_death_signal().unwrap(), Some(Signal::KILL));
        })
        .join()
        .unwrap();
    }
}
