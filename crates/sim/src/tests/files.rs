//! Tests of the simulated disk through `env::files`.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use block::{Block, Pool};
use env::files::{Error, File, Mode, Operation};
use env::tasks::Tasks;
use env::thread::Handle;
use types::time::{Monotonic, Span};

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
    sim.run_on(&node, body).unwrap()
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
    node.files().open(Path::new(path), mode).await.unwrap()
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
        let file = node.files().open(Path::new("a"), Mode::Write).await;
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
        let (files, clock) = (node.files(), node.clock());
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
        let files = node.files();
        files.create_dir(Path::new("d")).await.unwrap();
        drop(create(&node, "f", 4_096).await);
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
        let files = node.files();
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
        let files = node.files();
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
fn a_failed_create_leaves_no_file() {
    let results = run(0, 16 * KIB, |node, _| async move {
        let files = node.files();
        drop(create(&node, "empty", 0).await);
        let mut results = Vec::new();
        for path in ["missing", "empty"] {
            let mode = Mode::Create { len: MIB };
            results.push(files.open(Path::new(path), mode).await.map(drop));
            results.push(files.open(Path::new(path), Mode::Write).await.map(drop));
        }
        results
    });
    let expected = ["missing", "empty"].map(|path| {
        [
            Err(Error::Full { path: path.into() }),
            Err(Error::NotFound { path: path.into() }),
        ]
    });
    assert_eq!(results, expected.concat());
}

#[test]
fn create_allocates_an_empty_file_that_is_there_once_no_writer_holds_it() {
    let found = run(0, MIB, |node, _| async move {
        let (pool, files) = (pool(), node.files());
        let empty = create(&node, "a", 0).await;
        let mode = Mode::Create { len: 4 * KIB };
        let held = files.open(Path::new("a"), mode).await.map(drop);
        let free = files.free().await.unwrap();
        drop(empty);
        let file = create(&node, "a", 4 * KIB).await;
        let bytes = read(&file, &pool, 0, 4_096).await;
        (held, free, file.len(), bytes, files.free().await.unwrap())
    });
    let busy = Err(Error::Busy { path: "a".into() });
    assert_eq!(found, (busy, MIB, 4 * KIB, vec![0; 4_096], MIB - 4 * KIB));
}

#[test]
fn each_node_has_its_own_disk() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let made = a.shards().start(shard("a"), move |_| async move {
        a.files().create_dir(Path::new("d")).await.unwrap();
    });
    let names = sim.run_on(&b, |b, _| async move {
        b.clock().sleep(Span::MILLISECOND).await;
        b.files().list(Path::new("")).await
    });
    made.unwrap().join().unwrap();
    assert_eq!(names, Ok(Ok(Vec::new())));
}

#[test]
fn free_counts_a_removed_file_until_its_last_handle_closes_and_its_remove_is_durable() {
    let frees = run(0, MIB, |node, _| async move {
        let files = node.files();
        let mut frees = vec![files.free().await.unwrap()];
        let file = create(&node, "f", 64 * KIB).await;
        frees.push(files.free().await.unwrap());
        drop(files.open(Path::new("f"), Mode::Read).await.unwrap());
        frees.push(files.free().await.unwrap());
        files.create_dir(Path::new("d")).await.unwrap();
        frees.push(files.free().await.unwrap());
        files.remove(Path::new("f")).await.unwrap();
        frees.push(files.free().await.unwrap());
        drop(file);
        frees.push(files.free().await.unwrap());
        files.sync_dir(Path::new("")).await.unwrap();
        frees.push(files.free().await.unwrap());
        frees
    });
    let created = MIB - 64 * KIB;
    let expected = [MIB, created, created, created - 4 * KIB, created - 4 * KIB];
    assert_eq!(frees[..5], expected);
    assert_eq!(
        frees[5],
        created - 4 * KIB,
        "the create of `f` is not durable"
    );
    assert_eq!(frees[6], MIB - 4 * KIB, "the remove of `f` is durable");
}

#[test]
fn a_remove_frees_a_durable_file_only_after_sync_dir() {
    let frees = run(0, MIB, |node, _| async move {
        let files = node.files();
        drop(create(&node, "f", 64 * KIB).await);
        files.sync_dir(Path::new("")).await.unwrap();
        files.remove(Path::new("f")).await.unwrap();
        let removed = files.free().await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
        (removed, files.free().await.unwrap())
    });
    assert_eq!(frees, (MIB - 64 * KIB, MIB));
}

/// What a read open of a path gives a millisecond after a create at it, which starts
/// once a remove of the path, with no file there, is polled once and its future drops.
fn create_after_dropped_remove(value: u64) -> Option<Error> {
    run(value, MIB, |node, _| async move {
        let (files, path) = (node.files(), Path::new("a"));
        let mut remove = Box::pin(files.remove(path));
        pend(remove.as_mut()).await;
        drop(remove);
        let created = create(&node, "a", 1_024).await;
        node.clock().sleep(Span::MILLISECOND).await;
        drop(created);
        files.open(path, Mode::Read).await.err()
    })
}

#[test]
fn a_create_after_a_dropped_remove_keeps_its_file() {
    for value in 0..32 {
        assert_eq!(create_after_dropped_remove(value), None, "value {value}");
    }
}

/// What a read open of `a` gives while a remove of it is in flight: one whose future
/// lives, or, when `dropped`, one polled once whose future drops.
fn read_beside_remove(value: u64, dropped: bool) -> Option<Error> {
    run(value, MIB, move |node, tasks| async move {
        let (files, path) = (node.files(), Path::new("a"));
        drop(create(&node, "a", KIB).await);
        if dropped {
            let mut remove = Box::pin(files.remove(path));
            pend(remove.as_mut()).await;
        } else {
            let theirs = files.clone();
            tasks.spawn(async move { theirs.remove(Path::new("a")).await.unwrap() });
        }
        let found = files.open(path, Mode::Read).await.map(drop).err();
        node.clock().sleep(Span::MILLISECOND).await;
        found
    })
}

#[test]
fn a_read_open_does_not_wait_for_a_remove_in_flight() {
    let gone = Some(Error::NotFound { path: "a".into() });
    for dropped in [false, true] {
        let found: Vec<_> = (0..32)
            .map(|value| read_beside_remove(value, dropped))
            .collect();
        assert!(
            found.contains(&None) && found.contains(&gone),
            "{dropped}: {found:?}"
        );
    }
}

/// What a remove of `a` gives once a create of `a` is polled once and its future
/// drops, and the names in the data directory a millisecond later.
fn remove_after_dropped_create(value: u64) -> (Result<(), Error>, Vec<PathBuf>) {
    run(value, MIB, |node, _| async move {
        let (files, path) = (node.files(), Path::new("a"));
        let mut made = Box::pin(files.open(path, Mode::Create { len: KIB }));
        pend(made.as_mut()).await;
        drop(made);
        let removed = files.remove(path).await;
        node.clock().sleep(Span::MILLISECOND).await;
        (removed, files.list(Path::new("")).await.unwrap())
    })
}

#[test]
fn a_remove_waits_for_a_dropped_create() {
    for value in 0..32 {
        let found = remove_after_dropped_create(value);
        assert_eq!(found, (Ok(()), Vec::new()), "value {value}");
    }
}

/// What a create of `a` gives while a remove of it, whose future lives, is in flight,
/// and the names in the data directory a millisecond later.
fn create_beside_remove(value: u64) -> (Option<Error>, Vec<PathBuf>) {
    run(value, MIB, |node, tasks| async move {
        let files = node.files();
        let theirs = files.clone();
        tasks.spawn(async move { theirs.remove(Path::new("a")).await.unwrap() });
        let made = files.open(Path::new("a"), Mode::Create { len: KIB }).await;
        node.clock().sleep(Span::MILLISECOND).await;
        (made.err(), files.list(Path::new("")).await.unwrap())
    })
}

#[test]
fn a_create_does_not_wait_for_a_remove_whose_future_lives() {
    let runs: Vec<_> = (0..32).map(create_beside_remove).collect();
    assert!(runs.iter().all(|(made, _)| made.is_none()), "{runs:?}");
    let names: BTreeSet<_> = runs.into_iter().map(|(_, names)| names).collect();
    assert_eq!(names, BTreeSet::from([vec![], vec![PathBuf::from("a")]]));
}

#[test]
fn a_fault_fails_the_next_call_on_its_path_once() {
    let results = run(0, MIB, |node, _| async move {
        let (files, pool) = (node.files(), pool());
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

/// The bytes that a read of two sectors gives while a write of 0xab over them is in
/// flight.
fn read_in_flight(seed: u64) -> Vec<u8> {
    run(seed, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 1_024).await);
        let pool = Rc::new(pool());
        let (writer, parts) = (Rc::clone(&file), [block(&pool, &[0xab; 1_024])]);
        tasks.spawn(async move { writer.write_at(0, &parts).await.unwrap() });
        read(&file, &pool, 0, 1_024).await
    })
}

