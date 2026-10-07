//! The cost of one acknowledgment, which runs per message, of one live frame to the
//! complete readers, which runs per frame, and of one floor, which the home reads when
//! `buffer` trims.

use std::sync::Arc;

use delivery::complete::{Charge, Key};
use delivery::{Position, Reader, Readers, Start};
use divan::Bencher;
use types::channel;
use types::frame::key_set::{Group, Interner, KeySet};
use types::frame::{Draft, Form, Frame, Path};
use types::sample::{Scalar, Type};

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
        .map(|_| {
            readers
                .open(Reader::Unnamed, start, limit_bytes, Charge::Whole)
                .key
        })
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

/// A frame of one index, and its key set.
fn frame() -> (Frame, Arc<KeySet>) {
    let set = Interner::new().intern(&[Group {
        index: channel::Key::from_u128(1),
        data: &[],
    }]);
    let config = block::Config { budget: 1 << 16 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    let frame = Draft::new(&pool, &set, Form::Raw, &[(0, 8)])
        .expect("the pool holds the frame")
        .freeze(Path::Live);
    (frame, set)
}

/// One live frame to `sessions` recording readers that keep up: the home queues it,
/// releases it once it is on disk, and each session takes it.
#[divan::bench(args = [1, 16, 256])]
fn release(bencher: Bencher<'_, '_>, sessions: usize) {
    let (mut readers, keys) = opened(sessions, u64::MAX);
    let (frame, set) = frame();
    let mut seq = 0;
    bencher.bench_local(|| {
        readers.queue(&frame, &set, seq..seq + 1);
        seq += 1;
        divan::black_box(readers.release(seq));
        for &key in &keys {
            divan::black_box(readers.take(key.into()));
        }
    });
}

/// The channels of [`wide`]: one index and its data channels.
const CHANNELS: usize = 100_000;

/// A frame of [`CHANNELS`] channels of 8 bytes each, and its key set.
fn wide() -> (Frame, Arc<KeySet>) {
    let f64 = Type::Scalar(Scalar::F64);
    let data: Vec<_> = (2..=CHANNELS)
        .map(|n| {
            (
                channel::Key::from_u128(u128::try_from(n).expect("few")),
                f64,
            )
        })
        .collect();
    let set = Interner::new().intern(&[Group {
        index: channel::Key::from_u128(1),
        data: &data,
    }]);
    let config = block::Config { budget: 1 << 22 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    let lens: Vec<_> = (0..CHANNELS).map(|entry| (entry, 8)).collect();
    let frame = Draft::new(&pool, &set, Form::Raw, &lens)
        .expect("the pool holds the frame")
        .freeze(Path::Live);
    (frame, set)
}

/// One live frame of [`CHANNELS`] channels to one recording reader that keeps up and
/// pays for a remote frame of its `places`: the last channel, or every channel.
#[divan::bench(args = [1, CHANNELS])]
fn release_places(bencher: Bencher<'_, '_>, places: usize) {
    let (frame, set) = wide();
    let slots = set.entries()[CHANNELS - places..]
        .iter()
        .map(|entry| entry.slot)
        .collect();
    let mut readers = Readers::new(0);
    let start = Start::At(Position {
        live: 0,
        backfill: Some(0),
    });
    let key = readers
        .open(Reader::Unnamed, start, u64::MAX, Charge::Places(slots))
        .key;
    let mut seq = 0;
    bencher.bench_local(|| {
        readers.queue(&frame, &set, seq..seq + 1);
        seq += 1;
        divan::black_box(readers.release(seq));
        divan::black_box(readers.take(key.into()));
    });
}

#[divan::bench(args = [1, 16, 256])]
fn floor(bencher: Bencher<'_, '_>, sessions: usize) {
    let (readers, _) = opened(sessions, 0);
    bencher.bench_local(|| divan::black_box(&readers).floor());
}
