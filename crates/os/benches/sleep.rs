//! The cost of one poll of pending sleeps whose deadlines do not change, which a task
//! pays at each wake for another cause.

use std::pin::Pin;
use std::task::{Context, Poll};

use divan::Bencher;
use types::time::Span;

const POLLS: u64 = 1_000_000;

fn main() {
    divan::main();
}

/// One poll of each of `sleeps` sleeps a minute away, which an earlier poll armed, in
/// the `block_on` of a current-thread runtime. Its waker counts references with
/// atomics, as the waker of a task does. The carrier polls one, and two while a read
/// waits.
#[divan::bench(args = [1, 2], sample_count = 20)]
fn poll_armed(bencher: Bencher<'_, '_>, sleeps: usize) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the runtime builds");
    let clock = os::clock();
    runtime.block_on(async move {
        let waker = std::future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
        let mut cx = Context::from_waker(&waker);
        let mut armed: Vec<_> = (0..sleeps)
            .map(|_| clock.sleep(Span::from_nanos(60_000_000_000)))
            .collect();
        for sleep in &mut armed {
            assert!(
                Pin::new(sleep).poll(&mut cx).is_pending(),
                "a sleep a minute away is due"
            );
        }
        bencher
            .counter(divan::counter::ItemsCount::new(POLLS))
            .bench_local(|| {
                for _ in 0..POLLS {
                    for sleep in &mut armed {
                        divan::black_box_drop(Pin::new(sleep).poll(&mut cx));
                    }
                }
            });
    });
}
