use types::time::Monotonic;

use crate::{Drift, Measurement};

const CAPACITY: usize = 8;

/// The last 8 measurements of one source. It answers with the one whose bound is the
/// smallest now, so a precise measurement wins until drift makes it wider than a
/// newer one.
///
/// ```
/// use estimate::{Drift, Filter, Measurement};
/// use types::time::{Monotonic, Span};
///
/// let m = |at, error| Measurement::new(Monotonic(at), Span::ZERO, error);
/// let mut filter = Filter::default();
/// filter.push(m(0, Span::SECOND).expect("at most 36500 days"));
/// filter.push(m(1, Span::MILLISECOND).expect("at most 36500 days"));
/// let best = filter.best(Monotonic(2), Drift::UNDISCIPLINED);
/// assert_eq!(best.map(|m| m.error()), Some(Span::MILLISECOND));
/// ```
#[derive(Clone, Debug, Default)]
pub struct Filter {
    recent: [Option<Measurement>; CAPACITY],
    oldest: usize,
}

impl Filter {
    /// Adds a measurement. With 8 already held, it drops the oldest.
    pub fn push(&mut self, measurement: Measurement) {
        self.recent[self.oldest] = Some(measurement);
        self.oldest = (self.oldest + 1) % CAPACITY;
    }

    /// The held measurement with the smallest error bound at `now`, or `None` when it
    /// holds none. Of equal bounds, the one pushed last wins.
    #[must_use]
    pub fn best(&self, now: Monotonic, drift: Drift) -> Option<Measurement> {
        let (newer, older) = self.recent.split_at(self.oldest);
        older
            .iter()
            .chain(newer)
            .flatten()
            .rev()
            .min_by_key(|m| m.error_at(now, drift))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use types::time::{Monotonic, Span};

    use crate::{Drift, Filter, Measurement};

    const SECOND_NS: u64 = 1_000_000_000;

    fn measurement(at: u64, offset: i64, error: Span) -> Measurement {
        Measurement::new(Monotonic(at), Span::from_nanos(offset), error).expect("valid")
    }

    fn no_drift() -> Drift {
        Drift::from_ppb(0).expect("valid")
    }

    #[test]
    fn has_no_best_when_empty() {
        let best = Filter::default().best(Monotonic(0), Drift::UNDISCIPLINED);
        assert_eq!(best, None);
    }

    #[test]
    fn keeps_only_the_last_8() {
        let mut filter = Filter::default();
        filter.push(measurement(0, 0, Span::NANOSECOND));
        for at in 1..=8 {
            filter.push(measurement(at, 0, Span::SECOND));
        }
        let best = filter.best(Monotonic(8), no_drift());
        assert_eq!(best.map(Measurement::error), Some(Span::SECOND));
    }

    #[test]
    fn keeps_the_precise_one_among_the_last_8() {
        let mut filter = Filter::default();
        for at in 0..7 {
            filter.push(measurement(at, 0, Span::SECOND));
        }
        filter.push(measurement(7, 0, Span::NANOSECOND));
        let best = filter.best(Monotonic(7), no_drift());
        assert_eq!(best.map(Measurement::error), Some(Span::NANOSECOND));
    }

    #[test]
    fn prefers_a_fresh_coarse_one_once_drift_widens_an_old_precise_one() {
        let mut filter = Filter::default();
        filter.push(measurement(0, 1, Span::MICROSECOND));
        filter.push(measurement(10 * SECOND_NS, 2, Span::MILLISECOND));
        let now = Monotonic(10 * SECOND_NS);
        let offset = |drift| filter.best(now, drift).map(|m| m.offset().nanos());
        assert_eq!(offset(Drift::UNDISCIPLINED), Some(2));
        assert_eq!(offset(no_drift()), Some(1));
    }

    #[test]
    fn prefers_the_last_pushed_of_equal_bounds() {
        let mut filter = Filter::default();
        for offset in 0..12 {
            filter.push(measurement(0, offset, Span::SECOND));
        }
        let best = filter.best(Monotonic(0), Drift::UNDISCIPLINED);
        assert_eq!(best.map(|m| m.offset().nanos()), Some(11));
    }
}
