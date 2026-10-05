//! Benchmarks of the ring: the cost of a value on one thread, between two threads,
//! and through a park and a wake.

use std::hint::spin_loop;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
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
        capacity: CAPACITY,
        spins: 0,
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
            push_spinning(&mut producer, value);
        }
        for _ in 0..CAPACITY {
            divan::black_box(consumer.try_pop());
        }
    });
}

/// One thread pushes one value at a time and the other polls `try_pop`.
#[divan::bench(sample_count = 20)]
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

/// A value goes to a second thread and comes back. Each item is one hop.
#[divan::bench(sample_count = 20)]
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

/// The consumer parks, the producer pushes and wakes it, and the consumer pops, all
/// on one thread. The waker does nothing, so this is the cost of the protocol alone.
#[divan::bench]
fn park_and_wake(bencher: Bencher<'_, '_>) {
    const CYCLES: u64 = 1024;
    let (mut producer, mut consumer) = create_ring();
    let mut cx = Context::from_waker(Waker::noop());
    bencher.counter(ItemsCount::new(CYCLES)).bench_local(|| {
        for value in 0..CYCLES {
            let parked = pin!(consumer.pop()).poll(&mut cx);
            assert_eq!(parked, Poll::Pending, "the ring was empty");
            push_spinning(&mut producer, value);
            let popped = pin!(consumer.pop()).poll(&mut cx);
            assert_eq!(popped, Poll::Ready(Some(value)), "the push woke the pop");
        }
    });
}
