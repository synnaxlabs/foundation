//! Tests of a simulated run through the `env` handles that production code gets.

use std::collections::BTreeSet;
use std::future::pending;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::{Config, Error, Sim, node};
use env::threads::Error as Thread;
use proptest::prelude::*;
use types::time::{Monotonic, Span, Stamp};

/// Completes on its second poll, so other ready tasks may run first.
struct Yield(bool);

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

fn yield_now() -> Yield {
    Yield(false)
}

fn shard(name: &str) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: None,
    }
}

/// A run with a small step limit, so a broken scheduler fails fast.
fn sim(seed: u64) -> Sim {
    Sim::new(Config {
        seed,
        steps_max: 10_000,
    })
}

/// Runs `tasks` tasks on each of two nodes. Each task logs three steps.
fn trace(seed: u64, tasks: usize) -> Vec<String> {
    let mut sim = sim(seed);
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for n in 0..2 {
        let node = sim.node(node::Config::default());
        let clock = node.clock();
        let log = Arc::clone(&log);
        let handle =
            node.shards()
                .start(shard(&format!("n{n}")), move |spawner| async move {
                    for t in 0..tasks {
                        let log = Arc::clone(&log);
                        spawner.spawn(async move {
                            for step in 0..3 {
                                log.lock().unwrap().push(format!("{n}.{t}.{step}"));
                                yield_now().await;
                            }
                        });
                    }
                    clock.sleep(Span::SECOND).await;
                });
        handles.push(handle.unwrap());
    }
    sim.run().unwrap();
    for handle in handles {
        handle.join().unwrap();
    }
    let log = log.lock().unwrap().clone();
    assert_eq!(log.len(), 2 * tasks * 3, "every step ran: {log:?}");
    log
}

proptest! {
    #[test]
    fn the_same_seed_gives_the_same_order(seed: u64, tasks in 1usize..6) {
        prop_assert_eq!(trace(seed, tasks), trace(seed, tasks));
    }
}

#[test]
fn different_seeds_give_different_orders() {
    let orders: BTreeSet<Vec<String>> = (0..16).map(|seed| trace(seed, 4)).collect();
    assert_eq!(orders.len(), 16, "each seed gives its own order");
}

#[test]
fn time_stands_still_while_a_task_is_ready() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let readings = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&readings);
    let handle = node.shards().start(shard("shard-0"), move |_| async move {
        for _ in 0..10 {
            out.lock().unwrap().push(clock.now());
            yield_now().await;
        }
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    let start = node::Config::default().monotonic;
    assert_eq!(*readings.lock().unwrap(), vec![start; 10]);
}

#[test]
fn run_moves_time_to_each_timer() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        monotonic: Monotonic(5),
        ..node::Config::default()
    });
    let clock = node.clock();
    let readings = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&readings);
    let handle = node.shards().start(shard("shard-0"), move |_| async move {
        for _ in 0..3 {
            clock.sleep(Span::SECOND).await;
            out.lock().unwrap().push(clock.now());
        }
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    let s = Span::SECOND.nanos().unsigned_abs();
    assert_eq!(
        *readings.lock().unwrap(),
        vec![Monotonic(5 + s), Monotonic(5 + 2 * s), Monotonic(5 + 3 * s)]
    );
}

#[test]
fn run_for_moves_time_by_the_span_and_leaves_waiting_threads() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let woke = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&woke);
    let handle = node.shards().start(shard("shard-0"), move |_| async move {
        clock
            .sleep(Span::from_nanos(10 * Span::SECOND.nanos()))
            .await;
        flag.store(true, Ordering::Relaxed);
    });
    let start = node.clock().now();
    sim.run_for(Span::from_nanos(4 * Span::SECOND.nanos()))
        .unwrap();
    assert_eq!(
        node.clock().now(),
        start + Span::from_nanos(4 * Span::SECOND.nanos())
    );
    assert!(!woke.load(Ordering::Relaxed), "the timer is not due");
    sim.run_for(Span::from_nanos(6 * Span::SECOND.nanos()))
        .unwrap();
    assert!(woke.load(Ordering::Relaxed), "the timer fired at 10 s");
    handle.unwrap().join().unwrap();
}

