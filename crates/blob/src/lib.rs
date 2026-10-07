//! Stores chunks by digest on the node's disk, through `env::files`, and reads them
//! back only when their bytes hash to the digest. A chunk torn by a crash reads as
//! absent.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Poll, Waker};

use block::{Block, Pool};
use env::files::{self, File, Files, Mode};
use types::digest::Digest;

/// What a [`Store`] is given at open.
#[derive(Debug)]
pub struct Config {
    /// The file seam. `os` or `sim` implements it.
    pub files: Files,
    /// The directory of the store, relative to the data directory. Its parent must
    /// be there and durable.
    pub dir: PathBuf,
    /// Blocks for the chunks a get gives and for checks of chunks on disk. Its
    /// largest block bounds a chunk.
    pub pool: Rc<Pool>,
}

/// Why a store call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A file call failed.
    Files(files::Error),
    /// The pool has no block for a read.
    Pool(block::Error),
    /// The bytes of a put do not hash to its digest.
    Mismatch {
        /// The digest the put named.
        digest: Digest,
        /// The digest of the bytes.
        found: Digest,
    },
    /// A file in the directory of the store that is not named by a digest.
    Stray {
        /// The file.
        path: PathBuf,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Files(error) => error.fmt(f),
            Self::Pool(error) => error.fmt(f),
            Self::Mismatch { digest, found } => {
                write!(f, "the bytes of the put hash to {found}, not to {digest}")
            }
            Self::Stray { path } => write!(
                f,
                "{} is in the directory of the store, but it is not named by a digest",
                path.display()
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<files::Error> for Error {
    fn from(error: files::Error) -> Self {
        Self::Files(error)
    }
}

impl From<block::Error> for Error {
    fn from(error: block::Error) -> Self {
        Self::Pool(error)
    }
}

/// What the store knows of one digest.
enum State {
    /// A file of the name was listed at open, or a dropped put left one. Its bytes
    /// may be torn or not durable.
    Listed,
    /// A put returned in this open.
    Held,
    /// A put is in flight, and these calls wait for its end.
    Writing(Vec<Waker>),
    /// The close of the file of a dropped put. A write open of the path is `Busy`
    /// until it ends, so a call of the digest drives it to its end first.
    Closing(Pin<Box<dyn Future<Output = ()>>>),
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listed => f.write_str("Listed"),
            Self::Held => f.write_str("Held"),
            Self::Writing(wakers) => f.debug_tuple("Writing").field(wakers).finish(),
            Self::Closing(_) => f.write_str("Closing(..)"),
        }
    }
}

/// The chunks of one node, by digest. One shard owns a store; its calls may overlap
/// in time.
#[derive(Debug)]
pub struct Store {
    files: Files,
    dir: PathBuf,
    pool: Rc<Pool>,
    chunks: RefCell<BTreeMap<Digest, State>>,
    corruptions: Cell<u64>,
}

impl Store {
    /// Opens the store in `config.dir`, making the directory when it is not there.
    /// It reads no chunk.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when a file call fails, and [`Error::Stray`] when a file in
    /// the directory is not named by a digest.
    pub async fn open(config: Config) -> Result<Self, Error> {
        let Config { files, dir, pool } = config;
        // An earlier open can have made the directory and stopped before this sync.
        files.create_dir(&dir).await?;
        files
            .sync_dir(dir.parent().unwrap_or(Path::new("")))
            .await?;
        let names = files.list(&dir).await?;
        let mut chunks = BTreeMap::new();
        for name in names {
            let Some(digest) = digest(&name) else {
                let path = dir.join(name);
                return Err(Error::Stray { path });
            };
            chunks.insert(digest, State::Listed);
        }
        Ok(Self {
            files,
            dir,
            pool,
            chunks: RefCell::new(chunks),
            corruptions: Cell::new(0),
        })
    }