#[test]
fn a_read_in_flight_with_a_write_sees_old_or_new_bytes_and_may_split_a_sector() {
    let reads: Vec<Vec<u8>> = (0..128).map(read_in_flight).collect();
    assert_bytes_in(reads.iter().map(Vec::as_slice), &[0, 0xab]);
    let both =
        BTreeSet::from([vec![0, 0], vec![0, 0xab], vec![0xab, 0], vec![0xab; 2]]);
    assert_eq!(whole(&reads), both);
    assert!(reads.iter().any(|bytes| split(bytes)), "no sector split");
    let three = (reads.iter().flat_map(|bytes| bytes.chunks(512)))
        .any(|sector| sector.chunk_by(PartialEq::eq).count() == 3);
    assert!(three, "no sector in three runs");
    assert_eq!(read_in_flight(7), read_in_flight(7));
}

/// Asserts that each byte of each of `runs` is one of `values`.
fn assert_bytes_in<'a>(runs: impl IntoIterator<Item = &'a [u8]>, values: &[u8]) {
    for (value, bytes) in runs.into_iter().enumerate() {
        let wrong = bytes.iter().find(|byte| !values.contains(byte));
        assert_eq!(wrong, None, "value {value}");
    }
}

/// The sectors of each of `runs` in which no sector holds two values.
fn whole(runs: &[Vec<u8>]) -> BTreeSet<Vec<u8>> {
    (runs.iter())
        .filter(|bytes| !split(bytes))
        .map(|bytes| sectors(bytes))
        .collect()
}

/// Whether a sector of `bytes` holds two values.
fn split(bytes: &[u8]) -> bool {
    (bytes.chunks(512)).any(|sector| sector.iter().any(|&byte| byte != sector[0]))
}

/// Whether a write of 0xab over two sectors ended first, and the bytes that a read
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
        let bytes = read(&file, &pool, 0, 1_024).await;
        (written.get(), bytes)
    })
}

#[test]
fn a_read_may_miss_a_write_that_ends_before_it() {
    let reads: Vec<(bool, Vec<u8>)> = (0..64).map(read_over_write).collect();
    let missed = (reads.iter()).any(|(ended, bytes)| *ended && bytes.contains(&0));
    assert!(missed, "{reads:?}");
}

/// The bytes after two writes over the same two sectors were in flight at once.
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
        read(&file, &pool, 0, 1_024).await
    })
}

#[test]
fn writes_in_flight_over_one_sector_leave_either_bytes_and_may_split_it() {
    let writes: Vec<Vec<u8>> = (0..128).map(writes_in_flight).collect();
    assert_bytes_in(writes.iter().map(Vec::as_slice), &[1, 2]);
    let both = BTreeSet::from([vec![1, 1], vec![1, 2], vec![2, 1], vec![2, 2]]);
    assert_eq!(whole(&writes), both);
    assert!(writes.iter().any(|bytes| split(bytes)), "no sector split");
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
    let files = node.files();
    let mut free = pin!(files.free());
    drop(free.as_mut().poll(&mut Context::from_waker(Waker::noop())));
}

#[test]
fn a_file_call_on_a_thread_of_another_node_panics() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let handle: Handle = (b.shards())
        .start(
            shard("b"),
            move |_| async move { drop(a.files().free().await) },
        )
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
    let taken = (reads.iter()).any(|(ended, bytes)| !*ended && bytes.contains(&0xab));
    assert!(taken, "{reads:?}");
}

/// The bytes that a read of two sectors gives while a write of 0xab over the second
/// is in flight.
fn read_beside_write(seed: u64) -> Vec<u8> {
    run(seed, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 1_024).await);
        let pool = Rc::new(pool());
        let (writer, parts) = (Rc::clone(&file), [block(&pool, &[0xab; 512])]);
        tasks.spawn(async move { writer.write_at(512, &parts).await.unwrap() });
        read(&file, &pool, 0, 1_024).await
    })
}

#[test]
fn a_read_sees_a_write_in_flight_only_where_they_overlap() {
    let reads: Vec<Vec<u8>> = (0..64).map(read_beside_write).collect();
    assert_bytes_in(reads.iter().map(|bytes| &bytes[..512]), &[0]);
    assert_bytes_in(reads.iter().map(|bytes| &bytes[512..]), &[0, 0xab]);
    assert_eq!(whole(&reads), BTreeSet::from([vec![0, 0], vec![0, 0xab]]));
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
        let (files, clock) = (node.files(), node.clock());
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

/// The bytes after two writes were in flight at once over two sectors that a write
/// of 9 filled before.
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
        read(&file, &pool, 0, 1_024).await
    })
}

#[test]
fn writes_in_flight_over_filled_sectors_leave_either_bytes() {
    let writes: Vec<Vec<u8>> = (0..256).map(writes_over_filled_sectors).collect();
    assert_bytes_in(writes.iter().map(Vec::as_slice), &[1, 2]);
    let both = BTreeSet::from([vec![1, 1], vec![1, 2], vec![2, 1], vec![2, 2]]);
    assert_eq!(whole(&writes), both);
}

