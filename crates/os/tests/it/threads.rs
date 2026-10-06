//! Dedicated threads on real threads: their body, their blocking, and their panics.

use std::future::Ready;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use env::thread::{Handle, Panicked};
use tokio::task::yield_now;

/// Asserts that `handle` joins with `outcome` in ten seconds, so a thread that does not
/// end fails its test and does not hang the run.
#[expect(clippy::disallowed_methods, reason = "the test bounds the join")]
fn assert_joins(handle: Handle, outcome: Result<(), Panicked>) {
    let (done, joined) = mpsc::channel();
    std::thread::spawn(move || done.send(handle.join()));
    let joined = joined.recv_timeout(Duration::from_secs(10));
    assert_eq!(joined, Ok(outcome), "the thread ends in ten seconds");
}

fn panicked(name: &str) -> Result<(), Panicked> {
    Err(Panicked { name: name.into() })
}

/// Panics when it drops.
struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("bomb");
    }
}

#[test]
fn a_thread_runs_its_body_to_completion_and_joins() {
    let done = Arc::new(AtomicBool::new(false));
    let set = Arc::clone(&done);
    let handle = os::threads()
        .start("modbus-poll", move || async move {
            for _ in 0..3 {
                yield_now().await;
            }
            set.store(true, Ordering::SeqCst);
        })
        .unwrap();
    assert_joins(handle, Ok(()));
    assert!(done.load(Ordering::SeqCst));
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the body blocks as a vendor call does"
)]
fn start_returns_while_the_body_blocks_its_thread() {
    let (go, wait) = mpsc::channel::<()>();
    let (done, started) = mpsc::channel();
    std::thread::spawn(move || {
        let handle = os::threads().start("daqmx-dev1", move || async move {
            wait.recv().unwrap();
        });
        done.send(handle)
    });
    let started = started.recv_timeout(Duration::from_secs(10));
    go.send(()).unwrap();
    let handle = started
        .expect("start returns while the body blocks")
        .unwrap();
    assert_joins(handle, Ok(()));
}

#[test]
fn a_panic_in_the_poll_of_the_body_gives_panicked() {
    let handle = os::threads()
        .start("thread-0", || async { panic!("body") })
        .unwrap();
    assert_joins(handle, panicked("thread-0"));
}

#[test]
fn a_panic_in_the_call_of_the_body_gives_panicked() {
    let body = || -> Ready<()> { panic!("body") };
    let handle = os::threads().start("thread-1", body).unwrap();
    assert_joins(handle, panicked("thread-1"));
}

#[test]
fn a_panic_in_the_drop_of_the_body_gives_panicked() {
    let body = || {
        let bomb = Bomb;
        async move {
            let _bomb = bomb;
        }
    };
    let handle = os::threads().start("thread-2", body).unwrap();
    assert_joins(handle, panicked("thread-2"));
}

#[test]
fn a_name_with_a_nul_starts_and_panicked_keeps_the_whole_name() {
    let seen = Arc::new(Mutex::new(None));
    let name = Arc::clone(&seen);
    let body = move || {
        *name.lock().unwrap() = std::thread::current().name().map(str::to_owned);
        async { panic!("body") }
    };
    let handle = os::threads().start("thread\0three", body).unwrap();
    assert_joins(handle, panicked("thread\0three"));
    assert_eq!(*seen.lock().unwrap(), Some("thread".to_owned()));
}

#[test]
#[expect(clippy::disallowed_methods, reason = "each body blocks on the other")]
fn each_body_blocks_only_its_own_thread() {
    let (to_b, from_a) = mpsc::channel::<()>();
    let (to_a, from_b) = mpsc::channel::<()>();
    let threads = os::threads();
    let a = threads
        .start("thread-a", move || async move {
            to_b.send(()).unwrap();
            from_b.recv().unwrap();
        })
        .unwrap();
    let b = threads
        .start("thread-b", move || async move {
            from_a.recv().unwrap();
            to_a.send(()).unwrap();
        })
        .unwrap();
    assert_joins(a, Ok(()));
    assert_joins(b, Ok(()));
}
