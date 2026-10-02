//! Planning which segments to publish, from committed rows and journals.
//!
//! Pure: no I/O. The rules are in the parent module's docs.

use std::collections::{BTreeMap, BTreeSet};

use nota_core::{EpochId, SampleIndex, SampleRange, SampleRate, TrackId};
use nota_store::SegmentRow;

use super::SegmentLength;
use crate::journal::JournalId;

/// What planning needs to know of one journal: whose audio it holds, and
/// which samples. Read from the journal's header and valid frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JournalSummary {
    pub(super) id: JournalId,
    pub(super) track: TrackId,
    pub(super) epoch: EpochId,
    pub(super) rate: SampleRate,
    /// `None` if it has no valid frames.
    pub(super) range: Option<SampleRange>,
}

/// A segment's identity: its track and first sample.
pub(super) type SegmentKey = (TrackId, SampleIndex);

/// The samples of a segment taken from one journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Part {
    pub(super) journal: JournalId,
    pub(super) range: SampleRange,
}

/// A segment to publish: a continuous run of one track's samples in one
/// epoch (at one rate) and one window, made of parts in sample order with
/// no gaps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlannedSegment {
    pub(super) track: TrackId,
    pub(super) epoch: EpochId,
    pub(super) rate: SampleRate,
    pub(super) range: SampleRange,
    pub(super) parts: Vec<Part>,
}

impl PlannedSegment {
    pub(super) const fn key(&self) -> SegmentKey {
        (self.track, self.range.start())
    }
}

/// The plan: segments in (track, first sample) order, and for each journal
/// the segments that must be committed before it can be deleted: every new
/// segment overlapping its samples. A journal whose samples are all in
/// committed rows already (or that has none) needs nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Plan {
    pub(super) segments: Vec<PlannedSegment>,
    pub(super) needs: BTreeMap<JournalId, BTreeSet<SegmentKey>>,
}

/// Plans the segments that `rows` don't already hold, from `journals`.
pub(super) fn plan(
    rows: &[SegmentRow],
    journals: &[JournalSummary],
    length: SegmentLength,
) -> Plan {
    let mut needs: BTreeMap<JournalId, BTreeSet<SegmentKey>> =
        journals.iter().map(|j| (j.id, BTreeSet::new())).collect();
    let tracks: BTreeSet<TrackId> = journals.iter().map(|j| j.track).collect();
    let mut segments = Vec::new();
    for track in tracks {
        let mut claimed = Claimed::default();
        for row in rows.iter().filter(|r| r.track() == track) {
            claimed.add(row.range());
        }
        // Newest journal first: it wins any overlap.
        let mut mine: Vec<_> = journals.iter().filter(|j| j.track == track).collect();
        mine.sort_by_key(|j| std::cmp::Reverse(j.id));
        // Every part, grouped by epoch, rate and window, in sample order.
        // An epoch has one rate; the rate is in the key only so that headers
        // that disagree can't put two rates in one file.
        let mut groups: BTreeMap<(EpochId, u32, u64), BTreeMap<SampleIndex, Part>> =
            BTreeMap::new();
        for journal in mine {
            let Some(range) = journal.range else {
                continue;
            };
            for piece in claimed.subtract(range) {
                for (window, range) in split_at_windows(piece, length) {
                    let part = Part {
                        journal: journal.id,
                        range,
                    };
                    groups
                        .entry((journal.epoch, journal.rate.hz(), window))
                        .or_default()
                        .insert(range.start(), part);
                }
            }
            claimed.add(range);
        }
        let first = segments.len();
        for ((epoch, hz, _), parts) in groups {
            let Some(rate) = SampleRate::new(hz) else {
                continue;
            };
            for run in continuous_runs(parts.into_values()) {
                if let Some(segment) = segment_of(track, epoch, rate, run) {
                    segments.push(segment);
                }
            }
        }
        // A journal waits for every new segment its samples overlap, not
        // only those it supplies: a sample a newer journal won is still in
        // no row until that segment commits.
        for journal in journals.iter().filter(|j| j.track == track) {
            let Some(range) = journal.range else {
                continue;
            };
            let waits = needs.entry(journal.id).or_default();
            for segment in &segments[first..] {
                if segment.range.start() < range.end() && range.start() < segment.range.end() {
                    waits.insert(segment.key());
                }
            }
        }
    }
    segments.sort_by_key(PlannedSegment::key);
    Plan { segments, needs }
}

