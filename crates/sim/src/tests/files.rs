//! Tests of the simulated disk through `env::files`.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::future::poll_fn;
use std::path::Path;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use block::{Block, Pool};
use env::files::{Error, File, Mode, Operation};
use env::tasks::Tasks;
use env::thread::Handle;
use types::time::Span;

use super::{shard, sim};
use crate::node;

pub(super) const KIB: u64 = 1 << 10;
pub(super) const MIB: u64 = 1 << 20;

/// Runs `body` on a shard of a node with a disk of `disk_bytes`, and returns what it
/// gives.
fn run<T, F>(
    seed: u64,
    disk_bytes: u64,
    body: impl FnOnce(node::Node, Tasks) -> F + Send + 'static,
) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let mut sim = sim(seed);
    let node = sim.node(node::Config {
        disk_bytes,
        ..node::Config::default()
    });
    let out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&out);
    let shards = node.shards();
    let handle = shards.start(shard("disk"), move |tasks| async move {
        let value = body(node, tasks).await;
        *slot.lock().unwrap() = Some(value);
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    out.lock().unwrap().take().expect("the shard gave a value")
}

pub(super) fn pool() -> Pool {
    let config = block::Config { budget: 64 << 10 };
    let memory = block::Heap::new(config.reservation());
    Pool::new(config, memory)
}

pub(super) fn block(pool: &Pool, bytes: &[u8]) -> Block {
    let mut unique = pool.alloc(bytes.len()).unwrap();
    unique.copy_from_slice(bytes);
    unique.freeze()
}

/// The `len` bytes of `file` at `offset`.
pub(super) async fn read(file: &File, pool: &Pool, offset: u64, len: usize) -> Vec<u8> {
    let into = pool.alloc(len).unwrap();
    file.read_at(offset, into).await.unwrap().to_vec()
}

pub(super) async fn create(node: &node::Node, path: &str, len: u64) -> File {
    let mode = Mode::Create { len };
    node.files()
        .unwrap()
        .open(Path::new(path), mode)
        .await
        .unwrap()
}

pub(super) fn io(path: &str, operation: Operation, code: i32) -> Error {
    Error::Io {
        path: path.into(),
        operation,
        code,
    }
}

/// The first byte of each 512-byte sector of `bytes`, after a check that each sector
/// holds one value.
pub(super) fn sectors(bytes: &[u8]) -> Vec<u8> {
    (bytes.chunks(512))
        .map(|sector| {
            assert!(
                sector.iter().all(|&byte| byte == sector[0]),
                "a sector holds the bytes of one call: {sector:?}"
            );
            sector[0]
        })
        .collect()
}

#[test]
fn a_read_after_a_write_ends_sees_it_and_a_new_file_reads_zeros() {
    let bytes: Vec<u8> = (1..=250).cycle().take(1_000).collect();
    let written = bytes.clone();
    let (fresh, whole, reopened) = run(0, MIB, |node, _| async move {
        let pool = pool();
        let file = create(&node, "a", 4_096).await;
        let fresh = read(&file, &pool, 0, 4_096).await;
        let parts = [block(&pool, &written[..300]), block(&pool, &written[300..])];
        file.write_at(100, &parts).await.unwrap();
        let whole = read(&file, &pool, 0, 4_096).await;
        drop(file);
        let file = node
            .files()
            .unwrap()
            .open(Path::new("a"), Mode::Write)
            .await;
        let reopened = read(&file.unwrap(), &pool, 100, 1_000).await;
        (fresh, whole, reopened)
    });
    assert_eq!(fresh, vec![0; 4_096]);
    let mut expected = vec![0; 4_096];
    expected[100..1_100].copy_from_slice(&bytes);
    assert_eq!(whole, expected);
    assert_eq!(reopened, bytes);
}

#[test]
fn each_call_takes_up_to_100_us() {
    let spans = run(0, MIB, |node, _| async move {
        let (files, clock) = (node.files().unwrap(), node.clock());
        let mut spans = Vec::new();
        for _ in 0..32 {
            let start = clock.now();
            files.free().await.unwrap();
            spans.push(clock.now() - start);
        }
        spans
    });
    let range = Span::ZERO..=Span::from_nanos(100 * Span::MICROSECOND.nanos());
    assert!(spans.iter().all(|span| range.contains(span)), "{spans:?}");
    assert!(spans.iter().any(|&span| span > Span::ZERO), "{spans:?}");
}

#[test]
fn open_reports_each_failure() {
    let results = run(0, 64 * KIB, |node, _| async move {
        let files = node.files().unwrap();
        files.create_dir(Path::new("d")).await.unwrap();
        let file = create(&node, "f", 4_096).await;
        let mut results = Vec::new();
        for (path, mode) in [
            ("missing", Mode::Read),
            ("missing", Mode::Write),
            ("f", Mode::Create { len: 8 }),
            ("big", Mode::Create { len: 56 * KIB + 1 }),
            ("d", Mode::Read),
            ("", Mode::Write),
            ("f/g", Mode::Create { len: 1 }),
            ("missing/g", Mode::Create { len: 1 }),
            ("big", Mode::Create { len: 56 * KIB }),
        ] {
            results.push(files.open(Path::new(path), mode).await.map(drop));
        }
        drop(file);
        results
    });
    let missing = Error::NotFound {
        path: "missing".into(),
    };
    let length = Error::Length {
        path: "f".into(),
        expected: 8,
        found: 4_096,
    };
    let full = Error::Full { path: "big".into() };
    let expected = [
        Err(missing.clone()),
        Err(missing),
        Err(length),
        Err(full),
        Err(io("d", Operation::Open, 21)),
        Err(io("", Operation::Open, 21)),
        Err(io("f/g", Operation::Open, 20)),
        Err(Error::NotFound {
            path: "missing/g".into(),
        }),
        Ok(()),
    ];
    assert_eq!(results, expected);
}

#[test]
fn the_directory_calls_report_each_failure() {
    let results = run(0, 16 * KIB, |node, _| async move {
        let files = node.files().unwrap();
        drop(create(&node, "f", 4_096).await);
        let mut results = Vec::new();
        for dir in ["d", "d", "", "x/y", "f", "f/y", "e", "g", "h"] {
            results.push(files.create_dir(Path::new(dir)).await);
        }
        for dir in ["d", "", "missing", "f"] {
            results.push(files.sync_dir(Path::new(dir)).await);
        }
        for path in ["missing", "d", "", "f/y", "f"] {
            results.push(files.remove(Path::new(path)).await);
        }
        results
    });
    let expected = [
        Ok(()),
        Ok(()),
        Ok(()),
        Err(Error::NotFound { path: "x/y".into() }),
        Err(io("f", Operation::CreateDir, 17)),
        Err(io("f/y", Operation::CreateDir, 20)),
        Ok(()),
        Ok(()),
        Err(Error::Full { path: "h".into() }),
        Ok(()),
        Ok(()),
        Err(Error::NotFound {
            path: "missing".into(),
        }),
        Err(io("f", Operation::SyncDir, 20)),
        Ok(()),
        Err(io("d", Operation::Remove, 21)),
        Err(io("", Operation::Remove, 21)),
        Err(io("f/y", Operation::Remove, 20)),
        Ok(()),
    ];
    assert_eq!(results, expected);
}

#[test]
fn list_gives_the_bare_names_in_a_directory() {
    let results = run(0, MIB, |node, _| async move {
        let files = node.files().unwrap();
        files.create_dir(Path::new("d")).await.unwrap();
        files.create_dir(Path::new("d/c")).await.unwrap();
        for path in ["d/b", "d/a", "z", "d/c/x"] {
            drop(create(&node, path, 512).await);
        }
        let mut results = Vec::new();
        for dir in ["", "d", "./d", "missing", "z"] {
            results.push(files.list(Path::new(dir)).await);
        }
        results
    });
    let names = |names: &[&str]| Ok(names.iter().map(Into::into).collect());
    let expected = [
        names(&["d", "z"]),
        names(&["a", "b", "c"]),
        names(&["a", "b", "c"]),
        Err(Error::NotFound {
            path: "missing".into(),
        }),
        Err(io("z", Operation::List, 20)),
    ];
    assert_eq!(results, expected);
}

#[test]
fn each_node_has_its_own_disk() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let made = a.shards().start(shard("a"), move |_| async move {
        a.files().unwrap().create_dir(Path::new("d")).await.unwrap();
    });
    let names = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&names);
    let listed = b.shards().start(shard("b"), move |_| async move {
        b.clock().sleep(Span::MILLISECOND).await;
        *slot.lock().unwrap() = Some(b.files().unwrap().list(Path::new("")).await);
    });
    sim.run().unwrap();
    for handle in [made, listed] {
        handle.unwrap().join().unwrap();
    }
    assert_eq!(names.lock().unwrap().take(), Some(Ok(Vec::new())));
}

