const MAX_PPB: u32 = 100_000_000;

/// Billionths of a nanosecond in a nanosecond: the unit of [`Drift::over_exact`].
pub(crate) const PER_NANO: i128 = 1_000_000_000;

/// The fastest that the local oscillator can drift from true time, in parts per
/// billion. An error bound grows by this rate as its measurement ages.
///
/// ```
/// let drift = estimate::Drift::from_ppb(200_000);
/// assert_eq!(drift, Some(estimate::Drift::UNDISCIPLINED));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Drift(u32);

impl Drift {
    /// 200 ppm, a safe bound for an oscillator that nothing disciplines.
    pub const UNDISCIPLINED: Self = Self(200_000);

    /// The drift bound of `ppb` parts per billion, or `None` when `ppb` is more than
    /// 100,000,000 (10%).
    #[must_use]
    pub const fn from_ppb(ppb: u32) -> Option<Self> {
        if ppb > MAX_PPB {
            return None;
        }
        Some(Self(ppb))
    }

    /// [`Drift::over`] in billionths of a nanosecond, not rounded.
    pub(crate) fn over_exact(self, elapsed_ns: u64) -> i128 {
        i128::from(elapsed_ns) * i128::from(self.0)
    }

    /// The most that an offset can move in `elapsed_ns`, rounded up. It is at most a
    /// tenth of `u64::MAX`.
    pub(crate) fn over(self, elapsed_ns: u64) -> i64 {
        let ns = (u128::from(elapsed_ns) * u128::from(self.0)).div_ceil(1_000_000_000);
        i64::try_from(ns).expect("invariant: at most 10% of u64::MAX nanoseconds")
    }
}

#[cfg(test)]
mod tests {
    use super::Drift;

    fn drift(ppb: u32) -> Drift {
        Drift::from_ppb(ppb).expect("valid")
    }

    #[test]
    fn rejects_more_than_10_percent() {
        assert_eq!(Drift::from_ppb(100_000_000), Some(Drift(100_000_000)));
        assert_eq!(Drift::from_ppb(100_000_001), None);
    }

    #[test]
    fn rounds_growth_up() {
        assert_eq!(drift(1).over(1), 1);
        assert_eq!(drift(1).over(1_000_000_000), 1);
        assert_eq!(drift(1).over(1_000_000_001), 2);
    }

    #[test]
    fn adds_nothing_without_time_or_drift() {
        assert_eq!(Drift::UNDISCIPLINED.over(0), 0);
        assert_eq!(drift(0).over(u64::MAX), 0);
    }

    #[test]
    fn grows_200_ppm_undisciplined() {
        assert_eq!(Drift::UNDISCIPLINED.over(u64::MAX), 3_689_348_814_741_911);
    }

    #[test]
    fn grows_a_tenth_of_the_longest_time_at_most() {
        assert_eq!(drift(100_000_000).over(u64::MAX), 1_844_674_407_370_955_162);
    }
}
