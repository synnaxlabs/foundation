//! Strategies for property tests: a simulated true offset, measurements, and slews.

use proptest::collection::vec;
use proptest::prelude::*;
use types::time::{Monotonic, Span};

use crate::{Drift, Measurement, Slew};

pub(crate) const TIME_NS: u64 = 1 << 40;
pub(crate) const ERROR_NS: i64 = 1 << 30;
const OFFSET_NS: i64 = 1 << 60;

/// The widest offset of [`target`] and [`slew`], small enough that every estimate from
/// them is valid.
pub(crate) const SLEW_OFFSET_NS: i64 = 1 << 50;

/// A true offset that starts at `start` at local time zero and moves at `rate` parts
/// per billion, never faster than `drift`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct World {
    pub(crate) drift: Drift,
    rate: i64,
    start: i64,
    pub(crate) now: u64,
}

impl World {
    pub(crate) fn truth(self, t: u64) -> i128 {
        let moved = (i128::from(self.rate) * i128::from(t)).div_euclid(1_000_000_000);
        i128::from(self.start) + moved
    }

    /// Whether `m`, widened to `t`, holds the true offset at `t`.
    pub(crate) fn holds_truth_at(self, m: Measurement, t: u64) -> bool {
        let miss = i128::from(m.offset().nanos()) - self.truth(t);
        miss.abs() <= i128::from(m.error_at(Monotonic(t), self.drift).nanos())
    }
}

pub(crate) fn nanos(n: i128) -> Span {
    Span::from_nanos(i64::try_from(n).expect("test values fit in i64"))
}

pub(crate) fn world() -> impl Strategy<Value = World> {
    (0..=1_000_000_u32, -OFFSET_NS..OFFSET_NS, 0..TIME_NS).prop_flat_map(
        |(ppb, start, now)| {
            let most = i64::from(ppb);
            (-most..=most).prop_map(move |rate| World {
                drift: Drift::from_ppb(ppb).expect("valid"),
                rate,
                start,
                now,
            })
        },
    )
}

/// A measurement whose bound holds the true offset at its own time.
pub(crate) fn truechimer(world: World) -> impl Strategy<Value = Measurement> {
    (0..TIME_NS, 0..ERROR_NS).prop_flat_map(move |(at, error)| {
        (-error..=error).prop_map(move |slack| {
            let offset = nanos(world.truth(at) + i128::from(slack));
            let error = Span::from_nanos(error);
            Measurement::new(Monotonic(at), offset, error).expect("valid")
        })
    })
}

/// A world and 1 to 9 measurements that hold its true offset.
pub(crate) fn agreeing() -> impl Strategy<Value = (World, Vec<Measurement>)> {
    world().prop_flat_map(|w| (Just(w), vec(truechimer(w), 1..10)))
}

/// 1 to 9 measurements at any time, with offsets and errors up to the given sizes.
pub(crate) fn any_measurements(
    offset_ns: i64,
    error_ns: i64,
) -> impl Strategy<Value = Vec<Measurement>> {
    let one =
        (any::<u64>(), -offset_ns..=offset_ns, 0..=error_ns).prop_map(|(at, o, e)| {
            let (offset, error) = (Span::from_nanos(o), Span::from_nanos(e));
            Measurement::new(Monotonic(at), offset, error).expect("valid")
        });
    vec(one, 1..10)
}

/// A target for a slew.
pub(crate) fn target() -> impl Strategy<Value = Measurement> {
    let parts = (0..TIME_NS, -SLEW_OFFSET_NS..SLEW_OFFSET_NS, 0..ERROR_NS);
    parts.prop_map(|(at, offset, error)| {
        let (offset, error) = (Span::from_nanos(offset), Span::from_nanos(error));
        Measurement::new(Monotonic(at), offset, error).expect("valid")
    })
}

pub(crate) fn slew() -> impl Strategy<Value = Slew> {
    (0..TIME_NS, -SLEW_OFFSET_NS..SLEW_OFFSET_NS, target()).prop_map(
        |(start, from, target)| Slew {
            start: Monotonic(start),
            from: Span::from_nanos(from),
            target,
        },
    )
}
