//! Shards on real threads: their tasks, their end, and their panics.

use std::cell::Cell;
use std::future::{Ready, pending};
use std::panic::panic_any;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use env::shards::{Config, Shards};
use env::tasks::Tasks;
use tokio::sync::oneshot;
use tokio::task::yield_now;

use crate::common::{
    Armed, Bomb, Relay, Relayed, Stuck, assert_aborts, assert_joins, panicked,
};

fn shards() -> Shards {
    os::shards().expect("the OS gives the cores of this process")
}

fn config(name: &str) -> Config {
    Config {
        name: name.into(),
        core: None,
    }
}

/// A flag that a [`Dropped`] sets.
fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn raised(flag: &AtomicBool) -> bool {
    flag.load(Ordering::SeqCst)
}

/// Sets its flag when it drops.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_shard_runs_its_main_future_and_joins() {
    let ran = flag();
    let main = Dropped(Arc::clone(&ran));
    let handle = shards()
        .start(config("shard-0"), move |_| async move { drop(main) })
        .unwrap();
    assert_joins(handle, Ok(()));
    assert!(raised(&ran));
}

#[test]
fn a_task_and_a_task_it_spawns_run() {
    let runs = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&runs);
    let main = move |tasks: Tasks| async move {
        let ran = Rc::new(Cell::new(0));
        let (outer, inner) = (Rc::clone(&ran), Rc::clone(&ran));
        let spawner = tasks.clone();
        tasks.spawn(async move {
            outer.set(outer.get() + 1);
            spawner.spawn(async move { inner.set(inner.get() + 1) });
        });
        for _ in 0..100 {
            if ran.get() == 2 {
                break;
            }
            yield_now().await;
        }
        count.store(ran.get(), Ordering::SeqCst);
    };
    let handle = shards().start(config("shard-0"), main).unwrap();
    assert_joins(handle, Ok(()));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[test]
fn the_end_of_the_main_future_drops_the_other_tasks() {
    let (plain, holder) = (flag(), flag());
    let (first, second) = (Dropped(Arc::clone(&plain)), Dropped(Arc::clone(&holder)));
    let main = move |tasks: Tasks| async move {
        let clone = tasks.clone();
        tasks.spawn(async move {
            let _first = first;
            pending::<()>().await;
        });
        tasks.spawn(async move {
            let _held = (second, clone);
            pending::<()>().await;
        });
        yield_now().await;
    };
    let handle = shards().start(config("shard-0"), main).unwrap();
    assert_joins(handle, Ok(()));
    assert!(raised(&plain));
    assert!(raised(&holder), "a task that holds the shard's Tasks drops");
}

#[test]
fn a_panic_in_a_task_ends_the_shard() {
    let dropped = flag();
    let guard = Dropped(Arc::clone(&dropped));
    let main = move |tasks: Tasks| async move {
        let _main = guard;
        tasks.spawn(async { panic!("task") });
        pending::<()>().await;
    };
    let handle = shards().start(config("shard-1"), main).unwrap();
    assert_joins(handle, panicked("shard-1"));
    assert!(raised(&dropped), "the main future drops");
}

#[test]
fn a_panic_in_the_main_future_ends_the_shard() {
    let dropped = flag();
    let guard = Dropped(Arc::clone(&dropped));
    let main = move |tasks: Tasks| async move {
        tasks.spawn(async move {
            let _task = guard;
            pending::<()>().await;
        });
        yield_now().await;
        panic!("main");
    };
    let handle = shards().start(config("shard-2"), main).unwrap();
    assert_joins(handle, panicked("shard-2"));
    assert!(raised(&dropped), "the task drops");
}

#[test]
fn a_panic_in_the_main_function_ends_the_shard() {
    let main = |_: Tasks| -> Ready<()> { panic!("main") };
    let handle = shards().start(config("shard-3"), main).unwrap();
    assert_joins(handle, panicked("shard-3"));
}

