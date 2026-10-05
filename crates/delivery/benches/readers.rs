//! The cost of one acknowledgment, the only per-message input, and of one floor, which
//! the home reads when `buffer` trims.

use delivery::{Key, Position, Reader, Readers, Start};
use divan::Bencher;

fn main() {
    divan::main();
}

/// `sessions` recording readers.
fn opened(sessions: usize) -> (Readers, Vec<Key>) {
    let start = Start::At(Position {
        live: 0,
        backfill: Some(0),
    });
    let mut readers = Readers::new();
    let keys = (0..sessions)
        .map(|_| readers.open(Reader::Unnamed, start).key)
        .collect();
    (readers, keys)
}

/// The last of `sessions` recording readers acknowledges.
#[divan::bench(args = [1, 16])]
fn ack(bencher: Bencher<'_, '_>, sessions: usize) {
    let (mut readers, keys) = opened(sessions);
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

#[divan::bench(args = [1, 16, 256])]
fn floor(bencher: Bencher<'_, '_>, sessions: usize) {
    let (readers, _) = opened(sessions);
    bencher.bench_local(|| divan::black_box(&readers).floor());
}
