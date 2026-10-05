//! A put and a take make no heap allocation after the first put. This binary has no
//! test harness: the count covers each thread, and a harness allocates on its own
//! thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use delivery::Latest;
use types::channel::Slot;
use types::frame::key_set::{Group, Interner};
use types::frame::{Draft, Form, Path};

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
    let mut latest = Latest::new();
    let keys: Vec<_> = (0..SESSIONS).map(|_| latest.open()).collect();
    assert_eq!(
        latest.put(frame()).len(),
        SESSIONS,
        "the first put wakes all"
    );
    let (delivered, allocations) = ALLOCATOR.count(|| {
        let mut delivered = 0;
        for _ in 0..4 {
            for &key in &keys {
                delivered += usize::from(latest.take(key).is_some());
            }
            delivered += latest.put(frame()).len();
            delivered += latest.put(frame()).len();
        }
        delivered
    });
    assert_eq!(allocations, 0, "the hot path allocated");
    assert_eq!(
        delivered,
        8 * SESSIONS,
        "each round takes and wakes every session"
    );
}
