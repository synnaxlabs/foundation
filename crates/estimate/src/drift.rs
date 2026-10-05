/// The fastest that a local oscillator can drift from true time, in parts per
/// billion. An error bound grows by this rate as its measurement ages.
///
/// ```
/// assert_eq!(estimate::Drift::default(), estimate::Drift::from_ppb(200_000));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Drift(u32);

impl Drift {
    /// Wraps a rate in parts per billion.
    #[must_use]
    pub const fn from_ppb(ppb: u32) -> Self {
        Self(ppb)
    }

    /// The most that an offset can move in `elapsed_ns`, rounded up.
    pub(crate) fn over(self, elapsed_ns: u64) -> u128 {
        (u128::from(elapsed_ns) * u128::from(self.0)).div_ceil(1_000_000_000)
    }
}

/// 200 ppm, a safe bound for an oscillator that nothing disciplines.
impl Default for Drift {
    fn default() -> Self {
        Self(200_000)
    }
}

#[cfg(test)]
mod tests {
    use super::Drift;

    #[test]
    fn rounds_growth_up() {
        assert_eq!(Drift::from_ppb(1).over(1), 1);
        assert_eq!(Drift::from_ppb(1).over(1_000_000_000), 1);
        assert_eq!(Drift::from_ppb(1).over(1_000_000_001), 2);
    }

    #[test]
    fn adds_nothing_without_time_or_drift() {
        assert_eq!(Drift::default().over(0), 0);
        assert_eq!(Drift::from_ppb(0).over(u64::MAX), 0);
    }

    #[test]
    fn grows_200_ppm_by_default() {
        assert_eq!(Drift::default().over(u64::MAX), 3_689_348_814_741_911);
    }

    #[test]
    fn does_not_overflow_at_the_largest_inputs() {
        let most = Drift::from_ppb(u32::MAX).over(u64::MAX);
        assert!(most > u128::from(u64::MAX), "growth {most} at 4.29x");
    }
}
