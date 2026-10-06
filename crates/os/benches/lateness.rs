//! The lateness of a sequence of sleeps on a dedicated thread of `os`, at 1 kHz and
//! 10 kHz, and the cost of the poll that arms a sleep's OS timer. Divan gives no tail
//! percentiles, so this prints its own.

use std::pin::Pin;
use std::task::{Context, Waker};

use env::clock::Clock;
use types::time::Span;

const SLEEPS: i64 = 10_000;

#[expect(clippy::print_stdout, reason = "a benchmark prints its result")]
fn main() {
    let threads = os::threads().expect("the OS gives the cores of this process");
    let handle = threads.start("lateness", || async {
        let clock = os::clock();
        for (rate, period_ns) in [("1 kHz", 1_000_000), ("10 kHz", 100_000)] {
            let late = lateness(&clock, Span::from_nanos(period_ns)).await;
            let last = late.len() - 1;
            let at = |per_mille: usize| late[last * per_mille / 1_000] / 1_000;
            println!(
                "{rate}: median {} us, 99th {} us, 99.9th {} us, max {} us",
                at(500),
                at(990),
                at(999),
                at(1_000),
            );
        }
        for (sleep, fresh) in [("a reset sleep", false), ("a new sleep", true)] {
            println!("arm {sleep}: {} ns", arm_cost(&clock, fresh));
        }
    });
    let handle = handle.expect("the thread starts");
    handle.join().expect("the benchmark completes");
}

/// The lateness of each of `SLEEPS` sleeps `period` apart, in nanoseconds, sorted.
async fn lateness(clock: &Clock, period: Span) -> Vec<i64> {
    let start = clock.now();
    let mut sleep = clock.sleep_until(start);
    let mut late = Vec::with_capacity(usize::try_from(SLEEPS).expect("SLEEPS fits"));
    for n in 1..=SLEEPS {
        let deadline = start + Span::from_nanos(period.nanos() * n);
        sleep.reset(deadline);
        (&mut sleep).await;
        late.push((clock.now() - deadline).nanos());
    }
    late.sort_unstable();
    late
}

/// The mean time in nanoseconds of the first poll of a sleep 1 ms away, which arms
/// its OS timer. Each sleep is new when `fresh`, so the time includes the drop of the
/// last one.
fn arm_cost(clock: &Clock, fresh: bool) -> i64 {
    let mut cx = Context::from_waker(Waker::noop());
    let mut sleep = clock.sleep_until(clock.now());
    let start = clock.now();
    for _ in 0..SLEEPS {
        let deadline = clock.now() + Span::from_nanos(1_000_000);
        if fresh {
            sleep = clock.sleep_until(deadline);
        } else {
            sleep.reset(deadline);
        }
        let pending = Pin::new(&mut sleep).poll(&mut cx).is_pending();
        assert!(pending, "a sleep 1 ms away completed at its first poll");
    }
    (clock.now() - start).nanos() / SLEEPS
}
