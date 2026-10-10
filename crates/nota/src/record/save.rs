//! The saver thread: stores the live text, the marks and notes, and the
//! timeline's events, as they come.
//!
//! Whatever hands it something only puts it on a channel, so recording,
//! the screen and the engine never wait on the library database. The
//! saver writes each item through the library's one writer, in the order
//! given. When a write fails, the item waits with the ones after it and is
//! tried again (every [`RETRY_EVERY`], and as more come), the screen is
//! warned ([`Cause::LibraryUnavailable`]), and the warning is cleared once
//! a write goes through. The warning is kept on the session's timeline
//! like every other change: it waits in the same line, so once the
//! database takes writes again it's stored, raised and then cleared. Each
//! try takes everything waiting, so a store that fails slowly costs one
//! try, not one per item. What's still unsaved
//! when the recording stops is tried once more, then counted as lost: the
//! audio has it, and the final pass can rebuild the text from it.
//!
//! The Recording screen shows the warning (`⚠ library offline`); the
//! summary says what wasn't saved. [`to_screen`] is how whatever else
//! sends the screen a change also hands it to the saver for the timeline.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nota_core::recorder::{Cause, Event as RecorderEvent, Warning, WarningState};
use nota_core::{Clock, SessionTime, Utterance, Word};
use nota_store::{Annotation, StoreError, TimelineEvent};
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
    /// A change to the recording, for the session's timeline.
    Event(TimelineEvent),
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
    /// Timeline events stored.
    pub(super) events: usize,
    /// Timeline events never stored.
    pub(super) lost_events: usize,
    /// The last failure, if a write failed.
    pub(super) error: Option<String>,
}

impl Saved {
    /// Counts `item` as stored.
    fn stored(&mut self, item: &ToSave) {
        match item {
            ToSave::Heard(..) => self.text += 1,
            ToSave::Annotation(_) => self.annotations += 1,
            ToSave::Event(_) => self.events += 1,
        }
    }

    /// Counts `item` as never stored.
    fn lost(&mut self, item: &ToSave) {
        match item {
            ToSave::Heard(..) => self.lost_text += 1,
            ToSave::Annotation(_) => self.lost_annotations += 1,
            ToSave::Event(_) => self.lost_events += 1,
        }
    }
}

/// Sends `event` to the screen, and, if it's a change the timeline keeps,
/// to the saver (`save`) first, so it's queued for storing whatever the
/// screen does with it. `now` stands in for the time of an event that
/// carries none. Either channel may be closed: the screen may have gone,
/// and a saver that has stopped has said why in its report.
pub(super) fn to_screen(
    ui: &Sender<Event>,
    save: &Sender<ToSave>,
    event: RecorderEvent,
    now: SessionTime,
) {
    if let Some(change) = TimelineEvent::of(&event, now) {
        let _ = save.send(ToSave::Event(change));
    }
    let _ = ui.send(Event::Recorder(event));
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
            self.saved.lost(&item);
        }
    }

    fn queue(&mut self, item: ToSave) {
        if self.pending.len() >= MAX_PENDING
            && let Some(oldest) = self.pending.pop_front()
        {
            self.saved.lost(&oldest);
        }
        self.pending.push_back(item);
    }

    /// Writes what's waiting, in order, until a write fails.
    fn write_pending(&mut self) {
        while let Some(item) = self.pending.front() {
            match (self.write)(item) {
                Ok(()) => {
                    self.saved.stored(item);
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

    /// Warns the screen, and queues the change for the timeline behind
    /// what's waiting, so it's stored once the database takes writes
    /// again. It's queued past [`MAX_PENDING`] rather than through the
    /// bound, so a clear can't push out text: there's at most one more
    /// for each time a write succeeded after a failure.
    fn warn(&mut self, state: WarningState) {
        self.warned = state == WarningState::Raised;
        let at = self.clock.now();
        let event = RecorderEvent::Warning(Warning {
            cause: Cause::LibraryUnavailable,
            track: None,
            at,
            state,
        });
        if let Some(change) = TimelineEvent::of(&event, at) {
            self.pending.push_back(ToSave::Event(change));
        }
        // The screen may have closed; the summary still says.
        let _ = self.ui.send(Event::Recorder(event));
    }
}

#[cfg(test)]
mod tests;
