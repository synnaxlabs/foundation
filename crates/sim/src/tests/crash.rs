//! Tests of `Sim::crash`: what a node keeps when its process dies or it loses power.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::{pending, poll_fn};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use env::files::{Error, Mode, Operation};
use env::net::udp;
use types::time::Span;

use super::files::{KIB, MIB, block, create, io, pend, pool, read, sectors};
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

/// The sectors of the 1 KiB file `a` of `node`.
fn sectors_of(sim: &mut Sim, node: &node::Node) -> Vec<u8> {
    sim.run_on(node, |node, _| async move {
        let file = node.files().open(Path::new("a"), Mode::Read).await.unwrap();
        sectors(&read(&file, &pool(), 0, 1_024).await)
    })
    .unwrap()
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
        let names = sim
            .run_on(&node, |node, _| async move {
                node.files().list(Path::new("")).await.unwrap()
            })
            .unwrap();
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
    sim.run_on(&node, |node, _| async move {
        let files = node.files();
        let names = files.list(Path::new("")).await.unwrap();
        (names, files.free().await.unwrap())
    })
    .unwrap()
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
    let (names, free, opened) = sim
        .run_on(&node, |node, _| async move {
            let files = node.files();
            let names = files.list(Path::new("")).await.unwrap();
            let opened = files.open(Path::new("d/f"), Mode::Read).await.map(drop);
            (names, files.free().await.unwrap(), opened)
        })
        .unwrap();
    assert_eq!(names, Vec::<PathBuf>::new());
    assert_eq!(free, MIB);
    let path = "d/f".into();
    assert_eq!(opened, Err(Error::NotFound { path }));
}

/// Makes a synced file of 1s on `node`, then a write of 2s over both its sectors, a
/// sync that a fault fails, and a close. Gives the file opened again to write, as a
/// caller recovers.
async fn create_torn(node: &node::Node) -> env::files::File {
    let file = create_synced(node).await;
    file.write_at(0, &[block(&pool(), &[2; 1_024])])
        .await
        .unwrap();
    node.fail_file(Path::new("a"), Operation::Sync);
    assert_eq!(file.sync().await, Err(io("a", Operation::Sync, 5)));
    file.close().await;
    node.files()
        .open(Path::new("a"), Mode::Write)
        .await
        .unwrap()
}

/// The sectors of the file of [`create_torn`] as it reads them, and after a sync and
/// then a power cut.
fn torn(seed: u64) -> (Vec<u8>, Vec<u8>) {
    let (mut sim, node) = disk(seed);
    let shown = sim
        .run_on(&node, |node, _| async move {
            let file = create_torn(&node).await;
            let shown = sectors(&read(&file, &pool(), 0, 1_024).await);
            file.sync().await.unwrap();
            shown
        })
        .unwrap();
    sim.crash(&node, Crash::Power);
    (shown, sectors_of(&mut sim, &node))
}

#[test]
fn a_read_after_a_failed_sync_sees_its_lost_writes_until_the_cache_drops_them() {
    let outcomes: BTreeSet<(u8, u8)> = (0..64)
        .flat_map(|seed| {
            let (shown, kept) = torn(seed);
            shown.into_iter().zip(kept)
        })
        .collect();
    assert_eq!(outcomes, BTreeSet::from([(1, 1), (2, 1), (2, 2)]));
}

/// The bytes of the file of [`create_torn`] after a write of one 3 at its start, a
/// sync, and a power cut.
fn rewritten(seed: u64) -> Vec<u8> {
    let (mut sim, node) = disk(seed);
    sim.run_on(&node, |node, _| async move {
        let file = create_torn(&node).await;
        file.write_at(0, &[block(&pool(), &[3])]).await.unwrap();
        file.sync().await.unwrap();
    })
    .unwrap();
    sim.crash(&node, Crash::Power);
    sim.run_on(&node, |node, _| async move {
        let file = node.files().open(Path::new("a"), Mode::Read).await.unwrap();
        read(&file, &pool(), 0, 1_024).await
    })
    .unwrap()
}