/// Splits parts, in sample order, wherever one doesn't start where the last
/// ended.
fn continuous_runs(parts: impl Iterator<Item = Part>) -> Vec<Vec<Part>> {
    let mut runs: Vec<Vec<Part>> = Vec::new();
    for part in parts {
        match runs.last_mut() {
            Some(run)
                if run
                    .last()
                    .is_some_and(|p| p.range.end() == part.range.start()) =>
            {
                run.push(part);
            }
            _ => runs.push(vec![part]),
        }
    }
    runs
}

fn segment_of(
    track: TrackId,
    epoch: EpochId,
    rate: SampleRate,
    parts: Vec<Part>,
) -> Option<PlannedSegment> {
    let range = SampleRange::new(parts.first()?.range.start(), parts.last()?.range.end())?;
    Some(PlannedSegment {
        track,
        epoch,
        rate,
        range,
        parts,
    })
}

/// Splits a non-empty `range` where it crosses window boundaries, with each
/// piece's window.
fn split_at_windows(range: SampleRange, length: SegmentLength) -> Vec<(u64, SampleRange)> {
    let mut out = Vec::new();
    let mut start = range.start();
    while start < range.end() {
        let end = length
            .window_end(start)
            .map_or(range.end(), |e| e.min(range.end()));
        if let Some(piece) = SampleRange::new(start, end) {
            out.push((length.window_of(start), piece));
        }
        start = end;
    }
    out
}

/// Sample ranges already taken, kept sorted, disjoint and merged.
#[derive(Debug, Default)]
struct Claimed(Vec<SampleRange>);

impl Claimed {
    fn add(&mut self, range: SampleRange) {
        if range.is_empty() {
            return;
        }
        let mut start = range.start();
        let mut end = range.end();
        // Absorb every range that touches or overlaps the new one.
        self.0.retain(|r| {
            let touches = r.start() <= end && start <= r.end();
            if touches {
                start = start.min(r.start());
                end = end.max(r.end());
            }
            !touches
        });
        let at = self.0.partition_point(|r| r.start() < start);
        if let Some(merged) = SampleRange::new(start, end) {
            self.0.insert(at, merged);
        }
    }

