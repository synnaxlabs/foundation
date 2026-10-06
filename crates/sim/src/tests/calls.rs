//! Tests of a set delay and of the count of file calls.

use std::future::pending;
use std::path::Path;

use env::clock::Clock;
use env::files::{Mode, Operation};
use types::time::Span;

use super::files::{MIB, block, create, io, pool};
use super::{shard, sim};
use crate::{Crash, Sim, node};

const UP_TO: Span = Span::from_nanos(100 * Span::MICROSECOND.nanos());
const SLOW: Span = Span::from_nanos(5 * Span::MILLISECOND.nanos());

/// A run with one node, which has a disk of 1 MiB.
fn disk(seed: u64) -> (Sim, node::Node) {
    let mut sim = sim(seed);
    let node = sim.node(node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    });
    (sim, node)
}

/// The true time that `call` takes.
async fn timed<T>(clock: &Clock, call: impl Future<Output = T>) -> Span {
    let start = clock.now();
    call.await;
    clock.now() - start
}

#[test]
fn a_set_delay_makes_each_later_call_of_its_operation_on_its_path_take_it() {
    let (mut sim, node) = disk(0);
    let spans = sim.run_on(&node, |node, _| async move {
        let (files, clock, pool) = (node.files(), node.clock(), pool());
        let a = create(&node, "a", 512).await;
        let b = create(&node, "b", 512).await;
        node.delay_file(Path::new("./a"), Operation::Sync, SLOW);
        node.delay_file(Path::new(""), Operation::Free, SLOW);
        let parts = [block(&pool, &[1; 512])];
        [
            timed(&clock, a.sync()).await,
            timed(&clock, a.sync()).await,
            timed(&clock, files.free()).await,
            timed(&clock, a.write_at(0, &parts)).await,
            timed(&clock, b.sync()).await,
            timed(&clock, files.sync_dir(Path::new(""))).await,
        ]
    });
    let spans = spans.unwrap();
    assert_eq!(spans[..3], [SLOW; 3]);
    assert!(spans[3..].iter().all(|span| *span <= UP_TO), "{spans:?}");
}

#[test]
fn a_later_delay_replaces_the_earlier_one() {
    let (mut sim, node) = disk(0);
    let spans = sim.run_on(&node, |node, _| async move {
        let clock = node.clock();
        let a = create(&node, "a", 512).await;
        let mut spans = Vec::new();
        for delay in [SLOW, Span::MILLISECOND, Span::ZERO] {
            node.delay_file(Path::new("a"), Operation::Sync, delay);
            spans.push(timed(&clock, a.sync()).await);
        }
        spans
    });
    assert_eq!(spans.unwrap(), [SLOW, Span::MILLISECOND, Span::ZERO]);
}

/// The spans of the calls on `b`, with or without a set delay on the syncs of `a`.
fn spans_of_b(seed: u64, delayed: bool) -> Vec<Span> {
    let (mut sim, node) = disk(seed);
    let spans = sim.run_on(&node, move |node, _| async move {
        let (clock, pool) = (node.clock(), pool());
        let a = create(&node, "a", 512).await;
        let b = create(&node, "b", 512).await;
        if delayed {
            node.delay_file(Path::new("a"), Operation::Sync, SLOW);
        }
        let parts = [block(&pool, &[1; 512])];
        let mut spans = Vec::new();
        for _ in 0..4 {
            a.sync().await.unwrap();
            spans.push(timed(&clock, b.write_at(0, &parts)).await);
            spans.push(timed(&clock, b.sync()).await);
        }
        spans
    });
    spans.unwrap()
}

#[test]
fn a_set_delay_keeps_the_delays_of_other_calls() {
    for seed in 0..8 {
        let spans = spans_of_b(seed, false);
        assert!(spans.iter().any(|span| *span > Span::ZERO), "{spans:?}");
        assert_eq!(spans_of_b(seed, true), spans, "seed {seed}");
    }
}

#[test]
fn file_calls_counts_each_call_that_the_node_started() {
    let (mut sim, node) = disk(0);
    let other = sim.node(node::Config::default());
    let open = sim.run_on(&node, |node, _| async move {
        let files = node.files();
        node.fail_file(Path::new("a"), Operation::Open);
        let mode = Mode::Create { len: 512 };
        let failed = files.open(Path::new("a"), mode).await.map(drop);
        let file = files.open(Path::new("./a"), mode).await.unwrap();
        file.write_at(0, &[block(&pool(), &[1; 512])])
            .await
            .unwrap();
        failed
    });
    assert_eq!(open.unwrap(), Err(io("a", Operation::Open, 5)));
    assert_eq!(node.file_calls(Path::new("a"), Operation::Open), 2);
    assert_eq!(node.file_calls(Path::new("./a"), Operation::WriteAt), 1);
    assert_eq!(node.file_calls(Path::new("a"), Operation::Sync), 0);
    assert_eq!(node.file_calls(Path::new("b"), Operation::Open), 0);
    assert_eq!(other.file_calls(Path::new("a"), Operation::Open), 0);
}

#[test]
fn file_calls_counts_a_call_in_flight_and_a_call_before_a_crash() {
    let (mut sim, node) = disk(0);
    node.delay_file(Path::new("a"), Operation::Sync, Span::SECOND);
    let own = node.clone();
    let handle = node.shards().start(shard("sync"), move |_| async move {
        let a = create(&own, "a", 512).await;
        a.sync().await.unwrap();
        pending::<()>().await;
    });
    drop(handle.unwrap());
    sim.run_for(Span::MILLISECOND).unwrap();
    assert_eq!(node.file_calls(Path::new("a"), Operation::Sync), 1);
    sim.crash(&node, Crash::Process);
    assert_eq!(node.file_calls(Path::new("a"), Operation::Sync), 1);
    let synced = sim.run_on(&node, |node, _| async move {
        let file = node.files().open(Path::new("a"), Mode::Write).await;
        file.unwrap().sync().await
    });
    assert_eq!(synced.unwrap(), Ok(()));
    assert_eq!(node.file_calls(Path::new("a"), Operation::Sync), 2);
}

#[test]
#[should_panic(expected = "the sync calls on a take a negative delay of -1ns")]
fn a_negative_delay_panics() {
    let (_sim, node) = disk(0);
    node.delay_file(Path::new("a"), Operation::Sync, Span::from_nanos(-1));
}

#[test]
#[should_panic(expected = "free has no path; aim at it with an empty path")]
fn a_delay_of_free_with_a_path_panics() {
    let (_sim, node) = disk(0);
    node.delay_file(Path::new("a"), Operation::Free, SLOW);
}

#[test]
#[should_panic(expected = "free has no path; aim at it with an empty path")]
fn a_count_of_free_with_a_path_panics() {
    let (_sim, node) = disk(0);
    assert_eq!(node.file_calls(Path::new("a"), Operation::Free), 0);
}

#[test]
fn a_count_of_free_has_an_empty_path() {
    let (mut sim, node) = disk(0);
    let freed = sim.run_on(&node, |node, _| async move { node.files().free().await });
    assert_eq!(freed.unwrap(), Ok(MIB));
    assert_eq!(node.file_calls(Path::new(""), Operation::Free), 1);
}