    /// Stores `chunk` under `digest`. It returns only after the chunk is durable: a
    /// crash after the return keeps it. A put of a chunk the store holds makes no file
    /// call. A second put of one digest while the first is in flight waits for it.
    /// A put whose future is dropped before it returns stores nothing that a get gives
    /// unchecked: the next get of the digest reads and checks the file, and the next
    /// put writes it again.
    ///
    /// # Errors
    ///
    /// [`Error::Pool`] when `chunk` is longer than the largest block of the pool, and
    /// [`Error::Mismatch`] when `chunk` does not hash to `digest`; nothing is written
    /// in either case. [`Error::Files`] when a file call fails, among them `Full` when
    /// the disk has no room; the chunk then reads as absent.
    pub async fn put(&self, digest: Digest, chunk: &Block) -> Result<(), Error> {
        let largest = self.pool.largest();
        if chunk.len() > largest {
            let requested = chunk.len();
            return Err(block::Error::TooLarge { requested, largest }.into());
        }
        let found = Digest::of(chunk);
        if found != digest {
            return Err(Error::Mismatch { digest, found });
        }
        loop {
            match self.peek(digest) {
                Peek::Absent | Peek::Listed => break,
                Peek::Held => return Ok(()),
                Peek::Writing => self.wait(digest).await,
                Peek::Closing => self.settle(digest).await,
            }
        }
        let mut flight = Flight::new(self, digest);
        let written = flight.write(chunk).await;
        flight.after = written.is_ok().then_some(State::Held);
        written
    }

    /// The chunk stored under `digest`, or `None` when the store does not hold it or
    /// its bytes on disk do not hash to `digest`. In the second case the caller
    /// fetches the chunk again as for any absent one. A get during a put of the
    /// digest waits for the put.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when a file call fails, and [`Error::Pool`] when the pool has
    /// no block of the chunk's length.
    pub async fn get(&self, digest: Digest) -> Result<Option<Block>, Error> {
        loop {
            match self.peek(digest) {
                Peek::Absent => return Ok(None),
                Peek::Held | Peek::Listed => return self.read(digest).await,
                Peek::Writing => self.wait(digest).await,
                Peek::Closing => self.settle(digest).await,
            }
        }
    }

    /// The reads since the open whose bytes did not hash to their digest.
    #[cfg(test)]
    fn corruptions(&self) -> u64 {
        self.corruptions.get()
    }

    fn peek(&self, digest: Digest) -> Peek {
        Peek::of(self.chunks.borrow().get(&digest))
    }

    /// Makes `digest` absent after a read of it found no chunk, unless a put changed
    /// its state from `seen` during the read: the put's bytes are newer.
    fn forget(&self, digest: Digest, seen: Peek) {
        let mut chunks = self.chunks.borrow_mut();
        if Peek::of(chunks.get(&digest)) == seen {
            chunks.remove(&digest);
        }
    }

