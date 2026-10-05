//! Sound levels, and the bar each one draws as in the band.

/// The bars of the level band, quietest first.
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The quietest level the band tells apart from silence, in dBFS. Anything
/// quieter draws as the lowest bar.
const FLOOR_DBFS: f64 = -60.0;

/// The peak level of a run of 16-bit samples: their largest magnitude, from 0
/// (silence) to [`Level::FULL_SCALE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Level(u16);

impl Level {
    /// Silence.
    pub const SILENT: Self = Self(0);

    /// The loudest a 16-bit sample can be.
    pub const FULL_SCALE: Self = Self(i16::MAX.unsigned_abs());

    /// A level from a peak magnitude, capped at [`Level::FULL_SCALE`].
    #[must_use]
    pub fn from_peak(peak: u16) -> Self {
        Self(peak.min(Self::FULL_SCALE.0))
    }

    /// The peak of `samples`; silence if there are none. `i16::MIN` counts as
    /// full scale.
    #[must_use]
    pub fn of_samples(samples: &[i16]) -> Self {
        let peak = samples
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap_or(0);
        Self::from_peak(peak)
    }

    /// The peak magnitude.
    #[must_use]
    pub fn peak(self) -> u16 {
        self.0
    }

    /// The bar this level draws as: eight steps on a decibel scale from
    /// [`FLOOR_DBFS`] to full scale, so quiet speech still shows.
    pub(crate) fn bar(self) -> char {
        if self.0 == 0 {
            return BARS[0];
        }
        let dbfs = 20.0 * (f64::from(self.0) / f64::from(Self::FULL_SCALE.0)).log10();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_of_samples_is_the_largest_magnitude() {
        assert_eq!(Level::of_samples(&[]), Level::SILENT);
        assert_eq!(Level::of_samples(&[3, -700, 12]).peak(), 700);
        assert_eq!(Level::of_samples(&[i16::MIN]), Level::FULL_SCALE);
        assert_eq!(Level::of_samples(&[i16::MAX]), Level::FULL_SCALE);
    }

    #[test]
    fn from_peak_caps_at_full_scale() {
        assert_eq!(Level::from_peak(u16::MAX), Level::FULL_SCALE);
        assert_eq!(Level::from_peak(5).peak(), 5);
    }

    #[test]
    fn bars_step_up_with_loudness() {
        assert_eq!(Level::SILENT.bar(), '▁');
        // Below the -60 dBFS floor.
        assert_eq!(Level::from_peak(10).bar(), '▁');
        assert_eq!(Level::FULL_SCALE.bar(), '█');
        // -6 dBFS is already in the top step: (60 - 6) / 60 * 8 = 7.2.
        assert_eq!(Level::from_peak(16_422).bar(), '█');
        // -20 dBFS: 40 / 60 * 8 = 5.33, the sixth bar.
        assert_eq!(Level::from_peak(3_277).bar(), '▆');
        // -40 dBFS: 20 / 60 * 8 = 2.67, the third bar.
        assert_eq!(Level::from_peak(328).bar(), '▃');
        let mut last = '▁';
        for peak in (0..=Level::FULL_SCALE.peak()).step_by(97) {
            let bar = Level::from_peak(peak).bar();
            assert!(bar >= last, "{peak} drew {bar} after {last}");
            last = bar;
        }
    }
}
