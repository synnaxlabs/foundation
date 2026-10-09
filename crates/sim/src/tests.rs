//! Tests of a simulated run through the `env` handles that production code gets.

mod chance;
mod crash;
mod files;
mod name;
mod net;
mod run_on;
mod serial;
mod shards;
mod tcp;

use std::collections::BTreeSet;
use std::future::{Ready, pending};
use std::net::SocketAddr;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::drivers::yield_now;
use crate::{Config, Error, Sim, link, node};
use env::thread;
use proptest::prelude::*;
use types::time::{Monotonic, Span, Stamp};

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
        ..Config::default()
    })
}

/// A run of two nodes, `a` and `b`, with `link` between them.
fn pair(seed: u64, link: link::Config) -> (Sim, node::Node, node::Node) {
    let mut sim = Sim::new(Config {
        seed,
        steps_max: 1_000_000,
        link,
    });
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    (sim, a, b)
}

/// The address of `port` on the IPv4 address of `node`.
fn at(node: &node::Node, port: u16) -> SocketAddr {
    SocketAddr::new(node.addresses()[0], port)
}

/// The receiver's clock `span` after the run starts.
fn after(span: Span) -> Monotonic {
    node::Config::default().monotonic + span
}

/// The error of an I/O call on a failed socket or listener.
const EIO: env::net::Error = env::net::Error::Io { code: crate::EIO };

/// The default delay.
fn delay() -> Span {
    link::Config::default().delay
}

/// The error of a run whose thread `thread` panicked with `message`.
fn panicked(thread: &str, message: &str) -> Error {
    Error::Panicked {
        thread: thread.into(),
        message: message.into(),
        seed: 0,
    }
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
    let reading = sim.run_on(&node, |node, _| async move {
        let clock = node.clock();
        let start = clock.now();
        let mut sleep = clock.sleep_until(start + Span::SECOND);
        let woke =
            std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut sleep).poll(cx)));
        assert_eq!(woke.await, Poll::Pending, "the first deadline is not due");
        sleep.reset(start + Span::MILLISECOND);
        sleep.await;
        clock.now() - start
    });
    assert_eq!(reading, Ok(Span::MILLISECOND));
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
    assert_eq!(
        a.wall().now().time,
        node::Config::default().wall + Span::SECOND
    );
    assert_eq!(b.wall().now().time, Stamp::from_nanos(-7) + Span::SECOND);
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
        Err(thread::Panicked {
            name: "shard-0".into()
        })
    );
}

#[test]
fn a_panic_in_the_call_of_main_ends_the_run_and_its_thread() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let main = |_: env::tasks::Tasks| -> Ready<()> { panic!("main") };
    let handle = node.shards().start(shard("shard-0"), main);
    assert_panicked(&mut sim, handle.unwrap(), "main");
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
        Err(thread::Panicked {
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
        ..Config::default()
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
fn a_shard_pins_to_the_last_node_core() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    assert_eq!(node.shards().cores().get(), 4);
    let config = env::shards::Config {
        name: "shard-3".into(),
        core: Some(3),
    };
    let handle = node.shards().start(config, |_| async {});
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
}

#[test]
#[should_panic(expected = "shard-4 asks for core 4 of 4")]
fn a_shard_past_the_node_cores_panics() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let config = env::shards::Config {
        name: "shard-4".into(),
        core: Some(4),
    };
    drop(node.shards().start(config, |_| async {}));
}

#[test]
fn a_thread_name_with_a_nul_byte_starts() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let shard = node
        .shards()
        .start(shard("a\0b"), |_| async { panic!("shard") });
    let handle = node.threads().start("c\0d", || async {});
    let e = sim.run().unwrap_err();
    assert!(matches!(e, Error::Panicked { thread, .. } if thread == "a\0b"));
    assert_eq!(
        shard.unwrap().join(),
        Err(thread::Panicked {
            name: "a\0b".into()
        })
    );
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
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
            message: "a sleep of node 0 runs on a thread of node 1".into(),
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
fn the_digest_holds_each_poll() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let _idle = node.shards().start(shard("idle"), |_| async {}).unwrap();
    sim.run().unwrap();
    assert_ne!(sim.digest(), Sim::new(Config::default()).digest());
}

