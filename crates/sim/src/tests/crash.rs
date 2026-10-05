//! Tests of `Sim::crash`: what a node keeps when its process dies or it loses power.

use std::collections::BTreeSet;
use std::future::{pending, poll_fn};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use env::files::{Error, Mode, Operation};
use env::net::udp;
use types::time::Span;

use super::files::{KIB, MIB, block, create, io, pool, read, sectors};
use super::{Guard, shard, sim};
use crate::{Crash, Sim, node};

/// The true time that [`crash_after`] runs before the crash.
const BEFORE: Span = Span::from_nanos(10 * Span::MILLISECOND.nanos());

/// A run with one node, which has a disk of 1 MiB.
fn disk(seed: u64) -> (Sim, node::Node) {
    let mut sim = sim(seed);
    let node = sim.node(node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    });
    (sim, node)
}

/// Runs `body` on a shard of `node` that then waits forever, and crashes `node` by
/// `crash` once [`BEFORE`] of true time has passed.
fn crash_after<F>(
    sim: &mut Sim,
    node: &node::Node,
    crash: Crash,
    body: impl FnOnce(node::Node) -> F + Send + 'static,
) where
    F: Future<Output = ()> + 'static,
{
    let own = node.clone();
    let handle = node.shards().start(shard("before"), move |_| async move {
        body(own).await;
        pending::<()>().await;
    });
    drop(handle.unwrap());
    sim.run_for(BEFORE).unwrap();
    sim.crash(node, crash);
}

