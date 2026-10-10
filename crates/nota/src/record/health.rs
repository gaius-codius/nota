//! Whether the library database takes writes, as the two threads that
//! write to it during a recording find: the saver (live text, marks, notes
//! and the timeline) and the publisher (segment rows).
//!
//! The screen has one warning for it ([`Cause::LibraryUnavailable`]),
//! however many writers fail: it's raised when the first of them fails and
//! cleared only once none is failing. Each writer notes how its last write
//! went ([`LibraryHealth::note`]), and whichever changes the whole sends
//! the warning, while the state is held, so a raise and a clear from the
//! two threads can't reach the screen in the wrong order.
//!
//! [`ReportingStore`] is the publisher's side: the store its segment rows go
//! through, noting each call's outcome. Nothing about publishing changes:
//! a row that fails is tried again as before, and its audio stays on disk.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};

use nota_core::recorder::{Cause, Event as RecorderEvent, Warning, WarningState};
use nota_core::{Clock, SessionId};
use nota_recorder::segment::{DurableSegment, SegmentStore};
use nota_store::{RowKey, SegmentRow};
use nota_tui::Event;

use super::save::{ToSave, to_screen};

/// A thread that writes to the library database during a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Writer {
    /// The saver: live text, marks, notes and the timeline.
    Saver,
    /// The publisher: segment rows.
    Publisher,
}

/// Which writers' last write failed, shared by them.
#[derive(Debug, Clone, Default)]
pub(super) struct LibraryHealth {
    failing: Arc<Mutex<Failing>>,
}

/// The writers whose last write failed.
#[derive(Debug, Default)]
struct Failing {
    saver: bool,
    publisher: bool,
}

impl Failing {
    fn any(&self) -> bool {
        self.saver || self.publisher
    }

    fn of(&mut self, writer: Writer) -> &mut bool {
        match writer {
            Writer::Saver => &mut self.saver,
            Writer::Publisher => &mut self.publisher,
        }
    }
}

impl LibraryHealth {
    /// Notes whether `writer`'s last write `failed`. If that changes
    /// whether any writer is failing, calls `warn` with the warning's new
    /// state, before another writer can note anything.
    pub(super) fn note(&self, writer: Writer, failed: bool, warn: impl FnOnce(WarningState)) {
        // Nothing panics while holding it, so a poisoned state is still
        // the last one noted.
        let mut failing = self.failing.lock().unwrap_or_else(PoisonError::into_inner);
        let was = failing.any();
        *failing.of(writer) = failed;
        match (was, failing.any()) {
            (false, true) => warn(WarningState::Raised),
            (true, false) => warn(WarningState::Cleared),
            _ => {}
        }
    }
}

/// The library's warning in `state`, at `clock`'s now.
pub(super) fn library_warning(state: WarningState, clock: &dyn Clock) -> RecorderEvent {
    RecorderEvent::Warning(Warning {
        cause: Cause::LibraryUnavailable,
        track: None,
        at: clock.now(),
        state,
    })
}

/// The publisher's segment store, `inner`, noting how each call went in
/// `health` and, when that changes the library's warning, telling the
/// screen and the timeline.
pub(super) struct ReportingStore<T> {
    inner: T,
    health: LibraryHealth,
    ui: Sender<Event>,
    save: Sender<ToSave>,
    clock: Arc<dyn Clock>,
}

impl<T> ReportingStore<T> {
    /// `inner`, reporting to `ui` and the saver (`save`) through `health`,
    /// timed by `clock`.
    pub(super) const fn new(
        inner: T,
        health: LibraryHealth,
        ui: Sender<Event>,
        save: Sender<ToSave>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner,
            health,
            ui,
            save,
            clock,
        }
    }

    /// Notes how a call went, and passes its result on. A failure raises
    /// the warning; a success clears it only if it `commits` a row: a read
    /// that works says nothing of writes, and a publish run reads before
    /// it inserts, so a read clearing it would clear it every run while
    /// inserts still fail. A stored row that doesn't parse came from a
    /// database that answered: that's a damaged row for publishing to
    /// name, not the library being down.
    fn noted<R>(&self, result: Result<R, T::Error>, commits: bool) -> Result<R, T::Error>
    where
        T: SegmentStore,
    {
        let failed = result
            .as_ref()
            .is_err_and(|error| T::unparsable_row(error).is_none());
        if failed || commits {
            self.health.note(Writer::Publisher, failed, |state| {
                let warning = library_warning(state, self.clock.as_ref());
                to_screen(&self.ui, &self.save, warning, self.clock.now());
            });
        }
        result
    }
}

impl<T: SegmentStore> SegmentStore for ReportingStore<T> {
    type Error = T::Error;

    fn rows(&mut self, session: SessionId) -> Result<Vec<SegmentRow>, Self::Error> {
        let rows = self.inner.rows(session);
        self.noted(rows, false)
    }

    fn insert(&mut self, session: SessionId, segment: &DurableSegment) -> Result<(), Self::Error> {
        let inserted = self.inner.insert(session, segment);
        self.noted(inserted, true)
    }

    fn is_disk_full(error: &Self::Error) -> bool {
        T::is_disk_full(error)
    }

    fn unparsable_row(error: &Self::Error) -> Option<RowKey> {
        T::unparsable_row(error)
    }
}

#[cfg(test)]
mod tests;
