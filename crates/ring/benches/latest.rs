//! Benchmarks of `ring::latest`: the cost of a read of six words, alone and beside a
//! writer that updates once a millisecond.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::thread;
use std::time::Duration;

use divan::Bencher;
use ring::latest::{self, Reader};

const WORDS: usize = 6;
const READS: u64 = 1_000_000;

fn main() {
    divan::main();
}

fn read_all(reader: &Reader<WORDS>) {
    for _ in 0..READS {
        divan::black_box(reader.read(|value| value[0] + value[WORDS - 1]));
    }
}

/// Reads with no writer.
#[divan::bench(sample_count = 20)]
fn read(bencher: Bencher<'_, '_>) {
    let (_writer, reader) = latest::new([1; WORDS]);
    bencher
        .counter(divan::counter::ItemsCount::new(READS))
        .bench_local(|| read_all(&reader));
}

/// Reads beside a writer that updates once a millisecond on another thread.
#[divan::bench(sample_count = 20)]
fn read_with_a_writer(bencher: Bencher<'_, '_>) {
    let (mut writer, reader) = latest::new([1; WORDS]);
    let stopped = AtomicBool::new(false);
    thread::scope(|scope| {
        #[expect(
            clippy::disallowed_methods,
            reason = "a benchmark paces its writer with a real clock"
        )]
        scope.spawn(|| {
            while !stopped.load(Relaxed) {
                writer.update(|value| value.map(|word| word + 1));
                thread::sleep(Duration::from_millis(1));
            }
        });
        bencher
            .counter(divan::counter::ItemsCount::new(READS))
            .bench_local(|| read_all(&reader));
        stopped.store(true, Relaxed);
    });
}