/// The free bytes after an open that makes a 64 KiB file is polled once, its future
/// drops before or after the call ends, and the remove of the file is durable.
fn free_after_dropped_open(ended: bool) -> u64 {
    run(0, MIB, move |node, _| async move {
        let (files, clock) = (node.files(), node.clock());
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
        files.sync_dir(Path::new("")).await.unwrap();
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
        drop(node.files().free().await);
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
        let opened = node.files().open(Path::new("a/"), Mode::Read).await;
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

/// The first 200 bytes of a file after writes of 1 over bytes 0 to 100, 2 over 100 to
/// 200, and 3 over both were in flight at once.
fn three_writes_in_flight(value: u64) -> Vec<u8> {
    run(value, MIB, |node, tasks| async move {
        let file = Rc::new(create(&node, "a", 512).await);
        let pool = pool();
        for (offset, len, byte) in [(0, 100, 1), (100, 100, 2)] {
            let (writer, parts) = (Rc::clone(&file), [block(&pool, &vec![byte; len])]);
            tasks.spawn(async move { writer.write_at(offset, &parts).await.unwrap() });
        }
        file.write_at(0, &[block(&pool, &[3; 200])]).await.unwrap();
        node.clock().sleep(Span::MILLISECOND).await;
        read(&file, &pool, 0, 200).await
    })
}

/// The values of each run of `runs` whose bytes 0 to 100 hold one value and bytes 100
/// to 200 hold one value.
fn halves(runs: &[Vec<u8>]) -> BTreeSet<(u8, u8)> {
    (runs.iter())
        .filter(|bytes| bytes[..100].iter().all(|&byte| byte == bytes[0]))
        .filter(|bytes| bytes[100..].iter().all(|&byte| byte == bytes[100]))
        .map(|bytes| (bytes[0], bytes[100]))
        .collect()
}

#[test]
fn three_writes_in_flight_leave_the_result_of_each_order() {
    let results: Vec<Vec<u8>> = (0..128).map(three_writes_in_flight).collect();
    assert_bytes_in(results.iter().map(|bytes| &bytes[..100]), &[1, 3]);
    assert_bytes_in(results.iter().map(|bytes| &bytes[100..]), &[2, 3]);
    let orders = BTreeSet::from([(1, 2), (1, 3), (3, 2), (3, 3)]);
    assert_eq!(halves(&results), orders);
}

#[test]
fn a_path_with_a_trailing_slash_names_only_a_directory() {
    let results = run(0, MIB, |node, _| async move {
        let files = node.files();
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
            ("b/.", Mode::Read),
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
        Err(Error::NotFound { path: "b/.".into() }),
        Err(io("d/", Operation::Open, 21)),
        Err(io("a/", Operation::Remove, 20)),
        Ok(()),
        Err(io("d/", Operation::Remove, 21)),
        Ok(()),
    ];
    assert_eq!(results, expected);
}

#[test]
fn a_path_of_the_data_directory_names_no_file() {
    let results = run(0, MIB, |node, _| async move {
        let files = node.files();
        let mut results = Vec::new();
        for (path, mode) in [
            ("./", Mode::Read),
            ("./", Mode::Write),
            (".", Mode::Read),
            (".", Mode::Create { len: 1 }),
        ] {
            results.push(files.open(Path::new(path), mode).await.map(drop));
        }
        results.push(files.remove(Path::new(".")).await);
        results
    });
    let expected = [
        Err(io("./", Operation::Open, 21)),
        Err(io("./", Operation::Open, 21)),
        Err(io(".", Operation::Open, 21)),
        Err(io(".", Operation::Open, 21)),
        Err(io(".", Operation::Remove, 21)),
    ];
    assert_eq!(results, expected);
}

/// The first 200 bytes of a file after the writes of `order` (offset, length, and
/// value), each started when the one before it ended or, when `spawned`, at once with
/// it.
fn writes_in_order(value: u64, order: [(u64, usize, u8, bool); 3]) -> Vec<u8> {
    run(value, MIB, move |node, tasks| async move {
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
    })
}

#[test]
fn three_writes_in_flight_over_nested_bytes_leave_each_order_or_a_mix() {
    let writes = [(0, 200, 1, true), (0, 100, 2, true), (0, 200, 3, false)];
    let results: Vec<Vec<u8>> = (0..256)
        .map(|value| writes_in_order(value, writes))
        .collect();
    assert_bytes_in(results.iter().map(|bytes| &bytes[..100]), &[1, 2, 3]);
    assert_bytes_in(results.iter().map(|bytes| &bytes[100..]), &[1, 3]);
    let orders = BTreeSet::from([(1, 1), (2, 1), (2, 3), (3, 3)]);
    assert!(
        halves(&results).is_superset(&orders),
        "{:?}",
        halves(&results)
    );
}

#[test]
fn a_write_in_flight_over_two_in_turn_keeps_their_order() {
    let writes = [(0, 200, 1, true), (0, 100, 2, false), (0, 100, 3, false)];
    let results: Vec<Vec<u8>> = (0..256)
        .map(|value| writes_in_order(value, writes))
        .collect();
    assert_bytes_in(results.iter().map(|bytes| &bytes[..100]), &[1, 3]);
    assert_bytes_in(results.iter().map(|bytes| &bytes[100..]), &[1]);
    assert_eq!(halves(&results), BTreeSet::from([(1, 1), (3, 1)]));
    assert!(results.iter().any(|bytes| split(&bytes[..100])), "no mix");
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

fn busy(path: &str) -> Error {
    Error::Busy { path: path.into() }
}

#[test]
fn a_write_open_of_a_file_that_a_write_handle_holds_is_busy() {
    let creates = [Mode::Create { len: 1_024 }, Mode::Create { len: 512 }];
    let opens = run(0, MIB, move |node, _| async move {
        let (files, path) = (node.files(), Path::new("a"));
        drop(create(&node, "a", 1_024).await);
        let mut opens = Vec::new();
        for first in [Mode::Write, Mode::Create { len: 1_024 }] {
            let _held = files.open(path, first).await.unwrap();
            for second in [Mode::Write, creates[0], creates[1]] {
                opens.push(files.open(path, second).await.err());
            }
        }
        opens
    });
    assert_eq!(opens, vec![Some(busy("a")); 6]);
}

#[test]
fn a_read_handle_neither_takes_nor_checks_the_write_hold() {
    run(0, MIB, |node, _| async move {
        let (files, path) = (node.files(), Path::new("a"));
        let written = create(&node, "a", 1_024).await;
        files.open(path, Mode::Read).await.unwrap();
        drop(written);
        let _read = files.open(path, Mode::Read).await.unwrap();
        files.open(path, Mode::Write).await.unwrap();
    });
}

/// What a write open of a file gives after its write handle dropped with a write in
/// flight, and what a second write open gives while the first holds the file.
fn open_after_dropped_write(value: u64) -> (Option<Error>, Option<Error>) {
    run(value, MIB, |node, _| async move {
        let (files, path, pool) = (node.files(), Path::new("a"), pool());
        let file = create(&node, "a", 1_024).await;
        let parts = [block(&pool, &[1; 1_024])];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        drop(file);
        let first = files.open(path, Mode::Write).await;
        let second = files.open(path, Mode::Write).await.err();
        (first.err(), second)
    })
}

#[test]
fn a_write_open_waits_for_the_write_of_a_dropped_handle() {
    for value in 0..32 {
        let opens = open_after_dropped_write(value);
        assert_eq!(opens, (None, Some(busy("a"))), "value {value}");
    }
}

/// What a create of `a` gives after a drop of a write handle with a write in flight,
/// whose file a durable `Files::remove` of `a` unlinked first. The write holds the
/// 600 KiB of the old file until it ends.
fn create_after_dropped_handle_of_removed_file(value: u64) -> Result<(), Error> {
    run(value, MIB, |node, _| async move {
        let (files, path, pool) = (node.files(), Path::new("a"), pool());
        let file = create(&node, "a", 600 * KIB).await;
        files.sync_dir(Path::new("")).await.unwrap();
        files.remove(path).await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
        let parts = [block(&pool, &[1; 512])];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        drop(file);
        files
            .open(path, Mode::Create { len: 600 * KIB })
            .await
            .map(drop)
    })
}

#[test]
fn a_write_open_waits_for_the_write_of_a_dropped_handle_of_a_removed_file() {
    for value in 0..32 {
        let made = create_after_dropped_handle_of_removed_file(value);
        assert_eq!(made, Ok(()), "value {value}");
    }
}

/// What a create of `a` gives after a drop of a write handle with a write in flight,
/// whose file a durable `Files::remove` of `a` unlinked first, and then a rename of
/// the handle of another file from `c` to `d`. The write holds the 600 KiB of the old
/// file until it ends.
fn create_after_dropped_handle_of_removed_file_and_rename(
    value: u64,
) -> Result<(), Error> {
    run(value, MIB, |node, _| async move {
        let (files, path, pool) = (node.files(), Path::new("a"), pool());
        let mut other = create(&node, "c", KIB).await;
        let file = create(&node, "a", 600 * KIB).await;
        files.sync_dir(Path::new("")).await.unwrap();
        files.remove(path).await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
        let parts = [block(&pool, &[1; 512])];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        drop(file);
        other.rename(Path::new("d")).await.unwrap();
        files
            .open(path, Mode::Create { len: 600 * KIB })
            .await
            .map(drop)
    })
}

#[test]
fn a_rename_moves_only_the_calls_of_its_file() {
    for value in 0..64 {
        let made = create_after_dropped_handle_of_removed_file_and_rename(value);
        assert_eq!(made, Ok(()), "value {value}");
    }
}

/// What a write open of `a` gives after the drop of a handle with two writes in
/// flight.
fn open_after_two_dropped_writes(value: u64) -> Option<Error> {
    run(value, MIB, |node, _| async move {
        let (files, pool) = (node.files(), pool());
        let file = create(&node, "a", 1_024).await;
        let (first, second) = ([block(&pool, &[1; 512])], [block(&pool, &[2; 512])]);
        let mut writes = (
            Box::pin(file.write_at(0, &first)),
            Box::pin(file.write_at(512, &second)),
        );
        pend(writes.0.as_mut()).await;
        pend(writes.1.as_mut()).await;
        drop(writes);
        drop(file);
        files.open(Path::new("a"), Mode::Write).await.err()
    })
}

#[test]
fn a_write_open_waits_for_the_last_dropped_write_of_a_handle() {
    for value in 0..64 {
        assert_eq!(open_after_two_dropped_writes(value), None, "value {value}");
    }
}

/// What a write open of `b` gives after a drop of a write handle that a rename moved
/// from `a` to `b`, with a write dropped in flight before the rename.
fn open_after_rename_with_dropped_write(value: u64) -> Option<Error> {
    run(value, MIB, |node, _| async move {
        let (files, pool) = (node.files(), pool());
        let mut file = create(&node, "a", KIB).await;
        let parts = [block(&pool, &[1; 512])];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        file.rename(Path::new("b")).await.unwrap();
        drop(file);
        files.open(Path::new("b"), Mode::Write).await.err()
    })
}

#[test]
fn a_write_open_waits_for_a_dropped_call_on_the_file_that_its_path_names() {
    // The write outlasts the rename and the open in about 1 run in 30.
    for value in 0..256 {
        let found = open_after_rename_with_dropped_write(value);
        assert_eq!(found, None, "value {value}");
    }
}

#[test]
fn a_new_file_at_a_removed_path_is_free_while_the_old_file_is_open() {
    run(0, MIB, |node, _| async move {
        let old = create(&node, "a", 1_024).await;
        node.files().remove(Path::new("a")).await.unwrap();
        create(&node, "a", 512).await;
        drop(old);
    });
}

/// The error of the one of two creates of a missing file in flight at once that
/// failed, or `None` when both or neither failed.
fn racing_creates(value: u64) -> Option<Error> {
    run(value, MIB, |node, tasks| async move {
        let files = node.files();
        let other = Rc::new(Cell::new(None));
        let slot = Rc::clone(&other);
        let (mode, theirs) = (Mode::Create { len: 1_024 }, files.clone());
        tasks.spawn(
            async move { slot.set(Some(theirs.open(Path::new("a"), mode).await)) },
        );
        let mine = files.open(Path::new("a"), mode).await;
        node.clock().sleep(Span::MILLISECOND).await;
        match (mine, other.take().expect("the other create ended")) {
            (Ok(_), Err(e)) | (Err(e), Ok(_)) => Some(e),
            _ => None,
        }
    })
}

#[test]
fn of_two_creates_in_flight_the_second_to_end_is_busy() {
    for value in 0..16 {
        assert_eq!(racing_creates(value), Some(busy("a")), "value {value}");
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

/// Polls `future` once, and checks that it is pending.
pub(super) async fn pend(mut future: Pin<&mut impl Future>) {
    poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

/// What a write open of a file gives after a close of its write handle, whose sync
/// dropped in flight.
fn reopen_after_dropped_sync(value: u64) -> Result<(), Error> {
    run(value, MIB, |node, _| async move {
        let (files, path) = (node.files(), Path::new("a"));
        let file = create(&node, "a", 1_024).await;
        let mut sync = Box::pin(file.sync());
        pend(sync.as_mut()).await;
        drop(sync);
        let poisoned = Err(Error::Poisoned { path: path.into() });
        assert_eq!(file.sync().await, poisoned);
        file.close().await;
        files.open(path, Mode::Write).await.map(drop)
    })
}

#[test]
fn a_poisoned_file_reopens_at_once_after_a_close() {
    for value in 0..32 {
        assert_eq!(reopen_after_dropped_sync(value), Ok(()), "value {value}");
    }
}

/// The times before and after a write of a file, which is awaited, or dropped in
/// flight and then the file closed.
fn write_span(value: u64, dropped: bool) -> (Monotonic, Monotonic) {
    run(value, MIB, move |node, _| async move {
        let (file, pool, clock) =
            (create(&node, "a", 1_024).await, pool(), node.clock());
        let parts = [block(&pool, &[1; 1_024])];
        let start = clock.now();
        if dropped {
            let mut write = Box::pin(file.write_at(0, &parts));
            pend(write.as_mut()).await;
            drop(write);
            file.close().await;
        } else {
            file.write_at(0, &parts).await.unwrap();
        }
        (start, clock.now())
    })
}

#[test]
fn a_close_ends_when_a_dropped_write_ends() {
    for value in 0..8 {
        let (start, end) = write_span(value, true);
        assert!(start < end, "value {value}");
        assert_eq!((start, end), write_span(value, false), "value {value}");
    }
}

#[test]
fn a_close_with_no_call_in_flight_takes_no_time() {
    let (start, end) = run(0, MIB, |node, _| async move {
        let file = create(&node, "a", 1_024).await;
        let start = node.clock().now();
        file.close().await;
        (start, node.clock().now())
    });
    assert_eq!(start, end);
}

#[test]
fn a_close_waits_only_for_the_calls_of_its_handle() {
    let (start, end) = run(0, MIB, |node, _| async move {
        let (files, path, pool) = (node.files(), Path::new("a"), pool());
        drop(create(&node, "a", 1_024).await);
        let other = files.open(path, Mode::Read).await.unwrap();
        let mine = files.open(path, Mode::Read).await.unwrap();
        let mut read = Box::pin(other.read_at(0, pool.alloc(1_024).unwrap()));
        pend(read.as_mut()).await;
        let start = node.clock().now();
        mine.close().await;
        (start, node.clock().now())
    });
    assert_eq!(start, end);
}

/// What a write open of a file gives after a drop of a close that waits for a write
/// in flight, and then after a millisecond.
fn open_after_dropped_close(value: u64) -> (Option<Error>, Option<Error>) {
    run(value, MIB, |node, _| async move {
        let (files, path, pool) = (node.files(), Path::new("a"), pool());
        let file = create(&node, "a", 1_024).await;
        let parts = [block(&pool, &[1; 1_024])];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        let mut close = Box::pin(file.close());
        pend(close.as_mut()).await;
        drop(close);
        let first = files.open(path, Mode::Write).await.err();
        node.clock().sleep(Span::MILLISECOND).await;
        (first, files.open(path, Mode::Write).await.err())
    })
}

#[test]
fn a_write_open_waits_for_the_calls_of_a_dropped_close() {
    for value in 0..32 {
        let opens = open_after_dropped_close(value);
        assert_eq!(opens, (None, None), "value {value}");
    }
}

/// A waker that counts its wakes.
struct Wakes(AtomicUsize);

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn a_dropped_close_is_not_woken_when_its_calls_end() {
    for value in 0..8 {
        let woken = run(value, MIB, |node, _| async move {
            let pool = pool();
            let file = create(&node, "a", 1_024).await;
            let parts = [block(&pool, &[1; 1_024])];
            let mut write = Box::pin(file.write_at(0, &parts));
            pend(write.as_mut()).await;
            drop(write);
            let count = Arc::new(Wakes(AtomicUsize::new(0)));
            let waker = Waker::from(Arc::clone(&count));
            let mut close = Box::pin(file.close());
            let cx = &mut Context::from_waker(&waker);
            assert!(close.as_mut().poll(cx).is_pending());
            drop(close);
            node.clock().sleep(Span::MILLISECOND).await;
            count.0.load(Ordering::Relaxed)
        });
        assert_eq!(woken, 0, "value {value}");
    }
}

#[test]
fn a_rename_moves_the_file_and_the_handle_follows_it() {
    run(0, MIB, |node, _| async move {
        let (files, pool) = (node.files(), pool());
        files.create_dir(Path::new("d")).await.unwrap();
        let mut file = create(&node, "d/a", KIB).await;
        file.write_at(0, &[block(&pool, &[1; 512])]).await.unwrap();
        file.rename(Path::new("d/b")).await.unwrap();
        let found = files.open(Path::new("d/a"), Mode::Read).await.err();
        assert_eq!(found, Some(Error::NotFound { path: "d/a".into() }));
        file.write_at(512, &[block(&pool, &[2; 512])])
            .await
            .unwrap();
        let moved = files.open(Path::new("d/b"), Mode::Read).await.unwrap();
        assert_eq!(sectors(&read(&moved, &pool, 0, 1_024).await), [1, 2]);
        assert_eq!(files.list(Path::new("d")).await.unwrap(), [Path::new("b")]);
    });
}

#[test]
fn a_rename_to_a_taken_name_spelled_with_a_dot_gives_exists() {
    let (found, names) = run(0, MIB, |node, _| async move {
        drop(create(&node, "b", KIB).await);
        let mut file = create(&node, "a", KIB).await;
        let found = file.rename(Path::new("./b")).await;
        (found, node.files().list(Path::new("")).await.unwrap())
    });
    assert_eq!(found, Err(Error::Exists { path: "./b".into() }));
    assert_eq!(names, [PathBuf::from("a"), PathBuf::from("b")]);
}

#[test]
fn a_rename_onto_a_file_that_is_there_gives_exists_and_changes_nothing() {
    run(0, MIB, |node, _| async move {
        let (files, pool) = (node.files(), pool());
        let mut file = create(&node, "a", KIB).await;
        file.write_at(0, &[block(&pool, &[1; 512])]).await.unwrap();
        let other = create(&node, "b", KIB).await;
        other.write_at(0, &[block(&pool, &[2; 512])]).await.unwrap();
        other.close().await;
        let found = file.rename(Path::new("b")).await;
        assert_eq!(found, Err(Error::Exists { path: "b".into() }));
        for (path, value) in [("a", 1), ("b", 2)] {
            let kept = files.open(Path::new(path), Mode::Read).await.unwrap();
            assert_eq!(read(&kept, &pool, 0, 512).await, [value; 512]);
        }
        file.rename(Path::new("c")).await.unwrap();
        let moved = files.open(Path::new("c"), Mode::Read).await.unwrap();
        assert_eq!(read(&moved, &pool, 0, 512).await, [1; 512]);
    });
}

#[test]
fn a_rename_of_a_removed_path_gives_not_found_and_changes_nothing() {
    run(0, MIB, |node, _| async move {
        let files = node.files();
        let mut file = create(&node, "a", KIB).await;
        files.remove(Path::new("a")).await.unwrap();
        let found = file.rename(Path::new("b")).await;
        assert_eq!(found, Err(Error::NotFound { path: "a".into() }));
        assert!(files.list(Path::new("")).await.unwrap().is_empty());
    });
}

#[test]
fn a_rename_of_a_path_that_names_another_file_gives_not_found() {
    run(0, MIB, |node, _| async move {
        let (files, pool) = (node.files(), pool());
        let mut file = create(&node, "a", KIB).await;
        files.remove(Path::new("a")).await.unwrap();
        let other = create(&node, "a", KIB).await;
        other.write_at(0, &[block(&pool, &[3; 512])]).await.unwrap();
        let found = file.rename(Path::new("b")).await;
        assert_eq!(found, Err(Error::NotFound { path: "a".into() }));
        assert_eq!(files.list(Path::new("")).await.unwrap(), [Path::new("a")]);
        let kept = files.open(Path::new("a"), Mode::Read).await.unwrap();
        assert_eq!(read(&kept, &pool, 0, 512).await, [3; 512]);
    });
}

#[test]
fn a_write_open_of_the_new_name_is_busy_until_the_handle_closes() {
    run(0, MIB, |node, _| async move {
        let files = node.files();
        let mut file = create(&node, "a", KIB).await;
        file.rename(Path::new("b")).await.unwrap();
        let found = files.open(Path::new("b"), Mode::Write).await.err();
        assert_eq!(found, Some(busy("b")));
        file.close().await;
        files.open(Path::new("b"), Mode::Write).await.unwrap();
    });
}

#[test]
fn a_fault_on_a_rename_fails_it_and_a_fault_on_the_new_name_fails_the_next_write() {
    run(0, MIB, |node, _| async move {
        let (files, pool) = (node.files(), pool());
        let mut file = create(&node, "a", KIB).await;
        node.fail_file(Path::new("a"), Operation::Rename);
        let found = file.rename(Path::new("b")).await;
        assert_eq!(found, Err(io("a", Operation::Rename, 5)));
        assert_eq!(files.list(Path::new("")).await.unwrap(), [Path::new("a")]);
        file.rename(Path::new("b")).await.unwrap();
        node.fail_file(Path::new("b"), Operation::WriteAt);
        let found = file.write_at(0, &[block(&pool, &[1; 512])]).await;
        assert_eq!(found, Err(io("b", Operation::WriteAt, 5)));
    });
}

/// The names in the data directory after a rename of `a` to `b`, polled past its
/// sync and then dropped in flight, ends, when `a` was removed and made again first
/// or not.
fn dropped_rename(value: u64, remade: bool) -> Vec<PathBuf> {
    run(value, MIB, move |node, _| async move {
        let files = node.files();
        let mut file = create(&node, "a", KIB).await;
        if remade {
            files.remove(Path::new("a")).await.unwrap();
            Box::leak(Box::new(create(&node, "a", KIB).await));
        }
        let mut rename = Box::pin(file.rename(Path::new("b")));
        pend(rename.as_mut()).await;
        node.clock().sleep(Span::from_nanos(200_000)).await;
        pend(rename.as_mut()).await;
        drop(rename);
        node.clock().sleep(Span::MILLISECOND).await;
        files.list(Path::new("")).await.unwrap()
    })
}

#[test]
fn a_close_waits_for_a_dropped_rename() {
    for value in 0..8 {
        let names = run(value, MIB, move |node, _| async move {
            let files = node.files();
            let mut file = create(&node, "a", KIB).await;
            let mut rename = Box::pin(file.rename(Path::new("b")));
            pend(rename.as_mut()).await;
            node.clock().sleep(Span::from_nanos(200_000)).await;
            pend(rename.as_mut()).await;
            drop(rename);
            file.close().await;
            files.list(Path::new("")).await.unwrap()
        });
        assert_eq!(names, [Path::new("b")], "value {value}");
    }
}

#[test]
fn a_dropped_rename_still_ends() {
    for value in 0..8 {
        assert_eq!(
            dropped_rename(value, false),
            [Path::new("b")],
            "value {value}"
        );
        assert_eq!(
            dropped_rename(value, true),
            [Path::new("a")],
            "value {value}"
        );
    }
}

/// What a rename of `a` to `b` gives, and the names in the data directory a
/// millisecond later, when a remove of `b` is polled once and its future drops first.
/// `b` is there before the remove when `existing`.
fn rename_after_dropped_remove(
    value: u64,
    existing: bool,
) -> (Result<(), Error>, Vec<PathBuf>) {
    run(value, MIB, move |node, _| async move {
        let files = node.files();
        if existing {
            drop(create(&node, "b", KIB).await);
        }
        let mut file = create(&node, "a", KIB).await;
        let mut remove = Box::pin(files.remove(Path::new("b")));
        pend(remove.as_mut()).await;
        drop(remove);
        let renamed = file.rename(Path::new("b")).await;
        node.clock().sleep(Span::MILLISECOND).await;
        (renamed, files.list(Path::new("")).await.unwrap())
    })
}

#[test]
fn a_rename_waits_for_a_dropped_remove_of_its_new_name() {
    for (value, existing) in (0..32).flat_map(|value| [(value, false), (value, true)]) {
        let found = rename_after_dropped_remove(value, existing);
        let names = vec![PathBuf::from("b")];
        assert_eq!(found, (Ok(()), names), "value {value}, existing {existing}");
    }
}

/// What a write open of `b` gives after a drop of a write handle whose rename of `a`
/// to `b` dropped in flight, and the names in the data directory then.
fn open_after_dropped_rename(value: u64) -> (Option<Error>, Vec<PathBuf>) {
    run(value, MIB, |node, _| async move {
        let files = node.files();
        let mut file = create(&node, "a", KIB).await;
        let mut rename = Box::pin(file.rename(Path::new("b")));
        pend(rename.as_mut()).await;
        node.clock().sleep(Span::from_nanos(200_000)).await;
        pend(rename.as_mut()).await;
        drop(rename);
        drop(file);
        let found = files.open(Path::new("b"), Mode::Write).await.err();
        (found, files.list(Path::new("")).await.unwrap())
    })
}

#[test]
fn a_write_open_waits_for_a_dropped_rename_to_its_path() {
    for value in 0..32 {
        let found = open_after_dropped_rename(value);
        assert_eq!(found, (None, vec![PathBuf::from("b")]), "value {value}");
    }
}

/// Whether a create of `b` ended before a call on `a` whose future dropped in flight:
/// a write when not `renamed`, else a rename of `a` to `c`.
fn create_before_dropped_call(value: u64, renamed: bool) -> bool {
    run(value, MIB, move |node, _| async move {
        let (files, pool) = (node.files(), pool());
        let mut file = create(&node, "a", KIB).await;
        if renamed {
            let mut rename = Box::pin(file.rename(Path::new("c")));
            pend(rename.as_mut()).await;
            node.clock().sleep(Span::from_nanos(200_000)).await;
            pend(rename.as_mut()).await;
        } else {
            let parts = [block(&pool, &[1; 512])];
            let mut write = Box::pin(file.write_at(0, &parts));
            pend(write.as_mut()).await;
        }
        drop(create(&node, "b", KIB).await);
        if renamed {
            return files
                .list(Path::new(""))
                .await
                .unwrap()
                .contains(&"a".into());
        }
        let reader = files.open(Path::new("a"), Mode::Read).await.unwrap();
        read(&reader, &pool, 0, 512).await != [1; 512]
    })
}

#[test]
fn a_create_does_not_wait_for_a_dropped_call_on_another_path() {
    for renamed in [false, true] {
        let runs: Vec<_> = (0..32)
            .map(|value| create_before_dropped_call(value, renamed))
            .collect();
        assert!(runs.contains(&true), "renamed {renamed}: {runs:?}");
    }
}

/// Whether a create of `a` on node 1, one nanosecond after node 0 drops a remove of
/// its own `a`, ends before a read open on node 0 that still finds that file.
fn create_beside_remove_of_another_node(value: u64) -> bool {
    let mut sim = sim(value);
    let config = || node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    };
    let (a, b) = (sim.node(config()), sim.node(config()));
    let at = a.clock().now() + Span::MILLISECOND;
    let made = Arc::new(AtomicU64::new(0));
    let end = Arc::clone(&made);
    let create_b = b.shards().start(shard("b"), move |_| async move {
        b.clock().sleep_until(at + Span::from_nanos(1)).await;
        drop(create(&b, "a", KIB).await);
        end.store(b.clock().now().0, Ordering::Relaxed);
    });
    let (found, read) = sim
        .run_on(&a, move |a, _| async move {
            let files = a.files();
            drop(create(&a, "a", KIB).await);
            a.clock().sleep_until(at).await;
            let mut remove = Box::pin(files.remove(Path::new("a")));
            pend(remove.as_mut()).await;
            drop(remove);
            let found = files.open(Path::new("a"), Mode::Read).await.is_ok();
            (found, a.clock().now().0)
        })
        .unwrap();
    create_b.unwrap().join().unwrap();
    found && made.load(Ordering::Relaxed) < read
}

#[test]
fn a_call_does_not_wait_for_a_dropped_call_of_another_node() {
    assert!((0..32).any(create_beside_remove_of_another_node));
}

/// What a create of `b` gives while a rename of `a` to `b` through a live handle is
/// in flight, after the sync that the rename makes first.
fn create_beside_live_rename(value: u64) -> Option<Error> {
    run(value, MIB, |node, _| async move {
        let files = node.files();
        let mut file = create(&node, "a", KIB).await;
        let mut rename = Box::pin(file.rename(Path::new("b")));
        pend(rename.as_mut()).await;
        node.clock().sleep(Span::from_nanos(200_000)).await;
        pend(rename.as_mut()).await;
        let found = files.open(Path::new("b"), Mode::Create { len: KIB }).await;
        let found = found.map(drop).err();
        drop(rename.await);
        found
    })
}

#[test]
fn a_create_does_not_wait_for_a_live_rename_to_its_path() {
    let found: Vec<_> = (0..32).map(create_beside_live_rename).collect();
    assert!(found.contains(&None), "{found:?}");
}

/// Whether the block of a dropped write of a handle is still in use when a later
/// write of the handle ends.
fn block_held_after_write(value: u64) -> bool {
    run(value, MIB, move |node, _| async move {
        let pool = pool();
        let big = pool.largest();
        let file = create(&node, "a", big as u64).await;
        let parts = [pool.alloc(big).unwrap().freeze()];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        drop(parts);
        pool.alloc(big).unwrap_err();
        let small = [block(&pool, &[1; 512])];
        file.write_at(0, &small).await.unwrap();
        drop(small);
        pool.alloc(big).is_err()
    })
}

#[test]
fn a_write_does_not_wait_for_a_dropped_write_on_its_path() {
    assert!((0..32).any(block_held_after_write));
}

/// Whether the block of a dropped write of a handle is still in use when a read of the
/// handle ends.
fn block_held_after_read(value: u64) -> bool {
    run(value, MIB, move |node, _| async move {
        let pool = pool();
        let big = pool.largest();
        let file = create(&node, "a", big as u64).await;
        let parts = [pool.alloc(big).unwrap().freeze()];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        drop(parts);
        pool.alloc(big).unwrap_err();
        drop(read(&file, &pool, 0, 512).await);
        pool.alloc(big).is_err()
    })
}

#[test]
fn a_read_does_not_wait_for_a_dropped_write_on_its_path() {
    assert!((0..32).any(block_held_after_read));
}

/// What a sync and a list of `d` give after a create of `d` dropped in flight.
fn sync_and_list_after_dropped_create_dir(
    value: u64,
) -> (Option<Error>, Option<Error>) {
    run(value, MIB, |node, _| async move {
        let (files, dir) = (node.files(), Path::new("d"));
        let mut create = Box::pin(files.create_dir(dir));
        pend(create.as_mut()).await;
        drop(create);
        let synced = files.sync_dir(dir).await.err();
        (synced, files.list(dir).await.err())
    })
}

#[test]
fn a_sync_and_a_list_of_a_directory_do_not_wait_for_its_dropped_create() {
    let found: Vec<_> = (0..32)
        .map(sync_and_list_after_dropped_create_dir)
        .collect();
    let missing = Some(Error::NotFound { path: "d".into() });
    assert!(
        found.iter().any(|(synced, _)| *synced == missing),
        "{found:?}"
    );
    assert!(
        found.iter().any(|(_, listed)| *listed == missing),
        "{found:?}"
    );
}

/// Whether the block of a dropped write of a handle, sent on `a`, is still in use when
/// a rename of the handle from `a` to `b` ends.
fn block_held_after_rename(value: u64) -> bool {
    run(value, MIB, move |node, _| async move {
        let pool = pool();
        let big = pool.largest();
        let mut file = create(&node, "a", big as u64).await;
        let parts = [pool.alloc(big).unwrap().freeze()];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        drop(parts);
        pool.alloc(big).unwrap_err();
        file.rename(Path::new("b")).await.unwrap();
        pool.alloc(big).is_err()
    })
}

#[test]
fn a_rename_does_not_wait_for_the_dropped_write_of_its_handle_on_another_path() {
    assert!((0..32).any(block_held_after_rename));
}

/// Whether a remove through a handle of `a` is still in flight when a create of `b`
/// ends.
fn create_beside_handle_remove_of_another_path(value: u64) -> bool {
    run(value, MIB, |node, _| async move {
        let files = node.files();
        let file = create(&node, "a", KIB).await;
        let mut remove = Box::pin(file.remove());
        pend(remove.as_mut()).await;
        files
            .open(Path::new("b"), Mode::Create { len: KIB })
            .await
            .unwrap();
        poll_fn(|cx| Poll::Ready(remove.as_mut().poll(cx).is_pending())).await
    })
}

#[test]
fn a_create_does_not_wait_for_a_remove_through_a_handle_of_another_path() {
    assert!((0..32).any(create_beside_handle_remove_of_another_path));
}

/// Whether `a` is still there when a create of `b` ends, which starts once a remove
/// of `a` is polled once and its future drops.
fn create_beside_dropped_remove(value: u64) -> bool {
    run(value, MIB, |node, _| async move {
        let files = node.files();
        drop(create(&node, "a", KIB).await);
        let mut remove = Box::pin(files.remove(Path::new("a")));
        pend(remove.as_mut()).await;
        drop(remove);
        drop(create(&node, "b", KIB).await);
        files
            .list(Path::new(""))
            .await
            .unwrap()
            .contains(&"a".into())
    })
}

#[test]
fn a_create_does_not_wait_for_a_dropped_remove_of_another_path() {
    let runs: Vec<_> = (0..32).map(create_beside_dropped_remove).collect();
    assert!(runs.contains(&true), "{runs:?}");
}

/// What a write open of `a` gives while a `File::remove` of it, whose future lives,
/// is in flight.
fn open_beside_handle_remove(value: u64) -> Option<Error> {
    run(value, MIB, |node, tasks| async move {
        let file = create(&node, "a", KIB).await;
        tasks.spawn(async move { file.remove().await.unwrap() });
        node.clock().sleep(Span::from_nanos(1)).await;
        let found = node.files().open(Path::new("a"), Mode::Write).await;
        found.map(drop).err()
    })
}

#[test]
fn a_write_open_waits_for_a_remove_through_the_handle_whose_future_lives() {
    for value in 0..32 {
        let gone = Some(Error::NotFound { path: "a".into() });
        assert_eq!(open_beside_handle_remove(value), gone, "value {value}");
    }
}

#[test]
fn a_remove_through_the_handle_removes_the_file_and_closes_it() {
    run(0, MIB, |node, _| async move {
        let (files, pool, path) = (node.files(), pool(), Path::new("a"));
        let file = create(&node, "a", 64 * KIB).await;
        file.write_at(0, &[block(&pool, &[1; 512])]).await.unwrap();
        assert_eq!(file.remove().await, Ok(()));
        assert!(files.list(Path::new("")).await.unwrap().is_empty());
        let found = files.open(path, Mode::Write).await.err();
        assert_eq!(found, Some(Error::NotFound { path: "a".into() }));
        files.sync_dir(Path::new("")).await.unwrap();
        assert_eq!(files.free().await.unwrap(), MIB, "the handle closed");
        let made = create(&node, "a", KIB).await;
        assert_eq!(read(&made, &pool, 0, 512).await, [0; 512]);
    });
}

/// What a write open of `a` gives while a `File::remove` of it, whose future lives
/// and which a fault fails, is in flight. The open ends before the remove is polled
/// again.
fn open_beside_failed_handle_remove(value: u64) -> Option<Error> {
    run(value, MIB, |node, _| async move {
        let (files, file) = (node.files(), create(&node, "a", KIB).await);
        node.fail_file(Path::new("a"), Operation::Remove);
        let mut remove = Box::pin(file.remove());
        pend(remove.as_mut()).await;
        let found = files.open(Path::new("a"), Mode::Write).await;
        assert_eq!(remove.await, Err(io("a", Operation::Remove, 5)));
        found.map(drop).err()
    })
}

#[test]
fn a_write_open_waits_for_the_close_of_a_failed_remove_through_the_handle() {
    for value in 0..32 {
        assert_eq!(
            open_beside_failed_handle_remove(value),
            None,
            "value {value}"
        );
    }
}

/// What a write open of `a` gives when its node pauses while the open and a
/// `File::remove` of `a`, which a fault fails, are in flight.
fn open_beside_failed_handle_remove_in_pause(value: u64) -> Option<Error> {
    run(value, MIB, |node, tasks| async move {
        let file = create(&node, "a", KIB).await;
        node.fail_file(Path::new("a"), Operation::Remove);
        tasks.spawn(async move {
            assert_eq!(file.remove().await, Err(io("a", Operation::Remove, 5)));
        });
        node.clock().sleep(Span::from_nanos(1)).await;
        let files = node.files();
        let mut open = Box::pin(files.open(Path::new("a"), Mode::Write));
        pend(open.as_mut()).await;
        node.pause(Span::MILLISECOND);
        open.await.map(drop).err()
    })
}

#[test]
fn a_write_open_waits_for_the_close_of_a_failed_remove_through_the_handle_in_a_pause() {
    for value in 0..32 {
        assert_eq!(
            open_beside_failed_handle_remove_in_pause(value),
            None,
            "value {value}"
        );
    }
}

#[test]
fn a_write_open_waits_for_a_dropped_remove_at_the_end_of_true_time() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        monotonic: Monotonic(0),
        wall: types::time::Stamp::from_nanos(i64::MIN),
        disk_bytes: MIB,
        ..node::Config::default()
    });
    let found = sim
        .run_on(&node, |node, _| async move {
            let files = node.files();
            drop(create(&node, "a", KIB).await);
            node.clock().sleep_until(Monotonic(u64::MAX - 5)).await;
            let mut remove = Box::pin(files.remove(Path::new("a")));
            pend(remove.as_mut()).await;
            drop(remove);
            files.open(Path::new("a"), Mode::Write).await.map(drop)
        })
        .unwrap();
    assert_eq!(found, Err(Error::NotFound { path: "a".into() }));
}

/// What a write open of `a` gives after a `File::remove` of it is polled once and
/// dropped, then what a create gives, and the names in the data directory a
/// millisecond after the create.
fn opens_after_dropped_remove(
    value: u64,
) -> (Option<Error>, Option<Error>, Vec<PathBuf>) {
    run(value, MIB, |node, _| async move {
        let (files, path) = (node.files(), Path::new("a"));
        let file = create(&node, "a", KIB).await;
        let mut remove = Box::pin(file.remove());
        pend(remove.as_mut()).await;
        drop(remove);
        let opened = files.open(path, Mode::Write).await.err();
        let made = files.open(path, Mode::Create { len: KIB }).await.err();
        node.clock().sleep(Span::MILLISECOND).await;
        (opened, made, files.list(Path::new("")).await.unwrap())
    })
}

#[test]
fn a_write_open_waits_for_a_dropped_remove_and_then_a_create_stays() {
    let gone = Some(Error::NotFound { path: "a".into() });
    for value in 0..32 {
        let opens = opens_after_dropped_remove(value);
        assert_eq!(
            opens,
            (gone.clone(), None, vec!["a".into()]),
            "value {value}"
        );
    }
}

#[test]
fn a_remove_of_a_removed_path_gives_not_found() {
    run(0, MIB, |node, _| async move {
        let files = node.files();
        let file = create(&node, "a", KIB).await;
        files.remove(Path::new("a")).await.unwrap();
        let found = file.remove().await;
        assert_eq!(found, Err(Error::NotFound { path: "a".into() }));
        assert!(files.list(Path::new("")).await.unwrap().is_empty());
        files.sync_dir(Path::new("")).await.unwrap();
        assert_eq!(files.free().await.unwrap(), MIB, "the handle closed");
    });
}

#[test]
fn a_remove_of_a_path_that_names_another_file_gives_not_found_and_keeps_it() {
    run(0, MIB, |node, _| async move {
        let (files, pool) = (node.files(), pool());
        let file = create(&node, "a", KIB).await;
        files.remove(Path::new("a")).await.unwrap();
        let other = create(&node, "a", KIB).await;
        other.write_at(0, &[block(&pool, &[3; 512])]).await.unwrap();
        let found = file.remove().await;
        assert_eq!(found, Err(Error::NotFound { path: "a".into() }));
        assert_eq!(files.list(Path::new("")).await.unwrap(), [Path::new("a")]);
        let kept = files.open(Path::new("a"), Mode::Read).await.unwrap();
        assert_eq!(read(&kept, &pool, 0, 512).await, [3; 512]);
    });
}

/// The times before and after a `File::remove` of a file with a write in flight
/// whose future dropped.
fn remove_span(value: u64) -> (Monotonic, Monotonic) {
    run(value, MIB, move |node, _| async move {
        let (file, pool, clock) = (create(&node, "a", KIB).await, pool(), node.clock());
        let parts = [block(&pool, &[1; 1_024])];
        let start = clock.now();
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        file.remove().await.unwrap();
        (start, clock.now())
    })
}

#[test]
fn a_remove_ends_after_a_dropped_write_ends() {
    let mut waited = false;
    for value in 0..32 {
        let (start, end) = remove_span(value);
        let (_, written) = write_span(value, false);
        assert!(start < written && written <= end, "value {value}");
        waited |= written == end;
    }
    assert!(waited, "no remove ended with the write");
}

#[test]
fn a_fault_on_a_remove_fails_it_and_the_file_stays() {
    run(0, MIB, |node, _| async move {
        let (files, path) = (node.files(), Path::new("a"));
        let file = create(&node, "a", KIB).await;
        node.fail_file(path, Operation::Remove);
        assert_eq!(file.remove().await, Err(io("a", Operation::Remove, 5)));
        assert_eq!(files.list(Path::new("")).await.unwrap(), [path]);
        files.open(path, Mode::Write).await.unwrap();
    });
}

#[test]
fn a_remove_through_the_handle_frees_a_durable_file_only_after_sync_dir() {
    let frees = run(0, MIB, |node, _| async move {
        let files = node.files();
        let file = create(&node, "f", 64 * KIB).await;
        files.sync_dir(Path::new("")).await.unwrap();
        file.remove().await.unwrap();
        let removed = files.free().await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
        (removed, files.free().await.unwrap())
    });
    assert_eq!(frees, (MIB - 64 * KIB, MIB));
}

#[test]
fn a_remove_after_a_dropped_rename_gives_poisoned_and_the_file_stays() {
    for value in 0..8 {
        let (removed, names) = run(value, MIB, move |node, _| async move {
            let files = node.files();
            let mut file = create(&node, "a", KIB).await;
            let mut rename = Box::pin(file.rename(Path::new("b")));
            pend(rename.as_mut()).await;
            node.clock().sleep(Span::from_nanos(200_000)).await;
            pend(rename.as_mut()).await;
            drop(rename);
            node.clock().sleep(Span::MILLISECOND).await;
            let removed = file.remove().await;
            (removed, files.list(Path::new("")).await.unwrap())
        });
        let poisoned = Err(Error::Poisoned { path: "a".into() });
        assert_eq!(
            (removed, names),
            (poisoned, vec!["b".into()]),
            "value {value}"
        );
    }
}

#[test]
fn the_node_gives_the_path_of_each_file_that_it_closed_in_order() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    });
    sim.run_on(&node, |node, _| async move {
        let a = create(&node, "a", 0).await;
        let mut b = create(&node, "./b", 0).await;
        b.rename(Path::new("./c")).await.unwrap();
        let d = create(&node, "d", 0).await;
        b.close().await;
        drop(a);
        d.remove().await.unwrap();
        drop(create(&node, "./e", 0).await);
    })
    .unwrap();
    assert_eq!(node.file_closes(), ["c", "a", "d", "e"].map(PathBuf::from));
}

#[test]
fn a_file_removed_while_open_gives_the_path_of_its_open() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    });
    sim.run_on(&node, |node, _| async move {
        let a = create(&node, "a", 0).await;
        node.files().remove(Path::new("a")).await.unwrap();
        drop(a);
    })
    .unwrap();
    assert_eq!(node.file_closes(), [PathBuf::from("a")]);
}

