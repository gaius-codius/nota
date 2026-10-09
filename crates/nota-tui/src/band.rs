//! The timeline band: a level waveform of the whole session so far, with
//! marks and notes placed above it, both squeezed into the screen's width.

use std::ops::Range;
use std::time::Duration;

use nota_core::SessionTime;
use nota_core::recorder::Level;

/// How finely levels are kept: the loudest level in each 250 ms of the
/// session. A three-hour session is about 43 000 bins.
const BIN: Duration = Duration::from_millis(250);

/// The loudest level heard in each bin of the session, `None` where nothing
/// was heard (before the first level, or across a gap).
#[derive(Debug, Default)]
pub(crate) struct LevelHistory {
    bins: Vec<Option<Level>>,
}

impl LevelHistory {
    /// Records `level`, heard at `at`.
    pub(crate) fn record(&mut self, at: SessionTime, level: Level) {
        let Some(index) = bin_of(at) else { return };
        if self.bins.len() <= index {
            self.bins.resize(index + 1, None);
        }
        let bin = &mut self.bins[index];
        *bin = Some(bin.map_or(level, |seen| seen.max(level)));
    }

    /// The band's columns at `now`: `width` levels, the session from its
    /// start on the left to `now` on the right. Each column is the loudest
    /// level in its share of the session, or `None` if nothing was heard
    /// there.
    pub(crate) fn columns(&self, now: SessionTime, width: usize) -> Vec<Option<Level>> {
        let total = bins_until(now);
        (0..width)
            .map(|column| {
                let bins = column_bins(column, total, width);
                self.bins
                    .get(bins.start..bins.end.min(self.bins.len()))
                    .unwrap_or_default()
                    .iter()
                    .flatten()
                    .copied()
                    .max()
            })
            .collect()
    }
}

/// The column, of `width`, that session time `at` falls in when the band
/// shows the session up to `now`: the first column whose bins hold `at`'s,
/// so a mark sits over the level heard with it. Times after `now` land in
/// the last column.
pub(crate) fn column_of(at: SessionTime, now: SessionTime, width: usize) -> usize {
    let total = bins_until(now);
    let last = width.saturating_sub(1);
    let Some(bin) = bin_of(at).filter(|&bin| bin < total) else {
        return last;
    };
    (0..width)
        .find(|&column| column_bins(column, total, width).contains(&bin))
        .unwrap_or(last)
}

/// The bins up to and including the one `now` is in.
fn bins_until(now: SessionTime) -> usize {
    bin_of(now).map_or(usize::MAX, |bin| bin.saturating_add(1))
}

/// The bins column `column` of `width` covers, when the band shows `total`
/// bins. Each column covers at least one, so a short session still fills
/// the band rather than leaving gaps between columns.
fn column_bins(column: usize, total: usize, width: usize) -> Range<usize> {
    let start = share(column, total, width);
    start..share(column + 1, total, width).max(start + 1)
}

/// Bin `index * total / width`, rounded down.
fn share(index: usize, total: usize, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let share = index as u128 * total as u128 / width as u128;
    usize::try_from(share).unwrap_or(usize::MAX)
}

/// The bin `at` falls in, or `None` if it's too far into the session to index
/// (never, in practice).
fn bin_of(at: SessionTime) -> Option<usize> {
    usize::try_from(at.as_nanos() / u64::try_from(BIN.as_nanos()).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> SessionTime {
        SessionTime::from_elapsed(Duration::from_secs(s)).unwrap()
    }

    fn millis(ms: u64) -> SessionTime {
        SessionTime::from_elapsed(Duration::from_millis(ms)).unwrap()
    }

    #[test]
    fn a_bin_keeps_its_loudest_level() {
        let mut history = LevelHistory::default();
        history.record(millis(10), Level::from_peak(5));
        history.record(millis(200), Level::from_peak(50));
        history.record(millis(240), Level::from_peak(7));
        history.record(millis(260), Level::from_peak(1));
        assert_eq!(
            history.bins,
            vec![Some(Level::from_peak(50)), Some(Level::from_peak(1))]
        );
    }

    #[test]
    fn columns_spread_the_whole_session_across_the_width() {
        let mut history = LevelHistory::default();
        // One level per second for 8 s, the loudness rising with time.
        for s in 0..8_u16 {
            history.record(secs(s.into()), Level::from_peak(s * 100 + 1));
        }
        // 8 s is 33 bins (the last one is `now`'s); four columns of about
        // eight bins, each holding two of the levels.
        let columns = history.columns(secs(8), 4);
        assert_eq!(
            columns,
            [101, 301, 501, 701]
                .map(|p| Some(Level::from_peak(p)))
                .to_vec()
        );
    }

    #[test]
    fn a_short_session_fills_every_column() {
        let mut history = LevelHistory::default();
        history.record(millis(0), Level::from_peak(9));
        history.record(millis(300), Level::from_peak(4));
        let columns = history.columns(millis(300), 6);
        assert_eq!(
            columns,
            [9, 9, 9, 4, 4, 4]
                .map(|p| Some(Level::from_peak(p)))
                .to_vec()
        );
    }

    #[test]
    fn unheard_stretches_are_empty_columns() {
        let mut history = LevelHistory::default();
        history.record(secs(0), Level::from_peak(3));
        history.record(secs(9), Level::from_peak(3));
        let columns = history.columns(secs(10), 5);
        assert_eq!(columns[0], Some(Level::from_peak(3)));
        assert_eq!(columns[1..4], [None, None, None]);
        assert_eq!(columns[4], Some(Level::from_peak(3)));
        assert_eq!(LevelHistory::default().columns(secs(10), 3), vec![None; 3]);
    }

    #[test]
    fn column_of_scales_time_to_width() {
        // 100 s is 401 bins; 50 columns of eight or nine.
        let now = secs(100);
        assert_eq!(column_of(secs(0), now, 50), 0);
        assert_eq!(column_of(secs(1), now, 50), 0);
        assert_eq!(column_of(millis(1_999), now, 50), 0);
        assert_eq!(column_of(secs(2), now, 50), 1);
        assert_eq!(column_of(millis(2_250), now, 50), 1);
        assert_eq!(column_of(secs(99), now, 50), 49);
        assert_eq!(column_of(now, now, 50), 49);
        assert_eq!(column_of(secs(500), now, 50), 49);
        assert_eq!(column_of(secs(0), SessionTime::ZERO, 50), 0);
        assert_eq!(column_of(secs(1), SessionTime::ZERO, 50), 49);
        assert_eq!(column_of(secs(1), now, 0), 0);
    }

    #[test]
    fn marks_sit_over_the_level_heard_with_them() {
        // Short and long sessions, narrow and wide bands: the column a time
        // lands in is a column that shows the level recorded at that time.
        for (now_ms, width) in [
            (300, 62),
            (2_000, 62),
            (100_000, 58),
            (4_368_000, 58),
            (9_999, 7),
        ] {
            let now = millis(now_ms);
            for at_ms in (0..=now_ms).step_by(usize::try_from(now_ms / 97 + 1).unwrap()) {
                let mut history = LevelHistory::default();
                history.record(millis(at_ms), Level::from_peak(77));
                let column = column_of(millis(at_ms), now, width);
                assert_eq!(
                    history.columns(now, width)[column],
                    Some(Level::from_peak(77)),
                    "at {at_ms} ms of {now_ms} ms, {width} wide"
                );
            }
        }
    }
}
