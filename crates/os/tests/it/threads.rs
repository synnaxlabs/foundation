//! Dedicated threads on real threads: their body, their blocking, and their panics.

use std::future::{Ready, pending, poll_fn, ready};
use std::panic::panic_any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Poll, Waker};
use std::time::Duration;

use env::threads::Threads;
use tokio::sync::oneshot;
use tokio::task::yield_now;

use crate::common::{
    Armed, Bomb, Relay, Relayed, Stuck, assert_aborts, assert_joins, panicked,
};

fn threads() -> Threads {
    os::threads().expect("the OS gives the cores of this process")
}

#[test]
fn a_thread_runs_its_body_to_completion_and_joins() {
    let done = Arc::new(AtomicBool::new(false));
    let set = Arc::clone(&done);
    let handle = threads()
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
        let handle = threads().start("daqmx-dev1", move || async move {
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
    let handle = threads()
        .start("thread-0", || async { panic!("body") })
        .unwrap();
    assert_joins(handle, panicked("thread-0"));
}

#[test]
fn a_panic_in_the_call_of_the_body_gives_panicked() {
    let body = || -> Ready<()> { panic!("body") };
    let handle = threads().start("thread-1", body).unwrap();
    assert_joins(handle, panicked("thread-1"));
}

#[test]
fn a_panic_in_the_drop_of_the_body_gives_panicked() {
    let body = || Armed {
        faulty: false,
        _bomb: Bomb,
    };
    let handle = threads().start("thread-2", body).unwrap();
    assert_joins(handle, panicked("thread-2"));
}

#[test]
fn a_panic_in_the_poll_and_then_the_drop_of_the_body_gives_panicked() {
    let body = || Armed {
        faulty: true,
        _bomb: Bomb,
    };
    let handle = threads().start("thread-3", body).unwrap();
    assert_joins(handle, panicked("thread-3"));
}

#[test]
fn a_name_with_a_nul_starts_and_panicked_keeps_the_whole_name() {
    let seen = Arc::new(Mutex::new(None));
    let name = Arc::clone(&seen);
    let body = move || {
        *name.lock().unwrap() = std::thread::current().name().map(str::to_owned);
        async { panic!("body") }
    };
    let handle = threads().start("thread\0three", body).unwrap();
    assert_joins(handle, panicked("thread\0three"));
    assert_eq!(*seen.lock().unwrap(), Some("thread".to_owned()));
}

#[test]
#[expect(clippy::disallowed_methods, reason = "each body blocks on the other")]
fn each_body_blocks_only_its_own_thread() {
    let (to_b, from_a) = mpsc::channel::<()>();
    let (to_a, from_b) = mpsc::channel::<()>();
    let threads = threads();
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

/// Opens once, from any thread, and wakes the future that waits for it.
#[derive(Default)]
struct Gate(Mutex<(bool, Option<Waker>)>);

impl Gate {
    fn open(&self) {
        let waker = {
            let mut gate = self.0.lock().unwrap();
            gate.0 = true;
            gate.1.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    async fn wait(&self) {
        poll_fn(|cx| {
            let mut gate = self.0.lock().unwrap();
            if gate.0 {
                return Poll::Ready(());
            }
            gate.1 = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
    }
}

#[test]
fn a_body_awaits_a_future_that_another_thread_wakes() {
    let gate = Arc::new(Gate::default());
    let waits = Arc::clone(&gate);
    let handle = threads()
        .start("thread-5", move || async move { waits.wait().await })
        .unwrap();
    gate.open();
    assert_joins(handle, Ok(()));
}

/// Blocks on a runtime of its own, as the blocking API of a Rust client does.
fn vendor_call() -> u32 {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async { 7 })
}

#[test]
fn a_body_runs_in_the_context_of_a_tokio_runtime() {
    let handle = threads()
        .start("thread-7", || async {
            tokio::runtime::Handle::current();
        })
        .unwrap();
    assert_joins(handle, Ok(()));
}

#[test]
fn a_body_may_block_on_a_tokio_runtime_of_its_own() {
    let handle = threads()
        .start("thread-6", || async { assert_eq!(vendor_call(), 7) })
        .unwrap();
    assert_joins(handle, Ok(()));
}

#[cfg(target_os = "linux")]
#[test]
fn a_thread_started_on_a_pinned_shard_runs_on_every_cpu_of_the_set() {
    use crate::common::affinity;

    let (cpus, threads) = (affinity(), threads());
    let (seen, started) =
        (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None)));
    let (record, keep) = (Arc::clone(&seen), Arc::clone(&started));
    let config = env::shards::Config {
        name: "shard-0".into(),
        core: Some(0),
    };
    let shards = os::shards().expect("the OS gives the cores of this process");
    let main = move |_| async move {
        let body = move || async move { *record.lock().unwrap() = affinity() };
        *keep.lock().unwrap() = Some(threads.start("vendor", body));
    };
    assert_joins(shards.start(config, main).unwrap(), Ok(()));
    let handle = started.lock().unwrap().take();
    assert_joins(handle.unwrap().unwrap(), Ok(()));
    assert_eq!(*seen.lock().unwrap(), cpus);
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_in_the_call_of_the_body_gives_panicked() {
    let body = || -> Ready<()> { panic_any(Relay(2)) };
    let handle = threads().start("thread-8", body).unwrap();
    assert_joins(handle, panicked("thread-8"));
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_in_the_poll_of_the_body_gives_panicked() {
    let body = || async { panic_any(Relay(2)) };
    let handle = threads().start("thread-9", body).unwrap();
    assert_joins(handle, panicked("thread-9"));
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_in_the_drop_of_the_body_gives_panicked() {
    let handle = threads().start("thread-10", || Relayed).unwrap();
    assert_joins(handle, panicked("thread-10"));
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_as_a_tokio_task_drops_gives_panicked() {
    let body = || {
        // Tokio catches the first two panics of the chain at the drop of the runtime.
        drop(tokio::spawn(Stuck(4)));
        ready(())
    };
    let handle = threads().start("thread-11", body).unwrap();
    assert_joins(handle, panicked("thread-11"));
}

#[test]
fn a_panic_in_the_poll_of_a_tokio_task_gives_ok() {
    let body = || async {
        let (sender, dropped) = oneshot::channel::<()>();
        drop(tokio::spawn(async move {
            let _sender = sender;
            panic!("tokio task");
        }));
        dropped.await.expect_err("the panic drops the sender");
    };
    let handle = threads().start("thread-12", body).unwrap();
    assert_joins(handle, Ok(()));
}

#[test]
fn a_panic_in_the_drop_of_a_tokio_task_gives_ok() {
    let body = || {
        // The payload of the panic, `Relay(0)`, does not panic in its drop.
        drop(tokio::spawn(Stuck(0)));
        ready(())
    };
    let handle = threads().start("thread-13", body).unwrap();
    assert_joins(handle, Ok(()));
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_in_a_tokio_task_gives_panicked() {
    let body = || async {
        // Tokio catches the panic of the poll and the first panic in the drop of its
        // payload.
        drop(tokio::spawn(async { panic_any(Relay(2)) }));
        pending::<()>().await;
    };
    let handle = threads().start("thread-14", body).unwrap();
    assert_joins(handle, panicked("thread-14"));
}

#[test]
fn a_panic_in_the_poll_and_then_the_drop_of_a_tokio_task_aborts_the_process() {
    assert_aborts(|| {
        let body = || async {
            drop(tokio::spawn(Armed {
                faulty: true,
                _bomb: Bomb,
            }));
            pending::<()>().await;
        };
        let handle = threads().start("thread-15", body).unwrap();
        assert_joins(handle, Ok(()));
    });
}

#[test]
fn a_panic_over_a_local_that_panics_in_its_drop_in_the_body_aborts_the_process() {
    assert_aborts(|| {
        let body = || async {
            let _bomb = Bomb;
            panic!("body");
        };
        let handle = threads().start("thread-16", body).unwrap();
        assert_joins(handle, panicked("thread-16"));
    });
}

#[test]
fn a_panic_in_the_drop_of_an_aborted_tokio_task_gives_ok() {
    let body = || async {
        let task = tokio::spawn(Stuck(0));
        task.abort();
        let error = task.await.expect_err("the abort drops the task");
        assert!(error.is_panic(), "{error}");
    };
    let handle = threads().start("thread-17", body).unwrap();
    assert_joins(handle, Ok(()));
}
