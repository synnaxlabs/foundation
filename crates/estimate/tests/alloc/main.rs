//! Reading mesh time from a slew makes no heap allocation. This binary has no test
//! harness: the count covers each thread, and a harness allocates on its own thread at
//! any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use estimate::{Drift, Measurement, Slew};
use types::time::{Monotonic, Span};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );

    let estimate = |offset| Measurement::new(Monotonic(0), offset, Span::MICROSECOND);
    let first = estimate(Span::ZERO).expect("valid");
    let target = estimate(Span::MILLISECOND).expect("valid");
    let (m, allocations) = ALLOCATOR.count(|| {
        let slew = Slew::new(first).toward(Monotonic(0), target);
        slew.at(Monotonic(1_000_000_000), Drift::UNDISCIPLINED)
    });
    assert_eq!(allocations, 0, "the hot path allocated");
    let m = m.expect("a valid estimate");
    assert_eq!(
        (m.offset(), m.error()),
        (Span::from_nanos(500_000), Span::from_nanos(701_000)),
        "the slew reads back mid-way"
    );
}
