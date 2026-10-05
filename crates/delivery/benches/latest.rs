//! The cost of one live frame to the latest readers of its index, which the home pays
//! for each frame on the write path.

use delivery::{Key, Readers};
use divan::Bencher;
use types::channel;
use types::frame::key_set::{Group, Interner};
use types::frame::{Draft, Form, Frame, Path};
use types::time::Stamp;

fn main() {
    divan::main();
}

/// A frame of one index, and `sessions` latest readers of it.
fn opened(sessions: usize) -> (Frame, Readers, Vec<Key>) {
    let set = Interner::new().intern(&[Group {
        index: channel::Key::from_u128(1),
        data: &[],
    }]);
    let config = block::Config { budget: 1 << 16 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    let frame = Draft::new(&pool, &set, Form::Raw, &[(0, 8)])
        .expect("the pool holds the frame")
        .freeze(Path::Live);
    let mut readers = Readers::new();
    let keys = (0..sessions)
        .map(|_| readers.open_latest(None, Stamp::from_nanos(0)).key)
        .collect();
    (frame, readers, keys)
}

/// Readers that keep up: each put wakes every session, and each takes its frame.
#[divan::bench(args = [0, 1, 16])]
fn put_and_take(bencher: Bencher<'_, '_>, sessions: usize) {
    let (frame, mut readers, keys) = opened(sessions);
    bencher.bench_local(|| {
        divan::black_box(readers.put(frame.clone()));
        for &key in &keys {
            divan::black_box(readers.take(key));
        }
    });
}

/// Readers that fall behind: each put replaces the frame that waits for every session.
#[divan::bench(args = [0, 1, 16])]
fn replace(bencher: Bencher<'_, '_>, sessions: usize) {
    let (frame, mut readers, _) = opened(sessions);
    bencher.bench_local(|| {
        divan::black_box(readers.put(frame.clone()));
    });
}
