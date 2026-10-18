//! Tests of `buffer` through its public surface, on one shard of a simulated node.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod carry;
mod memory;

use std::collections::BTreeSet;
use std::future::poll_fn;
use std::num::{NonZeroU8, NonZeroU32};
use std::ops::Range;
use std::path::{Path as FilePath, PathBuf};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use block::{Block, Heap, Pool};
use buffer::{
    Buffer, Commit, Config, End, Entry, Error, Layout, Limit, Parts, Read, Rejected,
    Samples, Stored, Tail, Unfit,
};
use env::clock::Clock;
use env::entropy::Entropy;
use env::files::{Error as FileError, Files, Mode, Operation, SECTOR};
use env::tasks::Tasks;
use proptest::prelude::*;
use types::channel::{self, Slot, Slots};
use types::frame::Path;
use types::log::Mark;
use types::time::{Span, Stamp};

use crate::memory::Memory;

const BLOCK: u64 = 4096;
/// Sixteen records of one block.
const AREA: u64 = 16 * BLOCK;
/// A body that keeps a record in one block.
const BODY_MAX: usize = 4087;
/// The two header blocks come before the area.
const AREA_START: u64 = 2 * BLOCK;
const COMMIT: Span = Span::from_nanos(10_000_000);
const POOL: u64 = 1 << 21;
const DIR: &str = "shard-0";
const RING: &str = "shard-0/ring";
/// Where a header block keeps its version, its `body_max`, its tail offset, its tail
/// chain, its seq, and its CRC.
const VERSION_AT: usize = 8;
const BODY_MAX_AT: usize = 18;
const TAIL_AT: usize = 22;
const CHAIN_AT: usize = 30;
const SEQ_AT: usize = 34;
const CRC_AT: usize = 42;
/// The bytes the header CRC covers.
const COVER: usize = 512;
/// The kind bytes of a data record and of a restart record. The chain value of the
/// next record is the CRC of a data record and the body of a restart record.
const DATA: u8 = 1;
const RESTART: u8 = 3;
const CARRY: u8 = 4;

/// The disk that the files of a test are on.
enum Disk {
    Sim(sim::node::Node),
    Memory(Memory),
}

/// What one test gets on its shard.
struct Shard {
    disk: Disk,
    /// The files on `disk`.
    files: Files,
    pool: Rc<Pool>,
    clock: Clock,
    tasks: Tasks,
    entropy: Entropy,
}

impl Shard {
    async fn open(&self, layout: Layout, slots: &mut Slots) -> Result<Buffer, Error> {
        let config = Config {
            files: self.files.clone(),
            dir: PathBuf::from(DIR),
            pool: Rc::clone(&self.pool),
            clock: self.clock.clone(),
            tasks: self.tasks.clone(),
            entropy: self.entropy.clone(),
            layout,
            commit: COMMIT,
        };
        Buffer::open(config, slots).await
    }

    /// A block of `len` bytes that count up from `len`.
    fn block(&self, len: usize) -> Block {
        let mut block = self.pool.alloc(len).expect("the pool has a block");
        for (at, byte) in block.iter_mut().enumerate() {
            *byte = u8::try_from((len + at) % 251).expect("under 251");
        }
        block.freeze()
    }

    /// The `Memory` that the test runs on.
    ///
    /// # Panics
    ///
    /// When the test runs on the node's disk.
    fn memory(&self) -> &Memory {
        match &self.disk {
            Disk::Memory(memory) => memory,
            Disk::Sim(_) => panic!("the test runs on the node's disk"),
        }
    }

    /// The node whose disk the test runs on.
    ///
    /// # Panics
    ///
    /// When the test runs on memory.
    fn node(&self) -> &sim::node::Node {
        match &self.disk {
            Disk::Sim(node) => node,
            Disk::Memory(_) => panic!("the test runs on memory"),
        }
    }

    /// Makes the ring file with `len` zero bytes and no header, when no file is there.
    async fn create_zeroed(&self, len: u64) {
        let dir = self.files.create_dir(FilePath::new(DIR)).await;
        dir.expect("the directory is made");
        self.files
            .open(FilePath::new(RING), Mode::Create { len })
            .await
            .expect("the file is made");
    }

    /// Whether a write handle holds the ring: a write open of it is then busy.
    ///
    /// # Panics
    ///
    /// When the test runs on memory, which never gives `Busy`.
    async fn ring_held(&self) -> bool {
        self.node();
        match self.files.open(FilePath::new(RING), Mode::Write).await {
            Ok(_) => false,
            Err(FileError::Busy { .. }) => true,
            Err(error) => panic!("the ring does not open: {error}"),
        }
    }

    /// The bytes of the ring file.
    async fn bytes(&self) -> Vec<u8> {
        let ring = self.files.open(FilePath::new(RING), Mode::Read).await;
        let ring = ring.expect("the ring opens");
        let pool = pool();
        let mut bytes = Vec::new();
        for at in (0..ring.len()).step_by(to_usize(BLOCK)) {
            let len = to_usize(BLOCK.min(ring.len() - at));
            let block = pool.alloc(len).expect("a block");
            bytes.extend_from_slice(&ring.read_at(at, block).await.expect("reads"));
        }
        bytes
    }

    /// Puts `bytes` at `offset` of the ring file, as a crash or a defect would. No
    /// buffer may hold the ring.
    async fn put(&self, offset: usize, bytes: &[u8]) {
        let ring = self.files.open(FilePath::new(RING), Mode::Write).await;
        let ring = ring.expect("the ring opens to write");
        let block = pool().copy(bytes).expect("a block");
        let offset = u64::try_from(offset).expect("fits in u64");
        ring.write_at(offset, &[block]).await.expect("writes");
    }

    /// Puts `bytes` at `at` of both header blocks and fixes their CRCs.
    async fn tamper(&self, at: usize, bytes: &[u8]) {
        for place in [0, to_usize(BLOCK)] {
            self.tamper_block(place, at, bytes).await;
        }
    }

    /// Puts `bytes` at `at` of the header block at `place` and fixes its CRC.
    async fn tamper_block(&self, place: usize, at: usize, bytes: &[u8]) {
        let file = self.bytes().await;
        let mut block = file[place..place + to_usize(BLOCK)].to_vec();
        block[at..at + bytes.len()].copy_from_slice(bytes);
        let crc = crc32c::crc32c(&block[..CRC_AT]);
        let crc = crc32c::crc32c_append(crc, &block[CRC_AT + 4..COVER]);
        block[CRC_AT..CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
        self.put(place, &block).await;
    }

    /// Opens a ring that holds a record at `offset` that this build cannot read. The
    /// open must give `Invalid`, as [`Shard::open_refused`] says.
    async fn open_invalid(&self, layout: Layout, offset: u64) {
        let case = format!("a record at {offset}");
        let error = Error::Invalid { offset };
        self.open_refused(layout, error, &case).await;
    }

    /// Opens a ring that passes its CRCs and that this build cannot read. The open
    /// must give `error`, leave the file as it was, and not sync it: a fault fails
    /// the next sync of the ring, so an open that syncs it gives `Io`. Each failed
    /// assert names `case`.
    async fn open_refused(&self, layout: Layout, error: Error, case: &str) {
        let before = self.bytes().await;
        self.node().fail_file(FilePath::new(RING), Operation::Sync);
        let opened = self.open(layout, &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(error), "{case}");
        let changed = self.bytes().await != before;
        assert!(!changed, "{case}: the open changed the ring");
        let ring = self.files.open(FilePath::new(RING), Mode::Write).await;
        let synced = ring.expect("the ring opens to write").sync().await;
        let fault = FileError::Io {
            path: RING.into(),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(synced, Err(fault), "{case}: the open synced the ring");
    }

    /// Puts `bytes` at `at` of the body of the record at `offset` of the area, and
    /// seals the record.
    async fn tamper_record(&self, offset: u64, at: usize, bytes: &[u8]) {
        let body = to_usize(AREA_START + offset) + 9;
        self.put(body + at, bytes).await;
        self.seal(offset).await;
    }

    /// Fixes the CRC of the record at `offset` of the area, so that it still follows
    /// the record before it, a restart, data, or carry record. The tail offset in
    /// each header block must be 0. The first record follows the tail chain of the
    /// header, so for it the two header blocks must be the same up to the seq.
    async fn seal(&self, offset: u64) {
        let file = self.bytes().await;
        for block in [0, to_usize(BLOCK)] {
            let tail = &file[block + TAIL_AT..block + TAIL_AT + 8];
            assert_eq!(tail, [0; 8], "the tail offset of the ring is not 0");
        }
        let u32_at = |at: usize| {
            u32::from_le_bytes(file[at..at + 4].try_into().expect("four bytes"))
        };
        let len_at = |at: usize| to_usize(u64::from(u32_at(at)));
        let start = to_usize(AREA_START + offset);
        let (mut before, mut at) = (None, to_usize(AREA_START));
        while at < start {
            before = Some(at);
            at += (9 + len_at(at)).next_multiple_of(to_usize(BLOCK));
        }
        assert_eq!(at, start, "no record starts at {offset}");
        let chain = match before.map(|before| (before, file[before + 8])) {
            None => {
                let [first, second] =
                    [0, to_usize(BLOCK)].map(|block| &file[block..block + SEQ_AT]);
                assert_eq!(first, second, "the header blocks differ before the seq");
                assert!(first.starts_with(b"FNDNRING"), "the ring has no header");
                u32_at(CHAIN_AT)
            }
            Some((before, RESTART)) => u32_at(before + 9),
            Some((before, DATA | CARRY)) => u32_at(before + 4),
            Some((_, kind)) => panic!("no chain value of kind {kind} before {offset}"),
        };
        let crc = crc32c::crc32c_append(chain, &file[start..start + 4]);
        let end = start + 9 + len_at(start);
        let crc = crc32c::crc32c_append(crc, &file[start + 8..end]);
        self.put(start + 4, &crc.to_le_bytes()).await;
    }

    /// Puts a carry record with `body` at `offset` of the area and seals it.
    async fn put_carry(&self, offset: u64, body: &[u8]) {
        let len = u32::try_from(body.len()).expect("a short body");
        let record = [&len.to_le_bytes()[..], &[0; 4], &[CARRY], body].concat();
        self.put(to_usize(AREA_START + offset), &record).await;
        self.seal(offset).await;
    }

    /// Makes a ring with a data record at `BLOCK` and one at `2 * BLOCK`.
    async fn create_two_records(&self) {
        let mut slots = Slots::new();
        let buffer = self
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        for (first, stamp) in [(0, 30), (3, 60)] {
            let parts = Parts::default();
            buffer
                .append([entry(1, a, Path::Live, first, 3, Some(stamp), parts)])
                .expect("queues");
            buffer.committed().await.expect("commits");
        }
    }

    /// Makes a ring with a data record of two blocks at `BLOCK` and a data record at
    /// `3 * BLOCK`, and gives its layout.
    async fn create_long_record(&self) -> Layout {
        let ring = layout(AREA, 3 * to_usize(BLOCK) - 9);
        let mut slots = Slots::new();
        let buffer = self.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let long = Parts::from(self.block(4244));
        for (first, stamp, parts) in [(0, 30, long), (3, 60, Parts::default())] {
            buffer
                .append([entry(1, a, Path::Live, first, 3, Some(stamp), parts)])
                .expect("queues");
            buffer.committed().await.expect("commits");
        }
        ring
    }
}

fn to_usize(value: u64) -> usize {
    usize::try_from(value).expect("fits in usize")
}

/// `halves` half commit spans: `commits(21)` is ten and a half.
fn commits(halves: i64) -> Span {
    Span::from_nanos(COMMIT.nanos() / 2 * halves)
}

/// `count` tenths of a commit span.
fn tenths(count: i64) -> Span {
    Span::from_nanos(COMMIT.nanos() / 10 * count)
}

fn layout(area: u64, body_max: usize) -> Layout {
    Layout::new(area, body_max).expect("the sizes make a ring")
}

/// The smallest ring whose records hold a body of at most `body_max` bytes.
fn least(body_max: usize) -> Layout {
    let min = Layout::fit(0, body_max)
        .expect_err("no ring in no bytes")
        .min;
    Layout::fit(min, body_max).expect("the least length holds a ring")
}

fn key(index: u32) -> channel::Key {
    channel::Key::from_u128(u128::from(index))
}

fn entry(
    index: u32,
    slot: Slot,
    path: Path,
    first: u64,
    len: u32,
    last: Option<i64>,
    parts: Parts,
) -> Entry {
    Entry {
        index: key(index),
        slot,
        path,
        first,
        samples: samples(len, last),
        stored_at: Stamp::from_nanos(7),
        tag: 0,
        parts,
    }
}

/// The samples of an entry of `len` samples with the newest stamp `last`.
fn samples(len: u32, last: Option<i64>) -> Option<Samples> {
    let Some(len) = NonZeroU32::new(len) else {
        assert_eq!(last, None, "an entry with no samples has no stamp");
        return None;
    };
    let last = last.map(Stamp::from_nanos);
    Some(Samples { len, last })
}

fn tail(seq: u64, stamp: Option<i64>) -> Tail {
    Tail {
        seq,
        stamp: stamp.map(Stamp::from_nanos),
    }
}

fn mark(seq: u64, given: u64) -> Mark {
    Mark { seq, given }
}

/// One tail of a carry record of the live path of index `index`, as on disk.
fn carried(index: u32, seq: u64, given: u64, stamp: Option<i64>) -> Vec<u8> {
    let last = match stamp {
        Some(stamp) => [&[1][..], &stamp.to_le_bytes()].concat(),
        None => vec![0; 9],
    };
    let fields = [
        &u128::from(index).to_le_bytes()[..],
        &[0],
        &seq.to_le_bytes(),
        &given.to_le_bytes(),
        &last,
    ];
    fields.concat()
}

/// The body of a carry record that holds `tails`.
fn carry_body(tails: &[Vec<u8>]) -> Vec<u8> {
    let count = u32::try_from(tails.len()).expect("few tails");
    [count.to_le_bytes().to_vec(), tails.concat()].concat()
}

/// What a read gives back for an `entry` with tag 0 and `bytes`.
fn stored(first: u64, len: u32, last: Option<i64>, bytes: Block) -> Stored {
    Stored {
        first,
        samples: samples(len, last),
        stored_at: Stamp::from_nanos(7),
        tag: 0,
        bytes,
    }
}

/// A read that gives `entries` with no gap, and continues at `next`.
fn whole(entries: Vec<Stored>, next: Mark) -> Read {
    Read {
        gap: None,
        entries,
        next,
    }
}

/// Every read of `path` from `from` that follows `next` with `budget`, up to and
/// including the first that gives nothing. Each read keeps the read rules: a gap
/// only before its first entry and only when the entry starts past the mark, no
/// gap between its entries, `next` after its last entry, and at most one entry
/// past the budget.
async fn read_all(
    buffer: &Buffer,
    slot: Slot,
    path: Path,
    from: Mark,
    budget: usize,
) -> Vec<Read> {
    let mut reads = Vec::new();
    let mut from = from;
    loop {
        let read = buffer
            .reader()
            .read(slot, path, from, budget)
            .await
            .expect("the read passes");
        let mut at = from.seq;
        let mut given = 0;
        for (number, entry) in read.entries.iter().enumerate() {
            assert!(given < budget, "entry {number} came past the budget");
            given += block::footprint(entry.bytes.len());
            if number == 0 {
                let gap = (entry.first > at).then_some(at..entry.first);
                assert_eq!(read.gap, gap, "the gap before the first entry");
            } else {
                assert_eq!(entry.first, at, "a gap before entry {number}");
            }
            at = entry.seqs().end;
        }
        if read.entries.is_empty() {
            assert_eq!(read.gap, None, "a gap with no entry");
            assert_eq!(read.next, from, "an empty read moved the mark");
        } else {
            assert_eq!(read.next.seq, at, "next is after the last entry");
        }
        let done = read.entries.is_empty();
        from = read.next;
        reads.push(read);
        if done {
            return reads;
        }
    }
}

/// The entries of every read in `reads`, in order.
fn entries(reads: &[Read]) -> Vec<Stored> {
    reads.iter().flat_map(|read| read.entries.clone()).collect()
}

/// Starts one shard of one node to run `main` until it returns, on the node's
/// disk. A panic in `main` comes back from the run as [`sim::Error::Panicked`].
fn start<F>(
    seed: u64,
    main: impl FnOnce(Shard) -> F + Send + 'static,
) -> (sim::Sim, env::thread::Handle)
where
    F: Future<Output = ()> + 'static,
{
    let (sim, node) = create_node(seed);
    let config = env::shards::Config {
        name: DIR.into(),
        core: None,
    };
    let handle = node
        .shards()
        .start(config, move |tasks| main(shard_on(node, tasks)))
        .expect("the shard starts");
    (sim, handle)
}

/// A shard on the disk of `node`, with `tasks`, whose opens do not trim.
fn shard_on(node: sim::node::Node, tasks: Tasks) -> Shard {
    Shard {
        files: node.files(),
        pool: Rc::new(pool()),
        clock: node.clock(),
        tasks,
        entropy: node.entropy(),
        disk: Disk::Sim(node),
    }
}

/// Runs `main` as [`start`] does, until it returns.
fn run<F>(seed: u64, main: impl FnOnce(Shard) -> F + Send + 'static)
where
    F: Future<Output = ()> + 'static,
{
    let (mut sim, handle) = start(seed, main);
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
}

/// Runs `main` as [`run`] does, with the files in a new `Memory` in place of the
/// node's disk. Only a test that needs what `Memory` gives runs on it.
fn run_on_memory<F>(seed: u64, main: impl FnOnce(Shard) -> F + Send + 'static)
where
    F: Future<Output = ()> + 'static,
{
    run(seed, |shard| {
        let memory = Memory::default();
        main(Shard {
            files: memory.files(),
            disk: Disk::Memory(memory),
            ..shard
        })
    });
}

/// A pool with the budget of a test's shard.
fn pool() -> Pool {
    let config = block::Config::new(POOL).expect("the budget fits");
    Pool::new(config.clone(), Heap::new(config.reservation()))
}

/// The result of a call to the memory driver, which ends at once.
fn ready<T>(future: impl Future<Output = T>) -> T {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("a memory call ends at once"),
    }
}

#[test]
fn a_memory_rename_to_a_taken_name_spelled_with_a_dot_gives_exists() {
    let files = Memory::default().files();
    let create = Mode::Create { len: 4_096 };
    drop(ready(files.open(FilePath::new("b"), create)).unwrap());
    let mut file = ready(files.open(FilePath::new("a"), create)).unwrap();
    let found = ready(file.rename(FilePath::new("./b")));
    assert_eq!(found, Err(FileError::Exists { path: "./b".into() }));
    let names = ready(files.list(FilePath::new(""))).unwrap();
    assert_eq!(names, [PathBuf::from("a"), PathBuf::from("b")]);
    ready(file.rename(FilePath::new("./c"))).unwrap();
    let reopened = ready(files.open(FilePath::new("c"), Mode::Read)).unwrap();
    assert_eq!(reopened.len(), 4_096);
}

#[test]
fn a_memory_path_spelled_with_a_dot_names_the_same_file_in_each_call() {
    let memory = Memory::default();
    let files = memory.files();
    let create = Mode::Create { len: 4_096 };
    let mut file = ready(files.open(FilePath::new("./a"), create)).unwrap();
    drop(ready(files.open(FilePath::new("a"), create)).unwrap());
    assert_eq!(memory.bytes("./a").len(), 4_096);
    ready(file.rename(FilePath::new("b"))).unwrap();
    ready(files.remove(FilePath::new("./b"))).unwrap();
    let names = ready(files.list(FilePath::new(""))).unwrap();
    assert_eq!(names, Vec::<PathBuf>::new());
}

#[test]
fn a_memory_path_with_a_trailing_slash_names_only_a_directory() {
    let files = Memory::default().files();
    drop(ready(files.open(FilePath::new("a"), Mode::Create { len: 1 })).unwrap());
    let mut results = Vec::new();
    for (path, mode) in [
        ("a/", Mode::Write),
        ("a/.", Mode::Read),
        ("a//", Mode::Read),
        ("a/", Mode::Create { len: 1 }),
        ("b/", Mode::Create { len: 1 }),
        ("b/", Mode::Read),
        ("b/.", Mode::Read),
    ] {
        results.push(ready(files.open(FilePath::new(path), mode)).map(drop));
    }
    results.push(ready(files.remove(FilePath::new("a/"))));
    let names = ready(files.list(FilePath::new(""))).unwrap();
    assert_eq!(
        names,
        [PathBuf::from("a")],
        "a refused remove keeps the file"
    );
    for path in ["b/", "a"] {
        results.push(ready(files.remove(FilePath::new(path))));
    }
    let io = |path: &str, operation, code| FileError::Io {
        path: path.into(),
        operation,
        code,
    };
    let expected = [
        Err(io("a/", Operation::Open, 20)),
        Err(io("a/.", Operation::Open, 20)),
        Err(io("a//", Operation::Open, 20)),
        Err(io("a/", Operation::Open, 21)),
        Err(io("b/", Operation::Open, 21)),
        Err(FileError::NotFound { path: "b/".into() }),
        Err(FileError::NotFound { path: "b/.".into() }),
        Err(io("a/", Operation::Remove, 20)),
        Ok(()),
        Ok(()),
    ];
    assert_eq!(results, expected);
}

#[test]
fn a_memory_path_of_the_data_directory_names_no_file() {
    let files = Memory::default().files();
    let io = |path: &str, operation, code| FileError::Io {
        path: path.into(),
        operation,
        code,
    };
    let mut results = Vec::new();
    for (path, mode) in [
        ("./", Mode::Read),
        ("./", Mode::Write),
        (".", Mode::Read),
        (".", Mode::Create { len: 1 }),
    ] {
        results.push(ready(files.open(FilePath::new(path), mode)).map(drop));
    }
    results.push(ready(files.remove(FilePath::new("."))));
    let expected = [
        Err(io("./", Operation::Open, 21)),
        Err(io("./", Operation::Open, 21)),
        Err(io(".", Operation::Open, 21)),
        Err(io(".", Operation::Open, 21)),
        Err(io(".", Operation::Remove, 21)),
    ];
    assert_eq!(results, expected);
}

#[test]
fn a_memory_empty_path_names_no_file() {
    let files = Memory::default().files();
    let mut results = Vec::new();
    for mode in [Mode::Read, Mode::Write, Mode::Create { len: 1 }] {
        results.push(ready(files.open(FilePath::new(""), mode)).map(drop));
    }
    results.push(ready(files.remove(FilePath::new(""))));
    let not_found = || FileError::NotFound { path: "".into() };
    let expected = [Err(not_found()), Err(not_found()), Err(not_found()), Ok(())];
    assert_eq!(results, expected);
    assert_eq!(
        ready(files.list(FilePath::new(""))).unwrap(),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn a_new_ring_keeps_its_layout_across_opens() {
    run_on_memory(1, |shard| async move {
        let first = layout(AREA, BODY_MAX);
        let buffer = shard.open(first, &mut Slots::new()).await.expect("opens");
        assert_eq!(buffer.layout(), first);
        assert_eq!(
            shard.memory().syncs(),
            3,
            "the header and the two of the open"
        );
        drop(buffer);
        let header = shard.bytes().await[..to_usize(AREA_START)].to_vec();
        let other = layout(2 * AREA, 2 * BODY_MAX);
        let buffer = shard.open(other, &mut Slots::new()).await.expect("reopens");
        assert_eq!(buffer.layout(), first);
        let reopened = &shard.bytes().await[..to_usize(AREA_START)];
        assert_eq!(reopened, header, "the reopen rewrote the header");
        let bytes = shard.bytes().await;
        assert_eq!(bytes.len(), to_usize(AREA_START + AREA));
        assert_eq!(&bytes[..8], b"FNDNRING");
        assert_eq!(
            bytes[..to_usize(BLOCK)],
            bytes[to_usize(BLOCK)..to_usize(AREA_START)]
        );
    });
}

#[test]
fn pool_is_the_pool_of_the_config() {
    run(1, |shard| async move {
        let layout = layout(AREA, BODY_MAX);
        let buffer = shard.open(layout, &mut Slots::new()).await.expect("opens");
        assert!(std::ptr::eq(buffer.pool(), Rc::as_ptr(&shard.pool)));
    });
}

#[test]
fn entries_are_durable_at_committed_and_recovered_at_open() {
    run_on_memory(2, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let b = slots.index(key(2));
        let parts = Parts::from(shard.block(100));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), parts.clone())])
            .expect("queues");
        buffer
            .append([
                entry(2, b, Path::Backfill, 5, 2, Some(7), parts.clone()),
                entry(1, a, Path::Live, 3, 1, None, Parts::default()),
            ])
            .expect("queues");
        let live = tail(4, Some(30));
        let backfill = tail(7, Some(7));
        assert_eq!(buffer.tail(a, Path::Live), live);
        assert_eq!(buffer.tail(b, Path::Backfill), backfill);
        assert_eq!(buffer.durable(a, Path::Live), Tail::default());
        assert_eq!(buffer.durable(b, Path::Backfill), Tail::default());
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), live);
        assert_eq!(buffer.durable(b, Path::Backfill), backfill);
        assert_eq!(shard.memory().syncs(), 4, "one sync per commit");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        let b = slots.index(key(2));
        let a = slots.index(key(1));
        assert_eq!(buffer.tail(a, Path::Live), live);
        assert_eq!(buffer.durable(a, Path::Live), live);
        assert_eq!(buffer.tail(b, Path::Backfill), backfill);
        assert_eq!(buffer.tail(a, Path::Backfill), Tail::default());
        assert_eq!(buffer.tail(b, Path::Live), Tail::default());
    });
}

/// Two entries of one path in one record keep their own `stored_at`.
#[test]
fn a_read_gives_each_entry_its_own_stored_at() {
    run(1, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let stamped = |first, stored_at| Entry {
            stored_at: Stamp::from_nanos(stored_at),
            ..entry(1, a, Path::Live, first, 1, None, shard.block(8).into())
        };
        buffer
            .append([stamped(0, 7), stamped(1, 8)])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        let stamps: Vec<i64> = read
            .entries
            .iter()
            .map(|entry| entry.stored_at.nanos())
            .collect();
        assert_eq!(stamps, [7, 8]);
    });
}

/// A `Reader` taken before the buffer drops gives the entries made durable before the
/// drop, as before it, and keeps the ring file open until it drops.
#[test]
fn reader_reads_after_the_buffer_drops() {
    run_on_memory(28, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let parts = Parts::from(shard.block(100));
        buffer
            .append([
                entry(1, a, Path::Live, 0, 3, Some(30), parts.clone()),
                entry(1, a, Path::Live, 3, 2, Some(31), parts),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let reader = buffer.reader();
        let before = reader
            .read(a, Path::Live, Mark::at(1), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(before.entries.len(), 2);
        let ended = buffer.ended();
        drop(buffer);
        ended.await.expect("ends");
        let after = reader
            .read(a, Path::Live, Mark::at(1), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(after, before);
        assert_eq!(shard.memory().open_files(), 1, "the reader holds the ring");
        drop(reader);
        assert_eq!(shard.memory().open_files(), 0);
    });
}

/// An open takes a ring that holds a record of the caller, with no samples.
#[test]
fn an_open_takes_a_ring_with_a_caller_record() {
    run(1, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 0, None, shard.block(8).into())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        let reopened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(reopened.err(), None);
    });
}

#[test]
fn durable_moves_only_at_a_commit() {
    run(3, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        buffer
            .append([entry(1, a, Path::Live, 3, 2, Some(50), Parts::default())])
            .expect("queues");
        assert_eq!(buffer.tail(a, Path::Live), tail(5, Some(50)));
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(5, Some(50)));
    });
}

/// The shard waits on a gate with no timer, so only a task of the buffer can run in
/// the idle window.
#[test]
fn an_idle_buffer_wakes_no_task() {
    let gate = Arc::new(Mutex::new((false, None::<Waker>)));
    let held = Arc::clone(&gate);
    let (mut sim, handle) = start(20, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        poll_fn(|cx| {
            let mut gate = held.lock().unwrap();
            if gate.0 {
                return Poll::Ready(());
            }
            gate.1 = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        drop(buffer);
    });
    sim.run_for(COMMIT).expect("the open ends");
    let idle = sim.digest();
    sim.run_for(commits(200)).expect("the buffer idles");
    assert_eq!(sim.digest(), idle, "a task ran while the buffer idled");
    let waker = {
        let mut gate = gate.lock().unwrap();
        gate.0 = true;
        gate.1.take()
    };
    waker.expect("the shard waits on the gate").wake();
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
}

/// Runs a buffer that idles while its shard wakes every one and a half commits,
/// with `empty` empty appends at each wake. Returns the digest of the run.
fn idle_with_wakes(empty: usize) -> u64 {
    let (mut sim, handle) = start(27, move |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        for _ in 0..8 {
            shard.clock.sleep(commits(3)).await;
            for _ in 0..empty {
                buffer.append(Vec::new()).expect("takes an empty batch");
            }
        }
        drop(buffer);
    });
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
    sim.digest()
}

/// Empty appends on an idle buffer wake no task: the run goes as one with no
/// appends.
#[test]
fn empty_appends_wake_no_task() {
    assert_eq!(idle_with_wakes(2), idle_with_wakes(0));
}

/// Runs a buffer whose task waits for the deadline of one entry, with `empty` empty
/// appends meanwhile. Returns the digest of the run.
fn busy_with_appends(empty: usize) -> u64 {
    let (mut sim, handle) = start(28, move |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.clock.sleep(commits(3)).await;
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(3)).await;
        for _ in 0..empty {
            buffer.append(Vec::new()).expect("takes an empty batch");
        }
        assert_eq!(buffer.committed().await, Ok(()));
        drop(buffer);
    });
    sim.run().expect("the run ends");
    handle.join().expect("the shard ended");
    sim.digest()
}

/// Appends to a buffer whose task does not idle wake no task: the waker that the
/// first append takes is gone.
#[test]
fn appends_to_a_busy_buffer_wake_no_task() {
    assert_eq!(busy_with_appends(2), busy_with_appends(0));
}

