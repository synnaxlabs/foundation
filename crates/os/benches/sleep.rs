//! The cost of one poll of a pending sleep whose deadline does not change, which a
//! task pays at each wake for another cause.

use std::pin::pin;
use std::task::{Context, Waker};

use divan::Bencher;
use types::time::Span;

const POLLS: u64 = 1_000_000;

fn main() {
    divan::main();
}

/// One poll of a sleep a minute away, which an earlier poll armed.
#[divan::bench(sample_count = 20)]
fn poll_armed(bencher: Bencher<'_, '_>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the runtime builds");
    let _guard = runtime.enter();
    let clock = os::clock();
    let mut sleep = pin!(clock.sleep(Span::from_nanos(60_000_000_000)));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(
        sleep.as_mut().poll(&mut cx).is_pending(),
        "a sleep a minute away is due"
    );
    bencher
        .counter(divan::counter::ItemsCount::new(POLLS))
        .bench_local(|| {
            for _ in 0..POLLS {
                divan::black_box_drop(sleep.as_mut().poll(&mut cx));
            }
        });
}
