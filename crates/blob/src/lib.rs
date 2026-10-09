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
    /// The bytes the store leaves free on the disk. A put that would leave fewer
    /// gives [`Error::Floor`].
    pub floor_bytes: u64,
}

/// Why a store call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A file call failed.
    Files(files::Error),
    /// The pool has no block of the chunk's length.
    Pool(block::Error),
    /// The bytes of a put do not hash to its digest.
    Mismatch {
        /// The digest the put named.
        digest: Digest,
        /// The digest of the bytes.
        found: Digest,
    },
    /// A put would leave fewer bytes free on the disk than [`Config::floor_bytes`].
    /// Nothing is written.
    Floor {
        /// The bytes of the chunk.
        len: usize,
        /// The bytes free on the disk.
        free_bytes: u64,
        /// The floor of the store.
        floor_bytes: u64,
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
            Self::Floor {
                len,
                free_bytes,
                floor_bytes,
            } => write!(
                f,
                "a put of {len} bytes would leave fewer than the {floor_bytes} bytes \
                 that the store keeps free: the disk has {free_bytes} bytes free"
            ),
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
    /// A file of the name was listed at open (serial 0), or a dropped put left one
    /// (its serial). Its bytes may be torn or not durable.
    Listed(u64),
    /// A put returned in this open: its serial.
    Held(u64),
    /// A put is in flight, and these calls wait for its end, by the key of each.
    Writing(BTreeMap<u64, Waker>),
    /// A call of a dropped put that must end before the next open of the path: the
    /// close of its file, or its remove of a file of another length. A dropped call
    /// still runs, so a call of the digest drives it to its end first.
    Ending(Pending),
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listed(serial) => f.debug_tuple("Listed").field(serial).finish(),
            Self::Held(serial) => f.debug_tuple("Held").field(serial).finish(),
            Self::Writing(wakers) => {
                f.debug_tuple("Writing").field(&wakers.len()).finish()
            }
            Self::Ending(_) => f.write_str("Ending(..)"),
        }
    }
}

/// A file call of a put, kept past a drop of the put.
type Pending = Pin<Box<dyn Future<Output = Result<(), files::Error>>>>;

/// The chunks of one node, by digest. One shard owns a store; its calls may overlap
/// in time.
#[derive(Debug)]
pub struct Store {
    files: Files,
    dir: PathBuf,
    pool: Rc<Pool>,
    floor_bytes: u64,
    chunks: RefCell<BTreeMap<Digest, State>>,
    corruptions: Cell<u64>,
    /// The serial of the next flight. A read that saw one flight's state must not
    /// forget a later one's, so two flights never leave equal states.
    flights: Cell<u64>,
    /// The key of the next call that waits for a put.
    waiters: Cell<u64>,
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
        let Config {
            files,
            dir,
            pool,
            floor_bytes,
        } = config;
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
            chunks.insert(digest, State::Listed(0));
        }
        Ok(Self {
            files,
            dir,
            pool,
            floor_bytes,
            chunks: RefCell::new(chunks),
            corruptions: Cell::new(0),
            flights: Cell::new(1),
            waiters: Cell::new(0),
        })
    }

    /// Stores `chunk` under `digest`. It returns only after the chunk is durable: a
    /// crash after the return keeps it. A put of a digest that a put stored since the
    /// open makes no file call. A second put of one digest while the first is in
    /// flight waits for it. A put whose future is dropped before it returns stores
    /// nothing that a get gives unchecked: the next get of the digest reads and checks
    /// the file, and the next put writes it again.
    ///
    /// # Errors
    ///
    /// [`Error::Pool`] when `chunk` is longer than the largest block of the pool, and
    /// [`Error::Mismatch`] when `chunk` does not hash to `digest`; nothing is written
    /// in either case. [`Error::Floor`] when the chunk would leave fewer bytes free
    /// than the floor, and [`Error::Files`] when a file call fails, among them `Full`
    /// when the disk has no room; the chunk then reads as absent. The floor counts
    /// the whole chunk as new room, and not the puts in flight.
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
                Peek::Absent | Peek::Listed(_) => break,
                Peek::Held(_) => return Ok(()),
                Peek::Writing => self.wait(digest).await,
                Peek::Ending => self.settle(digest).await,
            }
        }
        let mut flight = Flight::new(self, digest);
        let written = flight.write(chunk).await;
        flight.after = written.is_ok().then_some(State::Held(flight.serial));
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
                Peek::Held(_) | Peek::Listed(_) => return self.read(digest).await,
                Peek::Writing => self.wait(digest).await,
                Peek::Ending => self.settle(digest).await,
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
    fn wait(&self, digest: Digest) -> Waiter<'_> {
        let key = self.waiters.get();
        self.waiters.set(key + 1);
        Waiter {
            store: self,
            digest,
            key,
        }
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

    /// Ends the pending call (close or remove) of a dropped put of `digest`, so that
    /// it ends before the next open of the file. The digest is `Listed` after it.
    async fn settle(&self, digest: Digest) {
        let ended = Flight::new(self, digest).end().await;
        // The error of a dropped put's call is not this caller's: the next put makes
        // the call again.
        drop(ended);
    }

    fn path(&self, digest: Digest) -> PathBuf {
        self.dir.join(digest.to_string())
    }
}

