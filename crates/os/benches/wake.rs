//! What Tokio's I/O driver costs each runtime of `os`: a wake from another thread, and
//! a park for no time at each yield of a task with nothing else to run.
//! Divan runs no body on a thread of `os`, so this prints its own.

use env::clock::Clock;
use tokio::sync::mpsc;

const ROUNDS: usize = 100_000;

#[expect(clippy::print_stdout, reason = "a benchmark prints its result")]
fn main() {
    let threads = os::threads().expect("the OS gives the cores of this process");
    let (to_echo, mut at_echo) = mpsc::unbounded_channel::<()>();
    let (to_ping, mut at_ping) = mpsc::unbounded_channel::<()>();
    let echo = threads.start("echo", move || async move {
        while at_echo.recv().await.is_some() {
            to_ping
                .send(())
                .expect("the ping thread waits for each reply");
        }
    });
    let ping = threads.start("ping", move || async move {
        let clock = os::clock();
        let mut trips = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let start = clock.now();
            to_echo
                .send(())
                .expect("the echo thread runs until this ends");
            at_ping.recv().await.expect("the echo thread replies");
            trips.push((clock.now() - start).nanos());
        }
        trips.sort_unstable();
        println!(
            "a round trip between two threads: median {} ns, 99th {} ns",
            trips[ROUNDS / 2],
            trips[ROUNDS * 99 / 100],
        );
        println!("a yield: {} ns", yield_cost(&clock).await);
    });
    ping.expect("the thread starts")
        .join()
        .expect("the benchmark completes");
    echo.expect("the thread starts")
        .join()
        .expect("the echo thread ends");
}

/// The mean time in nanoseconds of a yield of a task with nothing else to run. The
/// task runs in the runtime's `block_on`, so each yield parks the runtime for no time.
async fn yield_cost(clock: &Clock) -> i64 {
    let start = clock.now();
    tokio::spawn(async {
        for _ in 0..ROUNDS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the task completes");
    (clock.now() - start).nanos() / i64::try_from(ROUNDS).expect("ROUNDS fits")
}
