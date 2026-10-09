//! The bar each sound level draws as in the band.

use nota_core::recorder::Level;

/// The bars of the level band, quietest first.
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The quietest level the band tells apart from silence, in dBFS. Anything
/// quieter draws as the lowest bar.
const FLOOR_DBFS: f64 = -60.0;

/// The bar `level` draws as: eight steps on a decibel scale from
/// [`FLOOR_DBFS`] to full scale, so quiet speech still shows.
pub(crate) fn bar(level: Level) -> char {
    if level.peak() == 0 {
        return BARS[0];
    }
    let dbfs = 20.0 * (f64::from(level.peak()) / f64::from(Level::FULL_SCALE.peak())).log10();
    let step = ((dbfs - FLOOR_DBFS) / -FLOOR_DBFS * 8.0).floor();
    // `step` is finite and clamped to 0..=7 before the cast.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to the bar indices first"
    )]
    let index = step.clamp(0.0, 7.0) as usize;
    BARS[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_step_up_with_loudness() {
        assert_eq!(bar(Level::SILENT), '▁');
        // Below the -60 dBFS floor.
        assert_eq!(bar(Level::from_peak(10)), '▁');
        assert_eq!(bar(Level::FULL_SCALE), '█');
        // -6 dBFS is already in the top step: (60 - 6) / 60 * 8 = 7.2.
        assert_eq!(bar(Level::from_peak(16_422)), '█');
        // -20 dBFS: 40 / 60 * 8 = 5.33, the sixth bar.
        assert_eq!(bar(Level::from_peak(3_277)), '▆');
        // -40 dBFS: 20 / 60 * 8 = 2.67, the third bar.
        assert_eq!(bar(Level::from_peak(328)), '▃');
        let mut last = '▁';
        for peak in (0..=Level::FULL_SCALE.peak()).step_by(97) {
            let bar = bar(Level::from_peak(peak));
            assert!(bar >= last, "{peak} drew {bar} after {last}");
            last = bar;
        }
    }
}
