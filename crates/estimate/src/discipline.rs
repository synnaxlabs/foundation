//! What mesh time follows, and how each estimate changes it.

use types::time::Monotonic;

use crate::{Drift, Measurement, Slew, combine};

/// What mesh time follows: no time before the first estimate, a slew toward the
/// estimate of a majority, or the last slew in holdover.
///
/// ```
/// use estimate::combine::Error;
/// use estimate::discipline::{Cause, Discipline};
/// use estimate::{Drift, Measurement, Slew};
/// use types::time::{Monotonic, Span};
///
/// let (now, drift) = (Monotonic(10), Drift::UNDISCIPLINED);
/// let m = Measurement::new(now, Span::SECOND, Span::MILLISECOND).expect("valid");
/// let unsynced = Discipline::Unsynced(Error::NoSources);
/// assert_eq!(unsynced.next(Err(Error::NoSources), drift), None);
/// let synced = unsynced.next(Ok(m), drift).expect("a change").at(now, drift);
/// assert_eq!(synced, Discipline::Synced(Slew::new(m)));
/// let holdover = synced.next(Err(Error::NoSources), drift).expect("a change");
/// let kept = Discipline::Holdover(Slew::new(m), Cause::NoEstimate(Error::NoSources));
/// assert_eq!(holdover.at(now, drift), kept);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discipline {
    /// No majority of the sources has agreed yet, so there is no mesh time. Holds why.
    Unsynced(combine::Error),
    /// Mesh time slews toward the estimate of a majority.
    Synced(Slew),
    /// Mesh time keeps its slew, and the error of the slew's target grows by drift.
    /// Holds why.
    Holdover(Slew, Cause),
}

impl Discipline {
    /// The change that `estimate`, from [`combine::combine`] with `drift`, makes. The
    /// first estimate is served at once, and later ones are slewed toward. An error
    /// keeps the slew in holdover, or stays unsynced before the first estimate. An
    /// unknown estimate ([`Measurement::unknown`]) never replaces a known one: it keeps
    /// the slew in holdover while the slew's target, grown by `drift` to the estimate's
    /// time, is known. `None` only when the discipline stays as it is.
    #[must_use]
    pub fn next(
        self,
        estimate: Result<Measurement, combine::Error>,
        drift: Drift,
    ) -> Option<Change> {
        let known = |m: Measurement, at| m.known_at(at, drift);
        let step = match (self.slew(), estimate) {
            (None, Err(e)) => Step::To(Self::Unsynced(e)),
            (Some(slew), Err(e)) => {
                Step::To(Self::Holdover(slew, Cause::NoEstimate(e)))
            }
            (None, Ok(m)) => Step::To(Self::Synced(Slew::new(m))),
            (Some(slew), Ok(m)) if known(slew.target, m.at()) && !m.known() => {
                Step::To(Self::Holdover(slew, Cause::UnknownEstimate))
            }
            (Some(slew), Ok(m)) => Step::Toward(slew, m),
        };
        (step != Step::To(self)).then_some(Change(step))
    }

    /// The slew that mesh time follows, or `None` before the first estimate.
    #[must_use]
    pub const fn slew(self) -> Option<Slew> {
        match self {
            Self::Unsynced(_) => None,
            Self::Synced(slew) | Self::Holdover(slew, _) => Some(slew),
        }
    }
}

/// Why mesh time holds over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// [`combine`](crate::combine::combine) gave no estimate: no majority agrees, or no
    /// source is left.
    NoEstimate(combine::Error),
    /// Only sources with an unknown bound agree, after a known estimate.
    UnknownEstimate,
}

/// A change of [`Discipline`] that takes effect at a local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change(Step);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The same at any local time.
    To(Discipline),
    /// A slew toward a new target.
    Toward(Slew, Measurement),
}

