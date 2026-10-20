//! Stores chunks by digest on the node's disk, through `env::files`, and reads them
//! back only when their bytes hash to the digest. A chunk torn by a crash reads as
//! absent. [`peer`] serves the store to a peer, and gets and puts chunks of a peer,
//! over blob streams.

mod line;
pub mod peer;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Poll, Waker};

use block::{Block, Pool};
use env::files::{self, File, Files, Mode};
use types::digest::{Digest, Hasher};

use line::{Line, Slot};

/// The puts of [`peer::serve`] that one store holds a part file for at one time. A
/// head over it waits, with no block and no file, until one ends. Heads go in the
/// order they come.
const PUTS_MAX: usize = 8;

/// What a [`Store`] is given at open.
#[derive(Debug)]
pub struct Config {
    /// The file seam. `os` or `sim` implements it.
    pub files: Files,
    /// The directory of the store, relative to the data directory. Its parent must
    /// be there and durable.
    pub dir: PathBuf,
    /// Blocks for the chunks a get gives, for checks of chunks on disk, for the reply
    /// heads of [`peer::serve`], and for the headers, the get messages, the chunks,
    /// and the put heads of [`peer::get`], [`peer::put`], and [`peer::Push`].
    /// Its largest block bounds a chunk. A get that `serve` answers holds its chunk
    /// and its reply head at once, and a put holds its chunk and its put head at
    /// once. A pool that cannot hold both gives that get or put
    /// [`Error::Pool`], as a pool under load does, and the stream ends with code 0.
    pub pool: Rc<Pool>,
    /// The bytes the store leaves free on the disk. A put that would leave fewer gives
    /// [`Error::Floor`], unless a file of the chunk's length is at its name.
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
    /// A put would leave less free on the disk than [`Config::floor_bytes`], and no
    /// file of the chunk's length is at its name. Nothing is written.
    Floor {
        /// The bytes of the chunk.
        len: usize,
        /// The bytes free on the disk.
        free_bytes: u64,
        /// The floor of the store.
        floor_bytes: u64,
    },
    /// A file in the directory of the store that is not named by a digest or by the
    /// part of a digest.
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
                "a put of {len} bytes would leave less than the floor of {floor_bytes} \
                 bytes free on a disk with {free_bytes} free"
            ),
            Self::Stray { path } => write!(
                f,
                "{} is in the directory of the store, but it is not named by a digest \
                 or by the part of a digest",
                path.display()
            ),
        }
    }
}

impl std::error::Error for Error {}

/// What a [`Store`] counted after its open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    /// The times a get made a digest absent because the bytes of its file did not hash
    /// to it. A file that a crash tore adds one, and so does one that a put of an
    /// earlier open left before its end. A file that a put of this open left after
    /// its first call that can make or change the file adds none, whatever it held.
    pub corruptions: u64,
    /// The gets and puts that gave [`Error::Files`]. A full disk
    /// (`files::Error::Full`) is not one.
    pub failures: u64,
}

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
    /// after its first call that can make or change the file (its serial). Its bytes
    /// may be torn or not durable.
    Listed(u64),
    /// A put returned in this open: its serial.
    Held(u64),
    /// A put is in flight, and these calls wait for its end, by the key of each.
    Writing(BTreeMap<u64, Waker>),
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listed(serial) => f.debug_tuple("Listed").field(serial).finish(),
            Self::Held(serial) => f.debug_tuple("Held").field(serial).finish(),
            Self::Writing(wakers) => {
                f.debug_tuple("Writing").field(&wakers.len()).finish()
            }
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
    floor_bytes: u64,
    chunks: RefCell<BTreeMap<Digest, State>>,
    corruptions: Cell<u64>,
    failures: Cell<u64>,
    /// The serial of the next flight. A read that saw one flight's state must not
    /// forget a later one's, so two flights that reach their create never leave equal
    /// `Listed` or `Held` states. A flight dropped before its create leaves the state
    /// it took: it did not change the file. It starts at 1: serial 0 is the open's.
    flights: Cell<u64>,
    /// The key of the next call that waits for a put.
    waiters: Cell<u64>,
    /// The receipts that are open, at most [`PUTS_MAX`].
    receipts: Line,
    /// The paths of the parts that drops left, for the next floor check to remove.
    dropped: RefCell<VecDeque<PathBuf>>,
    /// The parts that drops and errors left that can be the only copy of their chunk,
    /// by digest and path, for the next floor check or put of the digest to place.
    kept: RefCell<BTreeSet<(Digest, PathBuf)>>,
    /// The removes of parts that drops left that ended Ok.
    removed: Cell<u64>,
    /// The value of `removed` at the start of the last directory sync that ended Ok.
    /// The room of a part counts as free once this holds its remove.
    synced: Cell<u64>,
    /// The calls of `clean` that run, at most one.
    cleans: Line,
}

