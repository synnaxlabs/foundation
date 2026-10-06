use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Instant;

use env::clock::Clock;
use env::shards::Config;
use types::time::{Monotonic, Span};

use crate::common::assert_joins;

/// Runs `body` on a dedicated thread of `os` and waits for it to end.
fn on_a_thread<F>(body: impl FnOnce() -> F + Send + 'static)
where
    F: Future<Output = ()> + 'static,
{
    let threads = os::threads().expect("the OS gives the cores of this process");
    assert_joins(threads.start("clock", body).unwrap(), Ok(()));
}

/// The polls of a sleep of `span` until it completes.
async fn polls(clock: &Clock, span: Span) -> u32 {
    let mut polls = 0;
    let mut sleep = pin!(clock.sleep(span));
    std::future::poll_fn(|cx| {
        polls += 1;
        sleep.as_mut().poll(cx)
    })
    .await;
    polls
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

/// The std monotonic clock, in nanoseconds since `origin`.
#[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
fn instant(origin: Instant) -> i64 {
    i64::try_from(origin.elapsed().as_nanos()).unwrap()
}

#[test]
fn starts_near_zero_and_moves_with_instant_while_awake() {
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    let origin = Instant::now();
    let clock = os::clock();
    let (before, first, after) = (instant(origin), clock.now(), instant(origin));
    assert!(first < Monotonic(1_000_000_000), "{first:?}");
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (later, second, last) = (instant(origin), clock.now(), instant(origin));
    let moved = (second - first).nanos();
    // A time daemon slews `Instant` on Linux by up to 500 ppm.
    let slew = (last - before) / 1_000;
    assert!(
        moved >= later - after - slew && moved <= last - before + slew,
        "moved {moved} ns, awake {} to {} ns",
        later - after,
        last - before
    );
}

#[test]
fn clones_read_the_same_clock() {
    let clock = os::clock();
    let other = clock.clone();
    let (a, b, c) = (clock.now(), other.now(), clock.now());
    assert!(a <= b && b <= c, "{a:?}, {b:?}, {c:?}");
}

#[test]
fn a_sleep_completes_on_a_dedicated_thread() {
    let clock = os::clock();
    let threads = os::threads().expect("the OS gives the cores of this process");
    let (start, sleeper) = (clock.now(), clock.clone());
    let handle = threads
        .start(
            "sleeper",
            move || async move { sleeper.sleep(millis(5)).await },
        )
        .unwrap();
    assert_joins(handle, Ok(()));
    assert!(clock.now() >= start + millis(5), "{:?}", clock.now());
}

#[test]
fn a_sleep_completes_on_a_shard() {
    let clock = os::clock();
    let shards = os::shards().expect("the OS gives the cores of this process");
    let (start, sleeper) = (clock.now(), clock.clone());
    let config = Config {
        name: "sleeper".into(),
        core: None,
    };
    let handle = shards
        .start(
            config,
            move |_| async move { sleeper.sleep(millis(5)).await },
        )
        .unwrap();
    assert_joins(handle, Ok(()));
    assert!(clock.now() >= start + millis(5), "{:?}", clock.now());
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
fn a_sleep_is_polled_again_only_when_due() {
    let clock = os::clock();
    on_a_thread(move || async move {
        let polls = polls(&clock, millis(50)).await;
        assert!(polls < 10, "{polls} polls");
    });
}

/// Counts its wakes.
#[derive(Default)]
struct Wakes(AtomicUsize);

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, SeqCst);
    }
}

#[test]
fn a_sleep_does_not_wake_its_task_before_the_deadline() {
    let clock = os::clock();
    on_a_thread(move || async move {
        let counter = Arc::new(Wakes::default());
        let waker = Waker::from(Arc::clone(&counter));
        let mut sleep = pin!(clock.sleep(millis(500)));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(sleep.as_mut().poll(&mut cx), Poll::Pending);
        clock.sleep(millis(20)).await;
        assert_eq!(counter.0.load(SeqCst), 0);
    });
}

#[test]
fn a_sleep_past_a_second_wakes_each_second() {
    let clock = os::clock();
    on_a_thread(move || async move {
        let start = clock.now();
        let polls = polls(&clock, millis(2_050)).await;
        assert!(polls >= 4, "{polls} polls");
        assert!(clock.now() >= start + millis(2_050), "{:?}", clock.now());
    });
}

#[test]
fn a_thousand_sleeps_each_complete_at_the_deadline_or_later() {
    let clock = os::clock();
    on_a_thread(move || async move {
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
    on_a_thread(move || async move {
        let deadline = clock.now();
        let mut sleep = pin!(clock.sleep_until(deadline));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(sleep.as_mut().poll(&mut cx), Poll::Ready(()));
    });
}

#[test]
fn a_reset_completes_at_the_new_deadline() {
    let clock = os::clock();
    on_a_thread(move || async move {
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
    on_a_thread(move || async move {
        let deadline = clock.now() + seconds(3_650 * 86_400);
        let mut sleep = pin!(clock.sleep_until(deadline));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(sleep.as_mut().poll(&mut cx), Poll::Pending);
    });
}

#[test]
fn a_deadline_at_the_end_of_the_clock_is_pending() {
    let clock = os::clock();
    on_a_thread(move || async move {
        let mut sleep = pin!(clock.sleep_until(Monotonic(u64::MAX)));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(sleep.as_mut().poll(&mut cx), Poll::Pending);
    });
}

#[test]
#[should_panic(expected = "must be called from the context of a Tokio 1.x runtime")]
fn a_sleep_panics_where_no_runtime_is_current() {
    let clock: Clock = os::clock();
    drop(clock.sleep_until(Monotonic(0)));
}

#[test]
#[should_panic(expected = "A Tokio 1.x context was found, but timers are disabled")]
fn a_sleep_panics_in_a_runtime_with_no_timer() {
    let clock: Clock = os::clock();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async { drop(clock.sleep_until(Monotonic(0))) });
}