/// A `committed` with nothing appended before it waits on nothing: it resolves at
/// once, and the task syncs nothing for it.
#[test]
fn committed_on_an_idle_buffer_resolves_at_once() {
    run_on_memory(21, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        shard.clock.sleep(commits(21)).await;
        let before = shard.clock.now();
        let mut commit = pin!(buffer.committed());
        let polled = poll_fn(|cx| Poll::Ready(commit.as_mut().poll(cx))).await;
        assert_eq!(polled, Poll::Ready(Ok(())), "the first poll resolves");
        shard.clock.sleep(commits(4)).await;
        assert_eq!(
            shard.memory().syncs(),
            3,
            "the header and the two of the open only"
        );
        assert_eq!(shard.clock.now() - before, commits(4), "no deadline ran");
    });
}

#[test]
fn the_first_append_after_an_idle_span_commits_after_one_commit() {
    run_on_memory(22, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.clock.sleep(commits(21)).await;
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(9)).await;
        assert_eq!(buffer.durable(a, Path::Live), tail(0, None));
        shard.clock.sleep(tenths(2)).await;
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
        assert_eq!(shard.memory().syncs(), 4, "the append alone woke the task");
    });
}

#[test]
fn a_busy_buffer_keeps_one_deadline_per_commit() {
    run_on_memory(42, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().slow_syncs(shard.clock.clone(), tenths(2));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(11)).await;
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues during the first sync");
        shard.clock.sleep(tenths(12)).await;
        assert_eq!(shard.memory().syncs(), 5, "two deadlines, one commit apart");
        assert_eq!(buffer.durable(a, Path::Live), tail(2, Some(2)));
    });
}

/// The task parks with a deadline it never polled. A push at that exact instant
/// wakes it, and the deadline that just passed fires at once.
#[test]
fn an_append_at_the_deadline_of_a_parked_task_commits_at_once() {
    run_on_memory(43, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.clock.sleep(COMMIT).await;
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() / 10))
            .await;
        assert_eq!(
            shard.memory().syncs(),
            4,
            "the passed deadline fired at the wake"
        );
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
    });
}

/// A deadline that passes while a sync runs fires when the sync ends: the task did not
/// idle, so the entries that came during the sync wait no longer. The task idles once
/// before the first entry, as after an open.
#[test]
fn a_deadline_that_passes_during_a_sync_fires_when_the_sync_ends() {
    for (tenths, seed) in [(10, 45), (11, 46), (30, 140)] {
        run_on_memory(seed, move |shard| async move {
            let mut slots = Slots::new();
            let buffer = shard
                .open(layout(AREA, BODY_MAX), &mut slots)
                .await
                .expect("opens");
            let a = slots.index(key(1));
            let tenth = COMMIT.nanos() / 10;
            let sync = Span::from_nanos(tenth * tenths);
            shard.memory().slow_syncs(shard.clock.clone(), sync);
            shard.clock.sleep(commits(21)).await;
            let opened = shard.clock.now();
            buffer
                .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
                .expect("queues");
            shard.clock.sleep(Span::from_nanos(tenth * 11)).await;
            buffer
                .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
                .expect("queues during the first sync");
            let end = opened + Span::from_nanos(tenth * (10 + 2 * tenths + 1));
            shard.clock.sleep_until(end).await;
            assert_eq!(
                buffer.durable(a, Path::Live),
                tail(2, Some(2)),
                "a sync of {tenths} tenths: the second entry waits for no fresh \
                 deadline"
            );
        });
    }
}

#[test]
fn a_dropped_idle_buffer_ends_its_task_at_once() {
    run(23, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        shard.clock.sleep(commits(21)).await;
        assert!(shard.ring_held().await, "the task holds the ring");
        drop(buffer);
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() / 4))
            .await;
        assert!(!shard.ring_held().await, "the task holds the ring open");
    });
}

#[test]
fn a_batch_past_the_open_group_starts_the_next_record() {
    run_on_memory(4, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let first = Parts::from(shard.block(2000));
        let rest = Parts::from(shard.block(1500));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), first.clone())])
            .expect("queues");
        buffer
            .append([
                entry(1, a, Path::Live, 1, 1, Some(2), rest.clone()),
                entry(1, a, Path::Live, 2, 1, Some(3), rest.clone()),
            ])
            .expect("the batch starts the next record");
        assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(3)));
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(3)));
        assert_eq!(shard.memory().syncs(), 4, "both records go in one sync");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.index(key(1)), Path::Live),
            tail(3, Some(3))
        );
    });
}

#[test]
fn a_full_ring_queues_nothing() {
    run(5, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(7 * BLOCK, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let parts = Parts::from(shard.block(3900));
        for seq in 0..3 {
            buffer
                .append([entry(1, a, Path::Live, seq, 1, None, parts.clone())])
                .expect("the record has room");
        }
        let full = buffer.append([entry(1, a, Path::Live, 3, 1, None, parts.clone())]);
        assert_eq!(
            full,
            Err(Rejected::Full {
                needed: 4096,
                free: 0
            })
        );
        assert_eq!(buffer.tail(a, Path::Live), tail(3, None));
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(3, None));
    });
}

#[test]
fn a_batch_is_queued_whole_or_not_at_all() {
    run(6, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(11 * BLOCK, 8183), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let long = Parts::from(shard.block(5000));
        let parts = Parts::from(shard.block(3900));
        for seq in 0..3 {
            buffer
                .append([entry(1, a, Path::Live, seq, 1, None, long.clone())])
                .expect("the record has room");
        }
        let full = buffer.append([
            entry(1, a, Path::Live, 3, 1, None, parts.clone()),
            entry(1, a, Path::Live, 4, 1, None, parts.clone()),
        ]);
        assert_eq!(
            full,
            Err(Rejected::Full {
                needed: 8192,
                free: 4096
            }),
            "the first entry alone has room, the batch does not"
        );
        assert_eq!(buffer.tail(a, Path::Live), tail(3, None));
        buffer.committed().await.expect("commits");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(11 * BLOCK, 8183), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(buffer.tail(slots.index(key(1)), Path::Live), tail(3, None));
    });
}

/// A batch over a limit of one record is refused whole, and the batches before
/// and after it commit in its place.
#[test]
fn a_batch_no_record_holds_is_large_and_queues_nothing() {
    run(108, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let small = Parts::from(shard.block(10));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, None, small.clone())])
            .expect("the record has room");
        let empty = Parts::from(shard.block(0));
        let two = Parts::from([shard.block(0), shard.block(0)]);
        let body = Parts::from(shard.block(buffer.layout().entry_max() + 1));
        let cases = [
            (
                vec![entry(1, a, Path::Live, 1, 1, None, empty.clone()); 1024],
                Limit::Entries { count: 1024 },
                "the batch has 1024 entries, and a record holds at most 1023",
            ),
            (
                vec![entry(1, a, Path::Live, 1, 1, None, two); 512],
                Limit::Parts { count: 1024 },
                "the batch has 1024 parts, and a record holds at most 1023",
            ),
            (
                vec![entry(1, a, Path::Live, 1, 1, None, body.clone())],
                Limit::Body {
                    len: 4088,
                    max: 4087,
                },
                "the batch needs a record body of 4088 bytes, and a record of this \
                 ring holds at most 4087",
            ),
        ];
        for (batch, limit, message) in cases {
            let large = buffer.append(batch);
            assert_eq!(large, Err(Rejected::Large(limit)));
            assert_eq!(Rejected::Large(limit).to_string(), message);
            assert_eq!(buffer.tail(a, Path::Live), tail(1, None));
        }
        buffer
            .append([entry(1, a, Path::Live, 1, 1, None, small.clone())])
            .expect("the record has room");
        buffer.committed().await.expect("commits");
        drop(buffer);
        let count = to_usize(AREA_START + BLOCK) + 9;
        assert_eq!(
            shard.bytes().await[count..count + 4],
            2_u32.to_le_bytes(),
            "the large batches left the group open, so one record holds both"
        );
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(buffer.tail(slots.index(key(1)), Path::Live), tail(2, None));
    });
}

/// `append` on a live buffer refuses a batch with `Rejected::Large(limit)` exactly
/// when `Layout::check` gives `Err(limit)` for its counts, so a caller can check a
/// batch before it takes the blocks of its entries.
#[test]
fn an_append_is_large_exactly_when_the_layout_check_fails() {
    run(141, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let max = buffer.layout().entry_max();
        let cases = [
            (1, 1, 0),
            (1, 1, max),
            (1, 1, max + 1),
            (1, 2, max),
            (2, 2, max - 51),
            (2, 2, max - 50),
            (1023, 0, 0),
            (1024, 0, 0),
            (512, 1024, 0),
            (1023, 1023, 0),
        ];
        let mut next = 0;
        for (entries, parts, bytes) in cases {
            let batch: Vec<Entry> = (0..entries)
                .map(|at| {
                    let own = (parts * (at + 1)) / entries - (parts * at) / entries;
                    let first = if at == 0 { bytes } else { 0 };
                    let parts = match own {
                        0 => Parts::default(),
                        1 => Parts::from(shard.block(first)),
                        _ => Parts::from([shard.block(first), shard.block(0)]),
                    };
                    let first = next + u64::try_from(at).expect("a count fits");
                    entry(1, a, Path::Live, first, 1, None, parts)
                })
                .collect();
            let checked = buffer.layout().check(entries, parts, bytes);
            let appended = buffer.append(batch);
            assert_eq!(
                appended,
                checked.map_err(Rejected::Large),
                "{entries} entries, {parts} parts, {bytes} bytes"
            );
            if appended.is_ok() {
                next += u64::try_from(entries).expect("a count fits");
            }
        }
    });
}

/// A block of a size class only the test uses: two purges give its class back
/// once the append dropped it. Two, because a purge frees a class that was idle
/// at the purge before.
#[test]
fn a_failed_append_holds_no_part() {
    run(109, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.pool.purge();
        shard.pool.purge();
        let before = shard.pool.committed();
        let body = Parts::from(shard.block(200_000));
        let large = buffer.append([entry(1, a, Path::Live, 0, 1, None, body)]);
        assert_eq!(
            large,
            Err(Rejected::Large(Limit::Body {
                len: 200_055,
                max: BODY_MAX,
            }))
        );
        shard.pool.purge();
        assert!(
            shard.pool.purge() > 0,
            "the class of the part has no block in use"
        );
        assert_eq!(shard.pool.committed(), before, "the part went back");
    });
}

/// A reopen syncs the ring twice, the records it read, then its restart record,
/// before it reports the tails durable, and a failed sync fails the reopen.
#[test]
fn a_reopen_syncs_the_ring_twice() {
    run_on_memory(26, |shard| async move {
        let ring = layout(AREA, BODY_MAX);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        for (first, last) in [(0, 30), (3, 60)] {
            buffer
                .append([entry(
                    1,
                    a,
                    Path::Live,
                    first,
                    3,
                    Some(last),
                    Parts::default(),
                )])
                .expect("queues");
            buffer.committed().await.expect("commits");
        }
        drop(buffer);
        drop(shard.open(ring, &mut Slots::new()).await.expect("reopens"));
        assert_eq!(
            shard.memory().syncs(),
            7,
            "the header, two of the open, two commits, two of the reopen"
        );
        shard.memory().fail_syncs();
        let reopened = shard.open(ring, &mut Slots::new()).await.map(drop);
        let failed = Error::Files(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        assert_eq!(reopened, Err(failed));
    });
}

/// An open syncs the header and the records that it walked, as read, before it
/// writes a new byte, in a ring of 64 blocks that it does not trim and in one of 7
/// that it trims: when that sync fails, the ring holds what it held before the open.
#[test]
fn an_open_whose_first_sync_fails_leaves_the_ring_as_it_was() {
    for blocks in [64, 7] {
        run_on_memory(21, move |shard| async move {
            let ring = layout(blocks * BLOCK, BODY_MAX);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            let parts = Parts::from(shard.block(3900));
            for seq in 0..3 {
                buffer
                    .append([entry(1, a, Path::Live, seq, 1, None, parts.clone())])
                    .expect("the record has room");
            }
            buffer.committed().await.expect("commits");
            drop(buffer);
            let before = shard.bytes().await;
            shard.memory().fail_syncs();
            let reopened = shard.open(ring, &mut Slots::new()).await.map(drop);
            let failed = Error::Files(FileError::Io {
                path: PathBuf::from(RING),
                operation: Operation::Sync,
                code: 5,
            });
            assert_eq!(reopened, Err(failed), "{blocks} blocks");
            let changed = shard.bytes().await != before;
            assert!(!changed, "{blocks} blocks: the open wrote before its sync");
        });
    }
}

/// A failed write of a window that the walk read fails the open with the write's
/// error.
#[test]
fn a_failed_write_of_the_walk_fails_the_open() {
    run_on_memory(7, |shard| async move {
        let ring = layout(AREA, BODY_MAX);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        // The walk writes the window at the tail, the area's start, as it read it.
        shard.memory().fail_write_at(AREA_START);
        let reopened = shard.open(ring, &mut Slots::new()).await.map(drop);
        let failed = Error::Files(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::WriteAt,
            code: 5,
        });
        assert_eq!(reopened, Err(failed));
    });
}

#[test]
fn a_failed_sync_ends_the_buffer_with_its_error() {
    run_on_memory(7, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        let ended = Err(failed.clone());
        assert_eq!(buffer.committed().await, ended);
        assert_eq!(buffer.durable(a, Path::Live), Tail::default());
        assert_eq!(
            buffer.append([entry(1, a, Path::Live, 3, 1, None, Parts::default())]),
            Err(Rejected::Files(failed.clone()))
        );
        assert_eq!(
            buffer.append(Vec::new()),
            Err(Rejected::Files(failed)),
            "an empty append"
        );
        assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(30)));
        assert_eq!(buffer.committed().await, ended);
        assert_eq!(
            shard.memory().syncs(),
            4,
            "the task ended at the failed sync"
        );
    });
}

/// `commits` counts the commits that ended. A `committed` with nothing pending
/// makes no commit, and a failed commit does not count.
#[test]
fn commits_counts_the_commits_that_ended() {
    run_on_memory(29, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        assert_eq!(buffer.commits(), 0);
        for (count, first, last) in [(1, 0, 30), (2, 3, 60)] {
            buffer
                .append([entry(
                    1,
                    a,
                    Path::Live,
                    first,
                    3,
                    Some(last),
                    Parts::default(),
                )])
                .expect("queues");
            buffer.committed().await.expect("commits");
            assert_eq!(buffer.commits(), count);
        }
        let syncs = shard.memory().syncs();
        buffer.committed().await.expect("nothing pending");
        assert_eq!(buffer.commits(), 2, "nothing pending made no commit");
        assert_eq!(shard.memory().syncs(), syncs, "it synced nothing");
        shard.memory().fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 6, 1, None, Parts::default())])
            .expect("queues");
        let failed = Err(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        assert_eq!(buffer.committed().await, failed);
        assert_eq!(buffer.commits(), 2, "the failed commit did not count");
    });
}

/// A move of `commits` does not make every entry durable: an entry appended while
/// a commit runs goes in the next one, and so does a `committed` future made then.
#[test]
fn commits_moves_before_an_entry_appended_during_the_commit_is_durable() {
    run_on_memory(30, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(12)).await;
        assert_eq!(buffer.commits(), 0, "the first sync runs");
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues during the sync");
        let mut commit = pin!(buffer.committed());
        let polled = poll_fn(|cx| Poll::Ready(commit.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "the future waits for the next commit");
        shard.clock.sleep(tenths(4)).await;
        assert_eq!(buffer.commits(), 1);
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
        let polled = poll_fn(|cx| Poll::Ready(commit.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "the future still waits");
    });
}

/// Two reads with one count see one `durable`, and a read that shows a new count
/// sees the entries of that commit durable, across every record of the commit.
#[test]
fn durable_changes_only_at_a_commit_that_moves_commits() {
    run_on_memory(132, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let b = slots.index(key(2));
        let read = |buffer: &Buffer| {
            (
                buffer.commits(),
                buffer.durable(a, Path::Live),
                buffer.durable(b, Path::Live),
            )
        };
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        let empty = (0, tail(0, None), tail(0, None));
        assert_eq!(read(&buffer), empty, "an append moves nothing");
        shard.clock.sleep(tenths(12)).await;
        assert_eq!(shard.memory().syncs(), 4, "the first sync runs");
        assert_eq!(read(&buffer), empty, "a sync in flight moves nothing");
        let big = Parts::from(shard.block(2000));
        let rest = Parts::from(shard.block(1500));
        buffer
            .append([entry(2, b, Path::Live, 0, 1, Some(2), big)])
            .expect("queues during the sync");
        buffer
            .append([
                entry(2, b, Path::Live, 1, 1, Some(3), rest.clone()),
                entry(2, b, Path::Live, 2, 1, Some(4), rest),
            ])
            .expect("starts a second record during the sync");
        shard.clock.sleep(tenths(1)).await;
        assert_eq!(read(&buffer), empty, "an append in the sync moves nothing");
        shard.clock.sleep(tenths(3)).await;
        let first = (1, tail(1, Some(1)), empty.2);
        assert_eq!(read(&buffer), first, "the count shows the commit durable");
        shard.clock.sleep(tenths(6)).await;
        assert_eq!(read(&buffer), first, "the second sync runs");
        shard.clock.sleep(tenths(4)).await;
        let second = (2, tail(1, Some(1)), tail(3, Some(4)));
        assert_eq!(read(&buffer), second, "the count shows the commit durable");
    });
}

#[test]
fn a_drop_ends_the_commit_task() {
    run_on_memory(8, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(shard.memory().syncs(), 4);
        buffer
            .append([entry(1, a, Path::Live, 3, 1, Some(31), Parts::default())])
            .expect("queues");
        drop(buffer);
        shard.clock.sleep(commits(10)).await;
        assert_eq!(
            shard.memory().syncs(),
            5,
            "one deadline runs after the drop"
        );
        assert_eq!(shard.memory().open_files(), 0, "the task ended");
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.index(key(1)), Path::Live),
            tail(4, Some(31)),
            "the entry queued at the drop was written"
        );
    });
}

/// A `Commit` on an entry queued at the drop resolves when the deadline after the
/// drop writes the entry.
#[test]
fn a_commit_on_an_entry_queued_at_the_drop_resolves_at_the_next_deadline() {
    run_on_memory(136, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        let commit = buffer.committed();
        drop(buffer);
        let started = shard.clock.now();
        assert_eq!(commit.await, Ok(()), "the deadline wrote the entry");
        assert_eq!(shard.clock.now() - started, COMMIT, "at the next deadline");
        assert_eq!(
            shard.memory().open_files(),
            0,
            "the ring closed with the future"
        );
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.index(key(1)), Path::Live),
            tail(1, Some(1))
        );
    });
}

#[test]
fn committed_waits_for_an_entry_appended_during_a_commit() {
    run_on_memory(19, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let sync = Span::from_nanos(4_000_000);
        shard.memory().slow_syncs(shard.clock.clone(), sync);
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() + sync.nanos() / 2))
            .await;
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues while the first sync runs");
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(2, Some(2)));
        assert_eq!(
            shard.memory().syncs(),
            5,
            "the second entry took its own sync"
        );
    });
}

#[test]
fn next_commit_on_an_idle_buffer_waits_for_an_append_and_its_commit() {
    run_on_memory(24, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let mut next = pin!(buffer.next_commit());
        shard.clock.sleep(commits(21)).await;
        let polled = poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await;
        assert_eq!(polled, Poll::Pending, "no append, no commit");
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let before = shard.clock.now();
        next.await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
        assert_eq!(shard.clock.now() - before, COMMIT, "one deadline");
    });
}

#[test]
fn next_commit_during_a_commit_resolves_at_its_end() {
    run_on_memory(25, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let sync = Span::from_nanos(4_000_000);
        shard.memory().slow_syncs(shard.clock.clone(), sync);
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() + sync.nanos() / 2))
            .await;
        let before = shard.clock.now();
        buffer.next_commit().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
        assert_eq!(
            shard.clock.now() - before,
            Span::from_nanos(sync.nanos() / 2)
        );
        assert_eq!(shard.memory().syncs(), 4, "the commit in flight only");
    });
}

#[test]
fn next_commit_after_a_failed_commit_gives_its_error_at_once() {
    let (mut sim, node) = create_node(120);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        node.fail_file(FilePath::new(RING), Operation::Sync);
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(buffer.committed().await, Err(failed.clone()));
        let mut next = pin!(buffer.next_commit());
        let polled = poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await;
        assert_eq!(polled, Poll::Ready(Err(failed)));
    })
    .expect("the buffer ends");
}

#[test]
fn next_commit_held_past_the_drop_of_an_idle_buffer_resolves_at_its_end() {
    run_on_memory(26, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let next = buffer.next_commit();
        let ended = buffer.ended();
        drop(buffer);
        ended.await.expect("ends");
        assert_eq!(next.await, Ok(()));
        assert_eq!(shard.memory().syncs(), 3, "no commit after the open");
    });
}

/// A `next_commit` whose sync was in flight at its call gives `Ok` past the drop, also
/// when a later sync ends the task with an error.
#[test]
fn next_commit_held_past_the_drop_gives_ok_when_a_later_sync_fails() {
    run_on_memory(27, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(12)).await;
        let mut next = pin!(buffer.next_commit());
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues while the first sync runs");
        let late = buffer.committed();
        shard.memory().fail_syncs();
        let ended = buffer.ended();
        drop(buffer);
        shard.clock.sleep(tenths(3)).await;
        let polled = poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await;
        assert_eq!(polled, Poll::Pending, "the task has not ended");
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(ended.await, Err(failed.clone()));
        assert_eq!(next.await, Ok(()));
        assert_eq!(late.await, Err(failed));
    });
}

#[test]
fn next_commit_held_past_the_drop_gives_the_error_of_its_failed_sync() {
    run_on_memory(28, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        shard.memory().fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(12)).await;
        let next = buffer.next_commit();
        drop(buffer);
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(next.await, Err(failed));
    });
}

#[test]
fn an_entry_below_the_tail_is_a_broken_invariant() {
    let (mut sim, _handle) = start(9, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, None, Parts::default())])
            .expect("queues");
        drop(buffer.append([entry(1, a, Path::Live, 2, 1, None, Parts::default())]));
    });
    assert_eq!(
        sim.run(),
        Err(sim::Error::Panicked {
            thread: DIR.into(),
            message: "invariant: an entry of index \
                      00000000-0000-0000-0000-000000000001 on path Live starts at 2, \
                      below the tail 3"
                .into(),
            seed: 9,
        })
    );
}

#[test]
fn an_entry_past_the_last_seq_is_a_broken_invariant() {
    let (mut sim, _handle) = start(11, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        drop(buffer.append([entry(
            1,
            a,
            Path::Live,
            u64::MAX,
            1,
            None,
            Parts::default(),
        )]));
    });
    assert_eq!(
        sim.run(),
        Err(sim::Error::Panicked {
            thread: DIR.into(),
            message: "invariant: an entry of index \
                      00000000-0000-0000-0000-000000000001 on path Live starts at \
                      18446744073709551615 with 1 samples, past the last seq"
                .into(),
            seed: 11,
        })
    );
}

/// The ring file of a `Layout::fit` layout takes at most the given length, and less
/// than one block under it.
#[test]
fn an_open_of_a_fit_layout_makes_a_file_of_at_most_its_length() {
    let min = Layout::fit(0, BODY_MAX)
        .expect_err("no ring in no bytes")
        .min;
    for len in [min, min + 1, min + 3 * BLOCK - 1, (1 << 30) + BLOCK / 2] {
        run(187, move |shard| async move {
            let ring = Layout::fit(len, BODY_MAX).expect("the length holds a ring");
            let buffer = shard.open(ring, &mut Slots::new()).await;
            drop(buffer.expect("opens"));
            let file = shard.files.open(FilePath::new(RING), Mode::Read).await;
            let file = file.expect("the ring opens").len();
            assert!(file <= len, "a file of {file} bytes for {len}");
            assert!(file > len - BLOCK, "a file of {file} bytes for {len}");
        });
    }
}

/// A tagged entry of `Layout::ENTRY_MAX_MIN` bytes comes back from `newest` after a
/// reopen, and one byte more is large.
#[test]
fn newest_gives_a_tagged_entry_of_entry_max_min_after_a_reopen() {
    run(188, |shard| async move {
        let max = Layout::ENTRY_MAX_MIN;
        let ring = least(600_000);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let over = tagged(1, a, Path::Live, 0, 1, shard.block(max + 1));
        let large = Rejected::Large(Limit::Tagged { len: max + 1, max });
        assert_eq!(buffer.append([over]).map(drop), Err(large));
        buffer
            .append([tagged(1, a, Path::Live, 0, 1, shard.block(max))])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("reopens");
        let found = newest(&buffer, Path::Live, 1);
        let expected = vec![(slots.index(key(1)), kept(&shard.block(max)))];
        assert_eq!(found, expected);
    });
}

#[test]
fn a_file_of_only_the_header_blocks_is_read_for_its_length() {
    run(107, |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        let blocks = shard.bytes().await[..to_usize(AREA_START)].to_vec();
        let files = &shard.files;
        files.remove(FilePath::new(RING)).await.expect("removes");
        shard.create_zeroed(AREA_START).await;
        shard.put(0, &blocks).await;
        let opened = shard
            .open(layout(2 * AREA, BODY_MAX), &mut Slots::new())
            .await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Length {
                expected: AREA_START + AREA,
                found: AREA_START,
            })
        );
    });
}

#[test]
fn a_file_shorter_than_the_header_blocks_is_not_read() {
    run(17, |shard| async move {
        shard.create_zeroed(BLOCK).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Length {
                expected: AREA_START + AREA,
                found: BLOCK,
            })
        );
    });
}

#[test]
fn a_file_with_no_header_is_missing() {
    run(11, |shard| async move {
        shard.create_zeroed(AREA_START + AREA).await;
        shard.put(0, b"not a ring").await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Missing));
    });
}

/// Bytes past the first sector of a header block still make the file not a ring.
#[test]
fn a_file_with_bytes_past_the_first_sector_of_a_header_block_is_missing() {
    run(11, |shard| async move {
        shard.create_zeroed(AREA_START + AREA).await;
        shard.put(COVER, b"not a ring").await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Missing));
    });
}

/// A checkpoint is in the first sector of its block, so a crash leaves each block
/// whole or zero. One whole block opens the ring with what it holds, and the open
/// leaves that block as it is.
#[test]
fn a_ring_with_one_zero_header_block_opens_from_the_other() {
    let block = to_usize(BLOCK);
    for (lost, kept) in [(0, block), (block, 0)] {
        run(102, move |shard| async move {
            let ring = layout(AREA, BODY_MAX);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            buffer
                .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
                .expect("queues");
            buffer.committed().await.expect("commits");
            drop(buffer);
            shard.put(lost, &[0; SECTOR]).await;
            let before = shard.bytes().await;
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens again");
            let a = slots.index(key(1));
            assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(30)), "{lost}");
            let after = shard.bytes().await;
            assert_eq!(after[kept..kept + block], before[kept..kept + block]);
        });
    }
}

/// The first write of the header goes to both blocks in one write. A crash that
/// keeps neither first sector leaves two zero blocks, and the ring opens as new:
/// both blocks get the same first checkpoint.
#[test]
fn a_ring_whose_first_header_write_was_lost_opens_as_new() {
    run(102, |shard| async move {
        let block = to_usize(BLOCK);
        let ring = layout(AREA, BODY_MAX);
        drop(shard.open(ring, &mut Slots::new()).await.expect("opens"));
        for place in [0, block] {
            shard.put(place, &[0; SECTOR]).await;
        }
        let opened = shard.open(ring, &mut Slots::new()).await;
        assert_eq!(opened.map(|buffer| buffer.layout()), Ok(ring));
        let bytes = shard.bytes().await;
        assert_eq!(&bytes[..8], b"FNDNRING");
        assert_eq!(bytes[..block], bytes[block..2 * block]);
    });
}

/// A sim with `seed` and one node on it.
fn create_node(seed: u64) -> (sim::Sim, sim::node::Node) {
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    (sim, node)
}

/// Appends `batch`, and waits one commit span after each [`Rejected::Full`], at
/// most `spans` times. Gives the result of the last append.
async fn append_within(
    buffer: &Buffer,
    clock: &Clock,
    batch: &[Entry],
    spans: u32,
) -> Result<(), Rejected> {
    for _ in 0..spans {
        match buffer.append(batch.iter().cloned()) {
            Err(Rejected::Full { .. }) => clock.sleep(COMMIT).await,
            done => return done,
        }
    }
    buffer.append(batch.iter().cloned())
}

/// Starts a shard named `name` on `node` to run `main`.
fn on_node<F>(
    node: &sim::node::Node,
    name: &str,
    main: impl FnOnce(Tasks) -> F + Send + 'static,
) -> env::thread::Handle
where
    F: Future<Output = ()> + 'static,
{
    let config = env::shards::Config {
        name: name.into(),
        core: None,
    };
    node.shards().start(config, main).expect("the shard starts")
}

/// A buffer config for the ring in `dir` on the files of `node`.
fn node_config(node: &sim::node::Node, tasks: Tasks, dir: &str) -> Config {
    let config = block::Config::new(POOL).expect("the budget fits");
    let pool = Pool::new(config.clone(), Heap::new(config.reservation()));
    Config {
        files: node.files(),
        dir: PathBuf::from(dir),
        pool: Rc::new(pool),
        clock: node.clock(),
        tasks,
        entropy: node.entropy(),
        layout: layout(AREA, BODY_MAX),
        commit: COMMIT,
    }
}

/// Calls `at` with each seed of `seeds` and each cut from 0 ns in steps of `step`
/// ns, until `at` returns that the work it cuts had ended at the cut.
fn each_cut(seeds: Range<u64>, step: i64, mut at: impl FnMut(u64, i64) -> bool) {
    for seed in seeds {
        let mut cut = 0;
        while !at(seed, cut) {
            cut += step;
        }
    }
}

/// Starts an open of the ring on `node`, and stops the node with `crash` `cut`
/// nanoseconds into it. Returns whether the open had ended.
fn cut_an_open(
    sim: &mut sim::Sim,
    node: &sim::node::Node,
    cut: i64,
    crash: sim::Crash,
) -> bool {
    let ended = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ended);
    let own = node.clone();
    drop(on_node(node, "cut", move |tasks| async move {
        let config = node_config(&own, tasks, DIR);
        let _buffer = Buffer::open(config, &mut Slots::new())
            .await
            .expect("the open ends well");
        flag.store(true, Ordering::Relaxed);
        std::future::pending::<()>().await;
    }));
    sim.run_for(Span::from_nanos(cut)).expect("the run goes on");
    sim.crash(node, crash);
    ended.load(Ordering::Relaxed)
}