#[test]
fn a_sync_after_a_write_on_a_sector_that_a_failed_sync_lost_makes_it_durable() {
    let kept: BTreeSet<Vec<u8>> = (0..64)
        .map(|seed| rewritten(seed)[..512].to_vec())
        .collect();
    let over = |byte| [[3].as_slice(), &[byte; 511]].concat();
    assert_eq!(kept, BTreeSet::from([over(1), over(2)]));
}

#[test]
fn a_process_crash_keeps_the_writes_that_a_failed_sync_lost() {
    let outcomes: BTreeSet<(u8, u8)> = (0..64)
        .flat_map(|seed| {
            let (mut sim, node) = disk(seed);
            crash_after(&mut sim, &node, Crash::Process, |node| async move {
                drop(create_torn(&node).await);
            });
            let shown = sectors_of(&mut sim, &node);
            sim.crash(&node, Crash::Power);
            shown.into_iter().zip(sectors_of(&mut sim, &node))
        })
        .collect();
    assert_eq!(outcomes, BTreeSet::from([(1, 1), (2, 1), (2, 2)]));
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
    sim.run_on(&node, |node, _| async move {
        node.clock().sleep(Span::MILLISECOND).await;
        let file = node.files().open(Path::new("a"), Mode::Read).await.unwrap();
        sectors(&read(&file, &pool(), 0, 1_024).await)
    })
    .unwrap()
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

/// [`write_in_flight`] with no fault, and with the file, the block, and the write
/// leaked.
async fn leaked_write_in_flight(node: node::Node) {
    let (file, pool) = (create_synced(&node).await, pool());
    until_crash(&node).await;
    let file = Box::leak(Box::new(file));
    let parts = Box::leak(Box::new([block(&pool, &[9; 1_024])]));
    hang(Box::leak(Box::new(Box::pin(file.write_at(0, parts))))).await;
}

#[test]
fn a_leaked_write_in_flight_at_a_crash_keeps_any_subset_of_its_sectors() {
    let all = BTreeSet::from([vec![1, 1], vec![1, 9], vec![9, 1], vec![9, 9]]);
    for crash in [Crash::Process, Crash::Power] {
        let outcomes: BTreeSet<Vec<u8>> = (0..128)
            .map(|seed| {
                let (mut sim, node) = disk(seed);
                crash_after(&mut sim, &node, crash, leaked_write_in_flight);
                sectors_of(&mut sim, &node)
            })
            .collect();
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
        let open = sim
            .run_on(&node, |node, _| async move {
                node.files().open(Path::new("a"), Mode::Write).await.err()
            })
            .unwrap();
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
    let free = sim
        .run_on(&node, |node, _| async move {
            let files = node.files();
            files.remove(Path::new("a")).await.unwrap();
            files.sync_dir(Path::new("")).await.unwrap();
            files.free().await.unwrap()
        })
        .unwrap();
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
    let names = sim
        .run_on(&node, |node, _| async move {
            node.files().list(Path::new("")).await.unwrap()
        })
        .unwrap();
    assert_eq!(names, Vec::<PathBuf>::new());
}

/// The length of file `a` after `crash` cuts a create of 1 KiB of it, or `None` when
/// no file is there, then its length after a create of 1 KiB, then the free bytes
/// after a remove of it. A removed file keeps its room while a durable entry names it.
/// When `empty`, a durable file `a` with no bytes is there before.
fn create_at_a_crash(seed: u64, crash: Crash, empty: bool) -> (Option<u64>, u64, u64) {
    let (mut sim, node) = disk(seed);
    crash_after(&mut sim, &node, crash, move |node| async move {
        if empty {
            drop(create(&node, "a", 0).await);
            node.files().sync_dir(Path::new("")).await.unwrap();
        }
        until_crash(&node).await;
        let mode = Mode::Create { len: 1_024 };
        hang(node.files().open(Path::new("a"), mode)).await;
    });
    sim.run_on(&node, |node, _| async move {
        let files = node.files();
        let opened = files.open(Path::new("a"), Mode::Write).await;
        let cut = opened.ok().map(|file| file.len());
        let len = create(&node, "a", 1_024).await.len();
        files.remove(Path::new("a")).await.unwrap();
        (cut, len, files.free().await.unwrap())
    })
    .unwrap()
}

/// The outcomes of [`create_at_a_crash`] over 32 seeds.
fn creates_at_a_crash(crash: Crash, empty: bool) -> BTreeSet<(Option<u64>, u64, u64)> {
    (0..32)
        .map(|seed| create_at_a_crash(seed, crash, empty))
        .collect()
}

#[test]
fn a_crash_in_a_create_can_leave_the_file_with_no_bytes() {
    let process = BTreeSet::from([(Some(0), 1_024, MIB), (Some(1_024), 1_024, MIB)]);
    assert_eq!(creates_at_a_crash(Crash::Process, false), process);
    let power = BTreeSet::from([
        (None, 1_024, MIB),
        (Some(0), 1_024, MIB - 1_024),
        (Some(1_024), 1_024, MIB - 1_024),
    ]);
    assert_eq!(creates_at_a_crash(Crash::Power, false), power);
}

#[test]
fn a_crash_in_a_create_over_a_synced_file_with_no_bytes_keeps_it() {
    let kept = MIB - 1_024;
    let process = BTreeSet::from([(Some(0), 1_024, kept), (Some(1_024), 1_024, kept)]);
    assert_eq!(creates_at_a_crash(Crash::Process, true), process);
    assert_eq!(creates_at_a_crash(Crash::Power, true), process);
}

/// The lengths of `a` after a crash in a create of 1 KiB, in each run, with the digest
/// of the run. `before` runs first.
fn cut_creates<F, B>(crash: Crash, before: B) -> BTreeSet<(u64, Option<u64>, u64)>
where
    B: Fn(node::Node) -> F + Copy + Send + 'static,
    F: Future<Output = ()> + 'static,
{
    (0..32)
        .map(|seed| {
            let (mut sim, node) = disk(seed);
            crash_after(&mut sim, &node, crash, move |node| async move {
                before(node.clone()).await;
                until_crash(&node).await;
                let mode = Mode::Create { len: 1_024 };
                hang(node.files().open(Path::new("a"), mode)).await;
            });
            let digest = sim.digest();
            sim.run_on(&node, move |node, _| async move {
                let files = node.files();
                let opened = files.open(Path::new("a"), Mode::Write).await;
                let len = opened.ok().map(|file| file.len());
                (digest, len, files.free().await.unwrap())
            })
            .unwrap()
        })
        .collect()
}

#[test]
fn a_power_crash_in_a_create_over_a_file_with_bytes_has_no_effect() {
    let before =
        |node: node::Node| async move { drop(create(&node, "a", 1_024).await) };
    let power = cut_creates(Crash::Power, before);
    let lens: BTreeSet<_> = power.iter().map(|&(_, len, free)| (len, free)).collect();
    assert_eq!(lens, BTreeSet::from([(None, MIB)]));
}

#[test]
fn a_create_that_a_power_crash_keeps_frees_the_file_of_the_old_entry() {
    let before = |node: node::Node| async move {
        drop(create(&node, "a", 4 * KIB).await);
        node.files().sync_dir(Path::new("")).await.unwrap();
        node.files().remove(Path::new("a")).await.unwrap();
    };
    let power = cut_creates(Crash::Power, before);
    let lens: BTreeSet<_> = power.iter().map(|&(_, len, free)| (len, free)).collect();
    let kept = MIB - 1_024;
    let reached = [
        (Some(4 * KIB), MIB - 4 * KIB),
        (Some(0), MIB),
        (Some(1_024), kept),
    ];
    assert_eq!(lens, BTreeSet::from(reached));
}

#[test]
fn a_power_crash_in_a_create_over_a_file_with_no_bytes_can_keep_its_entry() {
    let before = |node: node::Node| async move { drop(create(&node, "a", 0).await) };
    let power = cut_creates(Crash::Power, before);
    let lens: BTreeSet<_> = power.iter().map(|&(_, len, _)| len).collect();
    assert_eq!(lens, BTreeSet::from([None, Some(0), Some(1_024)]));
}

#[test]
fn a_crash_in_a_write_open_of_a_missing_file_makes_no_file() {
    for crash in [Crash::Process, Crash::Power] {
        for seed in 0..32 {
            let (mut sim, node) = disk(seed);
            crash_after(&mut sim, &node, crash, move |node| async move {
                until_crash(&node).await;
                hang(node.files().open(Path::new("a"), Mode::Write)).await;
            });
            let names = sim
                .run_on(&node, |node, _| async move {
                    node.files().list(Path::new("")).await.unwrap()
                })
                .unwrap();
            assert_eq!(names, Vec::<PathBuf>::new(), "{crash:?} {seed}");
        }
    }
}

#[test]
fn the_digest_holds_the_state_that_a_crash_in_a_create_drew() {
    for crash in [Crash::Process, Crash::Power] {
        let runs = cut_creates(crash, |_| async {});
        let mut states = BTreeMap::new();
        for (digest, len, _) in runs {
            states
                .entry(digest)
                .or_insert_with(BTreeSet::new)
                .insert(len);
        }
        assert!(
            states.values().all(|lens| lens.len() == 1),
            "{crash:?}: {states:?}"
        );
    }
}

#[test]
fn a_power_crash_in_a_create_that_fills_the_disk_leaves_no_file_with_bytes() {
    let mut outcomes = BTreeSet::new();
    for seed in 0..32 {
        let (mut sim, node) = disk(seed);
        crash_after(&mut sim, &node, Crash::Power, |node| async move {
            until_crash(&node).await;
            let mode = Mode::Create { len: 2 * MIB };
            hang(node.files().open(Path::new("a"), mode)).await;
        });
        let outcome = sim
            .run_on(&node, |node, _| async move {
                let files = node.files();
                let names = files.list(Path::new("")).await.unwrap();
                (names, files.free().await.unwrap())
            })
            .unwrap();
        outcomes.insert(outcome);
    }
    let made = vec![PathBuf::from("a")];
    assert_eq!(outcomes, BTreeSet::from([(Vec::new(), MIB), (made, MIB)]));
}

/// Whether a crash by `crash` in a create open of `path`, after `before`, draws a
/// state: its digest differs from that of a crash in a write open of `path`.
fn draws<F, B>(seed: u64, crash: Crash, path: &'static str, before: B) -> bool
where
    B: FnOnce(node::Node) -> F + Copy + Send + 'static,
    F: Future<Output = ()> + 'static,
{
    let digest = |mode| {
        let (mut sim, node) = disk(seed);
        crash_after(&mut sim, &node, crash, move |node| async move {
            before(node.clone()).await;
            until_crash(&node).await;
            hang(node.files().open(Path::new(path), mode)).await;
        });
        sim.digest()
    };
    digest(Mode::Create { len: 1_024 }) != digest(Mode::Write)
}

#[test]
fn a_crash_in_a_create_that_makes_no_file_draws_no_state() {
    let nothing = |_: node::Node| async {};
    let dir = |node: node::Node| async move {
        node.files().create_dir(Path::new("d")).await.unwrap();
    };
    // Leaked, so the crash ends the open while `a` is still held.
    let held = |node: node::Node| async move {
        Box::leak(Box::new(create(&node, "a", 0).await));
    };
    for crash in [Crash::Process, Crash::Power] {
        for seed in 0..8 {
            let at = format!("{crash:?} {seed}");
            assert!(draws(seed, crash, "a", nothing), "a: {at}");
            assert!(!draws(seed, crash, "d", dir), "d: {at}");
            assert!(!draws(seed, crash, "x/a", nothing), "x/a: {at}");
            assert!(!draws(seed, crash, "a/", nothing), "a/: {at}");
            assert!(!draws(seed, crash, "a", held), "held a: {at}");
        }
    }
}

#[test]
fn a_create_over_a_held_file_with_no_bytes_is_busy() {
    let (mut sim, node) = disk(0);
    let opened = sim
        .run_on(&node, |node, _| async move {
            let held = create(&node, "a", 0).await;
            let mode = Mode::Create { len: 1_024 };
            let opened = node.files().open(Path::new("a"), mode).await;
            drop(held);
            opened.map(|file| file.len())
        })
        .unwrap();
    assert_eq!(opened, Err(Error::Busy { path: "a".into() }));
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

#[test]
fn a_process_crash_frees_a_file_that_a_write_open_in_flight_opens() {
    for value in 0..16 {
        let (mut sim, node) = disk(value);
        crash_after(&mut sim, &node, Crash::Process, |node| async move {
            drop(create_synced(&node).await);
            until_crash(&node).await;
            hang(node.files().open(Path::new("a"), Mode::Write)).await;
        });
        let open = sim
            .run_on(&node, |node, _| async move {
                node.files().open(Path::new("a"), Mode::Write).await.err()
            })
            .unwrap();
        assert_eq!(open, None, "value {value}");
    }
}

#[test]
fn a_process_crash_applies_a_create_dir_in_flight() {
    for value in 0..16 {
        let (mut sim, node) = disk(value);
        crash_after(&mut sim, &node, Crash::Process, |node| async move {
            until_crash(&node).await;
            hang(node.files().create_dir(Path::new("d"))).await;
        });
        let names = sim
            .run_on(&node, |node, _| async move {
                node.files().list(Path::new("")).await.unwrap()
            })
            .unwrap();
        assert_eq!(names, [PathBuf::from("d")], "value {value}");
    }
}

/// The names in the data directory after a process crash with a remove of the
/// synced file `a` in flight, and then a create of `a` in flight.
fn remove_then_create_at_a_crash(value: u64) -> Vec<PathBuf> {
    let (mut sim, node) = disk(value);
    crash_after(&mut sim, &node, Crash::Process, |node| async move {
        drop(create_synced(&node).await);
        until_crash(&node).await;
        let files = node.files();
        let mut remove = pin!(files.remove(Path::new("a")));
        let mode = Mode::Create { len: 1_024 };
        let mut create = pin!(files.open(Path::new("a"), mode));
        poll_fn(|cx| {
            assert!(remove.as_mut().poll(cx).is_pending());
            assert!(create.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        pending::<()>().await;
    });
    sim.run_on(&node, |node, _| async move {
        node.clock().sleep(Span::MILLISECOND).await;
        node.files().list(Path::new("")).await.unwrap()
    })
    .unwrap()
}

#[test]
fn calls_in_flight_at_a_process_crash_end_in_any_order() {
    let outcomes: BTreeSet<Vec<PathBuf>> =
        (0..64).map(remove_then_create_at_a_crash).collect();
    let both = BTreeSet::from([vec![], vec![PathBuf::from("a")]]);
    assert_eq!(outcomes, both);
}

/// The names in the data directory after a process crash with a create of file `a`
/// in flight and then a `sync_dir` of its directory in flight, a restart, and a
/// power cut.
fn create_then_sync_dir_at_a_crash(value: u64) -> Vec<PathBuf> {
    let (mut sim, node) = disk(value);
    crash_after(&mut sim, &node, Crash::Process, |node| async move {
        until_crash(&node).await;
        let files = node.files();
        let mode = Mode::Create { len: 1_024 };
        let mut create = pin!(files.open(Path::new("a"), mode));
        let mut sync = pin!(files.sync_dir(Path::new("")));
        poll_fn(|cx| {
            assert!(create.as_mut().poll(cx).is_pending());
            assert!(sync.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        pending::<()>().await;
    });
    sim.run_on(&node, |node, _| async move {
        node.clock().sleep(Span::MILLISECOND).await;
    })
    .unwrap();
    sim.crash(&node, Crash::Power);
    sim.run_on(&node, |node, _| async move {
        node.files().list(Path::new("")).await.unwrap()
    })
    .unwrap()
}

#[test]
fn a_sync_dir_in_flight_at_a_process_crash_may_miss_a_create_in_flight() {
    let outcomes: BTreeSet<Vec<PathBuf>> =
        (0..64).map(create_then_sync_dir_at_a_crash).collect();
    let both = BTreeSet::from([vec![], vec![PathBuf::from("a")]]);
    assert_eq!(outcomes, both);
}

/// Whether a write open of the file `a` of `node` fails, in a new run.
fn write_open(sim: &mut Sim, node: &node::Node) -> Option<Error> {
    sim.run_on(node, |node, _| async move {
        node.files().open(Path::new("a"), Mode::Write).await.err()
    })
    .unwrap()
}

#[test]
fn a_leaked_write_handle_is_free_after_a_crash() {
    for crash in [Crash::Process, Crash::Power] {
        let (mut sim, node) = disk(0);
        crash_after(&mut sim, &node, crash, |node| async move {
            Box::leak(Box::new(create_synced(&node).await));
        });
        assert_eq!(write_open(&mut sim, &node), None, "{crash:?}");
    }
}

/// A value that owns itself through an `Rc`, so it never drops.
struct Cycle(RefCell<Option<(env::files::File, Rc<Cycle>)>>);

#[test]
fn a_write_handle_in_an_rc_cycle_is_free_after_a_crash() {
    for crash in [Crash::Process, Crash::Power] {
        let (mut sim, node) = disk(0);
        crash_after(&mut sim, &node, crash, |node| async move {
            let cycle = Rc::new(Cycle(RefCell::new(None)));
            let file = create_synced(&node).await;
            *cycle.0.borrow_mut() = Some((file, Rc::clone(&cycle)));
        });
        assert_eq!(write_open(&mut sim, &node), None, "{crash:?}");
    }
}

#[test]
fn a_crash_frees_a_file_that_a_leaked_open_holds() {
    for crash in [Crash::Process, Crash::Power] {
        let (mut sim, node) = disk(0);
        crash_after(&mut sim, &node, crash, |node| async move {
            drop(create_synced(&node).await);
            let files = node.files();
            let open = Box::new(Box::pin(files.open(Path::new("a"), Mode::Write)));
            let open = Box::leak(open);
            poll_fn(|cx| {
                assert!(open.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            // The open ends, and nothing takes its handle.
            node.clock().sleep(Span::MILLISECOND).await;
        });
        assert_eq!(write_open(&mut sim, &node), None, "{crash:?}");
    }
}

#[test]
fn a_crash_frees_a_removed_file_that_a_leaked_handle_holds() {
    for crash in [Crash::Process, Crash::Power] {
        let (mut sim, node) = disk(0);
        crash_after(&mut sim, &node, crash, |node| async move {
            Box::leak(Box::new(create(&node, "a", 64 * KIB).await));
            node.files().remove(Path::new("a")).await.unwrap();
        });
        let free = sim
            .run_on(&node, |node, _| async move {
                node.files().free().await.unwrap()
            })
            .unwrap();
        assert_eq!(free, MIB, "{crash:?}");
    }
}

#[test]
fn a_crash_gives_back_the_block_of_a_leaked_read_that_ended() {
    for crash in [Crash::Process, Crash::Power] {
        let (mut sim, node) = disk(0);
        let pool = Arc::new(Mutex::new(pool()));
        let lender = Arc::clone(&pool);
        crash_after(&mut sim, &node, crash, move |node| async move {
            let file = Box::leak(Box::new(create_synced(&node).await));
            let into = lender.lock().unwrap().alloc(1_024).unwrap();
            let read = Box::leak(Box::new(Box::pin(file.read_at(0, into))));
            poll_fn(|cx| {
                assert!(read.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            // The read ends, and nothing takes its block.
            node.clock().sleep(Span::MILLISECOND).await;
        });
        let pool = pool.lock().unwrap();
        // A size gives back its pages at the second purge after its last block returns.
        pool.purge();
        pool.purge();
        assert_eq!(pool.committed(), 0, "{crash:?}");
    }
}

/// A waker that does nothing, so that a test can count its clones.
struct Idle;

#[expect(clippy::manual_noop_waker, reason = "a test counts the clones")]
impl Wake for Idle {
    fn wake(self: Arc<Self>) {}
}

#[test]
fn a_crash_drops_the_waker_of_a_leaked_close() {
    for crash in [Crash::Process, Crash::Power] {
        let (mut sim, node) = disk(0);
        let idle = Arc::new(Idle);
        let waker = Waker::from(Arc::clone(&idle));
        crash_after(&mut sim, &node, crash, move |node| async move {
            let (file, pool) = (create_synced(&node).await, pool());
            until_crash(&node).await;
            let parts = [block(&pool, &[9; 1_024])];
            let mut write = Box::pin(file.write_at(0, &parts));
            poll_fn(|cx| {
                assert!(write.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(write);
            // The close waits for the write, which is in flight at the crash.
            let close = Box::leak(Box::new(Box::pin(file.close())));
            let mut cx = Context::from_waker(&waker);
            assert!(close.as_mut().poll(&mut cx).is_pending());
        });
        assert_eq!(Arc::strong_count(&idle), 1, "{crash:?}");
    }
}

/// A run with two nodes, each with a disk of 1 MiB.
fn disks(seed: u64) -> (Sim, node::Node, node::Node) {
    let mut sim = sim(seed);
    let config = node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    };
    let a = sim.node(config);
    let b = sim.node(config);
    (sim, a, b)
}

#[test]
fn a_crash_keeps_the_ended_call_of_another_node() {
    for crash in [Crash::Process, Crash::Power] {
        let (mut sim, a, b) = disks(0);
        let own = b.clone();
        let other = b.shards().start(shard("b"), move |_| async move {
            let (file, pool) = (create_synced(&own).await, pool());
            let mut read = Box::pin(file.read_at(0, pool.alloc(1_024).unwrap()));
            poll_fn(|cx| {
                assert!(read.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            // The read ends before the crash of `a`, and its result waits past it.
            until_crash(&own).await;
            own.clock().sleep(Span::MILLISECOND).await;
            assert_eq!(&read.await.unwrap()[..], &[1; 1_024][..]);
        });
        crash_after(&mut sim, &a, crash, |_| async {});
        sim.run().unwrap();
        other.unwrap().join().unwrap();
    }
}

#[test]
fn a_crash_keeps_the_close_of_another_node_waiting() {
    for crash in [Crash::Process, Crash::Power] {
        let (mut sim, a, b) = disks(0);
        let own = b.clone();
        let other = b.shards().start(shard("b"), move |_| async move {
            let (file, pool) = (create_synced(&own).await, pool());
            until_crash(&own).await;
            let parts = [block(&pool, &[9; 1_024])];
            let mut write = Box::pin(file.write_at(0, &parts));
            poll_fn(|cx| {
                assert!(write.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(write);
            // The close waits for the write, which is in flight at the crash of `a`.
            file.close().await;
        });
        crash_after(&mut sim, &a, crash, |_| async {});
        sim.run().unwrap();
        other.unwrap().join().unwrap();
    }
}

#[test]
fn a_crash_stops_a_leaked_timer() {
    for crash in [Crash::Process, Crash::Power] {
        let (mut sim, node) = disk(0);
        crash_after(&mut sim, &node, crash, |node| async move {
            let clock = node.clock();
            let sleep = Box::leak(Box::new(Box::pin(clock.sleep(Span::SECOND))));
            poll_fn(|cx| {
                assert!(sleep.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
        });
        let before = node.clock().now();
        sim.run().unwrap();
        assert_eq!(node.clock().now(), before, "{crash:?}");
    }
}

/// The names in the data directory, the sectors of the file at `b` if one is there,
/// and whether the first `sync_dir` ended, after a power cut at an instant set by
/// `seed` in a create of `a`, a `sync_dir`, a write of 1s, a sync, a rename to `b`,
/// and a `sync_dir`.
fn renamed_at_cut(seed: u64) -> (Vec<PathBuf>, Option<Vec<u8>>, bool) {
    let (mut sim, node) = disk(seed);
    let durable = Arc::new(AtomicBool::new(false));
    let synced = Arc::clone(&durable);
    crash_after(&mut sim, &node, Crash::Power, move |node| async move {
        let crash = node::Config::default().monotonic + BEFORE;
        let early = Span::from_nanos(i64::try_from(seed % 64).unwrap() * 10_000);
        node.clock().sleep_until(crash - early).await;
        let (files, pool) = (node.files(), pool());
        let mut file = create(&node, "a", 1_024).await;
        files.sync_dir(Path::new("")).await.unwrap();
        synced.store(true, Ordering::Relaxed);
        file.write_at(0, &[block(&pool, &[1; 1_024])])
            .await
            .unwrap();
        file.sync().await.unwrap();
        file.rename(Path::new("b")).await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
    });
    sim.run_on(&node, |node, _| async move {
        let files = node.files();
        let names = files.list(Path::new("")).await.unwrap();
        let at_b = match files.open(Path::new("b"), Mode::Read).await {
            Ok(file) => Some(sectors(&read(&file, &pool(), 0, 1_024).await)),
            Err(_) => None,
        };
        (names, at_b, durable.load(Ordering::Relaxed))
    })
    .unwrap()
}

#[test]
fn a_power_cut_leaves_a_renamed_file_at_one_name_with_its_synced_bytes() {
    let mut outcomes = BTreeSet::new();
    for seed in 0..64 {
        let (names, at_b, durable) = renamed_at_cut(seed);
        assert!(names.len() <= 1, "seed {seed}: {names:?}");
        if durable {
            assert_eq!(names.len(), 1, "seed {seed}: {names:?}");
        }
        if names == [PathBuf::from("b")] {
            assert_eq!(at_b, Some(vec![1, 1]), "seed {seed}");
        }
        outcomes.insert(names);
    }
    let all = BTreeSet::from([vec![], vec!["a".into()], vec!["b".into()]]);
    assert_eq!(outcomes, all);
}

#[test]
fn a_power_cut_after_a_rename_and_before_its_sync_dir_keeps_the_old_name() {
    for seed in 0..8 {
        let (mut sim, node) = disk(seed);
        crash_after(&mut sim, &node, Crash::Power, |node| async move {
            let mut file = create_synced(&node).await;
            file.rename(Path::new("b")).await.unwrap();
        });
        let names = sim
            .run_on(&node, |node, _| async move {
                node.files().list(Path::new("")).await.unwrap()
            })
            .unwrap();
        assert_eq!(names, [PathBuf::from("a")], "seed {seed}");
    }
}

/// The names in the data directory after `crash` with a rename of the synced file
/// `a` to `b` in flight: the rename is polled 200 us before the crash, so its sync
/// ends, and again at the crash, which puts the rename call in flight.
fn rename_in_flight(seed: u64, crash: Crash) -> Vec<PathBuf> {
    let (mut sim, node) = disk(seed);
    crash_after(&mut sim, &node, crash, |node| async move {
        let mut file = create_synced(&node).await;
        let mut rename = pin!(file.rename(Path::new("b")));
        let at = node::Config::default().monotonic + BEFORE;
        node.clock()
            .sleep_until(at - Span::from_nanos(200_000))
            .await;
        pend(rename.as_mut()).await;
        until_crash(&node).await;
        hang(rename).await;
    });
    sim.run_on(&node, |node, _| async move {
        node.clock().sleep(Span::MILLISECOND).await;
        node.files().list(Path::new("")).await.unwrap()
    })
    .unwrap()
}

#[test]
fn a_process_crash_applies_a_rename_in_flight_and_a_power_cut_drops_it() {
    for seed in 0..8 {
        let applied = rename_in_flight(seed, Crash::Process);
        assert_eq!(applied, [PathBuf::from("b")], "seed {seed}");
        let dropped = rename_in_flight(seed, Crash::Power);
        assert_eq!(dropped, [PathBuf::from("a")], "seed {seed}");
    }
}
