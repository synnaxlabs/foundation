//! One shard's buffer over its write-ahead ring. [`Buffer::open`] recovers the
//! tails from the records, [`Buffer::append`] queues a batch with no I/O, and a
//! commit task on the shard writes each group and syncs once per deadline.

#![deny(clippy::indexing_slicing, clippy::as_conversions)]

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::mem;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::slice;
use std::task::{Context, Poll, Waker};

use block::{Block, Pool};
use env::clock::Clock;
use env::entropy::Entropy;
use env::files::{self, File, Files, Mode};
use env::tasks::Tasks;
use types::channel::{Slot, Slots};
use types::frame::Path;
use types::time::Span;

use crate::entry::{self, ENTRIES_MAX, Entry};
use crate::group::{Closed, Group, Limit, META_LEN, Rejected, Sealed};
use crate::header::{self, Header};
use crate::record::{self, ALIGN, AREA_START, Body};
use crate::tails::{self, Tail, Tails};
use crate::wal::{self, Cursor, Layout, Step, Unfit, Window, Writer};

/// What one shard's buffer is given at open.
#[derive(Debug)]
pub struct Config {
    /// The file seam. `os` or `sim` implements it.
    pub files: Files,
    /// The directory of this shard's ring, relative to the data directory.
    pub dir: PathBuf,
    /// Blocks for record headers and recovery reads. It needs a class of 64 KiB.
    pub pool: Rc<Pool>,
    /// Deadlines of the group commit.
    pub clock: Clock,
    /// Runs the commit task on this shard.
    pub tasks: Tasks,
    /// The chain value of the restart record.
    pub entropy: Entropy,
    /// The sizes of a new ring. An existing ring keeps the sizes in its header; a
    /// change takes effect at the next create.
    pub layout: Layout,
    /// The longest time an entry waits for its group commit.
    pub commit: Span,
}

