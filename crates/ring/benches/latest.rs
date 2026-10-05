//! Benchmarks of `ring::latest`: the cost of a read of six words, alone and beside a
//! writer.

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

/// Reads beside a writer on another thread that updates, then waits `pause`.
fn read_beside_a_writer(bencher: Bencher<'_, '_>, pause: Duration) {
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
                if !pause.is_zero() {
                    thread::sleep(pause);
                }
            }
        });
        bencher
            .counter(divan::counter::ItemsCount::new(READS))
            .bench_local(|| read_all(&reader));
        stopped.store(true, Relaxed);
    });
}

/// Reads beside a writer that updates once a millisecond, about a thousand times the
/// production rate.
#[divan::bench(sample_count = 20)]
fn read_with_a_writer(bencher: Bencher<'_, '_>) {
    read_beside_a_writer(bencher, Duration::from_millis(1));
}

/// Reads beside a writer in a tight loop: the worst case, where most reads run again.
#[divan::bench(sample_count = 20)]
fn read_with_a_spinning_writer(bencher: Bencher<'_, '_>) {
    read_beside_a_writer(bencher, Duration::ZERO);
}