#[test]
fn a_reset_sleep_fires_at_its_new_deadline() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let reading = Arc::new(Mutex::new(None));
    let out = Arc::clone(&reading);
    let handle = node.shards().start(shard("shard-0"), move |_| async move {
        let start = clock.now();
        let mut sleep = clock.sleep_until(start + Span::SECOND);
        let woke =
            std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut sleep).poll(cx)));
        assert_eq!(woke.await, Poll::Pending, "the first deadline is not due");
        sleep.reset(start + Span::MILLISECOND);
        sleep.await;
        *out.lock().unwrap() = Some(clock.now() - start);
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    assert_eq!(*reading.lock().unwrap(), Some(Span::MILLISECOND));
}

#[test]
fn every_node_shares_one_epoch_that_never_changes() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let epoch = a.clock().epoch();
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(a.clock().epoch(), epoch);
    assert_eq!(b.clock().epoch(), epoch);
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn each_node_reads_its_own_wall_clock() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config {
        wall: Stamp::from_nanos(-7),
        ..node::Config::default()
    });
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(a.wall().now(), node::Config::default().wall + Span::SECOND);
    assert_eq!(b.wall().now(), Stamp::from_nanos(-7) + Span::SECOND);
}

fn bytes(seed: u64) -> ([u8; 16], [u8; 16]) {
    let mut sim = sim(seed);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let (mut x, mut y) = ([0; 16], [0; 16]);
    a.entropy().fill(&mut x);
    b.entropy().fill(&mut y);
    (x, y)
}

#[test]
fn entropy_comes_from_the_seed_with_a_stream_per_node() {
    let (a, b) = bytes(9);
    assert_eq!((a, b), bytes(9));
    assert_ne!(a, b, "each node has its own stream");
    assert_ne!(bytes(10).0, a, "each seed has its own stream");
}

#[test]
fn clones_of_a_node_entropy_read_one_stream() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let (mut x, mut y) = ([0; 8], [0; 8]);
    node.entropy().fill(&mut x);
    node.entropy().fill(&mut y);
    assert_ne!(x, y, "the second read continues the stream");
}

#[test]
fn a_panic_ends_the_run_and_its_thread() {
    let mut sim = sim(7);
    let node = sim.node(node::Config::default());
    let handle = node
        .shards()
        .start(shard("shard-0"), |_| async { panic!("boom") });
    let e = sim.run().unwrap_err();
    assert_eq!(
        e,
        Error::Panicked {
            thread: "shard-0".into(),
            message: "boom".into(),
            seed: 7,
        }
    );
    assert_eq!(
        e.to_string(),
        "thread shard-0 panicked: boom; replay with seed 0x7"
    );
    assert_eq!(
        handle.unwrap().join(),
        Err(Thread::Panicked {
            name: "shard-0".into()
        })
    );
}

#[test]
fn a_panic_in_a_spawned_task_ends_its_shard() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let handle = node
        .shards()
        .start(shard("shard-0"), move |tasks| async move {
            tasks.spawn(async { panic!("task {}", 3) });
            clock.sleep(Span::SECOND).await;
        });
    assert_eq!(
        sim.run(),
        Err(Error::Panicked {
            thread: "shard-0".into(),
            message: "task 3".into(),
            seed: 0,
        })
    );
    assert_eq!(
        handle.unwrap().join(),
        Err(Thread::Panicked {
            name: "shard-0".into()
        })
    );
    sim.run().unwrap();
}

#[test]
fn a_run_stops_past_the_step_limit() {
    let mut sim = Sim::new(Config {
        seed: 0,
        steps_max: 100,
    });
    let node = sim.node(node::Config::default());
    let _handle = node.shards().start(shard("shard-0"), |_| async {
        loop {
            yield_now().await;
        }
    });
    let e = sim.run().unwrap_err();
    assert_eq!(e, Error::Steps { max: 100, seed: 0 });
    assert_eq!(
        e.to_string(),
        "the run passed 100 steps; replay with seed 0x0"
    );
}

