//! The cost of one acknowledgment, the only per-message input.

use delivery::{Position, Reader, Readers, Start};
use divan::Bencher;
use types::time::Stamp;

fn main() {
    divan::main();
}

/// The last of `sessions` recording readers acknowledges.
#[divan::bench(args = [1, 16])]
fn ack(bencher: Bencher<'_, '_>, sessions: usize) {
    let start = Start::At(Position {
        live: 0,
        backfill: Some(0),
    });
    let mut readers = Readers::new();
    let keys: Vec<_> = (0..sessions)
        .map(|_| readers.open(Reader::Unnamed, start, Stamp::EPOCH).key)
        .collect();
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
