//! The cost of one read of mesh time, on a frame's path.

use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::thread;
use std::time::{Duration, Instant};

use clock::{Clock, Reader};
use divan::Bencher;
use estimate::Measurement;
use types::time::{Monotonic, Span};

const READS: u64 = 1_000_000;

fn main() {
    divan::main();
}

/// A stand-in for the `os` driver, which does not exist yet: `std`'s monotonic clock,
/// which stops in sleep on macOS and does not order its read.
struct Os(Instant);

impl env::clock::Driver for Os {
    #[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
    fn now(&self) -> Monotonic {
        let nanos = self.0.elapsed().as_nanos();
        Monotonic(u64::try_from(nanos).expect("under 584 years"))
    }

    fn epoch(&self) -> Instant {
        self.0
    }

    fn timer(&self) -> Pin<Box<dyn env::clock::Timer>> {
        unreachable!("the benchmark only reads")
    }
}

#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn monotonic() -> env::clock::Clock {
    env::clock::Clock::new(Os(Instant::now()))
}

/// Pushes one measurement from one source.
fn push(clock: &mut Clock, source: clock::source::Key, monotonic: &env::clock::Clock) {
    let m = Measurement::new(monotonic.now(), Span::HOUR, Span::MILLISECOND);
    let _ = clock.push(source, m.expect("at most 36500 days"));
}

fn read_all(reader: &Reader) {
    for _ in 0..READS {
        divan::black_box(reader.now());
    }
}

/// One read of the monotonic clock alone, for comparison.
#[divan::bench(sample_count = 20)]
fn monotonic_now(bencher: Bencher<'_, '_>) {
    let monotonic = monotonic();
    bencher
        .counter(divan::counter::ItemsCount::new(READS))
        .bench_local(|| {
            for _ in 0..READS {
                divan::black_box(monotonic.now());
            }
        });
}

/// Reads with no change to the sources.
#[divan::bench(sample_count = 20)]
fn now(bencher: Bencher<'_, '_>) {
    let monotonic = monotonic();
    let (mut clock, reader) = Clock::new(monotonic.clone());
    let source = clock.add();
    push(&mut clock, source, &monotonic);
    bencher
        .counter(divan::counter::ItemsCount::new(READS))
        .bench_local(|| read_all(&reader));
}

/// Reads on 1, 4, and 8 threads at once.
#[divan::bench(sample_count = 20, threads = [1, 4, 8])]
fn now_on_many_threads(bencher: Bencher<'_, '_>) {
    let monotonic = monotonic();
    let (mut clock, reader) = Clock::new(monotonic.clone());
    let source = clock.add();
    push(&mut clock, source, &monotonic);
    bencher
        .counter(divan::counter::ItemsCount::new(READS))
        .bench(|| read_all(&reader));
}

/// Reads while another thread pushes a measurement, then waits `pause`.
fn now_beside_a_writer(bencher: Bencher<'_, '_>, pause: Duration) {
    let monotonic = monotonic();
    let (mut clock, reader) = Clock::new(monotonic.clone());
    let source = clock.add();
    push(&mut clock, source, &monotonic);
    let stopped = AtomicBool::new(false);
    thread::scope(|scope| {
        #[expect(
            clippy::disallowed_methods,
            reason = "a benchmark paces its writer with a real clock"
        )]
        scope.spawn(|| {
            while !stopped.load(Relaxed) {
                push(&mut clock, source, &monotonic);
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

/// Reads while another thread pushes once a millisecond.
#[divan::bench(sample_count = 20)]
fn now_with_a_push_each_millisecond(bencher: Bencher<'_, '_>) {
    now_beside_a_writer(bencher, Duration::from_millis(1));
}

/// Reads while another thread pushes in a tight loop: the worst case, where most reads
/// run again.
#[divan::bench(sample_count = 20)]
fn now_with_a_spinning_push(bencher: Bencher<'_, '_>) {
    now_beside_a_writer(bencher, Duration::ZERO);
}
