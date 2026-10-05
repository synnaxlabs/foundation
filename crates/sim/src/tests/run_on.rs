//! Tests of `Sim::run_on`.

use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use types::time::Span;

use super::{shard, sim};
use crate::{Error, node, shard::Fault};

#[test]
fn run_on_gives_the_value_of_the_body() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let now = sim.run_on(&node, |node, _| async move {
        let clock = node.clock();
        clock.sleep(Span::SECOND).await;
        clock.now()
    });
    assert_eq!(now, Ok(node::Config::default().monotonic + Span::SECOND));
}

#[test]
fn the_body_spawns_on_the_tasks_of_its_shard() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    let spawned = sim.run_on(&node, move |node, tasks| async move {
        tasks.spawn(async move { flag.store(true, Ordering::Relaxed) });
        node.clock().sleep(Span::SECOND).await;
        ran.load(Ordering::Relaxed)
    });
    assert_eq!(spawned, Ok(true));
}

#[test]
fn run_on_returns_when_every_thread_has_ended() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let other = sim.node(node::Config::default());
    let (clock, two) = (other.clock(), Span::from_nanos(2 * Span::SECOND.nanos()));
    let handle = other.shards().start(shard("other"), move |_| async move {
        clock.sleep(two).await;
    });
    drop(handle.unwrap());
    assert_eq!(sim.run_on(&node, |_, _| async { 7 }), Ok(7));
    assert_eq!(other.clock().now(), node::Config::default().monotonic + two);
}

#[test]
fn a_panic_in_the_body_ends_the_run() {
    let mut sim = sim(3);
    let node = sim.node(node::Config::default());
    let e = sim.run_on(&node, |_, _| async { panic!("boom") });
    assert_eq!(
        e,
        Err::<(), _>(Error::Panicked {
            thread: "run_on".into(),
            message: "boom".into(),
            seed: 3,
        })
    );
}

#[test]
fn a_body_that_waits_forever_is_stuck() {
    let mut sim = sim(4);
    let node = sim.node(node::Config::default());
    let e = sim.run_on(&node, |_, _| pending::<()>());
    assert_eq!(
        e,
        Err(Error::Stuck {
            threads: vec!["run_on".into()],
            seed: 4,
        })
    );
}

#[test]
#[should_panic(expected = "Node(0) belongs to another sim")]
fn run_on_a_node_of_another_run_panics() {
    let node = sim(0).node(node::Config::default());
    drop(sim(0).run_on(&node, |_, _| async {}));
}

#[test]
fn a_shard_fault_never_reaches_the_shard_of_run_on() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    node.fail_shard(0, Fault::Start);
    assert_eq!(sim.run_on(&node, |_, _| async { 1 }), Ok(1));
    let pinned = env::shards::Config {
        name: "pinned".into(),
        core: Some(0),
    };
    let e = node.shards().start(pinned, |_| async {}).err();
    let reason = "injected".into();
    let start = env::thread::Error::Start {
        name: "pinned".into(),
        reason,
    };
    assert_eq!(e, Some(start));
}
