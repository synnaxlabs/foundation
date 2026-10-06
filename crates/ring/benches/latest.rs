//! Benchmarks of `ring::latest`: the cost of a read, alone and beside a writer.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::thread;
use std::time::Duration;

use divan::Bencher;
use ring::latest::{self, Reader};

const WORDS: usize = 6;
/// Words in a cell that spans two 64-byte lines with its sequence number.
const WIDE: usize = 9;
const READS: u64 = 1_000_000;

fn main() {
    divan::main();
}

fn read_all<const N: usize>(reader: &Reader<N>) {
    for _ in 0..READS {
        divan::black_box(reader.read(|value| value[0] + value[N - 1]));
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

/// Reads beside a writer on another thread that updates with `change`, then waits
/// `pause`.
fn read_beside_a_writer<const N: usize>(
    bencher: Bencher<'_, '_>,
    pause: Duration,
    change: fn([u64; N]) -> [u64; N],
) {
    let (mut writer, reader) = latest::new([1; N]);
    let stopped = AtomicBool::new(false);
    thread::scope(|scope| {
        #[expect(
            clippy::disallowed_methods,
            reason = "a benchmark paces its writer with a real clock"
        )]
        scope.spawn(|| {
            while !stopped.load(Relaxed) {
                writer.update(change);
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
    read_beside_a_writer::<WORDS>(bencher, Duration::from_millis(1), change_each_word);
}

/// Reads beside a writer in a tight loop: the worst case, where most reads run again.
#[divan::bench(sample_count = 20)]
fn read_with_a_spinning_writer(bencher: Bencher<'_, '_>) {
    read_beside_a_writer::<WORDS>(bencher, Duration::ZERO, change_each_word);
}

/// Reads a cell over 64 bytes beside a writer in a tight loop that changes only its
/// first word, so its second line does not change.
#[divan::bench(sample_count = 20)]
fn read_with_a_spinning_writer_that_changes_one_line(bencher: Bencher<'_, '_>) {
    read_beside_a_writer::<WIDE>(bencher, Duration::ZERO, |mut value| {
        value[0] += 1;
        divan::black_box(value)
    });
}

/// As above, but the writer also changes the last word, so it writes both lines.
#[divan::bench(sample_count = 20)]
fn read_with_a_spinning_writer_that_changes_both_lines(bencher: Bencher<'_, '_>) {
    read_beside_a_writer::<WIDE>(bencher, Duration::ZERO, |mut value| {
        value[0] += 1;
        value[WIDE - 1] += 1;
        divan::black_box(value)
    });
}

fn change_each_word<const N: usize>(value: [u64; N]) -> [u64; N] {
    value.map(|word| word + 1)
}