/// A copy of a [`State`] without its wakers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Peek {
    Absent,
    Listed(u64),
    Held(u64),
    Writing,
    Ending,
}

impl Peek {
    fn of(state: Option<&State>) -> Self {
        match state {
            None => Self::Absent,
            Some(State::Listed(serial)) => Self::Listed(*serial),
            Some(State::Held(serial)) => Self::Held(*serial),
            Some(State::Writing(_)) => Self::Writing,
            Some(State::Ending(_)) => Self::Ending,
        }
    }
}

/// A call that waits for the put of its digest. It keeps one waker, the last it was
/// polled with, and its drop takes it back.
struct Waiter<'a> {
    store: &'a Store,
    digest: Digest,
    /// Used by no other waiter, so a drop takes no waker of another call.
    key: u64,
}

impl Future for Waiter<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
        let mut chunks = self.store.chunks.borrow_mut();
        let Some(State::Writing(wakers)) = chunks.get_mut(&self.digest) else {
            return Poll::Ready(());
        };
        let replaced = wakers.insert(self.key, cx.waker().clone());
        // The last drop of a waker can drop a task that uses the store.
        drop(chunks);
        drop(replaced);
        Poll::Pending
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let mut chunks = self.store.chunks.borrow_mut();
        let removed = match chunks.get_mut(&self.digest) {
            Some(State::Writing(wakers)) => wakers.remove(&self.key),
            _ => None,
        };
        // As in `poll`, the waker drops after the borrow.
        drop(chunks);
        drop(removed);
    }
}

/// A put in flight. It holds [`State::Writing`] for its digest, and its drop sets the
/// next state and wakes the calls that waited. No call of the flight drops with it:
/// a drop keeps its file's close, or its remove, in [`State::Ending`], so that the
/// call ends before the next open of the path.
struct Flight<'a> {
    store: &'a Store,
    digest: Digest,
    serial: u64,
    file: Option<File>,
    pending: Option<Pending>,
    /// The state after the flight, when no call is pending. `None` is absent.
    after: Option<State>,
}

impl<'a> Flight<'a> {
    /// Takes `digest` from the state it is in, which is not `Writing`.
    fn new(store: &'a Store, digest: Digest) -> Self {
        let before = store
            .chunks
            .borrow_mut()
            .insert(digest, State::Writing(BTreeMap::new()));
        let pending = match before {
            Some(State::Writing(_)) => {
                panic!("invariant: one put of a digest is in flight at a time")
            }
            Some(State::Ending(pending)) => Some(pending),
            Some(State::Listed(_) | State::Held(_)) | None => None,
        };
        let serial = store.flights.get();
        store.flights.set(serial + 1);
        Flight {
            store,
            digest,
            serial,
            file: None,
            pending,
            after: Some(State::Listed(serial)),
        }
    }