#[test]
fn an_open_whose_future_dropped_closes_no_descriptor() {
    for ended in [false, true] {
        let mut sim = sim(0);
        let node = sim.node(node::Config {
            disk_bytes: MIB,
            ..node::Config::default()
        });
        sim.run_on(&node, move |node, _| async move {
            let (files, clock) = (node.files(), node.clock());
            let mut open =
                Box::pin(files.open(Path::new("a"), Mode::Create { len: 0 }));
            poll_fn(|cx| {
                assert!(open.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            if ended {
                clock.sleep(Span::MILLISECOND).await;
            }
            drop(open);
            clock.sleep(Span::MILLISECOND).await;
        })
        .unwrap();
        assert_eq!(node.file_closes(), Vec::<PathBuf>::new(), "ended: {ended}");
    }
}

#[test]
fn a_dropped_rename_that_ends_gives_the_path_of_the_rename() {
    for value in 0..8 {
        let mut sim = sim(value);
        let node = sim.node(node::Config {
            disk_bytes: MIB,
            ..node::Config::default()
        });
        let names = sim
            .run_on(&node, |node, _| async move {
                let files = node.files();
                let mut file = create(&node, "a", KIB).await;
                let mut rename = Box::pin(file.rename(Path::new("b")));
                pend(rename.as_mut()).await;
                node.clock().sleep(Span::from_nanos(200_000)).await;
                pend(rename.as_mut()).await;
                drop(rename);
                file.close().await;
                files.list(Path::new("")).await.unwrap()
            })
            .unwrap();
        assert_eq!(names, [Path::new("b")], "value {value}");
        assert_eq!(node.file_closes(), [PathBuf::from("b")], "value {value}");
    }
}

#[test]
fn a_failed_rename_keeps_the_path_of_its_open() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    });
    node.fail_file(Path::new("a"), Operation::Rename);
    sim.run_on(&node, |node, _| async move {
        let mut file = create(&node, "a", 0).await;
        let found = file.rename(Path::new("b")).await;
        assert_eq!(found, Err(io("a", Operation::Rename, 5)));
        drop(file);
    })
    .unwrap();
    assert_eq!(node.file_closes(), [PathBuf::from("a")]);
}

