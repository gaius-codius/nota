//! The framed PCM journal: the crash-safe first stop for every captured
//! sample.
//!
//! Each journal file holds one track's audio from one epoch, named by its
//! [`JournalId`]. Audio is appended as fixed-layout frames, each with a CRC,
//! and the file is fsync'd every [`SYNC_INTERVAL`] (850 ms). After a crash,
//! [`read_journal`] recovers every frame up to the first one that is torn
//! or corrupt, so at most the unsynced tail is lost and nothing is
//! misread. The layout is in [`format`](mod@format).

pub mod format;
mod id;
mod writer;

pub use format::{Frame, Invalid, JournalHeader, JournalRead, ReadEnd, read_journal};
pub use id::JournalId;
pub(crate) use writer::sync_budget;
pub use writer::{DurablePosition, JournalError, JournalWriter, SYNC_INTERVAL};

#[cfg(test)]
mod reader_props;
#[cfg(test)]
mod tests;