impl Change {
    /// The discipline from `now` on, for a local clock that drifts from mesh time by
    /// at most `drift`. Mesh time from it at or after `now` is never earlier than mesh
    /// time from the last discipline at or before `now`.
    #[must_use]
    pub fn at(self, now: Monotonic, drift: Drift) -> Discipline {
        match self.0 {
            Step::To(discipline) => discipline,
            Step::Toward(slew, target) => {
                Discipline::Synced(slew.toward(now, drift, target))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use types::time::{Monotonic, Span};

    use super::{Cause, Discipline};
    use crate::combine::Error;
    use crate::measurement::MAX_ERROR;
    use crate::world::{SLEW_OFFSET_NS, TIME_NS, slew, target};
    use crate::{Drift, Measurement, Slew};

    /// Few counts, so two errors are often equal.
    fn error() -> impl Strategy<Value = Error> {
        let counts = (1..4_usize, 0..4_usize, 0..4_usize);
        prop_oneof![
            Just(Error::NoSources),
            counts.prop_map(|(sources, agreeing, empty)| Error::NoMajority {
                sources,
                agreeing,
                empty,
            }),
        ]
    }

    fn no_estimate() -> impl Strategy<Value = Cause> {
        error().prop_map(Cause::NoEstimate)
    }

    fn cause() -> impl Strategy<Value = Cause> {
        prop_oneof![no_estimate(), Just(Cause::UnknownEstimate)]
    }

    /// An estimate from unknown bounds alone.
    fn unknown() -> impl Strategy<Value = Measurement> {
        let parts = (0..TIME_NS, -SLEW_OFFSET_NS..SLEW_OFFSET_NS);
        parts.prop_map(|(at, offset)| {
            Measurement::unknown(Monotonic(at), Span::from_nanos(offset))
        })
    }

    /// A slew toward an unknown estimate.
    fn unknown_slew() -> impl Strategy<Value = Slew> {
        (slew(), unknown()).prop_map(|(slew, target)| Slew { target, ..slew })
    }

    fn any_slew() -> impl Strategy<Value = Slew> {
        prop_oneof![slew(), unknown_slew()]
    }

    /// A discipline with a slew from `slew`, held over for a cause from `cause`, and
    /// the slew.
    fn slewing<S: Strategy<Value = Slew>>(
        slew: impl Fn() -> S,
        cause: impl Strategy<Value = Cause>,
    ) -> impl Strategy<Value = (Discipline, Slew)> {
        prop_oneof![
            slew().prop_map(|s| (Discipline::Synced(s), s)),
            (slew(), cause).prop_map(|(s, c)| (Discipline::Holdover(s, c), s)),
        ]
    }

    fn drift() -> impl Strategy<Value = Drift> {
        (0..=100_000_000_u32).prop_map(|ppb| Drift::from_ppb(ppb).expect("valid"))
    }

    proptest! {
        #[test]
        fn serves_the_first_estimate_at_once(
            error in error(),
            m in target(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let unsynced = Discipline::Unsynced(error);
            let change = unsynced.next(Ok(m), drift).expect("a change");
            let synced = Discipline::Synced(Slew::new(m));
            prop_assert_eq!(change.at(Monotonic(now), drift), synced);
        }

        #[test]
        fn serves_an_unknown_first_estimate_at_once(
            error in error(),
            m in unknown(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let unsynced = Discipline::Unsynced(error);
            let change = unsynced.next(Ok(m), drift).expect("a change");
            let synced = Discipline::Synced(Slew::new(m));
            prop_assert_eq!(change.at(Monotonic(now), drift), synced);
        }

        #[test]
        fn stays_unsynced_with_no_estimate(
            old in error(),
            new in error(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let change = Discipline::Unsynced(old).next(Err(new), drift);
            let changed = (old != new).then_some(Discipline::Unsynced(new));
            prop_assert_eq!(change.map(|c| c.at(Monotonic(now), drift)), changed);
        }

        #[test]
        fn holds_the_slew_over_with_no_estimate(
            (old, slew) in slewing(slew, no_estimate()),
            error in error(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let holdover = Discipline::Holdover(slew, Cause::NoEstimate(error));
            let changed = (old != holdover).then_some(holdover);
            let change = old.next(Err(error), drift);
            prop_assert_eq!(change.map(|c| c.at(Monotonic(now), drift)), changed);
        }

        #[test]
        fn holds_any_slew_over_with_no_estimate(
            (old, slew) in slewing(any_slew, cause()),
            error in error(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let holdover = Discipline::Holdover(slew, Cause::NoEstimate(error));
            let changed = (old != holdover).then_some(holdover);
            let change = old.next(Err(error), drift);
            prop_assert_eq!(change.map(|c| c.at(Monotonic(now), drift)), changed);
        }

        #[test]
        fn slews_toward_a_later_estimate(
            (old, slew) in slewing(slew, no_estimate()),
            m in target(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let now = Monotonic(now);
            let change = old.next(Ok(m), drift).expect("a change");
            let synced = Discipline::Synced(slew.toward(now, drift, m));
            prop_assert_eq!(change.at(now, drift), synced);
        }

        #[test]
        fn slews_from_any_slew_toward_a_known_estimate(
            (old, slew) in slewing(any_slew, cause()),
            m in target(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let now = Monotonic(now);
            let change = old.next(Ok(m), drift).expect("a change");
            let synced = Discipline::Synced(slew.toward(now, drift, m));
            prop_assert_eq!(change.at(now, drift), synced);
        }

        #[test]
        fn holds_a_known_slew_over_with_an_unknown_estimate(
            (old, slew) in slewing(slew, cause()),
            m in unknown(),
            again in unknown(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let holdover = Discipline::Holdover(slew, Cause::UnknownEstimate);
            let changed = (old != holdover).then_some(holdover);
            let change = old.next(Ok(m), drift);
            prop_assert_eq!(change.map(|c| c.at(Monotonic(now), drift)), changed);
            prop_assert_eq!(holdover.next(Ok(again), drift), None);
        }

        #[test]
        fn slews_from_an_unknown_slew_toward_an_unknown_estimate(
            (old, slew) in slewing(unknown_slew, cause()),
            m in unknown(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let now = Monotonic(now);
            let change = old.next(Ok(m), drift).expect("a change");
            let synced = Discipline::Synced(slew.toward(now, drift, m));
            prop_assert_eq!(change.at(now, drift), synced);
        }
    }

    /// Any error under 36500 days is known, also one wider than [`target`] gives.
    #[test]
    fn slews_toward_a_known_estimate_of_any_width() {
        let (now, drift) = (Monotonic(10), Drift::UNDISCIPLINED);
        let first =
            Measurement::new(now, Span::ZERO, Span::MILLISECOND).expect("valid");
        let synced = Discipline::Synced(Slew::new(first));
        let widest = Span::from_nanos(MAX_ERROR.nanos() - 1);
        for error in [Span::from_nanos(16 * Span::SECOND.nanos()), widest] {
            let wide = Measurement::new(now, Span::SECOND, error).expect("valid");
            let change = synced.next(Ok(wide), drift).expect("a change");
            let toward = Slew::new(first).toward(now, drift, wide);
            assert_eq!(
                change.at(now, drift),
                Discipline::Synced(toward),
                "{error:?}"
            );
        }
    }

    /// A held bound that drift grows to 36500 days is unknown, as `combine` sorts it.
    #[test]
    fn follows_an_unknown_estimate_once_drift_makes_the_slew_unknown() {
        let drift = Drift::UNDISCIPLINED;
        let widest = Span::from_nanos(MAX_ERROR.nanos() - 1);
        let held = Measurement::new(Monotonic(0), Span::ZERO, widest).expect("valid");
        let holdover = Discipline::Holdover(Slew::new(held), Cause::UnknownEstimate);
        let now = Monotonic(0);
        let unknown = Measurement::unknown(now, Span::SECOND);
        let change = Discipline::Synced(Slew::new(held)).next(Ok(unknown), drift);
        assert_eq!(change.map(|c| c.at(now, drift)), Some(holdover));
        // Drift grows the held bound by 1 ns, rounded up.
        let later = Monotonic(1);
        let unknown = Measurement::unknown(later, Span::SECOND);
        let change = holdover.next(Ok(unknown), drift).expect("a change");
        let toward = Slew::new(held).toward(later, drift, unknown);
        assert_eq!(change.at(later, drift), Discipline::Synced(toward));
    }
}