#[test]
fn the_digest_holds_the_order_of_the_picks() {
    let digest = |seed| {
        let mut sim = sim(seed);
        let node = sim.node(node::Config::default());
        let handles: Vec<_> = (["a", "b"].into_iter())
            .map(|name| {
                let start = node.shards().start(shard(name), |_| async {
                    for _ in 0..3 {
                        yield_now().await;
                    }
                });
                start.unwrap()
            })
            .collect();
        sim.run().unwrap();
        for handle in handles {
            handle.join().unwrap();
        }
        sim.digest()
    };
    let digests: BTreeSet<_> = (0..8).map(digest).collect();
    assert!(digests.len() > 1, "{digests:?}");
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
        "Sim { config: Config { seed: 0, steps_max: 10000, link: Config { \
         delay: Span(250000), jitter: Span(0), loss: 0.0, duplication: 0.0, \
         mtu: 1500, rate: None } }, .. }"
    );
    assert_eq!(format!("{node:?}"), "Node(0)");
}

#[test]
fn a_dropped_sleep_does_not_move_time() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let _waits = node.shards().start(shard("shard-0"), move |_| async move {
        let mut sleep = clock.sleep(Span::SECOND);
        let poll =
            std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut sleep).poll(cx)));
        assert_eq!(poll.await, Poll::Pending, "the sleep is not due");
        drop(sleep);
        pending::<()>().await;
    });
    let start = node.clock().now();
    let stuck = Error::Stuck {
        threads: vec!["shard-0".into()],
        seed: 0,
    };
    assert_eq!(sim.run(), Err(stuck));
    assert_eq!(node.clock().now(), start, "no timer waits");
}

#[test]
fn a_leaked_sleep_stops_at_the_end_of_its_thread() {
    for panics in [false, true] {
        let mut sim = sim(0);
        let node = sim.node(node::Config::default());
        let clock = node.clock();
        let handle = node.shards().start(shard("shard-0"), move |_| async move {
            let sleep = Box::leak(Box::new(clock.sleep(Span::SECOND)));
            let poll =
                std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut *sleep).poll(cx)));
            assert_eq!(poll.await, Poll::Pending, "the sleep is not due");
            assert!(!panics, "boom");
        });
        let start = node.clock().now();
        let panicked = Error::Panicked {
            thread: "shard-0".into(),
            message: "boom".into(),
            seed: 0,
        };
        assert_eq!(sim.run(), if panics { Err(panicked) } else { Ok(()) });
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(handle.unwrap().join().is_ok(), !panics);
        assert_eq!(node.clock().now(), start, "panics: {panics}");
    }
}

/// A leaked sleep that a drop polls at the end of its thread.
mod polled_in_a_drop {
    use super::*;

    /// Polls its sleep as it drops.
    struct Poller(&'static mut env::clock::Sleep);

    impl Drop for Poller {
        fn drop(&mut self) {
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(Pin::new(&mut *self.0).poll(&mut cx).is_pending());
        }
    }

    /// How the thread of the [`Poller`] ends.
    #[derive(Clone, Copy)]
    enum Ending {
        Finish,
        Panic,
        Crash,
    }

    /// Asserts that the sleep stops when its thread ends by `ending`: no later run
    /// moves true time.
    fn check(ending: Ending) {
        let mut sim = sim(0);
        let node = sim.node(node::Config::default());
        let clock = node.clock();
        let _handle = node
            .shards()
            .start(shard("shard-0"), move |tasks| async move {
                let poller = Poller(Box::leak(Box::new(clock.sleep(Span::SECOND))));
                tasks.spawn(async move {
                    let _poller = poller;
                    pending::<()>().await;
                });
                yield_now().await;
                match ending {
                    Ending::Finish => {}
                    Ending::Panic => panic!("boom"),
                    Ending::Crash => pending().await,
                }
            });
        let start = node.clock().now();
        let ran = sim.run();
        match ending {
            Ending::Finish => assert_eq!(ran, Ok(())),
            Ending::Panic => assert_eq!(ran, Err(panicked("shard-0", "boom"))),
            Ending::Crash => {
                let stuck = Error::Stuck {
                    threads: vec!["shard-0".into()],
                    seed: 0,
                };
                assert_eq!(ran, Err(stuck));
                sim.crash(&node, crate::Crash::Process);
            }
        }
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.clock().now(), start);
    }

