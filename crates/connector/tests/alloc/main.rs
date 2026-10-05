//! Polling a registered wait or race allocates nothing, and so does a tick from the
//! third call on. This binary has no test harness: the count covers each thread, and a
//! harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::cell::Cell;
use std::future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Wake, Waker};

use connector::cancel::Token;
use connector::pace::Timer;
use types::time::Rate;

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
    check(pin!(token.wait()), &token, &wakers, "wait");
    check(
        pin!(token.race(future::pending::<()>())),
        &token,
        &wakers,
        "race",
    );
    check_ticks();
}

/// Checks that 64 ticks after two warm-up ticks allocate nothing in their polls. The
/// count covers only the polls of `tick`, not the simulator around them.
fn check_ticks() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let clock = node.clock();
    let config = env::shards::Config {
        name: "shard-0".into(),
        core: Some(0),
    };
    let handle = node
        .shards()
        .start(config, move |_| async move {
            let token = Token::new();
            let rate = Rate::new(1_000, 1).expect("1 kHz is a rate");
            let mut timer = Timer::new(&clock, rate);
            // Tick 0 never waits, and the simulated clock grows its timer list on the
            // first wait.
            for _ in 0..2 {
                assert!(timer.tick(&token).await.is_some(), "a live token");
            }
            let allocations = Cell::new(0);
            for _ in 0..64 {
                let mut tick = pin!(timer.tick(&token));
                let tick = future::poll_fn(|cx| {
                    let (poll, count) = ALLOCATOR.count(|| tick.as_mut().poll(cx));
                    allocations.set(allocations.get() + count);
                    poll
                })
                .await;
                assert!(tick.is_some(), "a live token");
            }
            assert_eq!(
                allocations.get(),
                0,
                "a tick from the third call on allocates nothing"
            );
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ends");
}

/// Polls `f` once to register it, then checks that 64 more polls with alternating
/// wakers allocate nothing.
fn check<F: Future>(mut f: Pin<&mut F>, token: &Token, wakers: &[Waker], name: &str) {
    let mut cx = Context::from_waker(&wakers[0]);
    assert!(f.as_mut().poll(&mut cx).is_pending(), "{name}: live token");
    let ((), allocations) = ALLOCATOR.count(|| {
        for waker in wakers.iter().cycle().take(64) {
            let mut cx = Context::from_waker(waker);
            assert!(f.as_mut().poll(&mut cx).is_pending(), "{name}: live token");
        }
        assert!(!token.cancelled(), "{name}: the token is live");
    });
    assert_eq!(
        allocations, 0,
        "{name}: a registered poll allocates nothing"
    );
}
