//! The test stand-ins' watch on their parent: this binary's, and
//! `nota-fake-cli`'s too, which includes this file by its path.

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use nota_core::lifeline::RECORDER_PID_VAR;

/// How often a stand-in looks for its parent: well inside the second in
/// which it must end once its parent has gone.
const PARENT_LOOK: Duration = Duration::from_millis(100);

/// Ends this process soon after the process that started it has gone.
///
/// The deciders start a server or a CLI in a process group of its own, so
/// when nextest stops a hung test, or a developer kills one, the signal
/// misses the stand-in, and the test dies before the decider's Drop can
/// stop it. A process whose parent dies is given another (init, or a
/// subreaper), so a parent other than the one that started it is the sign.
/// The local decider names itself in [`RECORDER_PID_VAR`], which closes
/// the gap of a parent that dies before the stand-in first looks; started
/// without it (the cloud decider, or anything else), the parent it first
/// sees is the one it watches. The kernel's tie (the launch shim's) can't
/// do this for every test: it fires when the *thread* that started the
/// process ends, and a test may start a stand-in from a thread that ends
/// before the decider does.
///
/// # Errors
///
/// If the watch's thread can't be started: a stand-in that can't watch
/// would outlive a killed test, so it doesn't start.
pub(crate) fn end_with_parent() -> io::Result<()> {
    let named = std::env::var(RECORDER_PID_VAR)
        .ok()
        .and_then(|pid| pid.parse::<u32>().ok());
    let parent = named.unwrap_or_else(std::os::unix::process::parent_id);
    thread::Builder::new()
        .name("parent-watch".into())
        .spawn(move || {
            let (_keep, ticks) = mpsc::channel::<()>();
            while std::os::unix::process::parent_id() == parent {
                let _ = ticks.recv_timeout(PARENT_LOOK);
            }
            leave();
        })
        .map(drop)
}

/// Exits at once, from the parent watch's thread.
#[expect(
    clippy::disallowed_methods,
    reason = "a test stand-in with nothing to finish: its parent has gone, and \
              its main thread may be blocked in accept, a read or a hang forever"
)]
fn leave() {
    std::process::exit(0);
}