#[test]
fn free_counts_each_file_and_directory_until_its_last_handle_closes() {
    let frees = run(0, MIB, |node, _| async move {
        let files = node.files().unwrap();
        let mut frees = vec![files.free().await.unwrap()];
        let file = create(&node, "f", 64 * KIB).await;
        frees.push(files.free().await.unwrap());
        drop(create(&node, "f", 64 * KIB).await);
        frees.push(files.free().await.unwrap());
        files.create_dir(Path::new("d")).await.unwrap();
        frees.push(files.free().await.unwrap());
        files.remove(Path::new("f")).await.unwrap();
        frees.push(files.free().await.unwrap());
        drop(file);
        frees.push(files.free().await.unwrap());
        frees
    });
    let created = MIB - 64 * KIB;
    let expected = [MIB, created, created, created - 4 * KIB, created - 4 * KIB];
    assert_eq!(frees[..5], expected);
    assert_eq!(frees[5], MIB - 4 * KIB, "the last handle closed");
}

#[test]
fn a_remove_frees_a_durable_file_only_after_sync_dir() {
    let frees = run(0, MIB, |node, _| async move {
        let files = node.files().unwrap();
        drop(create(&node, "f", 64 * KIB).await);
        files.sync_dir(Path::new("")).await.unwrap();
        files.remove(Path::new("f")).await.unwrap();
        let removed = files.free().await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
        (removed, files.free().await.unwrap())
    });
    assert_eq!(frees, (MIB - 64 * KIB, MIB));
}