    /// Writes `chunk` to the file of the digest, durably.
    async fn write(&mut self, chunk: &Block) -> Result<(), Error> {
        let store = self.store;
        let path = store.path(self.digest);
        let len =
            u64::try_from(chunk.len()).expect("invariant: a length fits in 64 bits");
        let free_bytes = store.files.free().await?;
        if free_bytes.saturating_sub(len) < store.floor_bytes {
            return Err(Error::Floor {
                len: chunk.len(),
                free_bytes,
                floor_bytes: store.floor_bytes,
            });
        }
        let mode = Mode::Create { len };
        let opened = match store.files.open(&path, mode).await {
            // A file of another length at the name is not the chunk.
            Err(files::Error::Length { .. }) => {
                self.remove(&path).await?;
                store.files.open(&path, mode).await
            }
            opened => opened,
        };
        let file = match opened {
            // A removed file keeps its room until the directory syncs, and a remove
            // can end with no sync: a dropped put, a failed sync, or a crash.
            Err(files::Error::Full { .. }) => {
                store.files.sync_dir(&store.dir).await?;
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
        self.close().await?;
        written?;
        store.files.sync_dir(&store.dir).await?;
        Ok(())
    }

    /// Removes the file at `path`, through [`Flight::end`].
    async fn remove(&mut self, path: &Path) -> Result<(), files::Error> {
        let files = self.store.files.clone();
        let path = path.to_path_buf();
        self.pending = Some(Box::pin(async move { files.remove(&path).await }));
        self.end().await
    }

    /// Closes the file, through [`Flight::end`].
    async fn close(&mut self) -> Result<(), files::Error> {
        if let Some(file) = self.file.take() {
            self.pending = Some(Box::pin(async move {
                file.close().await;
                Ok(())
            }));
        }
        self.end().await
    }

    /// Drives the pending call to its end. A drop during it keeps the call, so it
    /// still ends before the next open of the path.
    async fn end(&mut self) -> Result<(), files::Error> {
        let result = poll_fn(|cx| {
            let pending = self.pending.as_mut().expect("invariant: a call is pending");
            pending.as_mut().poll(cx)
        })
        .await;
        self.pending = None;
        result
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let pending: Option<Pending> = match self.file.take() {
            Some(file) => Some(Box::pin(async move {
                file.close().await;
                Ok(())
            })),
            None => self.pending.take(),
        };
        let after = match pending {
            Some(pending) => Some(State::Ending(pending)),
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
        wakers.into_values().for_each(Waker::wake);
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

    /// The pool budget of the tests. A pool reserves address space of up to 96 times
    /// its budget, and the tests of one process share a limit on it.
    const BUDGET: usize = 1 << 20;

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
            floor_bytes: 0,
        })
        .await
    }

    async fn open(node: &sim::node::Node) -> Result<Store, Error> {
        open_with(node, create_pool(BUDGET)).await
    }

    /// A store that leaves `floor_bytes` free on the disk of `node`.
    async fn open_leaving(node: &sim::node::Node, floor_bytes: u64) -> Store {
        Store::open(Config {
            files: node.files(),
            dir: DIR.into(),
            pool: create_pool(BUDGET),
            floor_bytes,
        })
        .await
        .unwrap()
    }

    /// A chunk of `len` bytes of `byte`, with its digest.
    fn chunk(byte: u8, len: usize) -> (Digest, Block) {
        let mut block = create_pool(BUDGET).alloc(len).unwrap();
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
        let mut block = create_pool(BUDGET).alloc(bytes.len()).unwrap();
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
        fn that_leaves_the_floor_free_stores_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes - 3000).await;
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn that_leaves_one_byte_under_the_floor_gives_floor_and_writes_nothing() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let floor_bytes = free_bytes - 2999;
                let store = open_leaving(&node, floor_bytes).await;
                let error = store.put(digest, &block).await.unwrap_err();
                let expected = Error::Floor {
                    len: 3000,
                    free_bytes,
                    floor_bytes,
                };
                assert_eq!(error, expected);
                assert_absent(&store, digest).await;
                let left: Vec<PathBuf> = Vec::new();
                assert_eq!(node.files().list(Path::new(DIR)).await.unwrap(), left);
            })
            .unwrap();
        }

        #[test]
        fn over_a_floor_above_the_free_bytes_gives_floor() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes + 1).await;
                let error = store.put(digest, &block).await.unwrap_err();
                let expected = Error::Floor {
                    len: 3000,
                    free_bytes,
                    floor_bytes: free_bytes + 1,
                };
                assert_eq!(error, expected);
            })
            .unwrap();
        }

        #[test]
        fn whose_free_fails_gives_io_and_writes_nothing() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                node.fail_file(Path::new(""), Operation::Free);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, io(Path::new(""), Operation::Free));
                assert_absent(&store, digest).await;
                store.put(digest, &block).await.unwrap();
            })
            .unwrap();
        }

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
                let pool = create_pool(2048);
                let over = pool.largest() + 1;
                let store = open_with(&node, pool).await.unwrap();
                let (digest, block) = chunk(7, over);
                node.fail_file(&path(digest), Operation::Open);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, Error::Pool(too_large(over)));
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

        #[test]
        fn whose_open_fails_gives_io() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                node.fail_file(&path(digest), Operation::Open);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::Open));
                assert_absent(&store, digest).await;
            })
            .unwrap();
        }

        // The disk has room for the chunk once the file of another length is gone,
        // and only a sync of the directory frees that room.
        #[test]
        fn over_a_file_of_another_length_on_a_near_full_disk_stores_it() {
            let (mut sim, node) = create_node(0, 64 << 10);
            let (digest, block) = chunk(7, 30 << 10);
            sim.run_on(&node, move |node, _| async move {
                let (_, other) = chunk(7, 40 << 10);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let store = open(&node).await.unwrap();
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        // The sync after the remove fails once. The put after it must store the
        // chunk: the disk has room once the directory syncs.
        #[test]
        fn after_a_failed_sync_on_a_near_full_disk_stores_it() {
            let (mut sim, node) = create_node(0, 64 << 10);
            let (digest, block) = chunk(7, 30 << 10);
            sim.run_on(&node, move |node, _| async move {
                let (_, other) = chunk(7, 40 << 10);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let store = open(&node).await.unwrap();
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, io(Path::new(DIR), Operation::SyncDir));
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        // The first put drops with its remove in flight. The next put must store
        // the chunk on a nearly full disk, although no sync followed the remove.
        #[test]
        fn after_a_put_dropped_in_its_remove_on_a_near_full_disk_stores_it() {
            for seed in 0..64 {
                let (mut sim, node) = create_node(seed, 64 << 10);
                let (digest, block) = chunk(7, 30 << 10);
                sim.run_on(&node, move |node, _| async move {
                    let (_, other) = chunk(7, 40 << 10);
                    node.files().create_dir(Path::new(DIR)).await.unwrap();
                    create_file(&node, &path(digest), &other).await;
                    let store = open(&node).await.unwrap();
                    {
                        let mut first = pin!(store.put(digest, &block));
                        assert_eq!(poll_once(&mut first).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                        assert_eq!(poll_once(&mut first).await, Poll::Pending);
                    }
                    assert_eq!(store.put(digest, &block).await, Ok(()), "seed {seed}");
                })
                .unwrap();
            }
        }

        // A process crash after the remove and before any sync. The remove is made
        // by hand, as a put leaves it. The next open and put must store the chunk.
        #[test]
        fn after_a_process_crash_after_the_remove_stores_it() {
            let (mut sim, node) = create_node(0, 64 << 10);
            let (digest, block) = chunk(7, 30 << 10);
            sim.run_on(&node, move |node, _| async move {
                let (_, other) = chunk(7, 40 << 10);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                node.files().remove(&path(digest)).await.unwrap();
            })
            .unwrap();
            sim.crash(&node, Crash::Process);
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn over_a_file_of_another_length_on_a_full_disk_gives_full() {
            let (mut sim, node) = create_node(0, 64 << 10);
            let (digest, block) = chunk(7, 128 << 10);
            sim.run_on(&node, move |node, _| async move {
                let (_, other) = chunk(7, 512);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let store = open(&node).await.unwrap();
                let error = store.put(digest, &block).await.unwrap_err();
                let full = files::Error::Full { path: path(digest) };
                assert_eq!(error, Error::Files(full));
                assert_absent(&store, digest).await;
            })
            .unwrap();
        }

        #[test]
        fn over_a_file_of_another_length_whose_remove_fails_gives_io() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                let (_, other) = chunk(7, 512);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let store = open(&node).await.unwrap();
                node.fail_file(&path(digest), Operation::Remove);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::Remove));
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

        // The first put drops with its remove of a file of another length in
        // flight. The remove still ends, and must not unlink the next put's file.
        #[test]
        fn after_a_put_dropped_in_its_remove_keeps_the_chunk() {
            for seed in 0..64 {
                let (mut sim, node) = create_default_node(seed);
                let (digest, block) = chunk(7, 3000);
                let put = block.clone();
                sim.run_on(&node, move |node, _| async move {
                    let (_, other) = chunk(7, 512);
                    node.files().create_dir(Path::new(DIR)).await.unwrap();
                    create_file(&node, &path(digest), &other).await;
                    let store = open(&node).await.unwrap();
                    {
                        let mut first = pin!(store.put(digest, &put));
                        assert_eq!(poll_once(&mut first).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                        assert_eq!(poll_once(&mut first).await, Poll::Pending);
                    }
                    store.put(digest, &put).await.unwrap();
                    node.clock().sleep(Span::SECOND).await;
                })
                .unwrap();
                sim.crash(&node, Crash::Power);
                sim.run_on(&node, move |node, _| async move {
                    let store = open(&node).await.unwrap();
                    let got = store.get(digest).await.unwrap();
                    assert_eq!(
                        node.files().list(Path::new(DIR)).await.unwrap(),
                        vec![PathBuf::from(digest.to_string())],
                        "seed {seed}"
                    );
                    assert_eq!(&got.unwrap()[..], &block[..], "seed {seed}");
                })
                .unwrap();
            }
        }

        // Known bug, https://github.com/synnaxlabs/foundation/issues/1524: a store
        // dropped with a dropped put's remove in flight loses the remove, and it
        // unlinks the file of the next store's put after that put returned. The fix
        // is in `env`: a write open waits for the calls in flight on the path.
        #[test]
        fn after_a_store_dropped_with_a_remove_in_flight_loses_the_chunk() {
            let (mut sim, node) = create_default_node(22279);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                let (_, other) = chunk(7, 512);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let store = open(&node).await.unwrap();
                {
                    let mut first = pin!(store.put(digest, &block));
                    for _ in 0..2 {
                        assert_eq!(poll_once(&mut first).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                    assert_eq!(poll_once(&mut first).await, Poll::Pending);
                }
                drop(store);
                let store = open(&node).await.unwrap();
                store.put(digest, &block).await.unwrap();
                node.clock().sleep(Span::SECOND).await;
                let left: Vec<PathBuf> = Vec::new();
                assert_eq!(node.files().list(Path::new(DIR)).await.unwrap(), left);
            })
            .unwrap();
        }

        // Known bug, https://github.com/synnaxlabs/foundation/issues/1524: a store
        // dropped with a dropped put's write in flight loses the close of its file,
        // and the next store's put of the digest finds the file busy.
        #[test]
        fn after_a_store_dropped_with_a_write_in_flight_is_busy() {
            let (mut sim, node) = create_default_node(206);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                let store = open(&node).await.unwrap();
                {
                    let mut first = pin!(store.put(digest, &block));
                    for _ in 0..2 {
                        assert_eq!(poll_once(&mut first).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                    assert_eq!(poll_once(&mut first).await, Poll::Pending);
                }
                drop(store);
                let store = open(&node).await.unwrap();
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(
                    error,
                    Error::Files(files::Error::Busy { path: path(digest) })
                );
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
            let (digest, block) = chunk(7, 128 << 10);
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

        // A failed sync of the directory leaves the digest not held, so each put
        // after it writes and syncs again.
        #[test]
        fn whose_directory_sync_fails_gives_io_and_the_next_put_writes_again() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let expected = io(Path::new(DIR), Operation::SyncDir);
                for _ in 0..2 {
                    node.fail_file(Path::new(DIR), Operation::SyncDir);
                    assert_eq!(store.put(digest, &block).await.unwrap_err(), expected);
                }
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
                node.clock().sleep(Span::from_nanos(100_000)).await;
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

        // Two puts of different digests at once on a nearly full disk: a over a file
        // of another length, b beside it. In seed 13, a's remove ends between b's
        // sync and b's second create, so the removed file still holds its room and
        // b gives `Full`, as the disk has no room at that instant. a's final sync
        // frees the room, and a put of b again stores it.
        #[test]
        fn twice_at_once_of_two_digests_on_a_near_full_disk_gives_full_once() {
            let (mut sim, node) = create_node(13, 64 << 10);
            sim.run_on(&node, |node, _| async move {
                let (a, block_a) = chunk(7, 20 << 10);
                let (b, block_b) = chunk(8, 24 << 10);
                let (_, other) = chunk(7, 40 << 10);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(a), &other).await;
                let store = open(&node).await.unwrap();
                let full = Error::Files(files::Error::Full { path: path(b) });
                let results =
                    join(store.put(a, &block_a), store.put(b, &block_b)).await;
                assert_eq!(results, (Ok(()), Err(full)));
                assert_absent(&store, b).await;
                store.put(b, &block_b).await.unwrap();
                let got = store.get(b).await.unwrap().unwrap();
                assert_eq!(&got[..], &block_b[..]);
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

        // Two gets read the changed bytes. The first forgets the digest, a put
        // stores the chunk again, and then the second ends. Its stale result must
        // not forget the new chunk.
        #[test]
        fn of_a_changed_chunk_that_ends_after_a_new_put_keeps_the_put() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                put_bytes(&node, digest, 2999, &[8]).await;
                let mut stale = pin!(store.get(digest));
                // Open, then start the read of the changed bytes.
                for _ in 0..2 {
                    assert!(poll_once(&mut stale).await.is_pending());
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                }
                assert_absent(&store, digest).await;
                assert_eq!(store.corruptions(), 1);
                store.put(digest, &block).await.unwrap();
                assert!(stale.await.unwrap().is_none());
                assert_eq!(store.corruptions(), 2);
                node.fail_file(&path(digest), Operation::WriteAt);
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        // A stale get of torn listed bytes ends after a dropped put and a settle
        // left the digest listed again. It must not forget the whole chunk.
        #[test]
        fn of_a_torn_listed_chunk_that_ends_after_a_dropped_put_keeps_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                let (_, other) = chunk(8, 3000);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let store = open(&node).await.unwrap();
                let mut stale = pin!(store.get(digest));
                for _ in 0..2 {
                    assert!(poll_once(&mut stale).await.is_pending());
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                }
                assert_absent(&store, digest).await;
                {
                    let mut put = pin!(store.put(digest, &block));
                    for _ in 0..4 {
                        assert_eq!(poll_once(&mut put).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                }
                let mut next = pin!(store.get(digest));
                for _ in 0..2 {
                    assert!(poll_once(&mut next).await.is_pending());
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                }
                assert!(stale.await.unwrap().is_none());
                assert_eq!(&next.await.unwrap().unwrap()[..], &block[..]);
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        // A stale get of torn listed bytes ends after the first flight, a put dropped
        // after its close, left the digest listed again. The flight's serial is not
        // the serial of the open, so the get must not forget the whole chunk.
        #[test]
        fn of_a_torn_listed_chunk_that_ends_after_the_first_flight_keeps_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                let (_, other) = chunk(8, 3000);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let store = open(&node).await.unwrap();
                let mut stale = pin!(store.get(digest));
                for _ in 0..2 {
                    assert!(poll_once(&mut stale).await.is_pending());
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                }
                {
                    let mut put = pin!(store.put(digest, &block));
                    for _ in 0..5 {
                        assert_eq!(poll_once(&mut put).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                }
                assert!(stale.await.unwrap().is_none());
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
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

    mod debug {
        use super::*;

        // A put is in its write with a get waiting on it, and a second digest's put
        // was dropped in its write. The output names each state with no pointer.
        #[test]
        fn names_each_state_and_prints_no_pointer() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (listed, listed_block) = chunk(6, 100);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(listed), &listed_block).await;
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let (held, held_block) = chunk(8, 100);
                store.put(held, &held_block).await.unwrap();
                let (dropped, dropped_block) = chunk(9, 3000);
                {
                    let mut put = pin!(store.put(dropped, &dropped_block));
                    for _ in 0..2 {
                        assert_eq!(poll_once(&mut put).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                    assert_eq!(poll_once(&mut put).await, Poll::Pending);
                }
                let mut put = pin!(store.put(digest, &block));
                assert_eq!(poll_once(&mut put).await, Poll::Pending);
                let mut get = pin!(store.get(digest));
                assert!(poll_once(&mut get).await.is_pending());
                let text = format!("{store:?}");
                assert!(text.contains("Listed(0)"), "{text}");
                assert!(text.contains("Held(1)"), "{text}");
                assert!(text.contains("Writing(1)"), "{text}");
                assert!(text.contains("Ending(..)"), "{text}");
                assert!(!text.contains("0x"), "{text}");
            })
            .unwrap();
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
            let floor = Error::Floor {
                len: 3000,
                free_bytes: 4096,
                floor_bytes: 2048,
            };
            assert_eq!(
                floor.to_string(),
                "a put of 3000 bytes would leave fewer than the 2048 bytes that the \
                 store keeps free: the disk has 4096 bytes free"
            );
        }
    }

    mod dropped {
        use super::*;

        /// A waker that counts its wakes.
        #[derive(Default)]
        struct Count(AtomicU64);

        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        /// Polls `future` once with a waker of `count`.
        fn poll_with<F: Future>(
            future: Pin<&mut F>,
            count: &Arc<Count>,
        ) -> Poll<F::Output> {
            let waker = Waker::from(Arc::clone(count));
            future.poll(&mut std::task::Context::from_waker(&waker))
        }

        fn wakes(count: &Count) -> u64 {
            count.0.load(Ordering::Relaxed)
        }

        // A waker that the store keeps holds a count of its `Arc`.
        #[test]
        fn waiters_leave_no_waker() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut put = pin!(store.put(digest, &block));
                assert_eq!(poll_once(&mut put).await, Poll::Pending);
                let count = Arc::new(Count::default());
                for _ in 0..1000 {
                    let get = pin!(store.get(digest));
                    assert!(poll_with(get, &count).is_pending());
                }
                assert_eq!(Arc::strong_count(&count), 1);
            })
            .unwrap();
        }

        #[test]
        fn a_waiter_polled_with_new_wakers_keeps_the_last_one() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut put = pin!(store.put(digest, &block));
                assert_eq!(poll_once(&mut put).await, Poll::Pending);
                let mut get = pin!(store.get(digest));
                let counts: Vec<_> =
                    (0..1000).map(|_| Arc::new(Count::default())).collect();
                for count in &counts {
                    assert!(poll_with(get.as_mut(), count).is_pending());
                }
                let held: Vec<usize> = counts.iter().map(Arc::strong_count).collect();
                let mut expected = vec![1; 1000];
                expected[999] = 2;
                assert_eq!(held, expected);
                put.await.unwrap();
                let woken: Vec<u64> = counts.iter().map(|count| wakes(count)).collect();
                let mut expected = vec![0; 1000];
                expected[999] = 1;
                assert_eq!(woken, expected);
                assert_eq!(&get.await.unwrap().unwrap()[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn a_dropped_waiter_keeps_the_wakers_of_others() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut put = pin!(store.put(digest, &block));
                assert_eq!(poll_once(&mut put).await, Poll::Pending);
                let kept = Arc::new(Count::default());
                let mut get = pin!(store.get(digest));
                assert!(poll_with(get.as_mut(), &kept).is_pending());
                {
                    let dropped = pin!(store.get(digest));
                    let count = Arc::new(Count::default());
                    assert!(poll_with(dropped, &count).is_pending());
                }
                assert_eq!(Arc::strong_count(&kept), 2);
                put.await.unwrap();
                assert_eq!(wakes(&kept), 1);
                assert_eq!(&get.await.unwrap().unwrap()[..], &block[..]);
            })
            .unwrap();
        }

        // The first get keeps its key after its put drops. A key used again by a
        // waiter of the next put would let the first get's drop take its waker.
        #[test]
        fn a_waiter_of_a_dropped_put_keeps_the_wakers_of_the_next_put() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut first = Box::pin(store.get(digest));
                {
                    let mut put = pin!(store.put(digest, &block));
                    assert_eq!(poll_once(&mut put).await, Poll::Pending);
                    let count = Arc::new(Count::default());
                    assert!(poll_with(first.as_mut(), &count).is_pending());
                }
                let mut put = pin!(store.put(digest, &block));
                assert_eq!(poll_once(&mut put).await, Poll::Pending);
                let kept = Arc::new(Count::default());
                let mut get = pin!(store.get(digest));
                assert!(poll_with(get.as_mut(), &kept).is_pending());
                drop(first);
                assert_eq!(Arc::strong_count(&kept), 2);
                put.await.unwrap();
                assert_eq!(wakes(&kept), 1);
                assert_eq!(&get.await.unwrap().unwrap()[..], &block[..]);
            })
            .unwrap();
        }
    }
}
