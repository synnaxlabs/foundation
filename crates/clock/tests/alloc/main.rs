//! Reading mesh time or the status makes no heap allocation. This binary has no test
//! harness: the count covers each thread, and a harness allocates on its own thread at
//! any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use clock::{Clock, Status};
use estimate::Measurement;
use estimate::combine::Error;
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );

    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    let first = Measurement::new(node.clock().now(), Span::HOUR, Span::MILLISECOND);
    let first = first.expect("at most 36500 days");
    clock.push(source, first);
    let (interval, allocations) = ALLOCATOR.count(|| reader.now());
    assert_eq!(allocations, 0, "the hot path allocated");
    assert_eq!(
        interval,
        Some(first.interval()),
        "the reader reads mesh time"
    );

    let _ = clock.add();
    let (interval, allocations) = ALLOCATOR.count(|| reader.now());
    assert_eq!(allocations, 0, "the hot path allocated in holdover");
    assert_eq!(
        interval,
        Some(first.interval()),
        "the reader reads mesh time in holdover"
    );
    let (status, allocations) = ALLOCATOR.count(|| reader.status());
    assert_eq!(allocations, 0, "the status read allocated");
    let alone = Error::NoMajority {
        sources: 2,
        agreeing: 1,
        empty: 1,
    };
    assert_eq!(
        status,
        Status::Holdover(first, alone),
        "the reader reads the status"
    );
}