    /// Ends when no put of `digest` is in flight.
    async fn wait(&self, digest: Digest) {
        poll_fn(|cx| {
            let mut chunks = self.chunks.borrow_mut();
            let Some(State::Writing(wakers)) = chunks.get_mut(&digest) else {
                return Poll::Ready(());
            };
            if !wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await;
    }

    /// Reads the file of `digest` and gives its bytes when they hash to `digest`.
    /// Bytes that do not, or no file, make the digest absent. A match changes no
    /// state: a get cannot know that the bytes are durable.
    async fn read(&self, digest: Digest) -> Result<Option<Block>, Error> {
        let seen = self.peek(digest);
        let path = self.path(digest);
        let file = match self.files.open(&path, Mode::Read).await {
            Ok(file) => file,
            Err(files::Error::NotFound { .. }) => {
                self.forget(digest, seen);
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let len = usize::try_from(file.len()).unwrap_or(usize::MAX);
        let block = match self.pool.alloc(len) {
            Ok(block) => block,
            Err(error) => {
                file.close().await;
                return Err(error.into());
            }
        };
        let read = file.read_at(0, block).await;
        file.close().await;
        let block = read?.freeze();
        if Digest::of(&block) == digest {
            return Ok(Some(block));
        }
        self.corruptions
            .set(self.corruptions.get().saturating_add(1));
        self.forget(digest, seen);
        Ok(None)
    }

    /// Ends the close of the file of a dropped put of `digest`, so that the calls of
    /// the put end before the next open of the file. The digest is `Listed` after it.
    async fn settle(&self, digest: Digest) {
        Flight::new(self, digest).close().await;
    }

    fn path(&self, digest: Digest) -> PathBuf {
        self.dir.join(digest.to_string())
    }
}

/// A copy of a [`State`] without its wakers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Peek {
    Absent,
    Listed,
    Held,
    Writing,
    Closing,
}

impl Peek {
    fn of(state: Option<&State>) -> Self {
        match state {
            None => Self::Absent,
            Some(State::Listed) => Self::Listed,
            Some(State::Held) => Self::Held,
            Some(State::Writing(_)) => Self::Writing,
            Some(State::Closing(_)) => Self::Closing,
        }
    }
}

/// A put in flight. It holds [`State::Writing`] for its digest, and its drop sets the
/// next state and wakes the calls that waited. The file never drops with the flight:
/// a drop keeps its close in [`State::Closing`], so that the file's calls end before
/// the next write open of the path.
struct Flight<'a> {
    store: &'a Store,
    digest: Digest,
    file: Option<File>,
    closing: Option<Pin<Box<dyn Future<Output = ()>>>>,
    /// The state after the flight, when the file is closed. `None` is absent.
    after: Option<State>,
}

impl<'a> Flight<'a> {
    /// Takes `digest` from the state it is in, which is not `Writing`.
    fn new(store: &'a Store, digest: Digest) -> Self {
        let before = store
            .chunks
            .borrow_mut()
            .insert(digest, State::Writing(Vec::new()));
        let closing = match before {
            Some(State::Writing(_)) => {
                panic!("invariant: one put of a digest is in flight at a time")
            }
            Some(State::Closing(close)) => Some(close),
            Some(State::Listed | State::Held) | None => None,
        };
        Flight {
            store,
            digest,
            file: None,
            closing,
            after: Some(State::Listed),
        }
    }

    /// Writes `chunk` to the file of the digest, durably.
    async fn write(&mut self, chunk: &Block) -> Result<(), Error> {
        let store = self.store;
        let path = store.path(self.digest);
        let len =
            u64::try_from(chunk.len()).expect("invariant: a length fits in 64 bits");
        let mode = Mode::Create { len };
        let file = match store.files.open(&path, mode).await {
            // A file of another length at the name is not the chunk.
            Err(files::Error::Length { .. }) => {
                store.files.remove(&path).await?;
                store.files.open(&path, mode).await?
            }
            opened => opened?,
        };
        let file = self.file.insert(file);
        let written = async {
            file.write_at(0, std::slice::from_ref(chunk)).await?;
            file.sync().await
        }
        .await;
        self.close().await;
        written?;
        store.files.sync_dir(&store.dir).await?;
        Ok(())
    }

    /// Closes the file, when one is open or closing. A drop during the close keeps
    /// the close future, so the file's calls still end before the next open.
    async fn close(&mut self) {
        if let Some(file) = self.file.take() {
            self.closing = Some(Box::pin(file.close()));
        }
        if self.closing.is_none() {
            return;
        }
        poll_fn(|cx| {
            let close = self.closing.as_mut().expect("invariant: a close is set");
            close.as_mut().poll(cx)
        })
        .await;
        self.closing = None;
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let closing: Option<Pin<Box<dyn Future<Output = ()>>>> = match self.file.take()
        {
            Some(file) => Some(Box::pin(file.close())),
            None => self.closing.take(),
        };
        let after = match closing {
            Some(close) => Some(State::Closing(close)),
            None => self.after.take(),
        };
        let mut chunks = self.store.chunks.borrow_mut();
        let Some(State::Writing(wakers)) = chunks.remove(&self.digest) else {
            unreachable!("invariant: a put in flight holds the writing state");
        };
        if let Some(after) = after {
            chunks.insert(self.digest, after);
        }
        drop(chunks);
        wakers.into_iter().for_each(Waker::wake);
    }
}

/// The digest that `name` spells, when it is 64 lowercase hex digits.
fn digest(name: &Path) -> Option<Digest> {
    let name = name.to_str()?;
    if name.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (byte, pair) in bytes.iter_mut().zip(name.as_bytes().chunks(2)) {
        let pair = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    let digest = Digest(bytes);
    (digest.to_string() == name).then_some(digest)
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use env::files::Operation;
    use sim::{Crash, Sim};
    use types::time::Span;

    use super::*;

    const DIR: &str = "blob";

    fn create_node(seed: u64, disk_bytes: u64) -> (Sim, sim::node::Node) {
        let mut sim = Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config {
            disk_bytes,
            ..sim::node::Config::default()
        });
        (sim, node)
    }

    /// A node with a disk of 64 MiB.
    fn create_default_node(seed: u64) -> (Sim, sim::node::Node) {
        create_node(seed, 64 << 20)
    }

    /// A pool of `budget` bytes.
    fn create_pool(budget: usize) -> Rc<Pool> {
        let config = block::Config { budget };
        let memory = block::Heap::new(config.reservation());
        Rc::new(Pool::new(config, memory))
    }

    async fn open_with(node: &sim::node::Node, pool: Rc<Pool>) -> Result<Store, Error> {
        Store::open(Config {
            files: node.files(),
            dir: DIR.into(),
            pool,
        })
        .await
    }

    async fn open(node: &sim::node::Node) -> Result<Store, Error> {
        open_with(node, create_pool(4 << 20)).await
    }

    /// A chunk of `len` bytes of `byte`, with its digest.
    fn chunk(byte: u8, len: usize) -> (Digest, Block) {
        let mut block = create_pool(4 << 20).alloc(len).unwrap();
        block.fill(byte);
        let block = block.freeze();
        (Digest::of(&block), block)
    }

    /// The chunk of the power cut test with `index`: 3,000 bytes, so a cut can tear
    /// it.
    fn torn(index: u64) -> (Digest, Block) {
        chunk(u8::try_from(index).unwrap(), 3000)
    }

    fn path(digest: Digest) -> PathBuf {
        Path::new(DIR).join(digest.to_string())
    }

    fn too_large(requested: usize) -> block::Error {
        block::Error::TooLarge {
            requested,
            largest: 1792,
        }
    }

    fn io(path: &Path, operation: Operation) -> Error {
        Error::Files(files::Error::Io {
            path: path.to_path_buf(),
            operation,
            code: 5,
        })
    }

    /// Fails when `store` gives a chunk for `digest`.
    async fn assert_absent(store: &Store, digest: Digest) {
        assert!(store.get(digest).await.unwrap().is_none());
    }

    /// Fails when the fault on `Open` of `path` is not armed: a call took it.
    async fn assert_no_open(node: &sim::node::Node, path: &Path) {
        let error = node.files().open(path, Mode::Read).await.unwrap_err();
        assert_eq!(Error::Files(error), io(path, Operation::Open));
    }

    /// Fails when the fault on `SyncDir` of the store's directory is not armed.
    async fn assert_no_sync_dir(node: &sim::node::Node) {
        let error = node.files().sync_dir(Path::new(DIR)).await.unwrap_err();
        assert_eq!(Error::Files(error), io(Path::new(DIR), Operation::SyncDir));
    }

    /// Puts `bytes` at `offset` of the file of `digest`, durably, as a defect would.
    async fn put_bytes(
        node: &sim::node::Node,
        digest: Digest,
        offset: u64,
        bytes: &[u8],
    ) {
        let file = node.files().open(&path(digest), Mode::Write).await.unwrap();
        let mut block = create_pool(4 << 20).alloc(bytes.len()).unwrap();
        block.copy_from_slice(bytes);
        file.write_at(offset, &[block.freeze()]).await.unwrap();
        file.sync().await.unwrap();
        file.close().await;
    }

    /// Makes a file at `path` with the bytes of `block`, durably, as a defect would.
    async fn create_file(node: &sim::node::Node, path: &Path, block: &Block) {
        let files = node.files();
        let len = u64::try_from(block.len()).unwrap();
        let file = files.open(path, Mode::Create { len }).await.unwrap();
        file.write_at(0, std::slice::from_ref(block)).await.unwrap();
        file.sync().await.unwrap();
        file.close().await;
        files.sync_dir(Path::new(DIR)).await.unwrap();
    }

    /// Makes an empty file at `path`, durably, as a crash in a create leaves.
    async fn create_empty(node: &sim::node::Node, path: &Path) {
        let files = node.files();
        files.create_dir(Path::new(DIR)).await.unwrap();
        let file = files.open(path, Mode::Create { len: 0 }).await.unwrap();
        file.close().await;
        files.sync_dir(Path::new(DIR)).await.unwrap();
    }

    /// Polls `future` once.
    async fn poll_once<F: Future>(
        future: &mut std::pin::Pin<&mut F>,
    ) -> Poll<F::Output> {
        poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
    }

    /// Runs `a` and `b` together and gives both outputs.
    async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
        let (mut a, mut b) = (pin!(a), pin!(b));
        let (mut out_a, mut out_b) = (None, None);
        poll_fn(|cx| {
            if out_a.is_none()
                && let Poll::Ready(out) = a.as_mut().poll(cx)
            {
                out_a = Some(out);
            }
            if out_b.is_none()
                && let Poll::Ready(out) = b.as_mut().poll(cx)
            {
                out_b = Some(out);
            }
            if out_a.is_some() && out_b.is_some() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        (out_a.unwrap(), out_b.unwrap())
    }

    fn shard(name: &str) -> env::shards::Config {
        env::shards::Config {
            name: name.into(),
            core: None,
        }
    }

    mod put {
        use super::*;

        #[test]
        fn then_a_get_gives_the_same_bytes() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
                assert_eq!(store.corruptions(), 0);
            })
            .unwrap();
        }

        #[test]
        fn of_the_empty_chunk_round_trips() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(0, 0);
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert!(got.is_empty());
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert!(got.is_empty());
                assert_eq!(store.corruptions(), 0);
            })
            .unwrap();
        }

        #[test]
        fn of_a_held_chunk_makes_no_file_call() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                node.fail_file(&path(digest), Operation::Open);
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                store.put(digest, &block).await.unwrap();
                assert_no_open(&node, &path(digest)).await;
                assert_no_sync_dir(&node).await;
            })
            .unwrap();
        }

        #[test]
        fn of_a_chunk_of_the_largest_block_round_trips() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let pool = create_pool(2048);
                let largest = pool.largest();
                let store = open_with(&node, pool).await.unwrap();
                let (digest, block) = chunk(7, largest);
                store.put(digest, &block).await.unwrap();
                assert_eq!(*store.get(digest).await.unwrap().unwrap(), *block);
            })
            .unwrap();
        }

        #[test]
        fn of_a_chunk_longer_than_the_largest_block_writes_nothing() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open_with(&node, create_pool(2048)).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                node.fail_file(&path(digest), Operation::Open);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, Error::Pool(too_large(3000)));
                assert_absent(&store, digest).await;
                assert_no_open(&node, &path(digest)).await;
            })
            .unwrap();
        }

        #[test]
        fn over_a_file_of_another_length_removes_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                let (_, other) = chunk(7, 512);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let store = open(&node).await.unwrap();
                assert_absent(&store, digest).await;
                assert_eq!(store.corruptions(), 1);
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        // The first put drops with its write in flight. A get starts the close of
        // its file and drops while the close waits. The next put must not find the
        // file busy.
        #[test]
        fn after_a_get_dropped_in_a_settle_is_not_busy() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                {
                    let mut put = pin!(store.put(digest, &block));
                    assert_eq!(poll_once(&mut put).await, Poll::Pending);
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                    assert_eq!(poll_once(&mut put).await, Poll::Pending);
                }
                {
                    let mut get = pin!(store.get(digest));
                    assert!(poll_once(&mut get).await.is_pending());
                }
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn of_bytes_with_another_digest_writes_nothing() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (found, block) = chunk(7, 3000);
                let digest = Digest::of(b"another chunk");
                node.fail_file(&path(digest), Operation::Open);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, Error::Mismatch { digest, found });
                assert_absent(&store, digest).await;
                assert_no_open(&node, &path(digest)).await;
            })
            .unwrap();
        }

        #[test]
        fn on_a_full_disk_gives_full_and_the_chunk_reads_as_absent() {
            let (mut sim, node) = create_node(0, 64 << 10);
            let (digest, block) = chunk(7, 1 << 20);
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                let error = store.put(digest, &block).await.unwrap_err();
                let full = files::Error::Full { path: path(digest) };
                assert_eq!(error, Error::Files(full));
                assert_absent(&store, digest).await;
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                assert_absent(&store, digest).await;
            })
            .unwrap();
        }

        #[test]
        fn whose_sync_fails_gives_io_and_the_chunk_reads_as_absent() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                node.fail_file(&path(digest), Operation::Sync);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::Sync));
                node.fail_file(&path(digest), Operation::Open);
                assert_absent(&store, digest).await;
                assert_no_open(&node, &path(digest)).await;
                // The next put goes over what the failed one left.
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn whose_write_fails_gives_io() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                node.fail_file(&path(digest), Operation::WriteAt);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::WriteAt));
                assert_absent(&store, digest).await;
                store.put(digest, &block).await.unwrap();
            })
            .unwrap();
        }

        // The second put starts after the first opened its file, with a fault on
        // the next open, so it returns `Ok` only when it waited for the first.
        #[test]
        fn twice_at_once_writes_once() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut first = pin!(store.put(digest, &block));
                assert_eq!(poll_once(&mut first).await, Poll::Pending);
                node.fail_file(&path(digest), Operation::Open);
                let second = store.put(digest, &block);
                assert_eq!(join(first, second).await, (Ok(()), Ok(())));
                assert_no_open(&node, &path(digest)).await;
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn after_a_failed_put_of_the_digest_writes_itself() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                node.fail_file(&path(digest), Operation::Sync);
                let first = store.put(digest, &block);
                let second = store.put(digest, &block);
                let expected = Err(io(&path(digest), Operation::Sync));
                assert_eq!(join(first, second).await, (expected, Ok(())));
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        // The bytes are in the cache and no sync covers them, as a process crash in
        // a put leaves them. A put that trusted a read of them would return with no
        // sync, and the power cut would lose the chunk.
        #[test]
        fn of_a_listed_chunk_writes_it_again_and_makes_it_durable() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 32 << 10);
            let put = block.clone();
            sim.run_on(&node, move |node, _| async move {
                open(&node).await.unwrap();
                let files = node.files();
                let len = u64::try_from(put.len()).unwrap();
                let file = files
                    .open(&path(digest), Mode::Create { len })
                    .await
                    .unwrap();
                file.write_at(0, std::slice::from_ref(&put)).await.unwrap();
                file.close().await;
            })
            .unwrap();
            sim.crash(&node, Crash::Process);
            let put = block.clone();
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                store.put(digest, &put).await.unwrap();
                assert_eq!(store.corruptions(), 0);
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        // Each poll starts one call (the create, the write, the sync) and the sleep
        // after it ends that call, so the put drops with the call of `polls` in
        // flight. A get and a second put wait for it from their own tasks.
        #[test]
        fn dropped_in_flight_leaves_the_chunk_whole_or_absent() {
            for polls in 1..=3 {
                let (mut sim, node) = create_default_node(polls);
                sim.run_on(&node, move |node, tasks| async move {
                    let store = Rc::new(open(&node).await.unwrap());
                    let (digest, block) = chunk(7, 3000);
                    let got = Rc::new(RefCell::new(None));
                    let put_again = Rc::new(RefCell::new(None));
                    {
                        let mut put = pin!(store.put(digest, &block));
                        assert_eq!(poll_once(&mut put).await, Poll::Pending);
                        let (slot, reader) = (Rc::clone(&got), Rc::clone(&store));
                        tasks.spawn(async move {
                            *slot.borrow_mut() = Some(reader.get(digest).await);
                        });
                        let (slot, writer) = (Rc::clone(&put_again), Rc::clone(&store));
                        let again = block.clone();
                        tasks.spawn(async move {
                            *slot.borrow_mut() = Some(writer.put(digest, &again).await);
                        });
                        for _ in 1..polls {
                            node.clock().sleep(Span::from_nanos(100_000)).await;
                            assert_eq!(poll_once(&mut put).await, Poll::Pending);
                        }
                    }
                    node.clock().sleep(Span::SECOND).await;
                    let got = got.borrow_mut().take().expect("the get ended");
                    if let Some(got) = got.unwrap() {
                        assert_eq!(&got[..], &block[..], "polls {polls}");
                    }
                    let put_again = put_again.borrow_mut().take();
                    assert_eq!(put_again, Some(Ok(())), "polls {polls}");
                    let got = store.get(digest).await.unwrap().unwrap();
                    assert_eq!(&got[..], &block[..]);
                })
                .unwrap();
            }
        }
    }

    mod get {
        use super::*;

        #[test]
        fn of_an_unknown_digest_makes_no_file_call() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let digest = Digest::of(b"never put");
                node.fail_file(&path(digest), Operation::Open);
                assert_absent(&store, digest).await;
                assert_no_open(&node, &path(digest)).await;
            })
            .unwrap();
        }

        #[test]
        // The get runs in its own task, so only the store's wake can end it.
        fn during_a_put_gives_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, tasks| async move {
                let store = Rc::new(open(&node).await.unwrap());
                let (digest, block) = chunk(7, 3000);
                let mut put = pin!(store.put(digest, &block));
                assert_eq!(poll_once(&mut put).await, Poll::Pending);
                let got = Rc::new(RefCell::new(None));
                let (slot, reader) = (Rc::clone(&got), Rc::clone(&store));
                tasks.spawn(async move {
                    *slot.borrow_mut() = Some(reader.get(digest).await);
                });
                put.await.unwrap();
                // A sleep lets the get end: a spin would starve the clock.
                node.clock().sleep(Span::SECOND).await;
                let got = got.borrow_mut().take().expect("the get ended");
                assert_eq!(&got.unwrap().unwrap()[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn of_a_changed_chunk_gives_none_and_counts_one() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                put_bytes(&node, digest, 2999, &[8]).await;
                assert_absent(&store, digest).await;
                assert_eq!(store.corruptions(), 1);
                // The digest is absent now: no second read, no second count.
                node.fail_file(&path(digest), Operation::Open);
                assert_absent(&store, digest).await;
                assert_eq!(store.corruptions(), 1);
                assert_no_open(&node, &path(digest)).await;
                // A put goes over the changed bytes.
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
                assert_eq!(store.corruptions(), 1);
            })
            .unwrap();
        }

        #[test]
        fn of_an_empty_file_gives_none_and_a_put_goes_over_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_empty(&node, &path(digest)).await;
                let store = open(&node).await.unwrap();
                assert_absent(&store, digest).await;
                assert_eq!(store.corruptions(), 1);
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn of_a_removed_file_gives_none_and_does_not_count() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                node.files().remove(&path(digest)).await.unwrap();
                assert_absent(&store, digest).await;
                assert_eq!(store.corruptions(), 0);
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn whose_open_fails_gives_io_and_keeps_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                node.fail_file(&path(digest), Operation::Open);
                let error = store.get(digest).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::Open));
                node.fail_file(&path(digest), Operation::ReadAt);
                let error = store.get(digest).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::ReadAt));
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
                assert_eq!(store.corruptions(), 0);
            })
            .unwrap();
        }

        #[test]
        fn of_a_listed_file_too_large_for_the_pool_gives_pool() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                store.put(digest, &block).await.unwrap();
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, move |node, _| async move {
                let store = open_with(&node, create_pool(2048)).await.unwrap();
                let error = store.get(digest).await.unwrap_err();
                assert_eq!(error, Error::Pool(too_large(3000)));
            })
            .unwrap();
        }
    }

    mod open {
        use super::*;

        #[test]
        fn whose_list_fails_gives_io() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                node.fail_file(Path::new(DIR), Operation::List);
                let error = open(&node).await.unwrap_err();
                assert_eq!(error, io(Path::new(DIR), Operation::List));
            })
            .unwrap();
        }

        // The first open makes the directory and stops before the sync of its
        // parent. The second lists it from the cache and must sync the parent too.
        #[test]
        fn after_an_open_that_stopped_before_its_sync_keeps_a_put() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            let put = block.clone();
            sim.run_on(&node, move |node, _| async move {
                node.fail_file(Path::new(""), Operation::SyncDir);
                let error = open(&node).await.unwrap_err();
                assert_eq!(error, io(Path::new(""), Operation::SyncDir));
                let store = open(&node).await.unwrap();
                store.put(digest, &put).await.unwrap();
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn makes_the_directory_durably() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                open(&node).await.unwrap();
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, |node, _| async move {
                assert_eq!(node.files().list(Path::new(DIR)).await, Ok(vec![]));
            })
            .unwrap();
        }

        #[test]
        fn with_a_stray_file_gives_stray() {
            for name in [
                "notes",
                &"z".repeat(64),
                &chunk(7, 10).0.to_string().to_uppercase(),
            ] {
                let (mut sim, node) = create_default_node(0);
                let stray = Path::new(DIR).join(name);
                sim.run_on(&node, move |node, _| async move {
                    create_empty(&node, &stray).await;
                    let error = open(&node).await.unwrap_err();
                    assert_eq!(error, Error::Stray { path: stray });
                })
                .unwrap();
            }
        }

        #[test]
        fn after_a_restart_keeps_each_put_that_returned() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                for index in 1..=8 {
                    let (digest, block) = torn(index);
                    store.put(digest, &block).await.unwrap();
                }
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                for index in 1..=8 {
                    let (digest, block) = torn(index);
                    let got = store.get(digest).await.unwrap().unwrap();
                    assert_eq!(&got[..], &block[..], "chunk {index}");
                }
                assert_eq!(store.corruptions(), 0);
            })
            .unwrap();
        }

        /// The true time between the cuts of two seeds in a row.
        const CUT_STEP: i64 = 12_500;

        #[test]
        fn after_a_crash_in_a_put_gives_each_chunk_whole_or_absent() {
            for crash in [Crash::Power, Crash::Process] {
                let (mut torn_cuts, mut torn_chunks) = (0, 0);
                for seed in 0..64 {
                    let (mut sim, node) = create_default_node(seed);
                    let ended = Arc::new(AtomicU64::new(0));
                    let count = Arc::clone(&ended);
                    let own = node.clone();
                    let handle =
                        node.shards().start(shard("before"), move |_| async move {
                            let store = open(&own).await.unwrap();
                            for index in 1..=8 {
                                let (digest, block) = torn(index);
                                store.put(digest, &block).await.unwrap();
                                count.store(index, Ordering::Relaxed);
                            }
                            pending::<()>().await;
                        });
                    drop(handle.unwrap());
                    sim.run_for(Span::from_nanos(
                        CUT_STEP * i64::try_from(seed).unwrap(),
                    ))
                    .unwrap();
                    let ended = ended.load(Ordering::Relaxed);
                    sim.crash(&node, crash);
                    torn_cuts += u64::from(ended > 0 && ended < 8);

                    let absent = sim
                        .run_on(&node, move |node, _| async move {
                            let store = open(&node).await.unwrap();
                            let mut absent = 0;
                            for index in 1..=8 {
                                let (digest, block) = torn(index);
                                if let Some(got) = store.get(digest).await.unwrap() {
                                    assert_eq!(&got[..], &block[..], "seed {seed}");
                                } else {
                                    assert!(
                                        index > ended,
                                        "seed {seed}: chunk {index} lost"
                                    );
                                    absent += 1;
                                }
                            }
                            // A put after the cut makes each chunk whole.
                            for index in 1..=8 {
                                let (digest, block) = torn(index);
                                store.put(digest, &block).await.unwrap();
                            }
                            for index in 1..=8 {
                                let (digest, block) = torn(index);
                                let got = store.get(digest).await.unwrap().unwrap();
                                assert_eq!(&got[..], &block[..], "seed {seed}");
                            }
                            absent
                        })
                        .unwrap();
                    torn_chunks += u64::from(absent > 0);
                }
                assert!(
                    torn_cuts > 16,
                    "{crash:?}: only {torn_cuts} cuts were between puts"
                );
                assert!(
                    torn_chunks > 4,
                    "{crash:?}: only {torn_chunks} cuts left a chunk in flight absent"
                );
            }
        }
    }

    mod state {
        use super::*;

        #[test]
        fn debug_names_each_variant() {
            assert_eq!(format!("{:?}", State::Listed), "Listed");
            assert_eq!(format!("{:?}", State::Held), "Held");
            assert_eq!(format!("{:?}", State::Writing(Vec::new())), "Writing([])");
            let closing = State::Closing(Box::pin(async {}));
            assert_eq!(format!("{closing:?}"), "Closing(..)");
        }
    }

    mod error {
        use super::*;

        #[test]
        fn displays_its_cause() {
            let digest = Digest::of(b"a");
            let found = Digest::of(b"b");
            assert_eq!(
                Error::Mismatch { digest, found }.to_string(),
                format!("the bytes of the put hash to {found}, not to {digest}")
            );
            let path = PathBuf::from("blob/notes");
            assert_eq!(
                Error::Stray { path }.to_string(),
                "blob/notes is in the directory of the store, but it is not named by a \
                 digest"
            );
            let full = files::Error::Full {
                path: PathBuf::from("blob/x"),
            };
            assert_eq!(Error::from(full.clone()).to_string(), full.to_string());
            let pool = block::Error::TooLarge {
                requested: 3,
                largest: 2,
            };
            assert_eq!(Error::from(pool.clone()).to_string(), pool.to_string());
        }
    }
}
