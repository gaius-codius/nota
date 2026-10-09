//! The signal thread: SIGHUP, SIGTERM and SIGINT ask for a stop, SIGXCPU is
//! only noted.

use std::io;
use std::sync::mpsc::Sender;
#[cfg(unix)]
use std::thread::{self, JoinHandle};

use nota_tui::Event;

/// The handle that stops the signal thread.
#[cfg(unix)]
pub(super) struct SignalThread {
    handle: signal_hook::iterator::Handle,
    /// Returns whether a SIGXCPU arrived.
    thread: JoinHandle<bool>,
}

#[cfg(unix)]
impl SignalThread {
    /// Stops the thread, and returns whether a SIGXCPU arrived.
    pub(super) fn close(self) -> bool {
        self.handle.close();
        self.thread.join().unwrap_or(false)
    }
}

/// From now on, SIGHUP, SIGTERM and SIGINT close the screen rather than
/// end the process: the recording then stops in order (see the module
/// docs). SIGXCPU is only noted. Signals of one kind that arrive close
/// together may come through as one, so whether any arrived is all that's
/// known.
#[cfg(unix)]
pub(super) fn listen_for_signals(ui: Sender<Event>) -> io::Result<SignalThread> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGXCPU};
    let signals = signal_hook::iterator::Signals::new([SIGHUP, SIGTERM, SIGINT, SIGXCPU])?;
    let handle = signals.handle();
    let thread = thread::Builder::new()
        .name("nota-signals".into())
        .spawn(move || watch(signals, &ui))?;
    Ok(SignalThread { handle, thread })
}

/// The signal thread: until `signals` is closed, asks `ui` to close for
/// every signal but SIGXCPU. Returns whether a SIGXCPU arrived.
#[cfg(unix)]
fn watch(mut signals: signal_hook::iterator::Signals, ui: &Sender<Event>) -> bool {
    use signal_hook::consts::SIGXCPU;
    let mut overran = false;
    for signal in signals.forever() {
        if signal == SIGXCPU {
            overran = true;
        } else {
            let _ = ui.send(Event::Close);
        }
    }
    // Once closed, the iterator stops without reading what's still
    // pending: a SIGXCPU during the stop would be missed.
    overran || signals.pending().any(|signal| signal == SIGXCPU)
}

/// Elsewhere nothing records yet (see [`record`]), so there's nothing to
/// stop in order.
#[cfg(not(unix))]
pub(super) struct SignalThread;

#[cfg(not(unix))]
impl SignalThread {
    pub(super) fn close(self) -> bool {
        false
    }
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same shape as the Unix version"
)]
pub(super) fn listen_for_signals(_ui: Sender<Event>) -> io::Result<SignalThread> {
    Ok(SignalThread)
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::mpsc;

    use signal_hook::consts::{SIGHUP, SIGXCPU};
    use signal_hook::iterator::Signals;
    use signal_hook::low_level::raise;

    use super::*;

    /// A raised signal reaches every `Signals` registered for it, so tests
    /// that raise one take turns (if run as threads of one process).
    static RAISING: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A SIGXCPU that arrives during the stop, still pending when the
    /// signal thread is closed, is still noted.
    #[test]
    fn a_sigxcpu_still_pending_at_close_is_noted() {
        let _turn = RAISING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (ui, closes) = mpsc::channel();
        let signals = Signals::new([SIGHUP, SIGXCPU]).unwrap();
        // Closed first, so the iterator never reads it: only what's
        // pending is left to find it.
        signals.handle().close();
        raise(SIGXCPU).unwrap();
        assert!(watch(signals, &ui));
        assert!(closes.try_recv().is_err(), "SIGXCPU closed the screen");
    }

    #[test]
    fn nothing_pending_at_close_is_no_overrun() {
        let _turn = RAISING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (ui, _closes) = mpsc::channel();
        let signals = Signals::new([SIGHUP, SIGXCPU]).unwrap();
        signals.handle().close();
        raise(SIGHUP).unwrap();
        assert!(!watch(signals, &ui));
    }
}
