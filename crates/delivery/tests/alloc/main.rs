//! For latest sessions, a put and a take make no heap allocation after the first put,
//! and none after a later open. For complete sessions, a queue, a release, and a take
//! make none once each session got a frame, also for sessions charged by their places.
//! A release in which sessions start to wait for credit makes none, nor one in which
//! they miss. An ack makes none, and no call on a closed key of either mode makes one.
//! This binary has no test harness: the count covers each thread, and a harness
//! allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::sync::Arc;

use delivery::{Next, Position, Reader, Readers, Start, complete, latest};

use types::channel;
use types::frame::key_set::{Group, Interner, KeySet};
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
    complete(&frame, &set);
    places(&frame, &set);
    alternating();
    missed(&frame, &set);
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
        assert!(
            matches!(readers.take(closed.into()), Next::Empty),
            "a closed key takes"
        );
        readers.close(closed.into());
    });
    assert_eq!(allocations, 0, "a call on a closed latest key allocated");
}

fn complete(frame: &impl Fn() -> Frame, set: &Arc<KeySet>) {
    let mut readers = Readers::new(0);
    let mut keys: Vec<_> = (0..SESSIONS)
        .map(|_| open(&mut readers, 0, u64::MAX))
        .collect();
    let mut seq = 0;
    assert_eq!(
        flow(&mut readers, &keys, frame, set, &mut seq),
        SESSIONS,
        "the first release wakes all"
    );
    let (delivered, allocations) = ALLOCATOR.count(|| {
        (0..4)
            .map(|_| flow(&mut readers, &keys, frame, set, &mut seq))
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
            .map(|_| flow(&mut readers, &keys, frame, set, &mut seq))
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
        assert!(
            matches!(readers.take(closed.into()), Next::Empty),
            "a closed key takes"
        );
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
    readers
        .open(Reader::Unnamed, start, limit_bytes, complete::Charge::Whole)
        .key
}

/// Sessions charged by their places allocate only for the first frame of a key set.
fn places(frame: &impl Fn() -> Frame, set: &Arc<KeySet>) {
    let mut readers = Readers::new(0);
    let start = Start::At(Position {
        live: 0,
        backfill: None,
    });
    let slot = set.entries()[0].slot;
    let keys: Vec<_> = (0..SESSIONS)
        .map(|_| {
            let charge = complete::Charge::Places([slot, slot].into());
            readers.open(Reader::Unnamed, start, u64::MAX, charge).key
        })
        .collect();
    let mut seq = 0;
    flow(&mut readers, &keys, frame, set, &mut seq);
    let (delivered, allocations) = ALLOCATOR.count(|| {
        (0..4)
            .map(|_| flow(&mut readers, &keys, frame, set, &mut seq))
            .sum::<usize>()
    });
    assert_eq!(allocations, 0, "the live path allocated for places");
    assert_eq!(
        delivered,
        12 * SESSIONS,
        "each round takes two frames from and wakes every session"
    );
}

/// Sessions that never waited for credit start to wait in a release, and miss in the
/// next: neither allocates, so the frames that wait take no heap per session.
fn missed(frame: &impl Fn() -> Frame, set: &Arc<KeySet>) {
    let mut readers = Readers::new(0);
    let warm = [open(&mut readers, 0, u64::MAX)];
    let mut seq = 0;
    flow(&mut readers, &warm, frame, set, &mut seq);
    // The queue holds four frames at once: two that wait, and two queued after them.
    for _ in 0..4 {
        readers.queue(&frame(), set, seq..seq + 1);
        seq += 1;
    }
    assert!(
        readers.release(seq).is_empty(),
        "the warm session has frames to take"
    );
    // A session of credit 0 waits from the first frame, one of credit 1 from the
    // second.
    let keys: Vec<_> = (0..SESSIONS)
        .map(|i| open(&mut readers, seq, u64::from(i % 2 == 1)))
        .collect();
    let all: Vec<_> = warm.iter().chain(&keys).copied().collect();
    let ((owed, missed), allocations) = ALLOCATOR.count(|| {
        let owed = flow(&mut readers, &all, frame, set, &mut seq);
        (owed, flow(&mut readers, &warm, frame, set, &mut seq))
    });
    assert_eq!(
        allocations, 0,
        "a release that makes frames wait for credit, or that misses them, allocated"
    );
    assert_eq!(
        owed,
        6 + 1 + SESSIONS / 2,
        "the warm session takes six frames, and the release wakes it and each \
         session of credit 1"
    );
    assert_eq!(
        missed,
        2 + 1 + SESSIONS / 2,
        "the warm session takes two frames, and the release wakes it and each \
         session of credit 0"
    );
    let drained_behind = |readers: &mut Readers, key: complete::Key| loop {
        match readers.take(key.into()) {
            Next::Frame(_) => {}
            Next::Behind => break true,
            Next::Empty => break false,
        }
    };
    assert!(
        keys.iter().all(|&key| drained_behind(&mut readers, key)),
        "each session missed"
    );
}

/// Takes each session's frames, then queues and releases two frames from `seq`.
/// Returns the frames taken plus the sessions woken.
fn flow(
    readers: &mut Readers,
    keys: &[complete::Key],
    frame: &impl Fn() -> Frame,
    set: &Arc<KeySet>,
    seq: &mut u64,
) -> usize {
    let taken: usize = keys.iter().map(|&key| taken(readers, key)).sum();
    for _ in 0..2 {
        readers.queue(&frame(), set, *seq..*seq + 1);
        *seq += 1;
    }
    taken + readers.release(*seq).len()
}

/// The number of frames that the complete session `key` takes before
/// [`Next::Empty`].
///
/// # Panics
///
/// If the session gets [`Next::Behind`].
fn taken(readers: &mut Readers, key: complete::Key) -> usize {
    let mut count = 0;
    loop {
        match readers.take(key.into()) {
            Next::Frame(_) => count += 1,
            Next::Empty => return count,
            Next::Behind => panic!("the session is behind"),
        }
    }
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
        .map(|&key| usize::from(matches!(readers.take(key.into()), Next::Frame(_))))
        .sum();
    taken + readers.put(frame()).len() + readers.put(frame()).len()
}

/// Two writers of two key sets on one index: each key set's first frame is past, so
/// a release allocates nothing more.
fn alternating() {
    const F64: types::sample::Type =
        types::sample::Type::Scalar(types::sample::Scalar::F64);
    let mut interner = Interner::new();
    let index = channel::Key::from_u128(1);
    let a = interner.intern(&[Group {
        index,
        data: &[(channel::Key::from_u128(2), F64)],
    }]);
    let b = interner.intern(&[Group {
        index,
        data: &[(channel::Key::from_u128(3), F64)],
    }]);
    let config = block::Config { budget: 1 << 16 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    let frame = |set: &KeySet| {
        Draft::new(&pool, set, Form::Raw, &[(0, 8), (1, 8)])
            .expect("the pool holds the frame")
            .freeze(Path::Live)
    };
    let mut readers = Readers::new(0);
    let start = Start::At(Position {
        live: 0,
        backfill: None,
    });
    let slot = a.entries()[0].slot;
    let key = readers
        .open(
            Reader::Unnamed,
            start,
            u64::MAX,
            complete::Charge::Places([slot].into()),
        )
        .key;
    let mut seq = 0;
    let step = |readers: &mut Readers, seq: &mut u64| {
        let (fa, fb) = (frame(&a), frame(&b));
        readers.queue(&fa, &a, *seq..*seq + 1);
        readers.queue(&fb, &b, *seq + 1..*seq + 2);
        *seq += 2;
        let woken = readers.release(*seq).len();
        woken + taken(readers, key)
    };
    for _ in 0..2 {
        step(&mut readers, &mut seq);
    }
    let (delivered, allocations) =
        ALLOCATOR.count(|| (0..4).map(|_| step(&mut readers, &mut seq)).sum::<usize>());
    assert_eq!(
        delivered, 12,
        "each round wakes the session and takes two frames"
    );
    assert_eq!(
        allocations, 0,
        "a release of frames of two key sets, each seen before, allocated"
    );
}
