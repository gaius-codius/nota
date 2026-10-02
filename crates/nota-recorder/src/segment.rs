//! FLAC segments: turning finished journals into published files and rows,
//! without ever losing the only copy of the audio.
//!
//! # Publishing
//!
//! A segment is one track's continuous run of samples within one epoch and
//! one segment window ([`SegmentLength`]). It's published in a fixed order,
//! and the types make the order the only one that compiles (see `publish`):
//!
//! 1. encode to FLAC and write `seg-….flac.tmp`;
//! 2. fsync it;
//! 3. rename it to `seg-….flac`;
//! 4. fsync the directory;
//! 5. commit the segment's row (sample range, epoch, SHA-256) to the store;
//! 6. only then delete the journals whose audio it holds.
//!
//! So a row never exists without its file, and a journal is never deleted
//! before the rows for all of its audio. A crash at any step leaves every
//! sample in a journal, a published segment, or both.
//!
//! # Which segments, from which journals
//!
//! [`publish_journals`] reads journals back from disk, plans the segments
//! that aren't yet in the store, and publishes them. Live recording calls it
//! for the journals the [`SessionWriter`] has finished; salvage calls it for
//! every journal left in a session's directory. The rules:
//! - Committed rows claim their samples first, but only a row whose file is
//!   in the session directory and matches it (its SHA-256, and the number of
//!   samples its FLAC header declares). A row whose file is missing claims
//!   nothing; so does one whose file doesn't match, and nothing is
//!   published over its samples, so its file stays and so do the journals
//!   holding them. Then journals claim theirs in
//!   descending [`JournalId`] order: after a crash a broken journal's
//!   unsynced tail can survive alongside its replacement, and the
//!   replacement, started later, wins.
//! - What's left is split at window boundaries and grouped by epoch; each
//!   continuous run in a group is one segment.
//! - A journal is deleted once every sample it holds is claimed by
//!   committed rows.
//!
//! Publishing and salvage take a [`SessionStore`], which binds a session's
//! directory to the store holding its rows, so neither can be given a store
//! and a directory from different sessions.
//!
//! [`SessionWriter`]: crate::session::SessionWriter
//! [`SessionStore`]: crate::session::SessionStore
//! [`JournalId`]: crate::journal::JournalId

mod flac;
mod plan;
mod publish;
mod salvage;
mod store;

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use nota_core::{SampleIndex, SampleRange, SampleRate, TrackId};

pub use flac::FlacError;
pub use publish::DurableSegment;
pub use salvage::{PublishError, Published, needs_salvage, publish_journals, salvage};
#[cfg(any(test, feature = "fake-fs"))]
pub use store::FakeStore;
pub use store::SegmentStore;

/// How long a segment window is, in samples. Window `k` of a track holds
/// its samples from `k * length` up to `(k + 1) * length`. A journal never
/// crosses a window boundary, so this also bounds every journal's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentLength(NonZeroU64);

impl SegmentLength {
    /// The default: five minutes, about 10 MB of journal per track at
    /// 16 kHz, and what salvage holds in memory at most per segment.
    pub const DEFAULT_SECONDS: u64 = 300;

    /// A window of `samples` samples; `None` for zero.
    #[must_use]
    pub const fn new(samples: u64) -> Option<Self> {
        match NonZeroU64::new(samples) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The default length at `rate`.
    #[must_use]
    pub fn default_at(rate: SampleRate) -> Self {
        // A u32 rate times 300 can't overflow a u64, and neither is zero.
        let samples = u64::from(rate.hz()).saturating_mul(Self::DEFAULT_SECONDS);
        Self(NonZeroU64::new(samples).unwrap_or(NonZeroU64::MIN))
    }

    /// Samples per window.
    #[must_use]
    pub const fn samples(self) -> u64 {
        self.0.get()
    }

    /// The window `sample` falls in.
    #[must_use]
    pub const fn window_of(self, sample: SampleIndex) -> u64 {
        sample.get() / self.0.get()
    }

    /// The first sample after the window `sample` falls in, or `None` if
    /// that window runs to the end of the sample numbers.
    #[must_use]
    pub fn window_end(self, sample: SampleIndex) -> Option<SampleIndex> {
        let next = self.window_of(sample).checked_add(1)?;
        next.checked_mul(self.0.get()).map(SampleIndex::new)
    }
}

/// A published segment's file name in the session directory:
/// `seg-t0-000004800000.flac`, from its track and first sample. Segments of
/// one track never overlap, so the name is unique.
#[must_use]
pub fn segment_file_name(track: TrackId, range: SampleRange) -> String {
    format!("seg-t{}-{:012}.flac", track.get(), range.start().get())
}

/// The temp name a segment is written under before its rename.
fn temp_path(dir: &Path, track: TrackId, range: SampleRange) -> PathBuf {
    dir.join(format!("{}.tmp", segment_file_name(track, range)))
}

/// Whether `path` is a segment temp file: never the only copy of anything
/// (its journals are kept until after the rename), so salvage removes it.
fn is_temp_segment(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("seg-") && n.ends_with(".flac.tmp"))
}

#[cfg(test)]
mod tests;