#[test]
fn a_panic_in_the_drop_of_a_task_ends_the_shard() {
    let main = |tasks: Tasks| async move {
        let bomb = Bomb;
        tasks.spawn(async move {
            let _bomb = bomb;
            pending::<()>().await;
        });
    };
    let handle = shards().start(config("shard-4"), main).unwrap();
    assert_joins(handle, panicked("shard-4"));
}

#[test]
fn a_panic_in_the_drop_of_the_main_future_after_a_panic_in_a_task_ends_the_shard() {
    let main = |tasks: Tasks| {
        let bomb = Bomb;
        async move {
            let _bomb = bomb;
            tasks.spawn(async { panic!("task") });
            pending::<()>().await;
        }
    };
    let handle = shards().start(config("shard-5"), main).unwrap();
    assert_joins(handle, panicked("shard-5"));
}

#[test]
fn a_task_queued_behind_a_panic_does_not_run() {
    let ran = flag();
    let late = Arc::clone(&ran);
    let main = move |tasks: Tasks| async move {
        tasks.spawn(async { panic!("task") });
        tasks.spawn(async move { late.store(true, Ordering::SeqCst) });
        pending::<()>().await;
    };
    let handle = shards().start(config("shard-1"), main).unwrap();
    assert_joins(handle, panicked("shard-1"));
    assert!(!raised(&ran), "a task after the panic ran");
}

#[test]
#[expect(clippy::disallowed_methods, reason = "the test bounds the start")]
fn start_returns_while_the_shard_runs() {
    let go = flag();
    let wait = Arc::clone(&go);
    let main = move |_: Tasks| async move {
        while !raised(&wait) {
            yield_now().await;
        }
    };
    let (done, started) = mpsc::channel();
    std::thread::spawn(move || done.send(shards().start(config("shard-8"), main)));
    let started = started.recv_timeout(Duration::from_secs(10));
    go.store(true, Ordering::SeqCst);
    let handle = started
        .expect("start returns while the shard runs")
        .unwrap();
    assert_joins(handle, Ok(()));
}