#[test]
fn a_fault_fails_the_next_call_on_its_path_once() {
    let results = run(0, MIB, |node, _| async move {
        let (files, pool) = (node.files().unwrap(), pool());
        let a = create(&node, "a", 4_096).await;
        let b = create(&node, "b", 4_096).await;
        node.fail_file(Path::new("a"), Operation::Sync);
        node.fail_file(Path::new("./d"), Operation::CreateDir);
        node.fail_file(Path::new(""), Operation::Free);
        let parts = [block(&pool, &[1; 512])];
        vec![
            b.sync().await,
            a.write_at(0, &parts).await,
            a.sync().await,
            a.write_at(0, &parts).await,
            files.free().await.map(drop),
            files.free().await.map(drop),
            files.create_dir(Path::new("d")).await,
            files.create_dir(Path::new("d")).await,
            b.sync().await,
        ]
    });
    let expected = [
        Ok(()),
        Ok(()),
        Err(io("a", Operation::Sync, 5)),
        Err(Error::Poisoned { path: "a".into() }),
        Err(io("", Operation::Free, 5)),
        Ok(()),
        Err(io("d", Operation::CreateDir, 5)),
        Ok(()),
        Ok(()),
    ];
    assert_eq!(results, expected);
}

#[test]
#[should_panic(expected = "free has no path; aim a fault at it with an empty path")]
fn a_fault_on_free_with_a_path_panics() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    node.fail_file(Path::new("a"), Operation::Free);
}