/// Starts the first open of a ring on a new node with `seed`, and stops the node
/// with `crash` `cut` nanoseconds into it. Returns the sim, the node, and whether
/// the open had ended.
fn cut_the_first_open(
    seed: u64,
    cut: i64,
    crash: sim::Crash,
) -> (sim::Sim, sim::node::Node, bool) {
    let (mut sim, node) = create_node(seed);
    let ended = cut_an_open(&mut sim, &node, cut, crash);
    (sim, node, ended)
}

/// Opens the ring in `dir` on `node`, commits one entry, cuts the power, and opens
/// the ring again. Returns the tail that the last open recovers.
fn commit_cut_and_recover(
    sim: &mut sim::Sim,
    node: &sim::node::Node,
    dir: &'static str,
) -> Tail {
    sim.run_on(node, move |node, tasks| async move {
        let mut slots = Slots::new();
        let config = node_config(&node, tasks, dir);
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
    })
    .expect("the commit ends");
    sim.crash(node, sim::Crash::Power);
    let recovered = sim.run_on(node, move |node, tasks| async move {
        let mut slots = Slots::new();
        let config = node_config(&node, tasks, dir);
        let buffer = Buffer::open(config, &mut slots).await.expect("opens again");
        buffer.tail(slots.index(key(1)), Path::Live)
    });
    recovered.expect("the last open ends")
}

/// A power cut at any point of the first open leaves a ring that opens again
/// with its layout. A cut while the first header write is in flight keeps any
/// set of its sectors.
#[test]
fn a_power_cut_during_the_first_open_leaves_a_ring_that_opens() {
    each_cut(0..64, 10_000, |seed, cut| {
        let (mut sim, node, ended) = cut_the_first_open(seed, cut, sim::Crash::Power);
        let opened = sim.run_on(&node, |node, tasks| async move {
            let config = node_config(&node, tasks, DIR);
            let buffer = Buffer::open(config, &mut Slots::new()).await;
            buffer.map(|buffer| buffer.layout())
        });
        let opened = opened.expect("the second open ends");
        assert_eq!(
            opened,
            Ok(layout(AREA, BODY_MAX)),
            "seed {seed}, cut at {cut} ns"
        );
        ended
    });
}

#[test]
fn a_power_cut_after_the_first_commit_keeps_the_committed_entries() {
    let (mut sim, node) = create_node(1);
    let recovered = commit_cut_and_recover(&mut sim, &node, DIR);
    assert_eq!(recovered, tail(3, Some(30)));
}

/// A kill can leave a ring that the first open made but did not make durable. The
/// next open finds it and makes it durable.
#[test]
fn a_kill_during_the_first_open_keeps_the_commits_of_the_next() {
    each_cut(0..16, 2_000, |seed, cut| {
        let (mut sim, node, ended) = cut_the_first_open(seed, cut, sim::Crash::Process);
        let recovered = commit_cut_and_recover(&mut sim, &node, DIR);
        assert_eq!(
            recovered,
            tail(3, Some(30)),
            "seed {seed}, kill at {cut} ns"
        );
        ended
    });
}

/// Makes a durable ring file of `len` zero bytes on `node`, as an open that stops
/// before its first checkpoint leaves it.
fn create_unwritten(sim: &mut sim::Sim, node: &sim::node::Node, len: u64) {
    let made = sim.run_on(node, move |node, _| async move {
        let (files, dir) = (node.files(), FilePath::new(DIR));
        files.create_dir(dir).await.expect("makes the directory");
        let ring = files.open(FilePath::new(RING), Mode::Create { len }).await;
        drop(ring.expect("makes the file"));
        let root = files.sync_dir(FilePath::new("")).await;
        root.expect("syncs the data directory");
        files.sync_dir(dir).await.expect("syncs the directory");
    });
    made.expect("the file is made");
}

/// What the ring file holds before an open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Found {
    /// No file.
    Absent,
    /// A file of this length with no checkpoint.
    Unwritten(u64),
    /// A file with a checkpoint.
    Written,
}

/// Opens the ring on `node` with `layout`. Returns what the file held before the
/// open, what the open gave, and the length of the file after it, 0 with no file.
fn open_with(
    sim: &mut sim::Sim,
    node: &sim::node::Node,
    layout: Layout,
) -> (Found, Result<Layout, Error>, u64) {
    let found = sim.run_on(node, move |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let (files, pool) = (node.files(), Rc::clone(&config.pool));
        let found = match files.open(FilePath::new(RING), Mode::Read).await {
            Ok(file) if file.len() < AREA_START => Found::Unwritten(file.len()),
            Ok(file) => {
                let blocks = pool.alloc(to_usize(AREA_START)).expect("a block");
                let blocks = file.read_at(0, blocks).await.expect("reads");
                let mut blocks = blocks.chunks(to_usize(BLOCK));
                if blocks.any(|block| block.starts_with(b"FNDNRING")) {
                    Found::Written
                } else {
                    Found::Unwritten(file.len())
                }
            }
            Err(FileError::NotFound { .. }) => Found::Absent,
            Err(error) => panic!("the ring file does not open: {error}"),
        };
        let config = Config { layout, ..config };
        let buffer = Buffer::open(config, &mut Slots::new()).await;
        let opened = buffer.map(|buffer| buffer.layout());
        let file = files.open(FilePath::new(RING), Mode::Read).await;
        (found, opened, file.map_or(0, |file| file.len()))
    });
    found.expect("the open ends")
}

/// An open that stops before its first checkpoint leaves a ring file with no
/// header, or an empty one. The next open makes the ring again with its layout.
#[test]
fn a_ring_with_no_checkpoint_takes_the_layout_of_the_open() {
    let other = layout(2 * AREA, 2 * BODY_MAX);
    for len in [0, AREA_START, AREA_START + AREA, AREA_START + AREA + BLOCK] {
        let (mut sim, node) = create_node(10);
        create_unwritten(&mut sim, &node, len);
        let found = open_with(&mut sim, &node, other);
        let made = (Found::Unwritten(len), Ok(other), AREA_START + 2 * AREA);
        assert_eq!(found, made);
    }
}

/// An open makes a ring with no checkpoint again. A crash at any point of it leaves
/// a ring that opens: with the layout of its checkpoint when it has one, or else
/// with the layout of that open. The cuts leave each state that the remake goes
/// through, and the file with no bytes that a crash in its create leaves.
#[test]
fn a_crash_while_a_ring_is_made_again_leaves_a_ring_that_opens() {
    let (old, new) = (layout(2 * AREA, BODY_MAX), layout(AREA, BODY_MAX));
    let lens = [old, new].map(|layout| AREA_START + layout.area());
    for crash in [sim::Crash::Process, sim::Crash::Power] {
        let mut left = BTreeSet::new();
        each_cut(0..8, 5_000, |seed, cut| {
            let mut ended = false;
            for layout in [old, new] {
                let (mut sim, node) = create_node(seed);
                create_unwritten(&mut sim, &node, lens[0]);
                ended = cut_an_open(&mut sim, &node, cut, crash);
                let (found, opened, len) = open_with(&mut sim, &node, layout);
                let at = format!("seed {seed}, {crash:?} at {cut} ns, {layout:?}");
                assert!(found == Found::Written || !ended, "{at}");
                let expected = if found == Found::Written { new } else { layout };
                assert_eq!(opened, Ok(expected), "{at}");
                assert_eq!(len, AREA_START + expected.area(), "{at}");
                left.insert(found);
            }
            ended
        });
        let zero = [0, lens[0], lens[1]].map(Found::Unwritten);
        let all = [Found::Absent, Found::Written].into_iter().chain(zero);
        assert_eq!(left, all.collect(), "{crash:?}");
    }
}

/// A sim with `seed` and one node on it, whose disk holds the shard directory and
/// `bytes` of files.
fn create_node_with_disk(seed: u64, bytes: u64) -> (sim::Sim, sim::node::Node) {
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config {
        disk_bytes: BLOCK + bytes,
        ..sim::node::Config::default()
    });
    (sim, node)
}

/// The disk gives the room of a removed file back when the removal is durable. So a
/// ring with no checkpoint is made again on a disk with no room for it and the new
/// ring at once.
#[test]
fn a_ring_with_no_checkpoint_is_made_again_in_the_room_that_it_leaves() {
    let new = layout(AREA, BODY_MAX);
    let len = AREA_START + AREA;
    for old in [len, len + AREA] {
        let (mut sim, node) = create_node_with_disk(10, old + len / 2);
        create_unwritten(&mut sim, &node, old);
        let found = open_with(&mut sim, &node, new);
        assert_eq!(found, (Found::Unwritten(old), Ok(new), len));
    }
}

/// A crash at any point of the first open, or of an open that makes a ring with no
/// checkpoint again, on a disk with room for one ring and not for two, leaves a ring
/// that opens. A kill after the remove leaves a file with no name that keeps its room.
#[test]
fn a_crash_in_an_open_leaves_a_ring_that_opens_in_the_room_of_one() {
    let new = layout(AREA, BODY_MAX);
    let len = AREA_START + AREA;
    for (crash, unwritten) in [
        (sim::Crash::Process, false),
        (sim::Crash::Process, true),
        (sim::Crash::Power, false),
        (sim::Crash::Power, true),
    ] {
        each_cut(0..8, 5_000, |seed, cut| {
            let (mut sim, node) = create_node_with_disk(seed, len + len / 2);
            if unwritten {
                create_unwritten(&mut sim, &node, len);
            }
            let ended = cut_an_open(&mut sim, &node, cut, crash);
            let opened = open_with(&mut sim, &node, new);
            let at = format!("seed {seed}, {crash:?} at {cut} ns, {unwritten}");
            assert_eq!((opened.1, opened.2), (Ok(new), len), "{at}");
            ended
        });
    }
}

/// A failed remove of a ring with no checkpoint, or a failed sync of its directory
/// after the remove, fails the open. The next open makes the ring again, also on a
/// disk with no room for both files.
#[test]
fn a_failed_remove_of_a_ring_with_no_checkpoint_fails_the_open() {
    let (other, len) = (layout(2 * AREA, BODY_MAX), AREA_START + AREA);
    let faults = [
        (RING, Operation::Remove, Found::Unwritten(len)),
        (DIR, Operation::SyncDir, Found::Absent),
    ];
    for (path, operation, left) in faults {
        let (mut sim, node) =
            create_node_with_disk(10, AREA_START + 2 * AREA + len / 2);
        create_unwritten(&mut sim, &node, len);
        node.fail_file(FilePath::new(path), operation);
        let opened = sim.run_on(&node, move |node, tasks| async move {
            let config = node_config(&node, tasks, DIR);
            let config = Config {
                layout: other,
                ..config
            };
            Buffer::open(config, &mut Slots::new()).await.map(drop)
        });
        let error = FileError::Io {
            path: PathBuf::from(path),
            operation,
            code: 5,
        };
        assert_eq!(opened.expect("the open ends"), Err(Error::Files(error)));
        let found = open_with(&mut sim, &node, other);
        assert_eq!(found, (left, Ok(other), AREA_START + 2 * AREA));
    }
}

/// Two opens at once of a ring with no checkpoint, on a disk with room for one ring:
/// one gets the ring and the other gets `Busy`. The entry that the first commits is
/// there after a kill and an open.
#[test]
fn of_two_opens_at_once_of_a_ring_with_no_checkpoint_one_gets_busy() {
    let len = AREA_START + AREA;
    for seed in 0..256 {
        let (mut sim, node) = create_node_with_disk(seed, len + len / 2);
        create_unwritten(&mut sim, &node, len);
        let results = [1, 2].map(|index| {
            let result = Arc::new(Mutex::new(None));
            let (own, shared) = (node.clone(), Arc::clone(&result));
            let name = format!("open-{index}");
            drop(on_node(&node, &name, move |tasks| async move {
                let give = |ended| *shared.lock().expect("no panic") = Some(ended);
                let mut slots = Slots::new();
                let config = node_config(&own, tasks, DIR);
                let buffer = match Buffer::open(config, &mut slots).await {
                    Ok(buffer) => buffer,
                    Err(error) => return give(Err(error)),
                };
                let slot = slots.index(key(index));
                let parts = Parts::default();
                buffer
                    .append([entry(index, slot, Path::Live, 0, 3, Some(30), parts)])
                    .expect("queues");
                give(buffer.committed().await.map_err(Error::Files));
                std::future::pending::<()>().await;
            }));
            result
        });
        sim.run_for(commits(4)).expect("the run goes on");
        let gave = results.map(|result| result.lock().expect("no panic").take());
        let one = [Some(Ok(())), Some(Err(busy()))];
        let other = [Some(Err(busy())), Some(Ok(()))];
        assert!(gave == one || gave == other, "seed {seed}: {gave:?}");
        sim.crash(&node, sim::Crash::Process);
        let recovered = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
                .await
                .expect("opens again");
            [1, 2].map(|index| {
                buffer.tail(slots.index(key(index)), Path::Live) == tail(3, Some(30))
            })
        });
        let committed = gave.map(|ended| ended == Some(Ok(())));
        assert_eq!(recovered, Ok(committed), "seed {seed}");
    }
}

/// Drops an open of a ring with no checkpoint `after` nanoseconds into it, opens the
/// ring again at once, commits one entry, kills the process, and opens the ring.
/// Returns whether the entry is there, or `None` when the first open had ended or
/// the second open or its commit failed.
fn drop_an_open_then_commit(seed: u64, after: i64) -> Option<bool> {
    let (mut sim, node) = create_node(seed);
    create_unwritten(&mut sim, &node, AREA_START + AREA);
    let committed = sim.run_on(&node, move |node, tasks| async move {
        let mut slots = Slots::new();
        let config = node_config(&node, tasks.clone(), DIR);
        let mut first = Box::pin(Buffer::open(config, &mut slots));
        let mut sleep = Box::pin(node.clock().sleep(Span::from_nanos(after)));
        let ended = poll_fn(|cx| {
            if first.as_mut().poll(cx).is_ready() {
                return Poll::Ready(true);
            }
            sleep.as_mut().poll(cx).map(|()| false)
        })
        .await;
        drop(first);
        if ended {
            return None;
        }
        let mut slots = Slots::new();
        let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots).await;
        let buffer = buffer.ok()?;
        let slot = slots.index(key(1));
        buffer
            .append([entry(1, slot, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.ok()
    });
    committed.expect("the run ends")?;
    sim.crash(&node, sim::Crash::Process);
    let recovered = sim.run_on(&node, |node, tasks| async move {
        let mut slots = Slots::new();
        let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
            .await
            .expect("opens again");
        buffer.tail(slots.index(key(1)), Path::Live)
    });
    Some(recovered.expect("the last open ends") == tail(3, Some(30)))
}

/// The next open waits for the remove of a dropped open, so a kill keeps the entry
/// that the next open committed. Each of the 6 runs drops the open while a remove of
/// its ring is in flight.
#[test]
fn a_dropped_open_keeps_the_ring_of_the_next_open() {
    let cases = [
        (143_161, 70_000),
        (150_046, 120_000),
        (189_172, 80_000),
        (231_911, 70_000),
        (275_938, 140_000),
        (359_697, 130_000),
    ];
    for (seed, after) in cases {
        let kept = drop_an_open_then_commit(seed, after);
        assert_eq!(kept, Some(true), "seed {seed}, drop at {after} ns");
    }
}

/// Two opens at once of a ring that is not there, on a new node with `seed`. The
/// first commits one entry with a commit span of 1 ns and drops its buffer. The second
/// starts `gap` nanoseconds later with `layout` and keeps its buffer. Then a kill and
/// an open. Returns what the first commit gave, what the second open gave (the tail
/// of the entry), and the tail after the kill.
fn commit_and_close_during_another_open(
    seed: u64,
    gap: i64,
    layout: Layout,
) -> (Result<(), Error>, Result<Tail, Error>, Tail) {
    let (mut sim, node) = create_node(seed);
    let committed = Arc::new(Mutex::new(None));
    let opened = Arc::new(Mutex::new(None));
    let (own, shared) = (node.clone(), Arc::clone(&committed));
    drop(on_node(&node, "first", move |tasks| async move {
        let mut slots = Slots::new();
        let config = Config {
            commit: Span::from_nanos(1),
            ..node_config(&own, tasks, DIR)
        };
        let gave = async {
            let buffer = Buffer::open(config, &mut slots).await?;
            let slot = slots.index(key(1));
            buffer
                .append([entry(1, slot, Path::Live, 0, 3, Some(30), Parts::default())])
                .expect("queues");
            buffer.committed().await.map_err(Error::Files)
        }
        .await;
        *shared.lock().expect("no panic") = Some(gave);
        std::future::pending::<()>().await;
    }));
    let (own, shared) = (node.clone(), Arc::clone(&opened));
    drop(on_node(&node, "second", move |tasks| async move {
        own.clock().sleep(Span::from_nanos(gap)).await;
        let mut slots = Slots::new();
        let config = Config {
            layout,
            ..node_config(&own, tasks, DIR)
        };
        let buffer = Buffer::open(config, &mut slots).await;
        let slot = slots.index(key(1));
        let gave = match &buffer {
            Ok(buffer) => Ok(buffer.tail(slot, Path::Live)),
            Err(error) => Err(error.clone()),
        };
        *shared.lock().expect("no panic") = Some(gave);
        std::future::pending::<()>().await;
        drop(buffer);
    }));
    sim.run_for(commits(4)).expect("the run goes on");
    let committed = committed.lock().expect("no panic").take();
    let opened = opened.lock().expect("no panic").take();
    let committed = committed.expect("the first commit ends");
    let opened = opened.expect("the second open ends");
    sim.crash(&node, sim::Crash::Process);
    let recovered = sim.run_on(&node, |node, tasks| async move {
        let mut slots = Slots::new();
        let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
            .await
            .expect("opens again");
        buffer.tail(slots.index(key(1)), Path::Live)
    });
    (committed, opened, recovered.expect("the last open ends"))
}

/// Each seed and gap of a run where an open makes the ring, commits, and closes
/// between the first look of a later open and its create.
///
/// The runs hang on the delay of each file call. After a change that moves those
/// delays, the later open of a run can get `Busy`, and the tests of these runs fail:
/// a search of the first 200,000 values of `seed` at the gaps of 100,000, 125,000,
/// and 150,000 ns then finds such runs again.
const CLOSED_BEFORE_THE_CREATE: [(u64, i64); 4] = [
    (32_511, 125_000),
    (199_239, 125_000),
    (35_100, 150_000),
    (93_218, 150_000),
];

/// An open that finds no ring makes one later, with `Mode::Create`. Another open can
/// make the ring, commit, and close in between. The later open then recovers the
/// entry, and the entry stays.
#[test]
fn an_open_at_once_with_one_that_commits_and_closes_keeps_the_commit() {
    let kept = tail(3, Some(30));
    for (seed, gap) in CLOSED_BEFORE_THE_CREATE {
        let same = layout(AREA, BODY_MAX);
        let found = commit_and_close_during_another_open(seed, gap, same);
        let expected = (Ok(()), Ok(kept), kept);
        assert_eq!(found, expected, "seed {seed}, gap {gap} ns");
    }
}

/// As the test before this one, with another layout for the later open. Its create
/// asks for its own length, so it fails with the `Length` of `env::files`, where an
/// open after it takes the layout of the ring. The entry stays.
#[test]
fn an_open_with_another_layout_at_once_with_one_that_closes_gets_length() {
    let kept = tail(3, Some(30));
    let length = Error::Files(FileError::Length {
        path: PathBuf::from(RING),
        expected: AREA_START + 2 * AREA,
        found: AREA_START + AREA,
    });
    for (seed, gap) in CLOSED_BEFORE_THE_CREATE {
        let other = layout(2 * AREA, BODY_MAX);
        let found = commit_and_close_during_another_open(seed, gap, other);
        let expected = (Ok(()), Err(length.clone()), kept);
        assert_eq!(found, expected, "seed {seed}, gap {gap} ns");
    }
}

/// Three opens at once of a directory with no ring, on a disk of `disk` bytes. The
/// sync of the root fails for the open that makes the ring, so its ring has no
/// checkpoint. An open that passes commits one entry. Returns what each open gave,
/// and the tail that an open after a kill recovers.
fn three_opens_and_a_failed_sync(
    seed: u64,
    disk: u64,
) -> ([Result<(), Error>; 3], Tail) {
    let (mut sim, node) = create_node_with_disk(seed, disk);
    node.fail_file(FilePath::new(""), Operation::SyncDir);
    let results = [200_000, 75_000, 35_000].map(|gap| {
        let result = Arc::new(Mutex::new(None));
        let (own, shared) = (node.clone(), Arc::clone(&result));
        drop(on_node(
            &node,
            &format!("open-{gap}"),
            move |tasks| async move {
                own.clock().sleep(Span::from_nanos(gap)).await;
                let mut slots = Slots::new();
                let gave = async {
                    let config = node_config(&own, tasks, DIR);
                    let buffer = Buffer::open(config, &mut slots).await?;
                    let slot = slots.index(key(1));
                    let parts = Parts::default();
                    buffer
                        .append([entry(1, slot, Path::Live, 0, 3, Some(30), parts)])
                        .expect("queues");
                    buffer.committed().await.map_err(Error::Files)
                }
                .await;
                *shared.lock().expect("no panic") = Some(gave);
            },
        ));
        result
    });
    sim.run_for(commits(4)).expect("the run goes on");
    let results = results.map(|result| result.lock().expect("no panic").take());
    sim.crash(&node, sim::Crash::Process);
    let recovered = sim.run_on(&node, |node, tasks| async move {
        let mut slots = Slots::new();
        let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
            .await
            .expect("opens again");
        buffer.tail(slots.index(key(1)), Path::Live)
    });
    (
        results.map(|result| result.expect("each open ends")),
        recovered.expect("the last open ends"),
    )
}

/// A limit of opens at once. The first open removes the ring with no checkpoint that
/// the third left. The second found no ring before, and its create runs before the
/// directory sync of the first, while the removed ring keeps its room. `Files::open`
/// then syncs the directory and creates again, so a disk with room for one ring and a
/// half gives what a disk with room for two gives. The commit of the open that passes
/// stays.
///
/// The run hangs on the delay of each file call. After a change that moves those
/// delays, the second open can get another result, and this test fails: a search of
/// the first 20,000 values of `seed` then finds such runs again.
#[test]
fn an_open_at_once_with_a_remove_of_another_open_gets_the_room_of_the_ring() {
    let len = AREA_START + AREA;
    let failed = Error::Files(FileError::Io {
        path: PathBuf::new(),
        operation: Operation::SyncDir,
        code: 5,
    });
    let expected = ([Err(busy()), Ok(()), Err(failed)], tail(3, Some(30)));
    assert_eq!(three_opens_and_a_failed_sync(218, len + len / 2), expected);
    assert_eq!(three_opens_and_a_failed_sync(218, 2 * len), expected);
}

/// A failed read of the header blocks fails the open with its error and leaves the
/// ring: the next open makes a ring that was not there, and recovers the entry of a
/// ring with a checkpoint.
#[test]
fn a_failed_read_of_the_header_blocks_fails_the_open_and_keeps_the_ring() {
    let failed = Error::Files(FileError::Io {
        path: PathBuf::from(RING),
        operation: Operation::ReadAt,
        code: 5,
    });
    for committed in [false, true] {
        let (mut sim, node) = create_node(11);
        if committed {
            sim.run_on(&node, |node, tasks| async move {
                let mut slots = Slots::new();
                let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
                    .await
                    .expect("opens");
                let slot = slots.index(key(1));
                let parts = Parts::default();
                buffer
                    .append([entry(1, slot, Path::Live, 0, 3, Some(30), parts)])
                    .expect("queues");
                buffer.committed().await.expect("commits");
            })
            .expect("the run ends");
            sim.crash(&node, sim::Crash::Process);
        }
        node.fail_file(FilePath::new(RING), Operation::ReadAt);
        let opened = sim.run_on(&node, |node, tasks| async move {
            let config = node_config(&node, tasks, DIR);
            Buffer::open(config, &mut Slots::new()).await.map(drop)
        });
        assert_eq!(opened, Ok(Err(failed.clone())), "committed: {committed}");
        let recovered = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
                .await
                .expect("opens again");
            buffer.tail(slots.index(key(1)), Path::Live)
        });
        let kept = if committed {
            tail(3, Some(30))
        } else {
            tail(0, None)
        };
        assert_eq!(recovered, Ok(kept), "committed: {committed}");
    }
}

/// Starts a new ring on a node with `seed`, appends one entry, and kills the
/// process `cut` nanoseconds after a point at most 10 µs before the deadline of the
/// first commit. Returns the sim, the node, and whether the commit had ended.
fn kill_the_first_commit(seed: u64, cut: i64) -> (sim::Sim, sim::node::Node, bool) {
    let (mut sim, node) = create_node(seed);
    let opened = Arc::new(AtomicBool::new(false));
    let committed = Arc::new(AtomicBool::new(false));
    let (open, commit) = (Arc::clone(&opened), Arc::clone(&committed));
    let first = node.clone();
    drop(on_node(&node, "first", move |tasks| async move {
        let mut slots = Slots::new();
        let config = node_config(&first, tasks, DIR);
        let buffer = Buffer::open(config, &mut slots)
            .await
            .expect("the first open ends well");
        open.store(true, Ordering::Relaxed);
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        commit.store(true, Ordering::Relaxed);
        std::future::pending::<()>().await;
    }));
    let step = 10_000;
    while !opened.load(Ordering::Relaxed) {
        sim.run_for(Span::from_nanos(step))
            .expect("the run goes on");
    }
    let rest = Span::from_nanos(COMMIT.nanos() - step + cut);
    sim.run_for(rest).expect("the run goes on");
    sim.crash(&node, sim::Crash::Process);
    (sim, node, committed.load(Ordering::Relaxed))
}

/// A process that opens a ring after a kill at any point of the first commit
/// reports durable only what a power cut then keeps.
#[test]
fn a_tail_reported_durable_after_a_kill_survives_a_power_cut() {
    each_cut(0..32, 10_000, |seed, cut| {
        let (mut sim, node, ended) = kill_the_first_commit(seed, cut);
        assert!(
            cut != 0 || !ended,
            "seed {seed}: the kill missed the commit"
        );
        let reported = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
                .await
                .expect("opens after the kill");
            buffer.durable(slots.index(key(1)), Path::Live)
        });
        let reported = reported.unwrap_or_else(|e| panic!("cut at {cut} ns: {e}"));
        sim.crash(&node, sim::Crash::Power);
        let recovered = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
                .await
                .expect("opens after the power cut");
            buffer.tail(slots.index(key(1)), Path::Live)
        });
        let recovered = recovered.unwrap_or_else(|e| panic!("cut at {cut} ns: {e}"));
        assert_eq!(recovered, reported, "seed {seed}, cut at {cut} ns");
        ended
    });
}

/// The error of an open of the ring while another handle holds it.
fn busy() -> Error {
    Error::Files(FileError::Busy {
        path: PathBuf::from(RING),
    })
}

/// The commit task holds the ring until it ends, so an open right after a drop fails
/// with `Busy`. The task writes the entry queued at the drop, then ends, and the open
/// after it recovers the entry.
#[test]
fn an_open_right_after_a_drop_fails_with_busy_until_the_task_ended() {
    let (mut sim, node) = create_node(41);
    let run = sim.run_on(&node, |node, tasks| async move {
        let config = || node_config(&node, tasks.clone(), DIR);
        let mut slots = Slots::new();
        let first = Buffer::open(config(), &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        first
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        drop(first);
        let held = Buffer::open(config(), &mut slots).await.map(drop);
        assert_eq!(held, Err(busy()));
        node.clock().sleep(commits(20)).await;
        let buffer = Buffer::open(config(), &mut slots).await.expect("reopens");
        buffer.durable(a, Path::Live)
    });
    assert_eq!(run, Ok(tail(1, Some(1))));
}

/// A `Commit` held past the drop holds the ring after the task ended, so an open
/// fails with `Busy` until the commit drops.
#[test]
fn an_open_while_a_commit_of_a_dropped_buffer_is_held_fails_with_busy() {
    let (mut sim, node) = create_node(41);
    let run = sim.run_on(&node, |node, tasks| async move {
        let config = || node_config(&node, tasks.clone(), DIR);
        let mut slots = Slots::new();
        let first = Buffer::open(config(), &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        first
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        let held = first.committed();
        let ending = first.committed();
        drop(first);
        ending
            .await
            .expect("the task writes the queue before it ends");
        let busy_open = Buffer::open(config(), &mut slots).await.map(drop);
        assert_eq!(busy_open, Err(busy()));
        drop(held);
        let buffer = Buffer::open(config(), &mut slots).await.expect("reopens");
        buffer.durable(a, Path::Live)
    });
    assert_eq!(run, Ok(tail(1, Some(1))));
}

/// A failed sync leaves the record of entry 2 clean in the cache and not durable. A
/// reopen in the same process reports it durable and commits entry 3 after it, so a
/// power cut keeps both.
#[test]
fn a_reopen_after_a_failed_sync_keeps_what_it_reports_across_a_power_cut() {
    for seed in 0..16 {
        let (mut sim, node) = create_node(seed);
        let reported = sim.run_on(&node, |node, tasks| async move {
            let config = || node_config(&node, tasks.clone(), DIR);
            let mut slots = Slots::new();
            let first = Buffer::open(config(), &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            first
                .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
                .expect("queues");
            first.committed().await.expect("commits");
            node.fail_file(FilePath::new(RING), Operation::Sync);
            first
                .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
                .expect("queues");
            let failed = FileError::Io {
                path: PathBuf::from(RING),
                operation: Operation::Sync,
                code: 5,
            };
            assert_eq!(first.committed().await, Err(failed));
            drop(first);
            let second = Buffer::open(config(), &mut slots).await.expect("reopens");
            second
                .append([entry(1, a, Path::Live, 2, 1, Some(3), Parts::default())])
                .expect("queues");
            second.committed().await.expect("commits");
            second.durable(a, Path::Live)
        });
        assert_eq!(reported, Ok(tail(3, Some(3))), "seed {seed}");
        sim.crash(&node, sim::Crash::Power);
        let recovered = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
                .await
                .expect("opens after the power cut");
            buffer.tail(slots.index(key(1)), Path::Live)
        });
        assert_eq!(recovered, Ok(tail(3, Some(3))), "seed {seed}");
    }
}

/// A ring of 64 blocks with records of up to 60,000 bytes, on the files of `node`.
fn long_config(node: &sim::node::Node, tasks: Tasks) -> Config {
    Config {
        layout: layout(64 * BLOCK, 60_000),
        ..node_config(node, tasks, DIR)
    }
}

/// Commits one entry of `len` bytes, zero except at `marked`, on a new ring on a
/// node with `seed` while the commit's sync fails. The process dies, a new one opens
/// the ring and reports what is durable, the power is cut, and a last open recovers.
/// Returns what the second open reported and what the last one recovered.
fn fail_a_sync_and_cut(seed: u64, len: usize, marked: Range<usize>) -> (Tail, Tail) {
    let (mut sim, node) = create_node(seed);
    let failed = sim.run_on(&node, move |node, tasks| async move {
        let config = long_config(&node, tasks);
        let pool = Rc::clone(&config.pool);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        node.fail_file(FilePath::new(RING), Operation::Sync);
        let mut bytes = pool.alloc(len).expect("a block");
        bytes[marked].fill(0xab);
        let bytes = bytes.freeze();
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::from(bytes))])
            .expect("queues");
        buffer.committed().await
    });
    let sync = FileError::Io {
        path: PathBuf::from(RING),
        operation: Operation::Sync,
        code: 5,
    };
    assert_eq!(failed, Ok(Err(sync)), "seed {seed}");
    sim.crash(&node, sim::Crash::Process);
    let reported = sim.run_on(&node, |node, tasks| async move {
        let mut slots = Slots::new();
        let buffer = Buffer::open(long_config(&node, tasks), &mut slots)
            .await
            .expect("opens after the failed sync");
        buffer.durable(slots.index(key(1)), Path::Live)
    });
    let reported = reported.unwrap_or_else(|e| panic!("seed {seed}: {e}"));
    sim.crash(&node, sim::Crash::Power);
    let recovered = sim.run_on(&node, |node, tasks| async move {
        let mut slots = Slots::new();
        let buffer = Buffer::open(long_config(&node, tasks), &mut slots)
            .await
            .expect("opens after the power cut");
        buffer.tail(slots.index(key(1)), Path::Live)
    });
    let recovered = recovered.unwrap_or_else(|e| panic!("seed {seed}: {e}"));
    (reported, recovered)
}