#[test]
fn a_rename_gives_its_path_only_to_its_own_descriptor() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    });
    sim.run_on(&node, |node, _| async move {
        let mut writer = create(&node, "a", 0).await;
        let reader = node.files().open(Path::new("a"), Mode::Read).await.unwrap();
        writer.rename(Path::new("b")).await.unwrap();
        drop(writer);
        drop(reader);
    })
    .unwrap();
    assert_eq!(node.file_closes(), ["b", "a"].map(PathBuf::from));
}

#[test]
fn a_rename_of_a_removed_name_keeps_the_path_of_its_open() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    });
    sim.run_on(&node, |node, _| async move {
        let mut file = create(&node, "a", 0).await;
        node.files().remove(Path::new("a")).await.unwrap();
        let found = file.rename(Path::new("b")).await;
        assert_eq!(found, Err(Error::NotFound { path: "a".into() }));
        drop(file);
    })
    .unwrap();
    assert_eq!(node.file_closes(), [PathBuf::from("a")]);
}

#[test]
fn a_rename_onto_a_taken_name_keeps_the_path_of_its_open() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        disk_bytes: MIB,
        ..node::Config::default()
    });
    sim.run_on(&node, |node, _| async move {
        create(&node, "b", 0).await.close().await;
        let mut file = create(&node, "a", 0).await;
        let found = file.rename(Path::new("b")).await;
        assert_eq!(found, Err(Error::Exists { path: "b".into() }));
        drop(file);
    })
    .unwrap();
    assert_eq!(node.file_closes(), ["b", "a"].map(PathBuf::from));
}

