//! The framed PCM journal: the crash-safe first stop for every captured
//! sample.
//!
//! Audio is appended as fixed-layout frames, each with a CRC, and the file
//! is fsync'd about every second ([`JournalWriter`]). After a crash,
//! [`read_journal`] recovers every frame up to the first one that is torn or
//! corrupt, so at most the last second or so is lost and nothing is misread.
//! The layout is in [`format`].

pub mod format;
mod writer;

pub use format::{Frame, Invalid, JournalHeader, JournalRead, ReadEnd, read_journal};
pub use writer::{DurablePosition, JournalError, JournalWriter, SYNC_INTERVAL};

#[cfg(test)]
mod reader_props;
#[cfg(test)]
mod tests;