/// A process that opens a ring after a commit whose sync failed, in the same boot,
/// reports durable only what a power cut then keeps. The failed sync can leave a
/// record in the cache only, where the open's walk sees it.
#[test]
fn an_open_after_a_failed_sync_in_the_same_boot_reports_only_disk_records_durable() {
    for seed in 0..32 {
        let (reported, recovered) = fail_a_sync_and_cut(seed, 0, 0..0);
        assert_eq!(recovered, reported, "seed {seed}");
    }
}

/// As above, for a record of three blocks, which the walk reads in pieces and then
/// reads its start again. Only the entry's bytes in the third block are not zero:
/// a lost sector that reads as zeros on a new ring loses nothing, and many lost
/// sectors rarely all stay in the cache.
#[test]
fn an_open_after_a_failed_sync_of_a_long_record_reports_only_disk_records_durable() {
    for seed in 0..32 {
        let (reported, recovered) = fail_a_sync_and_cut(seed, 8_300, 8_128..8_300);
        assert_eq!(recovered, reported, "seed {seed}");
    }
}

/// A failed write of the bytes an open read fails the open with the write's error.
#[test]
fn a_failed_write_of_the_read_bytes_fails_the_open() {
    let (mut sim, node) = create_node(7);
    let first = sim.run_on(&node, |node, tasks| async move {
        let mut slots = Slots::new();
        let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await
    });
    assert_eq!(first, Ok(Ok(())));
    sim.crash(&node, sim::Crash::Process);
    let opened = sim.run_on(&node, |node, tasks| async move {
        node.fail_file(FilePath::new(RING), Operation::WriteAt);
        let config = node_config(&node, tasks, DIR);
        Buffer::open(config, &mut Slots::new()).await.map(drop)
    });
    let failed = FileError::Io {
        path: PathBuf::from(RING),
        operation: Operation::WriteAt,
        code: 5,
    };
    assert_eq!(opened, Ok(Err(Error::Files(failed))));
}

/// A process that opens a new ring whose first header sync failed, in the same
/// boot, reports durable only what a power cut then keeps. The failed sync can
/// leave the header in the cache only, where the open reads it.
#[test]
fn an_open_after_a_failed_sync_of_the_first_header_reports_only_disk_records_durable() {
    for seed in 0..32 {
        let (mut sim, node) = create_node(seed);
        let failed = sim.run_on(&node, |node, tasks| async move {
            node.fail_file(FilePath::new(RING), Operation::Sync);
            let config = node_config(&node, tasks, DIR);
            Buffer::open(config, &mut Slots::new()).await.map(drop)
        });
        let sync = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(failed, Ok(Err(Error::Files(sync))), "seed {seed}");
        sim.crash(&node, sim::Crash::Process);
        let reported = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
                .await
                .expect("opens after the failed sync");
            let a = slots.index(key(1));
            buffer
                .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
                .expect("queues");
            buffer.committed().await.expect("commits");
            buffer.durable(a, Path::Live)
        });
        let reported = reported.unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        sim.crash(&node, sim::Crash::Power);
        let recovered = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
                .await
                .expect("opens after the power cut");
            buffer.tail(slots.index(key(1)), Path::Live)
        });
        let recovered = recovered.unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(recovered, reported, "seed {seed}");
    }
}

/// A power cut at any point of an open whose restart record goes over the one of
/// an open with no data keeps the committed entry. The ring then takes the next
/// entry, and it survives a power cut.
#[test]
fn a_power_cut_during_a_restart_over_an_old_one_keeps_the_entries() {
    each_cut(0..32, 10_000, |seed, cut| {
        let (mut sim, node) = create_node(seed);
        sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let config = node_config(&node, tasks.clone(), DIR);
            let buffer = Buffer::open(config, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            buffer
                .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
                .expect("queues");
            buffer.committed().await.expect("commits");
            drop(buffer);
            let config = node_config(&node, tasks, DIR);
            drop(
                Buffer::open(config, &mut Slots::new())
                    .await
                    .expect("reopens"),
            );
        })
        .expect("the run ends");
        let ended = cut_an_open(&mut sim, &node, cut, sim::Crash::Power);
        let recovered = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let config = node_config(&node, tasks, DIR);
            let buffer = Buffer::open(config, &mut slots).await?;
            let a = slots.index(key(1));
            let recovered = buffer.tail(a, Path::Live);
            buffer
                .append([entry(1, a, Path::Live, 3, 2, Some(50), Parts::default())])
                .expect("queues");
            buffer.committed().await?;
            Ok::<_, Error>(recovered)
        });
        let recovered = recovered.unwrap_or_else(|e| panic!("cut at {cut} ns: {e}"));
        assert_eq!(
            recovered,
            Ok(tail(3, Some(30))),
            "seed {seed}, cut at {cut} ns"
        );
        sim.crash(&node, sim::Crash::Power);
        let last = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let config = node_config(&node, tasks, DIR);
            let buffer = Buffer::open(config, &mut slots).await;
            buffer.map(|buffer| buffer.tail(slots.index(key(1)), Path::Live))
        });
        let last = last.unwrap_or_else(|e| panic!("cut at {cut} ns: {e}"));
        assert_eq!(last, Ok(tail(5, Some(50))), "seed {seed}, cut at {cut} ns");
        ended
    });
}

/// A failed sync of the ring's directory or of its parent fails the open. The next
/// open makes a durable ring.
#[test]
fn a_failed_directory_sync_fails_the_open_and_the_next_one_keeps_its_commits() {
    for dir in ["", DIR] {
        let (mut sim, node) = create_node(1);
        node.fail_file(FilePath::new(dir), Operation::SyncDir);
        sim.run_on(&node, move |node, tasks| async move {
            let config = node_config(&node, tasks, DIR);
            let opened = Buffer::open(config, &mut Slots::new()).await;
            let error = FileError::Io {
                path: PathBuf::from(dir),
                operation: Operation::SyncDir,
                code: 5,
            };
            assert_eq!(opened.map(drop), Err(Error::Files(error)));
        })
        .expect("the first open ends");
        let recovered = commit_cut_and_recover(&mut sim, &node, DIR);
        assert_eq!(recovered, tail(3, Some(30)), "{dir:?}");
    }
}

/// A failed sync of the ring's directory at any point of a first open fails the open,
/// before the create and after it. The next open makes the ring.
#[test]
fn a_failed_directory_sync_at_any_point_of_the_open_fails_it() {
    let (new, len) = (layout(AREA, BODY_MAX), AREA_START + AREA);
    let mut left = BTreeSet::new();
    for at in (0..).step_by(5_000) {
        let (mut sim, node) = create_node(1);
        let result = Arc::new(Mutex::new(None));
        let (own, shared) = (node.clone(), Arc::clone(&result));
        drop(on_node(&node, "open", move |tasks| async move {
            let config = node_config(&own, tasks, DIR);
            let opened = Buffer::open(config, &mut Slots::new()).await.map(drop);
            *shared.lock().expect("no panic") = Some(opened);
            std::future::pending::<()>().await;
        }));
        sim.run_for(Span::from_nanos(at)).expect("the run goes on");
        node.fail_file(FilePath::new(DIR), Operation::SyncDir);
        sim.run_for(commits(2)).expect("the run goes on");
        let opened = result.lock().expect("no panic").take();
        if opened == Some(Ok(())) {
            break;
        }
        let error = FileError::Io {
            path: PathBuf::from(DIR),
            operation: Operation::SyncDir,
            code: 5,
        };
        assert_eq!(opened, Some(Err(Error::Files(error))), "fault at {at} ns");
        sim.crash(&node, sim::Crash::Process);
        let found = open_with(&mut sim, &node, new);
        assert_eq!((found.1, found.2), (Ok(new), len), "fault at {at} ns");
        left.insert(found.0);
    }
    assert_eq!(left, BTreeSet::from([Found::Absent, Found::Unwritten(len)]));
}

/// The open syncs the parent of a nested ring directory, not the data directory.
#[test]
fn a_ring_in_a_nested_directory_keeps_its_commits_across_a_power_cut() {
    let (mut sim, node) = create_node(1);
    sim.run_on(&node, |node, _tasks| async move {
        let files = node.files();
        files
            .create_dir(FilePath::new("a"))
            .await
            .expect("the dir is made");
        files
            .sync_dir(FilePath::new(""))
            .await
            .expect("the dir is durable");
    })
    .expect("the parent is made");
    let recovered = commit_cut_and_recover(&mut sim, &node, "a/shard-0");
    assert_eq!(recovered, tail(3, Some(30)));
}

#[test]
fn two_header_blocks_with_a_wrong_crc_are_damaged() {
    run(12, |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        shard.put(BODY_MAX_AT, &[1]).await;
        shard.put(to_usize(BLOCK) + BODY_MAX_AT, &[1]).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Damaged));
    });
}

#[test]
fn another_format_version_is_not_read() {
    run(13, |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        shard.tamper(VERSION_AT, &2_u16.to_le_bytes()).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Err(Error::Version(2)));
    });
}

#[test]
fn a_header_with_sizes_that_make_no_ring_is_unfit() {
    run(14, |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        shard.tamper(BODY_MAX_AT, &0_u32.to_le_bytes()).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Unfit(Unfit {
                area: AREA,
                body_max: 0
            }))
        );
    });
}

/// A header whose `body_max` is under one block less the record header makes no
/// ring.
#[test]
fn a_header_with_a_body_under_one_block_is_unfit() {
    run(28, |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        let body_max = BODY_MAX - 1;
        let small = u32::try_from(body_max).expect("a small size");
        shard.tamper(BODY_MAX_AT, &small.to_le_bytes()).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        let unfit = Unfit {
            area: AREA,
            body_max,
        };
        assert_eq!(opened.map(drop), Err(Error::Unfit(unfit)));
    });
}

#[test]
fn a_record_whose_entry_cannot_be_read_is_invalid() {
    run(18, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        shard.tamper_record(BLOCK, 4 + 16, &[2]).await;
        shard.open_invalid(layout(AREA, BODY_MAX), BLOCK).await;
    });
}

/// A whole record of 1023 entries opens, and one of 1024 is a wrong shape.
#[test]
fn a_record_over_the_most_entries_is_invalid() {
    for (count, opens) in [(1023_u32, true), (1024, false)] {
        run(102, move |shard| async move {
            let ring = least(100_000);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            let len = 4 + 51 * to_usize(count.into());
            let parts = Parts::from(shard.block(len - 55));
            buffer
                .append([entry(1, a, Path::Live, 0, 1, Some(1), parts)])
                .expect("queues");
            buffer.committed().await.expect("commits");
            drop(buffer);
            let mut body = count.to_le_bytes().to_vec();
            for first in 0..u64::from(count) {
                let last = i64::try_from(first + 1).expect("fits");
                body.extend_from_slice(&1_u128.to_le_bytes());
                body.push(0);
                body.extend_from_slice(&first.to_le_bytes());
                body.extend_from_slice(&1_u32.to_le_bytes());
                body.extend_from_slice(&7_i64.to_le_bytes());
                body.push(1);
                body.extend_from_slice(&last.to_le_bytes());
                body.push(0);
                body.extend_from_slice(&0_u32.to_le_bytes());
            }
            assert_eq!(body.len(), len);
            shard.tamper_record(BLOCK, 0, &body).await;
            if !opens {
                return shard.open_invalid(ring, BLOCK).await;
            }
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let tails = buffer.tail(slots.index(key(1)), Path::Live);
            assert_eq!(
                tails,
                tail(count.into(), Some(count.into())),
                "{count} entries"
            );
        });
    }
}

#[test]
fn a_record_with_an_entry_past_the_last_seq_is_invalid() {
    run(103, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        // `first` of the first entry: after the count, the index, and the path.
        shard
            .tamper_record(BLOCK, 4 + 16 + 1, &u64::MAX.to_le_bytes())
            .await;
        shard.open_invalid(layout(AREA, BODY_MAX), BLOCK).await;
    });
}

#[test]
fn a_record_with_an_entry_below_the_tail_is_invalid() {
    run(106, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([
                entry(1, a, Path::Live, 0, 3, Some(30), Parts::default()),
                entry(1, a, Path::Live, 3, 2, Some(50), Parts::default()),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        // `first` of the second entry: after the count, one table of 51 bytes,
        // the index, and the path.
        shard
            .tamper_record(BLOCK, 4 + 51 + 16 + 1, &1u64.to_le_bytes())
            .await;
        shard.open_invalid(layout(AREA, BODY_MAX), BLOCK).await;
    });
}

/// The open writes the header blocks and the records before the invalid one again.
#[test]
fn an_open_that_finds_an_invalid_record_leaves_the_ring_as_read() {
    run(107, |shard| async move {
        shard.create_two_records().await;
        shard.tamper_record(2 * BLOCK, 4 + 16, &[2]).await;
        shard.open_invalid(layout(AREA, BODY_MAX), 2 * BLOCK).await;
    });
}

/// One header block is zero, as a crash leaves it. The open does not repair it.
#[test]
fn an_open_that_finds_an_invalid_record_leaves_a_zero_header_block_as_read() {
    for lost in [0, to_usize(BLOCK)] {
        run(110, move |shard| async move {
            shard.create_two_records().await;
            shard.put(lost, &[0; SECTOR]).await;
            shard.tamper_record(2 * BLOCK, 4 + 16, &[2]).await;
            let invalid = Error::Invalid { offset: 2 * BLOCK };
            let case = format!("the zero block at {lost}");
            shard
                .open_refused(layout(AREA, BODY_MAX), invalid, &case)
                .await;
        });
    }
}

/// A checkpoint is in the first sector of its block. The open keeps the other bytes.
#[test]
fn an_open_that_finds_an_invalid_record_leaves_bytes_past_the_first_sector() {
    run(111, |shard| async move {
        shard.create_two_records().await;
        for block in [0, to_usize(BLOCK)] {
            shard.put(block + COVER, b"past the sector").await;
        }
        shard.tamper_record(2 * BLOCK, 4 + 16, &[2]).await;
        shard.open_invalid(layout(AREA, BODY_MAX), 2 * BLOCK).await;
    });
}

/// The first record follows the tail chain of the header, and a zero header block
/// holds another one.
#[test]
#[should_panic(expected = "the header blocks differ before the seq")]
fn seal_refuses_the_first_record_under_two_tail_chains() {
    run(112, |shard| async move {
        shard.create_two_records().await;
        shard.put(0, &[0; SECTOR]).await;
        shard.seal(0).await;
    });
}

/// Two whole header blocks can hold two tail chains, and the open takes one of them.
#[test]
#[should_panic(expected = "the header blocks differ before the seq")]
fn seal_refuses_the_first_record_under_the_tail_chains_of_two_whole_blocks() {
    run(121, |shard| async move {
        shard.create_two_records().await;
        let last = SEQ_AT - 1;
        let chain = shard.bytes().await[last] ^ 1;
        shard.tamper_block(to_usize(BLOCK), last, &[chain]).await;
        shard.seal(0).await;
    });
}

/// Two zero header blocks hold no tail chain: the open draws a new one.
#[test]
#[should_panic(expected = "the ring has no header")]
fn seal_refuses_the_first_record_of_a_ring_with_no_header() {
    run(120, |shard| async move {
        shard.create_two_records().await;
        for block in [0, to_usize(BLOCK)] {
            shard.put(block, &[0; SECTOR]).await;
        }
        shard.seal(0).await;
    });
}

/// The offset of a record can be 0, so a header block has its own error.
#[test]
fn a_record_of_an_unknown_kind_at_the_start_of_the_ring_is_invalid() {
    run(118, |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        shard.put(to_usize(AREA_START) + 8, &[5]).await;
        shard.seal(0).await;
        shard.open_invalid(layout(AREA, BODY_MAX), 0).await;
    });
}

/// With a tail past 0, the record before in the file can be a newer record.
#[test]
#[should_panic(expected = "the tail offset of the ring is not 0")]
fn seal_refuses_a_ring_with_a_moved_tail() {
    run(117, |shard| async move {
        shard.create_two_records().await;
        shard.tamper(TAIL_AT, &(2 * BLOCK).to_le_bytes()).await;
        shard.seal(2 * BLOCK).await;
    });
}

#[test]
#[should_panic(expected = "no record starts at 8192")]
fn seal_refuses_a_block_inside_a_record() {
    run(115, |shard| async move {
        shard.create_long_record().await;
        shard.seal(2 * BLOCK).await;
    });
}

#[test]
#[should_panic(expected = "no chain value of kind 0 before 16384")]
fn seal_refuses_a_record_after_a_block_that_is_no_record() {
    run(116, |shard| async move {
        shard.create_two_records().await;
        shard.seal(4 * BLOCK).await;
    });
}

/// The record before the invalid one has two blocks, and a byte of its body in the
/// second block reads as the kind of a data record.
#[test]
fn a_record_of_an_unknown_kind_after_a_long_record_is_invalid() {
    run(114, |shard| async move {
        let ring = shard.create_long_record().await;
        let kind = to_usize(AREA_START + 3 * BLOCK) + 8;
        assert_eq!(shard.bytes().await[kind - to_usize(BLOCK)], DATA);
        shard.put(kind, &[5]).await;
        shard.seal(3 * BLOCK).await;
        shard.open_invalid(ring, 3 * BLOCK).await;
    });
}

#[test]
fn a_record_of_an_unknown_kind_is_invalid() {
    run(109, |shard| async move {
        shard.create_two_records().await;
        let kind = to_usize(AREA_START + 2 * BLOCK) + 8;
        shard.put(kind, &[5]).await;
        shard.seal(2 * BLOCK).await;
        shard.open_invalid(layout(AREA, BODY_MAX), 2 * BLOCK).await;
    });
}

/// A zeroed block has kind 0, so kind 0 ends the walk for each chain value.
#[test]
fn a_block_of_kind_zero_that_follows_the_chain_ends_the_walk() {
    run(113, |shard| async move {
        shard.create_two_records().await;
        let kind = to_usize(AREA_START + 2 * BLOCK) + 8;
        shard.put(kind, &[0]).await;
        shard.seal(2 * BLOCK).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Ok(()));
        assert_eq!(shard.bytes().await[kind], RESTART);
    });
}

/// A trim passed the only record of index 1, and a carry record after the record
/// of index 2 holds its tail. Each open gives that tail, a read gives the seqs
/// before it as a gap, and the next entry of index 1 goes on from it.
#[test]
fn an_open_gives_the_carried_tail_of_a_path_with_no_record() {
    run(158, |shard| async move {
        let ring = layout(AREA, BODY_MAX);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let (a, b) = (slots.index(key(1)), slots.index(key(2)));
        for appended in [
            entry(1, a, Path::Live, 0, 3, Some(30), Parts::default()),
            entry(2, b, Path::Live, 0, 2, Some(40), shard.block(20).into()),
        ] {
            buffer.append([appended]).expect("queues");
            buffer.committed().await.expect("commits");
        }
        drop(buffer);
        shard
            .put_carry(3 * BLOCK, &carry_body(&[carried(1, 3, 0, Some(30))]))
            .await;
        let chain = to_usize(AREA_START + BLOCK) + 4;
        let chain = shard.bytes().await[chain..chain + 4].to_vec();
        shard.tamper(TAIL_AT, &(2 * BLOCK).to_le_bytes()).await;
        shard.tamper(CHAIN_AT, &chain).await;
        let hidden = Read {
            gap: Some(0..3),
            entries: vec![],
            next: mark(3, 0),
        };
        for open in ["the first open", "the second open"] {
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let (a, b) = (slots.index(key(1)), slots.index(key(2)));
            assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(30)), "{open}");
            assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)), "{open}");
            let read = buffer
                .reader()
                .read(a, Path::Live, mark(0, 0), usize::MAX)
                .await;
            assert_eq!(read, Ok(hidden.clone()), "{open}");
            let read = buffer
                .reader()
                .read(b, Path::Live, mark(0, 0), usize::MAX)
                .await;
            let entries = vec![stored(0, 2, Some(40), shard.block(20))];
            assert_eq!(read, Ok(whole(entries, mark(2, 0))), "{open}");
        }
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let next = entry(1, a, Path::Live, 3, 3, None, shard.block(10).into());
        buffer.append([next]).expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        assert_eq!(buffer.durable(a, Path::Live), tail(6, Some(30)));
        let read = buffer
            .reader()
            .read(a, Path::Live, mark(0, 0), usize::MAX)
            .await;
        let expected = Read {
            gap: Some(0..3),
            entries: vec![stored(3, 3, None, shard.block(10))],
            next: mark(6, 0),
        };
        assert_eq!(read, Ok(expected));
    });
}

/// A carry record after the records of a path holds the tail that they give, or
/// the open refuses the ring. The tail of each case is index 1 on the live path,
/// whose records give seq 6 with stamp 60.
#[test]
fn an_open_refuses_a_carried_tail_that_is_not_the_tail_of_the_records() {
    let cases = [
        (carried(1, 3, 0, Some(30)), false),
        (carried(1, 6, 1, Some(60)), false),
        (carried(1, 7, 0, Some(60)), false),
        (carried(1, 6, 0, Some(61)), false),
        (carried(1, 6, 0, None), false),
        (carried(1, 6, 0, Some(60)), true),
    ];
    for (carried, valid) in cases {
        run(159, move |shard| async move {
            shard.create_two_records().await;
            shard.put_carry(3 * BLOCK, &carry_body(&[carried])).await;
            let ring = layout(AREA, BODY_MAX);
            if !valid {
                shard.open_invalid(ring, 3 * BLOCK).await;
                return;
            }
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            assert_eq!(buffer.durable(a, Path::Live), tail(6, Some(60)));
            let read = buffer
                .reader()
                .read(a, Path::Live, mark(0, 0), usize::MAX)
                .await;
            let entries = vec![
                stored(0, 3, Some(30), shard.block(0)),
                stored(3, 3, Some(60), shard.block(0)),
            ];
            assert_eq!(read, Ok(whole(entries, mark(6, 0))));
        });
    }
}

/// A carried `given` of `u64::MAX`, then a data record with an entry with no
/// samples at the carried seq. The open takes the carry record and refuses the data
/// record: it does not count past `u64::MAX`.
#[test]
fn an_open_refuses_an_entry_with_no_samples_after_a_carried_given_of_u64_max() {
    run(161, |shard| async move {
        shard.create_two_records().await;
        shard
            .put_carry(3 * BLOCK, &carry_body(&[carried(2, 4, 5, None)]))
            .await;
        let ring = layout(AREA, BODY_MAX);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let b = slots.index(key(2));
        assert_eq!(buffer.durable(b, Path::Live), tail(4, None));
        let empty = entry(2, b, Path::Live, 4, 0, None, Parts::default());
        buffer.append([empty]).expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        let kind = to_usize(AREA_START + 5 * BLOCK) + 8;
        assert_eq!(shard.bytes().await[kind], DATA, "the data record");
        shard
            .put_carry(3 * BLOCK, &carry_body(&[carried(2, 4, u64::MAX, None)]))
            .await;
        shard.seal(4 * BLOCK).await;
        shard.open_invalid(ring, 5 * BLOCK).await;
    });
}

/// The open takes each carried `given`, up to `u64::MAX`.
#[test]
fn an_open_takes_a_carried_given_up_to_u64_max() {
    for given in [(1 << 63) - 1, 1 << 63, u64::MAX] {
        run(163, move |shard| async move {
            shard.create_two_records().await;
            let body = carry_body(&[carried(2, 4, given, None)]);
            shard.put_carry(3 * BLOCK, &body).await;
            let ring = layout(AREA, BODY_MAX);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let b = slots.index(key(2));
            assert_eq!(buffer.durable(b, Path::Live), tail(4, None));
        });
    }
}

/// A carry body over `body_max` makes a record longer than the window of the layout.
/// The walk cannot check such a record, of any kind, so it ends there.
#[test]
fn an_open_ends_at_a_carry_record_over_body_max() {
    run(162, |shard| async move {
        shard.create_two_records().await;
        let tails: Vec<_> = (2..=99).map(|index| carried(index, 4, 0, None)).collect();
        let body = carry_body(&tails);
        assert!(body.len() > BODY_MAX, "a body of {} bytes", body.len());
        shard.put_carry(3 * BLOCK, &body).await;
        let mut slots = Slots::new();
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut slots).await;
        let buffer = buffer.expect("opens");
        let b = slots.index(key(2));
        assert_eq!(buffer.durable(b, Path::Live), tail(0, None));
    });
}

/// A record that follows the chain with a body over `body_max` is a wrong shape, also
/// within the window. At a `body_max` of 4088 the window is two blocks, so a body of
/// 4120 bytes fits it.
#[test]
fn an_open_refuses_a_carry_body_over_body_max_within_the_window() {
    run(162, |shard| async move {
        let ring = layout(AREA, 4088);
        {
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            for (first, stamp) in [(0, 30), (3, 60)] {
                let parts = Parts::default();
                buffer
                    .append([entry(1, a, Path::Live, first, 3, Some(stamp), parts)])
                    .expect("queues");
                buffer.committed().await.expect("commits");
            }
        }
        let tails: Vec<_> = (2..=99).map(|index| carried(index, 4, 0, None)).collect();
        let body = carry_body(&tails);
        assert!(body.len() > 4088, "a body of {} bytes", body.len());
        shard.put_carry(3 * BLOCK, &body).await;
        shard.open_invalid(ring, 3 * BLOCK).await;
    });
}

/// A data record over `body_max` within the window is a wrong shape too: the header
/// of a ring of three-block records says 4088, so its two-block record passes it.
#[test]
fn an_open_refuses_a_data_body_over_body_max_within_the_window() {
    run(164, |shard| async move {
        shard.create_long_record().await;
        shard.tamper(BODY_MAX_AT, &4088_u32.to_le_bytes()).await;
        shard.open_invalid(layout(AREA, 4088), BLOCK).await;
    });
}

/// A carry record of the wrong shape: no count, a count over the tails it holds,
/// bytes after its tails, a wrong path byte, and a wrong presence byte.
#[test]
fn an_open_refuses_a_carry_record_of_the_wrong_shape() {
    let one = carry_body(&[carried(2, 4, 0, None)]);
    let with = |at: usize, byte: u8| {
        let mut body = one.clone();
        body[4 + at] = byte;
        body
    };
    let mut short = one.clone();
    short[0] = 2;
    let bodies = [
        vec![1, 0, 0],
        short,
        [one.clone(), vec![0]].concat(),
        with(16, 2),
        with(33, 2),
    ];
    for body in bodies {
        run(160, move |shard| async move {
            shard.create_two_records().await;
            shard.put_carry(3 * BLOCK, &body).await;
            shard.open_invalid(layout(AREA, BODY_MAX), 3 * BLOCK).await;
        });
    }
}

#[test]
fn an_open_refuses_a_carry_record_with_a_valid_entry() {
    let one = carry_body(&[carried(2, 4, 0, None)]);
    let header = [
        &2_u128.to_le_bytes()[..],
        &[0],
        &4_u64.to_le_bytes(),
        &1_u32.to_le_bytes(),
        &7_i64.to_le_bytes(),
        &[1],
        &30_i64.to_le_bytes(),
        &[0],
        &3_u32.to_le_bytes(),
    ]
    .concat();
    let mut with = [one.clone(), header, vec![9; 3]].concat();
    with[2] = 1;
    run(160, move |shard| async move {
        shard.create_two_records().await;
        shard.put_carry(3 * BLOCK, &one).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(opened.map(drop), Ok(()), "the body with no entry opens");
        shard.put_carry(3 * BLOCK, &with).await;
        shard.open_invalid(layout(AREA, BODY_MAX), 3 * BLOCK).await;
    });
}