    /// The parts of `range` not yet taken, in order.
    fn subtract(&self, range: SampleRange) -> Vec<SampleRange> {
        let mut out = Vec::new();
        let mut at = range.start();
        for taken in &self.0 {
            if taken.end() <= at {
                continue;
            }
            if taken.start() >= range.end() {
                break;
            }
            if taken.start() > at
                && let Some(free) = SampleRange::new(at, taken.start())
            {
                out.push(free);
            }
            at = at.max(taken.end());
        }
        if at < range.end()
            && let Some(free) = SampleRange::new(at, range.end())
        {
            out.push(free);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use nota_store::Sha256Digest;
    use proptest::prelude::*;

    use super::*;

    fn range(start: u64, end: u64) -> SampleRange {
        SampleRange::new(SampleIndex::new(start), SampleIndex::new(end)).unwrap()
    }

    fn journal(id: u64, track: u32, epoch: u32, r: Option<(u64, u64)>) -> JournalSummary {
        JournalSummary {
            id: JournalId::new(id),
            track: TrackId::new(track),
            epoch: EpochId::new(epoch),
            rate: SampleRate::SPEECH,
            range: r.map(|(s, e)| range(s, e)),
        }
    }

    fn row(track: u32, start: u64, end: u64) -> SegmentRow {
        SegmentRow::new(
            TrackId::new(track),
            EpochId::new(0),
            range(start, end),
            Sha256Digest::new([0; 32]),
        )
        .unwrap()
    }

    fn len(n: u64) -> SegmentLength {
        SegmentLength::new(n).unwrap()
    }

    /// A segment as (track, epoch, start, end, parts as (journal, start, end)).
    type Shown = (u32, u32, u64, u64, Vec<(u64, u64, u64)>);

    fn summary(plan: &Plan) -> Vec<Shown> {
        plan.segments
            .iter()
            .map(|s| {
                (
                    s.track.get(),
                    s.epoch.get(),
                    s.range.start().get(),
                    s.range.end().get(),
                    s.parts
                        .iter()
                        .map(|p| (p.journal.get(), p.range.start().get(), p.range.end().get()))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn one_journal_per_window_gives_one_segment_each() {
        let plan = plan(
            &[],
            &[
                journal(0, 0, 0, Some((0, 100))),
                journal(2, 0, 0, Some((100, 150))),
                journal(1, 1, 0, Some((30, 100))),
            ],
            len(100),
        );
        assert_eq!(
            summary(&plan),
            [
                (0, 0, 0, 100, vec![(0, 0, 100)]),
                (0, 0, 100, 150, vec![(2, 100, 150)]),
                (1, 0, 30, 100, vec![(1, 30, 100)]),
            ]
        );
        let key = |t, s| (TrackId::new(t), SampleIndex::new(s));
        assert_eq!(plan.needs[&JournalId::new(0)], BTreeSet::from([key(0, 0)]));
        assert_eq!(
            plan.needs[&JournalId::new(2)],
            BTreeSet::from([key(0, 100)])
        );
        assert_eq!(plan.needs[&JournalId::new(1)], BTreeSet::from([key(1, 30)]));
    }

    #[test]
    fn the_newer_journal_wins_an_overlap() {
        // Journal 3 broke after 60 samples were durable; journal 4 replayed
        // from 60. Journal 3's unsynced tail (60..80) survived too.
        let plan = plan(
            &[],
            &[
                journal(3, 0, 0, Some((0, 80))),
                journal(4, 0, 0, Some((60, 100))),
            ],
            len(100),
        );
        assert_eq!(
            summary(&plan),
            [(0, 0, 0, 100, vec![(3, 0, 60), (4, 60, 100)])]
        );
        // Both journals wait for the one segment.
        assert_eq!(plan.needs[&JournalId::new(3)].len(), 1);
        assert_eq!(plan.needs[&JournalId::new(4)].len(), 1);
    }

    #[test]
    fn a_journal_entirely_superseded_still_waits_for_the_segment() {
        // Journal 1 wins every sample; journal 0 supplies none, but its
        // samples are in no row until the segment commits.
        let plan = plan(
            &[],
            &[
                journal(0, 0, 0, Some((0, 100))),
                journal(1, 0, 0, Some((0, 200))),
            ],
            len(1_000),
        );
        assert_eq!(summary(&plan), [(0, 0, 0, 200, vec![(1, 0, 200)])]);
        let key = BTreeSet::from([(TrackId::new(0), SampleIndex::ZERO)]);
        assert_eq!(plan.needs[&JournalId::new(0)], key);
        assert_eq!(plan.needs[&JournalId::new(1)], key);
    }

    #[test]
    fn a_newer_journal_inside_an_older_one_splits_it() {
        let plan = plan(
            &[],
            &[
                journal(1, 0, 0, Some((0, 100))),
                journal(2, 0, 0, Some((40, 60))),
            ],
            len(1_000),
        );
        assert_eq!(
            summary(&plan),
            [(0, 0, 0, 100, vec![(1, 0, 40), (2, 40, 60), (1, 60, 100)])]
        );
    }

    #[test]
    fn committed_rows_win_over_every_journal() {
        // A crash after the row committed but before journal 5 was deleted;
        // journal 6's samples aren't in a row yet.
        let plan = plan(
            &[row(0, 0, 100)],
            &[
                journal(5, 0, 0, Some((0, 100))),
                journal(6, 0, 0, Some((90, 130))),
            ],
            len(1_000),
        );
        assert_eq!(summary(&plan), [(0, 0, 100, 130, vec![(6, 100, 130)])]);
        assert!(plan.needs[&JournalId::new(5)].is_empty());
        assert_eq!(plan.needs[&JournalId::new(6)].len(), 1);
    }

    #[test]
    fn gaps_epochs_and_windows_split_segments() {
        let plan = plan(
            &[],
            &[
                // A gap at 40..50.
                journal(0, 0, 0, Some((0, 40))),
                journal(1, 0, 0, Some((50, 70))),
                // Continuous samples, but a new epoch.
                journal(2, 0, 1, Some((70, 90))),
                // Crosses a window boundary (the writer never does this, but
                // salvage takes what it finds).
                journal(3, 0, 1, Some((90, 230))),
            ],
            len(100),
        );
        assert_eq!(
            summary(&plan),
            [
                (0, 0, 0, 40, vec![(0, 0, 40)]),
                (0, 0, 50, 70, vec![(1, 50, 70)]),
                (0, 1, 70, 100, vec![(2, 70, 90), (3, 90, 100)]),
                (0, 1, 100, 200, vec![(3, 100, 200)]),
                (0, 1, 200, 230, vec![(3, 200, 230)]),
            ]
        );
        assert_eq!(plan.needs[&JournalId::new(3)].len(), 3);
    }

    #[test]
    fn empty_journals_need_nothing() {
        let plan = plan(&[], &[journal(9, 0, 0, None)], len(100));
        assert!(plan.segments.is_empty());
        assert_eq!(plan.needs[&JournalId::new(9)], BTreeSet::new());
    }

    #[test]
    fn rows_of_other_tracks_claim_nothing() {
        let plan = plan(
            &[row(1, 0, 100)],
            &[journal(0, 0, 0, Some((0, 100)))],
            len(100),
        );
        assert_eq!(summary(&plan), [(0, 0, 0, 100, vec![(0, 0, 100)])]);
    }

    #[test]
    fn the_last_window_runs_to_the_end_of_the_numbers() {
        let top = u64::MAX;
        let plan = plan(
            &[],
            &[journal(0, 0, 0, Some((top - 10, top)))],
            len(u64::MAX / 2 + 1),
        );
        assert_eq!(
            summary(&plan),
            [(0, 0, top - 10, top, vec![(0, top - 10, top)])]
        );
    }

    #[test]
    fn claimed_merges_touching_and_overlapping_ranges() {
        let mut c = Claimed::default();
        c.add(range(10, 20));
        c.add(range(30, 40));
        c.add(range(20, 25));
        c.add(range(0, 0));
        assert_eq!(c.0, [range(10, 25), range(30, 40)]);
        c.add(range(5, 35));
        assert_eq!(c.0, [range(5, 40)]);
        assert_eq!(c.subtract(range(0, 50)), [range(0, 5), range(40, 50)]);
        assert_eq!(c.subtract(range(6, 39)), []);
    }

    /// Up to five journals of one or two tracks and a few rows, in a small
    /// sample space so overlaps are common.
    fn inputs() -> impl Strategy<Value = (Vec<SegmentRow>, Vec<JournalSummary>, u64)> {
        let journals = prop::collection::vec(
            (
                0..2_u32,
                0..2_u32,
                prop::option::of((0..120_u64, 1..60_u64)),
            ),
            0..6,
        )
        .prop_map(|js| {
            js.into_iter()
                .enumerate()
                .map(|(i, (track, epoch, r))| {
                    journal(i as u64, track, epoch, r.map(|(s, l)| (s, s + l)))
                })
                .collect::<Vec<_>>()
        });
        // Rows of one track never overlap each other.
        let rows = prop::collection::btree_set(0..12_u64, 0..4).prop_map(|starts| {
            starts
                .into_iter()
                .map(|k| row(0, k * 15, k * 15 + 7))
                .collect::<Vec<_>>()
        });
        (rows, journals, 5..80_u64)
    }

    /// Who holds a sample, by the rules.
    #[derive(Debug, PartialEq, Eq)]
    enum Owner {
        Nobody,
        Row,
        Journal(JournalId),
    }

    /// Who holds sample `s` of `track`: rows first, then the highest
    /// journal id.
    fn owner(
        rows: &[SegmentRow],
        journals: &[JournalSummary],
        track: TrackId,
        s: SampleIndex,
    ) -> Owner {
        if rows
            .iter()
            .any(|r| r.track() == track && r.range().contains(s))
        {
            return Owner::Row;
        }
        journals
            .iter()
            .filter(|j| j.track == track && j.range.is_some_and(|r| r.contains(s)))
            .map(|j| j.id)
            .max()
            .map_or(Owner::Nobody, Owner::Journal)
    }

    proptest! {
        #[test]
        fn plans_follow_the_rules((rows, journals, window) in inputs()) {
            let length = len(window);
            let plan = plan(&rows, &journals, length);
            let mut covered = BTreeMap::new();
            for (i, seg) in plan.segments.iter().enumerate() {
                // Ordered by key, one window, continuous parts from one epoch.
                if i > 0 {
                    prop_assert!(plan.segments[i - 1].key() < seg.key());
                }
                prop_assert_eq!(
                    length.window_of(seg.range.start()),
                    length.window_of(SampleIndex::new(seg.range.end().get() - 1))
                );
                let mut at = seg.range.start();
                for part in &seg.parts {
                    prop_assert_eq!(part.range.start(), at);
                    prop_assert!(!part.range.is_empty());
                    at = part.range.end();
                    let j = journals.iter().find(|j| j.id == part.journal).unwrap();
                    prop_assert_eq!(j.track, seg.track);
                    prop_assert_eq!(j.epoch, seg.epoch);
                    prop_assert!(plan.needs[&part.journal].contains(&seg.key()));
                    for s in part.range.start().get()..part.range.end().get() {
                        let s = SampleIndex::new(s);
                        // Each sample planned once, from the journal that owns it.
                        prop_assert!(covered.insert((seg.track, s), part.journal).is_none());
                        prop_assert_eq!(
                            owner(&rows, &journals, seg.track, s),
                            Owner::Journal(part.journal)
                        );
                    }
                }
                prop_assert_eq!(at, seg.range.end());
            }
            // Every journal-owned sample is planned; row-owned ones aren't.
            for j in &journals {
                let Some(r) = j.range else { continue };
                for s in r.start().get()..r.end().get() {
                    let s = SampleIndex::new(s);
                    match owner(&rows, &journals, j.track, s) {
                        Owner::Journal(id) => prop_assert_eq!(covered.get(&(j.track, s)), Some(&id)),
                        _ => prop_assert!(!covered.contains_key(&(j.track, s))),
                    }
                }
            }
            // A journal needs exactly the new segments its samples overlap,
            // which include every one it has parts in.
            for j in &journals {
                let expected: BTreeSet<_> = plan
                    .segments
                    .iter()
                    .filter(|s| {
                        s.track == j.track
                            && j.range.is_some_and(|r| {
                                s.range.start() < r.end() && r.start() < s.range.end()
                            })
                    })
                    .map(PlannedSegment::key)
                    .collect();
                prop_assert_eq!(&plan.needs[&j.id], &expected);
            }
            // Adjacent segments of a track and epoch in one window would have
            // been one segment.
            for pair in plan.segments.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                if a.track == b.track && a.epoch == b.epoch && a.range.end() == b.range.start() {
                    prop_assert_ne!(
                        length.window_of(a.range.start()),
                        length.window_of(b.range.start())
                    );
                }
            }
        }
    }
}