/// The sectors that a read of two sectors gives while a write of 0xab over them is
/// in flight.
fn read_in_flight(seed: u64) -> Vec<u8> {
    run(seed, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 1_024).await);
        let pool = Rc::new(pool());
        let (writer, parts) = (Rc::clone(&file), [block(&pool, &[0xab; 1_024])]);
        tasks.spawn(async move { writer.write_at(0, &parts).await.unwrap() });
        sectors(&read(&file, &pool, 0, 1_024).await)
    })
}

#[test]
fn a_read_in_flight_with_a_write_sees_old_or_new_bytes_per_sector() {
    let reads: BTreeSet<Vec<u8>> = (0..64).map(read_in_flight).collect();
    let both =
        BTreeSet::from([vec![0, 0], vec![0, 0xab], vec![0xab, 0], vec![0xab; 2]]);
    assert_eq!(reads, both);
    assert_eq!(read_in_flight(7), read_in_flight(7));
}

/// Whether a write of 0xab over two sectors ended first, and the sectors that a read
/// of them gives, when both start at once.
fn read_over_write(seed: u64) -> (bool, Vec<u8>) {
    run(seed, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 1_024).await);
        let pool = Rc::new(pool());
        let written = Rc::new(Cell::new(false));
        let (writer, parts) = (Rc::clone(&file), [block(&pool, &[0xab; 1_024])]);
        let flag = Rc::clone(&written);
        tasks.spawn(async move {
            writer.write_at(0, &parts).await.unwrap();
            flag.set(true);
        });
        let sectors = sectors(&read(&file, &pool, 0, 1_024).await);
        (written.get(), sectors)
    })
}

#[test]
fn a_read_may_miss_a_write_that_ends_before_it() {
    let reads: Vec<(bool, Vec<u8>)> = (0..64).map(read_over_write).collect();
    let missed = reads
        .iter()
        .any(|(ended, sectors)| *ended && sectors.contains(&0));
    assert!(missed, "{reads:?}");
}

/// The sectors after two writes over the same two sectors were in flight at once.
fn writes_in_flight(seed: u64) -> Vec<u8> {
    run(seed, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 1_024).await);
        let pool = pool();
        let (writer, parts) = (Rc::clone(&file), [block(&pool, &[1; 1_024])]);
        tasks.spawn(async move { writer.write_at(0, &parts).await.unwrap() });
        file.write_at(0, &[block(&pool, &[2; 1_024])])
            .await
            .unwrap();
        node.clock().sleep(Span::MILLISECOND).await;
        sectors(&read(&file, &pool, 0, 1_024).await)
    })
}

#[test]
fn writes_in_flight_over_one_sector_leave_either_bytes() {
    let writes: BTreeSet<Vec<u8>> = (0..64).map(writes_in_flight).collect();
    let both = BTreeSet::from([vec![1, 1], vec![1, 2], vec![2, 1], vec![2, 2]]);
    assert_eq!(writes, both);
}