#[test]
fn a_run_with_threads_that_nothing_can_wake_is_stuck() {
    let mut sim = sim(3);
    let node = sim.node(node::Config::default());
    let _a = node.shards().start(shard("a"), |_| pending::<()>());
    let _b = node.threads().start("b", pending::<()>);
    let e = sim.run().unwrap_err();
    assert_eq!(
        e,
        Error::Stuck {
            threads: vec!["a".into(), "b".into()],
            seed: 3,
        }
    );
    assert_eq!(
        e.to_string(),
        "threads a, b wait, and nothing can wake them; replay with seed 0x3"
    );
}

#[test]
fn a_shard_cannot_pin_past_the_node_cores() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    assert_eq!(node.shards().cores().get(), 4);
    let config = env::shards::Config {
        name: "shard-4".into(),
        core: Some(4),
    };
    assert_eq!(
        node.shards().start(config, |_| async {}).unwrap_err(),
        Thread::Pin {
            name: "shard-4".into(),
            core: 4
        }
    );
    let config = env::shards::Config {
        name: "shard-3".into(),
        core: Some(3),
    };
    let handle = node.shards().start(config, |_| async {});
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
}

#[test]
fn a_thread_name_with_a_nul_byte_starts() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let shard = node
        .shards()
        .start(shard("a\0b"), |_| async { panic!("shard") });
    let thread = node.threads().start("c\0d", || async {});
    let e = sim.run().unwrap_err();
    assert!(matches!(e, Error::Panicked { thread, .. } if thread == "a\0b"));
    assert_eq!(
        shard.unwrap().join(),
        Err(Thread::Panicked {
            name: "a\0b".into()
        })
    );
    sim.run().unwrap();
    thread.unwrap().join().unwrap();
}

#[test]
#[should_panic(expected = "thread shard-0 has not ended; run the sim until it ends")]
fn join_panics_before_the_thread_ends() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |_| async {});
    drop(handle.unwrap().join());
}

#[test]
#[should_panic(expected = "a sleep needs a thread that the sim started")]
fn a_sleep_outside_the_sim_panics() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    drop(node.clock().sleep(Span::SECOND));
}

#[test]
fn a_sleep_on_the_clock_of_another_node_panics() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let clock = a.clock();
    let _handle = b.shards().start(shard("b-0"), move |_| async move {
        clock.sleep(Span::SECOND).await;
    });
    assert_eq!(
        sim.run(),
        Err(Error::Panicked {
            thread: "b-0".into(),
            message: "a clock of node 0 sleeps on a thread of node 1".into(),
            seed: 0,
        })
    );
}

/// Sets its flag when dropped.
struct Guard(Arc<AtomicBool>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
fn a_shard_drops_its_tasks_when_its_main_future_completes() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dropped);
    let handle = node
        .shards()
        .start(shard("shard-0"), move |tasks| async move {
            let guard = Guard(flag);
            tasks.spawn(async move {
                let _guard = guard;
                pending::<()>().await;
            });
            yield_now().await;
        });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    assert!(
        dropped.load(Ordering::Relaxed),
        "the waiting task was dropped"
    );
}

#[test]
fn spawned_tasks_spawn_more_tasks() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    let handle = node
        .shards()
        .start(shard("shard-0"), move |tasks| async move {
            let inner = tasks.clone();
            tasks.spawn(async move {
                inner.spawn(async move { flag.store(true, Ordering::Relaxed) });
            });
            clock.sleep(Span::SECOND).await;
        });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    assert!(ran.load(Ordering::Relaxed), "the inner task ran");
}

