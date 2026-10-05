//! The timeline band: a level waveform of the whole session so far, with
//! marks and notes placed above it, both squeezed into the screen's width.

use std::time::Duration;

use nota_core::SessionTime;

use crate::level::Level;

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
        // Bins up to and including the one `now` is in.
        let total = bin_of(now).map_or(self.bins.len(), |bin| bin + 1);
        (0..width)
            .map(|column| {
                let start = share(column, total, width);
                // At least one bin each, so a short session still fills the
                // band rather than leaving gaps between columns.
                let end = share(column + 1, total, width).max(start + 1);
                self.bins
                    .get(start..end.min(self.bins.len()))
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
/// shows the session up to `now`. Times after `now` land in the last column.
pub(crate) fn column_of(at: SessionTime, now: SessionTime, width: usize) -> usize {
    let last = width.saturating_sub(1);
    if now.as_nanos() == 0 {
        return if at.as_nanos() == 0 { 0 } else { last };
    }
    let column = u128::from(at.as_nanos()) * width as u128 / u128::from(now.as_nanos());
    usize::try_from(column).map_or(last, |column| column.min(last))
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
        let now = secs(100);
        assert_eq!(column_of(secs(0), now, 50), 0);
        assert_eq!(column_of(secs(1), now, 50), 0);
        assert_eq!(column_of(secs(2), now, 50), 1);
        assert_eq!(column_of(secs(99), now, 50), 49);
        assert_eq!(column_of(now, now, 50), 49);
        assert_eq!(column_of(secs(500), now, 50), 49);
        assert_eq!(column_of(secs(0), SessionTime::ZERO, 50), 0);
        assert_eq!(column_of(secs(1), SessionTime::ZERO, 50), 49);
        assert_eq!(column_of(secs(1), now, 0), 0);
    }
}
