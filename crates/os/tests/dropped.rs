//! A lookup dropped before it answers. It counts the threads of the process, so it
//! runs in a test binary of its own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

use std::task::{Context, Waker};
use std::time::Duration;

use tokio::time::timeout;

/// The threads of this process.
fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").unwrap().count()
}

#[test]
fn a_dropped_lookup_ends_its_thread_with_no_panic() {
    let test = std::thread::current().id();
    // libtest sees a panic only on the thread of a test.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        if std::thread::current().id() != test {
            std::process::abort();
        }
    }));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a current-thread runtime builds");
    let before = threads();
    let net = os::net();
    let mut cx = Context::from_waker(Waker::noop());
    // A lookup that answers before its first poll ends leaves no answer to drop.
    let dropped = (0..100).any(|_| {
        let mut lookup = Box::pin(net.resolve("localhost", 4433));
        lookup.as_mut().poll(&mut cx).is_pending()
    });
    assert!(dropped, "a lookup is pending at its first poll");
    let ended = runtime.block_on(async {
        timeout(Duration::from_secs(10), async {
            while threads() > before {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
    });
    ended.expect("each lookup thread ends");
}