    #[test]
    fn a_leaked_sleep_stops_at_a_finish() {
        check(Ending::Finish);
    }

    #[test]
    fn a_leaked_sleep_stops_at_a_panic() {
        check(Ending::Panic);
    }

    #[test]
    fn a_leaked_sleep_stops_at_a_crash() {
        check(Ending::Crash);
    }
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

/// Panics with its message when dropped.
struct Bomb(&'static str);

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("{}", self.0);
    }
}

/// Runs `child` in a copy of this test in a child process, and asserts that the child
/// aborts at a panic that unwinds into the unwind of another panic.
#[cfg(unix)]
fn assert_aborts(child: impl FnOnce()) {
    use std::os::unix::process::ExitStatusExt;
    const CHILD: &str = "SIM_TEST_CHILD";
    const SIGABRT: i32 = 6;
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent sets it for the child"
    )]
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    let thread = std::thread::current();
    let test = thread.name().expect("invariant: libtest names the thread");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.signal(), Some(SIGABRT), "{stderr}");
    assert!(
        stderr.contains("panic in a destructor during cleanup"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn a_panic_over_a_local_that_panics_in_its_drop_in_a_task_aborts_the_process() {
    assert_aborts(|| {
        let mut sim = sim(0);
        let node = sim.node(node::Config::default());
        let handle = node.shards().start(shard("shard-0"), |tasks| async move {
            tasks.spawn(async {
                let _bomb = Bomb("bomb");
                panic!("task");
            });
            pending::<()>().await;
        });
        assert_panicked(&mut sim, handle.unwrap(), "task");
    });
}

#[test]
fn a_panic_in_the_drop_of_a_task_ends_the_run_and_its_thread() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |tasks| async move {
        let bomb = Bomb("bomb");
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
        Err(thread::Panicked {
            name: "shard-0".into()
        })
    );
}

/// Starts a shard that spawns `bombs` pending tasks that hold a [`Bomb`], then
/// panics with "boom" when `panics`, or ends.
fn start_bombs(node: &node::Node, bombs: usize, panics: bool) -> thread::Handle {
    let handle = node
        .shards()
        .start(shard("shard-0"), move |tasks| async move {
            for _ in 0..bombs {
                let bomb = Bomb("bomb");
                tasks.spawn(async move {
                    let _bomb = bomb;
                    pending::<()>().await;
                });
            }
            assert!(!panics, "boom");
        });
    handle.unwrap()
}

/// Asserts that the run reports `message` for "shard-0", and that `handle` joins as
/// panicked.
fn assert_panicked(sim: &mut Sim, handle: thread::Handle, message: &str) {
    assert_eq!(
        sim.run(),
        Err(Error::Panicked {
            thread: "shard-0".into(),
            message: message.into(),
            seed: 0,
        })
    );
    assert_eq!(
        handle.join(),
        Err(thread::Panicked {
            name: "shard-0".into()
        })
    );
}

#[test]
fn a_panic_in_a_drop_after_a_panic_in_a_poll_gives_both_panics() {
    let mut sim = sim(0);
    let handle = start_bombs(&sim.node(node::Config::default()), 1, true);
    assert_panicked(&mut sim, handle, "boom, then a drop panicked: bomb");
}

#[test]
fn two_panics_in_drops_after_a_panic_in_a_poll_give_each_panic() {
    let mut sim = sim(0);
    let handle = start_bombs(&sim.node(node::Config::default()), 2, true);
    let message = "boom, then a drop panicked: bomb, then a drop panicked: bomb";
    assert_panicked(&mut sim, handle, message);
}

#[test]
fn two_panics_in_drops_after_a_shard_ends_give_each_panic() {
    let mut sim = sim(0);
    let handle = start_bombs(&sim.node(node::Config::default()), 2, false);
    assert_panicked(&mut sim, handle, "bomb, then a drop panicked: bomb");
}

/// Panics when polled, and holds a [`Bomb`].
struct Fuse {
    _bomb: Bomb,
}

impl Future for Fuse {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        panic!("boom")
    }
}

