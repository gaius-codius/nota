//! The terminal while the screen is up: raw mode, the alternate screen,
//! bracketed paste and a hidden cursor, all undone on every way out.
//!
//! [`Screen::enter`] sets the terminal up and returns a guard that restores
//! it when dropped, so an error, an early return or a panic's unwinding all
//! restore it. A panic hook restores it too, before the panic message is
//! printed, so the message isn't lost on the alternate screen. After a
//! hangup the terminal is gone and restoring it fails; that's ignored.

use std::io::{self, Stdout, Write};
use std::mem::ManuallyDrop;
use std::sync::{Arc, Once};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use nota_core::Clock;

use crate::latency::{DrawEnds, Watched};

/// The screen's output: the terminal, watched for draws only when
/// measuring latency.
type Output = CrosstermBackend<Watched<Stdout>>;

/// The terminal set up for the screen. Restored when dropped.
#[derive(Debug)]
pub(crate) struct Screen {
    /// Never dropped. Its drop shows the cursor and, if that fails, prints
    /// the error with `eprintln!`, which panics when stderr is the terminal
    /// that has just hung up. [`restore`] shows the cursor instead, and the
    /// buffers are freed at exit (there's one screen per process).
    terminal: ManuallyDrop<Terminal<Output>>,
}

impl Screen {
    /// Sets the terminal up for the screen. With `draws`, the end of each
    /// draw is noted there, by its clock (for the latency log).
    ///
    /// # Errors
    ///
    /// If the terminal can't be set up; whatever was set up is undone.
    pub(crate) fn enter(draws: Option<(DrawEnds, Arc<dyn Clock>)>) -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        let set_up = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )
        .and_then(|()| Terminal::new(CrosstermBackend::new(Watched::new(io::stdout(), draws))));
        match set_up {
            Ok(terminal) => Ok(Self {
                terminal: ManuallyDrop::new(terminal),
            }),
            Err(e) => {
                restore();
                Err(e)
            }
        }
    }

    /// The terminal to draw on.
    pub(crate) fn terminal(&mut self) -> &mut Terminal<Output> {
        &mut self.terminal
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}

/// Undoes everything [`Screen::enter`] did, as far as it can. Safe to call
/// more than once, and with the terminal gone.
fn restore() {
    let mut out = io::stdout();
    let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen, Show);
    let _ = out.flush();
    let _ = disable_raw_mode();
}

/// Restores the terminal before a panic's message is printed, once per
/// process.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}
