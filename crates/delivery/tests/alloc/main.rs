//! A put and a take make no heap allocation after the first put, and none after a later
//! open. This binary has no test harness: the count covers each thread, and a harness
//! allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use delivery::{Key, Readers};
use types::channel::Slot;
use types::frame::key_set::{Group, Interner};
use types::frame::{Draft, Form, Frame, Path};
use types::time::Stamp;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const SESSIONS: usize = 16;

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );

    let set = Interner::new().intern(&[Group {
        index: Slot::new(1),
        data: &[],
    }]);
    let config = block::Config { budget: 1 << 16 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    let frame = || {
        Draft::new(&pool, &set, Path::Live, Form::Raw, &[(0, 8)])
            .expect("the pool holds the frame")
            .freeze()
    };
    let mut readers = Readers::new();
    let open =
        |readers: &mut Readers| readers.open_latest(None, Stamp::from_nanos(0)).key;
    let mut keys: Vec<_> = (0..SESSIONS).map(|_| open(&mut readers)).collect();
    assert_eq!(
        readers.put(frame()).len(),
        SESSIONS,
        "the first put wakes all"
    );
    let (delivered, allocations) = ALLOCATOR.count(|| {
        (0..4)
            .map(|_| round(&mut readers, &keys, frame))
            .sum::<usize>()
    });
    assert_eq!(allocations, 0, "the hot path allocated");
    assert_eq!(
        delivered,
        8 * SESSIONS,
        "each round takes and wakes every session"
    );

    keys.push(open(&mut readers));
    let (delivered, allocations) =
        ALLOCATOR.count(|| round(&mut readers, &keys, frame));
    assert_eq!(allocations, 0, "the hot path allocated after an open");
    assert_eq!(
        delivered,
        2 * (SESSIONS + 1),
        "the round takes and wakes the new session"
    );
}

/// Takes each session's frame, then puts two frames. Returns the frames taken plus the
/// sessions woken.
fn round(readers: &mut Readers, keys: &[Key], frame: impl Fn() -> Frame) -> usize {
    let taken: usize = keys
        .iter()
        .map(|&key| usize::from(readers.take(key).is_some()))
        .sum();
    taken + readers.put(frame()).len() + readers.put(frame()).len()
}
