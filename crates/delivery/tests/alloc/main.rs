//! For latest sessions, a put and a take make no heap allocation after the first put,
//! and none after a later open. For complete sessions, a queue, a release, and a take
//! make none once each session got a frame. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use delivery::{Position, Reader, Readers, Start, complete, latest};
use types::channel;
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
        index: channel::Key::from_u128(1),
        data: &[],
    }]);
    let config = block::Config { budget: 1 << 16 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    let frame = || {
        Draft::new(&pool, &set, Form::Raw, &[(0, 8)])
            .expect("the pool holds the frame")
            .freeze(Path::Live)
    };
    latest(&frame);
    complete(&frame);
}

fn latest(frame: &impl Fn() -> Frame) {
    let mut readers = Readers::new(0);
    let open =
        |readers: &mut Readers| readers.open_latest(None, Stamp::from_nanos(0)).key;
    let mut keys: Vec<_> = (0..SESSIONS).map(|_| open(&mut readers)).collect();
    assert_eq!(
        readers.put(frame(), 0..1).len(),
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

fn complete(frame: &impl Fn() -> Frame) {
    let mut readers = Readers::new(0);
    let open = |readers: &mut Readers, live: u64| {
        let start = Start::At(Position {
            live,
            backfill: None,
        });
        readers.open(Reader::Unnamed, start, u64::MAX).key
    };
    let mut keys: Vec<_> = (0..SESSIONS).map(|_| open(&mut readers, 0)).collect();
    let mut seq = 0;
    assert_eq!(
        flow(&mut readers, &keys, frame, &mut seq),
        SESSIONS,
        "the first release wakes all"
    );
    let (delivered, allocations) = ALLOCATOR.count(|| {
        (0..4)
            .map(|_| flow(&mut readers, &keys, frame, &mut seq))
            .sum::<usize>()
    });
    assert_eq!(allocations, 0, "the live path allocated");
    assert_eq!(
        delivered,
        12 * SESSIONS,
        "each round takes two frames from and wakes every session"
    );

    keys.push(open(&mut readers, seq));
    let (delivered, allocations) = ALLOCATOR.count(|| {
        (0..2)
            .map(|_| flow(&mut readers, &keys, frame, &mut seq))
            .sum::<usize>()
    });
    assert_eq!(allocations, 0, "the live path allocated after an open");
    assert_eq!(
        delivered,
        6 * SESSIONS + 4,
        "two rounds wake every session and take from the new one once"
    );
}

/// Takes each session's frames, then queues and releases two frames from `seq`.
/// Returns the frames taken plus the sessions woken.
fn flow(
    readers: &mut Readers,
    keys: &[complete::Key],
    frame: &impl Fn() -> Frame,
    seq: &mut u64,
) -> usize {
    let taken: usize = keys
        .iter()
        .map(|&key| std::iter::from_fn(|| readers.take(key.into())).count())
        .sum();
    for _ in 0..2 {
        readers.queue(&frame(), *seq..*seq + 1);
        *seq += 1;
    }
    taken + readers.release(*seq).len()
}

/// Takes each session's frame, then puts two frames. Returns the frames taken plus the
/// sessions woken.
fn round(
    readers: &mut Readers,
    keys: &[latest::Key],
    frame: &impl Fn() -> Frame,
) -> usize {
    let taken: usize = keys
        .iter()
        .map(|&key| usize::from(readers.take(key.into()).is_some()))
        .sum();
    taken + readers.put(frame(), 0..1).len() + readers.put(frame(), 0..1).len()
}