impl Store {
    /// Opens the store in `config.dir`, making the directory when it is not there.
    /// It reads no chunk. A part that a put of [`peer::serve`] left becomes the file
    /// of its digest when the digest has none, and is removed when it has one.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when a file call fails, and [`Error::Stray`] when a file in
    /// the directory is not named by a digest or by a part.
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
        let mut parts = Vec::new();
        for name in names {
            match named(&name) {
                Some((digest, false)) => {
                    chunks.insert(digest, State::Listed(0));
                }
                Some((digest, true)) => parts.push((digest, dir.join(name))),
                None => {
                    let path = dir.join(name);
                    return Err(Error::Stray { path });
                }
            }
        }
        // A crash can undo a remove or a rename here, so the next open does it again.
        for (digest, path) in parts {
            if chunks.contains_key(&digest) {
                files.remove(&path).await?;
                continue;
            }
            // The part may be torn, so it is listed as a file of an earlier open.
            let mut part = files.open(&path, Mode::Write).await?;
            let renamed = part.rename(&dir.join(digest.to_string())).await;
            part.close().await;
            renamed?;
            chunks.insert(digest, State::Listed(0));
        }
        Ok(Self {
            files,
            dir,
            pool,
            floor_bytes,
            chunks: RefCell::new(chunks),
            corruptions: Cell::new(0),
            failures: Cell::new(0),
            flights: Cell::new(1),
            waiters: Cell::new(0),
            receipts: Line::new(PUTS_MAX),
            dropped: RefCell::default(),
            kept: RefCell::default(),
            removed: Cell::new(0),
            synced: Cell::new(0),
            cleans: Line::new(1),
        })
    }

    /// Stores `chunk` under `digest`. It returns only after the chunk is durable: a
    /// crash after the return keeps it. A put of a digest that a put stored since the
    /// open makes no file call. A second put of one digest while the first is in
    /// flight waits for it. A put whose future is dropped before it returns stores
    /// nothing that a get gives unchecked: a get of the digest gives only bytes that it
    /// read and checked, and the next put writes it again.
    ///
    /// # Errors
    ///
    /// [`Error::Pool`] when `chunk` is longer than the largest block of the pool, and
    /// [`Error::Mismatch`] when `chunk` does not hash to `digest`; nothing is written
    /// in either case. [`Error::Floor`] when the chunk would leave fewer bytes free
    /// than the floor, and [`Error::Files`] when a file call fails, among them `Full`
    /// when the disk has no room. After an error, a get of the digest gives only bytes
    /// that it reads and checks, as after a drop of the put. A put over a file of the
    /// chunk's length takes no new room, so the floor does not refuse it. Each other
    /// put counts the whole chunk as new room, so a disk that is under the floor takes
    /// no new file, also not the empty chunk. The floor does not count the puts in
    /// flight. Its floor check first removes the parts that dropped puts of
    /// [`peer::serve`] left, so their room counts, and makes each part that can be the
    /// only copy of its chunk the file of its digest. A put of a digest with such a
    /// part renames that part in place of a write, with no floor check, as the part has
    /// its room. When that fails, the put gives its error, and a later put of the
    /// digest tries again to store the chunk.
    pub async fn put(&self, digest: Digest, chunk: &Block) -> Result<(), Error> {
        let put = async {
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
                }
            }
            Flight::new(self, digest).put(chunk).await
        };
        self.counted(put.await)
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
        let got = async {
            loop {
                match self.peek(digest) {
                    Peek::Absent => return Ok(None),
                    Peek::Held(_) | Peek::Listed(_) => return self.read(digest).await,
                    Peek::Writing => self.wait(digest).await,
                }
            }
        };
        self.counted(got.await)
    }

    /// Starts a put of a chunk of `len` bytes under `digest` whose bytes come in
    /// parts. It waits while [`PUTS_MAX`] receipts are open, and then makes a part
    /// file of its own, once the floor leaves room for it. No call waits on the
    /// receipt, and its digest keeps its state until its end.
    ///
    /// # Errors
    ///
    /// [`Error::Floor`] when the part would leave fewer bytes free than the floor and
    /// no file of the chunk's length is at the name of `digest`, and [`Error::Files`]
    /// when a file call fails.
    async fn receive(&self, digest: Digest, len: usize) -> Result<Receipt<'_>, Error> {
        let received = async {
            let slot = self.receipts.take().await;
            // A write open of the digest's name, outside its flight, would be busy
            // while a put or another receipt of the digest holds the file.
            self.room(len, &self.path(digest), Mode::Read).await?;
            let bytes =
                u64::try_from(len).expect("invariant: a length fits in 64 bits");
            // Before the create, as a dropped create can still make the file.
            let name = Name {
                store: self,
                path: self.dir.join(format!("{digest}.{}", slot.key())),
                fate: Fate::Remove,
            };
            let file = (self.files)
                .open(&name.path, Mode::Create { len: bytes })
                .await?;
            Ok(Receipt {
                store: self,
                digest,
                hasher: Hasher::default(),
                part: Part { file, name },
                at: 0,
                _slot: slot,
            })
        };
        self.counted(received.await)
    }

    /// Places each kept part: it removes the part when its digest is `Held`, skips it
    /// while a put of the digest is in flight, and else makes it the durable file of
    /// the digest, also over a listed file, as the part is synced and checked. Then it
    /// removes each part that a drop left, and syncs the directory when such a remove
    /// ended Ok after the start of the last sync that ended Ok, so a room check after
    /// it counts the room of each removed part. A failed call counts in
    /// [`Status::failures`]: the next call places and syncs again, and the next open
    /// takes a part that a failed remove left. One call runs at a time: a call during
    /// another waits for it, and then finds its work done.
    async fn clean(&self) {
        let _turn = self.cleans.take().await;
        // Each part once, so a place that fails waits for the next call.
        let mut after = None;
        loop {
            let next = {
                let kept = self.kept.borrow();
                let start = after.as_ref().map_or(Bound::Unbounded, Bound::Excluded);
                let next = kept.range((start, Bound::Unbounded)).next();
                next.cloned()
            };
            let Some((digest, path)) = next else {
                break;
            };
            after = Some((digest, path.clone()));
            match self.peek(digest) {
                Peek::Held(_) => {
                    let entry = (digest, path);
                    self.kept.borrow_mut().remove(&entry);
                    self.dropped.borrow_mut().push_back(entry.1);
                }
                Peek::Writing => {}
                Peek::Absent | Peek::Listed(_) => {
                    let flight = Flight::new(self, digest);
                    let placed = match self.open_kept(digest, path).await {
                        Ok(Some(part)) => flight.place(part).await,
                        opened => opened.map(drop),
                    };
                    if let Err(error) = placed {
                        self.count(&error);
                    }
                }
            }
        }
        // A path leaves the list only after its remove ends, so a drop of this call
        // leaves it for the next call, whose remove waits for this one.
        loop {
            let Some(path) = self.dropped.borrow().front().cloned() else {
                break;
            };
            let removed = self.files.remove(&path).await;
            self.dropped.borrow_mut().pop_front();
            match removed {
                Ok(()) => self.removed.set(self.removed.get() + 1),
                Err(error) => self.count(&Error::Files(error)),
            }
        }
        let removed = self.removed.get();
        if removed > self.synced.get() {
            match self.files.sync_dir(&self.dir).await {
                Ok(()) => self.synced.set(removed),
                Err(error) => self.count(&Error::Files(error)),
            }
        }
    }

    /// Gives `Ok` when `len` bytes of new room leave the floor free, or when a file of
    /// `len` bytes is at `path`, which it opens with `mode`. It first removes the
    /// parts that drops left, so their room counts, also when another check removes
    /// them.
    ///
    /// # Errors
    ///
    /// [`Error::Floor`] when neither holds, and [`Error::Files`] when a file call
    /// fails.
    async fn room(&self, len: usize, path: &Path, mode: Mode) -> Result<(), Error> {
        let bytes = u64::try_from(len).expect("invariant: a length fits in 64 bits");
        self.clean().await;
        let free_bytes = self.files.free().await?;
        if free_bytes.saturating_sub(bytes) < self.floor_bytes
            && self.len(path, mode).await? != Some(bytes)
        {
            return Err(Error::Floor {
                len,
                free_bytes,
                floor_bytes: self.floor_bytes,
            });
        }
        Ok(())
    }

    /// The length of the file at `path`, opened with `mode`, or `None` when no file is
    /// there.
    async fn len(&self, path: &Path, mode: Mode) -> Result<Option<u64>, Error> {
        match self.files.open(path, mode).await {
            Ok(file) => {
                let len = file.len();
                file.close().await;
                Ok(Some(len))
            }
            Err(files::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Makes `part`, whose bytes hash to `digest`, the durable file of `digest`. A
    /// digest that a put stored since the open, when this end comes, keeps its file,
    /// and the part goes.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when a file call fails. The part is then removed, unless it can
    /// be the only copy of the chunk.
    async fn place(&self, digest: Digest, part: Part<'_>) -> Result<(), Error> {
        // A remove of a listed file before this sync would leave a part that a crash
        // can tear as the only copy of the chunk.
        if let Err(error) = part.file.sync().await {
            self.remove_part(part).await;
            return Err(error.into());
        }
        loop {
            match self.peek(digest) {
                Peek::Absent | Peek::Listed(_) => break,
                Peek::Held(_) => {
                    self.remove_part(part).await;
                    return Ok(());
                }
                Peek::Writing => self.wait(digest).await,
            }
        }
        Flight::new(self, digest).place(part).await
    }

    /// Removes `part`, of a put that stores no chunk. A remove that fails counts in
    /// [`Status::failures`], and the next open takes the part.
    async fn remove_part(&self, part: Part<'_>) {
        let Part { file, mut name } = part;
        // By path, as a failed sync poisons the handle.
        file.close().await;
        let removed = self.files.remove(&name.path).await;
        name.fate = Fate::Leave;
        if let Err(error) = removed {
            self.count(&Error::Files(error));
        }
    }

    /// What this store counted after its open.
    #[must_use]
    pub fn status(&self) -> Status {
        Status {
            corruptions: self.corruptions.get(),
            failures: self.failures.get(),
        }
    }

    /// Gives `result`, the result of a get or a put, and counts it when it is a
    /// failure.
    fn counted<T>(&self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = &result {
            self.count(error);
        }
        result
    }

    /// Counts `error` in [`Status::failures`] when it is a failure.
    fn count(&self, error: &Error) {
        if let Error::Files(error) = error
            && !matches!(error, files::Error::Full { .. })
        {
            self.failures.set(self.failures.get().saturating_add(1));
        }
    }

    /// Takes the kept part of `digest` at `path` out of [`Store::kept`] and opens it
    /// for write. Gives `None` when a dropped rename made it the file of its digest.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when the open fails. The part is then kept again.
    async fn open_kept(
        &self,
        digest: Digest,
        path: PathBuf,
    ) -> Result<Option<Part<'_>>, Error> {
        let entry = (digest, path);
        self.kept.borrow_mut().remove(&entry);
        let mut name = Name {
            store: self,
            path: entry.1,
            fate: Fate::Place(digest),
        };
        match self.files.open(&name.path, Mode::Write).await {
            Ok(file) => Ok(Some(Part { file, name })),
            Err(files::Error::NotFound { .. }) => {
                name.fate = Fate::Leave;
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn peek(&self, digest: Digest) -> Peek {
        Peek::of(self.chunks.borrow().get(&digest))
    }

    /// Makes `digest` absent after a read of it found no chunk, unless its state
    /// changed from `seen` during the read: the bytes of a put are newer, and the read
    /// of another get was first. Gives whether it made the digest absent.
    fn forget(&self, digest: Digest, seen: Peek) -> bool {
        let mut chunks = self.chunks.borrow_mut();
        let unchanged = Peek::of(chunks.get(&digest)) == seen;
        if unchanged {
            chunks.remove(&digest);
        }
        unchanged
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
        // A read that ran into a put has unknown bytes, so only a get that makes the
        // digest absent counts.
        let absent = self.forget(digest, seen);
        if absent && !seen.left() {
            self.corruptions
                .set(self.corruptions.get().saturating_add(1));
        }
        Ok(None)
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
}

impl Peek {
    fn of(state: Option<&State>) -> Self {
        match state {
            None => Self::Absent,
            Some(State::Listed(serial)) => Self::Listed(*serial),
            Some(State::Held(serial)) => Self::Held(*serial),
            Some(State::Writing(_)) => Self::Writing,
        }
    }

    /// Whether a put of this open left the file before its end, so its bytes say
    /// nothing of the disk. Only the open lists a file by serial 0.
    fn left(self) -> bool {
        matches!(self, Self::Listed(1..))
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

/// A put whose chunk comes in parts. [`Receipt::write`] hashes and writes each part to
/// a part file as it comes, and [`Receipt::end`] checks the chunk and makes the part
/// the file of the chunk, or [`Receipt::discard`] removes the part. A drop of the
/// receipt, or of either call before it removes or renames the part, leaves the part
/// until the next floor check, which removes it. A drop of `end` from its remove of a
/// listed file of the chunk keeps the part, then the only copy of the chunk, for the
/// next floor check or put of the digest to place.
struct Receipt<'a> {
    store: &'a Store,
    digest: Digest,
    hasher: Hasher,
    part: Part<'a>,
    /// The bytes written.
    at: u64,
    /// Held only to free the slot on the drop.
    _slot: Slot<'a>,
}

impl Receipt<'_> {
    /// Hashes `bytes` and writes them after the parts before them. The parts hold at
    /// most the chunk's length.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when the write fails.
    async fn write(&mut self, bytes: Block) -> Result<(), Error> {
        self.hasher.update(&bytes);
        let at = self.at;
        self.at +=
            u64::try_from(bytes.len()).expect("invariant: a length fits in 64 bits");
        let written = self
            .part
            .file
            .write_at(at, std::slice::from_ref(&bytes))
            .await;
        self.store.counted(written.map_err(Error::from))
    }

    /// Checks that the parts, which hold the chunk's length, hash to the digest, and
    /// makes the chunk durable. It returns only after the chunk is durable.
    ///
    /// # Errors
    ///
    /// [`Error::Mismatch`] when the parts hash to another digest, which removes the
    /// part as [`Receipt::discard`] does, and [`Error::Files`] when a file call fails.
    /// The digest then keeps the state it had, other than after an error of the
    /// rename, which makes it absent, and of the last sync, which makes it `Listed`.
    async fn end(self) -> Result<(), Error> {
        let Self {
            store,
            digest,
            hasher,
            part,
            ..
        } = self;
        let found = hasher.finish();
        if found != digest {
            store.remove_part(part).await;
            return Err(Error::Mismatch { digest, found });
        }
        store.counted(store.place(digest, part).await)
    }

    /// Ends the put with no chunk, when its parts do not come whole. It removes the
    /// part, so a peer that stops in its bodies leaves no file to fill the disk.
    async fn discard(self) {
        self.store.remove_part(self.part).await;
    }
}

/// The file that a receipt writes, at a path of its own.
struct Part<'a> {
    file: File,
    name: Name<'a>,
}

/// The path of a part, and what its drop does with it.
struct Name<'a> {
    store: &'a Store,
    path: PathBuf,
    fate: Fate,
}

/// What the drop of a [`Name`] does with its part.
enum Fate {
    /// Lists it for the next floor check to remove.
    Remove,
    /// Lists it in [`Store::kept`] for the next floor check or put of the digest to
    /// place, as it can be the only copy of its chunk. A part of the digest that the
    /// list held goes to the next floor check to remove.
    Place(Digest),
    /// Nothing: it is removed or renamed, or the next open takes it.
    Leave,
}

impl Drop for Name<'_> {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.path);
        match self.fate {
            Fate::Remove => self.store.dropped.borrow_mut().push_back(path),
            Fate::Place(digest) => {
                let mut kept = self.store.kept.borrow_mut();
                // A digest keeps one part, so failed places take no more room. The
                // older one can be gone, renamed by a dropped place, so it goes.
                let older = (kept.range((digest, PathBuf::new())..).next())
                    .filter(|(other, _)| *other == digest)
                    .cloned();
                if let Some(older) = older {
                    kept.remove(&older);
                    self.store.dropped.borrow_mut().push_back(older.1);
                }
                kept.insert((digest, path));
            }
            Fate::Leave => {}
        }
    }
}

/// A put in flight. It holds [`State::Writing`] for its digest, and its drop sets the
/// next state and wakes the calls that waited. A drop drops the file call in flight:
/// the next write open or remove of the path, and the next `Files::free`, wait for it
/// in `env::files`.
struct Flight<'a> {
    store: &'a Store,
    digest: Digest,
    serial: u64,
    /// The state after the flight. `None` is absent.
    after: Option<State>,
}

impl<'a> Flight<'a> {
    /// Takes `digest` from the state it is in, which is absent or `Listed`. A drop
    /// before the first call that can make or change the file leaves that state.
    fn new(store: &'a Store, digest: Digest) -> Self {
        let before = store
            .chunks
            .borrow_mut()
            .insert(digest, State::Writing(BTreeMap::new()));
        let serial = store.flights.get();
        store.flights.set(serial + 1);
        Flight {
            store,
            digest,
            serial,
            after: before,
        }
    }

    /// Writes `chunk` to the file of the digest, durably, or places a kept part of the
    /// digest in place of the write. The digest is `Held` after it. An error of the
    /// write leaves the state that a drop leaves.
    async fn put(mut self, chunk: &Block) -> Result<(), Error> {
        let store = self.store;
        // Each open takes its part out of the list, so the loop ends.
        loop {
            let kept = (store.kept.borrow().range((self.digest, PathBuf::new())..))
                .next()
                .filter(|(digest, _)| *digest == self.digest)
                .map(|(_, path)| path.clone());
            let Some(path) = kept else {
                break;
            };
            // After a failed place, a write would make a file at the digest name, and
            // the next open would remove the part, the only copy of the chunk.
            if let Some(part) = store.open_kept(self.digest, path).await? {
                return self.place(part).await;
            }
        }
        let written = async {
            let file = self.create(chunk.len()).await?;
            let synced = async {
                file.write_at(0, std::slice::from_ref(chunk)).await?;
                file.sync().await
            }
            .await;
            file.close().await;
            synced?;
            self.store.files.sync_dir(&self.store.dir).await?;
            Ok(())
        }
        .await;
        if written.is_ok() {
            self.after = Some(State::Held(self.serial));
        }
        written
    }

    /// Opens the file of the digest with `len` bytes, once the floor leaves room for
    /// it. From its create, the first call that can make or change the file, a drop
    /// leaves the digest `Listed` by the serial of this flight.
    async fn create(&mut self, len: usize) -> Result<File, Error> {
        let store = self.store;
        let path = store.path(self.digest);
        let bytes = u64::try_from(len).expect("invariant: a length fits in 64 bits");
        store.room(len, &path, Mode::Write).await?;
        let mode = Mode::Create { len: bytes };
        self.after = Some(State::Listed(self.serial));
        let file = match store.files.open(&path, mode).await {
            // A file of another length at the name is not the chunk.
            Err(files::Error::Length { .. }) => {
                store.files.remove(&path).await?;
                store.files.open(&path, mode).await?
            }
            opened => opened?,
        };
        Ok(file)
    }

