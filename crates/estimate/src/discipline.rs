//! What mesh time follows, and how each estimate changes it.

use types::time::Monotonic;

use crate::measurement::MAX_ERROR;
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
/// assert_eq!(unsynced.next(Err(Error::NoSources)), None);
/// let synced = unsynced.next(Ok(m)).expect("a change").at(now, drift);
/// assert_eq!(synced, Discipline::Synced(Slew::new(m)));
/// let holdover = synced.next(Err(Error::NoSources)).expect("a change");
/// let kept = Discipline::Holdover(Slew::new(m), Cause::NoEstimate(Error::NoSources));
/// assert_eq!(holdover.at(now, drift), kept);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discipline {
    /// No majority of the sources has agreed yet, so there is no mesh time. Holds why.
    Unsynced(combine::Error),
    /// Mesh time slews toward the estimate of a majority.
    Synced(Slew),
    /// No majority agrees now, or only unknown bounds agree after a known estimate.
    /// Mesh time keeps its slew, and the error of the slew's target grows by drift.
    /// Holds why.
    Holdover(Slew, Cause),
}

impl Discipline {
    /// The change that `estimate`, a result of [`combine::combine`], makes. The first
    /// estimate is served at once, and later ones are slewed toward. An error keeps
    /// the slew in holdover, or stays unsynced before the first estimate. An unknown
    /// estimate ([`Measurement::unknown`]) never replaces a known one: it keeps a slew
    /// toward a known estimate in holdover. `None` only when the discipline stays as
    /// it is.
    #[must_use]
    pub fn next(self, estimate: Result<Measurement, combine::Error>) -> Option<Change> {
        let known = |m: Measurement| m.error() < MAX_ERROR;
        let step = match (self.slew(), estimate) {
            (None, Err(e)) => Step::To(Self::Unsynced(e)),
            (Some(slew), Err(e)) => {
                Step::To(Self::Holdover(slew, Cause::NoEstimate(e)))
            }
            (None, Ok(m)) => Step::To(Self::Synced(Slew::new(m))),
            (Some(slew), Ok(m)) if known(slew.target) && !known(m) => {
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
    /// [`combine`](crate::combine::combine) gave no estimate.
    NoEstimate(combine::Error),
    /// Only sources with an unknown bound agree, and an unknown estimate never replaces
    /// a known one.
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

    fn cause() -> impl Strategy<Value = Cause> {
        prop_oneof![
            error().prop_map(Cause::NoEstimate),
            Just(Cause::UnknownEstimate)
        ]
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

    /// A discipline that has a slew from `slew`, and the slew.
    fn slewing<S: Strategy<Value = Slew>>(
        slew: impl Fn() -> S,
    ) -> impl Strategy<Value = (Discipline, Slew)> {
        prop_oneof![
            slew().prop_map(|s| (Discipline::Synced(s), s)),
            (slew(), cause()).prop_map(|(s, c)| (Discipline::Holdover(s, c), s)),
        ]
    }

    fn drift() -> impl Strategy<Value = Drift> {
        (0..=100_000_000_u32).prop_map(|ppb| Drift::from_ppb(ppb).expect("valid"))
    }

    proptest! {
        #[test]
        fn serves_the_first_estimate_at_once(
            error in error(),
            m in prop_oneof![target(), unknown()],
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let change = Discipline::Unsynced(error).next(Ok(m)).expect("a change");
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
            let change = Discipline::Unsynced(old).next(Err(new));
            let changed = (old != new).then_some(Discipline::Unsynced(new));
            prop_assert_eq!(change.map(|c| c.at(Monotonic(now), drift)), changed);
        }

        #[test]
        fn holds_the_slew_over_with_no_estimate(
            (old, slew) in slewing(any_slew),
            error in error(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let holdover = Discipline::Holdover(slew, Cause::NoEstimate(error));
            let changed = (old != holdover).then_some(holdover);
            let change = old.next(Err(error));
            prop_assert_eq!(change.map(|c| c.at(Monotonic(now), drift)), changed);
        }

        #[test]
        fn slews_toward_a_later_known_estimate(
            (old, slew) in slewing(any_slew),
            m in target(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let now = Monotonic(now);
            let change = old.next(Ok(m)).expect("a change");
            let synced = Discipline::Synced(slew.toward(now, drift, m));
            prop_assert_eq!(change.at(now, drift), synced);
        }

        #[test]
        fn holds_a_known_slew_over_with_an_unknown_estimate(
            (old, slew) in slewing(slew),
            m in unknown(),
            again in unknown(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let holdover = Discipline::Holdover(slew, Cause::UnknownEstimate);
            let changed = (old != holdover).then_some(holdover);
            let change = old.next(Ok(m));
            prop_assert_eq!(change.map(|c| c.at(Monotonic(now), drift)), changed);
            prop_assert_eq!(holdover.next(Ok(again)), None);
        }

        #[test]
        fn slews_from_an_unknown_slew_toward_an_unknown_estimate(
            (old, slew) in slewing(unknown_slew),
            m in unknown(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let now = Monotonic(now);
            let change = old.next(Ok(m)).expect("a change");
            let synced = Discipline::Synced(slew.toward(now, drift, m));
            prop_assert_eq!(change.at(now, drift), synced);
        }
    }
}