/// Runs `body` on a new shard of `node` until it ends, and returns what it gives.
fn on<T, F>(
    sim: &mut Sim,
    node: &node::Node,
    body: impl FnOnce(node::Node) -> F + Send + 'static,
) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&out);
    let own = node.clone();
    let handle = node.shards().start(shard("after"), move |_| async move {
        *slot.lock().unwrap() = Some(body(own).await);
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    out.lock().unwrap().take().expect("the shard gave a value")
}

/// The sectors of the 1 KiB file `a` of `node`.
fn sectors_of(sim: &mut Sim, node: &node::Node) -> Vec<u8> {
    on(sim, node, |node| async move {
        let file = node.files().open(Path::new("a"), Mode::Read).await.unwrap();
        sectors(&read(&file, &pool(), 0, 1_024).await)
    })
}

/// Makes the 1 KiB file `a` durable in the data directory, with each sector 1.
async fn create_synced(node: &node::Node) -> env::files::File {
    let file = create(node, "a", 1_024).await;
    node.files().sync_dir(Path::new("")).await.unwrap();
    file.write_at(0, &[block(&pool(), &[1; 1_024])])
        .await
        .unwrap();
    file.sync().await.unwrap();
    file
}

#[test]
fn a_process_crash_keeps_each_call_that_ended() {
    for seed in 0..16 {
        let (mut sim, node) = disk(seed);
        crash_after(&mut sim, &node, Crash::Process, |node| async move {
            let file = create(&node, "a", 1_024).await;
            file.write_at(0, &[block(&pool(), &[7; 1_024])])
                .await
                .unwrap();
            node.files().create_dir(Path::new("d")).await.unwrap();
        });
        let names = on(&mut sim, &node, |node| async move {
            node.files().list(Path::new("")).await.unwrap()
        });
        assert_eq!(
            names,
            [PathBuf::from("a"), PathBuf::from("d")],
            "seed {seed}"
        );
        assert_eq!(sectors_of(&mut sim, &node), [7, 7], "seed {seed}");
    }
}

#[test]
fn a_power_cut_keeps_a_synced_write_in_a_synced_file() {
    for seed in 0..16 {
        let (mut sim, node) = disk(seed);
        crash_after(&mut sim, &node, Crash::Power, |node| async move {
            drop(create_synced(&node).await);
        });
        assert_eq!(sectors_of(&mut sim, &node), [1, 1], "seed {seed}");
    }
}

/// The sectors of a synced file of 1s after unsynced writes of 2s and then 3s over
/// both its sectors, and a power cut.
fn unsynced(seed: u64) -> Vec<u8> {
    let (mut sim, node) = disk(seed);
    crash_after(&mut sim, &node, Crash::Power, |node| async move {
        let (file, pool) = (create_synced(&node).await, pool());
        file.write_at(0, &[block(&pool, &[2; 1_024])])
            .await
            .unwrap();
        file.write_at(0, &[block(&pool, &[3; 1_024])])
            .await
            .unwrap();
    });
    sectors_of(&mut sim, &node)
}

#[test]
fn a_power_cut_keeps_the_durable_bytes_or_one_write_per_sector() {
    let outcomes: Vec<Vec<u8>> = (0..64).map(unsynced).collect();
    for sector in 0..2 {
        let kept: BTreeSet<u8> = outcomes.iter().map(|bytes| bytes[sector]).collect();
        assert_eq!(kept, BTreeSet::from([1, 2, 3]), "sector {sector}");
    }
    let mixed = outcomes.iter().any(|bytes| bytes[0] != bytes[1]);
    assert!(mixed, "the sectors are kept apart: {outcomes:?}");
}

/// A synced file of 1s gets a write of 2s over its first sector, then a sync, then,
/// once the sync is in flight, a write of 3s over its second sector, then a power
/// cut. Returns whether the sync was still in flight when the second write ended,
/// and the sectors.
fn sync_in_flight(seed: u64) -> (bool, Vec<u8>) {
    let (mut sim, node) = disk(seed);
    let raced = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&raced);
    crash_after(&mut sim, &node, Crash::Power, move |node| async move {
        let (file, pool) = (create_synced(&node).await, pool());
        file.write_at(0, &[block(&pool, &[2; 512])]).await.unwrap();
        let mut sync = pin!(file.sync());
        poll_fn(|cx| {
            assert!(sync.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        file.write_at(512, &[block(&pool, &[3; 512])])
            .await
            .unwrap();
        match poll_fn(|cx| Poll::Ready(sync.as_mut().poll(cx))).await {
            Poll::Ready(synced) => synced.unwrap(),
            Poll::Pending => {
                flag.store(true, Ordering::Relaxed);
                sync.await.unwrap();
            }
        }
    });
    (raced.load(Ordering::Relaxed), sectors_of(&mut sim, &node))
}

#[test]
fn a_sync_covers_only_the_writes_that_ended_before_it_started() {
    let outcomes: BTreeSet<(bool, Vec<u8>)> = (0..64).map(sync_in_flight).collect();
    let all = [false, true].map(|raced| [(raced, vec![2, 1]), (raced, vec![2, 3])]);
    assert_eq!(outcomes, BTreeSet::from_iter(all.into_iter().flatten()));
}

/// The names in the data directory and the free bytes after a power cut, when a
/// synced file `old` of 64 KiB is removed, and a file `new` of 128 KiB and a
/// directory `d` are made, with a `sync_dir` after them or not.
fn changed(seed: u64, synced: bool) -> (Vec<PathBuf>, u64) {
    let (mut sim, node) = disk(seed);
    crash_after(&mut sim, &node, Crash::Power, move |node| async move {
        let files = node.files();
        drop(create(&node, "old", 64 * KIB).await);
        files.sync_dir(Path::new("")).await.unwrap();
        files.remove(Path::new("old")).await.unwrap();
        drop(create(&node, "new", 128 * KIB).await);
        files.create_dir(Path::new("d")).await.unwrap();
        if synced {
            files.sync_dir(Path::new("")).await.unwrap();
        }
    });
    on(&mut sim, &node, |node| async move {
        let files = node.files();
        let names = files.list(Path::new("")).await.unwrap();
        (names, files.free().await.unwrap())
    })
}

#[test]
fn a_power_cut_undoes_the_creates_and_removes_that_no_sync_dir_covers() {
    for seed in 0..8 {
        let old = (vec![PathBuf::from("old")], MIB - 64 * KIB);
        assert_eq!(changed(seed, false), old, "seed {seed}");
        let new = vec![PathBuf::from("d"), PathBuf::from("new")];
        assert_eq!(changed(seed, true), (new, MIB - 132 * KIB), "seed {seed}");
    }
}

#[test]
fn a_power_cut_drops_a_directory_whose_parent_was_never_synced() {
    let (mut sim, node) = disk(0);
    crash_after(&mut sim, &node, Crash::Power, |node| async move {
        let files = node.files();
        files.create_dir(Path::new("d")).await.unwrap();
        drop(create(&node, "d/f", 64 * KIB).await);
        files.sync_dir(Path::new("d")).await.unwrap();
    });
    let (names, free, opened) = on(&mut sim, &node, |node| async move {
        let files = node.files();
        let names = files.list(Path::new("")).await.unwrap();
        let opened = files.open(Path::new("d/f"), Mode::Read).await.map(drop);
        (names, files.free().await.unwrap(), opened)
    });
    assert_eq!(names, Vec::<PathBuf>::new());
    assert_eq!(free, MIB);
    let path = "d/f".into();
    assert_eq!(opened, Err(Error::NotFound { path }));
}

/// The sectors of a synced file of 1s after an unsynced write of 2s and a sync that
/// a fault fails: as a new descriptor reads them, and after a power cut.
fn torn(seed: u64) -> (Vec<u8>, Vec<u8>) {
    let (mut sim, node) = disk(seed);
    let shown = on(&mut sim, &node, |node| async move {
        let (file, pool) = (create_synced(&node).await, pool());
        file.write_at(0, &[block(&pool, &[2; 1_024])])
            .await
            .unwrap();
        node.fail_file(Path::new("a"), Operation::Sync);
        assert_eq!(file.sync().await, Err(io("a", Operation::Sync, 5)));
        let reopened = node.files().open(Path::new("a"), Mode::Read).await;
        sectors(&read(&reopened.unwrap(), &pool, 0, 1_024).await)
    });
    sim.crash(&node, Crash::Power);
    (shown, sectors_of(&mut sim, &node))
}

#[test]
fn a_failed_sync_keeps_the_durable_bytes_or_the_write_per_sector() {
    let outcomes: BTreeSet<(Vec<u8>, Vec<u8>)> = (0..64).map(torn).collect();
    for (seen, kept) in &outcomes {
        assert_eq!(seen, kept, "the bytes that a reopen reads are durable");
    }
    let seen: BTreeSet<Vec<u8>> = outcomes.into_iter().map(|(seen, _)| seen).collect();
    let all = BTreeSet::from([vec![1, 1], vec![1, 2], vec![2, 1], vec![2, 2]]);
    assert_eq!(seen, all);
}

/// Sleeps until the instant of the crash of [`crash_after`].
async fn until_crash(node: &node::Node) {
    let crash = node::Config::default().monotonic + BEFORE;
    node.clock().sleep_until(crash).await;
}

/// Starts `call`, checks that it is in flight, and waits forever.
async fn hang(call: impl Future) {
    let mut call = pin!(call);
    poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    pending::<()>().await;
}

/// Makes a synced file of 1s on `node`, then starts a write of 9s over both its
/// sectors and waits forever, with a fault that fails the write when `failed`.
async fn write_in_flight(node: node::Node, failed: bool) {
    let (file, pool) = (create_synced(&node).await, pool());
    until_crash(&node).await;
    if failed {
        node.fail_file(Path::new("a"), Operation::WriteAt);
    }
    hang(file.write_at(0, &[block(&pool, &[9; 1_024])])).await;
}

/// The sectors of the file of [`write_in_flight`] after `crash`.
fn in_flight(seed: u64, crash: Crash, failed: bool) -> Vec<u8> {
    let (mut sim, node) = disk(seed);
    crash_after(&mut sim, &node, crash, move |node| {
        write_in_flight(node, failed)
    });
    on(&mut sim, &node, |node| async move {
        node.clock().sleep(Span::MILLISECOND).await;
        let file = node.files().open(Path::new("a"), Mode::Read).await.unwrap();
        sectors(&read(&file, &pool(), 0, 1_024).await)
    })
}

#[test]
fn a_write_in_flight_at_a_crash_keeps_any_subset_of_its_sectors() {
    let all = BTreeSet::from([vec![1, 1], vec![1, 9], vec![9, 1], vec![9, 9]]);
    for crash in [Crash::Process, Crash::Power] {
        let outcomes: BTreeSet<Vec<u8>> =
            (0..128).map(|seed| in_flight(seed, crash, false)).collect();
        assert_eq!(outcomes, all, "{crash:?}");
    }
}

#[test]
fn a_crash_frees_each_file_that_a_write_handle_held() {
    for (crash, value) in [Crash::Process, Crash::Power]
        .into_iter()
        .flat_map(|crash| (0..64).map(move |value| (crash, value)))
    {
        let (mut sim, node) = disk(value);
        crash_after(&mut sim, &node, crash, |node| write_in_flight(node, false));
        let open = on(&mut sim, &node, |node| async move {
            node.files().open(Path::new("a"), Mode::Write).await.err()
        });
        assert_eq!(open, None, "{crash:?}, value {value}");
    }
}

#[test]
fn a_write_that_a_fault_fails_leaves_no_bytes_at_a_crash() {
    for crash in [Crash::Process, Crash::Power] {
        let outcomes: BTreeSet<Vec<u8>> =
            (0..64).map(|seed| in_flight(seed, crash, true)).collect();
        assert_eq!(outcomes, BTreeSet::from([vec![1, 1]]), "{crash:?}");
    }
}

/// A run in which the power is cut with a sync in flight, after an unsynced write of
/// 2s over the synced file `a` of 1s.
fn sync_at_cut(seed: u64) -> (Sim, node::Node) {
    let (mut sim, node) = disk(seed);
    crash_after(&mut sim, &node, Crash::Power, |node| async move {
        let file = create_synced(&node).await;
        file.write_at(0, &[block(&pool(), &[2; 1_024])])
            .await
            .unwrap();
        until_crash(&node).await;
        hang(file.sync()).await;
    });
    (sim, node)
}

#[test]
fn a_sync_in_flight_at_a_power_cut_has_no_effect() {
    let outcomes: BTreeSet<Vec<u8>> = (0..64)
        .map(|seed| {
            let (mut sim, node) = sync_at_cut(seed);
            sectors_of(&mut sim, &node)
        })
        .collect();
    let all = BTreeSet::from([vec![1, 1], vec![1, 2], vec![2, 1], vec![2, 2]]);
    assert_eq!(outcomes, all);
}

#[test]
fn a_power_cut_frees_a_file_that_a_call_in_flight_held() {
    let (mut sim, node) = sync_at_cut(0);
    let free = on(&mut sim, &node, |node| async move {
        let files = node.files();
        files.remove(Path::new("a")).await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
        files.free().await.unwrap()
    });
    assert_eq!(free, MIB);
}

#[test]
fn a_sync_dir_in_flight_at_a_power_cut_has_no_effect() {
    let (mut sim, node) = disk(0);
    crash_after(&mut sim, &node, Crash::Power, |node| async move {
        drop(create(&node, "a", 1_024).await);
        until_crash(&node).await;
        hang(node.files().sync_dir(Path::new(""))).await;
    });
    let names = on(&mut sim, &node, |node| async move {
        node.files().list(Path::new("")).await.unwrap()
    });
    assert_eq!(names, Vec::<PathBuf>::new());
}

/// The digest of a run in which the power is cut during [`write_in_flight`].
fn cut(failed: bool) -> u64 {
    let (mut sim, node) = disk(0);
    let body = move |node| write_in_flight(node, failed);
    crash_after(&mut sim, &node, Crash::Power, body);
    sim.digest()
}

#[test]
fn the_digest_holds_each_call_that_a_power_cut_ends() {
    assert_ne!(cut(false), cut(true));
}

#[test]
#[should_panic(expected = "thread ran ended in a crash of node 0")]
fn join_on_a_thread_that_a_crash_ended_panics() {
    let (mut sim, node) = disk(0);
    let handle = node.shards().start(shard("ran"), |_| pending::<()>());
    sim.run_for(Span::SECOND).unwrap();
    sim.crash(&node, Crash::Process);
    drop(handle.unwrap().join());
}

#[test]
#[should_panic(expected = "thread unstarted ended in a crash of node 0")]
fn join_on_a_thread_that_a_crash_ended_before_it_ran_panics() {
    let (mut sim, node) = disk(0);
    let handle = node.shards().start(shard("unstarted"), |_| async {});
    sim.crash(&node, Crash::Power);
    drop(handle.unwrap().join());
}

#[test]
fn a_crash_ends_only_the_threads_of_its_node() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let (polls, dropped, unstarted) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let (count, guard, clock) =
        (Arc::clone(&polls), Guard(Arc::clone(&dropped)), a.clock());
    let _ticking = a.shards().start(shard("a-0"), move |_| async move {
        let _guard = guard;
        loop {
            count.fetch_add(1, Ordering::Relaxed);
            clock.sleep(Span::MILLISECOND).await;
        }
    });
    let clock = b.clock();
    let other = b.shards().start(shard("b-0"), move |_| async move {
        clock.sleep(Span::SECOND).await;
    });
    sim.run_for(BEFORE).unwrap();
    let guard = Guard(Arc::clone(&unstarted));
    let _late = a.shards().start(shard("a-1"), move |_| async move {
        drop(guard);
        unreachable!("a-1 runs after the crash");
    });
    sim.crash(&a, Crash::Process);
    let before = polls.load(Ordering::Relaxed);
    assert!(dropped.load(Ordering::Relaxed), "the crash dropped a-0");
    assert!(unstarted.load(Ordering::Relaxed), "the crash dropped a-1");
    sim.run().unwrap();
    other.unwrap().join().unwrap();
    assert_eq!(
        polls.load(Ordering::Relaxed),
        before,
        "a-0 never polled again"
    );
}

#[test]
#[should_panic(expected = "Node(0) belongs to another sim")]
fn a_crash_of_a_node_of_another_sim_panics() {
    let (_other, node) = disk(0);
    let (mut sim, _own) = disk(0);
    sim.crash(&node, Crash::Power);
}

/// Binds UDP port 5000 on the IPv4 address of `node`.
fn bind(node: &node::Node) -> Result<(udp::Sender, udp::Receiver), env::net::Error> {
    node.net().udp(&udp::Config {
        local: SocketAddr::new(node.addresses()[0], 5_000),
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
    })
}

#[test]
fn a_crash_closes_the_sockets_of_its_node() {
    let (mut sim, node) = disk(0);
    crash_after(&mut sim, &node, Crash::Process, |node| async move {
        let _socket = bind(&node).unwrap();
        pending::<()>().await;
    });
    bind(&node).unwrap();
}

/// The monotonic and wall readings of a node after [`BEFORE`] and `crash`.
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads the simulated wall"
)]
fn clocks(crash: Crash) -> (types::time::Monotonic, types::time::Stamp) {
    let (mut sim, node) = disk(0);
    crash_after(&mut sim, &node, crash, |_| async {});
    (node.clock().now(), node.wall().now().time)
}

#[test]
fn a_power_cut_restarts_the_monotonic_clock_and_the_wall_runs_on() {
    let config = node::Config::default();
    let ran = (config.monotonic + BEFORE, config.wall + BEFORE);
    assert_eq!(clocks(Crash::Process), ran);
    assert_eq!(clocks(Crash::Power), (config.monotonic, ran.1));
}
