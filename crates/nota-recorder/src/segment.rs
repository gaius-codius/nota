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
//!   in the session directory, can be read, and matches it (its SHA-256, and
//!   the number of samples its FLAC header declares). A row whose file is
//!   missing, can't be read or doesn't match claims nothing, and nothing is
//!   published over its samples, so its file, if any, stays and so do the
//!   journals holding them; every other segment is published. Such rows are
//!   findings, kept in the session directory for the app to show (see
//!   `findings`). Then journals claim theirs in descending [`JournalId`]
//!   order: after a crash a broken journal's unsynced tail can survive
//!   alongside its replacement, and the replacement, started later, wins.
//! - What's left is split at window boundaries and grouped by epoch; each
//!   continuous run in a group is one segment.
//! - A journal is deleted once every sample it holds is claimed by
//!   committed rows.
//! - A journal that's there but can't be read (`EIO`, `EACCES`, a directory
//!   under its name) is left as it is and reported, and the rest is planned
//!   without it. What it holds isn't known, so a window it shares may be
//!   published without its samples, and where it overlaps an older journal
//!   the older one's copy is published; a later run that reads it publishes
//!   the rest as further segments of that window. Both copies of an overlap
//!   hold the same samples (the replacement replays them), so nothing is
//!   lost either way.
//! - A name a run can't use (a directory or an immutable file under a
//!   segment's temp or own name, a journal that can't be unlinked, a
//!   journal's aside name already taken) is left as it is and reported in
//!   [`Published`]; it holds up only what needs that name, and the run goes
//!   on.
//!
//! Publishing and salvage take a [`SessionStore`], which binds a session's
//! directory to the store holding its rows, so neither can be given a store
//! and a directory from different sessions.
//!
//! [`SessionWriter`]: crate::session::SessionWriter
//! [`SessionStore`]: crate::session::SessionStore
//! [`JournalId`]: crate::journal::JournalId

mod findings;
mod flac;
mod plan;
mod publish;
mod publisher;
mod salvage;
mod store;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use nota_core::{SampleCount, SampleIndex, SampleRange, SampleRate, TrackId};

use crate::fs::Fs;

pub use findings::{
    FILE_NAME as FINDINGS_FILE_NAME, Finding, Findings, FindingsError, Problem, ReadFailure,
    Status, Verification, read_findings,
};
pub use flac::FlacError;
pub use publish::DurableSegment;
pub use publisher::{PublishQueue, PublishReport, Publisher, PublisherPanicked, Stopped};
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
    pub const fn new(samples: SampleCount) -> Option<Self> {
        match NonZeroU64::new(samples.get()) {
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
    pub const fn samples(self) -> SampleCount {
        SampleCount::new(self.0.get())
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
    name_starting_at(track, range.start())
}

/// The file name of `track`'s segment starting at `start`.
fn name_starting_at(track: TrackId, start: SampleIndex) -> String {
    format!("seg-t{}-{:012}.flac", track.get(), start.get())
}

/// The track and first sample in a published segment's file name, if
/// `name` is exactly what [`segment_file_name`] makes for some segment.
fn segment_in_file_name(name: &OsStr) -> Option<(TrackId, SampleIndex)> {
    let rest = name
        .to_str()?
        .strip_prefix("seg-t")?
        .strip_suffix(".flac")?;
    let (track, start) = rest.split_once('-')?;
    let track = TrackId::new(track.parse().ok()?);
    let start = SampleIndex::new(start.parse().ok()?);
    // One spelling per segment, as journal names.
    (name.to_str()? == name_starting_at(track, start)).then_some((track, start))
}

/// For each track with segments published in `dir`, whose listing is
/// `paths`, the first sample after all of them: where a resumed track may
/// start without landing inside one. Segments of a track never overlap, so
/// only each track's newest file is read. If it can't be read, or its
/// length can't, the end of its window under `length` stands in: no
/// segment crosses a window boundary.
pub(crate) fn published_ends<S: Fs>(
    fs: &S,
    dir: &Path,
    paths: &[PathBuf],
    length: SegmentLength,
) -> BTreeMap<TrackId, SampleIndex> {
    let mut newest: BTreeMap<TrackId, SampleIndex> = BTreeMap::new();
    for path in paths {
        if let Some((track, start)) = path.file_name().and_then(segment_in_file_name) {
            let latest = newest.entry(track).or_insert(start);
            *latest = (*latest).max(start);
        }
    }
    let mut ends = BTreeMap::new();
    for (track, start) in newest {
        let end = fs
            .read(&dir.join(name_starting_at(track, start)))
            .ok()
            .and_then(|bytes| flac::stream_len(&bytes))
            .and_then(|len| start.checked_add(SampleCount::new(len)))
            .or_else(|| length.window_end(start))
            .unwrap_or(SampleIndex::new(u64::MAX));
        ends.insert(track, end);
    }
    ends
}

/// The temp name a segment is written under before its rename.
fn temp_path(dir: &Path, track: TrackId, range: SampleRange) -> PathBuf {
    dir.join(format!("{}.tmp", segment_file_name(track, range)))
}

/// Whether `path` is a segment temp file: exactly the name [`temp_path`]
/// gives some segment, never the only copy of anything (its journals are
/// kept until after the rename), so salvage removes it. Other names that
/// only look like one are left alone: nota never wrote them.
fn is_temp_segment(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".tmp"))
        .is_some_and(|n| segment_in_file_name(OsStr::new(n)).is_some())
}

#[cfg(test)]
mod tests;