    /// Renames `part`, a synced part whose bytes hash to the digest, to the file of
    /// the digest, in place of a file there, and syncs the directory. The digest is
    /// `Held` after it. After an error before the rename, it keeps its state; after an
    /// error of the rename, it is absent; after an error of the last sync, it is
    /// `Listed`, as the file can then be the only copy of the chunk. After an error,
    /// the part goes unless it can be the only copy of the chunk, and then stays in
    /// [`Store::kept`].
    async fn place(mut self, mut part: Part<'_>) -> Result<(), Error> {
        let store = self.store;
        let path = store.path(self.digest);
        let from_kept = matches!(part.name.fate, Fate::Place(_));
        let renamed = async {
            // The name of the part is durable before the listed file goes, so a crash
            // between the remove and the rename keeps a copy of the chunk.
            if matches!(self.after, Some(State::Listed(_))) && !from_kept {
                store.files.sync_dir(&store.dir).await?;
                // From the remove, the part can be the only copy of the chunk.
                part.name.fate = Fate::Place(self.digest);
            }
            if let Err(error) = store.files.remove(&path).await {
                // A failed remove leaves the listed file.
                if !from_kept {
                    part.name.fate = Fate::Remove;
                }
                return Err(error);
            }
            self.after = Some(State::Listed(self.serial));
            let renamed = part.file.rename(&path).await;
            if renamed.is_err() {
                self.after = None;
            }
            renamed
        };
        if let Err(error) = renamed.await {
            if matches!(part.name.fate, Fate::Place(_)) {
                part.file.close().await;
            } else {
                store.remove_part(part).await;
            }
            return Err(error.into());
        }
        part.name.fate = Fate::Leave;
        part.file.close().await;
        let synced = store.files.sync_dir(&store.dir).await;
        self.after = Some(match synced {
            Ok(()) => State::Held(self.serial),
            Err(_) => State::Listed(self.serial),
        });
        Ok(synced?)
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut chunks = self.store.chunks.borrow_mut();
        let Some(State::Writing(wakers)) = chunks.remove(&self.digest) else {
            unreachable!("invariant: a put in flight holds the writing state");
        };
        if let Some(after) = self.after.take() {
            chunks.insert(self.digest, after);
        }
        drop(chunks);
        wakers.into_values().for_each(Waker::wake);
    }
}

/// The digest that `name` spells, and whether `name` is of a part: 64 lowercase hex
/// digits, then for a part a dot and the key of its receipt in decimal.
fn named(name: &Path) -> Option<(Digest, bool)> {
    let name = name.to_str()?;
    let Some((hex, key)) = name.split_once('.') else {
        return Some((digest(name)?, false));
    };
    let canonical = key.parse::<u64>().ok()?.to_string() == key;
    canonical.then_some((digest(hex)?, true))
}