#[test]
fn a_panic_in_the_drop_of_the_future_that_panicked_gives_both_panics() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |tasks| async move {
        tasks.spawn(Fuse {
            _bomb: Bomb("bomb"),
        });
        pending::<()>().await;
    });
    assert_panicked(
        &mut sim,
        handle.unwrap(),
        "boom, then a drop panicked: bomb",
    );
}

#[test]
fn two_panics_in_drops_at_a_crash_panic_after_the_crash() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |tasks| async move {
        for bomb in [Bomb("bomb"), Bomb("bomb")] {
            tasks.spawn(async move {
                let _bomb = bomb;
                pending::<()>().await;
            });
        }
        pending::<()>().await;
    });
    sim.run_for(Span::SECOND).unwrap();
    let message = "bomb, then a drop panicked: bomb";
    assert_eq!(crash_panic(&mut sim, &node, crate::Crash::Power), message);
    assert_eq!(node.clock().now(), node::Config::default().monotonic);
    drop(handle);
    assert_restarts(&mut sim, &node);
}

#[test]
fn a_panic_in_the_drop_of_an_unstarted_thread_at_a_power_cut_still_ends_the_cut() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let running = node.shards().start(shard("running"), |tasks| async move {
        tasks.spawn(async move {
            let _bomb = Bomb("bomb");
            pending::<()>().await;
        });
        pending::<()>().await;
    });
    sim.run_for(Span::SECOND).unwrap();
    let late = Bomb("late");
    let unstarted = node
        .shards()
        .start(shard("unstarted"), move |_| async move {
            let _late = late;
        });
    let message = "bomb, then a drop panicked: late";
    assert_eq!(crash_panic(&mut sim, &node, crate::Crash::Power), message);
    assert_eq!(node.clock().now(), node::Config::default().monotonic);
    drop((running, unstarted));
    assert_restarts(&mut sim, &node);
}

#[test]
fn two_panics_in_drops_of_unstarted_threads_at_a_crash_give_one_panic() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handles = ["first", "second"].map(|message| {
        let bomb = Bomb(message);
        node.shards().start(shard(message), move |_| async move {
            let _bomb = bomb;
        })
    });
    let message = "first, then a drop panicked: second";
    assert_eq!(crash_panic(&mut sim, &node, crate::Crash::Process), message);
    drop(handles);
    assert_restarts(&mut sim, &node);
}

/// Panics in its drop with a [`Bomb`] as the payload.
struct Thrower;

impl Drop for Thrower {
    fn drop(&mut self) {
        panic::panic_any(Bomb("bomb"));
    }
}

/// A panic payload whose drop panics with another one, forever.
struct Relay;

/// A panic payload whose drop panics with a `Countdown` of one less, or with "last"
/// at 0.
struct Countdown(usize);

impl Drop for Countdown {
    fn drop(&mut self) {
        assert!(self.0 > 0, "last");
        panic::panic_any(Countdown(self.0 - 1));
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        panic::panic_any(Relay);
    }
}

/// The message of a panic with a [`Bomb`] as the payload.
const THROWN: &str = "a payload that is not a string, then a drop panicked: bomb";

#[test]
fn a_poll_whose_payload_panics_in_its_drop_gives_both_panics() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |_| async {
        panic::panic_any(Bomb("bomb"));
    });
    assert_panicked(&mut sim, handle.unwrap(), THROWN);
}

#[test]
fn a_body_whose_payload_panics_in_its_drop_gives_both_panics() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = (node.threads())
        .start("body", || -> Ready<()> { panic::panic_any(Bomb("bomb")) });
    assert_eq!(sim.run(), Err(panicked("body", THROWN)));
    let panicked = thread::Panicked {
        name: "body".into(),
    };
    assert_eq!(handle.unwrap().join(), Err(panicked));
}

#[test]
fn a_drop_whose_payload_panics_in_its_drop_gives_both_panics() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |tasks| async move {
        spawn_holding(&tasks, Thrower);
    });
    assert_panicked(&mut sim, handle.unwrap(), THROWN);
}

#[test]
fn a_drop_at_a_crash_whose_payload_panics_in_its_drop_panics_with_both() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let _handle = node.shards().start(shard("shard-0"), |tasks| async move {
        spawn_holding(&tasks, Thrower);
        pending::<()>().await;
    });
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(crash_panic(&mut sim, &node, crate::Crash::Process), THROWN);
    assert_restarts(&mut sim, &node);
}

