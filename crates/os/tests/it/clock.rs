use std::pin::pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use env::clock::Clock;
use tokio::runtime::{Builder, Runtime};
use types::time::{Monotonic, Span};

/// The runtime that a thread of `os` runs, with Tokio's timer.
fn runtime() -> Runtime {
    Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a current-thread runtime builds")
}

fn millis(n: i64) -> Span {
    Span::from_nanos(n * 1_000_000)
}

fn seconds(n: i64) -> Span {
    Span::from_nanos(n * 1_000_000_000)
}

#[test]
fn never_goes_backwards_across_threads() {
    let clock = os::clock();
    let latest = AtomicU64::new(0);
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..100_000 {
                    let seen = latest.load(SeqCst);
                    let now = clock.now().0;
                    assert!(now >= seen, "{now} after {seen}");
                    latest.fetch_max(now, SeqCst);
                }
            });
        }
    });
}

#[test]
fn starts_near_zero_and_moves_with_instant_while_awake() {
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    let start = Instant::now();
    let clock = os::clock();
    assert!(clock.now() < Monotonic(1_000_000_000), "{:?}", clock.now());
    let first = clock.now();
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    std::thread::sleep(std::time::Duration::from_millis(50));
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    let awake = start.elapsed();
    let moved = clock.now() - first;
    let awake = Span::from_nanos(i64::try_from(awake.as_nanos()).unwrap());
    let gap = if moved > awake {
        moved - awake
    } else {
        awake - moved
    };
    assert!(gap <= Span::MILLISECOND, "moved {moved}, awake {awake}");
}

#[test]
fn clones_read_the_same_clock() {
    let clock = os::clock();
    let other = clock.clone();
    let before = clock.now();
    assert!(other.now() >= before);
    assert!(clock.now() >= other.now() - millis(1));
}

#[test]
fn each_call_starts_a_new_clock() {
    let first = os::clock();
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = os::clock();
    assert!(second.now() < first.now(), "the second clock started later");
}

#[test]
fn a_thousand_sleeps_each_complete_at_the_deadline_or_later() {
    let clock = os::clock();
    runtime().block_on(async {
        for i in 0..1_000 {
            let deadline = clock.now() + millis(i % 4);
            clock.sleep_until(deadline).await;
            let now = clock.now();
            assert!(
                now >= deadline,
                "sleep {i} woke at {now:?} for {deadline:?}"
            );
        }
    });
}

#[test]
fn a_passed_deadline_completes_on_the_first_poll() {
    let clock = os::clock();
    let runtime = runtime();
    let _guard = runtime.enter();
    let deadline = clock.now();
    let mut sleep = pin!(clock.sleep_until(deadline));
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(sleep.as_mut().poll(&mut cx), Poll::Ready(()));
}

#[test]
fn a_reset_completes_at_the_new_deadline() {
    let clock = os::clock();
    runtime().block_on(async {
        let start = clock.now();
        let mut sleep = clock.sleep_until(start + seconds(2));
        sleep.reset(start + millis(5));
        (&mut sleep).await;
        let woke = clock.now();
        assert!(woke >= start + millis(5), "early: {woke:?}");
        assert!(
            woke < start + Span::SECOND,
            "the old deadline held: {woke:?}"
        );

        let start = clock.now();
        let mut sleep = clock.sleep_until(start + millis(5));
        sleep.reset(start + millis(50));
        (&mut sleep).await;
        assert!(
            clock.now() >= start + millis(50),
            "early: {:?}",
            clock.now()
        );

        let again = clock.now() + millis(5);
        sleep.reset(again);
        (&mut sleep).await;
        assert!(clock.now() >= again, "early: {:?}", clock.now());
    });
}

#[test]
fn a_far_deadline_is_pending() {
    let clock = os::clock();
    let runtime = runtime();
    let _guard = runtime.enter();
    let deadline = clock.now() + seconds(3_650 * 86_400);
    let mut sleep = pin!(clock.sleep_until(deadline));
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(sleep.as_mut().poll(&mut cx), Poll::Pending);
}

#[test]
#[should_panic(expected = "must be called from the context of a Tokio 1.x runtime")]
fn a_sleep_panics_on_a_thread_with_no_runtime() {
    let clock: Clock = os::clock();
    drop(clock.sleep_until(Monotonic(0)));
}
