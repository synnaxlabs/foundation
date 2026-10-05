//! Polling a registered wait or race allocates nothing. This binary has no test harness: the
//! count covers each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Poll, Wake, Waker};

use connector::cancel::Token;

/// A waker with a reference count, as a task has. It counts its wakes.
#[derive(Default)]
struct Tally(AtomicU64);

impl Wake for Tally {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );

    let token = Token::new();
    let wakers = [
        Waker::from(Arc::new(Tally::default())),
        Waker::from(Arc::new(Tally::default())),
    ];
    let mut wait = pin!(token.wait());
    let mut cx = Context::from_waker(&wakers[0]);
    assert_eq!(wait.as_mut().poll(&mut cx), Poll::Pending, "live token");
    let ((), allocations) = ALLOCATOR.count(|| {
        for waker in wakers.iter().cycle().take(64) {
            let mut cx = Context::from_waker(waker);
            assert_eq!(wait.as_mut().poll(&mut cx), Poll::Pending, "live token");
        }
        assert!(!token.cancelled(), "the token is live");
    });
    assert_eq!(
        allocations, 0,
        "polling a registered wait allocates nothing"
    );

    let mut race = pin!(token.race(std::future::pending::<()>()));
    let mut cx = Context::from_waker(&wakers[0]);
    assert_eq!(race.as_mut().poll(&mut cx), Poll::Pending, "live token");
    let ((), allocations) = ALLOCATOR.count(|| {
        for waker in wakers.iter().cycle().take(64) {
            let mut cx = Context::from_waker(waker);
            assert_eq!(race.as_mut().poll(&mut cx), Poll::Pending, "live token");
        }
    });
    assert_eq!(
        allocations, 0,
        "polling a registered race allocates nothing"
    );
}