/// Spawns a task when it drops.
struct Spawns {
    tasks: Tasks,
    ran: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Drop for Spawns {
    fn drop(&mut self) {
        let ran = Arc::clone(&self.ran);
        let dropped = Dropped(Arc::clone(&self.dropped));
        self.tasks.spawn(async move {
            let _dropped = dropped;
            ran.store(true, Ordering::SeqCst);
        });
    }
}

#[test]
fn a_task_spawned_as_the_shard_ends_drops_without_a_run() {
    let (ran, dropped) = (flag(), flag());
    let (late, gone) = (Arc::clone(&ran), Arc::clone(&dropped));
    let main = move |tasks: Tasks| async move {
        let spawns = Spawns {
            tasks: tasks.clone(),
            ran: late,
            dropped: gone,
        };
        tasks.spawn(async move {
            let _spawns = spawns;
            pending::<()>().await;
        });
    };
    let handle = shards().start(config("shard-6"), main).unwrap();
    assert_joins(handle, Ok(()));
    assert!(!raised(&ran));
    assert!(raised(&dropped));
}

#[test]
fn a_task_spawned_as_the_main_future_ends_drops_without_a_run() {
    let (ran, dropped) = (flag(), flag());
    let (late, gone) = (Arc::clone(&ran), Arc::clone(&dropped));
    let main = move |tasks: Tasks| {
        let spawns = Spawns {
            tasks,
            ran: late,
            dropped: gone,
        };
        async move {
            let _spawns = spawns;
        }
    };
    let handle = shards().start(config("shard-7"), main).unwrap();
    assert_joins(handle, Ok(()));
    assert!(!raised(&ran));
    assert!(raised(&dropped));
}

#[test]
fn a_name_with_a_nul_starts_and_panicked_keeps_the_whole_name() {
    let seen = Arc::new(Mutex::new(None));
    let name = Arc::clone(&seen);
    let main = move |tasks: Tasks| {
        let os = std::thread::current().name().map(str::to_owned);
        *name.lock().unwrap() = os;
        tasks.spawn(async { panic!("task") });
        pending::<()>()
    };
    let handle = shards().start(config("shard\0seven"), main).unwrap();
    assert_joins(handle, panicked("shard\0seven"));
    assert_eq!(*seen.lock().unwrap(), Some("shard".to_owned()));
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::common::affinity;

    #[test]
    fn each_core_pins_its_shard_to_its_cpu_of_the_affinity_set() {
        let cpus = affinity();
        let shards = shards();
        assert!(shards.pinnable());
        assert_eq!(shards.cores().get(), cpus.len());
        for (core, &cpu) in cpus.iter().enumerate() {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let pinned = Arc::clone(&seen);
            let config = Config {
                name: format!("shard-{core}"),
                core: Some(core),
            };
            let main = move |_| async move { *pinned.lock().unwrap() = affinity() };
            let handle = shards.start(config, main).unwrap();
            assert_joins(handle, Ok(()));
            assert_eq!(*seen.lock().unwrap(), [cpu], "core {core}");
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod other {
    use super::*;

    #[test]
    fn shards_cannot_pin() {
        assert!(!shards().pinnable());
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "the oracle of the count")]
    fn the_cores_are_the_available_parallelism() {
        let count = std::thread::available_parallelism().unwrap();
        assert_eq!(shards().cores(), count);
    }
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_in_a_task_ends_the_shard() {
    let main = |tasks: Tasks| async move {
        tasks.spawn(async { panic_any(Relay(2)) });
        pending::<()>().await;
    };
    let handle = shards().start(config("shard-9"), main).unwrap();
    assert_joins(handle, panicked("shard-9"));
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_in_the_drop_of_a_task_ends_the_shard() {
    let main = |tasks: Tasks| async move {
        tasks.spawn(Relayed);
        pending::<()>().await;
    };
    let handle = shards().start(config("shard-10"), main).unwrap();
    assert_joins(handle, panicked("shard-10"));
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_in_the_main_future_ends_the_shard() {
    let main = |_: Tasks| async {
        yield_now().await;
        panic_any(Relay(2))
    };
    let handle = shards().start(config("shard-11"), main).unwrap();
    assert_joins(handle, panicked("shard-11"));
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_as_a_tokio_task_drops_ends_the_shard() {
    let main = |_: Tasks| async {
        // Tokio catches the first two panics of the chain at the drop of the runtime.
        drop(tokio::task::spawn_local(Stuck(4)));
    };
    let handle = shards().start(config("shard-12"), main).unwrap();
    assert_joins(handle, panicked("shard-12"));
}

#[test]
fn a_panic_in_the_poll_of_a_tokio_task_does_not_end_the_shard() {
    let main = |_: Tasks| async {
        let (sender, dropped) = oneshot::channel::<()>();
        drop(tokio::task::spawn_local(async move {
            let _sender = sender;
            panic!("tokio task");
        }));
        dropped.await.expect_err("the panic drops the sender");
    };
    let handle = shards().start(config("shard-13"), main).unwrap();
    assert_joins(handle, Ok(()));
}

#[test]
fn a_panic_in_the_drop_of_a_tokio_task_does_not_end_the_shard() {
    // The payload of the panic, `Relay(0)`, does not panic in its drop.
    let main = |_: Tasks| async { drop(tokio::task::spawn_local(Stuck(0))) };
    let handle = shards().start(config("shard-14"), main).unwrap();
    assert_joins(handle, Ok(()));
}

#[test]
fn a_panic_whose_payload_panics_in_its_drop_in_a_tokio_task_ends_the_shard() {
    let main = |_: Tasks| async {
        drop(tokio::task::spawn_local(async { panic_any(Relay(2)) }));
        pending::<()>().await;
    };
    let handle = shards().start(config("shard-15"), main).unwrap();
    assert_joins(handle, panicked("shard-15"));
}

#[test]
fn a_panic_in_the_poll_and_then_the_drop_of_a_tokio_task_aborts_the_process() {
    assert_aborts(|| {
        let main = |_: Tasks| async {
            drop(tokio::task::spawn_local(Armed {
                faulty: true,
                _bomb: Bomb,
            }));
            pending::<()>().await;
        };
        let handle = shards().start(config("shard-16"), main).unwrap();
        assert_joins(handle, Ok(()));
    });
}