/// A trim passed the record with the first entry with no samples that index 1 has at
/// seq 3, and kept a record with two more. A carry record after the kept ones gives
/// the count of them all. Each case: the seqs of the entries with no samples of each
/// record, the count of a carry record at seq 3 in place of the first kept record,
/// the carried count, and the block of the record that the open refuses. A count
/// that is not the walked one is a wrong shape.
#[test]
fn an_open_counts_the_entries_with_no_samples_that_a_trim_passed() {
    type Records = &'static [&'static [u64]];
    let cases: [(Records, Option<u64>, u64, Option<u64>); 6] = [
        (&[&[3], &[3, 3]], None, 3, None),
        (&[&[3], &[3, 3]], None, 1, Some(3)),
        (&[&[3], &[3, 3]], None, 4, Some(3)),
        (&[&[3], &[3, 3], &[4]], None, 2, Some(4)),
        (&[&[3], &[3], &[3]], Some(2), 4, Some(4)),
        (&[&[3], &[3], &[3]], Some(1), 3, Some(3)),
    ];
    for (records, first, given, refused) in cases {
        run(188, move |shard| async move {
            let ring = layout(AREA, BODY_MAX);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            let empty =
                |first| entry(1, a, Path::Live, first, 0, None, Parts::default());
            for firsts in records {
                buffer
                    .append(firsts.iter().map(|&first| empty(first)))
                    .expect("queues");
                buffer.committed().await.expect("commits");
            }
            drop(buffer);
            let carry = BLOCK * u64::try_from(records.len() + 1).expect("few records");
            if let Some(first) = first {
                let carried = carried(1, 3, first, Some(30));
                shard.put_carry(2 * BLOCK, &carry_body(&[carried])).await;
                for offset in (3 * BLOCK..carry).step_by(to_usize(BLOCK)) {
                    shard.seal(offset).await;
                }
            }
            let seq = *records.concat().last().expect("an entry");
            let carried = carried(1, seq, given, Some(30));
            shard.put_carry(carry, &carry_body(&[carried])).await;
            let chain = to_usize(AREA_START + BLOCK) + 4;
            let chain = shard.bytes().await[chain..chain + 4].to_vec();
            shard.tamper(TAIL_AT, &(2 * BLOCK).to_le_bytes()).await;
            shard.tamper(CHAIN_AT, &chain).await;
            if let Some(block) = refused {
                return shard.open_invalid(ring, block * BLOCK).await;
            }
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let reader = buffer.reader();
            let a = slots.index(key(1));
            assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
            let stored = || stored(3, 0, None, shard.block(0));
            let read = reader.read(a, Path::Live, mark(0, 0), usize::MAX).await;
            let expected = Read {
                gap: Some(0..3),
                entries: vec![stored(), stored()],
                next: mark(3, 3),
            };
            assert_eq!(read, Ok(expected));
            let read = reader.read(a, Path::Live, mark(3, 2), usize::MAX).await;
            assert_eq!(read, Ok(whole(vec![stored()], mark(3, 3))));
            let read = reader.read(a, Path::Live, mark(3, 3), usize::MAX).await;
            assert_eq!(read, Ok(whole(vec![], mark(3, 3))));
            buffer.append([empty(3)]).expect("queues");
            buffer.committed().await.expect("commits");
            let read = reader.read(a, Path::Live, mark(3, 3), usize::MAX).await;
            assert_eq!(read, Ok(whole(vec![stored()], mark(3, 4))));
        });
    }
}

/// Data records at 1, 2, and 4 blocks each hold an entry with no samples at seq 3,
/// and a trim passed the first two. The carry record at 3 blocks, written over the
/// third, gives a count, and the records after it count on from it. Each case: the
/// count of the carry record at 3 blocks, the count of the one at 5 blocks, and the
/// block of the record that the open refuses.
#[test]
fn an_open_refuses_a_count_after_a_carry_record_that_is_not_the_walked_one() {
    let cases = [
        (3, 4, None),
        (3, 5, Some(5)),
        (3, 3, Some(5)),
        (2, 4, Some(4)),
    ];
    for (first, given, refused) in cases {
        run(189, move |shard| async move {
            let ring = layout(AREA, BODY_MAX);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            for _ in 0..4 {
                let empty = entry(1, a, Path::Live, 3, 0, None, Parts::default());
                buffer.append([empty]).expect("queues");
                buffer.committed().await.expect("commits");
            }
            drop(buffer);
            shard
                .put_carry(3 * BLOCK, &carry_body(&[carried(1, 3, first, None)]))
                .await;
            shard.seal(4 * BLOCK).await;
            shard
                .put_carry(5 * BLOCK, &carry_body(&[carried(1, 3, given, None)]))
                .await;
            let chain = to_usize(AREA_START + 2 * BLOCK) + 4;
            let chain = shard.bytes().await[chain..chain + 4].to_vec();
            shard.tamper(TAIL_AT, &(3 * BLOCK).to_le_bytes()).await;
            shard.tamper(CHAIN_AT, &chain).await;
            if let Some(block) = refused {
                return shard.open_invalid(ring, block * BLOCK).await;
            }
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            assert_eq!(buffer.durable(a, Path::Live), tail(3, None));
        });
    }
}

#[test]
fn an_open_that_finds_an_unaligned_tail_leaves_the_ring_as_read() {
    run(108, |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        shard.tamper(TAIL_AT, &(BLOCK + 1).to_le_bytes()).await;
        let unaligned = Error::Unaligned { tail: BLOCK + 1 };
        let case = "a tail one byte past a block";
        shard
            .open_refused(layout(AREA, BODY_MAX), unaligned, case)
            .await;
    });
}

/// The open takes the newer header block, or the first one on a tie. It does not
/// check the tail of the other, and an open that gives `Unaligned` leaves the two
/// blocks as read. Each case: the block made newer, the block with the tail off a
/// block boundary, and whether the open takes that block.
#[test]
fn an_open_checks_the_tail_of_the_header_block_that_it_takes() {
    let (first, second) = (0, to_usize(BLOCK));
    let cases = [
        (None, first, true),
        (None, second, false),
        (Some(first), first, true),
        (Some(first), second, false),
        (Some(second), first, false),
        (Some(second), second, true),
    ];
    for (newer, unaligned, taken) in cases {
        run(119, move |shard| async move {
            let ring = layout(AREA, BODY_MAX);
            let buffer = shard.open(ring, &mut Slots::new()).await;
            drop(buffer.expect("opens"));
            if let Some(place) = newer {
                shard.tamper_block(place, SEQ_AT, &1u64.to_le_bytes()).await;
            }
            shard
                .tamper_block(unaligned, TAIL_AT, &u64::MAX.to_le_bytes())
                .await;
            let case = format!("newer: {newer:?}, the block at {unaligned}");
            if taken {
                let refused = Error::Unaligned { tail: u64::MAX };
                shard.open_refused(ring, refused, &case).await;
            } else {
                let opened = shard.open(ring, &mut Slots::new()).await;
                assert_eq!(opened.map(drop), Ok(()), "{case}");
            }
        });
    }
}

#[test]
fn a_header_with_an_area_at_the_end_of_u64_is_not_read() {
    run(104, |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        let area = u64::MAX - 4095;
        // The area is 8 bytes at offset 10 of a header block.
        shard.tamper(10, &area.to_le_bytes()).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        assert_eq!(
            opened.map(drop),
            Err(Error::Unfit(Unfit {
                area,
                body_max: BODY_MAX
            }))
        );
    });
}

#[test]
fn a_header_with_an_area_under_four_records_is_unfit() {
    run(157, |shard| async move {
        let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        drop(buffer.expect("opens"));
        let area = 3 * BLOCK;
        // The area is 8 bytes at offset 10 of a header block.
        shard.tamper(10, &area.to_le_bytes()).await;
        let opened = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
        let unfit = Unfit {
            area,
            body_max: BODY_MAX,
        };
        assert_eq!(opened.map(drop), Err(Error::Unfit(unfit)));
    });
}

#[test]
fn a_layout_with_an_area_at_the_end_of_u64_makes_no_ring() {
    let area = u64::MAX - 4095;
    assert_eq!(
        Layout::new(area, BODY_MAX),
        Err(Unfit {
            area,
            body_max: BODY_MAX
        })
    );
}

/// Opens with no data write their restart records at the same place, each with
/// a new chain.
#[test]
fn each_open_starts_a_new_chain() {
    run(20, |shard| async move {
        let mut chains = Vec::new();
        for _ in 0..2 {
            let buffer = shard.open(layout(AREA, BODY_MAX), &mut Slots::new()).await;
            drop(buffer.expect("opens"));
            let file = shard.bytes().await;
            let at = to_usize(AREA_START) + 9;
            let chain = file[at..at + 4].try_into().expect("four bytes");
            chains.push(u32::from_le_bytes(chain));
        }
        assert_ne!(chains[0], chains[1], "both opens drew the same chain");
    });
}

/// Opens with no data leave one restart record, so a ring opened more times than
/// it has blocks opens and takes its largest record.
#[test]
fn opens_with_no_data_leave_room_for_the_largest_record() {
    for ring in [least(BODY_MAX), layout(AREA, BODY_MAX)] {
        run(24, move |shard| async move {
            let area = ring.area();
            for _ in 0..area / BLOCK {
                drop(shard.open(ring, &mut Slots::new()).await.expect("opens"));
            }
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens again");
            let a = slots.index(key(1));
            let largest = Parts::from(shard.block(BODY_MAX - 55));
            let batch = [entry(1, a, Path::Live, 0, 1, None, largest)];
            assert_eq!(buffer.append(batch), Ok(()), "an area of {area}");
        });
    }
}

/// Each open with no data writes its restart record right after the last entry,
/// over the one before it. The entry survives, also when the first sector of the
/// last restart record holds other bytes, and the ring takes the next entry.
#[test]
fn opens_with_no_data_after_an_entry_leave_room_for_the_next() {
    for torn in [false, true] {
        run(25, move |shard| async move {
            let ring = layout(7 * BLOCK, BODY_MAX);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens");
            let a = slots.index(key(1));
            buffer
                .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
                .expect("queues");
            buffer.committed().await.expect("commits");
            drop(buffer);
            for _ in 0..3 {
                drop(shard.open(ring, &mut Slots::new()).await.expect("opens"));
            }
            if torn {
                let after = to_usize(AREA_START + 2 * BLOCK);
                shard.put(after, &[0xA5; SECTOR]).await;
            }
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("opens again");
            let a = slots.index(key(1));
            assert_eq!(
                buffer.tail(a, Path::Live),
                tail(3, Some(30)),
                "torn: {torn}"
            );
            let next = [entry(1, a, Path::Live, 3, 2, Some(50), Parts::default())];
            assert_eq!(buffer.append(next), Ok(()), "torn: {torn}");
            buffer.committed().await.expect("commits");
            drop(buffer);
            let mut slots = Slots::new();
            let buffer = shard.open(ring, &mut slots).await.expect("reopens");
            let a = slots.index(key(1));
            assert_eq!(
                buffer.tail(a, Path::Live),
                tail(5, Some(50)),
                "torn: {torn}"
            );
        });
    }
}

/// Each record leaves a block free for the restart record of the next open, so a
/// full ring reopens. Each record holds the tail of its own path, so no trim frees
/// one, and the ring takes no entry before a tail moves.
#[test]
fn a_full_ring_reopens_into_the_block_that_its_records_leave_free() {
    run(21, |shard| async move {
        let ring = layout(7 * BLOCK, BODY_MAX);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let parts = Parts::from(shard.block(3900));
        for index in 1..4 {
            let slot = slots.index(key(index));
            buffer
                .append([entry(index, slot, Path::Live, 0, 1, None, parts.clone())])
                .expect("the record has room");
        }
        buffer.committed().await.expect("commits");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("reopens");
        for index in 1..4 {
            let slot = slots.index(key(index));
            assert_eq!(buffer.tail(slot, Path::Live), tail(1, None), "{index}");
        }
        let a = slots.index(key(1));
        let full = buffer.append([entry(1, a, Path::Live, 1, 1, None, parts.clone())]);
        assert_eq!(
            full,
            Err(Rejected::Full {
                needed: 4096,
                free: 0
            })
        );
    });
}

/// The reopen of a full ring syncs the header and the records it walked, as read,
/// then frees its oldest records up to the record of seq 2, which holds the tail,
/// in the sync of its restart record. The next entry then fits, and a read from
/// seq 0 gives the freed seqs as a gap.
#[test]
fn an_open_that_trims_adds_no_sync() {
    run_on_memory(21, |shard| async move {
        let ring = layout(7 * BLOCK, BODY_MAX);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let parts = Parts::from(shard.block(3900));
        for seq in 0..3 {
            buffer
                .append([entry(1, a, Path::Live, seq, 1, None, parts.clone())])
                .expect("the record has room");
        }
        buffer.committed().await.expect("commits");
        drop(buffer);
        let syncs = shard.memory().syncs();
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("reopens");
        assert_eq!(
            shard.memory().syncs(),
            syncs + 2,
            "the walked records, then the restart record and the trim"
        );
        let a = slots.index(key(1));
        assert_eq!(buffer.tail(a, Path::Live), tail(3, None));
        buffer
            .append([entry(1, a, Path::Live, 3, 1, None, parts.clone())])
            .expect("the open freed room");
        buffer.committed().await.expect("commits");
        let reads = read_all(&buffer, a, Path::Live, Mark::at(0), usize::MAX).await;
        let firsts: Vec<u64> =
            entries(&reads).iter().map(|entry| entry.first).collect();
        assert_eq!((reads[0].gap.clone(), firsts), (Some(0..2), vec![2, 3]));
    });
}

/// After a commit cut in its sync, with its record on the disk and its tail not, the
/// open frees past the headroom: two records of the largest size then fit, the
/// second placed while the first syncs. With no trim at the open, the second gets
/// `Full { needed: 16384, free: 8192 }`.
#[test]
fn an_open_after_a_cut_commit_frees_past_the_headroom() {
    run_on_memory(167, |shard| async move {
        let ring = layout(19 * BLOCK, 4 * to_usize(BLOCK) - 9);
        let record = |a: Slot, seq: u64, blocks: u64| {
            let parts = Parts::from(shard.block(to_usize(blocks * BLOCK) - 400));
            entry(1, a, Path::Live, seq, 1, None, parts)
        };
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer.append([record(a, 0, 4)]).expect("queues");
        buffer.committed().await.expect("commits");
        shard.memory().fail_syncs();
        buffer.append([record(a, 1, 4)]).expect("queues");
        let cut = buffer.committed().await;
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(cut, Err(failed));
        assert_eq!(buffer.durable(a, Path::Live), tail(1, None));
        drop(buffer);
        shard.memory().pass_syncs();
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("reopens");
        let a = slots.index(key(1));
        assert_eq!(buffer.tail(a, Path::Live), tail(2, None));
        let sync = Span::from_nanos(4_000_000);
        shard.memory().slow_syncs(shard.clock.clone(), sync);
        buffer.append([record(a, 2, 4)]).expect("the first fits");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() + sync.nanos() / 2))
            .await;
        let second = buffer.append([record(a, 3, 4)]);
        assert_eq!(second, Ok(()), "placed while the first syncs");
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(4, None));
    });
}

/// Takes each block that `pool` has room for, so that each later alloc fails until
/// they drop.
fn drain(pool: &Pool) -> Vec<block::Unique> {
    let mut held = Vec::new();
    let mut len = pool.largest();
    while len > 0 {
        match pool.alloc(len) {
            Ok(block) => held.push(block),
            Err(_) => len /= 2,
        }
    }
    held
}

/// Commits of index 2 whose carry finds no pool block fill the ring as far as its
/// records let it, with the tail of index 1 in the first data record. The buffer
/// then drops, which writes nothing, as a crash does. The open succeeds and gives
/// each tail. The next two commits carry the tail of index 1 and trim past its
/// record, so an append fits again.
#[test]
fn a_crash_after_a_commit_with_no_carry_block_opens() {
    run_on_memory(169, |shard| async move {
        let ring = layout(AREA, BODY_MAX);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let (a, b) = (slots.index(key(1)), slots.index(key(2)));
        let next = |seq: u64| {
            let stamp = 40 + i64::try_from(seq).expect("a small seq");
            [entry(
                2,
                b,
                Path::Live,
                seq,
                1,
                Some(stamp),
                Parts::default(),
            )]
        };
        let first = entry(1, a, Path::Live, 0, 3, Some(30), Parts::default());
        buffer.append([first]).expect("queues");
        buffer.committed().await.expect("commits");
        let mut held = Vec::new();
        for seq in 0..11 {
            held.clear();
            buffer.append(next(seq)).expect("the ring has room");
            held = drain(&shard.pool);
            buffer.committed().await.expect("commits");
        }
        let full = Rejected::Full {
            needed: 4096,
            free: 0,
        };
        assert_eq!(buffer.append(next(11)), Err(full.clone()));
        for _ in 0..4 {
            shard.clock.sleep(commits(1)).await;
            held.extend(drain(&shard.pool));
        }
        drop(buffer);
        drop(held);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let (a, b) = (slots.index(key(1)), slots.index(key(2)));
        assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(30)));
        assert_eq!(buffer.tail(b, Path::Live), tail(11, Some(50)));
        for commit in ["the carry", "the trim"] {
            assert_eq!(buffer.append(next(11)), Err(full.clone()), "{commit}");
            shard.clock.sleep(commits(4)).await;
        }
        buffer.append(next(11)).expect("the trim freed room");
        buffer.committed().await.expect("commits");
        let read = buffer
            .reader()
            .read(a, Path::Live, mark(0, 0), usize::MAX)
            .await;
        let hidden = Read {
            gap: Some(0..3),
            entries: vec![],
            next: mark(3, 0),
        };
        assert_eq!(read, Ok(hidden));
        assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(30)));
    });
}

/// Makes a ring of `AREA` with a data record of index 1 at `BLOCK`, one of index 2
/// at `2 * BLOCK`, and a carry record of `tails` in each block after them but the
/// last `free`.
async fn create_carried_ring(shard: &Shard, tails: &[Vec<u8>], free: u64) {
    let mut slots = Slots::new();
    let buffer = shard
        .open(layout(AREA, BODY_MAX), &mut slots)
        .await
        .expect("opens");
    let (a, b) = (slots.index(key(1)), slots.index(key(2)));
    for appended in [
        entry(1, a, Path::Live, 0, 3, Some(30), Parts::default()),
        entry(2, b, Path::Live, 0, 2, Some(40), Parts::default()),
    ] {
        buffer.append([appended]).expect("queues");
        buffer.committed().await.expect("commits");
    }
    drop(buffer);
    let body = carry_body(tails);
    for block in 3..AREA / BLOCK - free {
        shard.put_carry(block * BLOCK, &body).await;
    }
}

/// An open of a ring with no free block for its restart record syncs the records
/// it walked, frees the oldest records a step past the headroom in a second sync,
/// then writes its restart record in a freed block in a third. Each path keeps its
/// tail.
#[test]
fn an_open_of_a_ring_with_no_free_block_frees_its_oldest_records() {
    run_on_memory(163, |shard| async move {
        let tails = [carried(1, 3, 0, Some(30)), carried(2, 2, 0, Some(40))];
        create_carried_ring(&shard, &tails, 0).await;
        let ring = layout(AREA, BODY_MAX);
        let syncs = shard.memory().syncs();
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let synced = shard.memory().syncs();
        assert_eq!(synced, syncs + 3, "the walk, the trim, then the restart");
        let (a, b) = (slots.index(key(1)), slots.index(key(2)));
        assert_eq!(buffer.tail(a, Path::Live), tail(3, Some(30)));
        assert_eq!(buffer.tail(b, Path::Live), tail(2, Some(40)));
        let read = buffer
            .reader()
            .read(a, Path::Live, mark(0, 0), usize::MAX)
            .await;
        let hidden = Read {
            gap: Some(0..3),
            entries: vec![],
            next: mark(3, 0),
        };
        assert_eq!(read, Ok(hidden));
        let parts = Parts::from(shard.block(3900));
        for seq in 3..10 {
            let next = entry(1, a, Path::Live, seq, 1, None, parts.clone());
            assert_eq!(buffer.append([next]), Ok(()), "seq {seq}");
        }
        let full = buffer.append([entry(1, a, Path::Live, 10, 1, None, parts)]);
        let refused = Rejected::Full {
            needed: 4096,
            free: 0,
        };
        assert_eq!(full, Err(refused), "a step past the headroom");
        buffer.committed().await.expect("commits");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("reopens");
        let (a, b) = (slots.index(key(1)), slots.index(key(2)));
        assert_eq!(buffer.tail(a, Path::Live), tail(10, Some(30)));
        assert_eq!(buffer.tail(b, Path::Live), tail(2, Some(40)));
    });
}

/// An open of a ring with no free block, whose oldest record holds the only tail
/// of index 1, frees nothing and gives `Full`.
#[test]
fn an_open_that_kept_stops_gives_full() {
    run(165, |shard| async move {
        let tails = [carried(2, 2, 0, Some(40))];
        create_carried_ring(&shard, &tails, 0).await;
        let file = shard.bytes().await;
        let at = |offset: u64| to_usize(AREA_START + offset);
        let u32_at = |at: usize| {
            u32::from_le_bytes(file[at..at + 4].try_into().expect("four bytes"))
        };
        let restart = u32_at(at(0) + 9);
        let chain = u32_at(at(AREA - BLOCK) + 4);
        let body = carry_body(&tails);
        let len = u32::try_from(body.len()).expect("a short body");
        let mut record = [&len.to_le_bytes()[..], &[0; 4], &[CARRY], &body].concat();
        let crc = crc32c::crc32c_append(chain, &record[..4]);
        let crc = crc32c::crc32c_append(crc, &record[8..]);
        record[4..8].copy_from_slice(&crc.to_le_bytes());
        shard.put(at(0), &record).await;
        shard.tamper(TAIL_AT, &BLOCK.to_le_bytes()).await;
        shard.tamper(CHAIN_AT, &restart.to_le_bytes()).await;
        let full = Error::Full {
            needed: 4096,
            free: 0,
        };
        assert_eq!(
            full.to_string(),
            "the ring has no room for its restart record: it needs 4096 bytes and 0 \
             are free"
        );
        let ring = layout(AREA, BODY_MAX);
        shard
            .open_refused(ring, full, "a ring that no trim frees")
            .await;
    });
}

/// A power cut at any point of an open that trims, with a free block for its
/// restart record and with none, leaves a ring that opens. Each path keeps its
/// tail, and a read of index 1 gives its entry or the gap of its trim.
#[test]
fn a_cut_at_each_file_call_of_an_open_that_trims_keeps_each_tail() {
    for free in [1, 0] {
        each_cut(0..8, 10_000, |seed, cut| {
            let (mut sim, node) = create_node(seed);
            sim.run_on(&node, move |node, tasks| async move {
                let tails = [carried(1, 3, 0, Some(30)), carried(2, 2, 0, Some(40))];
                create_carried_ring(&shard_on(node, tasks), &tails, free).await;
            })
            .expect("the ring is made");
            let ended = cut_an_open(&mut sim, &node, cut, sim::Crash::Power);
            let opened = sim.run_on(&node, |node, tasks| async move {
                let mut slots = Slots::new();
                let config = node_config(&node, tasks, DIR);
                let buffer = Buffer::open(config, &mut slots).await?;
                let (a, b) = (slots.index(key(1)), slots.index(key(2)));
                let tails = [a, b].map(|slot| buffer.tail(slot, Path::Live));
                let read = buffer
                    .reader()
                    .read(a, Path::Live, mark(0, 0), usize::MAX)
                    .await;
                Ok::<_, Error>((tails, read))
            });
            let case = format!("{free} free, seed {seed}, cut at {cut} ns");
            let opened = opened.unwrap_or_else(|e| panic!("{case}: {e}"));
            let (tails, read) = opened.unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(tails, [tail(3, Some(30)), tail(2, Some(40))], "{case}");
            let empty = pool().alloc(0).expect("a block").freeze();
            let kept = whole(vec![stored(0, 3, Some(30), empty)], mark(3, 0));
            let hidden = Read {
                gap: Some(0..3),
                entries: vec![],
                next: mark(3, 0),
            };
            let read = read.unwrap_or_else(|e| panic!("{case}: {e}"));
            assert!(read == kept || read == hidden, "{case}: {read:?}");
            ended
        });
    }
}

/// Starts an open that trims of the ring on `node`, as the shard `name`, and keeps
/// its buffer. Gives what the open gave once it ends.
fn start_open(
    node: &sim::node::Node,
    name: &str,
) -> Arc<Mutex<Option<Result<(), Error>>>> {
    let ended = Arc::new(Mutex::new(None));
    let gave = Arc::clone(&ended);
    let own = node.clone();
    drop(on_node(node, name, move |tasks| async move {
        let config = node_config(&own, tasks, DIR);
        let opened = Buffer::open(config, &mut Slots::new()).await;
        *gave.lock().expect("no panic") =
            Some(opened.as_ref().map(drop).map_err(Clone::clone));
        std::future::pending::<()>().await;
        drop(opened);
    }));
    ended
}

/// The first multiple of `step` at which `ended` holds, when it holds at each one
/// after that too and not at 0.
fn first_end(step: i64, mut ended: impl FnMut(i64) -> bool) -> i64 {
    let (mut low, mut high) = (0, step);
    while !ended(high) {
        (low, high) = (high, 2 * high);
    }
    while high - low > step {
        let mid = i64::midpoint(low, high) / step * step;
        if ended(mid) {
            high = mid;
        } else {
            low = mid;
        }
    }
    high
}

/// On a new node with `seed`, makes a ring with no free block, starts an open that
/// trims, fails its next sync `fault` ns into it, and stops the process. Then
/// starts a second open and cuts the power `cut` ns into it. Gives the sim, the
/// node, whether the first open failed, and whether the second had ended before its
/// cut.
fn fail_then_cut(
    seed: u64,
    fault: i64,
    cut: i64,
) -> (sim::Sim, sim::node::Node, bool, bool) {
    let (mut sim, node) = create_node(seed);
    sim.run_on(&node, move |node, tasks| async move {
        let tails = [carried(1, 3, 0, Some(30)), carried(2, 2, 0, Some(40))];
        create_carried_ring(&shard_on(node, tasks), &tails, 0).await;
    })
    .expect("the ring is made");
    let first = start_open(&node, "first");
    sim.run_for(Span::from_nanos(fault))
        .expect("the run goes on");
    node.fail_file(FilePath::new(RING), Operation::Sync);
    sim.run_for(Span::from_nanos(10_000_000))
        .expect("the run goes on");
    let first = first.lock().expect("no panic").take();
    let failed = first.expect("the first open ends").is_err();
    sim.crash(&node, sim::Crash::Process);
    let second = start_open(&node, "second");
    sim.run_for(Span::from_nanos(cut)).expect("the run goes on");
    let second = second.lock().expect("no panic").is_some();
    sim.crash(&node, sim::Crash::Power);
    (sim, node, failed, second)
}

/// A failed sync near the end of an open that trims a ring with no free block, then a
/// power cut during the next open, keeps each tail. The failed sync can leave the
/// checkpoint of the trim in the cache only, and the next open reads it there: it
/// writes over the records that it frees only once it is durable. Each fault fails a
/// sync of the first open: a later fault would fail a sync of the next.
#[test]
fn a_cut_open_after_a_failed_sync_of_an_open_that_trims_keeps_each_tail() {
    for seed in 0..2 {
        let last = first_end(10_000, |fault| !fail_then_cut(seed, fault, 0).2);
        for fault in ((last - 300_000).max(0)..last).step_by(10_000) {
            let second = first_end(2_000, |cut| fail_then_cut(seed, fault, cut).3);
            for cut in ((second - 400_000).max(0)..second).step_by(2_000) {
                let (mut sim, node, ..) = fail_then_cut(seed, fault, cut);
                let opened = sim.run_on(&node, |node, tasks| async move {
                    let mut slots = Slots::new();
                    let config = node_config(&node, tasks, DIR);
                    let buffer = Buffer::open(config, &mut slots).await?;
                    let (a, b) = (slots.index(key(1)), slots.index(key(2)));
                    Ok::<_, Error>([a, b].map(|slot| buffer.tail(slot, Path::Live)))
                });
                let case = format!("seed {seed}, fault at {fault} ns, cut at {cut} ns");
                let opened = opened.unwrap_or_else(|e| panic!("{case}: {e}"));
                let tails = opened.unwrap_or_else(|e| panic!("{case}: {e}"));
                assert_eq!(tails, [tail(3, Some(30)), tail(2, Some(40))], "{case}");
            }
        }
    }
}

/// On a new node with `seed`, appends a record of index 1 at each seq in a commit of
/// its own to a ring that trims, and fails the sync of the commit of seq `failed`.
/// Then stops the process, starts an open, and cuts the power `cut` ns into it.
/// Gives the sim, the node, whether the last two commits wrote a checkpoint, and
/// whether the open had ended before its cut.
fn fail_a_commit_then_cut(
    seed: u64,
    failed: u64,
    cut: i64,
) -> (sim::Sim, sim::node::Node, bool, bool) {
    let (mut sim, node) = create_node(seed);
    let trimmed = sim.run_on(&node, move |node, tasks| async move {
        let shard = shard_on(node, tasks);
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let parts = Parts::from(shard.block(3900));
        let record = |seq: u64| {
            let last = i64::try_from(seq).expect("a small seq") + 1;
            [entry(1, a, Path::Live, seq, 1, Some(last), parts.clone())]
        };
        let headers = to_usize(AREA_START);
        let mut before = shard.bytes().await[..headers].to_vec();
        for seq in 0..failed {
            if seq + 1 == failed {
                before = shard.bytes().await[..headers].to_vec();
            }
            buffer.append(record(seq)).expect("the ring has room");
            buffer.committed().await.expect("commits");
        }
        shard.node().fail_file(FilePath::new(RING), Operation::Sync);
        buffer.append(record(failed)).expect("the ring has room");
        let fault = Err(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        assert_eq!(buffer.committed().await, fault, "seq {failed}");
        shard.bytes().await[..headers] != before
    });
    let trimmed = trimmed.expect("the commits run");
    sim.crash(&node, sim::Crash::Process);
    let open = start_open(&node, "open");
    sim.run_for(Span::from_nanos(cut)).expect("the run goes on");
    let ended = open.lock().expect("no panic").is_some();
    sim.crash(&node, sim::Crash::Power);
    (sim, node, trimmed, ended)
}

/// A failed sync of a commit that trims, then a power cut during the next open, keeps
/// each durable tail. The failed sync can leave the checkpoint in the cache only, and
/// the open reads it there.
#[test]
fn a_cut_open_after_a_failed_sync_of_a_commit_that_trims_keeps_each_tail() {
    for seed in 0..2 {
        let mut trims = 0;
        for failed in 0..24 {
            if !fail_a_commit_then_cut(seed, failed, 0).2 {
                continue;
            }
            trims += 1;
            let end =
                first_end(2_000, |cut| fail_a_commit_then_cut(seed, failed, cut).3);
            for cut in (0..end).step_by(10_000) {
                let (mut sim, node, ..) = fail_a_commit_then_cut(seed, failed, cut);
                let opened = sim.run_on(&node, |node, tasks| async move {
                    let mut slots = Slots::new();
                    let config = node_config(&node, tasks, DIR);
                    let buffer = Buffer::open(config, &mut slots).await?;
                    Ok::<_, Error>(buffer.tail(slots.index(key(1)), Path::Live))
                });
                let case = format!("seed {seed}, seq {failed} failed, cut at {cut} ns");
                let opened = opened.unwrap_or_else(|e| panic!("{case}: {e}"));
                let got = opened.unwrap_or_else(|e| panic!("{case}: {e}"));
                let last = i64::try_from(failed).expect("a small seq");
                let kept = [tail(failed, Some(last)), tail(failed + 1, Some(last + 1))];
                assert!(kept.contains(&got), "{case}: {got:?}");
            }
        }
        assert!(trims > 1, "seed {seed}: {trims} failed commits trimmed");
    }
}

#[test]
fn a_failed_record_write_ends_the_buffer_with_its_error() {
    let (mut sim, node) = create_node(111);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        node.fail_file(FilePath::new(RING), Operation::WriteAt);
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::WriteAt,
            code: 5,
        };
        assert_eq!(buffer.committed().await, Err(failed.clone()));
        assert_eq!(buffer.durable(a, Path::Live), Tail::default());
        assert_eq!(
            buffer.append([entry(1, a, Path::Live, 3, 1, None, Parts::default())]),
            Err(Rejected::Files(failed))
        );
    })
    .expect("the buffer ends");
}