#[test]
fn a_payload_whose_drop_always_panics_ends_the_run_after_the_chain() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |_| async {
        panic::panic_any(Relay);
    });
    let chain = ["a payload that is not a string"; crate::CHAIN + 1];
    let message = chain.join(", then a drop panicked: ");
    assert_panicked(&mut sim, handle.unwrap(), &message);
}

#[test]
fn a_panic_in_the_last_drop_of_the_chain_gives_its_message() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let handle = node.shards().start(shard("shard-0"), |_| async {
        panic::panic_any(Countdown(crate::CHAIN - 1));
    });
    let mut chain = vec!["a payload that is not a string"; crate::CHAIN];
    chain.push("last");
    let message = chain.join(", then a drop panicked: ");
    assert_panicked(&mut sim, handle.unwrap(), &message);
}

/// A panic payload whose drop joins the thread it holds, and panics with the result.
struct Joiner(Arc<Mutex<Option<thread::Handle>>>);

impl Drop for Joiner {
    fn drop(&mut self) {
        let handle = self.0.lock().unwrap().take().unwrap();
        panic!("joined: {:?}", handle.join());
    }
}

#[test]
fn a_payload_drops_after_its_thread_ends() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let slot = Arc::new(Mutex::new(None));
    let own = Arc::clone(&slot);
    let handle = node.shards().start(shard("shard-0"), move |_| async move {
        panic::panic_any(Joiner(own));
    });
    *slot.lock().unwrap() = Some(handle.unwrap());
    let message = "a payload that is not a string, then a drop panicked: joined: \
        Err(Panicked { name: \"shard-0\" })";
    assert_eq!(sim.run(), Err(panicked("shard-0", message)));
}

/// Spawns on `tasks` a task that holds `value` and waits forever.
fn spawn_holding<T: 'static>(tasks: &env::tasks::Tasks, value: T) {
    tasks.spawn(async move {
        let _value = value;
        pending::<()>().await;
    });
}

/// Starts on `node` a thread that holds `value` and has not run.
fn start_holding(node: &node::Node, value: impl Send + 'static) {
    let handle = node
        .shards()
        .start(shard("unstarted"), move |_| async move {
            let _value = value;
        });
    drop(handle.unwrap());
}

/// A run with waiting tasks that hold a [`Bomb`] "first" and "second", in that order,
/// and a thread that holds a [`Bomb`] "late" and has not run.
fn create_bombed() -> Sim {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let _running = node.shards().start(shard("running"), |tasks| async move {
        spawn_holding(&tasks, Bomb("first"));
        spawn_holding(&tasks, Bomb("second"));
        pending::<()>().await;
    });
    sim.run_for(Span::SECOND).unwrap();
    start_holding(&node, Bomb("late"));
    sim
}

/// The message of the panic that dropping `sim` must raise.
fn drop_panic(sim: Sim) -> String {
    let dropped = panic::catch_unwind(AssertUnwindSafe(move || drop(sim)));
    crate::message(&*dropped.unwrap_err())
}

#[test]
fn panics_in_drops_when_the_sim_drops_give_one_panic() {
    let message = "first, then a drop panicked: second, then a drop panicked: late";
    assert_eq!(drop_panic(create_bombed()), message);
}

#[test]
fn a_sim_that_drops_in_a_panic_does_not_panic_again() {
    let sim = create_bombed();
    let unwound = panic::catch_unwind(AssertUnwindSafe(move || {
        let _sim = sim;
        panic!("failed");
    }));
    assert_eq!(crate::message(&*unwound.unwrap_err()), "failed");
}

/// Spawns a waiting task that holds a [`Bomb`] "spawned", when dropped.
struct Planter(env::tasks::Tasks);

impl Drop for Planter {
    fn drop(&mut self) {
        spawn_holding(&self.0, Bomb("spawned"));
    }
}

#[test]
fn a_panic_in_a_task_that_a_drop_spawns_comes_before_the_panics_of_threads() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let _handle = node.shards().start(shard("shard-0"), |tasks| async move {
        spawn_holding(&tasks, Planter(tasks.clone()));
        pending::<()>().await;
    });
    sim.run_for(Span::SECOND).unwrap();
    start_holding(&node, Bomb("late"));
    assert_eq!(drop_panic(sim), "spawned, then a drop panicked: late");
}