/// Why a call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The batch alone is over a limit of one record, so it never fits this ring.
    /// The limit is the first one it is over, in the order of [`Limit`]. Nothing
    /// is queued.
    Large(Limit),
    /// The ring has no room for the batch. The caller records a gap. Room returns
    /// at a commit, or never when the offsets left before their end are under
    /// `needed`.
    Full {
        /// Bytes of the area that the batch's record needs, with the rest of the
        /// area it must skip.
        needed: u64,
        /// Bytes the record may take: the area not in use, or the offsets left
        /// before their end, whichever is less.
        free: u64,
    },
    /// The pool has no block for a record header or a recovery read.
    Pool(block::Error),
    /// A file call failed. After a failed sync, every call fails with it.
    Files(files::Error),
    /// The ring file has another length than its header, or the layout for a new
    /// ring, says.
    Length {
        /// The length the header or the layout says.
        expected: u64,
        /// The length of the file.
        found: u64,
    },
    /// No header block has the magic: the file is not a ring.
    Missing,
    /// Both header blocks have the magic and a wrong CRC, which no crash leaves:
    /// the ring is lost.
    Damaged,
    /// The ring has a format version this build does not read.
    Version(u16),
    /// The header holds sizes that make no ring.
    Unfit(Unfit),
    /// A header block or a record at `offset` passed its CRC but cannot be read:
    /// another version or a defect wrote it. The ring is not written to.
    Invalid {
        /// The offset in the ring file.
        offset: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Large(limit) => write!(f, "{limit}"),
            Self::Full { needed, free } => write!(
                f,
                "the ring has no room for the batch: it needs {needed} bytes and \
                 {free} are free"
            ),
            Self::Pool(error) => write!(f, "the pool has no block: {error}"),
            Self::Files(error) => write!(f, "a file call failed: {error}"),
            Self::Length { expected, found } => write!(
                f,
                "the ring file is {found} bytes long, and its ring is {expected}"
            ),
            Self::Missing => write!(f, "the ring file has no header: it is not a ring"),
            Self::Damaged => {
                write!(f, "the ring is lost: both header blocks have a wrong CRC")
            }
            Self::Version(version) => write!(
                f,
                "the ring has format version {version}, which this build does not \
                 read"
            ),
            Self::Unfit(unfit) => write!(
                f,
                "the ring header holds an area of {} bytes and a body of at most {} \
                 bytes, which make no ring",
                unfit.area, unfit.body_max
            ),
            Self::Invalid { offset } => write!(
                f,
                "the ring holds bytes at {offset} that this build cannot read"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<block::Error> for Error {
    fn from(error: block::Error) -> Self {
        Self::Pool(error)
    }
}

impl From<files::Error> for Error {
    fn from(error: files::Error) -> Self {
        Self::Files(error)
    }
}

impl From<wal::Full> for Error {
    fn from(full: wal::Full) -> Self {
        Self::Full {
            needed: full.needed,
            free: full.free,
        }
    }
}

impl From<wal::Invalid> for Error {
    fn from(invalid: wal::Invalid) -> Self {
        Self::Invalid {
            offset: invalid.offset,
        }
    }
}

impl From<header::Error> for Error {
    fn from(error: header::Error) -> Self {
        match error {
            header::Error::Missing => Self::Missing,
            header::Error::Damaged => Self::Damaged,
            header::Error::Version(version) => Self::Version(version),
            header::Error::Unfit(unfit) => Self::Unfit(unfit),
            header::Error::Unaligned(_) => Self::Invalid { offset: 0 },
        }
    }
}

/// One shard's logs. It lives on its shard: the commit task runs on the shard's
/// `tasks`. The task idles while nothing is queued and nothing waits on a commit.
/// A drop ends the task at once when it idles, else at its next deadline. Entries
/// queued and not yet committed at the drop are not written.
#[derive(Debug)]
pub struct Buffer {
    shared: Rc<Shared>,
    /// The entries of the append in progress, kept with capacity for up to one
    /// record of entries.
    batch: RefCell<Vec<Entry>>,
}

/// What the handle and the commit task share.
#[derive(Debug)]
struct Shared {
    file: File,
    pool: Rc<Pool>,
    layout: Layout,
    state: RefCell<State>,
}

#[derive(Debug)]
struct State {
    writer: Writer,
    /// The group that takes the next batch.
    open: Group,
    /// Cleared groups with their capacity, for the next open group.
    spares: Vec<Group>,
    /// Closed groups the task writes at its next deadline.
    queue: Vec<Closed>,
    /// With every appended entry.
    tails: Tails,
    /// With every synced entry.
    durable: Tails,
    /// How many deadlines took the groups to write.
    taken: u64,
    /// How many deadlines synced what they took.
    commits: u64,
    wakers: Vec<Waker>,
    /// The task, while it idles. Whoever ends the idle span takes it and wakes it.
    parked: Option<Waker>,
    /// Whether the handle dropped. The task ends when it next decides.
    closed: bool,
    /// The error that ended the task.
    failed: Option<Error>,
}

impl State {
    /// Whether nothing is queued and nothing waits on a commit.
    fn idle(&self) -> bool {
        self.open.is_empty() && self.queue.is_empty() && self.wakers.is_empty()
    }

    /// Closes the open group into the queue and opens a spare.
    fn close_open(&mut self) {
        let spare = self.spares.pop().unwrap_or_default();
        let full = mem::replace(&mut self.open, spare);
        self.queue.push(full.close(&mut self.writer));
    }

    /// Moves the durable tails past the synced `sealed` groups and keeps their
    /// records as spares.
    fn synced(&mut self, sealed: impl Iterator<Item = Sealed>) {
        for record in sealed {
            for (&slot, header) in record.slots().iter().zip(record.headers()) {
                self.durable
                    .advance(slot, header)
                    .expect("invariant: append checked the entry");
            }
            self.spares.push(record.clear());
        }
        self.commits += 1;
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let mut state = self.shared.state.borrow_mut();
        state.closed = true;
        let parked = state.parked.take();
        drop(state);
        if let Some(waker) = parked {
            waker.wake();
        }
    }
}

impl Buffer {
    /// Opens the ring in `config.dir`, or creates it, and recovers the tail of
    /// every path from its records. Each recovered index gets its slot from `slots`.
    /// Starts the commit task.
    ///
    /// # Errors
    ///
    /// [`Error::Files`], [`Error::Pool`], [`Error::Length`], [`Error::Missing`],
    /// [`Error::Damaged`], [`Error::Version`], [`Error::Unfit`], and
    /// [`Error::Invalid`] as each says. [`Error::Full`] when the ring has no block
    /// for its restart record.
    ///
    /// # Panics
    ///
    /// When a record window of the layout does not fit in memory: `body_max` is at
    /// most `u32::MAX`, so only on a target under 64 bits.
    pub async fn open(config: Config, slots: &mut Slots) -> Result<Self, Error> {
        let Config {
            files,
            dir,
            pool,
            clock,
            tasks,
            entropy,
            layout,
            commit,
        } = config;
        drop(pool.alloc(META_LEN)?);
        let path = dir.join("ring");
        let file = match files.open(&path, Mode::Write).await {
            Ok(file) => file,
            Err(files::Error::NotFound { .. }) => {
                files.create_dir(&dir).await?;
                let len = layout.file_len();
                let file = files.open(&path, Mode::Create { len }).await?;
                files.sync_dir(&dir).await?;
                file
            }
            Err(error) => return Err(error.into()),
        };
        let header = read_header(&file, &pool, &entropy, layout).await?;
        let mut cursor = Cursor::new(header.layout, header.tail, pool.largest());
        let mut tails = Tails::default();
        loop {
            let Window { place, len } = cursor.window();
            let bytes = file.read_at(AREA_START + place, pool.alloc(len)?).await?;
            let offset = cursor.offset();
            match cursor.next(&bytes)? {
                Step::Data(body) => recover(body, offset, slots, &mut tails)?,
                Step::Moved | Step::More => {}
                Step::End => break,
            }
        }
        let chain = random(&entropy);
        let (writer, sealed) = cursor.writer(header.tail.offset(), chain)?;
        write_restart(&file, &pool, sealed, chain).await?;
        let shared = Rc::new(Shared {
            file,
            pool,
            layout: header.layout,
            state: RefCell::new(State {
                writer,
                open: Group::default(),
                spares: Vec::new(),
                queue: Vec::new(),
                durable: tails.clone(),
                tails,
                taken: 0,
                commits: 0,
                wakers: Vec::new(),
                parked: None,
                closed: false,
                failed: None,
            }),
        });
        tasks.spawn(run(Rc::clone(&shared), clock, commit, chain));
        Ok(Self {
            shared,
            batch: RefCell::default(),
        })
    }

    /// The sizes of the ring. A commit holds at most `body_max` bytes.
    #[must_use]
    pub fn layout(&self) -> Layout {
        self.shared.layout
    }

    /// Where `path` of the index at `slot` stands, with every appended entry.
    #[must_use]
    pub fn tail(&self, slot: Slot, path: Path) -> Tail {
        self.shared.state.borrow().tails.get(slot, path)
    }

    /// Where `path` of the index at `slot` stands on disk.
    #[must_use]
    pub fn durable(&self, slot: Slot, path: Path) -> Tail {
        self.shared.state.borrow().durable.get(slot, path)
    }

    /// Queues every entry of `entries` for the next group commit, or none, and
    /// returns at once with no I/O. The entries are durable when a later
    /// [`committed`](Self::committed) resolves. They go in one record, in order. A
    /// `first` past the tail is a skip ahead; `read` reports the range as a gap.
    ///
    /// The buffer keeps capacity for the largest batch it took and for every slot
    /// it saw, so after those first calls `append` makes no heap allocation.
    ///
    /// # Errors
    ///
    /// [`Error::Large`] when no record holds the entries together, [`Error::Full`]
    /// when the ring has no room for the whole call, and [`Error::Pool`] when the
    /// pool has no block for the record header; nothing is queued and no tail
    /// moves. [`Error::Files`] after a failed sync. An append that fails takes no
    /// part: the parts of `entries` are dropped.
    ///
    /// # Panics
    ///
    /// When a `first` is below the tail of its path, with the index, the path,
    /// `first`, and the tail, or when `first + len` passes `u64::MAX`.
    pub fn append(
        &self,
        entries: impl IntoIterator<Item = Entry>,
    ) -> Result<(), Error> {
        let mut batch = self.batch.take();
        batch.extend(entries);
        let queued = self.queue(&mut batch);
        batch.clear();
        batch.shrink_to(ENTRIES_MAX);
        self.batch.replace(batch);
        queued
    }

    /// Takes `batch` into a group, or leaves its entries in it.
    fn queue(&self, batch: &mut Vec<Entry>) -> Result<(), Error> {
        let shared = &*self.shared;
        let mut guard = shared.state.borrow_mut();
        let state = &mut *guard;
        if let Some(error) = &state.failed {
            return Err(error.clone());
        }
        let taken = match state.open.push(&shared.pool, &state.writer, batch) {
            Ok(taken) => taken,
            Err(Rejected::Record) => {
                state.close_open();
                state
                    .open
                    .push(&shared.pool, &state.writer, batch)
                    .map_err(rejected)?
            }
            Err(other) => return Err(rejected(other)),
        };
        for (slot, header) in state.open.entries(taken) {
            state
                .tails
                .advance(slot, header)
                .unwrap_or_else(|invalid| panic!("invariant: {invalid}"));
        }
        let parked = state.parked.take();
        drop(guard);
        if let Some(waker) = parked {
            waker.wake();
        }
        Ok(())
    }

    /// Resolves at the end of the next group commit, when every entry appended
    /// before the call is durable, or with the error that ended the buffer.
    #[must_use]
    pub fn committed(&self) -> Commit<'_> {
        Commit {
            shared: &self.shared,
            since: self.shared.state.borrow().taken,
        }
    }
}

/// The error of a push that a new group refuses too.
fn rejected(rejected: Rejected) -> Error {
    match rejected {
        Rejected::Large(limit) => Error::Large(limit),
        Rejected::Ring(full) => full.into(),
        Rejected::Pool(error) => error.into(),
        Rejected::Record => {
            unreachable!("invariant: an empty group takes a batch under the maximum")
        }
    }
}

/// A random chain value.
fn random(entropy: &Entropy) -> u32 {
    let mut bytes = [0; 4];
    entropy.fill(&mut bytes);
    u32::from_le_bytes(bytes)
}

/// Reads the newer checkpoint. Two zero blocks are a ring made and not yet
/// written: the first checkpoint goes to both blocks.
async fn read_header(
    file: &File,
    pool: &Pool,
    entropy: &Entropy,
    layout: Layout,
) -> Result<Header, Error> {
    let found = file.len();
    let length = |layout: Layout| Error::Length {
        expected: layout.file_len(),
        found,
    };
    if found < AREA_START {
        return Err(length(layout));
    }
    let blocks = file.read_at(0, pool.alloc(2 * ALIGN)?).await?;
    let (first, rest) = blocks
        .split_first_chunk::<ALIGN>()
        .expect("invariant: the read gave two blocks");
    let second = rest
        .first_chunk::<ALIGN>()
        .expect("invariant: the read gave two blocks");
    if blocks.iter().any(|&byte| byte != 0) {
        let header = Header::decode(first, second)?;
        if found != header.layout.file_len() {
            return Err(length(header.layout));
        }
        return Ok(header);
    }
    if found != layout.file_len() {
        return Err(length(layout));
    }
    let header = Header::new(layout, random(entropy));
    let mut block = pool.alloc(ALIGN)?;
    block.copy_from_slice(&header.encode());
    let block = block.freeze();
    file.write_at(0, &[block.clone(), block]).await?;
    file.sync().await?;
    Ok(header)
}

/// Advances the tails past the entries of a record body at `offset`.
fn recover(
    body: Body<'_>,
    offset: u64,
    slots: &mut Slots,
    tails: &mut Tails,
) -> Result<(), Error> {
    let unread = |_: entry::Invalid| Error::Invalid { offset };
    let misplaced = |_: tails::Invalid| Error::Invalid { offset };
    for header in entry::parse(body.start, body.len).map_err(unread)? {
        let header = header.map_err(unread)?;
        tails
            .advance(slots.assign(header.index), &header)
            .map_err(misplaced)?;
    }
    Ok(())
}

/// Writes the restart record `sealed`, whose body is `chain`.
async fn write_restart(
    file: &File,
    pool: &Pool,
    sealed: wal::Sealed,
    chain: u32,
) -> Result<(), Error> {
    assert!(
        sealed.wrap.is_none(),
        "invariant: a restart record is one block and never wraps"
    );
    let block = small_record(pool, &sealed.record.header, &chain.to_le_bytes())?;
    file.write_at(AREA_START + sealed.record.place, slice::from_ref(&block))
        .await?;
    Ok(())
}

/// A record with a short body, at the end of one block of the smallest class.
fn small_record(
    pool: &Pool,
    header: &[u8; record::HEADER_LEN],
    body: &[u8],
) -> Result<Block, block::Error> {
    let mut block = pool.alloc(block::ALIGN)?;
    let start = block
        .len()
        .checked_sub(header.len() + body.len())
        .expect("invariant: a small record fits one block");
    let (_, rest) = block.split_at_mut(start);
    let (first, second) = rest.split_at_mut(header.len());
    first.copy_from_slice(header);
    second.copy_from_slice(body);
    Ok(block.freeze().skip(start))
}

/// The commit task. It parks while the state idles; a push, a `Commit` poll, or
/// the drop wakes it. Each deadline takes the closed groups and the open one,
/// seals them in order from `chain`, the value of the restart record, writes
/// them, syncs once, and wakes the waiters. A failed file call or the drop ends
/// the task.
async fn run(shared: Rc<Shared>, clock: Clock, commit: Span, chain: u32) {
    let mut chain = chain;
    let mut taken: Vec<Closed> = Vec::new();
    let mut sealed: Vec<Sealed> = Vec::new();
    let mut woken: Vec<Waker> = Vec::new();
    let mut sleep = clock.sleep(commit);
    loop {
        let ended = poll_fn(|cx| {
            let mut state = shared.state.borrow_mut();
            if state.closed {
                return Poll::Ready(true);
            }
            if !state.idle() {
                return Poll::Ready(false);
            }
            state.parked = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        if ended {
            return;
        }
        if sleep.deadline() < clock.now() {
            sleep.reset(clock.now() + commit);
        }
        (&mut sleep).await;
        sleep.reset(clock.now() + commit);
        {
            let mut state = shared.state.borrow_mut();
            if state.closed {
                return;
            }
            state.taken += 1;
            if !state.open.is_empty() {
                state.close_open();
            }
            mem::swap(&mut state.queue, &mut taken);
        }
        for closed in taken.drain(..) {
            let (record, next) = closed.seal(chain);
            chain = next;
            sealed.push(record);
        }
        let result = write(&shared, &sealed).await;
        let failed = result.is_err();
        let mut state = shared.state.borrow_mut();
        match result {
            Ok(()) => state.synced(sealed.drain(..)),
            Err(error) => state.failed = Some(error.into()),
        }
        woken.append(&mut state.wakers);
        drop(state);
        for waker in woken.drain(..) {
            waker.wake();
        }
        if failed {
            return;
        }
    }
}

/// Writes each sealed group's records and syncs once.
async fn write(shared: &Shared, sealed: &[Sealed]) -> Result<(), files::Error> {
    if sealed.is_empty() {
        return Ok(());
    }
    for record in sealed {
        for (place, blocks) in record.writes() {
            shared.file.write_at(AREA_START + place, blocks).await?;
        }
    }
    shared.file.sync().await
}

/// The future of [`Buffer::committed`].
#[derive(Debug)]
pub struct Commit<'a> {
    shared: &'a Shared,
    since: u64,
}

impl Future for Commit<'_> {
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.shared.state.borrow_mut();
        if let Some(error) = &state.failed {
            return Poll::Ready(Err(error.clone()));
        }
        if state.commits > self.since {
            return Poll::Ready(Ok(()));
        }
        if !state.wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
            state.wakers.push(cx.waker().clone());
        }
        let parked = state.parked.take();
        drop(state);
        if let Some(waker) = parked {
            waker.wake();
        }
        Poll::Pending
    }
}