/// The `Commit` that `committed` gives answers only for the entries appended before
/// its call: past the drop, a later failed write does not change its result.
#[test]
fn a_commit_held_past_the_drop_gives_ok_when_a_later_write_fails() {
    let (mut sim, node) = create_node(113);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let early = buffer.committed();
        assert_eq!(buffer.committed().await, Ok(()));
        node.fail_file(FilePath::new(RING), Operation::WriteAt);
        buffer
            .append([entry(1, a, Path::Live, 3, 1, Some(40), Parts::default())])
            .expect("queues");
        let late = buffer.committed();
        drop(buffer);
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::WriteAt,
            code: 5,
        };
        assert_eq!(early.await, Ok(()));
        assert_eq!(late.await, Err(failed));
    })
    .expect("the buffer ends");
}

/// An `End` gives the error of the write that ended the task after the drop, while a
/// `Commit` of the entries before it gives `Ok`.
#[test]
fn an_end_gives_the_error_of_a_write_after_the_drop() {
    let (mut sim, node) = create_node(114);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let end = buffer.ended();
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let early = buffer.committed();
        assert_eq!(buffer.committed().await, Ok(()));
        node.fail_file(FilePath::new(RING), Operation::WriteAt);
        buffer
            .append([entry(1, a, Path::Live, 3, 1, Some(40), Parts::default())])
            .expect("queues");
        drop(buffer);
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::WriteAt,
            code: 5,
        };
        assert_eq!(end.await, Err(failed));
        assert_eq!(early.await, Ok(()));
    })
    .expect("the buffer ends");
}

/// With no fault, an `End` gives `Ok` once the task wrote the entries queued at the
/// drop, and a reopen recovers them.
#[test]
fn an_end_gives_ok_once_the_entries_queued_at_the_drop_are_durable() {
    let (mut sim, node) = create_node(115);
    let run = sim.run_on(&node, |node, tasks| async move {
        let config = || node_config(&node, tasks.clone(), DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config(), &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer
            .append([entry(1, a, Path::Live, 3, 2, Some(50), Parts::default())])
            .expect("queues");
        let end = buffer.ended();
        drop(buffer);
        assert_eq!(end.await, Ok(()));
        let buffer = Buffer::open(config(), &mut slots).await.expect("reopens");
        buffer.durable(a, Path::Live)
    });
    assert_eq!(run, Ok(tail(5, Some(50))));
}

/// A failed write ends the task while the buffer is held, and an `End` gives its
/// error then.
#[test]
fn an_end_gives_the_error_of_a_write_before_the_drop() {
    let (mut sim, node) = create_node(116);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let end = buffer.ended();
        node.fail_file(FilePath::new(RING), Operation::WriteAt);
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::WriteAt,
            code: 5,
        };
        assert_eq!(end.await, Err(failed.clone()));
        assert_eq!(
            buffer.append([entry(1, a, Path::Live, 3, 1, None, Parts::default())]),
            Err(Rejected::Files(failed))
        );
    })
    .expect("the buffer ends");
}

/// Records that it woke.
struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// An `End` polled while the buffer is held stays pending across a commit that
/// syncs, which does not wake it, and gives `Ok` after the drop.
#[test]
fn an_end_stays_pending_across_a_commit_while_the_buffer_is_held() {
    let (mut sim, node) = create_node(118);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let mut end = pin!(buffer.ended());
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let polled = end.as_mut().poll(&mut Context::from_waker(&waker));
        assert_eq!(polled, Poll::Pending);
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        assert_eq!(buffer.committed().await, Ok(()));
        assert!(!flag.0.load(Ordering::Relaxed), "a commit wakes no End");
        let polled = poll_fn(|cx| Poll::Ready(end.as_mut().poll(cx))).await;
        assert_eq!(polled, Poll::Pending);
        drop(buffer);
        assert_eq!(end.await, Ok(()));
    })
    .expect("the buffer ends");
}

/// The test of the hand-written `Debug` of the private `Waiter` that a `Commit` and an
/// `End` hold: it prints only its event and its key.
#[test]
fn a_waiter_prints_only_its_event_and_key() {
    let (mut sim, node) = create_node(142);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let buffer = Buffer::open(config, &mut Slots::new())
            .await
            .expect("opens");
        let mut polled = pin!(buffer.ended());
        let polled = polled
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert_eq!(polled, Poll::Pending);
        let commit = format!("{:?}", buffer.committed());
        let end = format!("{:?}", buffer.ended());
        let waiter = "Waiter { event: Commit, key: 1, .. }";
        assert!(commit.contains(waiter), "{commit}");
        let waiter = "Waiter { event: End, key: 2, .. }";
        assert!(end.contains(waiter), "{end}");
    })
    .expect("the buffer ends");
}

/// The test of the hand-written `Debug` of the private `Parked`: a buffer prints
/// whether its task idles, and no pointer of the task's waker.
#[test]
fn a_buffer_prints_whether_it_idles_and_no_pointer() {
    run_on_memory(144, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.clock.sleep(commits(3)).await;
        let printed = format!("{buffer:?}");
        assert!(printed.contains("parked: true"), "{printed}");
        assert!(!printed.contains("0x"), "{printed}");
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let printed = format!("{buffer:?}");
        assert!(printed.contains("parked: false"), "{printed}");
    });
}

/// A waker drops after the borrow of the state ends: a `Commit` and an `End` drop it
/// in a poll that replaces it and in their own drop. The waker holds an `End`, whose
/// drop borrows the state.
#[test]
fn a_waker_drops_after_the_borrow_of_the_state() {
    let (mut sim, node) = create_node(143);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        drop_wakers(&buffer, buffer.committed());
        drop_wakers(&buffer, buffer.ended());
        assert_eq!(buffer.committed().await, Ok(()));
    })
    .expect("the buffer ends");
}

/// The commit task drops each waker of an `End` at its end after the borrow of the
/// state ends. The waker holds an `End`, whose drop borrows the state.
#[test]
fn a_waker_drops_at_the_end_after_the_borrow_of_the_state() {
    let (mut sim, node) = create_node(145);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let mut end = pin!(buffer.ended());
        let waker = waker::holding(buffer.ended());
        assert!(
            end.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(waker);
        let other = buffer.ended();
        drop(buffer);
        assert_eq!(other.await, Ok(()));
    })
    .expect("the buffer ends");
}

/// The commit task drops each waker of a `Commit` that it wakes after the borrow of
/// the state ends. The waker holds an `End`, whose drop borrows the state.
#[test]
fn a_waker_drops_at_the_commit_after_the_borrow_of_the_state() {
    let (mut sim, node) = create_node(146);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let mut commit = pin!(buffer.committed());
        let waker = waker::holding(buffer.ended());
        assert!(
            commit
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(waker);
        assert_eq!(buffer.committed().await, Ok(()));
        assert_eq!(commit.await, Ok(()));
    })
    .expect("the buffer ends");
}

/// Polls `waiter` with a waker that holds an `End` of `buffer`, then with a no-op
/// waker, then with one more such waker, and drops `waiter`.
fn drop_wakers<F: Future>(buffer: &Buffer, waiter: F) {
    let mut waiter = pin!(waiter);
    let waker = waker::holding(buffer.ended());
    assert!(
        waiter
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
    let noop = Waker::noop();
    assert!(
        waiter
            .as_mut()
            .poll(&mut Context::from_waker(noop))
            .is_pending()
    );
    let waker = waker::holding(buffer.ended());
    assert!(
        waiter
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
}

/// `End`s polled from other tasks and dropped while the buffer is held keep no
/// waker.
#[test]
fn a_dropped_end_keeps_no_waker_while_the_buffer_is_held() {
    let (mut sim, node) = create_node(130);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let flags: Vec<Arc<Flag>> = (0..64)
            .map(|_| Arc::new(Flag(AtomicBool::new(false))))
            .collect();
        for flag in &flags {
            let mut end = pin!(buffer.ended());
            let waker = Waker::from(Arc::clone(flag));
            let polled = end.as_mut().poll(&mut Context::from_waker(&waker));
            assert_eq!(polled, Poll::Pending);
        }
        let held = flags
            .iter()
            .filter(|flag| Arc::strong_count(flag) > 1)
            .count();
        assert_eq!(held, 0, "the drop of each End takes its waker out");
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        assert_eq!(buffer.committed().await, Ok(()));
    })
    .expect("the buffer ends");
}

/// Each `End` keeps only the waker of its last poll, and the end of the task wakes
/// the last waker of each.
#[test]
fn the_end_of_the_task_wakes_the_last_waker_of_each_end() {
    let (mut sim, node) = create_node(131);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let flags: Vec<Arc<Flag>> = (0..3)
            .map(|_| Arc::new(Flag(AtomicBool::new(false))))
            .collect();
        let poll = |end: &mut Pin<&mut End>, flag: &Arc<Flag>| {
            let waker = Waker::from(Arc::clone(flag));
            end.as_mut().poll(&mut Context::from_waker(&waker))
        };
        let mut first = pin!(buffer.ended());
        let mut second = pin!(buffer.ended());
        assert_eq!(poll(&mut first, &flags[0]), Poll::Pending);
        assert_eq!(poll(&mut first, &flags[1]), Poll::Pending);
        assert_eq!(Arc::strong_count(&flags[0]), 1, "a later poll takes it out");
        assert_eq!(poll(&mut second, &flags[2]), Poll::Pending);
        drop(buffer);
        node.clock().sleep(commits(20)).await;
        let woken: Vec<bool> = flags
            .iter()
            .map(|flag| flag.0.load(Ordering::Relaxed))
            .collect();
        assert_eq!(woken, [false, true, true]);
        assert_eq!(first.await, Ok(()));
        assert_eq!(second.await, Ok(()));
    })
    .expect("the buffer ends");
}

/// `Commit`s polled once while an entry waits and dropped keep no waker.
#[test]
fn a_dropped_commit_keeps_no_waker() {
    let (mut sim, node) = create_node(132);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        for _ in 0..1000 {
            let commit = pin!(buffer.committed());
            let waker = Waker::from(Arc::clone(&flag));
            let polled = commit.poll(&mut Context::from_waker(&waker));
            assert_eq!(polled, Poll::Pending);
        }
        assert_eq!(Arc::strong_count(&flag), 1, "the drop takes its waker out");
        assert_eq!(buffer.committed().await, Ok(()));
    })
    .expect("the buffer ends");
}

/// A `Commit` keeps only the waker of its last poll, and the commit wakes it.
#[test]
fn a_commit_polled_with_new_wakers_keeps_the_last_one() {
    let (mut sim, node) = create_node(133);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let mut commit = pin!(buffer.committed());
        let flags: Vec<Arc<Flag>> = (0..1000)
            .map(|_| Arc::new(Flag(AtomicBool::new(false))))
            .collect();
        for flag in &flags {
            let waker = Waker::from(Arc::clone(flag));
            let polled = commit.as_mut().poll(&mut Context::from_waker(&waker));
            assert_eq!(polled, Poll::Pending);
        }
        let held: Vec<usize> = flags.iter().map(Arc::strong_count).collect();
        let mut expected = vec![1; 1000];
        expected[999] = 2;
        assert_eq!(held, expected);
        assert_eq!(buffer.committed().await, Ok(()));
        let woken: Vec<bool> = flags
            .iter()
            .map(|flag| flag.0.load(Ordering::Relaxed))
            .collect();
        let mut expected = vec![false; 1000];
        expected[999] = true;
        assert_eq!(woken, expected);
        assert_eq!(commit.await, Ok(()));
    })
    .expect("the buffer ends");
}

/// The drop of a polled `Commit` leaves the waker of another, and the commit wakes it.
#[test]
fn a_dropped_commit_keeps_the_wakers_of_others() {
    let (mut sim, node) = create_node(134);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let poll = |commit: Pin<&mut Commit>, flag: &Arc<Flag>| {
            let waker = Waker::from(Arc::clone(flag));
            commit.poll(&mut Context::from_waker(&waker))
        };
        let kept = Arc::new(Flag(AtomicBool::new(false)));
        let mut commit = pin!(buffer.committed());
        assert_eq!(poll(commit.as_mut(), &kept), Poll::Pending);
        let dropped = Arc::new(Flag(AtomicBool::new(false)));
        assert_eq!(poll(pin!(buffer.committed()), &dropped), Poll::Pending);
        assert_eq!(Arc::strong_count(&kept), 2);
        assert_eq!(buffer.committed().await, Ok(()));
        assert!(kept.0.load(Ordering::Relaxed), "the commit wakes it");
        assert_eq!(commit.await, Ok(()));
    })
    .expect("the buffer ends");
}

/// An `End` taken at a drop during a sync that then fails gives its error.
#[test]
fn an_end_taken_at_a_drop_during_a_failing_sync_gives_its_error() {
    run_on_memory(120, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        shard.memory().fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(12)).await;
        assert_eq!(shard.memory().syncs(), 4, "the sync runs");
        let end = buffer.ended();
        drop(buffer);
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(end.await, Err(failed));
    });
}

/// An `End` taken after a failed commit gives its error at its first poll.
#[test]
fn an_end_taken_after_a_failed_commit_gives_its_error_at_once() {
    let (mut sim, node) = create_node(119);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        node.fail_file(FilePath::new(RING), Operation::WriteAt);
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::WriteAt,
            code: 5,
        };
        assert_eq!(buffer.committed().await, Err(failed.clone()));
        let mut end = pin!(buffer.ended());
        let polled = poll_fn(|cx| Poll::Ready(end.as_mut().poll(cx))).await;
        assert_eq!(polled, Poll::Ready(Err(failed)));
    })
    .expect("the buffer ends");
}

/// An `End` held past the drop holds the ring after the task ended, so an open fails
/// with `Busy` until the `End` drops.
#[test]
fn an_open_while_an_end_of_a_dropped_buffer_is_held_fails_with_busy() {
    let (mut sim, node) = create_node(117);
    let run = sim.run_on(&node, |node, tasks| async move {
        let config = || node_config(&node, tasks.clone(), DIR);
        let mut slots = Slots::new();
        let first = Buffer::open(config(), &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        first
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        let mut end = first.ended();
        drop(first);
        assert_eq!((&mut end).await, Ok(()));
        let busy_open = Buffer::open(config(), &mut slots).await.map(drop);
        assert_eq!(busy_open, Err(busy()));
        drop(end);
        let buffer = Buffer::open(config(), &mut slots).await.expect("reopens");
        buffer.durable(a, Path::Live)
    });
    assert_eq!(run, Ok(tail(1, Some(1))));
}

#[test]
fn an_append_with_no_block_for_its_record_header_is_refused() {
    run(112, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let mut held = Vec::new();
        let mut len = shard.pool.largest();
        while len > 0 {
            while let Ok(block) = shard.pool.alloc(len) {
                held.push(block);
            }
            len -= len.div_ceil(16);
        }
        let refused =
            buffer.append([entry(1, a, Path::Live, 0, 1, None, Parts::default())]);
        let header = block::Error::Exhausted {
            requested: 52186,
            available: 0,
        };
        assert_eq!(refused, Err(Rejected::Pool(header)));
        assert_eq!(buffer.tail(a, Path::Live), tail(0, None));
        drop(held);
    });
}

#[test]
fn an_error_says_what_went_wrong() {
    let pool = Error::Pool(block::Error::TooLarge {
        requested: 1,
        largest: 0,
    });
    let files = Error::Files(FileError::NotFound {
        path: PathBuf::from(RING),
    });
    let unfit = Error::Unfit(Unfit {
        area: 1,
        body_max: 2,
    });
    let texts = [
        (
            Error::Full { needed: 1, free: 0 },
            "the ring has no room for its restart record: it needs 1 bytes and 0 \
             are free",
        ),
        (
            pool,
            "the pool has no block: block of 1 bytes is above the largest block of \
             0 bytes",
        ),
        (files, "a file call failed: path shard-0/ring is not there"),
        (
            Error::Length {
                expected: 1,
                found: 2,
            },
            "the ring file is 2 bytes long, and its ring is 1",
        ),
        (
            Error::Missing,
            "the ring file has no header: it is not a ring",
        ),
        (
            Error::Damaged,
            "the ring is lost: both header blocks have a wrong CRC",
        ),
        (
            Error::Version(3),
            "the ring has format version 3, which this build does not read",
        ),
        (
            unfit,
            "the ring header holds an area of 1 bytes and a body of at most 2 \
             bytes, which make no ring",
        ),
        (
            Error::Unaligned { tail: 4097 },
            "the ring header holds a tail at 4097, which is not on a block boundary",
        ),
        (
            Error::Invalid { offset: 4 },
            "the ring holds bytes at 4 that this build cannot read",
        ),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
}

/// The text of `Large` is the text of its limit, checked with each limit above.
#[test]
fn a_rejected_append_says_why() {
    let pool = Rejected::Pool(block::Error::TooLarge {
        requested: 1,
        largest: 0,
    });
    let files = Rejected::Files(FileError::NotFound {
        path: PathBuf::from(RING),
    });
    let texts = [
        (
            Rejected::Full { needed: 1, free: 0 },
            "the ring has no room for the batch: it needs 1 bytes and 0 are free",
        ),
        (
            pool,
            "the pool has no block: block of 1 bytes is above the largest block of \
             0 bytes",
        ),
        (files, "a file call failed: path shard-0/ring is not there"),
    ];
    for (rejected, text) in texts {
        assert_eq!(rejected.to_string(), text);
    }
}

/// One step of a model run.
#[derive(Clone, Debug)]
enum Step {
    Append {
        index: u32,
        path: Path,
        skip: u64,
        len: u32,
        bytes: usize,
        last: Option<i64>,
    },
    Commit,
    Reopen,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (
            0..3_u32,
            prop_oneof![Just(Path::Live), Just(Path::Backfill)],
            0..3_u64,
            0..5_u32,
            0..200_usize,
            prop::option::of(any::<i64>()),
        )
            .prop_map(|(index, path, skip, len, bytes, last)| Step::Append {
                index,
                path,
                skip,
                len,
                bytes,
                last: last.filter(|_| len != 0),
            }),
        2 => Just(Step::Commit),
        1 => Just(Step::Reopen),
    ]
}

type Model = types::hash::Map<(u32, Path), Tail>;

/// Runs the steps against a model: `tails` moves at each append, `durable` at each
/// commit and at the drop before a reopen.
async fn follow(shard: Shard, steps: Vec<Step>) {
    let large = layout(256 * BLOCK, BODY_MAX);
    let mut slots = Slots::new();
    let mut buffer = shard.open(large, &mut slots).await.expect("opens");
    let mut tails = Model::default();
    let mut durable = Model::default();
    for step in steps {
        match step {
            Step::Append {
                index,
                path,
                skip,
                len,
                bytes,
                last,
            } => {
                let slot = slots.index(key(index));
                let at = tails.entry((index, path)).or_default();
                let first = at.seq + skip;
                let parts = Parts::from(shard.block(bytes));
                buffer
                    .append([entry(index, slot, path, first, len, last, parts.clone())])
                    .expect("the ring has room");
                at.seq = first + u64::from(len);
                if let Some(last) = last {
                    at.stamp = Some(Stamp::from_nanos(last));
                }
            }
            Step::Commit => {
                buffer.committed().await.expect("commits");
                durable.clone_from(&tails);
            }
            Step::Reopen => {
                let ending = buffer.committed();
                drop(buffer);
                ending
                    .await
                    .expect("the task writes the queue before it ends");
                durable.clone_from(&tails);
                slots = Slots::new();
                buffer = shard.open(large, &mut slots).await.expect("reopens");
            }
        }
        for index in 0..3 {
            for path in [Path::Live, Path::Backfill] {
                let slot = slots.index(key(index));
                let expected = tails.get(&(index, path)).copied().unwrap_or_default();
                assert_eq!(
                    buffer.tail(slot, path),
                    expected,
                    "tail of {index} {path:?}"
                );
                let expected = durable.get(&(index, path)).copied().unwrap_or_default();
                assert_eq!(
                    buffer.durable(slot, path),
                    expected,
                    "durable of {index} {path:?}"
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn follows_a_model_across_commits_and_reopens(
        seed in any::<u64>(),
        steps in prop::collection::vec(step(), 0..40),
    ) {
        run(seed, |shard| follow(shard, steps));
    }
}

#[test]
fn a_drop_right_after_the_first_append_of_an_idle_span_ends_the_task_at_its_deadline() {
    run_on_memory(40, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.clock.sleep(commits(21)).await;
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        drop(buffer);
        shard.clock.sleep(tenths(9)).await;
        assert_eq!(
            shard.memory().syncs(),
            3,
            "the entry waits for the deadline"
        );
        assert_eq!(shard.memory().open_files(), 1, "the task runs");
        shard.clock.sleep(tenths(2)).await;
        assert_eq!(shard.memory().syncs(), 4, "the deadline writes the entry");
        assert_eq!(shard.memory().open_files(), 0, "the task ended");
        shard.clock.sleep(commits(10)).await;
        assert_eq!(shard.memory().syncs(), 4, "no later deadline runs");
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.index(key(1)), Path::Live),
            tail(3, Some(30)),
            "the entry queued at the drop was written"
        );
    });
}

#[test]
fn a_drop_during_a_commit_ends_the_task_after_the_next_commit() {
    run_on_memory(41, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let sync = Span::from_nanos(4_000_000);
        shard.memory().slow_syncs(shard.clock.clone(), sync);
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() + sync.nanos() / 2))
            .await;
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues while the first sync runs");
        drop(buffer);
        assert_eq!(
            shard.memory().open_files(),
            1,
            "the task syncs the first entry"
        );
        shard.clock.sleep(sync).await;
        assert_eq!(shard.memory().syncs(), 4, "the first sync ended");
        assert_eq!(shard.memory().open_files(), 1, "the second entry waits");
        shard.clock.sleep(commits(10)).await;
        assert_eq!(shard.memory().syncs(), 5, "the next deadline writes it");
        assert_eq!(shard.memory().open_files(), 0, "the task ended");
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.index(key(1)), Path::Live),
            tail(2, Some(2)),
            "the entry queued at the drop was written"
        );
    });
}

/// A record under `body_max` but over the largest block of the pool is recovered:
/// the walk reads it in pieces of one block. The walk reads the first thirteen
/// blocks of a record on their own, so the rest is over the largest block too.
#[test]
fn a_record_over_the_largest_block_of_the_pool_is_recovered() {
    run(101, |mut shard| async move {
        let parts_pool = Rc::clone(&shard.pool);
        let config = block::Config::new(96 << 10).expect("the budget fits");
        shard.pool =
            Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
        assert_eq!(shard.pool.largest(), 80 << 10);
        let ring = least(150_000);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let mut part = parts_pool.alloc(28_000).expect("the pool has a block");
        part.fill(7);
        let parts = Parts::from(part.freeze());
        let entries = (0..5u32).map(|seq| {
            entry(
                1,
                a,
                Path::Live,
                u64::from(seq),
                1,
                Some(i64::from(seq) + 1),
                parts.clone(),
            )
        });
        buffer.append(entries).expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(5, Some(5)));
        drop(buffer);
        let mut slots = Slots::new();
        let opened = shard.open(ring, &mut slots).await;
        let tails = opened.map(|buffer| buffer.tail(slots.index(key(1)), Path::Live));
        assert_eq!(tails, Ok(tail(5, Some(5))));
    });
}

/// The walk reads a long record in pieces of the pool's largest block. An open
/// with no such block free fails with [`Error::Pool`] and leaves the ring as it
/// is; the next open recovers the record.
#[test]
fn an_open_with_no_largest_block_free_fails_and_the_next_recovers() {
    run(143, |mut shard| async move {
        let config = block::Config::new(640 << 10).expect("the budget fits");
        shard.pool =
            Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
        assert_eq!(shard.pool.largest(), 512 << 10);
        let ring = least(600_000);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let first = Parts::from(shard.block(512 << 10));
        let second = Parts::from(shard.block(60_000));
        buffer
            .append([
                entry(1, a, Path::Live, 0, 1, Some(1), first),
                entry(1, a, Path::Live, 1, 1, Some(2), second),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(2, Some(2)));
        drop(buffer);
        let held = shard.pool.alloc(150_000).expect("the pool has a block");
        let opened = shard.open(ring, &mut Slots::new()).await;
        let exhausted = block::Error::Exhausted {
            requested: 512 << 10,
            available: 306_688,
        };
        assert_eq!(opened.map(drop), Err(Error::Pool(exhausted)));
        drop(held);
        let mut slots = Slots::new();
        let opened = shard.open(ring, &mut slots).await;
        let tails = opened.map(|buffer| buffer.tail(slots.index(key(1)), Path::Live));
        assert_eq!(tails, Ok(tail(2, Some(2))));
    });
}

/// `append` refuses an entry whose parts, joined, pass `Buffer::block_max`, with
/// `Limit::Block` for the first such entry, and takes nothing. An entry of
/// `block_max` bytes commits, a read gives it, and an open recovers it.
#[test]
fn an_entry_over_block_max_is_large() {
    run(155, |mut shard| async move {
        let largest = 1 << 17;
        let config = block::Config::new(
            u64::try_from(block::footprint(largest)).expect("a usize fits in a u64"),
        )
        .expect("the budget fits");
        let own = Rc::clone(&shard.pool);
        shard.pool =
            Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
        assert_eq!(shard.pool.largest(), largest);
        let ring = least(600_000);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let max = buffer.block_max();
        assert_eq!(max, 61_441);
        let over = parts_of(&own, &[max, 1]);
        let more = parts_of(&own, &[max, 2]);
        let fits = parts_of(&own, &[max]);
        let a = slots.index(key(1));
        let large = buffer.append([
            entry(1, a, Path::Live, 0, 1, Some(1), over),
            entry(1, a, Path::Live, 1, 1, Some(2), more),
        ]);
        let limit = Limit::Block { len: max + 1, max };
        assert_eq!(large, Err(Rejected::Large(limit)));
        assert_eq!(
            Rejected::Large(limit).to_string(),
            "an entry has 61442 bytes of parts, and an entry of this buffer holds at \
             most 61441"
        );
        assert_eq!(buffer.tail(a, Path::Live), tail(0, None));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), fits)])
            .expect("queues");
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await;
        let lens =
            read.map(|read| read.entries.iter().map(|e| e.bytes.len()).collect());
        assert_eq!(lens, Ok(vec![max]));
        drop(buffer);
        let mut slots = Slots::new();
        let opened = shard.open(ring, &mut slots).await;
        let tails = opened.map(|buffer| buffer.tail(slots.index(key(1)), Path::Live));
        assert_eq!(tails, Ok(tail(1, Some(1))), "an open recovers it");
    });
}

/// One part of each of `lens` bytes from `pool`.
fn parts_of(pool: &Pool, lens: &[usize]) -> Parts {
    let mut blocks = lens.iter().map(|&len| {
        let mut block = pool.alloc(len).expect("the pool has a block");
        block.fill(7);
        block.freeze()
    });
    match (blocks.next(), blocks.next(), blocks.next()) {
        (first, None, None) => Parts::from(first),
        (Some(first), Some(second), None) => Parts::from([first, second]),
        _ => unreachable!("one part or two"),
    }
}

/// Opens a buffer on a new pool of `budget` bytes, then appends one entry whose
/// parts have the lengths that `split` gives for `Buffer::block_max`. The parts come
/// from the new pool when `from_buffer`, or else from the pool of the test. Gives
/// `block_max` and what the append and its commit returned.
async fn append_split(
    mut shard: Shard,
    budget: u64,
    from_buffer: bool,
    split: impl FnOnce(usize) -> Vec<usize>,
) -> (usize, Result<(), Rejected>) {
    let own = Rc::clone(&shard.pool);
    shard.pool = Rc::new(Pool::heap(
        block::Config::new(budget).expect("the budget fits"),
    ));
    let mut slots = Slots::new();
    let buffer = shard.open(least(600_000), &mut slots).await.expect("opens");
    let max = buffer.block_max();
    // The open leaves free blocks carved. A size whose blocks are all free goes
    // back at its second purge, so the entry meets a fresh pool.
    buffer.pool().purge();
    buffer.pool().purge();
    assert_eq!(buffer.pool().committed(), 0);
    assert!(
        max <= buffer.pool().largest(),
        "a read holds the entry in a block"
    );
    let pool = if from_buffer { buffer.pool() } else { &*own };
    let parts = parts_of(pool, &split(max));
    let a = slots.index(key(1));
    let appended = buffer.append([entry(1, a, Path::Live, 0, 1, Some(1), parts)]);
    if appended.is_ok() {
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
    }
    (max, appended)
}