#[test]
fn a_fault_on_the_path_of_a_reader_fails_it_after_another_descriptor_renames() {
    run(0, MIB, |node, _| async move {
        let pool = pool();
        let mut writer = create(&node, "a", KIB).await;
        let reader = node.files().open(Path::new("a"), Mode::Read).await.unwrap();
        writer.rename(Path::new("b")).await.unwrap();
        node.fail_file(Path::new("a"), Operation::ReadAt);
        let found = reader.read_at(0, pool.alloc(512).unwrap()).await;
        assert_eq!(found.map(|_| ()), Err(io("a", Operation::ReadAt, 5)));
    });
}

#[test]
fn a_fault_on_the_new_path_misses_a_reader_after_another_descriptor_renames() {
    run(0, MIB, |node, _| async move {
        let pool = pool();
        let mut writer = create(&node, "a", KIB).await;
        let reader = node.files().open(Path::new("a"), Mode::Read).await.unwrap();
        writer.rename(Path::new("b")).await.unwrap();
        node.fail_file(Path::new("b"), Operation::ReadAt);
        let found = reader.read_at(0, pool.alloc(512).unwrap()).await;
        assert_eq!(found.map(|_| ()), Ok(()));
    });
}

#[test]
fn an_error_of_a_descriptor_names_the_path_of_its_open_as_given() {
    run(0, MIB, |node, _| async move {
        let pool = pool();
        let file = create(&node, "./a", KIB).await;
        node.fail_file(Path::new("a"), Operation::ReadAt);
        let found = file.read_at(0, pool.alloc(512).unwrap()).await;
        assert_eq!(found.map(|_| ()), Err(io("./a", Operation::ReadAt, 5)));
    });
}