#[test]
fn a_dedicated_thread_runs_its_body_and_sleeps() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let slept = Arc::new(Mutex::new(None));
    let out = Arc::clone(&slept);
    let handle = node.threads().start("modbus-poll", move || async move {
        let start = clock.now();
        clock.sleep(Span::MILLISECOND).await;
        *out.lock().unwrap() = Some(clock.now() - start);
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    assert_eq!(*slept.lock().unwrap(), Some(Span::MILLISECOND));
}

#[test]
fn a_task_that_wakes_itself_as_it_completes_ends() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let handle = node
        .shards()
        .start(shard("shard-0"), move |tasks| async move {
            tasks.spawn(std::future::poll_fn(|cx| {
                cx.waker().wake_by_ref();
                Poll::Ready(())
            }));
            clock.sleep(Span::SECOND).await;
        });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
}

#[test]
fn a_shard_started_by_a_task_runs() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let shards = node.shards();
    let inner = Arc::new(Mutex::new(None));
    let out = Arc::clone(&inner);
    let handle = node.shards().start(shard("outer"), move |_| async move {
        *out.lock().unwrap() = Some(shards.start(shard("inner"), |_| async {}));
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    let inner = inner.lock().unwrap().take().unwrap();
    inner.unwrap().join().unwrap();
}

#[test]
fn a_run_with_no_threads_ends_at_once() {
    let mut sim = sim(0);
    sim.node(node::Config::default());
    sim.run().unwrap();
}

#[test]
fn dropping_the_sim_drops_waiting_tasks_and_unstarted_threads() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let (waiting, unstarted) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let guard = Guard(Arc::clone(&waiting));
    let _a = node.shards().start(shard("a"), move |tasks| async move {
        tasks.spawn(async move {
            let _guard = guard;
            pending::<()>().await;
        });
        pending::<()>().await;
    });
    sim.run_for(Span::SECOND).unwrap();
    let guard = Guard(Arc::clone(&unstarted));
    let _b = node.shards().start(shard("b"), move |_| async move {
        let _guard = guard;
    });
    drop(sim);
    assert!(
        waiting.load(Ordering::Relaxed),
        "the waiting task was dropped"
    );
    assert!(
        unstarted.load(Ordering::Relaxed),
        "the unstarted thread was dropped"
    );
}

#[test]
fn debug_names_the_config_and_the_node() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    assert_eq!(
        format!("{sim:?}"),
        "Sim { config: Config { seed: 0, steps_max: 10000 }, .. }"
    );
    assert_eq!(format!("{node:?}"), "Node(0)");
}

#[test]
fn a_dropped_sleep_does_not_move_time() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let handle = node.shards().start(shard("shard-0"), move |_| async move {
        let mut sleep = clock.sleep(Span::SECOND);
        let poll =
            std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut sleep).poll(cx)));
        assert_eq!(poll.await, Poll::Pending, "the sleep is not due");
    });
    let start = node.clock().now();
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    assert_eq!(node.clock().now(), start, "no timer waits");
}

#[test]
#[should_panic(expected = "a sleep needs a thread that the sim started")]
fn a_sleep_outside_the_sim_panics_after_a_run() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |_| async {});
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    drop(node.clock().sleep(Span::SECOND));
}

#[test]
fn a_panic_drops_the_other_tasks_of_its_thread() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Guard(Arc::clone(&dropped));
    let _handle = node.shards().start(shard("shard-0"), |tasks| async move {
        tasks.spawn(async move {
            let _guard = guard;
            pending::<()>().await;
        });
        tasks.spawn(async { panic!("boom") });
        pending::<()>().await;
    });
    assert!(matches!(sim.run(), Err(Error::Panicked { .. })));
    assert!(dropped.load(Ordering::Relaxed));
}

/// Spawns a task that sets its flag, when dropped.
struct Spawner(env::tasks::Tasks, Arc<AtomicBool>);

impl Drop for Spawner {
    fn drop(&mut self) {
        let ran = Arc::clone(&self.1);
        self.0
            .spawn(async move { ran.store(true, Ordering::Relaxed) });
    }
}

