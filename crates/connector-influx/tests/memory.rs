//! A store of the lab's data lines holds each point in a few tens of bytes, so the
//! STORE AND FORWARD scenario fits a CI runner. The count covers each thread, so this
//! binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::io::Write as _;

use connector_influx::sim::Store;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const POINTS: usize = 1_000_000;

/// The most heap bytes a point may take: 8 of time, 8 of value, and 1 of presence,
/// with room for the chunks.
const BUDGET: usize = 32;

fn main() {
    let mut store = Store::default();
    let mut body = Vec::new();
    let before = ALLOCATOR.held();
    for k in 0..POINTS {
        writeln!(
            body,
            "edge.value,node=edge,unit=V value={k} {}",
            1_000_000 + k * 1_000
        )
        .expect("a Vec takes each write");
        if body.len() > 1 << 20 {
            store.write(&body).expect("valid lines");
            body.clear();
        }
    }
    store.write(&body).expect("valid lines");
    drop(body);
    let held = ALLOCATOR.held().strict_sub(before);
    assert_eq!(
        store.points("edge.value", &[]).count(),
        POINTS,
        "each point is stored"
    );
    assert!(
        held <= BUDGET * POINTS,
        "the store holds {held} bytes, more than {BUDGET} a point"
    );
}