/// The sectors of a file after a write of 0xab over its four sectors whose future
/// dropped after one poll.
fn dropped_write(seed: u64) -> Vec<u8> {
    run(seed, MIB, |node, _| async move {
        let (file, pool) = (create(&node, "a", 2_048).await, pool());
        let parts = [block(&pool, &[0xab; 2_048])];
        let mut write = Box::pin(file.write_at(0, &parts));
        poll_fn(|cx| {
            assert!(write.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(write);
        node.clock().sleep(Span::MILLISECOND).await;
        sectors(&read(&file, &pool, 0, 2_048).await)
    })
}

#[test]
fn a_dropped_write_still_ends_with_any_subset_of_its_sectors() {
    let writes: BTreeSet<Vec<u8>> = (0..64).map(dropped_write).collect();
    assert!(writes.contains(&vec![0xab; 4]), "all kept: {writes:?}");
    assert!(writes.contains(&vec![0; 4]), "none kept: {writes:?}");
    let some = |sectors: &Vec<u8>| sectors.contains(&0) && sectors.contains(&0xab);
    assert!(writes.iter().any(some), "some kept: {writes:?}");
}

#[test]
#[should_panic(expected = "a file call needs a thread that the sim started")]
fn a_file_call_outside_the_sim_panics() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let files = node.files().unwrap();
    let mut free = pin!(files.free());
    drop(free.as_mut().poll(&mut Context::from_waker(Waker::noop())));
}

#[test]
fn a_file_call_on_a_thread_of_another_node_panics() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let handle: Handle = (b.shards())
        .start(shard("b"), move |_| async move {
            drop(a.files().unwrap().free().await);
        })
        .unwrap();
    let e = sim.run().unwrap_err();
    let message = "a file call of node 0 runs on a thread of node 1";
    assert!(matches!(e, crate::Error::Panicked { message: m, .. } if m == message));
    drop(handle.join());
}

#[test]
fn a_zero_length_write_races_no_write() {
    let reads: BTreeSet<Vec<u8>> = (0..64)
        .map(|seed| {
            run(seed, MIB, |node, tasks| async move {
                let file = Rc::new(create(&node, "a", 1_024).await);
                let pool = pool();
                let (writer, parts) = (Rc::clone(&file), [block(&pool, &[1; 1_024])]);
                tasks.spawn(async move { writer.write_at(0, &parts).await.unwrap() });
                file.write_at(100, &[]).await.unwrap();
                node.clock().sleep(Span::MILLISECOND).await;
                sectors(&read(&file, &pool, 0, 1_024).await)
            })
        })
        .collect();
    assert_eq!(reads, BTreeSet::from([vec![1, 1]]));
}

#[test]
fn a_read_may_take_the_bytes_of_a_write_still_in_flight() {
    let reads: Vec<(bool, Vec<u8>)> = (0..64).map(read_over_write).collect();
    let taken = reads
        .iter()
        .any(|(ended, sectors)| !*ended && sectors.contains(&0xab));
    assert!(taken, "{reads:?}");
}

/// The sectors that a read of two sectors gives while a write of 0xab over the
/// second is in flight.
fn read_beside_write(seed: u64) -> Vec<u8> {
    run(seed, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 1_024).await);
        let pool = Rc::new(pool());
        let (writer, parts) = (Rc::clone(&file), [block(&pool, &[0xab; 512])]);
        tasks.spawn(async move { writer.write_at(512, &parts).await.unwrap() });
        sectors(&read(&file, &pool, 0, 1_024).await)
    })
}

#[test]
fn a_read_sees_a_write_in_flight_only_where_they_overlap() {
    let reads: BTreeSet<Vec<u8>> = (0..64).map(read_beside_write).collect();
    assert_eq!(reads, BTreeSet::from([vec![0, 0], vec![0, 0xab]]));
}

/// The sectors that a read of file `a` gives while a write of 0xab over file `b` is
/// in flight.
fn read_beside_other_file(seed: u64) -> Vec<u8> {
    run(seed, MIB, |node, tasks| async move {
        let a = create(&node, "a", 1_024).await;
        let b = create(&node, "b", 1_024).await;
        let pool = Rc::new(pool());
        let parts = [block(&pool, &[0xab; 1_024])];
        tasks.spawn(async move { b.write_at(0, &parts).await.unwrap() });
        sectors(&read(&a, &pool, 0, 1_024).await)
    })
}

#[test]
fn a_read_never_sees_a_write_in_flight_on_another_file() {
    for seed in 0..64 {
        assert_eq!(read_beside_other_file(seed), [0, 0], "seed {seed}");
    }
}

/// The sectors that a read gives while a write that a fault fails is in flight over
/// them.
fn read_over_failed_write(seed: u64) -> Vec<u8> {
    run(seed, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 1_024).await);
        let pool = Rc::new(pool());
        node.fail_file(Path::new("a"), Operation::WriteAt);
        let (writer, parts) = (Rc::clone(&file), [block(&pool, &[0xab; 1_024])]);
        tasks.spawn(async move {
            let failed = Err(io("a", Operation::WriteAt, 5));
            assert_eq!(writer.write_at(0, &parts).await, failed);
        });
        sectors(&read(&file, &pool, 0, 1_024).await)
    })
}

#[test]
fn a_read_never_sees_a_write_that_a_fault_fails() {
    for seed in 0..64 {
        assert_eq!(read_over_failed_write(seed), [0, 0], "seed {seed}");
    }
}

