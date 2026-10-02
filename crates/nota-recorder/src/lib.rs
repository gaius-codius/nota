//! nota's recorder.
//!
//! - [`fs`]: the filesystem layer every durable write goes through, with a
//!   fake that simulates crashes for exhaustive crash tests.
//! - [`journal`]: the framed PCM journal, the crash-safe first stop for every
//!   captured sample.

pub mod fs;
pub mod journal;
