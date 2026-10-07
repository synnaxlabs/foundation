//! A write from the holder, a read of the handoff, and a record make no heap
//! allocation. This binary has no test harness: the count covers each thread, and
//! a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use control::lease::Lease;
use control::{Gate, Key, Writer};
use types::authority::Authority;
use types::time::{Monotonic, Span};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const FRAMES: u64 = 64;

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );

    let writer = |subject: &str| Writer {
        subject: subject.parse().expect("valid name"),
        authority: Authority(100),
    };
    let lease = Lease::new(Span::SECOND).expect("positive lease");
    let mut gate = Gate::new();
    let key = gate.open(writer("plc.valve"), Some(lease), Monotonic(0));
    gate.open(writer("plc.backup"), None, Monotonic(0));

    let (waiting, allocations) = ALLOCATOR.count(|| frames(&mut gate, key, 0));
    assert_eq!(allocations, 0, "a frame allocated while a handoff waits");
    assert_eq!(waiting, FRAMES, "the handoff waits until recorded");

    let ((), allocations) = ALLOCATOR.count(|| gate.recorded());
    assert_eq!(allocations, 0, "a record allocated");

    let (waiting, allocations) = ALLOCATOR.count(|| frames(&mut gate, key, FRAMES));
    assert_eq!(allocations, 0, "a frame allocated after the record");
    assert_eq!(waiting, 0, "no handoff waits after the record");
}

/// Writes `FRAMES` frames from the holder after `start`, as the home does for each
/// accepted frame. Returns how many found a handoff waiting.
fn frames(gate: &mut Gate, key: Key, start: u64) -> u64 {
    (start + 1..=start + FRAMES)
        .map(|now| {
            let permit = gate.check(key, Monotonic(now)).expect("the holder writes");
            gate.renew(permit);
            u64::from(gate.handoff().is_some())
        })
        .sum()
}
