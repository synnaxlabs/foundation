//! What mesh time follows, and how each estimate changes it.

use types::time::Monotonic;

use crate::{Drift, Measurement, Slew, combine};

/// What mesh time follows: no time before the first estimate, a slew toward the
/// estimate of a majority, or the last slew while no majority agrees.
///
/// ```
/// use estimate::combine::Error;
/// use estimate::discipline::Discipline;
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
/// let kept = Discipline::Holdover(Slew::new(m), Error::NoSources);
/// assert_eq!(holdover.at(now, drift), kept);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discipline {
    /// No majority of the sources has agreed yet, so there is no mesh time. Holds why.
    Unsynced(combine::Error),
    /// Mesh time slews toward the estimate of a majority.
    Synced(Slew),
    /// No majority agrees now. Mesh time keeps its slew, and its error grows by drift.
    /// Holds why.
    Holdover(Slew, combine::Error),
}

impl Discipline {
    /// The change that `estimate`, a result of [`combine::combine`], makes. The first
    /// estimate is served at once, and later ones are slewed toward. An error keeps
    /// the slew in holdover, or stays unsynced before the first estimate. `None` when
    /// the discipline stays the same. A slew toward an estimate is always a change.
    #[must_use]
    pub fn next(self, estimate: Result<Measurement, combine::Error>) -> Option<Next> {
        let slew = match self {
            Self::Unsynced(_) => None,
            Self::Synced(slew) | Self::Holdover(slew, _) => Some(slew),
        };
        let change = match (slew, estimate) {
            (None, Err(e)) => Change::To(Self::Unsynced(e)),
            (Some(slew), Err(e)) => Change::To(Self::Holdover(slew, e)),
            (None, Ok(m)) => Change::To(Self::Synced(Slew::new(m))),
            (Some(slew), Ok(m)) => Change::Toward(slew, m),
        };
        (change != Change::To(self)).then_some(Next(change))
    }
}

/// A change of [`Discipline`] that takes effect at a local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Next(Change);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// The same at any local time.
    To(Discipline),
    /// A slew toward a new target.
    Toward(Slew, Measurement),
}

impl Next {
    /// The discipline from `now` on, for a local clock that drifts from mesh time by
    /// at most `drift`. Mesh time from it at or after `now` is never earlier than mesh
    /// time from the last discipline at or before `now`.
    #[must_use]
    pub fn at(self, now: Monotonic, drift: Drift) -> Discipline {
        match self.0 {
            Change::To(discipline) => discipline,
            Change::Toward(slew, target) => {
                Discipline::Synced(slew.toward(now, drift, target))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use types::time::Monotonic;

    use super::Discipline;
    use crate::combine::Error;
    use crate::world::{TIME_NS, slew, target};
    use crate::{Drift, Slew};

    /// Few counts, so two causes are often equal.
    fn cause() -> impl Strategy<Value = Error> {
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

    /// A discipline that has a slew, and the slew.
    fn slewing() -> impl Strategy<Value = (Discipline, Slew)> {
        prop_oneof![
            slew().prop_map(|s| (Discipline::Synced(s), s)),
            (slew(), cause()).prop_map(|(s, e)| (Discipline::Holdover(s, e), s)),
        ]
    }

    fn drift() -> impl Strategy<Value = Drift> {
        (0..=100_000_000_u32).prop_map(|ppb| Drift::from_ppb(ppb).expect("valid"))
    }

    proptest! {
        #[test]
        fn serves_the_first_estimate_at_once(
            cause in cause(),
            m in target(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let next = Discipline::Unsynced(cause).next(Ok(m)).expect("a change");
            let synced = Discipline::Synced(Slew::new(m));
            prop_assert_eq!(next.at(Monotonic(now), drift), synced);
        }

        #[test]
        fn stays_unsynced_with_no_estimate(
            old in cause(),
            new in cause(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let next = Discipline::Unsynced(old).next(Err(new));
            let changed = (old != new).then_some(Discipline::Unsynced(new));
            prop_assert_eq!(next.map(|n| n.at(Monotonic(now), drift)), changed);
        }

        #[test]
        fn holds_the_slew_over_with_no_estimate(
            (old, slew) in slewing(),
            cause in cause(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let holdover = Discipline::Holdover(slew, cause);
            let changed = (old != holdover).then_some(holdover);
            let next = old.next(Err(cause));
            prop_assert_eq!(next.map(|n| n.at(Monotonic(now), drift)), changed);
        }

        #[test]
        fn slews_toward_a_later_estimate(
            (old, slew) in slewing(),
            m in target(),
            now in 0..TIME_NS,
            drift in drift(),
        ) {
            let now = Monotonic(now);
            let next = old.next(Ok(m)).expect("a change");
            let synced = Discipline::Synced(slew.toward(now, drift, m));
            prop_assert_eq!(next.at(now, drift), synced);
        }
    }
}
