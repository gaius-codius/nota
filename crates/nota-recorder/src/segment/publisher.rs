//! Publishing finished journals on a thread of its own, so recording never
//! waits for an encode.

use std::fmt;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use nota_store::SegmentRow;

use super::{PublishError, SegmentLength, SegmentStore, publish_journals};
use crate::fs::Fs;
use crate::session::{FinishedJournal, SessionStore};

/// A thread that publishes each batch of finished journals sent to it, in
/// order, as [`publish_journals`] does.
///
/// A journal still on disk after a publish run (the run failed, the
/// journal couldn't be read, or a committed segment that doesn't match its
/// file holds it back) is kept and tried again with the next batch, and
/// once more when the publisher finishes; what still fails is left on disk for
/// the next start's salvage, which loses nothing (see the
/// [`segment`](super) module). Recording never waits for it: sending a
/// batch only puts it on a channel.
#[derive(Debug)]
pub struct Publisher {
    queue: PublishQueue,
    thread: JoinHandle<PublishReport>,
}

/// Where finished journals are sent to be published. Clone it for each
/// thread that finishes journals.
#[derive(Debug, Clone)]
pub struct PublishQueue {
    batches: Sender<Vec<FinishedJournal>>,
}

impl PublishQueue {
    /// Queues `journals` to be published, after every batch sent before.
    /// Never blocks. Returns `false` if the publisher has stopped (its
    /// thread panicked): the journals stay on disk for salvage.
    #[must_use = "a refused batch stays on disk for salvage; say so"]
    pub fn send(&self, journals: Vec<FinishedJournal>) -> bool {
        journals.is_empty() || self.batches.send(journals).is_ok()
    }
}

/// What a [`Publisher`] did, once it finished.
#[derive(Debug, Default)]
pub struct PublishReport {
    rows: Vec<SegmentRow>,
    errors: Vec<PublishError>,
    left: Vec<FinishedJournal>,
}

impl PublishReport {
    /// Every row in the session's store once publishing finished, in
    /// order: those committed by runs that went on to fail too. If the
    /// store couldn't be read then, the rows the successful runs reported.
    #[must_use]
    pub fn rows(&self) -> &[SegmentRow] {
        &self.rows
    }

    /// Every error a publish run stopped at, in order, including those a
    /// later run got past.
    #[must_use]
    pub fn errors(&self) -> &[PublishError] {
        &self.errors
    }

    /// The journals still on disk after the last try: its run failed,
    /// they couldn't be read (see
    /// [`Published::unread`](super::Published::unread)), or a finding
    /// holds their audio back (see
    /// [`Published::findings`](super::Published::findings)). They're left
    /// for salvage at the next start.
    #[must_use]
    pub fn left(&self) -> &[FinishedJournal] {
        &self.left
    }

    /// Whether every journal sent was published: nothing is left for
    /// salvage.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.left.is_empty()
    }
}

/// The publisher's thread panicked, so what it did isn't known. Whatever
/// it didn't publish is still on disk, for salvage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublisherPanicked;

impl fmt::Display for PublisherPanicked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the segment publisher stopped unexpectedly")
    }
}

impl std::error::Error for PublisherPanicked {}

impl Publisher {
    /// Starts publishing into `session`, in windows of `length` (what the
    /// session records with).
    ///
    /// # Errors
    ///
    /// If the thread can't be spawned.
    pub fn spawn<S, T>(session: SessionStore<S, T>, length: SegmentLength) -> io::Result<Self>
    where
        S: Fs + Send + 'static,
        S::Lock: Send + Sync,
        T: SegmentStore + Send + 'static,
    {
        let (batches, received) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("nota-publisher".into())
            .spawn(move || publish_all(session, length, &received))?;
        Ok(Self {
            queue: PublishQueue { batches },
            thread,
        })
    }

    /// A queue to send finished journals on.
    #[must_use]
    pub fn queue(&self) -> PublishQueue {
        self.queue.clone()
    }

    /// Waits until every queue is dropped and every batch sent is
    /// published, then reports. Drop every [`PublishQueue`] first, or this
    /// waits for them.
    ///
    /// # Errors
    ///
    /// [`PublisherPanicked`] if the thread panicked.
    pub fn finish(self) -> Result<PublishReport, PublisherPanicked> {
        drop(self.queue);
        self.thread.join().map_err(|_| PublisherPanicked)
    }
}

/// The publisher's thread: publishes each batch, with any that failed
/// before, until every queue is gone; then tries what failed once more.
fn publish_all<S: Fs, T: SegmentStore>(
    mut session: SessionStore<S, T>,
    length: SegmentLength,
    batches: &Receiver<Vec<FinishedJournal>>,
) -> PublishReport {
    let mut report = PublishReport::default();
    let mut pending = Vec::new();
    for batch in batches {
        pending.extend(batch);
        publish_pending(&mut session, length, &mut pending, &mut report);
    }
    if !pending.is_empty() {
        publish_pending(&mut session, length, &mut pending, &mut report);
    }
    report.left = pending;
    if let Ok(rows) = session.parts().1.rows() {
        report.rows = rows;
    }
    report
}

/// One publish run over `pending`; afterwards only the journals still on
/// disk are kept, to try again. If the directory can't be listed, all are.
fn publish_pending<S: Fs, T: SegmentStore>(
    session: &mut SessionStore<S, T>,
    length: SegmentLength,
    pending: &mut Vec<FinishedJournal>,
    report: &mut PublishReport,
) {
    match publish_journals(session, length, pending) {
        Ok(published) => report.rows.extend_from_slice(published.segments()),
        Err(error) => report.errors.push(error),
    }
    let dir = session.session();
    if let Ok(there) = dir.fs().list(dir.dir()) {
        pending.retain(|journal| there.contains(&dir.dir().join(journal.id().file_name())));
    }
}

#[cfg(test)]
mod tests;
