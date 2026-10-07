//! For latest sessions, a put and a take make no heap allocation after the first put,
//! and none after a later open. For complete sessions, a queue, a release, and a take
//! make none once each session got a frame, nor a release that wakes sessions that
//! missed a frame. An ack makes none, and no call on a closed key of either mode makes
//! one. This binary has no test harness: the count covers each
//! thread, and a harness allocates on its own thread at any time.

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
    missed(&frame);
}

fn latest(frame: &impl Fn() -> Frame) {
    let mut readers = Readers::new(0);
    let open = |readers: &mut Readers| readers.open_latest().key;
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

    let closed = keys[0];
    readers.close(closed.into());
    let ((), allocations) = ALLOCATOR.count(|| {
        assert!(readers.take(closed.into()).is_none(), "a closed key takes");
        readers.close(closed.into());
    });
    assert_eq!(allocations, 0, "a call on a closed latest key allocated");
}

fn complete(frame: &impl Fn() -> Frame) {
    let mut readers = Readers::new(0);
    let mut keys: Vec<_> = (0..SESSIONS)
        .map(|_| open(&mut readers, 0, u64::MAX))
        .collect();
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

    keys.push(open(&mut readers, seq, u64::MAX));
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

    let position = Position {
        live: seq,
        backfill: None,
    };
    let closed = keys.pop().expect("a session is open");
    readers.close(closed.into());
    let ((), allocations) = ALLOCATOR.count(|| {
        for &key in &keys {
            assert_eq!(readers.ack(key, position), Ok(()), "the ack moves forward");
        }
        readers.grant(closed, u64::MAX);
        assert_eq!(readers.ack(closed, position), Ok(()), "a closed key acks");
        assert!(readers.take(closed.into()).is_none(), "a closed key takes");
        readers.close(closed.into());
        readers.close_named(closed, Stamp::from_nanos(0));
    });
    assert_eq!(allocations, 0, "an ack or a call on a closed key allocated");
    assert_eq!(
        readers.floor(),
        Some(position),
        "each ack reached its session"
    );
}

/// Opens a complete session at live seq `live` with credit for `limit_bytes`.
fn open(readers: &mut Readers, live: u64, limit_bytes: u64) -> complete::Key {
    let start = Start::At(Position {
        live,
        backfill: None,
    });
    readers.open(Reader::Unnamed, start, limit_bytes).key
}

fn missed(frame: &impl Fn() -> Frame) {
    let mut readers = Readers::new(0);
    let warm = [open(&mut readers, 0, u64::MAX)];
    let mut seq = 0;
    for _ in 0..2 {
        flow(&mut readers, &warm, frame, &mut seq);
    }
    let keys: Vec<_> = (0..SESSIONS).map(|_| open(&mut readers, seq, 0)).collect();
    let (woken, allocations) =
        ALLOCATOR.count(|| flow(&mut readers, &warm, frame, &mut seq));
    assert_eq!(
        allocations, 0,
        "a release that wakes a missed session allocated"
    );
    assert_eq!(
        woken,
        SESSIONS + 3,
        "the warm session takes two frames, and the release wakes each session"
    );
    assert!(
        keys.iter().all(|&key| readers.behind(key)),
        "each session missed"
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
    taken + readers.put(frame()).len() + readers.put(frame()).len()
}
