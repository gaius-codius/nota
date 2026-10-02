//! nota's recorder.
//!
//! - [`fs`]: the filesystem layer every durable write goes through, with a
//!   fake that simulates crashes for exhaustive crash tests.
//! - [`journal`]: the framed PCM journal, the crash-safe first stop for every
//!   captured sample.
//! - [`engine`]: supervision of the speech engine child: restarts, timeouts,
//!   and resuming from the last confirmed sample.
//!
//! The `nota-fake-engine` binary is a test double for the engine, for the
//! supervisor's tests; it isn't part of nota.

pub mod engine;
pub mod fs;
pub mod journal;

#[cfg(test)]
mod test_dir;
