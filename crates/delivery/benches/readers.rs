//! The cost of one acknowledgment, which runs per message, of one live frame to the
//! complete readers, which runs per frame, and of one floor, which the home reads when
//! `buffer` trims.

use delivery::complete::Key;
use delivery::{Position, Reader, Readers, Start};
use divan::Bencher;
use types::channel;
use types::frame::key_set::{Group, Interner};
use types::frame::{Draft, Form, Frame, Path};

fn main() {
    divan::main();
}

/// `sessions` recording readers, each with credit for `limit_bytes`.
fn opened(sessions: usize, limit_bytes: u64) -> (Readers, Vec<Key>) {
    let start = Start::At(Position {
        live: 0,
        backfill: Some(0),
    });
    let mut readers = Readers::new(0);
    let keys = (0..sessions)
        .map(|_| readers.open(Reader::Unnamed, start, limit_bytes).key)
        .collect();
    (readers, keys)
}

/// The last of `sessions` recording readers acknowledges.
#[divan::bench(args = [1, 16])]
fn ack(bencher: Bencher<'_, '_>, sessions: usize) {
    let (mut readers, keys) = opened(sessions, 0);
    let key = keys[sessions - 1];
    let mut seq = 0;
    bencher.bench_local(|| {
        seq += 1;
        let position = Position {
            live: seq,
            backfill: Some(seq),
        };
        readers.ack(divan::black_box(key), position)
    });
}

/// A frame of one index.
fn frame() -> Frame {
    let set = Interner::new().intern(&[Group {
        index: channel::Key::from_u128(1),
        data: &[],
    }]);
    let config = block::Config { budget: 1 << 16 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    Draft::new(&pool, &set, Form::Raw, &[(0, 8)])
        .expect("the pool holds the frame")
        .freeze(Path::Live)
}

/// One live frame to `sessions` recording readers that keep up: the home queues it,
/// releases it once it is on disk, and each session takes it.
#[divan::bench(args = [1, 16, 256])]
fn release(bencher: Bencher<'_, '_>, sessions: usize) {
    let (mut readers, keys) = opened(sessions, u64::MAX);
    let frame = frame();
    let mut seq = 0;
    bencher.bench_local(|| {
        readers.queue(&frame, seq..seq + 1);
        seq += 1;
        divan::black_box(readers.release(seq));
        for &key in &keys {
            divan::black_box(readers.take(key.into()));
        }
    });
}

#[divan::bench(args = [1, 16, 256])]
fn floor(bencher: Bencher<'_, '_>, sessions: usize) {
    let (readers, _) = opened(sessions, 0);
    bencher.bench_local(|| divan::black_box(&readers).floor());
}
