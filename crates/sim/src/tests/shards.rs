//! Tests of shard faults through `env::shards`.

use std::collections::BTreeSet;
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use env::thread;

use super::{shard, sim};
use crate::shard::Fault;
use crate::{Error, node};

fn pinned(name: &str, core: usize) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: Some(core),
    }
}

#[test]
fn a_start_fault_fails_only_the_next_start_on_its_core_of_its_node() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let other = sim.node(node::Config::default());
    node.fail_shard(1, Fault::Start);
    let shards = node.shards();
    let beside = shards.start(pinned("shard-0", 0), |_| async {});
    let elsewhere = other.shards().start(pinned("other-1", 1), |_| async {});
    let e = shards
        .start(pinned("shard-1", 1), |_| async {})
        .unwrap_err();
    assert_eq!(
        e,
        thread::Error::Start {
            name: "shard-1".into(),
            reason: "injected".into(),
        }
    );
    assert_eq!(e.to_string(), "cannot start thread shard-1: injected");
    let again = shards.start(pinned("shard-1", 1), |_| async {});
    sim.run().unwrap();
    for handle in [beside, elsewhere, again] {
        handle.unwrap().join().unwrap();
    }
}

#[test]
fn a_pin_fault_fails_the_start_with_its_core() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    node.fail_shard(2, Fault::Pin);
    let e = node
        .shards()
        .start(pinned("shard-2", 2), |_| async {})
        .unwrap_err();
    assert_eq!(
        e,
        thread::Error::Pin {
            name: "shard-2".into(),
            core: 2,
        }
    );
    assert_eq!(e.to_string(), "cannot pin thread shard-2 to core 2");
    sim.run().unwrap();
}

#[test]
fn a_panic_fault_panics_a_task_of_the_shard() {
    let mut sim = sim(5);
    let node = sim.node(node::Config::default());
    node.fail_shard(0, Fault::Panic);
    let handle = node
        .shards()
        .start(pinned("shard-0", 0), |_| pending::<()>());
    assert_eq!(
        sim.run(),
        Err(Error::Panicked {
            thread: "shard-0".into(),
            message: "injected".into(),
            seed: 5,
        })
    );
    assert_eq!(
        handle.unwrap().join(),
        Err(thread::Panicked {
            name: "shard-0".into()
        })
    );
}

/// Whether the main future of a shard with a panic fault ran before the panic.
fn ran_before_panic(seed: u64) -> bool {
    let mut sim = sim(seed);
    let node = sim.node(node::Config::default());
    node.fail_shard(0, Fault::Panic);
    let ran = Arc::new(AtomicBool::new(false));
    let main = Arc::clone(&ran);
    let _handle = node
        .shards()
        .start(pinned("shard-0", 0), move |_| async move {
            main.store(true, Ordering::Relaxed);
            pending::<()>().await;
        });
    assert_eq!(sim.run(), Err(panicked(seed)));
    ran.load(Ordering::Relaxed)
}

fn panicked(seed: u64) -> Error {
    Error::Panicked {
        thread: "shard-0".into(),
        message: "injected".into(),
        seed,
    }
}

#[test]
fn the_panic_runs_before_or_after_the_main_future_by_the_seed() {
    let orders: BTreeSet<bool> = (0..32).map(ran_before_panic).collect();
    assert_eq!(orders, BTreeSet::from([false, true]));
}

#[test]
fn a_panic_fault_fires_when_the_main_future_completes_first() {
    let completed: BTreeSet<bool> = (0..32)
        .map(|seed| {
            let mut sim = sim(seed);
            let node = sim.node(node::Config::default());
            node.fail_shard(0, Fault::Panic);
            let ran = Arc::new(AtomicBool::new(false));
            let main = Arc::clone(&ran);
            let handle =
                node.shards()
                    .start(pinned("shard-0", 0), move |_| async move {
                        main.store(true, Ordering::Relaxed);
                    });
            assert_eq!(sim.run(), Err(panicked(seed)));
            assert_eq!(
                handle.unwrap().join(),
                Err(thread::Panicked {
                    name: "shard-0".into()
                })
            );
            ran.load(Ordering::Relaxed)
        })
        .collect();
    assert_eq!(completed, BTreeSet::from([false, true]));
}

#[test]
fn a_panic_fault_fails_only_the_next_start_on_its_core() {
    let mut sim = sim(5);
    let node = sim.node(node::Config::default());
    node.fail_shard(0, Fault::Panic);
    let _failed = node
        .shards()
        .start(pinned("shard-0", 0), |_| pending::<()>());
    assert_eq!(sim.run(), Err(panicked(5)));
    let again = node.shards().start(pinned("shard-0", 0), |_| async {});
    sim.run().unwrap();
    again.unwrap().join().unwrap();
}

#[test]
fn faults_on_one_core_fire_in_turn() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    node.fail_shard(1, Fault::Pin);
    node.fail_shard(1, Fault::Start);
    let shards = node.shards();
    let first = shards.start(pinned("a", 1), |_| async {});
    let second = shards.start(pinned("b", 1), |_| async {});
    let third = shards.start(pinned("c", 1), |_| async {});
    assert_eq!(
        first.unwrap_err(),
        thread::Error::Pin {
            name: "a".into(),
            core: 1,
        }
    );
    assert_eq!(
        second.unwrap_err(),
        thread::Error::Start {
            name: "b".into(),
            reason: "injected".into(),
        }
    );
    sim.run().unwrap();
    third.unwrap().join().unwrap();
}

#[test]
fn a_shard_with_no_core_gets_no_fault() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    node.fail_shard(0, Fault::Start);
    let shards = node.shards();
    let free = shards.start(shard("free"), |_| async {});
    let e = shards
        .start(pinned("shard-0", 0), |_| async {})
        .unwrap_err();
    assert_eq!(
        e,
        thread::Error::Start {
            name: "shard-0".into(),
            reason: "injected".into(),
        }
    );
    sim.run().unwrap();
    free.unwrap().join().unwrap();
}

#[test]
fn shard_starts_lists_each_start_of_its_node_in_order() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    a.fail_shard(1, Fault::Start);
    let _failed = a.shards().start(pinned("a-1", 1), |_| async {});
    let _free = a.shards().start(shard("a-free"), |_| async {});
    let _b = b.shards().start(pinned("b-0", 0), |_| async {});
    let _pinned = a.shards().start(pinned("a-0", 0), |_| async {});
    assert_eq!(
        a.shard_starts(),
        vec![pinned("a-1", 1), shard("a-free"), pinned("a-0", 0)]
    );
    assert_eq!(b.shard_starts(), vec![pinned("b-0", 0)]);
    sim.run().unwrap();
}

#[test]
#[should_panic(expected = "a shard fault aims at core 4 of 4")]
fn a_shard_fault_past_the_node_cores_panics() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    node.fail_shard(4, Fault::Start);
}