#[test]
fn the_panics_of_tasks_come_in_start_order_across_threads() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let _a = node.shards().start(shard("a"), move |tasks| async move {
        clock.sleep(Span::MILLISECOND).await;
        spawn_holding(&tasks, Bomb("later"));
        pending::<()>().await;
    });
    let _b = node.shards().start(shard("b"), |tasks| async move {
        spawn_holding(&tasks, Bomb("sooner"));
        pending::<()>().await;
    });
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(drop_panic(sim), "sooner, then a drop panicked: later");
}

/// Starts a thread that holds a [`Bomb`] "launched" on its node, when dropped.
struct Launcher(node::Node);

impl Drop for Launcher {
    fn drop(&mut self) {
        start_holding(&self.0, Bomb("launched"));
    }
}

#[test]
fn a_panic_in_a_thread_that_a_drop_starts_joins_the_drop_panic() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    start_holding(&node, Launcher(node.clone()));
    start_holding(&node, Bomb("late"));
    assert_eq!(drop_panic(sim), "launched, then a drop panicked: late");
}

#[test]
fn a_crash_leaves_an_unstarted_thread_of_another_node() {
    let mut sim = sim(0);
    let (a, b) = (
        sim.node(node::Config::default()),
        sim.node(node::Config::default()),
    );
    let handle = b.shards().start(shard("b-0"), |_| async {});
    sim.crash(&a, crate::Crash::Process);
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
}

/// Crashes `node` and gives the message of the panic that the crash must raise.
fn crash_panic(sim: &mut Sim, node: &node::Node, crash: crate::Crash) -> String {
    let crashed = panic::catch_unwind(AssertUnwindSafe(|| sim.crash(node, crash)));
    crate::message(&*crashed.unwrap_err())
}