/// The span of one `free`, with or without a 1 ns sleep in flight on another task.
fn free_span(seed: u64, sleep: bool) -> Span {
    run(seed, MIB, move |node, tasks| async move {
        let (files, clock) = (node.files().unwrap(), node.clock());
        if sleep {
            let clock = node.clock();
            tasks.spawn(async move { clock.sleep(Span::from_nanos(1)).await });
        }
        let start = clock.now();
        files.free().await.unwrap();
        clock.now() - start
    })
}

#[test]
fn a_call_ends_at_its_own_time_whatever_else_is_due() {
    for seed in 0..16 {
        assert_eq!(free_span(seed, false), free_span(seed, true), "seed {seed}");
    }
}

/// The sectors after two writes were in flight at once over two sectors that a
/// write of 9 filled before.
fn writes_over_filled_sectors(seed: u64) -> Vec<u8> {
    run(seed, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 1_024).await);
        let pool = pool();
        file.write_at(0, &[block(&pool, &[9; 1_024])])
            .await
            .unwrap();
        let (writer, parts) = (Rc::clone(&file), [block(&pool, &[1; 1_024])]);
        tasks.spawn(async move { writer.write_at(0, &parts).await.unwrap() });
        file.write_at(0, &[block(&pool, &[2; 1_024])])
            .await
            .unwrap();
        node.clock().sleep(Span::MILLISECOND).await;
        sectors(&read(&file, &pool, 0, 1_024).await)
    })
}

#[test]
fn writes_in_flight_over_filled_sectors_leave_either_bytes() {
    let writes: BTreeSet<Vec<u8>> = (0..64).map(writes_over_filled_sectors).collect();
    let both = BTreeSet::from([vec![1, 1], vec![1, 2], vec![2, 1], vec![2, 2]]);
    assert_eq!(writes, both);
}

