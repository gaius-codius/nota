//! The signal thread: SIGHUP, SIGTERM and SIGINT ask for a stop, SIGXCPU is
//! only noted.

use std::io;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
#[cfg(unix)]
use std::sync::{Arc, Mutex, PoisonError};
#[cfg(unix)]
use std::thread::{self, JoinHandle};

#[cfg(unix)]
use nota_core::recorder;
use nota_tui::Event;

/// The handle that stops the signal thread.
#[cfg(unix)]
pub(crate) struct SignalThread {
    handle: signal_hook::iterator::Handle,
    /// Returns whether a SIGXCPU arrived. `None` once joined.
    thread: Option<JoinHandle<bool>>,
}

#[cfg(unix)]
impl SignalThread {
    /// Stops the thread, and returns whether a SIGXCPU arrived.
    pub(crate) fn close(mut self) -> bool {
        self.handle.close();
        self.thread
            .take()
            .is_some_and(|thread| thread.join().unwrap_or(false))
    }
}

/// A thread dropped without [`SignalThread::close`] (a start that failed
/// after it began) is stopped too, so failed starts don't leave threads
/// behind. Whether a SIGXCPU arrived is then lost: there's no recording.
#[cfg(unix)]
impl Drop for SignalThread {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// From now on, SIGHUP, SIGTERM and SIGINT close the screen rather than
/// end the process: the recording then stops in order (see the module
/// docs). SIGXCPU is only noted. Signals of one kind that arrive close
/// together may come through as one, so whether any arrived is all that's
/// known.
#[cfg(unix)]
pub(crate) fn listen_for_signals(ui: Sender<Event>) -> io::Result<SignalThread> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGXCPU};
    let signals = signal_hook::iterator::Signals::new([SIGHUP, SIGTERM, SIGINT, SIGXCPU])?;
    let handle = signals.handle();
    let thread = thread::Builder::new()
        .name("nota-signals".into())
        .spawn(move || watch(signals, &ui))?;
    Ok(SignalThread {
        handle,
        thread: Some(thread),
    })
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
            let _ = ui.send(Event::Recorder(recorder::Event::Stopping));
        }
    }
    // Once closed, the iterator stops without reading what's still
    // pending: a SIGXCPU during the stop would be missed.
    overran || signals.pending().any(|signal| signal == SIGXCPU)
}

/// The app's own signals, for its whole life: SIGHUP, SIGTERM and SIGINT
/// ask nota to close. Each one is noted ([`QuitSignals::asked`]) and, while
/// Home shows, closes it. A recording listens for them too (each listener
/// gets every signal), so one arriving during a recording stops it in
/// order, and the app then closes instead of showing Home again. Without
/// this, a signal between two screens would be lost: once a listener is
/// closed, signal-hook doesn't restore the default action.
#[cfg(unix)]
pub(crate) struct QuitSignals {
    handle: signal_hook::iterator::Handle,
    thread: Option<JoinHandle<()>>,
    asked: Arc<AtomicBool>,
    /// Home's events, while it shows.
    home: Arc<Mutex<Option<Sender<Event>>>>,
}

#[cfg(unix)]
impl QuitSignals {
    /// Starts listening.
    ///
    /// # Errors
    ///
    /// The signals can't be registered, or the thread can't start.
    pub(crate) fn listen() -> io::Result<Self> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        let mut signals = signal_hook::iterator::Signals::new([SIGHUP, SIGTERM, SIGINT])?;
        let handle = signals.handle();
        let asked = Arc::new(AtomicBool::new(false));
        let home: Arc<Mutex<Option<Sender<Event>>>> = Arc::new(Mutex::new(None));
        let thread = thread::Builder::new().name("nota-quit".into()).spawn({
            let (asked, home) = (Arc::clone(&asked), Arc::clone(&home));
            move || {
                for _ in signals.forever() {
                    asked.store(true, Ordering::SeqCst);
                    let home = home.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(home) = home.as_ref() {
                        let _ = home.send(Event::Recorder(recorder::Event::Stopping));
                    }
                }
            }
        })?;
        Ok(Self {
            handle,
            thread: Some(thread),
            asked,
            home,
        })
    }

    /// Whether a signal has asked nota to close.
    pub(crate) fn asked(&self) -> bool {
        self.asked.load(Ordering::SeqCst)
    }

    /// Sends signals to Home's events from now on (`None`: Home is gone).
    /// A signal that came before is in [`QuitSignals::asked`].
    pub(crate) fn show_home(&self, events: Option<Sender<Event>>) {
        *self.home.lock().unwrap_or_else(PoisonError::into_inner) = events;
    }
}

#[cfg(unix)]
impl Drop for QuitSignals {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Elsewhere there's nothing to listen to: no signal ever asks.
#[cfg(not(unix))]
pub(crate) struct QuitSignals;

#[cfg(not(unix))]
impl QuitSignals {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the same shape as the Unix version"
    )]
    pub(crate) fn listen() -> io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn asked(&self) -> bool {
        false
    }

    pub(crate) fn show_home(&self, _events: Option<Sender<Event>>) {}
}

/// Elsewhere nothing records yet (see [`record`](super::record)), so
/// there's nothing to stop in order.
#[cfg(not(unix))]
pub(crate) struct SignalThread;

#[cfg(not(unix))]
impl SignalThread {
    pub(crate) fn close(self) -> bool {
        false
    }
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same shape as the Unix version"
)]
pub(crate) fn listen_for_signals(_ui: Sender<Event>) -> io::Result<SignalThread> {
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
    static RAISING: Mutex<()> = Mutex::new(());

    /// A SIGXCPU that arrives during the stop, still pending when the
    /// signal thread is closed, is still noted.
    #[test]
    fn a_sigxcpu_still_pending_at_close_is_noted() {
        let _turn = RAISING.lock().unwrap_or_else(PoisonError::into_inner);
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
        let _turn = RAISING.lock().unwrap_or_else(PoisonError::into_inner);
        let (ui, _closes) = mpsc::channel();
        let signals = Signals::new([SIGHUP, SIGXCPU]).unwrap();
        signals.handle().close();
        raise(SIGHUP).unwrap();
        assert!(!watch(signals, &ui));
    }

    /// A thread dropped without being closed stops all the same: the
    /// sender it held goes with it.
    #[test]
    fn a_dropped_signal_thread_stops() {
        let (ui, events) = mpsc::channel();
        let thread = listen_for_signals(ui).unwrap();
        drop(thread);
        assert_eq!(
            events.recv_timeout(std::time::Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
    }
}