/// Asserts that a thread started on `node` after a crash runs to its end.
fn assert_restarts(sim: &mut Sim, node: &node::Node) {
    let handle = node.shards().start(shard("restart"), |_| async {});
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
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
    assert_eq!(node.wall().now().time, Stamp::from_nanos(i64::MAX));
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
    assert_eq!(node.wall().now().time, config.wall);
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

fn millis(n: i64) -> Span {
    Span::from_nanos(n * Span::MILLISECOND.nanos())
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn a_wall_step_moves_only_the_wall_of_its_node() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let config = node::Config::default();
    a.step_wall(Span::HOUR);
    assert_eq!(a.wall().now().time, config.wall + Span::HOUR);
    assert_eq!(a.clock().now(), config.monotonic);
    assert_eq!(b.wall().now().time, config.wall);
    a.step_wall(Span::from_nanos(-Span::DAY.nanos()));
    sim.run_for(Span::SECOND).unwrap();
    let wall = config.wall + Span::HOUR - Span::DAY + Span::SECOND;
    assert_eq!(a.wall().now().time, wall);
    assert_eq!(b.wall().now().time, config.wall + Span::SECOND);
}

#[test]
#[should_panic(expected = "step_wall(2s) moves the wall of node 0 out of range")]
fn a_wall_step_out_of_range_panics() {
    let mut sim = sim(0);
    let node = ending(&mut sim);
    node.step_wall(Span::from_nanos(2 * Span::SECOND.nanos()));
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn each_node_reads_its_own_wall_error() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config {
        wall_error: None,
        ..node::Config::default()
    });
    sim.run_for(Span::SECOND).unwrap();
    let config = node::Config::default();
    let reading = env::wall::Reading {
        time: config.wall + Span::SECOND,
        error: Some(millis(10)),
    };
    assert_eq!(a.wall().now(), reading);
    assert_eq!(
        b.wall().now(),
        env::wall::Reading {
            error: None,
            ..reading
        }
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn a_wall_error_change_applies_to_the_next_reading_of_its_node() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let wall = a.wall();
    a.set_wall_error(Some(Span::SECOND));
    let config = node::Config::default();
    let reading = env::wall::Reading {
        time: config.wall,
        error: Some(Span::SECOND),
    };
    assert_eq!(wall.now(), reading);
    assert_eq!(b.wall().now().error, config.wall_error);
    a.set_wall_error(None);
    assert_eq!(
        wall.now(),
        env::wall::Reading {
            error: None,
            ..reading
        }
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn a_wall_step_keeps_the_error() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    node.set_wall_error(Some(Span::ZERO));
    node.step_wall(Span::HOUR);
    let reading = env::wall::Reading {
        time: node::Config::default().wall + Span::HOUR,
        error: Some(Span::ZERO),
    };
    assert_eq!(node.wall().now(), reading);
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn a_wall_error_change_does_not_move_the_wall() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    sim.run_for(Span::SECOND).unwrap();
    node.set_wall_error(Some(Span::SECOND));
    sim.run_for(Span::SECOND).unwrap();
    let config = node::Config::default();
    let two = Span::from_nanos(2 * Span::SECOND.nanos());
    assert_eq!(node.wall().now().time, config.wall + two);
    assert_eq!(node.clock().now(), config.monotonic + two);
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn a_bad_wall_error_reaches_the_reading() {
    let mut sim = sim(0);
    let negative = Some(Span::from_nanos(-1));
    let a = sim.node(node::Config {
        wall_error: negative,
        ..node::Config::default()
    });
    let b = sim.node(node::Config::default());
    b.set_wall_error(negative);
    assert_eq!(a.wall().now().error, negative);
    assert_eq!(b.wall().now().error, negative);
}

#[test]
fn a_timer_past_the_end_after_a_wall_step_waits() {
    let mut sim = sim(0);
    let node = ending(&mut sim);
    let clock = node.clock();
    let _handle = node.shards().start(shard("shard-0"), move |_| async move {
        clock.sleep(millis(750)).await;
    });
    sim.run_for(Span::ZERO).unwrap();
    node.step_wall(millis(500));
    assert_eq!(
        sim.run(),
        Err(Error::Stuck {
            threads: vec!["shard-0".into()],
            seed: 0,
        })
    );
}

/// Starts a shard on `node` that sleeps for `span`, then logs its name and the time.
fn log_after(
    node: &node::Node,
    name: &'static str,
    span: Span,
    log: &Arc<Mutex<Vec<(&'static str, Monotonic)>>>,
) -> env::thread::Handle {
    let clock = node.clock();
    let log = Arc::clone(log);
    let handle = node.shards().start(shard(name), move |_| async move {
        clock.sleep(span).await;
        log.lock().unwrap().push((name, clock.now()));
    });
    handle.unwrap()
}

#[test]
fn a_paused_node_runs_nothing_until_the_pause_ends() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let log = Arc::new(Mutex::new(Vec::new()));
    a.pause(Span::SECOND);
    let handles = [
        log_after(&a, "a", Span::ZERO, &log),
        log_after(&b, "b", millis(500), &log),
    ];
    sim.run().unwrap();
    for handle in handles {
        handle.join().unwrap();
    }
    let start = node::Config::default().monotonic;
    let log = log.lock().unwrap().clone();
    assert_eq!(
        log,
        [("b", start + millis(500)), ("a", start + Span::SECOND)]
    );
}

#[test]
fn a_timer_that_falls_due_in_a_pause_fires_when_it_ends() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = log_after(&node, "a", millis(250), &log);
    sim.run_for(Span::ZERO).unwrap();
    node.pause(Span::SECOND);
    sim.run_for(millis(999)).unwrap();
    assert_eq!(log.lock().unwrap().clone(), []);
    sim.run().unwrap();
    handle.join().unwrap();
    let start = node::Config::default().monotonic;
    assert_eq!(log.lock().unwrap().clone(), [("a", start + Span::SECOND)]);
}

#[test]
fn overlapping_pauses_end_at_the_later_end() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let log = Arc::new(Mutex::new(Vec::new()));
    node.pause(Span::from_nanos(2 * Span::SECOND.nanos()));
    node.pause(Span::SECOND);
    node.pause(Span::from_nanos(-1));
    let handle = log_after(&node, "a", Span::ZERO, &log);
    sim.run().unwrap();
    handle.join().unwrap();
    let start = node::Config::default().monotonic;
    let end = start + Span::from_nanos(2 * Span::SECOND.nanos());
    assert_eq!(log.lock().unwrap().clone(), [("a", end)]);
}

#[test]
fn a_run_ends_without_waiting_for_a_pause_with_nothing_ready() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    node.pause(Span::SECOND);
    sim.run().unwrap();
    assert_eq!(node.clock().now(), node::Config::default().monotonic);
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn a_wall_step_in_a_run_ends_it_at_the_new_end_of_true_time() {
    let mut sim = sim(0);
    let node = ending(&mut sim);
    let stepper = node.clone();
    let handle = node.shards().start(shard("shard-0"), move |_| async move {
        stepper.step_wall(millis(500));
    });
    sim.run_for(millis(900)).unwrap();
    handle.unwrap().join().unwrap();
    assert_eq!(node.wall().now().time, Stamp::from_nanos(i64::MAX));
    let start = node::Config::default().monotonic;
    assert_eq!(node.clock().now(), start + millis(500));
}

#[test]
fn a_pause_past_the_end_of_true_time_never_ends() {
    let mut sim = sim(0);
    let node = ending(&mut sim);
    node.pause(Span::from_nanos(2 * Span::SECOND.nanos()));
    let _handle = node.shards().start(shard("shard-0"), |_| async {});
    assert_eq!(
        sim.run(),
        Err(Error::Stuck {
            threads: vec!["shard-0".into()],
            seed: 0,
        })
    );
}

#[test]
fn a_wall_step_back_lets_a_waiting_timer_fire() {
    let mut sim = sim(0);
    let node = ending(&mut sim);
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = log_after(&node, "a", millis(750), &log);
    sim.run_for(Span::ZERO).unwrap();
    node.step_wall(millis(500));
    sim.run_for(millis(500)).unwrap();
    node.step_wall(millis(-500));
    sim.run().unwrap();
    handle.join().unwrap();
    let start = node::Config::default().monotonic;
    assert_eq!(log.lock().unwrap().clone(), [("a", start + millis(750))]);
}

#[test]
fn a_node_added_later_holds_back_a_timer_past_its_end() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let log = Arc::new(Mutex::new(Vec::new()));
    let _handle =
        log_after(&node, "a", Span::from_nanos(2 * Span::SECOND.nanos()), &log);
    sim.run_for(Span::ZERO).unwrap();
    let _ending = ending(&mut sim);
    assert_eq!(
        sim.run(),
        Err(Error::Stuck {
            threads: vec!["a".into()],
            seed: 0,
        })
    );
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(log.lock().unwrap().clone(), []);
}