/// The defect of #1053: an entry of the largest block, made from the pool of the
/// buffer, got `Pool(Exhausted)` on each try. It is over `block_max`.
#[test]
fn an_entry_of_the_largest_block_made_from_the_pool_of_the_buffer_is_large() {
    run(201, |shard| async move {
        let largest = 1 << 17;
        let budget = u64::try_from(block::footprint(largest)).expect("fits in u64");
        let (max, appended) =
            append_split(shard, budget, true, |_| vec![largest]).await;
        let limit = Limit::Block { len: largest, max };
        assert_eq!(appended, Err(Rejected::Large(limit)));
    });
}

/// The second case of #1053: an entry under the largest block that does not fit
/// beside the blocks of its group.
#[test]
fn an_entry_under_the_largest_block_made_from_the_pool_of_the_buffer_is_large() {
    run(202, |shard| async move {
        let (max, appended) =
            append_split(shard, 1 << 17, true, |_| vec![70_000]).await;
        let limit = Limit::Block { len: 70_000, max };
        assert_eq!(appended, Err(Rejected::Large(limit)));
    });
}

/// An entry of exactly `block_max` bytes, made from the pool of the buffer, appends
/// in one part and in the split that takes the most of the budget.
#[test]
fn an_entry_of_block_max_made_from_the_pool_of_the_buffer_appends() {
    let budget = u64::try_from(block::footprint(1 << 17)).expect("fits in u64");
    run(203, move |shard| async move {
        let appended = append_split(shard, budget, true, |max| vec![max]).await;
        assert_eq!(appended, (61_441, Ok(())));
    });
    run(204, move |shard| async move {
        let appended =
            append_split(shard, budget, true, |max| vec![57_345, max - 57_345]).await;
        assert_eq!(appended, (61_441, Ok(())));
    });
}

/// The limit of `block_max`: a free block of a size with a block in use keeps the
/// budget of its payload when the payload holds no whole page, as a payload of 4096
/// bytes that starts off a page. So an entry at `block_max` gets `Exhausted` when the
/// pool keeps a second block of the size of a part. After two purges the pool
/// commits nothing, and the same entry appends.
#[test]
fn an_entry_of_block_max_beside_a_free_block_of_its_size_is_exhausted() {
    run(206, |mut shard| async move {
        let budget = u64::try_from(block::footprint(1 << 17)).expect("fits in u64");
        let config = block::Config::new(budget).expect("the budget fits");
        shard.pool = Rc::new(Pool::heap(config));
        let mut slots = Slots::new();
        let buffer = shard.open(least(600_000), &mut slots).await.expect("opens");
        let pool = buffer.pool();
        pool.purge();
        pool.purge();
        let lens = [57_345, buffer.block_max() - 57_345];
        let a = slots.index(key(1));
        let parts = parts_of(pool, &lens);
        drop(pool.alloc(lens[1]).expect("a second block of the size"));
        let appended = buffer.append([entry(1, a, Path::Live, 0, 1, Some(1), parts)]);
        let exhausted = block::Error::Exhausted {
            requested: 52_186,
            available: 57_216,
        };
        assert_eq!(appended, Err(Rejected::Pool(exhausted)));
        pool.purge();
        pool.purge();
        assert_eq!(pool.committed(), 0);
        let parts = parts_of(pool, &lens);
        let appended = buffer.append([entry(1, a, Path::Live, 0, 1, Some(1), parts)]);
        assert_eq!(appended, Ok(()));
        buffer.committed().await.expect("commits");
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
    });
}

/// An entry of `block_max` bytes in two parts, a head of 30 bytes and a body, as
/// `home` makes a stored body, appends from the pool of a buffer just opened, with no
/// purge: the open holds no block of the pool.
#[test]
fn an_entry_at_block_max_with_a_short_head_appends_after_an_open() {
    for (case, budget, max) in [(207, 62_272, 4096), (208, 68_000, 8_257)] {
        run(case, move |mut shard| async move {
            let config = block::Config::new(budget).expect("the budget fits");
            shard.pool = Rc::new(Pool::heap(config));
            let mut slots = Slots::new();
            let buffer = shard.open(least(600_000), &mut slots).await.expect("opens");
            assert_eq!(buffer.block_max(), max);
            let a = slots.index(key(1));
            for first in 0..3 {
                let parts = parts_of(buffer.pool(), &[30, max - 30]);
                let appended =
                    buffer.append([entry(1, a, Path::Live, first, 1, Some(1), parts)]);
                assert_eq!(appended, Ok(()), "budget {budget}, append {first}");
                buffer.committed().await.expect("commits");
            }
        });
    }
}

/// An open fails with `Budget` when its pool holds no entry of
/// `Layout::ENTRY_MAX_MIN` bytes beside the blocks of a group, also when its largest
/// block is under that entry, and opens from `min`.
#[test]
fn an_open_fails_on_a_pool_that_holds_no_entry() {
    run(205, |mut shard| async move {
        let config = block::Config::new(62_271).expect("the budget fits");
        shard.pool = Rc::new(Pool::heap(config));
        let opened = shard.open(least(600_000), &mut Slots::new()).await;
        let error = opened.map(drop).expect_err("the open fails");
        assert_eq!(
            error,
            Error::Budget {
                budget: 62_271,
                min: 62_272
            }
        );
        assert_eq!(
            error.to_string(),
            "a pool of 62271 bytes holds no entry of 4032 bytes: give it 62272 bytes \
             or more"
        );
        let config = block::Config::new(1_000).expect("the budget fits");
        shard.pool = Rc::new(Pool::heap(config));
        let opened = shard.open(least(600_000), &mut Slots::new()).await;
        let error = opened.map(drop).expect_err("the open fails");
        assert_eq!(
            error,
            Error::Budget {
                budget: 1_000,
                min: 62_272
            }
        );
        let names = shard.files.list(FilePath::new("")).await.expect("lists");
        assert_eq!(names, Vec::<PathBuf>::new());
        let config = block::Config::new(62_272).expect("the budget fits");
        shard.pool = Rc::new(Pool::heap(config));
        let opened = shard.open(least(600_000), &mut Slots::new()).await;
        assert_eq!(opened.map(|buffer| buffer.block_max()), Ok(4096));
    });
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// On a pool of any budget that commits nothing before it takes the entry's
    /// parts, an entry of `block_max` bytes in any split into one or two parts made
    /// from that pool appends, and one byte more gets `Limit::Block`.
    #[test]
    fn block_max_bounds_an_entry_in_any_split(
        seed in any::<u64>(),
        budget in 62_272..=1u64 << 19,
        two in any::<bool>(),
        cut in 0.0..=1.0f64,
        over in any::<bool>(),
    ) {
        let split = move |len: usize| {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "a fraction of a length under 2^53"
            )]
            let first = (len as f64 * cut) as usize;
            if two { vec![first, len - first] } else { vec![len] }
        };
        run(seed, move |shard| async move {
            let (max, appended) = if over {
                append_split(shard, budget, false, |max| split(max + 1)).await
            } else {
                append_split(shard, budget, true, split).await
            };
            let expected = if over {
                Err(Rejected::Large(Limit::Block { len: max + 1, max }))
            } else {
                Ok(())
            };
            assert_eq!(appended, expected);
        });
    }
}

/// An open fails with `Pool(TooLarge)` when a recovered entry is over the largest
/// block of its pool, as after a restart with a smaller budget, and leaves the ring
/// as it is: an open with the larger pool recovers the entry.
#[test]
fn an_open_with_an_entry_over_the_largest_pool_block_fails() {
    run(156, |mut shard| async move {
        let ring = least(600_000);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let bytes = Parts::from(shard.block(200_000));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), bytes)])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        let larger = Rc::clone(&shard.pool);
        let config = block::Config::new(1 << 17).expect("the budget fits");
        shard.pool =
            Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
        let opened = shard.open(ring, &mut Slots::new()).await;
        let large = block::Error::TooLarge {
            requested: 200_000,
            largest: 114_688,
        };
        assert_eq!(opened.map(drop), Err(Error::Pool(large.clone())));
        assert_eq!(
            Error::Pool(large).to_string(),
            "the pool has no block: block of 200000 bytes is above the largest block \
             of 114688 bytes"
        );
        shard.pool = larger;
        let mut slots = Slots::new();
        let opened = shard.open(ring, &mut slots).await;
        let tails = opened.map(|buffer| buffer.tail(slots.index(key(1)), Path::Live));
        assert_eq!(tails, Ok(tail(1, Some(1))));
    });
}

/// A batch of one entry with `entry_max` bytes of parts commits after an entry that
/// opened a group, and is recovered at the next open.
#[test]
fn one_entry_of_the_entry_max_commits_and_is_recovered() {
    run(110, |shard| async move {
        let ring = layout(AREA, BODY_MAX);
        let mut slots = Slots::new();
        let buffer = shard.open(ring, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let small = Parts::from(shard.block(10));
        let max = Parts::from(shard.block(buffer.layout().entry_max()));
        assert_eq!(buffer.layout().entry_max(), 4032);
        let appended = buffer.append([entry(1, a, Path::Live, 0, 1, None, small)]);
        assert_eq!(appended, Ok(()));
        let appended = buffer.append([entry(1, a, Path::Live, 1, 1, Some(1), max)]);
        assert_eq!(appended, Ok(()));
        buffer.committed().await.expect("commits");
        drop(buffer);
        let mut slots = Slots::new();
        let opened = shard.open(ring, &mut slots).await;
        let tails = opened.map(|buffer| buffer.tail(slots.index(key(1)), Path::Live));
        assert_eq!(tails, Ok(tail(2, Some(1))));
    });
}

/// A commit future whose group commit synced resolves well, also when a later
/// commit failed before its first poll: every entry appended before the call is
/// durable.
#[test]
fn a_synced_commit_resolves_well_after_a_later_commit_failed() {
    run_on_memory(120, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let first = buffer.committed();
        shard.clock.sleep(commits(3)).await;
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
        shard.memory().fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 3, 1, None, Parts::default())])
            .expect("queues");
        shard.clock.sleep(commits(3)).await;
        assert_eq!(
            shard.memory().syncs(),
            5,
            "the second commit failed its sync"
        );
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
        let ended = Err(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        assert_eq!(buffer.committed().await, ended, "the buffer ended");
        assert_eq!(first.await, Ok(()), "its entries are durable");
    });
}

/// A resolved `Commit` leaves no waker behind, so the buffer idles after it and a
/// drop ends the task at once.
#[test]
fn a_drop_after_an_abandoned_commit_ends_the_task_at_once() {
    run(123, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        shard.clock.sleep(commits(21)).await;
        {
            let mut commit = pin!(buffer.committed());
            let polled = poll_fn(|cx| Poll::Ready(commit.as_mut().poll(cx))).await;
            assert_eq!(polled, Poll::Ready(Ok(())), "nothing waits");
        }
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() / 10))
            .await;
        drop(buffer);
        shard
            .clock
            .sleep(Span::from_nanos(COMMIT.nanos() / 4))
            .await;
        assert!(!shard.ring_held().await, "the task holds the ring open");
    });
}

/// Two futures see the same durable entries: one made before the commit that
/// synced them, one made after it with nothing pending. A later commit fails.
/// Both resolve well: every entry appended before each call is durable.
#[test]
fn a_commit_made_after_its_entries_synced_resolves_well_after_a_later_failure() {
    run_on_memory(122, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        let before = buffer.committed();
        shard.clock.sleep(commits(3)).await;
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
        let after = buffer.committed();
        shard.memory().fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 3, 1, None, Parts::default())])
            .expect("queues");
        shard.clock.sleep(commits(3)).await;
        assert_eq!(
            shard.memory().syncs(),
            5,
            "the second commit failed its sync"
        );
        assert_eq!(buffer.durable(a, Path::Live), tail(3, Some(30)));
        assert_eq!(before.await, Ok(()), "its entries are durable");
        assert_eq!(after.await, Ok(()), "the same entries are durable");
    });
}

/// A `committed` called while a sync runs, with nothing appended since, waits for
/// that sync only: it resolves when the sync ends, not one commit span later.
#[test]
fn a_commit_awaited_during_a_slow_sync_resolves_when_the_sync_ends() {
    run_on_memory(124, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let tenth = COMMIT.nanos() / 10;
        shard
            .memory()
            .slow_syncs(shard.clock.clone(), Span::from_nanos(tenth * 15));
        let opened = shard.clock.now();
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(Span::from_nanos(tenth * 12)).await;
        buffer.committed().await.expect("commits");
        assert_eq!(shard.clock.now() - opened, Span::from_nanos(tenth * 25));
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
    });
}

/// A commit future made while a deadline is in flight, with nothing open or queued,
/// resolves well when that deadline syncs, also when the next one fails.
#[test]
fn a_commit_made_during_a_sync_resolves_with_that_sync() {
    run_on_memory(131, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let sync = Span::from_nanos(COMMIT.nanos() * 4 / 10);
        shard.memory().slow_syncs(shard.clock.clone(), sync);
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        let half = Span::from_nanos(COMMIT.nanos() + sync.nanos() / 2);
        shard.clock.sleep(half).await;
        let first = buffer.committed();
        shard.clock.sleep(commits(1)).await;
        assert_eq!(buffer.durable(a, Path::Live), tail(1, Some(1)));
        shard.memory().fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues");
        shard.clock.sleep(commits(3)).await;
        assert_eq!(
            shard.memory().syncs(),
            5,
            "the second commit failed its sync"
        );
        assert_eq!(first.await, Ok(()), "its entries synced");
    });
}

/// A `Commit` held across two commits after the one that synced its entries still
/// resolves well: `commits` passed its target, it did not land on it.
#[test]
fn a_commit_held_across_two_later_commits_resolves_well() {
    run(125, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        let held = buffer.committed();
        for seq in 1..3 {
            shard.clock.sleep(commits(3)).await;
            buffer
                .append([entry(1, a, Path::Live, seq, 1, None, Parts::default())])
                .expect("queues");
        }
        shard.clock.sleep(commits(3)).await;
        assert_eq!(buffer.commits(), 3, "two commits ran after its own");
        assert_eq!(held.await, Ok(()), "its entries are durable");
    });
}

/// A `Commit` does not borrow its buffer: the buffer moves and takes an append
/// while the future is pending, and the future resolves with its entries durable.
#[test]
fn a_commit_outlives_the_borrow_of_its_buffer() {
    run(133, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        let commit = buffer.committed();
        let moved = Box::new(buffer);
        moved
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues while the future is pending");
        assert_eq!(commit.await, Ok(()), "its entries are durable");
        assert_eq!(moved.durable(a, Path::Live), tail(2, Some(2)));
    });
}

/// A `Commit` held past the drop of its buffer during a sync resolves with the
/// sync's result, and the ring closes when the future drops.
#[test]
fn a_commit_held_past_the_drop_during_a_sync_resolves_with_the_sync() {
    run_on_memory(134, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(12)).await;
        let commit = buffer.committed();
        drop(buffer);
        shard.clock.sleep(tenths(4)).await;
        assert_eq!(
            shard.memory().open_files(),
            1,
            "the future holds the ring open"
        );
        assert_eq!(commit.await, Ok(()), "the sync made its entries durable");
        assert_eq!(
            shard.memory().open_files(),
            0,
            "the ring closed with the future"
        );
    });
}

/// A `Commit` held past the drop of its buffer during a failing sync resolves
/// with the error that ended the buffer.
#[test]
fn a_commit_held_past_the_drop_during_a_failing_sync_resolves_with_its_error() {
    run_on_memory(135, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        shard.memory().fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(12)).await;
        let commit = buffer.committed();
        drop(buffer);
        let ended = Err(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        assert_eq!(commit.await, ended, "the sync failed");
        assert_eq!(
            shard.memory().open_files(),
            0,
            "the ring closed with the future"
        );
    });
}

/// A `Commit` taken before a later append and held past the drop resolves once the
/// task ended, here after it wrote that append, so a reopen recovers both entries.
#[test]
fn a_commit_held_past_the_drop_resolves_after_the_last_write() {
    run_on_memory(137, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        let commit = buffer.committed();
        shard.clock.sleep(tenths(12)).await;
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues while the first sync runs");
        drop(buffer);
        let dropped = shard.clock.now();
        assert_eq!(commit.await, Ok(()), "the task wrote both entries");
        assert_eq!(
            shard.clock.now() - dropped,
            tenths(12),
            "after the second sync"
        );
        assert_eq!(shard.memory().syncs(), 5);
        assert_eq!(shard.memory().open_files(), 0, "the task ended");
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!(
            buffer.tail(slots.index(key(1)), Path::Live),
            tail(2, Some(2))
        );
    });
}

/// A drop while a commit runs, with nothing queued, ends the task at the end of that
/// commit: not at once, and not at its next deadline.
#[test]
fn a_drop_with_nothing_queued_ends_the_task_at_the_commit_in_flight() {
    run_on_memory(162, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(12)).await;
        drop(buffer);
        shard.clock.sleep(tenths(1)).await;
        assert_eq!(shard.memory().open_files(), 1, "the sync still runs");
        shard.clock.sleep(tenths(2)).await;
        assert_eq!(shard.memory().open_files(), 0, "the task ended at the sync");
    });
}

/// A drop while a commit runs ends the task when that commit fails: it never writes
/// the entries queued at the drop, nor waits for its next deadline.
#[test]
fn a_drop_ends_the_task_at_a_failed_commit_in_flight() {
    run_on_memory(161, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let opened = shard.memory().syncs();
        shard.memory().slow_syncs(shard.clock.clone(), tenths(4));
        shard.memory().fail_syncs();
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        shard.clock.sleep(tenths(12)).await;
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues while the first sync runs");
        let commit = buffer.committed();
        drop(buffer);
        let dropped = shard.clock.now();
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(commit.await, Err(failed));
        assert_eq!(shard.clock.now() - dropped, tenths(2), "at the failed sync");
        assert_eq!(
            shard.memory().syncs() - opened,
            1,
            "no write after the drop"
        );
        assert_eq!(shard.memory().open_files(), 0, "the task ended");
    });
}

/// A `Commit` on an entry queued at the drop resolves with the error of the write
/// after the drop.
#[test]
fn a_commit_on_an_entry_queued_at_the_drop_resolves_with_a_failed_write() {
    run(138, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        shard.node().fail_file(FilePath::new(RING), Operation::Sync);
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        let commit = buffer.committed();
        drop(buffer);
        let ended = Err(FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        assert_eq!(commit.await, ended, "the write after the drop failed");
        assert!(!shard.ring_held().await, "the task ended");
    });
}

/// A synced `Commit` held past the drop resolves well when the write after the
/// drop fails.
#[test]
fn a_synced_commit_held_past_the_drop_resolves_well_after_a_failed_write() {
    run(139, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 1, Some(1), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let first = buffer.committed();
        shard.node().fail_file(FilePath::new(RING), Operation::Sync);
        buffer
            .append([entry(1, a, Path::Live, 1, 1, Some(2), Parts::default())])
            .expect("queues");
        drop(buffer);
        assert_eq!(
            first.await,
            Ok(()),
            "its entries were durable before the drop"
        );
        assert!(!shard.ring_held().await, "the task ended");
    });
}

/// Three paths over three commits: a read from the start gives each path's entries
/// in order, with their headers and bytes, and never another path's. A path with
/// no entry gives nothing.
#[test]
fn a_read_gives_the_entries_of_one_path_in_order() {
    run(144, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let reader = buffer.reader();
        let a = slots.index(key(1));
        let b = slots.index(key(2));
        let live = |first, len, last, bytes| {
            entry(
                1,
                a,
                Path::Live,
                first,
                len,
                last,
                shard.block(bytes).into(),
            )
        };
        for batch in [
            vec![
                live(0, 3, Some(30), 100),
                entry(1, a, Path::Backfill, 0, 1, Some(1), shard.block(7).into()),
            ],
            vec![
                entry(2, b, Path::Live, 0, 2, None, shard.block(20).into()),
                live(3, 1, None, 300),
            ],
            vec![
                live(4, 2, Some(60), 0),
                entry(2, b, Path::Live, 2, 2, Some(4), shard.block(21).into()),
            ],
        ] {
            buffer.append(batch).expect("queues");
            buffer.committed().await.expect("commits");
        }
        let read = reader
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        let expected = vec![
            stored(0, 3, Some(30), shard.block(100)),
            stored(3, 1, None, shard.block(300)),
            stored(4, 2, Some(60), shard.block(0)),
        ];
        assert_eq!(read, whole(expected, mark(6, 0)));
        let read = reader
            .read(a, Path::Backfill, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        let expected = vec![stored(0, 1, Some(1), shard.block(7))];
        assert_eq!(read, whole(expected, mark(1, 0)));
        let read = reader
            .read(b, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        let expected = vec![
            stored(0, 2, None, shard.block(20)),
            stored(2, 2, Some(4), shard.block(21)),
        ];
        assert_eq!(read, whole(expected, mark(4, 0)));
        let read = reader
            .read(b, Path::Backfill, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(read, whole(Vec::new(), mark(0, 0)));
    });
}

/// A budget of pool bytes splits the log over reads, each passing the budget by
/// less than one entry's block. An entry with no bytes costs its block. Reads that
/// follow `next` give each entry once, zero-length entries included, and the last
/// read gives nothing.
#[test]
fn a_budget_splits_the_log_over_reads_that_follow_next() {
    run(145, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let live = |first, len, last, bytes| {
            entry(
                1,
                a,
                Path::Live,
                first,
                len,
                last,
                shard.block(bytes).into(),
            )
        };
        buffer
            .append([live(0, 3, Some(30), 100), live(3, 0, None, 0)])
            .expect("queues");
        buffer.committed().await.expect("commits");
        buffer
            .append([live(3, 2, Some(50), 50), live(5, 0, None, 0)])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let all = vec![
            stored(0, 3, Some(30), shard.block(100)),
            stored(3, 0, None, shard.block(0)),
            stored(3, 2, Some(50), shard.block(50)),
            stored(5, 0, None, shard.block(0)),
        ];
        let reads = read_all(&buffer, a, Path::Live, Mark::at(0), 1).await;
        let expected = vec![
            whole(all[..1].to_vec(), mark(3, 0)),
            whole(all[1..2].to_vec(), mark(3, 1)),
            whole(all[2..3].to_vec(), mark(5, 0)),
            whole(all[3..].to_vec(), mark(5, 1)),
            whole(Vec::new(), mark(5, 1)),
        ];
        assert_eq!(reads, expected, "a budget of one byte");
        let budget = block::footprint(100) + block::footprint(0);
        let reads = read_all(&buffer, a, Path::Live, Mark::at(0), budget).await;
        let expected = vec![
            whole(all[..2].to_vec(), mark(3, 1)),
            whole(all[2..].to_vec(), mark(5, 1)),
            whole(Vec::new(), mark(5, 1)),
        ];
        assert_eq!(reads, expected, "a budget of the first two blocks");
        let reads = read_all(&buffer, a, Path::Live, Mark::at(0), usize::MAX).await;
        let expected = vec![whole(all, mark(5, 1)), whole(Vec::new(), mark(5, 1))];
        assert_eq!(reads, expected, "no budget");
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), 0)
            .await
            .expect("reads");
        assert_eq!(read, whole(Vec::new(), mark(0, 0)), "a budget of zero");
    });
}

/// A handoff at the tail is given once with its tag. A read from `next` gives
/// nothing until the next frame is durable.
#[test]
fn a_handoff_at_the_tail_is_given_once() {
    run(146, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let frame = entry(1, a, Path::Live, 0, 3, Some(30), shard.block(10).into());
        buffer.append([frame]).expect("queues");
        buffer.committed().await.expect("commits");
        let handoff = Entry {
            tag: 2,
            ..entry(1, a, Path::Live, 3, 0, None, Parts::default())
        };
        buffer.append([handoff]).expect("queues");
        buffer.committed().await.expect("commits");
        let expected = vec![
            stored(0, 3, Some(30), shard.block(10)),
            Stored {
                tag: 2,
                ..stored(3, 0, None, shard.block(0))
            },
        ];
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(read, whole(expected, mark(3, 1)));
        let read = buffer
            .reader()
            .read(a, Path::Live, mark(3, 1), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(read, whole(Vec::new(), mark(3, 1)), "after the handoff");
        let frame = entry(1, a, Path::Live, 3, 2, Some(50), shard.block(11).into());
        buffer.append([frame]).expect("queues");
        let read = buffer
            .reader()
            .read(a, Path::Live, mark(3, 1), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(
            read,
            whole(Vec::new(), mark(3, 1)),
            "the frame is not durable"
        );
        buffer.committed().await.expect("commits");
        let read = buffer
            .reader()
            .read(a, Path::Live, mark(3, 1), usize::MAX)
            .await
            .expect("reads");
        let expected = vec![stored(3, 2, Some(50), shard.block(11))];
        assert_eq!(read, whole(expected, mark(5, 0)), "the frame is durable");
    });
}

/// A read stops before a skip ahead. The next read reports the seqs the skip left
/// out as a gap and gives the entries after it, and so does a read from a mark in
/// the gap.
#[test]
fn a_read_stops_before_a_skip_and_the_next_reports_the_gap() {
    run(147, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let live = |first, len, last, bytes| {
            entry(
                1,
                a,
                Path::Live,
                first,
                len,
                last,
                shard.block(bytes).into(),
            )
        };
        buffer
            .append([live(0, 3, Some(30), 30), live(5, 2, Some(70), 70)])
            .expect("queues");
        buffer.committed().await.expect("commits");
        buffer.append([live(7, 1, None, 80)]).expect("queues");
        buffer.committed().await.expect("commits");
        let before = vec![stored(0, 3, Some(30), shard.block(30))];
        let after = vec![
            stored(5, 2, Some(70), shard.block(70)),
            stored(7, 1, None, shard.block(80)),
        ];
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(read, whole(before, mark(3, 0)), "stops before the skip");
        let read = buffer
            .reader()
            .read(a, Path::Live, mark(3, 0), usize::MAX)
            .await
            .expect("reads");
        let expected = Read {
            gap: Some(3..5),
            entries: after.clone(),
            next: mark(8, 0),
        };
        assert_eq!(read, expected, "reports the gap");
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(4), usize::MAX)
            .await
            .expect("reads");
        let expected = Read {
            gap: Some(4..5),
            entries: after,
            next: mark(8, 0),
        };
        assert_eq!(read, expected, "from a mark in the gap");
    });
}

/// Entries not yet synced are not given. After `committed`, they are.
#[test]
fn a_read_gives_only_durable_entries() {
    run(148, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let frame = entry(1, a, Path::Live, 0, 3, Some(30), shard.block(10).into());
        buffer.append([frame]).expect("queues");
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(read, whole(Vec::new(), mark(0, 0)), "before the commit");
        buffer.committed().await.expect("commits");
        let frame = entry(1, a, Path::Live, 3, 1, Some(40), shard.block(11).into());
        buffer.append([frame]).expect("queues");
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        let expected = vec![stored(0, 3, Some(30), shard.block(10))];
        assert_eq!(read, whole(expected, mark(3, 0)), "after the first commit");
        buffer.committed().await.expect("commits");
        let read = buffer
            .reader()
            .read(a, Path::Live, mark(3, 0), usize::MAX)
            .await
            .expect("reads");
        let expected = vec![stored(3, 1, Some(40), shard.block(11))];
        assert_eq!(read, whole(expected, mark(4, 0)), "after the second commit");
    });
}

/// A mark inside an entry gives that entry whole. A mark at or past the tail gives
/// nothing, and `next` is the mark.
#[test]
fn a_mark_inside_an_entry_gives_it_whole_and_one_past_the_tail_gives_nothing() {
    run(149, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let live = |first, len, last, bytes| {
            entry(
                1,
                a,
                Path::Live,
                first,
                len,
                last,
                shard.block(bytes).into(),
            )
        };
        buffer.append([live(0, 3, Some(30), 30)]).expect("queues");
        buffer.committed().await.expect("commits");
        buffer.append([live(3, 4, Some(70), 70)]).expect("queues");
        buffer.committed().await.expect("commits");
        let all = vec![
            stored(0, 3, Some(30), shard.block(30)),
            stored(3, 4, Some(70), shard.block(70)),
        ];
        for seq in [1, 2] {
            let read = buffer
                .reader()
                .read(a, Path::Live, Mark::at(seq), usize::MAX)
                .await
                .expect("reads");
            assert_eq!(read, whole(all.clone(), mark(7, 0)), "from {seq}");
        }
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(5), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(read, whole(all[1..].to_vec(), mark(7, 0)), "from 5");
        for from in [mark(7, 0), mark(7, 5), mark(9, 0)] {
            let read = buffer
                .reader()
                .read(a, Path::Live, from, usize::MAX)
                .await
                .expect("reads");
            assert_eq!(read, whole(Vec::new(), from), "from {from:?}");
        }
    });
}

/// A failed read of the ring gives its error, and a later read passes.
#[test]
fn a_failed_ring_read_gives_its_error_and_a_later_read_passes() {
    let (mut sim, node) = create_node(150);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        node.fail_file(FilePath::new(RING), Operation::ReadAt);
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::ReadAt,
            code: 5,
        };
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await;
        assert_eq!(read, Err(Error::Files(failed)));
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("the next read passes");
        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].first, 0);
        assert_eq!(read.entries[0].samples, samples(3, Some(30)));
        assert_eq!(&*read.entries[0].bytes, &[]);
        assert_eq!(read.next, mark(3, 0));
    })
    .expect("the buffer ends");
}

/// A pool with no block for the table gives `Error::Pool`, and a read after the
/// blocks come back passes.
#[test]
fn a_read_with_no_block_for_the_table_gives_the_pool_error() {
    run(151, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let frame = entry(1, a, Path::Live, 0, 3, Some(30), shard.block(10).into());
        buffer.append([frame]).expect("queues");
        buffer.committed().await.expect("commits");
        let mut held = Vec::new();
        let mut len = shard.pool.largest();
        while len > 0 {
            while let Ok(block) = shard.pool.alloc(len) {
                held.push(block);
            }
            len -= len.div_ceil(16);
        }
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await;
        let exhausted = block::Error::Exhausted {
            requested: 4096,
            available: 0,
        };
        assert_eq!(read, Err(Error::Pool(exhausted)));
        let read = buffer.reader().read(a, Path::Live, Mark::at(0), 0).await;
        let empty = whole(Vec::new(), mark(0, 0));
        assert_eq!(read, Ok(empty), "a budget of zero takes no block");
        drop(held);
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("the next read passes");
        let expected = vec![stored(0, 3, Some(30), shard.block(10))];
        assert_eq!(read, whole(expected, mark(3, 0)));
    });
}