#[test]
fn an_error_of_a_descriptor_names_the_path_of_its_rename_as_given() {
    run(0, MIB, |node, _| async move {
        let pool = pool();
        let mut file = create(&node, "./a", KIB).await;
        file.rename(Path::new("./c")).await.unwrap();
        node.fail_file(Path::new("c"), Operation::WriteAt);
        let found = file.write_at(0, &[block(&pool, &[1; 512])]).await;
        assert_eq!(found, Err(io("./c", Operation::WriteAt, 5)));
        node.fail_file(Path::new("c"), Operation::Sync);
        assert_eq!(file.sync().await, Err(io("./c", Operation::Sync, 5)));
        let found = file.sync().await;
        assert_eq!(found, Err(Error::Poisoned { path: "./c".into() }));
    });
}

/// How a test ends a handle.
enum End {
    Close,
    Remove,
}

/// Whether the block of a dropped write of a handle is still in use when `end` of the
/// handle returns. A `Files::remove` unlinked the path of the handle before the write.
fn block_held_after_end(value: u64, end: End) -> bool {
    run(value, MIB, move |node, _| async move {
        let (pool, files) = (pool(), node.files());
        let big = pool.largest();
        let file = create(&node, "a", big as u64).await;
        files.remove(Path::new("a")).await.unwrap();
        let parts = [pool.alloc(big).unwrap().freeze()];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        drop(parts);
        pool.alloc(pool.largest()).unwrap_err();
        match end {
            End::Close => file.close().await,
            End::Remove => {
                let found = file.remove().await;
                assert_eq!(found, Err(Error::NotFound { path: "a".into() }));
            }
        }
        pool.alloc(pool.largest()).is_err()
    })
}

