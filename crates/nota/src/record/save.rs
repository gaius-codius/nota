//! The saver thread: stores the live text, and the marks and notes, as
//! they come.
//!
//! Whatever hands it something only puts it on a channel, so recording,
//! the screen and the engine never wait on the library database. The
//! saver writes each item through the library's one writer, in the order
//! given. When a write fails, the item waits with the ones after it and is
//! tried again (every [`RETRY_EVERY`], and as more come), the screen is
//! warned ([`Cause::LibraryUnavailable`]), and the warning is cleared once
//! a write goes through. Each try takes everything waiting, so a store
//! that fails slowly costs one try, not one per item. What's still unsaved
//! when the recording stops is tried once more, then counted as lost: the
//! audio has it, and the final pass can rebuild the text from it.
//!
//! The screen doesn't show warnings yet (M2's warnings work, GAI-320,
//! does); the summary says what wasn't saved.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::recorder::{Cause, Event as RecorderEvent, Warning, WarningState};
use nota_core::{Clock, Utterance, Word};
use nota_store::{Annotation, StoreError};
use nota_tui::Event;

/// How often unsaved items are tried again while nothing new comes.
const RETRY_EVERY: Duration = Duration::from_secs(1);

/// The most items kept waiting for the database; past it, the oldest are
/// dropped and counted as lost. Hours of speech, so only a database that
/// stays down for most of a long recording loses any.
const MAX_PENDING: usize = 10_000;

/// Something to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ToSave {
    /// Text the engine heard, and its words, placed in session time.
    Heard(Utterance, Vec<Word>),
    /// A mark or a note.
    Annotation(Annotation),
}

impl ToSave {
    const fn is_text(&self) -> bool {
        matches!(self, Self::Heard(..))
    }
}

/// What the saver stored, and what it couldn't.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Saved {
    /// Utterances stored.
    pub(super) text: usize,
    /// Marks and notes stored.
    pub(super) annotations: usize,
    /// Utterances never stored.
    pub(super) lost_text: usize,
    /// Marks and notes never stored.
    pub(super) lost_annotations: usize,
    /// The last failure, if a write failed.
    pub(super) error: Option<String>,
}

/// The saver thread, and the channel to it.
pub(super) struct Saver {
    sender: Sender<ToSave>,
    thread: JoinHandle<Saved>,
}

impl Saver {
    /// Starts the saver, storing each item with `write`, and warning on
    /// `ui` while writes fail, at times read from `clock`.
    ///
    /// # Errors
    ///
    /// If the thread can't be started.
    pub(super) fn spawn(
        write: impl FnMut(&ToSave) -> Result<(), StoreError> + Send + 'static,
        ui: Sender<Event>,
        clock: Arc<dyn Clock>,
    ) -> io::Result<Self> {
        let (sender, inputs) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("nota-saver".into())
            .spawn(move || {
                let mut saving = Saving {
                    write,
                    ui,
                    clock,
                    pending: VecDeque::new(),
                    warned: false,
                    saved: Saved::default(),
                };
                saving.run(&inputs);
                saving.saved
            })?;
        Ok(Self { sender, thread })
    }

    /// A channel to the saver. It stops once every one is dropped and
    /// [`Saver::finish`] is called.
    pub(super) fn sender(&self) -> Sender<ToSave> {
        self.sender.clone()
    }

    /// Waits for everything given to be stored, or given up on, once every
    /// other channel to it is dropped; says what became of it.
    pub(super) fn finish(self) -> Saved {
        drop(self.sender);
        self.thread.join().unwrap_or_else(|_| Saved {
            error: Some("the saver stopped unexpectedly".to_owned()),
            ..Saved::default()
        })
    }
}

struct Saving<W> {
    write: W,
    ui: Sender<Event>,
    clock: Arc<dyn Clock>,
    pending: VecDeque<ToSave>,
    /// The screen has been warned, and not told it's cleared.
    warned: bool,
    saved: Saved,
}

impl<W: FnMut(&ToSave) -> Result<(), StoreError>> Saving<W> {
    fn run(&mut self, inputs: &Receiver<ToSave>) {
        loop {
            let received = if self.pending.is_empty() {
                match inputs.recv() {
                    Ok(item) => Some(item),
                    Err(_) => break,
                }
            } else {
                match inputs.recv_timeout(RETRY_EVERY) {
                    Ok(item) => Some(item),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };
            if let Some(item) = received {
                self.queue(item);
            }
            // Everything waiting, so a store that's slow to fail (a lock
            // held elsewhere waits out the busy timeout) costs one try for
            // all of it, not one each.
            for item in inputs.try_iter() {
                self.queue(item);
            }
            self.write_pending();
        }
        // One last try, then what's left is lost.
        self.write_pending();
        for item in self.pending.drain(..) {
            lose(&mut self.saved, &item);
        }
    }

    fn queue(&mut self, item: ToSave) {
        if self.pending.len() >= MAX_PENDING
            && let Some(oldest) = self.pending.pop_front()
        {
            lose(&mut self.saved, &oldest);
        }
        self.pending.push_back(item);
    }

    /// Writes what's waiting, in order, until a write fails.
    fn write_pending(&mut self) {
        while let Some(item) = self.pending.front() {
            match (self.write)(item) {
                Ok(()) => {
                    if item.is_text() {
                        self.saved.text += 1;
                    } else {
                        self.saved.annotations += 1;
                    }
                    self.pending.pop_front();
                    if self.warned {
                        self.warn(WarningState::Cleared);
                    }
                }
                Err(e) => {
                    self.saved.error = Some(e.to_string());
                    if !self.warned {
                        self.warn(WarningState::Raised);
                    }
                    return;
                }
            }
        }
    }

    fn warn(&mut self, state: WarningState) {
        self.warned = state == WarningState::Raised;
        // The screen may have closed; the summary still says.
        let _ = self
            .ui
            .send(Event::Recorder(RecorderEvent::Warning(Warning {
                cause: Cause::LibraryUnavailable,
                track: None,
                at: self.clock.now(),
                state,
            })));
    }
}

fn lose(saved: &mut Saved, item: &ToSave) {
    if item.is_text() {
        saved.lost_text += 1;
    } else {
        saved.lost_annotations += 1;
    }
}

#[cfg(test)]
mod tests;