#[test]
fn a_pause_past_the_range_of_true_time_never_ends() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        monotonic: Monotonic(0),
        wall: Stamp::from_nanos(i64::MIN),
        ..node::Config::default()
    });
    sim.run_for(Span::from_nanos(i64::MAX)).unwrap();
    sim.run_for(Span::from_nanos(i64::MAX)).unwrap();
    node.pause(Span::from_nanos(i64::MAX));
    let _handle = node.shards().start(shard("shard-0"), |_| async {});
    assert_eq!(
        sim.run(),
        Err(Error::Stuck {
            threads: vec!["shard-0".into()],
            seed: 0,
        })
    );
}

/// Starts a shard named `late` on its node when dropped, and sets `ran` when that
/// shard runs.
struct Restarter {
    node: node::Node,
    ran: Arc<AtomicBool>,
}

impl Drop for Restarter {
    fn drop(&mut self) {
        let ran = Arc::clone(&self.ran);
        let handle = self
            .node
            .shards()
            .start(shard("late"), move |_| async move {
                ran.store(true, Ordering::Relaxed);
            });
        drop(handle.unwrap());
    }
}

#[test]
fn a_thread_started_by_a_drop_at_a_crash_never_runs() {
    for started in [false, true] {
        let mut sim = sim(0);
        let node = sim.node(node::Config::default());
        let ran = Arc::new(AtomicBool::new(false));
        let restarter = Restarter {
            node: node.clone(),
            ran: Arc::clone(&ran),
        };
        let shard = node
            .shards()
            .start(shard("restarter"), move |_| async move {
                let _restarter = restarter;
                pending::<()>().await;
            });
        if started {
            sim.run_for(Span::SECOND).unwrap();
        }
        sim.crash(&node, crate::Crash::Process);
        sim.run().unwrap();
        assert!(!ran.load(Ordering::Relaxed), "started: {started}");
        drop(shard);
    }
}