/// After a failed sync, a read gives the error that ended the buffer.
#[test]
fn a_read_after_a_failed_sync_gives_the_error_that_ended_the_buffer() {
    run(152, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let frame = entry(1, a, Path::Live, 0, 3, Some(30), shard.block(10).into());
        buffer.append([frame]).expect("queues");
        buffer.committed().await.expect("commits");
        shard.node().fail_file(FilePath::new(RING), Operation::Sync);
        buffer
            .append([entry(1, a, Path::Live, 3, 1, None, Parts::default())])
            .expect("queues");
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(buffer.committed().await, Err(failed.clone()));
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await;
        assert_eq!(read, Err(Error::Files(failed)));
    });
}

/// Reads run back to back while a commit's sync fails. Each read gives its 20
/// durable entries or, once the sync failed, the error that ended the buffer, also
/// a read in flight when the sync failed.
#[test]
fn a_read_across_a_failed_sync_gives_the_error_that_ended_the_buffer() {
    let (mut sim, node) = create_node(160);
    let errors = sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let pool = Rc::clone(&config.pool);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let batch: Vec<Entry> = (0..20)
            .map(|first| {
                let bytes = pool.alloc(8).expect("a block").freeze();
                entry(1, a, Path::Live, first, 1, None, Parts::from(bytes))
            })
            .collect();
        buffer.append(batch).expect("queues");
        buffer.committed().await.expect("commits");
        node.fail_file(FilePath::new(RING), Operation::Sync);
        buffer
            .append([entry(1, a, Path::Live, 20, 1, None, Parts::default())])
            .expect("queues");
        let mut errors = Vec::new();
        while errors.len() < 2 {
            match buffer
                .reader()
                .read(a, Path::Live, Mark::at(0), usize::MAX)
                .await
            {
                Ok(read) => assert_eq!(read.entries.len(), 20),
                Err(error) => errors.push(error),
            }
        }
        errors
    });
    let failed = Error::Files(FileError::Io {
        path: PathBuf::from(RING),
        operation: Operation::Sync,
        code: 5,
    });
    assert_eq!(errors.expect("the run ends"), [failed.clone(), failed]);
}

/// A read that holds entries ends where the pool has no block for the next one,
/// as at its budget, and the next read goes on from there.
#[test]
fn a_read_ends_at_a_pool_shortage_and_keeps_what_it_holds() {
    run(153, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let first = entry(1, a, Path::Live, 0, 3, Some(30), shard.block(10).into());
        buffer.append([first]).expect("queues");
        buffer.committed().await.expect("commits");
        let second = entry(1, a, Path::Live, 3, 2, Some(50), shard.block(12).into());
        buffer.append([second]).expect("queues");
        buffer.committed().await.expect("commits");
        let one = shard
            .pool
            .alloc(4096)
            .expect("a block for a table, then an entry");
        let mut held = Vec::new();
        let mut len = shard.pool.largest();
        while len > 0 {
            while let Ok(block) = shard.pool.alloc(len) {
                held.push(block);
            }
            len -= len.div_ceil(16);
        }
        drop(one);
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("the read keeps the first entry");
        let expected = vec![stored(0, 3, Some(30), shard.block(10))];
        assert_eq!(read, whole(expected, mark(3, 0)));
        drop(held);
        let read = buffer
            .reader()
            .read(a, Path::Live, read.next, usize::MAX)
            .await
            .expect("the next read passes");
        let expected = vec![stored(3, 2, Some(50), shard.block(12))];
        assert_eq!(read, whole(expected, mark(5, 0)));
    });
}

/// A record whose header and entry table pass one 4 KiB block is read whole.
#[test]
fn a_record_with_a_table_over_one_block_is_read() {
    run(154, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(2 * AREA, BODY_MAX * 4), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let batch: Vec<_> = (0..100)
            .map(|first| entry(1, a, Path::Live, first, 1, None, shard.block(3).into()))
            .collect();
        buffer.append(batch).expect("queues");
        buffer.committed().await.expect("commits");
        let read = buffer
            .reader()
            .read(a, Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        let expected = (0..100).map(|first| stored(first, 1, None, shard.block(3)));
        assert_eq!(read, whole(expected.collect(), mark(100, 0)));
    });
}

/// Commits one entry, appends a second, and cuts the power `cut` nanoseconds after
/// a point before the deadline of the second commit. That deadline is one commit
/// span after the start of the first commit, whose write and sync take up to 100 µs
/// each. Returns the sim, the node, and whether the second commit had ended.
fn cut_the_second_commit(seed: u64, cut: i64) -> (sim::Sim, sim::node::Node, bool) {
    let (mut sim, node) = create_node(seed);
    let committed = Arc::new(AtomicBool::new(false));
    let commit = Arc::clone(&committed);
    let first = node.clone();
    drop(on_node(&node, "first", move |tasks| async move {
        let mut slots = Slots::new();
        let config = node_config(&first, tasks, DIR);
        let buffer = Buffer::open(config, &mut slots)
            .await
            .expect("the first open ends well");
        let a = slots.index(key(1));
        buffer
            .append([entry(1, a, Path::Live, 0, 3, Some(30), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        commit.store(true, Ordering::Relaxed);
        buffer
            .append([entry(1, a, Path::Live, 3, 2, Some(50), Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        commit.store(true, Ordering::Relaxed);
        std::future::pending::<()>().await;
    }));
    let step = 10_000;
    while !committed.load(Ordering::Relaxed) {
        sim.run_for(Span::from_nanos(step))
            .expect("the run goes on");
    }
    committed.store(false, Ordering::Relaxed);
    let rest = Span::from_nanos(COMMIT.nanos() - 25 * step + cut);
    sim.run_for(rest).expect("the run goes on");
    sim.crash(&node, sim::Crash::Power);
    (sim, node, committed.load(Ordering::Relaxed))
}

/// After a power cut at any point of a commit, a read gives the entries the
/// recovered tail reports, and nothing else.
#[test]
fn a_read_after_a_power_cut_gives_the_entries_the_tail_reports() {
    each_cut(0..16, 10_000, |seed, cut| {
        let (mut sim, node, ended) = cut_the_second_commit(seed, cut);
        let read = sim.run_on(&node, |node, tasks| async move {
            let mut slots = Slots::new();
            let buffer = Buffer::open(node_config(&node, tasks, DIR), &mut slots)
                .await
                .expect("opens after the power cut");
            let a = slots.index(key(1));
            let tail = buffer.tail(a, Path::Live);
            let reads = read_all(&buffer, a, Path::Live, Mark::at(0), 1).await;
            (tail, reads)
        });
        let (tail, reads) = read.unwrap_or_else(|e| panic!("cut at {cut} ns: {e}"));
        let seqs: Vec<Range<u64>> = entries(&reads).iter().map(Stored::seqs).collect();
        let entries = if tail.seq == 5 { 2 } else { 1 };
        assert_eq!(
            seqs,
            &[0..3, 3..5][..entries],
            "seed {seed}, cut at {cut} ns, tail {tail:?}"
        );
        assert!(cut != 0 || !ended, "seed {seed}: the cut missed the commit");
        ended
    });
}

/// An entry of `index` with `tag`, no samples at `first`, and `bytes`.
fn tagged(
    index: u32,
    slot: Slot,
    path: Path,
    first: u64,
    tag: u8,
    bytes: Block,
) -> Entry {
    Entry {
        tag,
        ..entry(index, slot, path, first, 0, None, bytes.into())
    }
}

/// What `newest` gives: the bytes of each entry, with its slot.
type Kept = Vec<(Slot, Box<[u8]>)>;

/// What `newest` gives back for an entry of `bytes`.
fn kept(bytes: &[u8]) -> Box<[u8]> {
    Box::from(bytes)
}

/// What `newest` gives for `tag`, which is over 0.
fn newest(buffer: &Buffer, path: Path, tag: u8) -> Kept {
    buffer.newest(path, NonZeroU8::new(tag).expect("a tag over 0"))
}

/// `newest` gives the last durable entry with the tag on the path of each index,
/// across records and within one, also after a reopen. An index with no such entry
/// is absent.
#[test]
fn newest_gives_the_last_durable_entry_of_the_tag_on_the_path_of_each_index() {
    run(170, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let (a, b, c) = (
            slots.index(key(1)),
            slots.index(key(2)),
            slots.index(key(3)),
        );
        let data = |index, slot, first| {
            entry(
                index,
                slot,
                Path::Live,
                first,
                1,
                Some(9),
                shard.block(20).into(),
            )
        };
        buffer
            .append([
                tagged(1, a, Path::Live, 0, 1, shard.block(1)),
                data(3, c, 0),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        buffer
            .append([
                tagged(1, a, Path::Live, 0, 1, shard.block(2)),
                tagged(1, a, Path::Live, 0, 1, shard.block(3)),
                tagged(2, b, Path::Live, 0, 1, shard.block(4)),
                tagged(1, a, Path::Backfill, 0, 1, shard.block(5)),
                tagged(2, b, Path::Live, 0, 2, shard.block(6)),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        buffer
            .append([data(1, a, 0), data(2, b, 0), data(3, c, 1)])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let expected = vec![(a, kept(&shard.block(3))), (b, kept(&shard.block(4)))];
        assert_eq!(newest(&buffer, Path::Live, 1), expected);
        let backfill = vec![(a, kept(&shard.block(5)))];
        assert_eq!(newest(&buffer, Path::Backfill, 1), backfill);
        assert_eq!(newest(&buffer, Path::Live, 4), Vec::new());
        buffer
            .append([tagged(1, a, Path::Live, 1, 1, shard.block(7))])
            .expect("queues");
        buffer.committed().await.expect("commits");
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        let (a, b) = (slots.index(key(1)), slots.index(key(2)));
        let reopened = vec![(a, kept(&shard.block(7))), (b, kept(&shard.block(4)))];
        assert_eq!(newest(&buffer, Path::Live, 1), reopened);
    });
}

/// Entries of tags 1 and 2 on both paths of two indexes: a reopen gives the same
/// entries for each tag and path.
#[test]
fn newest_gives_the_copy_after_a_reopen() {
    run(189, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let (a, b) = (slots.index(key(1)), slots.index(key(2)));
        let mut len = 0;
        let mut batch = Vec::new();
        for (index, slot) in [(1, a), (2, b)] {
            for path in [Path::Live, Path::Backfill] {
                for tag in [1, 2] {
                    len += 1;
                    batch.push(tagged(
                        index,
                        slot,
                        path,
                        len,
                        tag,
                        shard.block(to_usize(len)),
                    ));
                }
            }
        }
        buffer.append(batch).expect("queues");
        buffer.committed().await.expect("commits");
        let all = |buffer: &Buffer| {
            let found = [Path::Live, Path::Backfill]
                .map(|path| [1, 2].map(|tag| newest(buffer, path, tag)));
            assert!(found.iter().flatten().all(|found| found.len() == 2));
            found
        };
        let before = all(&buffer);
        drop(buffer);
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        assert_eq!((slots.index(key(1)), slots.index(key(2))), (a, b));
        assert_eq!(all(&buffer), before);
    });
}

/// `newest` reads no file: with a ring read set to fail, it gives the entry.
#[test]
fn newest_reads_no_file() {
    let (mut sim, node) = create_node(171);
    sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let pool = Rc::clone(&config.pool);
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let a = slots.index(key(1));
        let bytes = || {
            let mut block = pool.alloc(1).expect("a block");
            block[0] = 8;
            block.freeze()
        };
        buffer
            .append([tagged(1, a, Path::Live, 0, 1, bytes())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        node.fail_file(FilePath::new(RING), Operation::ReadAt);
        let found = buffer.newest(Path::Live, NonZeroU8::MIN);
        assert_eq!(found, vec![(a, kept(&bytes()))]);
    })
    .expect("the buffer ends");
}

/// After a failed sync, `newest` gives the entry that was durable before it.
#[test]
fn newest_after_a_failed_sync_gives_the_durable_entry() {
    run(173, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([tagged(1, a, Path::Live, 0, 1, shard.block(1))])
            .expect("queues");
        buffer.committed().await.expect("commits");
        shard.node().fail_file(FilePath::new(RING), Operation::Sync);
        buffer
            .append([tagged(1, a, Path::Live, 0, 1, shard.block(2))])
            .expect("queues");
        let failed = FileError::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(buffer.committed().await, Err(failed));
        let found = buffer.newest(Path::Live, NonZeroU8::MIN);
        assert_eq!(found, vec![(a, kept(&shard.block(1)))]);
    });
}

/// Before the commit with a failing sync runs, and after it failed, `newest` gives
/// the entries of the 20 indexes that were durable before it, not the newer one of
/// the same tag in the failed commit.
#[test]
fn newest_across_a_failed_sync_gives_the_durable_entries() {
    let (mut sim, node) = create_node(185);
    let found = sim.run_on(&node, |node, tasks| async move {
        let config = node_config(&node, tasks, DIR);
        let pool = Rc::clone(&config.pool);
        let block = |index: u8| pool.copy(&[index; 8]).expect("a block");
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let (batch, durable): (Vec<Entry>, Kept) = (1..=20)
            .map(|index| {
                let slot = slots.index(key(index));
                let byte = u8::try_from(index).expect("20 fits a u8");
                let entry = tagged(index, slot, Path::Live, 0, 1, block(byte));
                (entry, (slot, kept(&block(byte))))
            })
            .unzip();
        buffer.append(batch).expect("queues");
        buffer.committed().await.expect("commits");
        node.fail_file(FilePath::new(RING), Operation::Sync);
        let a = slots.index(key(1));
        buffer
            .append([tagged(1, a, Path::Live, 0, 1, block(0xff))])
            .expect("queues");
        let during = newest(&buffer, Path::Live, 1);
        let failed = buffer.committed().await.map_err(drop);
        (durable, during, failed, newest(&buffer, Path::Live, 1))
    });
    let (durable, during, failed, after) = found.expect("the run ends");
    assert_eq!((during, failed, after), (durable.clone(), Err(()), durable));
}

/// `newest` takes no block of the pool: with each block held, it gives the entry.
#[test]
fn newest_with_no_free_block_gives_the_entry() {
    run(186, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(2 * AREA, 3 * 4096), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let tag = tagged(1, a, Path::Live, 0, 1, shard.block(4000));
        buffer.append([tag]).expect("queues");
        buffer.committed().await.expect("commits");
        let expected = vec![(a, kept(&shard.block(4000)))];
        let mut held = Vec::new();
        let mut len = shard.pool.largest();
        while len > 0 {
            while let Ok(block) = shard.pool.alloc(len) {
                held.push(block);
            }
            len -= len.div_ceil(16);
        }
        let exhausted = block::Error::Exhausted {
            requested: 4000,
            available: 0,
        };
        assert_eq!(shard.pool.alloc(4000).map(drop), Err(exhausted));
        assert_eq!(newest(&buffer, Path::Live, 1), expected);
    });
}

/// Three indexes, each tagged in the first record and with data in the second:
/// `newest` gives the three entries with no read.
#[test]
fn newest_gives_the_indexes_tagged_in_one_record_with_no_read() {
    run_on_memory(180, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let (a, b, c) = (
            slots.index(key(1)),
            slots.index(key(2)),
            slots.index(key(3)),
        );
        buffer
            .append([
                tagged(1, a, Path::Live, 0, 1, shard.block(1)),
                tagged(2, b, Path::Live, 0, 1, shard.block(2)),
                tagged(3, c, Path::Live, 0, 1, shard.block(3)),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let data = |index, slot| {
            entry(
                index,
                slot,
                Path::Live,
                0,
                1,
                Some(9),
                shard.block(20).into(),
            )
        };
        buffer
            .append([data(1, a), data(2, b), data(3, c)])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let before = shard.memory().reads();
        let found = newest(&buffer, Path::Live, 1);
        assert_eq!(found.len(), 3);
        assert_eq!(shard.memory().reads() - before, 0);
    });
}

/// Odd indexes are tagged in the first record and even ones in the second:
/// `newest` gives the 16 entries with no read.
#[test]
fn newest_gives_the_indexes_tagged_in_two_records_with_no_read() {
    run_on_memory(186, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let indexes: Vec<_> = (1..=16).map(|n| (n, slots.index(key(n)))).collect();
        for odd in [true, false] {
            let batch = indexes
                .iter()
                .filter(|&&(n, _)| (n % 2 == 1) == odd)
                .map(|&(n, slot)| tagged(n, slot, Path::Live, 0, 1, shard.block(1)));
            buffer.append(batch).expect("queues");
            buffer.committed().await.expect("commits");
        }
        let before = shard.memory().reads();
        let found = newest(&buffer, Path::Live, 1);
        assert_eq!(found.len(), 16);
        assert_eq!(shard.memory().reads() - before, 0);
    });
}

/// A is tagged 1 in the first record and 2 in the second: `newest` of tag 1 gives
/// the first with no read.
#[test]
fn newest_gives_no_entry_of_another_tag() {
    run_on_memory(187, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        for tag in [1, 2] {
            buffer
                .append([tagged(1, a, Path::Live, 0, tag, shard.block(1))])
                .expect("queues");
            buffer.committed().await.expect("commits");
        }
        let before = shard.memory().reads();
        let found = newest(&buffer, Path::Live, 1);
        assert_eq!(found, vec![(a, kept(&shard.block(1)))]);
        assert_eq!(shard.memory().reads() - before, 0);
    });
}

/// A is tagged in the first record and b in the third, with data after each:
/// `newest` gives both with no read.
#[test]
fn newest_gives_the_newest_tagged_entries_past_later_records() {
    run_on_memory(184, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let (a, b) = (slots.index(key(1)), slots.index(key(2)));
        let data = |index, slot| {
            entry(
                index,
                slot,
                Path::Live,
                0,
                1,
                Some(9),
                shard.block(20).into(),
            )
        };
        for batch in [
            tagged(1, a, Path::Live, 0, 1, shard.block(1)),
            data(2, b),
            tagged(2, b, Path::Live, 1, 1, shard.block(2)),
            data(1, a),
        ] {
            buffer.append([batch]).expect("queues");
            buffer.committed().await.expect("commits");
        }
        let before = shard.memory().reads();
        let expected = vec![(a, kept(&shard.block(1))), (b, kept(&shard.block(2)))];
        assert_eq!(newest(&buffer, Path::Live, 1), expected);
        assert_eq!(shard.memory().reads() - before, 0);
    });
}

/// 64 indexes tagged in the first record, then 400 records that each hold data of
/// all 64: `newest` gives the 64 entries with no read.
#[test]
fn newest_reads_no_record_after_the_tags() {
    run_on_memory(183, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(512 * BLOCK, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let all: Vec<(u32, Slot)> = (1..=64)
            .map(|index| (index, slots.index(key(index))))
            .collect();
        buffer
            .append(all.iter().map(|&(index, slot)| {
                tagged(index, slot, Path::Live, 0, 1, shard.block(1))
            }))
            .expect("queues");
        buffer.committed().await.expect("commits");
        for record in 0..400 {
            buffer
                .append(all.iter().map(|&(index, slot)| {
                    entry(
                        index,
                        slot,
                        Path::Live,
                        record,
                        1,
                        Some(9),
                        Parts::default(),
                    )
                }))
                .expect("queues");
            buffer.committed().await.expect("commits");
        }
        let before = shard.memory().reads();
        let found = newest(&buffer, Path::Live, 1);
        assert_eq!(found.len(), 64);
        assert_eq!(shard.memory().reads() - before, 0);
    });
}

/// A table past 4 KiB, with the tagged entry last and data bytes before it.
#[test]
fn newest_reads_an_entry_after_a_table_of_two_blocks() {
    run(184, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(64 * BLOCK, 4 * 4096), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let b = slots.index(key(2));
        let mut batch: Vec<Entry> = (0..90)
            .map(|first| {
                entry(2, b, Path::Live, first, 1, Some(9), shard.block(30).into())
            })
            .collect();
        batch.push(tagged(1, a, Path::Live, 0, 1, shard.block(77)));
        batch.push(tagged(2, b, Path::Live, 90, 1, shard.block(55)));
        batch.push(entry(
            1,
            a,
            Path::Live,
            0,
            1,
            Some(9),
            shard.block(40).into(),
        ));
        buffer.append(batch).expect("queues");
        buffer.committed().await.expect("commits");
        let expected = vec![(a, kept(&shard.block(77))), (b, kept(&shard.block(55)))];
        assert_eq!(newest(&buffer, Path::Live, 1), expected);
    });
}

/// A handoff record with no holder has empty bytes: `newest` gives it.
#[test]
fn newest_gives_an_entry_with_no_bytes() {
    run(181, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        buffer
            .append([tagged(1, a, Path::Live, 0, 1, shard.block(3))])
            .expect("queues");
        buffer.committed().await.expect("commits");
        buffer
            .append([tagged(1, a, Path::Live, 0, 1, shard.block(0))])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let found = newest(&buffer, Path::Live, 1);
        assert_eq!(found, vec![(a, kept(&shard.block(0)))]);
    });
}

/// `newest` gives the entries in the order of their slots, not the order of the
/// table or of a hash map.
#[test]
fn newest_gives_the_entries_in_slot_order() {
    run(181, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let keyed: Vec<Slot> = (1..=8).map(|n| slots.index(key(n))).collect();
        let batch: Vec<Entry> = (1..=8)
            .rev()
            .zip(keyed.iter().rev())
            .map(|(n, &slot)| tagged(n, slot, Path::Live, 0, 1, shard.block(0)))
            .collect();
        buffer.append(batch).expect("queues");
        buffer.committed().await.expect("commits");
        let found = newest(&buffer, Path::Live, 1);
        let given: Vec<Slot> = found.iter().map(|&(slot, _)| slot).collect();
        assert_eq!(given, keyed);
    });
}

/// A newer tagged entry of an index moves its copy to its own record: after a trim
/// passes the record of the older entry but not the record of the newer, `newest`
/// gives the newer bytes.
#[test]
fn newest_after_a_trim_past_the_older_record_gives_the_newer_entry() {
    run(190, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let (a, b) = (slots.index(key(1)), slots.index(key(2)));
        let data = |seq| {
            let last = Some(i64::try_from(seq).expect("a small seq") + 1);
            entry(2, b, Path::Live, seq, 1, last, shard.block(1000).into())
        };
        let newer = 8;
        for seq in 0..=newer {
            let tag = match seq {
                0 => vec![tagged(1, a, Path::Live, 0, 1, shard.block(1))],
                _ if seq == newer => {
                    vec![tagged(1, a, Path::Live, 0, 1, shard.block(2))]
                }
                _ => Vec::new(),
            };
            buffer
                .append([data(seq)].into_iter().chain(tag))
                .expect("queues");
            buffer.committed().await.expect("commits");
        }
        let mut between = false;
        for seq in newer + 1..200 {
            append_within(&buffer, &shard.clock, &[data(seq)], 100)
                .await
                .expect("the ring frees room");
            buffer.committed().await.expect("commits");
            let read = buffer
                .reader()
                .read(b, Path::Live, mark(0, 0), usize::MAX)
                .await
                .expect("reads");
            let oldest = read.entries.first().expect("an entry").first;
            if (1..=newer).contains(&oldest) {
                between = true;
                let found = newest(&buffer, Path::Live, 1);
                assert_eq!(found, vec![(a, kept(&shard.block(2)))], "seq {seq}");
            }
        }
        assert!(between, "a trim lands between the two records");
    });
}

/// An appended entry that is not durable yet is absent, also while its commit
/// runs.
#[test]
fn newest_gives_no_entry_that_is_not_durable() {
    run(182, |shard| async move {
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("opens");
        let a = slots.index(key(1));
        let b = slots.index(key(2));
        buffer
            .append([
                tagged(1, a, Path::Live, 0, 1, shard.block(1)),
                tagged(2, b, Path::Live, 0, 1, shard.block(2)),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let appended = shard.clock.now();
        buffer
            .append([
                tagged(1, a, Path::Live, 0, 1, shard.block(5)),
                tagged(2, b, Path::Live, 0, 1, shard.block(6)),
            ])
            .expect("queues");
        let found = newest(&buffer, Path::Live, 1);
        assert!(
            shard.clock.now() - appended < COMMIT,
            "the commit is pending"
        );
        let first = vec![(a, kept(&shard.block(1))), (b, kept(&shard.block(2)))];
        let second = vec![(a, kept(&shard.block(5))), (b, kept(&shard.block(6)))];
        assert_eq!(found, first);
        buffer.committed().await.expect("commits");
        assert_eq!(newest(&buffer, Path::Live, 1), second);
    });
}

/// Writes an entry with no samples of index 1 at seq 0, `between` records of index
/// 2, and a second entry with no samples of index 1 at seq 0, each its own commit.
/// With `held`, the record of the second entry also holds the only entry of index 3.
/// Then 200 records of index 2 fill a ring of 32 blocks, with trims. Gives the first
/// of them that the ring still refuses after 100 commit spans, with its error.
fn refused_after_entries_with_no_samples_at_a_seq(
    between: u64,
    held: bool,
) -> Option<(u64, Rejected)> {
    let (mut sim, node) = create_node(0);
    let own = node.clone();
    let found = Arc::new(Mutex::new(None));
    let given = Arc::clone(&found);
    drop(on_node(&node, "stall", move |tasks| async move {
        let config = Config {
            layout: layout(32 * BLOCK, BODY_MAX),
            ..node_config(&own, tasks, DIR)
        };
        let pool = Rc::clone(&config.pool);
        let clock = own.clock();
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let one = slots.index(key(1));
        let two = slots.index(key(2));
        let empty = || entry(1, one, Path::Live, 0, 0, None, Parts::default());
        let data = |seq: u64| {
            let parts = Parts::from(pool.alloc(100).expect("a block").freeze());
            let last = i64::try_from(seq).expect("a small seq") + 1;
            entry(2, two, Path::Live, seq, 1, Some(last), parts)
        };
        let three = slots.index(key(3));
        let mut last = vec![empty()];
        if held {
            let parts = Parts::from(pool.alloc(100).expect("a block").freeze());
            last.push(entry(3, three, Path::Live, 0, 1, Some(1), parts));
        }
        let mut batches = vec![vec![empty()]];
        batches.extend((0..between).map(|seq| vec![data(seq)]));
        batches.push(last);
        for batch in batches {
            buffer.append(batch).expect("the ring has room");
            buffer.committed().await.expect("commits");
        }
        for (n, seq) in (between..between + 200).enumerate() {
            if let Err(error) = append_within(&buffer, &clock, &[data(seq)], 100).await
            {
                let n = u64::try_from(n).expect("a small count");
                *given.lock().expect("no panic held the lock") = Some((n, error));
                return;
            }
            buffer.committed().await.expect("commits");
        }
    }));
    sim.run_for(Span::from_nanos(60_000_000_000))
        .expect("the run goes on");
    found.lock().expect("no panic held the lock").take()
}

/// A trim lands in the records between two entries with no samples at one seq, so
/// the ring frees room for each later record. When the record of the second entry
/// holds another path, a carry first moves that path out, also when the record is
/// past the room that the trim wants.
#[test]
fn a_trim_between_entries_with_no_samples_at_a_seq_frees_room() {
    for held in [false, true] {
        for between in 0..8 {
            let refused = refused_after_entries_with_no_samples_at_a_seq(between, held);
            assert_eq!(refused, None, "with {between} records between, held {held}");
        }
    }
}

/// Writes an entry with no samples of index 1 at seq 0, sets its stored count to
/// `count`, and reopens the ring.
async fn reopen_with_count(shard: &Shard, count: u64) -> (Buffer, Slot) {
    let ring = layout(AREA, BODY_MAX);
    let mut slots = Slots::new();
    let buffer = shard.open(ring, &mut slots).await.expect("opens");
    let one = slots.index(key(1));
    buffer
        .append([entry(1, one, Path::Live, 0, 0, None, Parts::default())])
        .expect("queues");
    buffer.committed().await.expect("commits");
    let ending = buffer.ended();
    drop(buffer);
    ending.await.expect("ends");
    // The table count, then `index`, `path`, `first`, `len`, `stored_at`, and the
    // presence byte come before the count.
    let at = 4 + 16 + 1 + 8 + 4 + 8 + 1;
    shard.tamper_record(BLOCK, at, &count.to_le_bytes()).await;
    let mut slots = Slots::new();
    let buffer = shard.open(ring, &mut slots).await.expect("the count opens");
    (buffer, slots.index(key(1)))
}

/// An entry with no samples after a count of 2^63 - 1 commits a count of 2^63, and
/// the ring reopens.
#[test]
fn a_count_past_two_to_the_63_reopens() {
    run(211, |shard| async move {
        let (buffer, one) = reopen_with_count(&shard, (1 << 63) - 1).await;
        buffer
            .append([entry(1, one, Path::Live, 0, 0, None, Parts::default())])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let ending = buffer.ended();
        drop(buffer);
        ending.await.expect("ends");
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("a count the buffer wrote opens");
        let read = buffer
            .reader()
            .read(slots.index(key(1)), Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(read.next, mark(0, 1 << 63));
    });
}

/// The ring opens with the largest count, and the next entry with no samples at its
/// seq panics.
#[test]
#[should_panic(
    expected = "invariant: an entry of index 00000000-0000-0000-0000-000000000001 on \
                path Live with no samples follows u64::MAX of them at seq 0"
)]
fn an_entry_with_no_samples_after_the_largest_count_panics() {
    run(212, |shard| async move {
        let (buffer, one) = reopen_with_count(&shard, u64::MAX).await;
        buffer
            .append([entry(1, one, Path::Live, 0, 0, None, Parts::default())])
            .expect("queues");
    });
}

/// After the largest count, an entry with samples at its seq and an entry with no
/// samples at a later seq commit, and the ring reopens.
#[test]
fn after_the_largest_count_other_entries_commit_and_reopen() {
    run(213, |shard| async move {
        let (buffer, one) = reopen_with_count(&shard, u64::MAX).await;
        buffer
            .append([
                entry(1, one, Path::Live, 0, 1, Some(9), Parts::default()),
                entry(1, one, Path::Live, 1, 0, None, Parts::default()),
            ])
            .expect("queues");
        buffer.committed().await.expect("commits");
        let ending = buffer.ended();
        drop(buffer);
        ending.await.expect("ends");
        let mut slots = Slots::new();
        let buffer = shard
            .open(layout(AREA, BODY_MAX), &mut slots)
            .await
            .expect("reopens");
        let read = buffer
            .reader()
            .read(slots.index(key(1)), Path::Live, Mark::at(0), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(read.next, mark(1, 1));
    });
}