#[test]
fn a_task_spawned_as_its_shard_ends_never_runs() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    let handle = node.shards().start(shard("shard-0"), |tasks| async move {
        let spawner = Spawner(tasks.clone(), flag);
        tasks.spawn(async move {
            let _spawner = spawner;
            pending::<()>().await;
        });
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    assert!(!ran.load(Ordering::Relaxed));
}

/// Panics when dropped.
struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("bomb");
    }
}

#[test]
fn a_panic_in_the_drop_of_a_task_ends_the_run_and_its_thread() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |tasks| async move {
        let bomb = Bomb;
        tasks.spawn(async move {
            let _bomb = bomb;
            pending::<()>().await;
        });
    });
    assert_eq!(
        sim.run(),
        Err(Error::Panicked {
            thread: "shard-0".into(),
            message: "bomb".into(),
            seed: 0,
        })
    );
    assert_eq!(
        handle.unwrap().join(),
        Err(Thread::Panicked {
            name: "shard-0".into()
        })
    );
}

#[test]
fn a_sleep_past_the_end_of_true_time_never_fires() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let _handle = node.shards().start(shard("shard-0"), move |_| async move {
        clock.sleep_until(Monotonic(u64::MAX)).await;
    });
    assert_eq!(
        sim.run(),
        Err(Error::Stuck {
            threads: vec!["shard-0".into()],
            seed: 0,
        })
    );
}

/// A node whose wall clock ends one second after the run starts.
fn ending(sim: &mut Sim) -> node::Node {
    sim.node(node::Config {
        wall: Stamp::from_nanos(i64::MAX - Span::SECOND.nanos()),
        ..node::Config::default()
    })
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn the_clocks_read_up_to_the_end_of_true_time() {
    let mut sim = sim(0);
    let node = ending(&mut sim);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(node.wall().now(), Stamp::from_nanos(i64::MAX));
    let start = node::Config::default().monotonic;
    assert_eq!(node.clock().now(), start + Span::SECOND);
}

#[test]
#[should_panic(expected = "run_for(1ns) passes the end of true time")]
fn run_for_past_the_end_of_true_time_panics() {
    let mut sim = sim(0);
    let _node = ending(&mut sim);
    sim.run_for(Span::SECOND).unwrap();
    drop(sim.run_for(Span::NANOSECOND));
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn a_node_added_later_reads_its_config_now() {
    let mut sim = sim(0);
    sim.node(node::Config::default());
    sim.run_for(Span::SECOND).unwrap();
    let config = node::Config::default();
    let node = sim.node(config);
    assert_eq!(node.clock().now(), config.monotonic);
    assert_eq!(node.wall().now(), config.wall);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(node.clock().now(), config.monotonic + Span::SECOND);
}

#[test]
fn run_for_runs_a_negative_span_as_zero() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |_| yield_now());
    sim.run_for(Span::from_nanos(-1)).unwrap();
    handle.unwrap().join().unwrap();
    assert_eq!(node.clock().now(), node::Config::default().monotonic);
}

/// Counts the polls of its future.
struct Counted<F>(Pin<Box<F>>, Arc<AtomicUsize>);

impl<F: Future> Future for Counted<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        self.1.fetch_add(1, Ordering::Relaxed);
        self.0.as_mut().poll(cx)
    }
}

#[test]
fn a_timer_wakes_its_task_only_when_due() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let (early, late) = (node.clock(), node.clock());
    let polls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&polls);
    let a = node.shards().start(shard("early"), move |_| async move {
        early.sleep(Span::SECOND).await;
    });
    let b = node.shards().start(shard("late"), move |_| {
        let sleep = async move { late.sleep(Span::from_nanos(2_000_000_000)).await };
        Counted(Box::pin(sleep), count)
    });
    sim.run().unwrap();
    a.unwrap().join().unwrap();
    b.unwrap().join().unwrap();
    assert_eq!(polls.load(Ordering::Relaxed), 2);
}