/// The free bytes after an open that makes a 64 KiB file is polled once, its future
/// drops before or after the call ends, and the file is removed.
fn free_after_dropped_open(ended: bool) -> u64 {
    run(0, MIB, move |node, _| async move {
        let (files, clock) = (node.files().unwrap(), node.clock());
        let mut open =
            Box::pin(files.open(Path::new("a"), Mode::Create { len: 64 * KIB }));
        poll_fn(|cx| {
            assert!(open.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        if ended {
            clock.sleep(Span::MILLISECOND).await;
            drop(open);
        } else {
            drop(open);
            clock.sleep(Span::MILLISECOND).await;
        }
        files.remove(Path::new("a")).await.unwrap();
        files.free().await.unwrap()
    })
}

#[test]
fn a_dropped_open_releases_its_file() {
    assert_eq!(free_after_dropped_open(false), MIB, "dropped in flight");
    assert_eq!(free_after_dropped_open(true), MIB, "dropped after the end");
}

/// The digest of a run with one `free`, which a fault fails or not.
fn free_digest(failed: bool) -> u64 {
    let mut sim = sim(3);
    let node = sim.node(node::Config::default());
    if failed {
        node.fail_file(Path::new(""), Operation::Free);
    }
    let handle = node.shards().start(shard("d"), move |_| async move {
        drop(node.files().unwrap().free().await);
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    sim.digest()
}

#[test]
fn the_digest_holds_the_result_of_each_file_call() {
    assert_eq!(free_digest(false), free_digest(false));
    assert_ne!(free_digest(false), free_digest(true));
}

/// The 512 bytes of a file after writes of 1 over bytes 0 to 100 and of 2 over bytes
/// 200 to 300 were in flight at once and both ended with success.
fn writes_that_share_no_byte(value: u64) -> Vec<u8> {
    run(value, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 512).await);
        let pool = pool();
        let (writer, parts) = (Rc::clone(&file), [block(&pool, &[1; 100])]);
        tasks.spawn(async move { writer.write_at(0, &parts).await.unwrap() });
        file.write_at(200, &[block(&pool, &[2; 100])])
            .await
            .unwrap();
        node.clock().sleep(Span::MILLISECOND).await;
        read(&file, &pool, 0, 512).await
    })
}

#[test]
fn writes_in_flight_that_share_no_byte_both_stay() {
    for value in 0..64 {
        let found = writes_that_share_no_byte(value);
        let kept = (found[..100] == [1; 100], found[200..300] == [2; 100]);
        assert_eq!(
            kept,
            (true, true),
            "value {value}: (first kept, second kept)"
        );
    }
}

#[test]
fn a_read_of_the_last_sector_of_the_largest_file_ends() {
    let bytes = run(0, u64::MAX, |node, _| async move {
        let file = create(&node, "a", u64::MAX).await;
        read(&file, &pool(), u64::MAX - 1, 1).await
    });
    assert_eq!(bytes, vec![0]);
}

#[test]
fn an_open_of_a_file_with_a_trailing_slash_fails() {
    let opened = run(0, MIB, |node, _| async move {
        drop(create(&node, "a", 1).await);
        let opened = node
            .files()
            .unwrap()
            .open(Path::new("a/"), Mode::Read)
            .await;
        opened.map(drop)
    });
    assert_eq!(opened, Err(io("a/", Operation::Open, 20)));
}

#[test]
fn a_write_to_the_last_sector_of_the_largest_file_stays() {
    let bytes = run(0, u64::MAX, |node, _| async move {
        let file = create(&node, "a", u64::MAX).await;
        let pool = pool();
        file.write_at(u64::MAX - 1, &[block(&pool, &[7])])
            .await
            .unwrap();
        read(&file, &pool, u64::MAX - 2, 2).await
    });
    assert_eq!(bytes, vec![0, 7]);
}

/// The values of bytes 0 to 100 and 100 to 200 of a file after writes of 1 over the
/// first, 2 over the second, and 3 over both were in flight at once.
fn three_writes_in_flight(value: u64) -> (u8, u8) {
    let bytes = run(value, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 512).await);
        let pool = pool();
        for (offset, len, byte) in [(0, 100, 1), (100, 100, 2)] {
            let (writer, parts) = (Rc::clone(&file), [block(&pool, &vec![byte; len])]);
            tasks.spawn(async move { writer.write_at(offset, &parts).await.unwrap() });
        }
        file.write_at(0, &[block(&pool, &[3; 200])]).await.unwrap();
        node.clock().sleep(Span::MILLISECOND).await;
        read(&file, &pool, 0, 200).await
    });
    let (first, second) = bytes.split_at(100);
    assert!(first.iter().all(|&byte| byte == first[0]), "{first:?}");
    assert!(second.iter().all(|&byte| byte == second[0]), "{second:?}");
    (first[0], second[0])
}

#[test]
fn three_writes_in_flight_leave_the_result_of_each_order() {
    let results: BTreeSet<(u8, u8)> = (0..128).map(three_writes_in_flight).collect();
    let orders = BTreeSet::from([(1, 2), (1, 3), (3, 2), (3, 3)]);
    assert_eq!(results, orders);
}

#[test]
fn a_path_with_a_trailing_slash_names_only_a_directory() {
    let results = run(0, MIB, |node, _| async move {
        let files = node.files().unwrap();
        drop(create(&node, "a", 1).await);
        files.create_dir(Path::new("d")).await.unwrap();
        let mut results = Vec::new();
        for (path, mode) in [
            ("a/", Mode::Write),
            ("a/.", Mode::Read),
            ("a//", Mode::Read),
            ("a/", Mode::Create { len: 1 }),
            ("b/", Mode::Create { len: 1 }),
            ("b/", Mode::Read),
            ("d/", Mode::Read),
        ] {
            results.push(files.open(Path::new(path), mode).await.map(drop));
        }
        for path in ["a/", "b/", "d/", "a"] {
            results.push(files.remove(Path::new(path)).await);
        }
        results
    });
    let expected = [
        Err(io("a/", Operation::Open, 20)),
        Err(io("a/.", Operation::Open, 20)),
        Err(io("a//", Operation::Open, 20)),
        Err(io("a/", Operation::Open, 21)),
        Err(io("b/", Operation::Open, 21)),
        Err(Error::NotFound { path: "b/".into() }),
        Err(io("d/", Operation::Open, 21)),
        Err(io("a/", Operation::Remove, 20)),
        Ok(()),
        Err(io("d/", Operation::Remove, 21)),
        Ok(()),
    ];
    assert_eq!(results, expected);
}

