//! A connector of the `opcua` kind whose writes pass the room of the ring of its home
//! ends its call with `Ok` in bounded sim steps. Each commit trims the ring, so no
//! write is lost, and the last status is the end of the run.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

#[path = "common/kind.rs"]
#[expect(dead_code, reason = "this binary runs only the writes")]
mod kind;

use std::collections::BTreeMap;

use connector::status::State;
use types::sample::Scalar;

#[test]
fn a_run_that_writes_past_the_ring_of_its_home_returns_ok() {
    let (writes, status) = kind::writes(
        256,
        &[Scalar::F64],
        8_000,
        |_| 0,
        |work| {
            work();
            0
        },
    );
    assert_eq!(writes.len(), 8_000, "each write of the run");
    let last = status.last().expect("the home applied a status");
    let counts = [
        ("bad", 0),
        ("in", 2_048_000),
        ("lost", 0),
        ("missed", 0),
        ("mistyped", 0),
        ("uncertain", 0),
    ];
    let counts = counts.map(|(count, n)| (count.parse().expect("a name"), n));
    assert_eq!(last.state, State::Stopped);
    assert_eq!(last.counts, BTreeMap::from(counts));
}
