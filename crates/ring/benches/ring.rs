//! Benchmarks of the ring: the cost of a value on one thread, between two threads,
//! and through a park and a wake.

use std::hint::spin_loop;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use divan::Bencher;
use divan::counter::ItemsCount;
use ring::{Config, Consumer, Full, Producer};

const CAPACITY: usize = 1024;
/// Stops the echo thread of `round_trip`.
const STOP: u64 = u64::MAX;

fn main() {
    divan::main();
}

fn create_ring() -> (Producer<u64>, Consumer<u64>) {
    ring::new(Config {
        capacity: NonZeroUsize::new(CAPACITY).expect("not zero"),
    })
}

/// Pushes `value`, and spins while the ring is full.
fn push_spinning(producer: &mut Producer<u64>, mut value: u64) {
    while let Err(Full(back)) = producer.push(value) {
        value = back;
        spin_loop();
    }
}

/// Pops a value, and spins while the ring is empty.
fn pop_spinning(consumer: &mut Consumer<u64>) -> u64 {
    loop {
        if let Some(value) = consumer.try_pop() {
            return value;
        }
        spin_loop();
    }
}

/// Fills and drains the ring on one thread.
#[divan::bench]
fn one_thread(bencher: Bencher<'_, '_>) {
    let (mut producer, mut consumer) = create_ring();
    bencher.counter(ItemsCount::new(CAPACITY)).bench_local(|| {
        for value in 0..CAPACITY as u64 {
            assert_eq!(producer.push(value), Ok(()), "the ring has room");
        }
        for _ in 0..CAPACITY {
            divan::black_box(consumer.try_pop());
        }
    });
}

/// One thread pushes one value at a time and the other polls `try_pop`. The time
/// includes the start and the join of the pushing thread.
#[divan::bench(sample_count = 20)]
#[expect(
    clippy::disallowed_methods,
    reason = "a benchmark; `ring` has no `env`"
)]
fn two_threads(bencher: Bencher<'_, '_>) {
    const VALUES: u64 = 1_000_000;
    bencher
        .counter(ItemsCount::new(VALUES))
        .with_inputs(create_ring)
        .bench_local_values(|(mut producer, mut consumer)| {
            thread::scope(|scope| {
                scope.spawn(move || {
                    for value in 0..VALUES {
                        push_spinning(&mut producer, value);
                    }
                });
                for _ in 0..VALUES {
                    divan::black_box(pop_spinning(&mut consumer));
                }
            });
        });
}

/// One thread stages up to `BATCH` values and publishes them, and the other polls
/// `try_pop`. The time includes the start and the join of the pushing thread.
#[divan::bench(sample_count = 20)]
#[expect(
    clippy::disallowed_methods,
    reason = "a benchmark; `ring` has no `env`"
)]
fn two_threads_batch(bencher: Bencher<'_, '_>) {
    const VALUES: u64 = 1_000_000;
    const BATCH: u64 = 64;
    bencher
        .counter(ItemsCount::new(VALUES))
        .with_inputs(create_ring)
        .bench_local_values(|(mut producer, mut consumer)| {
            thread::scope(|scope| {
                scope.spawn(move || {
                    let mut value = 0;
                    while value < VALUES {
                        let end = (value + BATCH).min(VALUES);
                        while value < end {
                            if let Err(Full(back)) = producer.stage(value) {
                                value = back;
                                producer.publish();
                                spin_loop();
                            } else {
                                value += 1;
                            }
                        }
                        producer.publish();
                    }
                });
                for _ in 0..VALUES {
                    divan::black_box(pop_spinning(&mut consumer));
                }
            });
        });
}

/// A value goes to a second thread and comes back. Each item is one hop. The time
/// includes the start and the join of the echo thread.
#[divan::bench(sample_count = 20)]
#[expect(
    clippy::disallowed_methods,
    reason = "a benchmark; `ring` has no `env`"
)]
fn round_trip(bencher: Bencher<'_, '_>) {
    const TRIPS: u64 = 100_000;
    bencher
        .counter(ItemsCount::new(2 * TRIPS))
        .with_inputs(|| (create_ring(), create_ring()))
        .bench_local_values(|((mut out, mut echo_in), (mut echo_out, mut back))| {
            thread::scope(|scope| {
                scope.spawn(move || {
                    loop {
                        let value = pop_spinning(&mut echo_in);
                        if value == STOP {
                            break;
                        }
                        push_spinning(&mut echo_out, value);
                    }
                });
                for value in 0..TRIPS {
                    push_spinning(&mut out, value);
                    divan::black_box(pop_spinning(&mut back));
                }
                push_spinning(&mut out, STOP);
            });
        });
}

/// Counts the wakes it gets.
#[derive(Default)]
struct Tally(AtomicU64);

impl Wake for Tally {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Relaxed);
    }
}

/// The consumer parks, the producer pushes and wakes it, and the consumer pops, all
/// on one thread. This is a lower bound: no cache line moves between cores, and the
/// waker only counts.
#[divan::bench]
fn park_and_wake(bencher: Bencher<'_, '_>) {
    const CYCLES: u64 = 1024;
    let (mut producer, mut consumer) = create_ring();
    let tally = Arc::new(Tally::default());
    let waker = Waker::from(Arc::clone(&tally));
    let mut cx = Context::from_waker(&waker);
    bencher.counter(ItemsCount::new(CYCLES)).bench_local(|| {
        let before = tally.0.load(Relaxed);
        for value in 0..CYCLES {
            let parked = pin!(consumer.pop()).poll(&mut cx);
            assert_eq!(parked, Poll::Pending, "the ring was empty");
            assert_eq!(producer.push(value), Ok(()), "the ring has room");
            let popped = pin!(consumer.pop()).poll(&mut cx);
            assert_eq!(popped, Poll::Ready(Some(value)), "the push woke the pop");
        }
        assert_eq!(tally.0.load(Relaxed) - before, CYCLES, "each cycle parked");
    });
}