/// The values of bytes 0 to 100 and 100 to 200 of a file after the writes of `order`
/// (offset, length, and value), each started when the one before it ended or, when
/// `spawned`, at once with it.
fn writes_in_order(value: u64, order: [(u64, usize, u8, bool); 3]) -> (u8, u8) {
    let bytes = run(value, MIB, move |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 512).await);
        let pool = pool();
        for (offset, len, byte, spawned) in order {
            let (writer, parts) = (Rc::clone(&file), [block(&pool, &vec![byte; len])]);
            let write = async move { writer.write_at(offset, &parts).await.unwrap() };
            if spawned {
                tasks.spawn(write);
            } else {
                write.await;
            }
        }
        node.clock().sleep(Span::MILLISECOND).await;
        read(&file, &pool, 0, 200).await
    });
    let (first, second) = bytes.split_at(100);
    assert!(first.iter().all(|&byte| byte == first[0]), "{first:?}");
    assert!(second.iter().all(|&byte| byte == second[0]), "{second:?}");
    (first[0], second[0])
}

#[test]
fn three_writes_in_flight_over_nested_bytes_leave_the_result_of_an_order() {
    let writes = [(0, 200, 1, true), (0, 100, 2, true), (0, 200, 3, false)];
    let results: BTreeSet<(u8, u8)> = (0..256)
        .map(|value| writes_in_order(value, writes))
        .collect();
    let orders = BTreeSet::from([(1, 1), (2, 1), (2, 3), (3, 3)]);
    assert_eq!(results, orders);
}

#[test]
fn a_write_in_flight_over_two_in_turn_keeps_their_order() {
    let writes = [(0, 200, 1, true), (0, 100, 2, false), (100, 100, 3, false)];
    let results: BTreeSet<(u8, u8)> = (0..256)
        .map(|value| writes_in_order(value, writes))
        .collect();
    let orders = BTreeSet::from([(1, 1), (1, 3), (2, 3)]);
    assert_eq!(results, orders);
}

/// The bytes of a one-sector file after writes of 1, 2, and 3, with a sync started
/// after each of the first two and still in flight.
fn syncs_in_flight(value: u64) -> Vec<u8> {
    run(value, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 512).await);
        let pool = pool();
        for byte in 1..=3 {
            file.write_at(0, &[block(&pool, &[byte; 512])])
                .await
                .unwrap();
            let syncer = Rc::clone(&file);
            tasks.spawn(async move { syncer.sync().await.unwrap() });
        }
        node.clock().sleep(Span::MILLISECOND).await;
        read(&file, &pool, 0, 512).await
    })
}

#[test]
fn syncs_in_flight_end_in_any_order() {
    for value in 0..64 {
        assert_eq!(syncs_in_flight(value), vec![3; 512], "value {value}");
    }
}

/// The digest of a run in which a shard starts a `free`, drops it, and sleeps `sleep`,
/// so that it wakes before the call ends or after it.
fn slept_around_end(sleep: Span) -> u64 {
    let mut sim = sim(3);
    let node = sim.node(node::Config::default());
    let clock = node.clock();
    let handle = node.shards().start(shard("d"), move |_| async move {
        let files = node.files();
        let mut free = Box::pin(files.free());
        poll_fn(|cx| {
            assert!(free.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(free);
        clock.sleep(sleep).await;
    });
    sim.run().unwrap();
    handle.unwrap().join().unwrap();
    sim.digest()
}

#[test]
fn the_digest_holds_the_polls_before_a_file_end() {
    let early = slept_around_end(Span::from_nanos(1));
    assert_ne!(early, slept_around_end(Span::MILLISECOND));
}
