//! How sure the engine was of what it heard: a log-probability.
//!
//! The engine gives each token it decodes a log-probability, and each word
//! the lowest of its tokens' (T10, gaius-codius/nota#149, found the lowest
//! marks misheard terms better than the mean). The value is kept with the
//! word as heard and never changed. A word may have none, from an engine
//! that didn't give one or stored before they were kept: that is unknown,
//! never zero, since zero means the model was certain.

use std::cmp::Ordering;

/// The natural log of the probability the model gave what it heard: always
/// finite and at most zero, where zero is certain.
///
/// There is one zero: a negative zero is made positive. So two values are
/// equal exactly when their bits are, which keeps an encoded value
/// canonical and lets the type be [`Eq`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogProb(f32);

// No NaN gets in, so equality is an equivalence.
impl Eq for LogProb {}

impl PartialOrd for LogProb {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LogProb {
    fn cmp(&self, other: &Self) -> Ordering {
        // With no NaN and one zero, the total order is the numeric one.
        self.0.total_cmp(&other.0)
    }
}

impl LogProb {
    /// Certain: a probability of one.
    pub const CERTAIN: Self = Self(0.0);

    /// `value` as a log-probability, a negative zero made positive. `None`
    /// if it is NaN, infinite or above zero.
    #[must_use]
    pub fn new(value: f32) -> Option<Self> {
        // Adding zero turns -0.0 into 0.0 and leaves every other value.
        (value.is_finite() && value <= 0.0).then_some(Self(value + 0.0))
    }

    /// The log-probability whose `f32` bits are `bits`, if those bits are
    /// one: not NaN, infinite, above zero or a negative zero. The inverse
    /// of [`LogProb::to_bits`], so each value has one encoding.
    #[must_use]
    pub fn from_bits(bits: u32) -> Option<Self> {
        Self::new(f32::from_bits(bits)).filter(|value| value.to_bits() == bits)
    }

    /// The value's `f32` bits.
    #[must_use]
    pub const fn to_bits(self) -> u32 {
        self.0.to_bits()
    }

    /// The value.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only finite values at or below zero are log-probabilities.
    #[test]
    fn only_finite_values_at_most_zero_are_taken() {
        assert_eq!(LogProb::new(-0.488).map(LogProb::get), Some(-0.488));
        assert_eq!(LogProb::new(0.0), Some(LogProb::CERTAIN));
        assert_eq!(LogProb::new(f32::MIN).map(LogProb::get), Some(f32::MIN));
        for bad in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN_POSITIVE,
            1.0,
        ] {
            assert_eq!(LogProb::new(bad), None, "{bad}");
        }
    }

    /// A negative zero is the one zero, so equal values have equal bits.
    #[test]
    fn a_negative_zero_is_made_positive() {
        let zero = LogProb::new(-0.0).unwrap();
        assert_eq!(zero.to_bits(), 0);
        assert_eq!(zero, LogProb::CERTAIN);
    }

    /// Bits read back only as the value they encode, so no two encodings
    /// mean the same value.
    #[test]
    fn bits_read_back_only_as_their_own_value() {
        let value = LogProb::new(-1.954).unwrap();
        assert_eq!(LogProb::from_bits(value.to_bits()), Some(value));
        assert_eq!(LogProb::from_bits(0), Some(LogProb::CERTAIN));
        for bad in [
            (-0.0_f32).to_bits(),
            f32::NAN.to_bits(),
            0x7fc0_0001,
            f32::NEG_INFINITY.to_bits(),
            1.0_f32.to_bits(),
        ] {
            assert_eq!(LogProb::from_bits(bad), None, "{bad:#010x}");
        }
    }

    /// Values order numerically, the least sure first.
    #[test]
    fn values_order_by_how_sure_they_are() {
        let mut values: Vec<LogProb> = [-0.2, 0.0, -1.5, -0.488]
            .into_iter()
            .filter_map(LogProb::new)
            .collect();
        values.sort();
        let got: Vec<f32> = values.into_iter().map(LogProb::get).collect();
        assert_eq!(got, [-1.5, -0.488, -0.2, 0.0]);
        assert!(LogProb::new(-1.0) < LogProb::new(-0.5));
    }
}