#[test]
fn a_close_ends_after_the_dropped_write_of_its_handle() {
    for value in 0..32 {
        assert!(!block_held_after_end(value, End::Close), "value {value}");
    }
}

#[test]
fn a_remove_through_the_handle_ends_after_the_dropped_write_of_its_handle() {
    for value in 0..32 {
        assert!(!block_held_after_end(value, End::Remove), "value {value}");
    }
}

/// What a create of `a` gives, and whether it ended before a `File::remove` of `a`
/// whose future lives. A durable `Files::remove` of `a` unlinked the file of the
/// handle first, so only the handle holds its 600 KiB.
fn create_beside_handle_remove_of_unlinked(value: u64) -> (Result<(), Error>, bool) {
    run(value, MIB, |node, _| async move {
        let (files, path) = (node.files(), Path::new("a"));
        let file = create(&node, "a", 600 * KIB).await;
        files.sync_dir(Path::new("")).await.unwrap();
        files.remove(path).await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
        let mut remove = Box::pin(file.remove());
        pend(remove.as_mut()).await;
        let made = files.open(path, Mode::Create { len: 600 * KIB }).await;
        let ended =
            poll_fn(|cx| Poll::Ready(remove.as_mut().poll(cx).is_ready())).await;
        (made.map(drop), !ended)
    })
}

#[test]
fn a_write_open_waits_for_a_remove_through_a_handle_whose_path_names_another_file() {
    for value in 0..32 {
        let found = create_beside_handle_remove_of_unlinked(value);
        assert_eq!(found, (Ok(()), false), "value {value}");
    }
}

/// Whether the block of a dropped write of a handle of `a` is still in use when a
/// remove through the handle ends. A remove of `b` starts, and a rename of the handle
/// to `b` ends before it, so the remove unlinks the file. `None` when the run takes
/// another order.
fn block_held_after_remove_of_renamed(value: u64) -> Option<bool> {
    run(value, MIB, move |node, _| async move {
        let (pool, files) = (pool(), node.files());
        let big = pool.largest();
        let mut file = create(&node, "a", big as u64).await;
        let parts = [pool.alloc(big).unwrap().freeze()];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        drop(parts);
        let mut remove = Box::pin(files.remove(Path::new("b")));
        pend(remove.as_mut()).await;
        file.rename(Path::new("b")).await.unwrap();
        remove.await.unwrap();
        let found = file.remove().await;
        let held = pool.alloc(big).is_err();
        (found == Err(Error::NotFound { path: "b".into() })).then_some(held)
    })
}

#[test]
fn a_remove_through_a_renamed_handle_ends_after_the_dropped_write_of_its_handle() {
    let runs: Vec<_> = (0..1024).map(block_held_after_remove_of_renamed).collect();
    assert!(runs.iter().flatten().count() > 32, "{runs:?}");
    let held: Vec<_> = (runs.iter().enumerate())
        .filter_map(|(value, run)| (*run == Some(true)).then_some(value))
        .collect();
    assert_eq!(held, Vec::<usize>::new());
}

/// What a create of `b` gives after the drop of a handle of `a` with a dropped write
/// in flight. A remove of `b` starts, a rename of the handle to `b` ends before it, and
/// the remove can unlink the file, which holds 600 KiB of the 1 MiB disk until the
/// write ends.
fn create_after_handle_of_renamed_removed(value: u64) -> Result<(), Error> {
    run(value, MIB, move |node, _| async move {
        let (pool, files) = (pool(), node.files());
        let mut file = create(&node, "a", 600 * KIB).await;
        let parts = [block(&pool, &[1; 512])];
        let mut write = Box::pin(file.write_at(0, &parts));
        pend(write.as_mut()).await;
        drop(write);
        let mut remove = Box::pin(files.remove(Path::new("b")));
        pend(remove.as_mut()).await;
        file.rename(Path::new("b")).await.unwrap();
        remove.await.unwrap();
        files.sync_dir(Path::new("")).await.unwrap();
        drop(file);
        (files.open(Path::new("b"), Mode::Create { len: 600 * KIB }))
            .await
            .map(drop)
    })
}

#[test]
fn a_write_open_waits_for_the_write_of_a_dropped_handle_of_a_renamed_removed_file() {
    let failed: Vec<_> = (0..2048)
        .filter_map(|value| {
            let made = create_after_handle_of_renamed_removed(value);
            made.err().map(|error| (value, error))
        })
        .collect();
    assert_eq!(failed, Vec::new());
}

/// Whether the block of a dropped read of a read handle of `a` is still in use when a
/// create of `a` ends, after the write handle of the file renames it to `b`. The read
/// is on `a`: the rename gives `b` only to the write handle.
fn block_held_after_create_of_reader_path(value: u64) -> bool {
    run(value, MIB, move |node, _| async move {
        let (files, pool) = (node.files(), pool());
        let big = pool.largest();
        let mut writer = create(&node, "a", big as u64).await;
        let reader = files.open(Path::new("a"), Mode::Read).await.unwrap();
        let mut read = Box::pin(reader.read_at(0, pool.alloc(big).unwrap()));
        pend(read.as_mut()).await;
        drop(read);
        drop(reader);
        pool.alloc(big).unwrap_err();
        writer.rename(Path::new("b")).await.unwrap();
        drop(writer);
        drop(create(&node, "a", KIB).await);
        pool.alloc(big).is_err()
    })
}

#[test]
fn a_create_waits_for_a_dropped_read_of_a_reader_on_its_path_after_a_rename() {
    let held: Vec<_> = (0..256)
        .filter(|&value| block_held_after_create_of_reader_path(value))
        .collect();
    assert_eq!(held, Vec::<u64>::new());
}
