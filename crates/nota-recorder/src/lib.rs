//! nota's recorder.
//!
//! - [`capture`]: audio capture (`PipeWire` on Linux) and the recorder loop
//!   that journals it.
//! - [`fs`]: the filesystem layer every durable write goes through, with a
//!   fake that simulates crashes for exhaustive crash tests.
//! - [`journal`]: the framed PCM journal, the crash-safe first stop for every
//!   captured sample.
//! - [`engine`]: supervision of the speech engine child: restarts, timeouts,
//!   and resuming from the last confirmed sample.
//! - [`session`]: a session's tracks recorded into journals that rotate at
//!   every segment boundary.
//! - [`segment`]: finished journals published as FLAC segments with their
//!   rows, and salvage after a crash.
//!
//! # Native libraries
//!
//! On Linux, capture goes through cpal, which links `libpipewire-0.3` and
//! `libasound` dynamically. Building needs their headers, `pkg-config` and
//! libclang (for bindgen); [`capture`] lists the packages. Elsewhere the
//! crate builds without them and records nothing yet. `SQLite` comes bundled
//! through `nota-store`.
//!
//! The `nota-fake-engine` binary is a test double for the engine, for the
//! supervisor's tests; it isn't part of nota.

pub mod capture;
pub mod engine;
pub mod fs;
pub mod journal;
pub mod segment;
pub mod session;

#[cfg(test)]
mod test_dir;