/// The digest that `name` spells, when it is 64 lowercase hex digits.
fn digest(name: &str) -> Option<Digest> {
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
    use std::future::{pending, poll_fn};
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
    const BUDGET: u64 = 1 << 20;

    /// A pool of `budget` bytes.
    fn create_pool(budget: u64) -> Rc<Pool> {
        let config = block::Config::new(budget).expect("the budget fits");
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

    /// The part of the receipt of `digest` whose slot has `key`.
    fn part(digest: Digest, key: u64) -> PathBuf {
        Path::new(DIR).join(format!("{digest}.{key}"))
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

    /// The state of a digest, as [`seen`] finds it.
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Absent,
        Listed,
        Held,
    }

    /// The state of `digest`, through public calls only: a get opens no file of an
    /// absent digest, and a put of `block` opens none of a held one. A listed or held
    /// digest counts one failure, and a listed one two, so call it after each check
    /// of [`Status::failures`].
    async fn seen(
        node: &sim::node::Node,
        store: &Store,
        digest: Digest,
        block: &Block,
    ) -> Seen {
        let at = path(digest);
        node.fail_file(&at, Operation::Open);
        match store.get(digest).await {
            Ok(None) => {
                assert_no_open(node, &at).await;
                return Seen::Absent;
            }
            got => assert_eq!(got.err(), Some(io(&at, Operation::Open))),
        }
        node.fail_file(&at, Operation::Open);
        match store.put(digest, block).await {
            Ok(()) => {
                assert_no_open(node, &at).await;
                Seen::Held
            }
            put => {
                assert_eq!(put, Err(io(&at, Operation::Open)));
                Seen::Listed
            }
        }
    }

    /// Drops a put of `block` with its second file call in flight: its create, or the
    /// open of its floor check when that runs.
    async fn drop_put(
        node: &sim::node::Node,
        store: &Store,
        digest: Digest,
        block: &Block,
    ) {
        {
            let mut put = pin!(store.put(digest, block));
            assert_eq!(poll_once(&mut put).await, Poll::Pending);
            node.clock().sleep(Span::from_nanos(100_000)).await;
            assert_eq!(poll_once(&mut put).await, Poll::Pending);
        }
        // A dropped call runs to its end, in 100 us at most.
        node.clock().sleep(Span::from_nanos(200_000)).await;
        let file = node.files().open(&path(digest), Mode::Read).await;
        file.expect("the dropped put left a file").close().await;
    }

    /// Drops a put of `block` with its first file call, the read of the free bytes,
    /// in flight.
    async fn drop_put_in_free(
        node: &sim::node::Node,
        store: &Store,
        digest: Digest,
        block: &Block,
    ) {
        {
            let mut put = pin!(store.put(digest, block));
            assert_eq!(poll_once(&mut put).await, Poll::Pending);
        }
        // A dropped call runs to its end, in 100 us at most.
        node.clock().sleep(Span::from_nanos(200_000)).await;
    }

    /// Fails unless the file of `digest` holds `bytes`.
    async fn assert_bytes(node: &sim::node::Node, digest: Digest, bytes: &[u8]) {
        let file = node.files().open(&path(digest), Mode::Read).await.unwrap();
        let space = create_pool(BUDGET).alloc(bytes.len()).unwrap();
        let read = file.read_at(0, space).await;
        file.close().await;
        assert_eq!(&read.unwrap().freeze()[..], bytes);
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
        let block = create_pool(BUDGET).copy(bytes).unwrap();
        file.write_at(offset, &[block]).await.unwrap();
        file.sync().await.unwrap();
        file.close().await;
    }

    /// Makes a file at `path` with the bytes of `block`, durably, as a defect would.
    async fn create_file(node: &sim::node::Node, path: &Path, block: &Block) {
        let files = node.files();
        files.create_dir(Path::new(DIR)).await.unwrap();
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

    /// Fails when a get or a put of `store` counted as a failure.
    fn assert_no_failure(store: &Store) {
        assert_eq!(store.status().failures, 0);
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
                assert_no_failure(&store);
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

        // A new file takes a name in the directory, which the free bytes do not show.
        #[test]
        fn of_the_empty_chunk_with_no_file_under_the_floor_gives_floor() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 0);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes + 1).await;
                let error = store.put(digest, &block).await.unwrap_err();
                let expected = Error::Floor {
                    len: 0,
                    free_bytes,
                    floor_bytes: free_bytes + 1,
                };
                assert_eq!(error, expected);
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, Vec::<PathBuf>::new());
            })
            .unwrap();
        }

        #[test]
        fn of_the_empty_chunk_over_an_empty_file_under_the_floor_stores_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 0);
                create_empty(&node, &path(digest)).await;
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes + 1).await;
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn longer_than_the_free_bytes_gives_floor_and_writes_nothing() {
            let (mut sim, node) = create_node(0, 64 << 10);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 128 << 10);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, 1).await;
                let error = store.put(digest, &block).await.unwrap_err();
                let expected = Error::Floor {
                    len: 128 << 10,
                    free_bytes,
                    floor_bytes: 1,
                };
                assert_eq!(error, expected);
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, Vec::<PathBuf>::new());
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
        fn over_a_file_of_its_length_under_the_floor_stores_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                let (_, other) = chunk(9, 3000);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes + 1).await;
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
                assert_eq!(node.files().free().await.unwrap(), free_bytes);
            })
            .unwrap();
        }

        #[test]
        fn over_a_file_of_another_length_under_the_floor_gives_floor_and_keeps_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                let (_, other) = chunk(9, 512);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let free_bytes = node.files().free().await.unwrap();
                let floor_bytes = free_bytes + 1;
                let store = open_leaving(&node, floor_bytes).await;
                let error = store.put(digest, &block).await.unwrap_err();
                let expected = Error::Floor {
                    len: 3000,
                    free_bytes,
                    floor_bytes,
                };
                assert_eq!(error, expected);
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, [PathBuf::from(digest.to_string())]);
                assert_eq!(node.files().free().await.unwrap(), free_bytes);
            })
            .unwrap();
        }

        #[test]
        fn over_a_longer_file_under_the_floor_gives_floor_and_keeps_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                let (_, other) = chunk(9, 4096);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &other).await;
                let free_bytes = node.files().free().await.unwrap();
                let floor_bytes = free_bytes + 1;
                let store = open_leaving(&node, floor_bytes).await;
                let error = store.put(digest, &block).await.unwrap_err();
                let expected = Error::Floor {
                    len: 3000,
                    free_bytes,
                    floor_bytes,
                };
                assert_eq!(error, expected);
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, [PathBuf::from(digest.to_string())]);
                assert_eq!(node.files().free().await.unwrap(), free_bytes);
            })
            .unwrap();
        }

        #[test]
        fn after_a_remove_with_no_sync_the_floor_counts_the_removed_file() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (a, a_block) = chunk(7, 3000);
                let (b, b_block) = chunk(8, 1000);
                let (_, other) = chunk(9, 4096);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(a), &other).await;
                let before = node.files().free().await.unwrap();
                let floor_bytes = before - 3999;
                let store = open_leaving(&node, floor_bytes).await;
                node.fail_file(&path(a), Operation::Sync);
                let error = store.put(a, &a_block).await.unwrap_err();
                assert_eq!(error, io(&path(a), Operation::Sync));
                let free_bytes = node.files().free().await.unwrap();
                assert_eq!(free_bytes, before - 3000);
                let error = store.put(b, &b_block).await.unwrap_err();
                let expected = Error::Floor {
                    len: 1000,
                    free_bytes,
                    floor_bytes,
                };
                assert_eq!(error, expected);
            })
            .unwrap();
        }

        #[test]
        fn over_a_file_under_the_floor_whose_open_fails_gives_io() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &block).await;
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes + 1).await;
                node.fail_file(&path(digest), Operation::Open);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::Open));
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
                assert_eq!(store.status().corruptions, 0);
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
                assert_eq!(store.status().corruptions, 0);
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
                assert_no_failure(&store);
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
                assert_eq!(store.status().corruptions, 1);
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
                assert_no_failure(&store);
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
                assert_no_failure(&store);
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
                assert_no_failure(&store);
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

        // The first put drops with its write in flight, and a get drops in its open.
        // The next put must not find the file busy.
        #[test]
        fn after_a_dropped_put_and_a_dropped_get_is_not_busy() {
            let (mut sim, node) = create_default_node(5);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                {
                    let mut put = pin!(store.put(digest, &block));
                    assert_eq!(poll_once(&mut put).await, Poll::Pending);
                    node.clock().sleep(Span::from_nanos(100_000)).await;
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
            for seed in 0..128 {
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

        #[test]
        fn after_a_store_dropped_with_a_remove_in_flight_keeps_the_chunk() {
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
                let names = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(names, vec![PathBuf::from(digest.to_string())]);
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn after_a_store_dropped_with_a_write_in_flight_puts_the_chunk() {
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
                assert_no_failure(&store);
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
                assert_no_failure(&store);
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
        fn whose_sync_fails_gives_io_and_leaves_the_digest_listed() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                node.fail_file(&path(digest), Operation::Sync);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::Sync));
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Listed);
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
        // of another length, b beside it. In sim config 13, the removed file still
        // holds its room at b's second create, after its sync, so b gives `Full`, as
        // the disk has no room at that instant. a's final sync frees the room, and a
        // put of b again stores it.
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
                assert_eq!(store.status().corruptions, 0);
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

        // Each poll starts one call (the free, the create, the write, the sync) and
        // the sleep after it ends that call, so the put drops with the call of
        // `polls` in flight. A get and a second put wait for it from their own tasks.
        #[test]
        fn dropped_in_flight_leaves_the_chunk_whole_or_absent() {
            for polls in 1..=4 {
                let (mut sim, node) = create_default_node(5);
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
                assert_eq!(store.status().corruptions, 1);
                store.put(digest, &block).await.unwrap();
                assert!(stale.await.unwrap().is_none());
                assert_eq!(store.status().corruptions, 1);
                node.fail_file(&path(digest), Operation::WriteAt);
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        // A stale get of torn listed bytes ends after a dropped put left the digest
        // listed again. It must not forget the whole chunk.
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
                assert_eq!(store.status().corruptions, 0);
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn after_a_dropped_put_reads_the_file_one_time() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                drop_put(&node, &store, digest, &block).await;
                assert_absent(&store, digest).await;
                node.fail_file(&path(digest), Operation::Open);
                assert_absent(&store, digest).await;
                assert_no_open(&node, &path(digest)).await;
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
                assert_eq!(store.status().corruptions, 1);
                assert_no_failure(&store);
                // The digest is absent now: no second read, no second count.
                node.fail_file(&path(digest), Operation::Open);
                assert_absent(&store, digest).await;
                assert_eq!(store.status().corruptions, 1);
                assert_no_open(&node, &path(digest)).await;
                // A put goes over the changed bytes.
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
                assert_eq!(store.status().corruptions, 1);
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
                assert_eq!(store.status().corruptions, 1);
                store.put(digest, &block).await.unwrap();
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
                assert_no_failure(&store);
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
                assert_eq!(store.status().corruptions, 0);
                assert_no_failure(&store);
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
                assert_eq!(store.status().corruptions, 0);
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
                assert_no_failure(&store);
            })
            .unwrap();
        }
    }

    mod status {
        use super::*;

        fn status(corruptions: u64, failures: u64) -> Status {
            Status {
                corruptions,
                failures,
            }
        }

        #[test]
        fn of_a_new_store_counts_nothing() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                assert_eq!(store.status(), status(0, 0));
            })
            .unwrap();
        }

        // No crash, no fault, and no write from outside the store.
        #[test]
        fn counts_no_corruption_for_a_get_after_a_dropped_put() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                drop_put(&node, &store, digest, &block).await;
                assert_absent(&store, digest).await;
                assert_eq!(store.status(), status(0, 0));
            })
            .unwrap();
        }

        // The dropped put is the second flight: each serial above 0 is a put's.
        #[test]
        fn counts_no_corruption_for_a_get_after_a_dropped_put_that_was_not_the_first() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (first, other) = chunk(8, 3000);
                store.put(first, &other).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                drop_put(&node, &store, digest, &block).await;
                assert_absent(&store, digest).await;
                assert_eq!(store.status(), status(0, 0));
            })
            .unwrap();
        }

        #[test]
        fn counts_one_corruption_for_two_gets_that_read_one_changed_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                put_bytes(&node, digest, 2999, &[8]).await;
                let mut second = pin!(store.get(digest));
                for _ in 0..2 {
                    assert!(poll_once(&mut second).await.is_pending());
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                }
                assert_absent(&store, digest).await;
                assert!(second.await.unwrap().is_none());
                assert_eq!(store.status(), status(1, 0));
            })
            .unwrap();
        }

        #[test]
        fn counts_one_for_a_torn_listed_file_after_a_put_dropped_before_its_create() {
            for seed in 0..64 {
                let (mut sim, node) = create_default_node(seed);
                sim.run_on(&node, move |node, _| async move {
                    let (digest, block) = chunk(7, 3000);
                    let (_, other) = chunk(8, 3000);
                    node.files().create_dir(Path::new(DIR)).await.unwrap();
                    create_file(&node, &path(digest), &other).await;
                    let store = open(&node).await.unwrap();
                    drop_put_in_free(&node, &store, digest, &block).await;
                    assert_bytes(&node, digest, &other).await;
                    assert_absent(&store, digest).await;
                    assert_eq!(store.status(), status(1, 0), "seed {seed}");
                })
                .unwrap();
            }
        }

        #[test]
        fn counts_one_for_a_torn_listed_file_after_a_put_dropped_in_its_floor_check() {
            for seed in 0..64 {
                let (mut sim, node) = create_default_node(seed);
                sim.run_on(&node, move |node, _| async move {
                    let (digest, block) = chunk(7, 3000);
                    let (_, other) = chunk(8, 3000);
                    node.files().create_dir(Path::new(DIR)).await.unwrap();
                    create_file(&node, &path(digest), &other).await;
                    let free_bytes = node.files().free().await.unwrap();
                    let store = open_leaving(&node, free_bytes).await;
                    drop_put(&node, &store, digest, &block).await;
                    assert_bytes(&node, digest, &other).await;
                    assert_absent(&store, digest).await;
                    assert_eq!(store.status(), status(1, 0), "seed {seed}");
                })
                .unwrap();
            }
        }

        #[test]
        fn counts_none_for_a_torn_listed_file_after_a_put_dropped_in_its_create() {
            for seed in 0..64 {
                let (mut sim, node) = create_default_node(seed);
                sim.run_on(&node, move |node, _| async move {
                    let (digest, block) = chunk(7, 3000);
                    let (_, other) = chunk(8, 3000);
                    node.files().create_dir(Path::new(DIR)).await.unwrap();
                    create_file(&node, &path(digest), &other).await;
                    let store = open(&node).await.unwrap();
                    drop_put(&node, &store, digest, &block).await;
                    assert_bytes(&node, digest, &other).await;
                    assert_absent(&store, digest).await;
                    assert_eq!(store.status(), status(0, 0), "seed {seed}");
                })
                .unwrap();
            }
        }

        #[test]
        fn a_get_after_a_put_dropped_before_its_create_makes_no_file_call() {
            for seed in 0..64 {
                let (mut sim, node) = create_default_node(seed);
                sim.run_on(&node, move |node, _| async move {
                    let store = open(&node).await.unwrap();
                    let (digest, block) = chunk(7, 3000);
                    drop_put_in_free(&node, &store, digest, &block).await;
                    node.fail_file(&path(digest), Operation::Open);
                    assert_absent(&store, digest).await;
                    assert_no_open(&node, &path(digest)).await;
                    assert_eq!(store.status(), status(0, 0));
                })
                .unwrap();
            }
        }

        // The open lists names, so it cannot tell the file from a torn write.
        #[test]
        fn counts_a_corruption_for_a_get_after_a_dropped_put_of_an_earlier_open() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                drop_put(&node, &store, digest, &block).await;
                drop(store);
                let store = open(&node).await.unwrap();
                assert_absent(&store, digest).await;
                assert_eq!(store.status(), status(1, 0));
            })
            .unwrap();
        }

        #[test]
        fn counts_a_put_whose_write_fails() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                node.fail_file(&path(digest), Operation::WriteAt);
                let error = store.put(digest, &block).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::WriteAt));
                assert_eq!(store.status(), status(0, 1));
                assert_absent(&store, digest).await;
                assert_eq!(store.status(), status(0, 1));
                store.put(digest, &block).await.unwrap();
                assert_eq!(store.status(), status(0, 1));
            })
            .unwrap();
        }

        // A file error that is not `Io`: the rule counts each one but `Full`.
        #[test]
        fn counts_a_put_of_a_file_that_another_handle_holds() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let create = Mode::Create { len: 3000 };
                let held = node.files().open(&path(digest), create).await.unwrap();
                let error = store.put(digest, &block).await.unwrap_err();
                let busy = files::Error::Busy { path: path(digest) };
                assert_eq!(error, Error::Files(busy));
                assert_eq!(store.status(), status(0, 1));
                held.close().await;
            })
            .unwrap();
        }

        #[test]
        fn counts_a_get_whose_read_fails() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                node.fail_file(&path(digest), Operation::ReadAt);
                let error = store.get(digest).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::ReadAt));
                assert_eq!(store.status(), status(0, 1));
                let got = store.get(digest).await.unwrap().unwrap();
                assert_eq!(&got[..], &block[..]);
                assert_eq!(store.status(), status(0, 1));
            })
            .unwrap();
        }

        #[test]
        fn counts_each_failed_call() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                for failures in 1..=2 {
                    node.fail_file(&path(digest), Operation::Open);
                    let error = store.put(digest, &block).await.unwrap_err();
                    assert_eq!(error, io(&path(digest), Operation::Open));
                    assert_eq!(store.status(), status(0, failures));
                }
                store.put(digest, &block).await.unwrap();
                node.fail_file(&path(digest), Operation::Open);
                let error = store.get(digest).await.unwrap_err();
                assert_eq!(error, io(&path(digest), Operation::Open));
                assert_eq!(store.status(), status(0, 3));
            })
            .unwrap();
        }

        // The put that waits gives the result of its own write, so each of the two
        // counts.
        #[test]
        fn counts_a_put_that_waited_for_a_failed_put() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let expected = io(Path::new(DIR), Operation::SyncDir);
                for _ in 0..2 {
                    node.fail_file(Path::new(DIR), Operation::SyncDir);
                }
                let puts = join(store.put(digest, &block), store.put(digest, &block));
                let (first, second) = puts.await;
                assert_eq!(first.unwrap_err(), expected);
                assert_eq!(second.unwrap_err(), expected);
                assert_eq!(store.status(), status(0, 2));
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
            let digest = chunk(7, 10).0;
            for name in [
                "notes",
                &"z".repeat(64),
                &digest.to_string().to_uppercase(),
                &format!("{digest}."),
                &format!("{digest}.01"),
                &format!("{digest}.+1"),
                &format!("{digest}.x"),
                &format!("{digest}.18446744073709551616"),
                &format!("{}.0", "z".repeat(64)),
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
        fn with_a_part_and_the_file_of_its_digest_removes_the_part() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                create_file(&node, &part(digest, 3), &chunk(8, 3000).1).await;
                let store = open(&node).await.unwrap();
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn with_a_part_alone_lists_it_as_the_file_of_its_digest() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (whole, block) = chunk(7, 3000);
                let (torn, _) = chunk(9, 3000);
                create_file(&node, &part(whole, 0), &block).await;
                create_file(&node, &part(torn, 18_446_744_073_709_551_615), &block)
                    .await;
                let store = open(&node).await.unwrap();
                let mut listed = node.files().list(Path::new(DIR)).await.unwrap();
                listed.sort();
                let mut named: Vec<PathBuf> =
                    vec![whole.to_string().into(), torn.to_string().into()];
                named.sort();
                assert_eq!(listed, named);
                assert_eq!(&store.get(whole).await.unwrap().unwrap()[..], &block[..]);
                assert_absent(&store, torn).await;
                assert_eq!(store.status().corruptions, 1);
                assert_eq!(seen(&node, &store, whole, &block).await, Seen::Listed);
            })
            .unwrap();
        }

        #[test]
        fn with_a_part_whose_remove_fails_gives_io() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                create_file(&node, &part(digest, 0), &block).await;
                node.fail_file(&part(digest, 0), Operation::Remove);
                let error = open(&node).await.unwrap_err();
                assert_eq!(error, io(&part(digest, 0), Operation::Remove));
            })
            .unwrap();
        }

        #[test]
        fn with_a_part_whose_rename_fails_gives_io() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &part(digest, 0), &block).await;
                node.fail_file(&part(digest, 0), Operation::Rename);
                let error = open(&node).await.unwrap_err();
                assert_eq!(error, io(&part(digest, 0), Operation::Rename));
                let store = open(&node).await.unwrap();
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
            })
            .unwrap();
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
                assert_eq!(store.status().corruptions, 0);
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
        // was dropped in its write, which leaves its digest listed by its serial. The
        // output names each state with no pointer.
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
                assert!(text.contains("Listed(2)"), "{text}");
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
                 digest or by the part of a digest"
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
                "a put of 3000 bytes would leave less than the floor of 2048 bytes \
                 free on a disk with 4096 free"
            );
        }
    }

    mod dropped {
        use super::*;

        /// A waker that counts its wakes.
        #[derive(Default)]
        pub(super) struct Count(AtomicU64);

        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        /// Polls `future` once with a waker of `count`.
        pub(super) fn poll_with<F: Future>(
            future: Pin<&mut F>,
            count: &Arc<Count>,
        ) -> Poll<F::Output> {
            let waker = Waker::from(Arc::clone(count));
            future.poll(&mut std::task::Context::from_waker(&waker))
        }

        pub(super) fn wakes(count: &Count) -> u64 {
            count.0.load(Ordering::Relaxed)
        }

        #[test]
        fn puts_hold_no_open_file() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let mut digests = Vec::new();
                for index in 0..200 {
                    let (digest, block) = chunk(7, 3000 + index);
                    let mut put = pin!(store.put(digest, &block));
                    assert_eq!(poll_once(&mut put).await, Poll::Pending);
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                    assert_eq!(poll_once(&mut put).await, Poll::Pending);
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                    assert_eq!(poll_once(&mut put).await, Poll::Pending);
                    digests.push(digest);
                }
                node.clock().sleep(Span::SECOND).await;
                let mut busy = 0;
                for digest in digests {
                    match node.files().open(&path(digest), Mode::Write).await {
                        Ok(file) => file.close().await,
                        Err(files::Error::Busy { .. }) => busy += 1,
                        Err(error) => panic!("{error:?}"),
                    }
                }
                assert_eq!(busy, 0);
            })
            .unwrap();
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

        // One put at a time, each dropped at a random wait. The floor leaves room for
        // one chunk, and no put is in flight at the check.
        #[test]
        fn puts_one_at_a_time_keep_the_floor_free() {
            for run in 0..600 {
                let (mut sim, node) = create_node(run, 1 << 20);
                sim.run_on(&node, move |node, _| async move {
                    let mut rng = env::rng::Rng::from_seed(run);
                    node.files().create_dir(Path::new(DIR)).await.unwrap();
                    let floor_bytes = node.files().free().await.unwrap() - 1000;
                    let store = open_leaving(&node, floor_bytes).await;
                    let (mut stored, mut refused) = (0, 0);
                    for index in 0..8 {
                        let (digest, block) = chunk(index + 1, 1000);
                        let mut put = pin!(store.put(digest, &block));
                        for _ in 0..rng.below(40) {
                            match poll_once(&mut put).await {
                                Poll::Pending => {}
                                Poll::Ready(Ok(())) => {
                                    stored += 1;
                                    break;
                                }
                                Poll::Ready(Err(Error::Floor { .. })) => {
                                    refused += 1;
                                    break;
                                }
                                Poll::Ready(Err(error)) => panic!("{error:?}"),
                            }
                            node.clock().sleep(Span::from_nanos(5_000)).await;
                        }
                    }
                    node.clock().sleep(Span::SECOND).await;
                    let files = node.files().list(Path::new(DIR)).await.unwrap().len();
                    let free_bytes = node.files().free().await.unwrap();
                    assert!(
                        free_bytes >= floor_bytes,
                        "run {run}: {free_bytes} bytes are free and the floor is \
                         {floor_bytes}; {files} files, {stored} puts stored, \
                         {refused} puts gave Floor"
                    );
                })
                .unwrap();
            }
        }
    }

    mod receive {
        use super::dropped::{Count, poll_with, wakes};
        use super::*;

        /// Opens [`PUTS_MAX`] receipts of chunks of 100 bytes, one digest each.
        async fn receipts(store: &Store) -> Vec<Receipt<'_>> {
            let mut open = Vec::new();
            for byte in 1..=PUTS_MAX {
                let (digest, _) = chunk(u8::try_from(byte).unwrap(), 100);
                open.push(store.receive(digest, 100).await.unwrap());
            }
            open
        }

        /// Writes `block` to `receipt` and ends it.
        async fn end(mut receipt: Receipt<'_>, block: &Block) -> Result<(), Error> {
            receipt.write(block.clone()).await?;
            receipt.end().await
        }

        #[test]
        fn over_puts_max_waits_and_heads_go_in_the_order_they_come() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let mut open = receipts(&store).await;
                let (first, _) = chunk(101, 100);
                let (later, _) = chunk(102, 100);
                let mut first = pin!(store.receive(first, 100));
                assert!(poll_once(&mut first).await.is_pending());
                let mut later = pin!(store.receive(later, 100));
                assert!(poll_once(&mut later).await.is_pending());
                open.pop().unwrap().discard().await;
                assert!(poll_once(&mut later).await.is_pending());
                let _first = first.await.unwrap();
                assert!(poll_once(&mut later).await.is_pending());
                drop(open.pop());
                later.await.unwrap();
            })
            .unwrap();
        }

        #[test]
        fn a_first_head_dropped_after_its_wake_wakes_the_next() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let mut open = receipts(&store).await;
                let (first, _) = chunk(101, 100);
                let (later, _) = chunk(102, 100);
                let dropped = Arc::new(Count::default());
                let mut first = Box::pin(store.receive(first, 100));
                assert!(poll_with(first.as_mut(), &dropped).is_pending());
                let kept = Arc::new(Count::default());
                let mut later = pin!(store.receive(later, 100));
                assert!(poll_with(later.as_mut(), &kept).is_pending());
                drop(open.pop());
                assert_eq!((wakes(&dropped), wakes(&kept)), (1, 0));
                drop(first);
                assert_eq!(wakes(&kept), 1);
                assert_eq!(Arc::strong_count(&dropped), 1);
                later.await.unwrap();
            })
            .unwrap();
        }

        #[test]
        fn of_a_held_chunk_keeps_its_file_and_removes_the_part() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                node.fail_file(&path(digest), Operation::Open);
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                let receipt = store.receive(digest, 3000).await.unwrap();
                assert_eq!(end(receipt, &block).await, Ok(()));
                let (found, other) = chunk(8, 3000);
                let receipt = store.receive(digest, 3000).await.unwrap();
                let mismatch = Error::Mismatch { digest, found };
                assert_eq!(end(receipt, &other).await, Err(mismatch));
                assert_no_open(&node, &path(digest)).await;
                assert_no_sync_dir(&node).await;
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
                assert_no_failure(&store);
            })
            .unwrap();
        }

        #[test]
        fn of_a_held_chunk_that_a_get_forgets_in_the_body_stores_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                store.put(digest, &block).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                put_bytes(&node, digest, 0, &[0xff; 16]).await;
                assert_absent(&store, digest).await;
                assert_eq!(store.status().corruptions, 1);
                assert_eq!(end(receipt, &block).await, Ok(()));
                assert_bytes(&node, digest, &block).await;
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn whose_parts_hash_to_another_digest_removes_the_file_and_counts_nothing() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, _) = chunk(7, 3000);
                let (found, other) = chunk(8, 3000);
                let receipt = store.receive(digest, 3000).await.unwrap();
                let mismatch = Error::Mismatch { digest, found };
                assert_eq!(end(receipt, &other).await, Err(mismatch));
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, Vec::<PathBuf>::new());
                assert_absent(&store, digest).await;
                let counted = Status {
                    corruptions: 0,
                    failures: 0,
                };
                assert_eq!(store.status(), counted);
            })
            .unwrap();
        }

        #[test]
        fn two_slots_that_free_at_once_take_two_heads() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let mut open = receipts(&store).await;
                let (first, _) = chunk(101, 100);
                let (later, _) = chunk(102, 100);
                let (woken, kept) =
                    (Arc::new(Count::default()), Arc::new(Count::default()));
                let mut first = pin!(store.receive(first, 100));
                assert!(poll_with(first.as_mut(), &woken).is_pending());
                let mut later = pin!(store.receive(later, 100));
                assert!(poll_with(later.as_mut(), &kept).is_pending());
                drop(open.pop());
                drop(open.pop());
                assert_eq!(wakes(&kept), 0);
                let _first = first.await.unwrap();
                assert_eq!(wakes(&kept), 1);
                later.await.unwrap();
            })
            .unwrap();
        }

        // A member that sends a head and no body stalls no call of the store.
        #[test]
        fn open_with_no_parts_lets_a_get_and_a_put_of_its_digest_return() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let receipt = store.receive(digest, 3000).await.unwrap();
                assert_absent(&store, digest).await;
                store.put(digest, &block).await.unwrap();
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
                assert_eq!(end(receipt, &block).await, Ok(()));
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
                assert_bytes(&node, digest, &block).await;
                assert_no_failure(&store);
            })
            .unwrap();
        }

        #[test]
        fn of_a_listed_chunk_stores_it_durably() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            let put = block.clone();
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &chunk(8, 3000).1).await;
                let store = open(&node).await.unwrap();
                assert_eq!(
                    end(store.receive(digest, 3000).await.unwrap(), &put).await,
                    Ok(())
                );
                assert_eq!(seen(&node, &store, digest, &put).await, Seen::Held);
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, move |node, _| async move {
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
                let store = open(&node).await.unwrap();
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_parts_leaves_a_chunk_that_the_next_put_writes() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                let (_, part) = chunk(7, 1000);
                receipt.write(part).await.unwrap();
                drop(receipt);
                assert_absent(&store, digest).await;
                store.put(digest, &block).await.unwrap();
                assert_bytes(&node, digest, &block).await;
                assert_eq!(store.status().corruptions, 0);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_parts_leaves_no_part_after_the_next_receive() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, _) = chunk(7, 3000);
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                let (_, bytes) = chunk(7, 1000);
                receipt.write(bytes).await.unwrap();
                drop(receipt);
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(format!("{other}.1"))]);
                assert_no_failure(&store);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_parts_frees_its_room_for_the_next_receive() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes - 3000).await;
                let (digest, block) = chunk(7, 3000);
                drop(store.receive(digest, 3000).await.unwrap());
                let receipt = store.receive(digest, 3000).await.unwrap();
                assert_eq!(end(receipt, &block).await, Ok(()));
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_end_leaves_no_part_after_the_next_receive() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                receipt.write(block.clone()).await.unwrap();
                {
                    // The first file call of the end is the sync of the part.
                    let mut end = pin!(receipt.end());
                    assert_eq!(poll_once(&mut end).await, Poll::Pending);
                }
                // A dropped call runs to its end, in 100 us at most.
                node.clock().sleep(Span::from_nanos(200_000)).await;
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(format!("{other}.1"))]);
                assert_absent(&store, digest).await;
                assert_no_failure(&store);
            })
            .unwrap();
        }

        #[test]
        fn dropped_with_a_failed_remove_leaves_a_part_that_the_next_open_takes() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                receipt.write(block.clone()).await.unwrap();
                drop(receipt);
                node.fail_file(&part(digest, 0), Operation::Remove);
                let (other, _) = chunk(8, 100);
                store.receive(other, 100).await.unwrap().discard().await;
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Absent);
                drop(store);
                let store = open(&node).await.unwrap();
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_rename_and_then_a_receive_keep_the_chunk_with_no_failure() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                receipt.write(block.clone()).await.unwrap();
                {
                    // The fourth file call of the end is the rename.
                    let mut end = pin!(receipt.end());
                    for _ in 0..4 {
                        assert_eq!(poll_once(&mut end).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                }
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                let mut listed = node.files().list(Path::new(DIR)).await.unwrap();
                listed.sort();
                let mut names = [digest.to_string(), format!("{other}.1")];
                names.sort();
                assert_eq!(listed, names.map(PathBuf::from));
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
                assert_no_failure(&store);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_create_leaves_no_part_after_the_next_receive() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, _) = chunk(7, 3000);
                {
                    // The first file call is the free of the floor check, the second
                    // the create of the part.
                    let mut receive = pin!(store.receive(digest, 3000));
                    assert!(poll_once(&mut receive).await.is_pending());
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                    assert!(poll_once(&mut receive).await.is_pending());
                }
                node.clock().sleep(Span::from_nanos(200_000)).await;
                let left = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(left, vec![PathBuf::from(format!("{digest}.0"))]);
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(format!("{other}.1"))]);
                assert_no_failure(&store);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_the_remove_of_its_parts_frees_their_room_for_the_next_receive() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes - 3000).await;
                let (digest, block) = chunk(7, 3000);
                drop(store.receive(digest, 3000).await.unwrap());
                {
                    // Its slot is free, so its first file call is the remove.
                    let (other, _) = chunk(8, 100);
                    let mut receive = pin!(store.receive(other, 100));
                    assert!(poll_once(&mut receive).await.is_pending());
                }
                node.clock().sleep(Span::from_nanos(200_000)).await;
                let receipt = store.receive(digest, 3000).await.unwrap();
                assert_eq!(end(receipt, &block).await, Ok(()));
                assert_no_failure(&store);
            })
            .unwrap();
        }

        /// Over many orders of the ready tasks.
        #[test]
        fn dropped_twice_and_then_two_receives_at_once_both_count_their_room() {
            for seed in 0..32 {
                let (mut sim, node) = create_default_node(seed);
                sim.run_on(&node, move |node, _| async move {
                    node.files().create_dir(Path::new(DIR)).await.unwrap();
                    let free_bytes = node.files().free().await.unwrap();
                    let store = open_leaving(&node, free_bytes - 3050).await;
                    let (first, _) = chunk(6, 1500);
                    let (second, _) = chunk(7, 1500);
                    let first = store.receive(first, 1500).await.unwrap();
                    let second = store.receive(second, 1500).await.unwrap();
                    drop((first, second));
                    let (a, _) = chunk(8, 100);
                    let (b, _) = chunk(9, 100);
                    let (got_a, got_b) =
                        join(store.receive(a, 100), store.receive(b, 100)).await;
                    let got = (got_a.as_ref().err(), got_b.as_ref().err());
                    assert_eq!(got, (None, None), "seed {seed}");
                    let mut listed = node.files().list(Path::new(DIR)).await.unwrap();
                    listed.sort();
                    let mut names = [format!("{a}.2"), format!("{b}.3")];
                    names.sort();
                    assert_eq!(listed, names.map(PathBuf::from), "seed {seed}");
                })
                .unwrap();
            }
        }

        #[test]
        fn dropped_and_then_a_receive_during_the_sync_of_another_counts_its_room() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes - 3050).await;
                let (digest, _) = chunk(7, 3000);
                drop(store.receive(digest, 3000).await.unwrap());
                let (a, _) = chunk(8, 100);
                let (b, _) = chunk(9, 100);
                let mut first = pin!(store.receive(a, 100));
                // Its first file call is the remove, its second the directory sync.
                assert!(poll_once(&mut first).await.is_pending());
                node.clock().sleep(Span::from_nanos(100_000)).await;
                assert!(poll_once(&mut first).await.is_pending());
                let (got_b, got_a) = join(store.receive(b, 100), first).await;
                let got = (got_a.as_ref().err(), got_b.as_ref().err());
                assert_eq!(got, (None, None));
            })
            .unwrap();
        }

        #[test]
        fn dropped_and_then_a_put_counts_its_room() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes - 3000).await;
                let (digest, block) = chunk(7, 3000);
                drop(store.receive(digest, 3000).await.unwrap());
                assert_eq!(store.put(digest, &block).await, Ok(()));
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
            })
            .unwrap();
        }

        #[test]
        fn dropped_with_a_failed_directory_sync_counts_a_failure_and_syncs_again() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes - 3000).await;
                let (digest, block) = chunk(7, 3000);
                drop(store.receive(digest, 3000).await.unwrap());
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                let (other, _) = chunk(8, 0);
                store.receive(other, 0).await.unwrap().discard().await;
                assert_eq!(store.status().failures, 1);
                let receipt = store.receive(digest, 3000).await.unwrap();
                assert_eq!(end(receipt, &block).await, Ok(()));
            })
            .unwrap();
        }

        #[test]
        fn dropped_twice_and_then_two_receives_of_other_lengths_at_once_count_both() {
            for seed in 0..64 {
                let (mut sim, node) = create_default_node(seed);
                sim.run_on(&node, move |node, _| async move {
                    node.files().create_dir(Path::new(DIR)).await.unwrap();
                    let free_bytes = node.files().free().await.unwrap();
                    let store = open_leaving(&node, free_bytes - 3050).await;
                    let (first, _) = chunk(6, 1500);
                    let (second, _) = chunk(7, 1500);
                    let first = store.receive(first, 1500).await.unwrap();
                    let second = store.receive(second, 1500).await.unwrap();
                    drop((first, second));
                    let (a, _) = chunk(8, 100);
                    let (b, _) = chunk(9, 2000);
                    let (got_a, got_b) =
                        join(store.receive(a, 100), store.receive(b, 2000)).await;
                    let got = (got_a.as_ref().err(), got_b.as_ref().err());
                    assert_eq!(got, (None, None), "seed {seed}");
                })
                .unwrap();
            }
        }

        #[test]
        fn dropped_and_removed_makes_no_sync_at_a_later_receive() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, _) = chunk(7, 3000);
                drop(store.receive(digest, 3000).await.unwrap());
                let (other, _) = chunk(8, 100);
                store.receive(other, 100).await.unwrap().discard().await;
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                let (third, _) = chunk(9, 100);
                let _receipt = store.receive(third, 100).await.unwrap();
                assert_no_failure(&store);
                assert_no_sync_dir(&node).await;
            })
            .unwrap();
        }

        #[test]
        fn ended_leaves_no_part_for_the_next_receive_to_remove() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let receipt = store.receive(digest, 3000).await.unwrap();
                assert_eq!(end(receipt, &block).await, Ok(()));
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                assert_no_failure(&store);
                assert_no_sync_dir(&node).await;
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_the_sync_of_a_listed_end_leaves_no_part_after_the_next_receive() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                receipt.write(block.clone()).await.unwrap();
                {
                    // The second file call of the end is the directory sync.
                    let mut end = pin!(receipt.end());
                    for _ in 0..2 {
                        assert_eq!(poll_once(&mut end).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                }
                node.clock().sleep(Span::from_nanos(200_000)).await;
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                let mut names = [digest.to_string(), format!("{other}.1")];
                names.sort();
                assert_eq!(listed, names.map(PathBuf::from));
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_of_a_listed_file_keeps_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                receipt.write(block.clone()).await.unwrap();
                {
                    // The third file call of the end is the remove of the listed file.
                    let mut end = pin!(receipt.end());
                    for _ in 0..3 {
                        assert_eq!(poll_once(&mut end).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                }
                node.clock().sleep(Span::from_nanos(200_000)).await;
                let left = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(left, vec![PathBuf::from(format!("{digest}.0"))]);
                let (other, _) = chunk(8, 100);
                store.receive(other, 100).await.unwrap().discard().await;
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        /// Opens a receipt of `block` under `digest` and writes it.
        async fn written<'a>(
            store: &'a Store,
            digest: Digest,
            block: &Block,
        ) -> Receipt<'a> {
            let mut receipt = store.receive(digest, block.len()).await.unwrap();
            receipt.write(block.clone()).await.unwrap();
            receipt
        }

        /// Drops the end of `receipt` in its remove of the listed file of its digest,
        /// so its part is then the only copy of the chunk.
        async fn drop_in_remove(node: &sim::node::Node, receipt: Receipt<'_>) {
            {
                // The third file call of the end is the remove of the listed file.
                let mut end = pin!(receipt.end());
                for _ in 0..3 {
                    assert_eq!(poll_once(&mut end).await, Poll::Pending);
                    node.clock().sleep(Span::from_nanos(100_000)).await;
                }
            }
            node.clock().sleep(Span::from_nanos(200_000)).await;
        }

        /// The names in the directory of the store, in order.
        async fn names(node: &sim::node::Node) -> Vec<String> {
            let listed = node.files().list(Path::new(DIR)).await.unwrap();
            let mut names: Vec<_> = listed
                .into_iter()
                .map(|name| name.to_str().unwrap().to_owned())
                .collect();
            names.sort();
            names
        }

        /// The names of `digests`, and of the parts of `parts`, in order.
        fn sorted(digests: &[Digest], parts: &[(Digest, u64)]) -> Vec<String> {
            let mut names: Vec<_> = digests.iter().map(ToString::to_string).collect();
            names.extend(parts.iter().map(|(digest, key)| format!("{digest}.{key}")));
            names.sort();
            names
        }

        #[test]
        fn dropped_in_its_remove_of_a_listed_file_and_then_a_put_keep_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &block).await).await;
                // The put renames the part, so a write would take this fault.
                node.fail_file(&path(digest), Operation::WriteAt);
                assert_eq!(store.put(digest, &block).await, Ok(()));
                assert_eq!(names(&node).await, sorted(&[digest], &[]));
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        /// The drop, then a put whose place fails at `operation` of `at` and leaves
        /// the names of `left`, then a power cut before a floor check.
        fn assert_kept_after_a_failed_put_and_a_cut(
            at: fn(Digest) -> PathBuf,
            operation: Operation,
            left: fn(Digest) -> Vec<String>,
        ) {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            let put = block.clone();
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &put).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &put).await).await;
                node.fail_file(&at(digest), operation);
                let failed = store.put(digest, &put).await;
                assert_eq!(failed, Err(io(&at(digest), operation)));
                assert_eq!(names(&node).await, left(digest));
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            sim.run_on(&node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_and_then_a_put_whose_rename_fails_keep_the_chunk() {
            assert_kept_after_a_failed_put_and_a_cut(
                |digest| part(digest, 0),
                Operation::Rename,
                |digest| sorted(&[], &[(digest, 0)]),
            );
        }

        #[test]
        fn dropped_in_its_remove_and_then_a_put_whose_dir_sync_fails_keep_the_chunk() {
            assert_kept_after_a_failed_put_and_a_cut(
                |_| PathBuf::from(DIR),
                Operation::SyncDir,
                |digest| sorted(&[digest], &[]),
            );
        }

        #[test]
        fn dropped_in_its_remove_of_a_listed_file_is_placed_by_the_next_check() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &block).await).await;
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                assert_eq!(names(&node).await, sorted(&[digest], &[(other, 1)]));
                assert_no_failure(&store);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_and_made_absent_by_a_get_is_placed_by_the_next_check()
        {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &block).await).await;
                assert_absent(&store, digest).await;
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                assert_eq!(names(&node).await, sorted(&[digest], &[(other, 1)]));
                assert_no_failure(&store);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_of_a_chunk_that_another_end_holds_is_removed() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let first = written(&store, digest, &block).await;
                let second = written(&store, digest, &block).await;
                drop_in_remove(&node, first).await;
                second.end().await.unwrap();
                let (other, _) = chunk(8, 100);
                let _first = store.receive(other, 100).await.unwrap();
                assert_eq!(names(&node).await, sorted(&[digest], &[(other, 2)]));
                // The part left the list, so no check removes it again.
                node.fail_file(&part(digest, 0), Operation::Remove);
                let (another, _) = chunk(9, 100);
                let _second = store.receive(another, 100).await.unwrap();
                assert_no_failure(&store);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_is_skipped_by_a_check_during_another_end() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let first = written(&store, digest, &block).await;
                let second = written(&store, digest, &block).await;
                drop_in_remove(&node, first).await;
                let mut end = pin!(second.end());
                // The second file call of the end, the directory sync, is in the
                // flight of the digest.
                assert_eq!(poll_once(&mut end).await, Poll::Pending);
                node.clock().sleep(Span::from_nanos(100_000)).await;
                assert_eq!(poll_once(&mut end).await, Poll::Pending);
                let (other, _) = chunk(8, 100);
                let _first = store.receive(other, 100).await.unwrap();
                let left = sorted(&[], &[(digest, 0), (digest, 1), (other, 2)]);
                assert_eq!(names(&node).await, left);
                end.await.unwrap();
                let (another, _) = chunk(9, 100);
                let _second = store.receive(another, 100).await.unwrap();
                let left = sorted(&[digest], &[(other, 2), (another, 3)]);
                assert_eq!(names(&node).await, left);
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
                assert_no_failure(&store);
            })
            .unwrap();
        }

        #[test]
        fn dropped_twice_in_its_remove_leaves_no_part_after_the_next_check() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let first = written(&store, digest, &block).await;
                let second = written(&store, digest, &block).await;
                drop_in_remove(&node, first).await;
                drop_in_remove(&node, second).await;
                let left = sorted(&[], &[(digest, 0), (digest, 1)]);
                assert_eq!(names(&node).await, left);
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                assert_eq!(names(&node).await, sorted(&[digest], &[(other, 2)]));
                assert_no_failure(&store);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_and_a_check_whose_rename_fails_keep_it_for_the_next() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &block).await).await;
                node.fail_file(&part(digest, 0), Operation::Rename);
                let (other, _) = chunk(8, 100);
                let _first = store.receive(other, 100).await.unwrap();
                let left = sorted(&[], &[(digest, 0), (other, 1)]);
                assert_eq!(names(&node).await, left);
                assert_eq!(store.status().failures, 1);
                let (another, _) = chunk(9, 100);
                let _second = store.receive(another, 100).await.unwrap();
                let left = sorted(&[digest], &[(other, 1), (another, 2)]);
                assert_eq!(names(&node).await, left);
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_and_a_check_whose_open_of_it_fails_keep_it() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &block).await).await;
                node.fail_file(&part(digest, 0), Operation::Open);
                let (other, _) = chunk(8, 100);
                let _first = store.receive(other, 100).await.unwrap();
                let left = sorted(&[], &[(digest, 0), (other, 1)]);
                assert_eq!(names(&node).await, left);
                assert_eq!(store.status().failures, 1);
                let (another, _) = chunk(9, 100);
                let _second = store.receive(another, 100).await.unwrap();
                let left = sorted(&[digest], &[(other, 1), (another, 2)]);
                assert_eq!(names(&node).await, left);
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_and_a_put_whose_open_of_it_fails_keep_it() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &block).await).await;
                node.fail_file(&part(digest, 0), Operation::Open);
                let failed = store.put(digest, &block).await;
                assert_eq!(failed, Err(io(&part(digest, 0), Operation::Open)));
                let (other, _) = chunk(8, 100);
                let _receipt = store.receive(other, 100).await.unwrap();
                assert_eq!(names(&node).await, sorted(&[digest], &[(other, 1)]));
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_and_a_check_dropped_in_its_rename_keep_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &block).await).await;
                let (other, _) = chunk(8, 100);
                {
                    // The fourth poll leaves the check in the rename of the part.
                    let mut receive = pin!(store.receive(other, 100));
                    for _ in 0..4 {
                        assert!(poll_once(&mut receive).await.is_pending());
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                }
                node.clock().sleep(Span::from_nanos(200_000)).await;
                assert_eq!(names(&node).await, sorted(&[digest], &[]));
                let _first = store.receive(other, 100).await.unwrap();
                assert_eq!(names(&node).await, sorted(&[digest], &[(other, 2)]));
                // The part left the list, so no check opens it again.
                node.fail_file(&part(digest, 0), Operation::Open);
                let (another, _) = chunk(9, 100);
                let _second = store.receive(another, 100).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
                assert_no_failure(&store);
            })
            .unwrap();
        }

        #[test]
        fn under_the_floor_drops_in_failed_removes_keep_one_part() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open_leaving(&node, 1 << 40).await;
                for round in 0..4 {
                    for _ in 0..round {
                        node.fail_file(&path(digest), Operation::Remove);
                    }
                    let receipt = written(&store, digest, &block).await;
                    node.fail_file(&path(digest), Operation::Remove);
                    drop_in_remove(&node, receipt).await;
                }
                // The list keeps the last part, and the next check removes the other.
                let left = sorted(&[digest], &[(digest, 2), (digest, 3)]);
                assert_eq!(names(&node).await, left);
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_and_a_check_whose_remove_fails_keep_it() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &block).await).await;
                node.fail_file(&path(digest), Operation::Remove);
                let (other, _) = chunk(8, 100);
                store.receive(other, 100).await.unwrap().discard().await;
                assert_eq!(names(&node).await, sorted(&[], &[(digest, 0)]));
                assert_eq!(store.status().failures, 1);
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        #[test]
        fn dropped_in_its_remove_and_a_put_whose_remove_fails_keep_it() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, digest, &block).await).await;
                node.fail_file(&path(digest), Operation::Remove);
                let failed = store.put(digest, &block).await;
                assert_eq!(failed, Err(io(&path(digest), Operation::Remove)));
                assert_eq!(names(&node).await, sorted(&[], &[(digest, 0)]));
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        #[test]
        fn a_put_of_a_lower_digest_leaves_a_kept_part_of_another() {
            let (mut sim, node) = create_default_node(0);
            let mut pair = [chunk(7, 3000), chunk(8, 3000)];
            pair.sort_by_key(|(digest, _)| *digest);
            let [(low, low_block), (high, high_block)] = pair;
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(high), &high_block).await;
                let store = open(&node).await.unwrap();
                drop_in_remove(&node, written(&store, high, &high_block).await).await;
                assert_eq!(store.put(low, &low_block).await, Ok(()));
                let got = store.get(low).await.unwrap();
                assert_eq!(got.as_deref(), Some(&low_block[..]));
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(high).await.unwrap();
                assert_eq!(got.as_deref(), Some(&high_block[..]));
            })
            .unwrap();
        }

        #[test]
        fn a_put_of_a_higher_digest_places_its_kept_part() {
            let (mut sim, node) = create_default_node(0);
            let mut pair = [chunk(7, 3000), chunk(8, 3000)];
            pair.sort_by_key(|(digest, _)| *digest);
            let [(low, low_block), (high, high_block)] = pair;
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(low), &low_block).await;
                create_file(&node, &path(high), &high_block).await;
                let store = open(&node).await.unwrap();
                let first = written(&store, low, &low_block).await;
                let second = written(&store, high, &high_block).await;
                drop_in_remove(&node, first).await;
                drop_in_remove(&node, second).await;
                node.fail_file(&path(high), Operation::WriteAt);
                assert_eq!(store.put(high, &high_block).await, Ok(()));
            })
            .unwrap();
        }

        #[test]
        fn a_put_after_a_stale_kept_part_places_the_next_one() {
            let (mut sim, node) = create_default_node(0);
            let (digest, block) = chunk(7, 3000);
            sim.run_on(&node, move |node, _| async move {
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let first = written(&store, digest, &block).await;
                let second = written(&store, digest, &block).await;
                drop_in_remove(&node, first).await;
                let (other, _) = chunk(8, 100);
                {
                    // The fourth poll leaves the check in the rename of the part.
                    let mut receive = pin!(store.receive(other, 100));
                    for _ in 0..4 {
                        assert!(poll_once(&mut receive).await.is_pending());
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                }
                node.clock().sleep(Span::from_nanos(200_000)).await;
                // The first part is the file of the digest, and stays in the list.
                assert_eq!(names(&node).await, sorted(&[digest], &[(digest, 1)]));
                drop_in_remove(&node, second).await;
                assert_eq!(names(&node).await, sorted(&[], &[(digest, 1)]));
                // The put renames the second part, so a write would take this fault.
                node.fail_file(&path(digest), Operation::WriteAt);
                assert_eq!(store.put(digest, &block).await, Ok(()));
                assert_eq!(names(&node).await, sorted(&[digest], &[]));
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        /// Over many orders of the ready tasks.
        #[test]
        fn dropped_in_its_rename_and_at_once_a_receive_keep_the_chunk() {
            for seed in 0..32 {
                let (mut sim, node) = create_default_node(seed);
                sim.run_on(&node, move |node, _| async move {
                    let store = open(&node).await.unwrap();
                    let (digest, block) = chunk(7, 3000);
                    let mut receipt = store.receive(digest, 3000).await.unwrap();
                    receipt.write(block.clone()).await.unwrap();
                    {
                        // The fourth file call of the end is the rename.
                        let mut end = pin!(receipt.end());
                        for _ in 0..3 {
                            assert_eq!(poll_once(&mut end).await, Poll::Pending);
                            node.clock().sleep(Span::from_nanos(100_000)).await;
                        }
                        assert_eq!(poll_once(&mut end).await, Poll::Pending);
                    }
                    let (other, _) = chunk(8, 100);
                    let _receipt = store.receive(other, 100).await.unwrap();
                    let got = store.get(digest).await.unwrap();
                    assert_eq!(got.as_deref(), Some(&block[..]), "seed {seed}");
                    assert_no_failure(&store);
                })
                .unwrap();
            }
        }

        #[test]
        fn dropped_in_its_rename_leaves_a_chunk_that_a_get_gives() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                receipt.write(block.clone()).await.unwrap();
                {
                    // Each file call takes at most 100 us, and the fourth of the end
                    // is the rename: three polls before it leave no file at the name.
                    let mut end = pin!(receipt.end());
                    for _ in 0..4 {
                        assert_eq!(poll_once(&mut end).await, Poll::Pending);
                        node.clock().sleep(Span::from_nanos(100_000)).await;
                    }
                }
                // A dropped call runs to its end, in 100 us at most.
                node.clock().sleep(Span::from_nanos(200_000)).await;
                assert_bytes(&node, digest, &block).await;
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Listed);
            })
            .unwrap();
        }

        #[test]
        fn a_head_polled_with_new_wakers_keeps_the_last_one() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let mut open = receipts(&store).await;
                let (digest, _) = chunk(101, 100);
                let mut head = pin!(store.receive(digest, 100));
                let (old, new) =
                    (Arc::new(Count::default()), Arc::new(Count::default()));
                assert!(poll_with(head.as_mut(), &old).is_pending());
                assert!(poll_with(head.as_mut(), &new).is_pending());
                assert_eq!((Arc::strong_count(&old), Arc::strong_count(&new)), (1, 2));
                drop(open.pop());
                assert_eq!((wakes(&old), wakes(&new)), (0, 1));
                head.await.unwrap();
            })
            .unwrap();
        }

        #[test]
        fn whose_parts_hash_to_another_digest_keeps_a_listed_file() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let (found, other) = chunk(8, 3000);
                let receipt = store.receive(digest, 3000).await.unwrap();
                let mismatch = Error::Mismatch { digest, found };
                assert_eq!(end(receipt, &other).await, Err(mismatch));
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
                assert_no_failure(&store);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Listed);
            })
            .unwrap();
        }

        #[test]
        fn of_a_digest_with_another_receipt_open_writes_a_part_of_its_own() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let (found, other) = chunk(8, 3000);
                let mut good = store.receive(digest, 3000).await.unwrap();
                let mut bad = store.receive(digest, 3000).await.unwrap();
                good.write(block.clone()).await.unwrap();
                bad.write(other).await.unwrap();
                let mismatch = Error::Mismatch { digest, found };
                assert_eq!(bad.end().await, Err(mismatch));
                assert_eq!(good.end().await, Ok(()));
                assert_bytes(&node, digest, &block).await;
                assert_no_failure(&store);
            })
            .unwrap();
        }

        #[test]
        fn whose_parts_hash_to_another_digest_and_whose_remove_fails_gives_mismatch() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let (found, other) = chunk(8, 3000);
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Remove);
                let mismatch = Error::Mismatch { digest, found };
                assert_eq!(end(receipt, &other).await, Err(mismatch));
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Absent);
            })
            .unwrap();
        }

        #[test]
        fn discarded_with_a_failed_remove_leaves_a_part_that_the_next_open_takes() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                receipt.write(block.clone()).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Remove);
                receipt.discard().await;
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Absent);
                drop(store);
                let store = open(&node).await.unwrap();
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Listed);
            })
            .unwrap();
        }

        #[test]
        fn whose_write_fails_and_is_discarded_leaves_no_file() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let mut receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::WriteAt);
                let written = receipt.write(block).await;
                assert_eq!(written, Err(io(&part(digest, 0), Operation::WriteAt)));
                receipt.discard().await;
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, Vec::<PathBuf>::new());
                assert_eq!(store.status().failures, 1);
                let (_, block) = chunk(7, 3000);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Absent);
            })
            .unwrap();
        }

        #[test]
        fn whose_part_create_fails_gives_io_and_keeps_a_listed_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Open);
                let refused = store.receive(digest, 3000).await.err();
                assert_eq!(refused, Some(io(&part(digest, 0), Operation::Open)));
                assert_eq!(store.status().failures, 1);
                assert_eq!(receipts(&store).await.len(), PUTS_MAX);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Listed);
            })
            .unwrap();
        }

        #[test]
        fn whose_part_sync_fails_gives_io_and_keeps_a_listed_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Sync);
                let synced = end(receipt, &block).await;
                assert_eq!(synced, Err(io(&part(digest, 0), Operation::Sync)));
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Listed);
            })
            .unwrap();
        }

        #[test]
        fn whose_remove_of_a_listed_file_fails_gives_io_and_keeps_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&path(digest), Operation::Remove);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(&path(digest), Operation::Remove)));
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Listed);
            })
            .unwrap();
        }

        #[test]
        fn whose_removes_of_a_listed_file_fail_leave_no_part() {
            for len in [0, 3000] {
                let (mut sim, node) = create_default_node(0);
                sim.run_on(&node, move |node, _| async move {
                    let (digest, block) = chunk(7, len);
                    create_file(&node, &path(digest), &block).await;
                    let store = open(&node).await.unwrap();
                    for _ in 0..3 {
                        let receipt = store.receive(digest, len).await.unwrap();
                        node.fail_file(&path(digest), Operation::Remove);
                        let placed = end(receipt, &block).await;
                        assert_eq!(placed, Err(io(&path(digest), Operation::Remove)));
                    }
                    let (other, _) = chunk(8, 100);
                    let _receipt = store.receive(other, 100).await.unwrap();
                    assert_eq!(names(&node).await, sorted(&[digest], &[(other, 3)]));
                    assert_eq!(store.status().failures, 3);
                })
                .unwrap();
            }
        }

        #[test]
        fn of_a_listed_chunk_under_the_floor_while_its_file_is_held_for_write_opens() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                node.files().create_dir(Path::new(DIR)).await.unwrap();
                create_file(&node, &path(digest), &block).await;
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes + 1).await;
                let held = node.files().open(&path(digest), Mode::Write).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                held.close().await;
                end(receipt, &block).await.unwrap();
                assert_eq!(store.status().failures, 0);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        #[test]
        fn of_a_listed_chunk_under_the_floor_with_a_local_put_fails_neither() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let free_bytes = node.files().free().await.unwrap();
                let store = open_leaving(&node, free_bytes + 1).await;
                let received = async {
                    let receipt = store.receive(digest, 3000).await?;
                    end(receipt, &block).await
                };
                let got = Box::pin(join(received, store.put(digest, &block))).await;
                assert_eq!(got, (Ok(()), Ok(())));
                assert_eq!(store.status().failures, 0);
            })
            .unwrap();
        }

        #[test]
        fn of_a_listed_chunk_whose_first_directory_sync_fails_gives_io_and_keeps_it() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(Path::new(DIR), Operation::SyncDir)));
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
                assert_eq!(store.status().failures, 1);
                assert_eq!(&store.get(digest).await.unwrap().unwrap()[..], &block[..]);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Listed);
            })
            .unwrap();
        }

        #[test]
        fn whose_rename_fails_gives_io_and_keeps_the_part_for_the_next_put() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Rename);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(&part(digest, 0), Operation::Rename)));
                assert_eq!(names(&node).await, sorted(&[], &[(digest, 0)]));
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Absent);
                // The put renames the part, so a write would take this fault.
                node.fail_file(&path(digest), Operation::WriteAt);
                store.put(digest, &block).await.unwrap();
                assert_eq!(names(&node).await, sorted(&[digest], &[]));
                assert_bytes(&node, digest, &block).await;
            })
            .unwrap();
        }

        #[test]
        fn whose_rename_fails_and_then_a_put_whose_place_fails_keep_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Rename);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(&part(digest, 0), Operation::Rename)));
                node.fail_file(&part(digest, 0), Operation::Open);
                node.fail_file(&path(digest), Operation::WriteAt);
                let failed = store.put(digest, &block).await;
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
                assert_eq!(failed, Err(io(&part(digest, 0), Operation::Open)));
            })
            .unwrap();
        }

        #[test]
        fn whose_rename_fails_keeps_the_part_for_the_next_open() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Rename);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(&part(digest, 0), Operation::Rename)));
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        #[test]
        fn placed_by_a_check_whose_last_sync_fails_keep_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Rename);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(&part(digest, 0), Operation::Rename)));
                // The check renames the kept part, and its directory sync fails.
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                let (other, _) = chunk(8, 100);
                store.receive(other, 100).await.unwrap().discard().await;
                assert_eq!(names(&node).await, sorted(&[digest], &[]));
                // The file of the digest is now the only copy of the chunk.
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 2), Operation::Rename);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(&part(digest, 2), Operation::Rename)));
                assert_eq!(names(&node).await, sorted(&[], &[(digest, 2)]));
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        #[test]
        fn placed_by_a_put_whose_last_sync_fails_keep_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Rename);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(&part(digest, 0), Operation::Rename)));
                // The put renames the kept part, and its directory sync fails.
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                let failed = store.put(digest, &block).await;
                assert_eq!(failed, Err(io(Path::new(DIR), Operation::SyncDir)));
                assert_eq!(names(&node).await, sorted(&[digest], &[]));
                // The file of the digest is now the only copy of the chunk.
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 1), Operation::Rename);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(&part(digest, 1), Operation::Rename)));
                assert_eq!(names(&node).await, sorted(&[], &[(digest, 1)]));
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        #[test]
        fn whose_rename_fails_after_a_failed_write_of_a_put_keeps_the_chunk() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let (digest, block) = chunk(7, 3000);
                create_file(&node, &path(digest), &block).await;
                let store = open(&node).await.unwrap();
                node.fail_file(&path(digest), Operation::WriteAt);
                let failed = store.put(digest, &block).await;
                assert_eq!(failed, Err(io(&path(digest), Operation::WriteAt)));
                assert_eq!(names(&node).await, sorted(&[digest], &[]));
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(&part(digest, 0), Operation::Rename);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(&part(digest, 0), Operation::Rename)));
                assert_eq!(names(&node).await, sorted(&[], &[(digest, 0)]));
                drop(store);
                let store = open(&node).await.unwrap();
                let got = store.get(digest).await.unwrap();
                assert_eq!(got.as_deref(), Some(&block[..]));
            })
            .unwrap();
        }

        #[test]
        fn whose_directory_sync_fails_gives_io_and_the_next_put_writes_again() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let receipt = store.receive(digest, 3000).await.unwrap();
                node.fail_file(Path::new(DIR), Operation::SyncDir);
                let placed = end(receipt, &block).await;
                assert_eq!(placed, Err(io(Path::new(DIR), Operation::SyncDir)));
                assert_eq!(store.status().failures, 1);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Listed);
                store.put(digest, &block).await.unwrap();
                assert_bytes(&node, digest, &block).await;
            })
            .unwrap();
        }

        #[test]
        fn of_a_digest_that_a_put_stores_in_its_end_keeps_the_put() {
            let (mut sim, node) = create_default_node(0);
            sim.run_on(&node, |node, _| async move {
                let store = open(&node).await.unwrap();
                let (digest, block) = chunk(7, 3000);
                let receipt = store.receive(digest, 3000).await.unwrap();
                let (placed, put) =
                    Box::pin(join(end(receipt, &block), store.put(digest, &block)))
                        .await;
                assert_eq!((placed, put), (Ok(()), Ok(())));
                let listed = node.files().list(Path::new(DIR)).await.unwrap();
                assert_eq!(listed, vec![PathBuf::from(digest.to_string())]);
                assert_bytes(&node, digest, &block).await;
                assert_no_failure(&store);
                assert_eq!(seen(&node, &store, digest, &block).await, Seen::Held);
            })
            .unwrap();
        }

        /// Opens the store after a cut in the ends of
        /// `cut_in_its_end_leaves_each_chunk_whole_or_absent`, of which `ended`
        /// returned, and gives the number of chunks that are absent.
        fn absent_after_cut(
            sim: &mut Sim,
            node: &sim::node::Node,
            seed: u64,
            ended: u64,
        ) -> u64 {
            sim.run_on(node, move |node, _| async move {
                let store = open(&node).await.unwrap();
                let mut absent = 0;
                for index in 1..=8 {
                    let (digest, block) = torn(index);
                    if let Some(got) = store.get(digest).await.unwrap() {
                        assert_eq!(&got[..], &block[..], "seed {seed}");
                    } else {
                        assert!(
                            index > ended && index % 2 == 0,
                            "seed {seed}: chunk {index} lost"
                        );
                        absent += 1;
                    }
                    assert_ne!(
                        seen(&node, &store, digest, &block).await,
                        Seen::Held,
                        "seed {seed}: chunk {index} is held at the open"
                    );
                }
                let listed = node.files().list(Path::new(DIR)).await;
                assert!(
                    listed.unwrap().len() <= 8,
                    "seed {seed}: the open left a part"
                );
                absent
            })
            .unwrap()
        }

        /// The true time between the cuts of two seeds in a row. An end makes more
        /// file calls than a put.
        const END_STEP: i64 = 75_000;

        // The ends of puts of peers, over chunks listed by an earlier open and over
        // new ones, cut at each step: after the sync of a part, after the remove of a
        // listed file, and after a rename.
        #[test]
        fn cut_in_its_end_leaves_each_chunk_whole_or_absent() {
            for crash in [Crash::Power, Crash::Process] {
                let (mut cuts, mut absent_chunks) = (0, 0);
                for seed in 0..64 {
                    let (mut sim, node) = create_default_node(seed);
                    sim.run_on(&node, |node, _| async move {
                        let store = open(&node).await.unwrap();
                        for index in (1..=8).step_by(2) {
                            let (digest, block) = torn(index);
                            store.put(digest, &block).await.unwrap();
                        }
                    })
                    .unwrap();
                    let ended = Arc::new(AtomicU64::new(0));
                    let count = Arc::clone(&ended);
                    let own = node.clone();
                    let handle =
                        node.shards().start(shard("before"), move |_| async move {
                            let store = open(&own).await.unwrap();
                            for index in 1..=8 {
                                let (digest, _) = torn(index);
                                let byte = u8::try_from(index).unwrap();
                                let mut receipt =
                                    store.receive(digest, 3000).await.unwrap();
                                for _ in 0..3 {
                                    receipt.write(chunk(byte, 1000).1).await.unwrap();
                                }
                                receipt.end().await.unwrap();
                                count.store(index, Ordering::Relaxed);
                            }
                            pending::<()>().await;
                        });
                    drop(handle.unwrap());
                    sim.run_for(Span::from_nanos(
                        END_STEP * i64::try_from(seed).unwrap(),
                    ))
                    .unwrap();
                    let ended = ended.load(Ordering::Relaxed);
                    sim.crash(&node, crash);
                    cuts += u64::from(ended > 0 && ended < 8);

                    let absent = absent_after_cut(&mut sim, &node, seed, ended);
                    absent_chunks += u64::from(absent > 0);
                }
                assert!(cuts > 16, "{crash:?}: only {cuts} cuts were between ends");
                assert!(
                    absent_chunks > 4,
                    "{crash:?}: only {absent_chunks} cuts left a new chunk absent"
                );
            }
        }
    }
}
