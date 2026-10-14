//! One shard's buffer over its write-ahead ring. [`Buffer::open`] recovers the
//! tails from the records, [`Buffer::append`] queues a batch with no I/O, and a
//! commit task on the shard writes each group and syncs once per deadline.

#![deny(clippy::indexing_slicing, clippy::as_conversions)]

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::mem;
use std::num::NonZeroU8;
use std::path::{self, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::slice;
use std::task::{Context, Poll, Waker};

use block::{Block, Pool, Unique};
use env::clock::Clock;
use env::entropy::Entropy;
use env::files::{self, File, Files, Mode};
use env::tasks::Tasks;
use types::channel::{self, Slot, Slots};
use types::frame::Path;
use types::hash;
use types::time::Span;
use types::wait;

use crate::carry;
use crate::entry::{self, ENTRIES_MAX, Entry};
use crate::group::{self, Closed, Group, Sealed};
use crate::header::{self, Header};
use crate::log::{self, Carried, Found, Logs, Mark, Tail};
use crate::read::{self, Read, Reading, Stored};
use crate::record::{self, ALIGN, AREA_START, Body, HEADER_LEN, Kind};
use crate::wal::{
    self, Cursor, Ends, Layout, Limit, Plan, Position, Step, Unfit, Window, Writer,
};

/// How many reads at once hold records with no allocation.
const HOLDS: usize = 16;

/// Whether each commit trims the ring and writes carry records.
const TRIM: bool = cfg!(feature = "trim");

/// What one shard's buffer is given at open.
#[derive(Debug)]
pub struct Config {
    /// The file seam. `os` or `sim` implements it.
    pub files: Files,
    /// The directory of this shard's ring, relative to the data directory. Its parent
    /// must be there and durable.
    pub dir: PathBuf,
    /// Blocks for record headers, recovery reads, and the entries a read gives. It
    /// needs a class of 64 KiB. Its budget bounds an entry, with
    /// [`Buffer::block_max`].
    pub pool: Rc<Pool>,
    /// Deadlines of the group commit.
    pub clock: Clock,
    /// Runs the commit task on this shard.
    pub tasks: Tasks,
    /// The chain value of the restart record.
    pub entropy: Entropy,
    /// The sizes of a new ring. A ring with a checkpoint keeps the sizes in its
    /// header. A ring with none, which a crash before the first checkpoint leaves,
    /// takes these.
    pub layout: Layout,
    /// The longest time an entry waits for its group commit to start. An entry
    /// queued during a commit longer than `commit` waits until that commit ends.
    pub commit: Span,
}

/// Why an open or a read failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The ring has no room for its restart record.
    Full {
        /// Bytes of the area that the restart record needs, with the rest of the
        /// area it must skip.
        needed: u64,
        /// Bytes the record may take: the area not in use, or the offsets left
        /// before their end, whichever is less.
        free: u64,
    },
    /// The pool has no block for a header, a recovery read, the restart record, a
    /// recovered entry, or a read.
    Pool(block::Error),
    /// The budget of the pool holds no entry of [`Layout::ENTRY_MAX_MIN`] bytes beside
    /// the blocks of a group. A pool of `min` bytes or more opens the ring.
    Budget {
        /// The budget of the pool.
        budget: usize,
        /// The least budget that opens the ring.
        min: usize,
    },
    /// A file call failed.
    Files(files::Error),
    /// The ring file has another length than its header says, or it is not empty and
    /// ends inside its header blocks.
    Length {
        /// The length the header says, or the length of a new ring.
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
    /// A header block passed its CRC but holds a tail off a block boundary: another
    /// version or a defect wrote it. The ring reads as it did before the open.
    Unaligned {
        /// The tail offset that the header block holds.
        tail: u64,
    },
    /// A record at `offset` passed its CRC but cannot be read: another version or a
    /// defect wrote it. The ring reads as it did before the open.
    Invalid {
        /// The offset of the record in the ring.
        offset: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full { needed, free } => write!(
                f,
                "the ring has no room for its restart record: it needs {needed} \
                 bytes and {free} are free"
            ),
            Self::Pool(error) => write!(f, "the pool has no block: {error}"),
            Self::Budget { budget, min } => write!(
                f,
                "a pool of {budget} bytes holds no entry of {} bytes: give it {min} \
                 bytes or more",
                Layout::ENTRY_MAX_MIN
            ),
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
            Self::Unaligned { tail } => write!(
                f,
                "the ring header holds a tail at {tail}, which is not on a block \
                 boundary"
            ),
            Self::Invalid { offset } => write!(
                f,
                "the ring holds bytes at {offset} that this build cannot read"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Why an append took no entry. Nothing is queued, no tail moves, and the parts of
/// the entries are dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// The batch alone is over a [`Limit`], so this buffer never takes it. The
    /// limit is the first one it is over, in the order of [`Limit`].
    Large(Limit),
    /// The ring has no room for the batch: one block stays free for the carry record
    /// of the next commit, and one for the restart record of the next open. Room
    /// returns only when records leave the ring at its tail, and never when the
    /// offsets left before their end are under `needed`. Nothing moves records out
    /// of the ring yet.
    Full {
        /// Bytes of the area that the batch's record needs, with the rest of the
        /// area it must skip.
        needed: u64,
        /// Bytes the record may take: the area not in use, or the offsets left
        /// before their end, whichever is less, less the two blocks that stay free.
        free: u64,
    },
    /// The pool has no block for the record header.
    Pool(block::Error),
    /// A file call of a commit failed, which ended the buffer.
    Files(files::Error),
}

impl fmt::Display for Rejected {
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
        }
    }
}

impl std::error::Error for Rejected {}

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
            header::Error::Unaligned(unaligned) => Self::Unaligned {
                tail: unaligned.offset,
            },
        }
    }
}

/// One shard's logs. It lives on its shard: the commit task runs on the shard's
/// `tasks`. The task idles while nothing is queued and no commit runs. A drop ends
/// the task at once when it idles, else at the end of its last commit, which writes
/// each entry queued at the drop, or earlier at the first file call that fails.
/// Await an [`End`] past the drop, then drop it, before a reopen and before the shard
/// ends, which cancels the task.
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
    block_max: usize,
    layout: Layout,
    state: RefCell<State>,
}

impl Shared {
    /// Ends the task's idle span. The caller holds no borrow of `state`.
    fn unpark(&self) {
        let parked = self.state.borrow_mut().parked.0.take();
        if let Some(waker) = parked {
            waker.wake();
        }
    }
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
    /// Where each path stands, and the records that hold it.
    logs: Logs,
    /// The last checkpoint written.
    header: Header,
    /// The tails of the carry record of the commit in flight.
    carried: Carried,
    /// Whether an append got [`Rejected::Full`] since the last deadline. The task
    /// then commits also with no entry, so that its trim and carry can free room.
    starved: bool,
    /// The record that each read in progress holds, in no order.
    held: Vec<u64>,
    /// How many deadlines took the groups to write.
    taken: u64,
    /// How many deadlines ended with no error.
    commits: u64,
    /// The waiting [`Commit`]s, which the end of each commit and of the task wakes.
    committing: wait::Set,
    /// The waiting [`End`]s. Only the end of the task wakes them.
    ending: wait::Set,
    /// The key of the next [`Commit`] or [`End`] in its set.
    next_key: u64,
    /// The task, while it idles. Whoever ends the idle span takes it and wakes it.
    parked: Parked,
    /// Whether the handle dropped. The task ends when it next idles.
    closed: bool,
    /// Whether the task ended. An [`End`], and a [`Commit`] held past the drop, wait
    /// for it.
    ended: bool,
    /// The error that ended the task.
    failed: Option<files::Error>,
}

/// The task's waker, while it idles.
struct Parked(Option<Waker>);

/// Prints whether the task idles in place of its waker, whose `Debug` prints pointers.
impl fmt::Debug for Parked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.is_some().fmt(f)
    }
}

impl State {
    /// How the task ended, once it ended: with the error of a failed file call, else
    /// with `Ok`.
    fn outcome(&self) -> Option<Result<(), files::Error>> {
        self.ended.then(|| self.failed.clone().map_or(Ok(()), Err))
    }

    /// Whether nothing is queued.
    fn idle(&self) -> bool {
        self.open.is_empty() && self.queue.is_empty()
    }

    /// The `commits` count at which every entry appended so far is durable. `taken`
    /// counts a commit in flight, and a queued entry needs the next one.
    fn durable_at(&self) -> u64 {
        if self.idle() {
            self.taken
        } else {
            self.taken + 1
        }
    }

    /// Marks the task ended and moves each waiter into `woken`.
    fn end(&mut self, woken: &mut Vec<Waker>) {
        self.ended = true;
        self.committing.drain(woken);
        self.ending.drain(woken);
    }

    /// The oldest record that a trim must keep: the oldest that holds the durable
    /// tail of a path or that a read in progress holds. `None` when no record does.
    fn kept(&mut self) -> Option<u64> {
        let held = self.held.iter().min().copied();
        self.logs.kept().into_iter().chain(held).min()
    }

    /// The key of the next [`Commit`] or [`End`].
    fn key(&mut self) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        key
    }

    /// Closes the open group into the queue and opens a spare.
    fn close_open(&mut self) {
        let spare = self.spares.pop().unwrap_or_default();
        let full = mem::replace(&mut self.open, spare);
        self.queue.push(full.close(&mut self.writer));
    }

    /// Moves the tail past the trimmed records and the durable tails past the
    /// synced `sealed` groups and carry record, which is at the offset of `carry`
    /// with its boundaries. Keeps the records as spares, and counts a commit with
    /// groups.
    fn synced(
        &mut self,
        sealed: &mut Vec<Sealed>,
        carry: Option<(u64, Ends)>,
        tail: Option<Position>,
    ) {
        let counted = !sealed.is_empty();
        let ends = sealed.iter().map(Sealed::ends);
        self.writer
            .committed(ends.chain(carry.map(|(_, ends)| ends)), tail);
        for record in sealed.drain(..) {
            for (&slot, header) in record.slots().iter().zip(record.headers()) {
                self.logs
                    .sync(slot, header, record.offset())
                    .expect("invariant: append checked the entry");
            }
            self.spares.push(record.clear());
        }
        if let Some((offset, _)) = carry {
            self.logs.carried(&self.carried, offset);
            self.carried.clear();
        }
        if counted {
            self.commits += 1;
        }
    }

    /// Takes the groups of a commit into `taken`, and gives its trim and carry
    /// record.
    fn take(&mut self, taken: &mut Vec<Closed>, pool: &Pool) -> Trim {
        self.starved = false;
        if !self.open.is_empty() {
            self.close_open();
        }
        mem::swap(&mut self.queue, taken);
        if !taken.is_empty() {
            self.taken += 1;
        }
        self.trim(pool)
    }

    /// The trim and the carry record of the commit whose groups the caller just
    /// took, in the same borrow. The trim needs a pool block for its checkpoint, and
    /// the carry record needs blocks for its body and a wrap. Without them, the
    /// commit does not trim, or carries at a later deadline. Gives nothing when
    /// [`TRIM`] is off.
    fn trim(&mut self, pool: &Pool) -> Trim {
        if !TRIM {
            return Trim::default();
        }
        let tail = self.trimmed();
        let checkpoint = tail.zip(pool.alloc(ALIGN).ok()).map(|(tail, mut block)| {
            self.logs.hide(tail.offset());
            self.header = self.header.next(tail);
            block.copy_from_slice(&self.header.encode());
            Checkpoint {
                tail,
                place: self.header.place(),
                block: block.freeze(),
            }
        });
        Trim {
            checkpoint,
            carry: self.carry(pool),
        }
    }

    /// The tail of a trim that keeps the oldest record that a path or a read holds,
    /// and lands out of each marked range.
    fn trimmed(&mut self) -> Option<wal::Position> {
        let kept = self.kept().unwrap_or(u64::MAX);
        let logs = &self.logs;
        self.writer
            .trimmed(|offset| logs.marked().land(offset.min(kept)))
    }

    /// Plans the carry record of the paths whose holder is before the reach of the
    /// next trim, as many as one record holds. `None` when no path needs one, or
    /// the pool has no blocks for it: the next deadline then scans the same paths.
    fn carry(&mut self, pool: &Pool) -> Option<Carry> {
        let largest = pool.largest().saturating_sub(HEADER_LEN);
        let limit = self.writer.tails().min(carry::count(largest));
        if limit == 0 {
            return None;
        }
        self.logs
            .idle(self.writer.reach(), limit, &mut self.carried);
        let count = self.carried.tails().len();
        if count == 0 {
            return None;
        }
        let len = HEADER_LEN + carry::body_len(count);
        let (Ok(mut block), Ok(wrap)) = (pool.alloc(len), pool.alloc(ALIGN)) else {
            self.logs.dropped(&mut self.carried);
            return None;
        };
        let plan = self
            .writer
            .carry(count)
            .expect("invariant: `tails` gave the room of the record");
        let (_, body) = block.split_at_mut(HEADER_LEN);
        carry::write(self.carried.tails(), body);
        Some(Carry { plan, block, wrap })
    }
}

/// What a commit writes beside its groups.
#[derive(Debug, Default)]
struct Trim {
    /// The checkpoint of the new tail, when the commit trims.
    checkpoint: Option<Checkpoint>,
    /// The carry record, when the commit carries.
    carry: Option<Carry>,
}

/// The checkpoint block of a trim.
#[derive(Debug)]
struct Checkpoint {
    /// The new tail.
    tail: Position,
    /// The offset of the block in the ring file.
    place: u64,
    block: Block,
}

/// A planned carry record whose body is in `block`, after room for its header.
#[derive(Debug)]
struct Carry {
    plan: Plan,
    block: Unique,
    /// The block for the wrap header, when the record wraps.
    wrap: Unique,
}

impl Carry {
    /// Makes the headers from `chain`, the chain value of the record before.
    fn seal(self, chain: u32) -> Carrying {
        let Self {
            plan,
            mut block,
            wrap,
        } = self;
        let (header, body) = block.split_at_mut(HEADER_LEN);
        let (sealed, ends) = plan.seal(chain, Kind::Carry, [&*body]);
        header.copy_from_slice(&sealed.record.header);
        let wrap = sealed
            .wrap
            .map(|write| (write.place, end_block(wrap, &write.header)));
        Carrying {
            wrap,
            place: plan.place,
            block: block.freeze(),
            offset: plan.offset,
            ends,
        }
    }
}

/// A sealed carry record.
#[derive(Debug)]
struct Carrying {
    /// The place and block of the wrap record, when the record wraps.
    wrap: Option<(u64, Block)>,
    place: u64,
    block: Block,
    offset: u64,
    ends: Ends,
}

/// `header` at the end of `block`, as a record of no body.
fn end_block(mut block: Unique, header: &[u8; HEADER_LEN]) -> Block {
    let start = block
        .len()
        .checked_sub(HEADER_LEN)
        .expect("invariant: a block holds a record header");
    let (_, end) = block.split_at_mut(start);
    end.copy_from_slice(header);
    block.freeze().skip(start)
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.shared.state.borrow_mut().closed = true;
        self.shared.unpark();
    }
}

impl Buffer {
    /// Opens the ring in `config.dir`, or creates it, makes the ring and its directory
    /// durable, and recovers the tail of every path from its records. A ring file with
    /// no checkpoint holds no record: the open makes it again. Each recovered index
    /// gets its slot from `slots`. Starts the commit task. Each tail it reports is
    /// durable. It reads the header and the records from the ring's tail and writes
    /// them again, so its time grows with the records. Open one directory at most one
    /// time at once. Opens at once can fail with `Busy` or `Files(Length)`.
    ///
    /// # Errors
    ///
    /// [`Error::Files`], [`Error::Pool`], [`Error::Length`], [`Error::Missing`],
    /// [`Error::Damaged`], [`Error::Version`], [`Error::Unfit`], [`Error::Unaligned`],
    /// and [`Error::Invalid`] as each says. [`Error::Budget`] when the budget of `pool`
    /// holds no entry of [`Layout::ENTRY_MAX_MIN`] bytes beside the blocks of a group.
    /// No file changes. [`Error::Full`] when the ring has no block for its restart
    /// record. [`Error::Pool`] with `TooLarge` when a recovered entry is over the
    /// largest block of `pool`, which a read must give it in: a larger pool must open
    /// the ring.
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
        let block_max = group::block_max(&pool).map_err(|min| Error::Budget {
            budget: pool.budget(),
            min,
        })?;
        let (file, header) = open_ring(&files, &dir, &pool, &entropy, layout).await?;
        let (cursor, logs) = walk(&file, &pool, &header, slots).await?;
        let chain = random(&entropy);
        let (writer, sealed) = cursor.writer(header.tail.offset(), chain)?;
        write_restart(&file, &pool, sealed, chain).await?;
        file.sync().await?;
        let shared = Rc::new(Shared {
            file,
            pool,
            block_max,
            layout: header.layout,
            state: RefCell::new(State {
                writer,
                open: Group::default(),
                spares: Vec::new(),
                queue: Vec::new(),
                logs,
                header,
                carried: Carried::default(),
                starved: false,
                held: Vec::with_capacity(HOLDS),
                taken: 0,
                commits: 0,
                committing: wait::Set::new(),
                ending: wait::Set::new(),
                next_key: 0,
                parked: Parked(None),
                closed: false,
                ended: false,
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

    /// The block pool from [`Config::pool`]. Its budget bounds an entry, with
    /// [`Buffer::block_max`].
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.shared.pool
    }

    /// The most bytes of parts, joined, that one entry may have ([`Limit::Block`]). The
    /// pool has room for an entry at or under it, in one part or two, when it commits
    /// only the bytes of that entry's parts. It is at most the largest block of
    /// [`Buffer::pool`]. It is at least [`Layout::ENTRY_MAX_MIN`].
    #[must_use]
    pub fn block_max(&self) -> usize {
        self.shared.block_max
    }

    /// Where `path` of the index at `slot` stands, with every appended entry.
    #[must_use]
    pub fn tail(&self, slot: Slot, path: Path) -> Tail {
        self.shared.state.borrow().logs.appended(slot, path)
    }

    /// Where `path` of the index at `slot` stands on disk.
    #[must_use]
    pub fn durable(&self, slot: Slot, path: Path) -> Tail {
        self.shared.state.borrow().logs.durable(slot, path)
    }

    /// The durable entries of `path` of the index at `slot` from `from`, in order,
    /// until their blocks take `budget` pool bytes, by [`block::footprint`] of each,
    /// seqs that the path no longer has start, or the pool has no block for the next
    /// entry. The last entry may pass the budget. An entry that holds `from` comes
    /// whole. A read from a mark at or in seqs that the path no longer has reports them
    /// as `gap` and goes on after them. When the path holds no entry after them, the
    /// read gives the gap and no entry. The first read starts at `Mark::at(0)`; each
    /// read continues at `next`, which a read that gives no entry and no gap does not
    /// move. A read makes one file read for the table of each record it visits, two
    /// when the record header and table pass 4 KiB, and one per entry with bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when a ring read fails and [`Error::Pool`] when the pool
    /// has no block for the first entry or its table; the buffer goes on. When a
    /// commit's file call fails before the read ends, the error that ended the
    /// buffer.
    pub async fn read(
        &self,
        slot: Slot,
        path: Path,
        from: Mark,
        budget: usize,
    ) -> Result<Read, Error> {
        let Shared {
            file, pool, layout, ..
        } = &*self.shared;
        let mut reading = Reading::new(file, pool, *layout, path, from, budget);
        let walked = self.walk(&mut reading, slot, path).await;
        // After the walk: a ring read after a failed sync gives `Poisoned`.
        if let Some(failed) = &self.shared.state.borrow().failed {
            return Err(Error::Files(failed.clone()));
        }
        walked.map(|()| reading.finish())
    }

    /// The newest durable entry with `tag` on `path` of each index that has one, with
    /// the index's slot, in no order. It skips records that a trim hid, and a trim
    /// frees no record that the call reads. It reads only the table of each record
    /// that holds an entry it gives, once, then the bytes of each entry it gives.
    ///
    /// # Errors
    ///
    /// [`Error::Files`] when a ring read fails and [`Error::Pool`] when the pool has
    /// no block for a table or an entry; the buffer goes on. When a commit's file call
    /// fails before the call ends, the error that ended the buffer.
    pub async fn newest(
        &self,
        path: Path,
        tag: NonZeroU8,
    ) -> Result<Vec<(Slot, Stored)>, Error> {
        let found = self.search(path, tag).await;
        // After the reads: a ring read after a failed sync gives `Poisoned`.
        if let Some(failed) = &self.shared.state.borrow().failed {
            return Err(Error::Files(failed.clone()));
        }
        found
    }

    /// The newest durable entry with `tag` on `path` of each index, as
    /// [`newest`](Self::newest) gives, with the error of the first failed read. It
    /// takes the records when called, so a record made during the call is not read.
    async fn search(
        &self,
        path: Path,
        tag: NonZeroU8,
    ) -> Result<Vec<(Slot, Stored)>, Error> {
        let Shared {
            file, pool, layout, ..
        } = &*self.shared;
        let mut records = self.shared.state.borrow().logs.tagged(path, tag);
        records.sort_unstable_by_key(|&(offset, ..)| offset);
        // A trim frees the oldest records first, so holding the oldest holds each.
        let _hold =
            (records.first()).map(|&(offset, ..)| Hold::new(&self.shared, offset));
        let mut found = Vec::with_capacity(records.len());
        for record in records.chunk_by(|a, b| a.0 == b.0) {
            let [(offset, ..), ..] = *record else {
                unreachable!("invariant: a chunk is not empty");
            };
            let wanted: hash::Map<channel::Key, Slot> = record
                .iter()
                .map(|&(_, slot, index)| (index, slot))
                .collect();
            let place = AREA_START + layout.place(offset);
            found.extend(read::newest(file, pool, place, path, tag, &wanted).await?);
        }
        Ok(found)
    }

    /// Gives `reading` the records of `path` of the index at `slot` until it ends.
    async fn walk(
        &self,
        reading: &mut Reading<'_>,
        slot: Slot,
        path: Path,
    ) -> Result<(), Error> {
        // No await comes between a lookup and its hold, or between the drop of a
        // hold and the next lookup, so no trim passes the records the read goes on
        // to.
        while let Some(from) = reading.next() {
            let found = self.shared.state.borrow().logs.find(slot, path, from);
            match found {
                Found::Run(index, run) => {
                    let _hold = Hold::new(&self.shared, run.offset);
                    if !reading.record(index, run).await? {
                        break;
                    }
                }
                Found::End(end) => {
                    reading.end(end);
                    break;
                }
            }
        }
        Ok(())
    }

    /// How many group commits ended since the open. It moves before the [`Commit`]
    /// futures that the commit resolves wake, and a failed commit does not move it.
    /// [`durable`](Self::durable) changes only at a commit that moves the count, and
    /// before the count moves. A move does not make every entry durable: an entry
    /// appended while a commit runs goes in the next one, so read
    /// [`durable`](Self::durable) after a move.
    #[must_use]
    pub fn commits(&self) -> u64 {
        self.shared.state.borrow().commits
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
    /// [`Rejected`] as each variant says.
    ///
    /// # Panics
    ///
    /// When a `first` is below the tail of its path, with the index, the path,
    /// `first`, and the tail, when `first + len` passes `u64::MAX`, or when an
    /// entry's slot held another index on its path before.
    pub fn append(
        &self,
        entries: impl IntoIterator<Item = Entry>,
    ) -> Result<(), Rejected> {
        let mut batch = self.batch.take();
        batch.extend(entries);
        let queued = self.queue(&mut batch);
        batch.clear();
        batch.shrink_to(ENTRIES_MAX);
        self.batch.replace(batch);
        queued
    }

    /// Takes `batch` into a group, or leaves its entries in it.
    fn queue(&self, batch: &mut Vec<Entry>) -> Result<(), Rejected> {
        let shared = &*self.shared;
        let mut guard = shared.state.borrow_mut();
        let state = &mut *guard;
        if let Some(error) = &state.failed {
            return Err(Rejected::Files(error.clone()));
        }
        let (pool, block_max) = (&*shared.pool, shared.block_max);
        let taken = match state.open.push(pool, block_max, &state.writer, batch) {
            Ok(taken) => taken,
            Err(group::Rejected::Record) => {
                state.close_open();
                state
                    .open
                    .push(pool, block_max, &state.writer, batch)
                    .map_err(rejected)?
            }
            #[cfg(feature = "trim")]
            Err(full @ group::Rejected::Ring(_)) => {
                state.starved = true;
                drop(guard);
                shared.unpark();
                return Err(rejected(full));
            }
            Err(other) => return Err(rejected(other)),
        };
        for (slot, header) in state.open.entries(taken) {
            state
                .logs
                .append(slot, header)
                .unwrap_or_else(|invalid| panic!("invariant: {invalid}"));
        }
        let idle = state.idle();
        drop(guard);
        if !idle {
            shared.unpark();
        }
        Ok(())
    }

    /// Resolves when every entry appended before the call is durable: at once when
    /// none waits, else at the end of the group commit that holds the last of them.
    /// Gives the file error that ended the buffer when it ended before they were
    /// durable. [`Commit`] says what one held past the drop gives.
    #[must_use]
    pub fn committed(&self) -> Commit {
        let until = self.shared.state.borrow().durable_at();
        Commit {
            waiter: Waiter::new(&self.shared, Event::Commit),
            until,
        }
    }

    /// Resolves at the end of the next group commit after the call, also one in
    /// flight, or gives the file error that ended the buffer. It does not resolve
    /// while nothing is appended. [`Commit`] says what one held past the drop gives.
    #[must_use]
    pub fn next_commit(&self) -> Commit {
        let until = self.shared.state.borrow().commits + 1;
        Commit {
            waiter: Waiter::new(&self.shared, Event::Commit),
            until,
        }
    }

    /// Resolves once the commit task ended: after the drop, once nothing is queued,
    /// or at a failed file call. Gives the error of that call, so `Ok` means that
    /// each entry appended before the drop is durable.
    #[must_use]
    pub fn ended(&self) -> End {
        End {
            waiter: Waiter::new(&self.shared, Event::End),
        }
    }
}

/// The error of a push that a new group refuses too.
fn rejected(rejected: group::Rejected) -> Rejected {
    match rejected {
        group::Rejected::Large(limit) => Rejected::Large(limit),
        group::Rejected::Ring(full) => Rejected::Full {
            needed: full.needed,
            free: full.free,
        },
        group::Rejected::Pool(error) => Rejected::Pool(error),
        group::Rejected::Record => {
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

/// Opens the ring file in `dir` and reads its checkpoint. Makes the ring with
/// `layout`, and writes its first checkpoint, when the file is not there or holds no
/// checkpoint. An open that stopped before its first checkpoint leaves such a file:
/// it holds no record, and its length can be that of another layout.
async fn open_ring(
    files: &Files,
    dir: &path::Path,
    pool: &Pool,
    entropy: &Entropy,
    layout: Layout,
) -> Result<(File, Header), Error> {
    let written = open_written(files, dir, pool, layout).await?;
    let (file, blocks) = if let Some(written) = written {
        written
    } else {
        files.create_dir(dir).await?;
        // A removed ring keeps its room until this sync, and the new ring needs it.
        files.sync_dir(dir).await?;
        let len = layout.file_len();
        let file = files.open(&dir.join("ring"), Mode::Create { len }).await?;
        // Another open can make this ring, commit to it, and close in between.
        let blocks = file.read_at(0, pool.alloc(2 * ALIGN)?).await?;
        (file, blocks)
    };
    // An open that stopped after it made the ring may not have made it durable.
    if let Some(parent) = dir.parent() {
        files.sync_dir(parent).await?;
    }
    files.sync_dir(dir).await?;
    let header = if unwritten(&blocks) {
        create_header(&file, pool, entropy, layout).await?
    } else {
        read_header(&file, blocks).await?
    };
    Ok((file, header))
}

/// True when `blocks`, the two header blocks of a ring file, hold no checkpoint.
fn unwritten(blocks: &[u8]) -> bool {
    blocks.iter().all(|&byte| byte == 0)
}

/// Opens the ring file in `dir` and reads its two header blocks. `None` when no file
/// with a checkpoint is there. A file that is empty, or whose header blocks are
/// zero, holds no checkpoint: this removes it.
///
/// # Errors
///
/// [`Error::Length`] when the file is not empty and ends inside its header blocks.
async fn open_written(
    files: &Files,
    dir: &path::Path,
    pool: &Pool,
    layout: Layout,
) -> Result<Option<(File, Unique)>, Error> {
    let path = dir.join("ring");
    let file = match files.open(&path, Mode::Write).await {
        Ok(file) => file,
        Err(files::Error::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let found = file.len();
    if found != 0 {
        if found < AREA_START {
            let expected = layout.file_len();
            return Err(Error::Length { expected, found });
        }
        let blocks = file.read_at(0, pool.alloc(2 * ALIGN)?).await?;
        if !unwritten(&blocks) {
            return Ok(Some((file, blocks)));
        }
    }
    // The handle stays through the remove, so no other open takes this file.
    files.remove(&path).await?;
    drop(file);
    Ok(None)
}

/// Reads the newer checkpoint of `blocks`, the two header blocks of `file`, and
/// writes both blocks again, as read, for the reason [`walk`] gives.
async fn read_header(file: &File, blocks: Unique) -> Result<Header, Error> {
    let (first, rest) = blocks
        .split_first_chunk::<ALIGN>()
        .expect("invariant: `blocks` holds the two header blocks");
    let second = rest
        .first_chunk::<ALIGN>()
        .expect("invariant: `blocks` holds the two header blocks");
    let header = Header::decode(first, second)?;
    let (expected, found) = (header.layout.file_len(), file.len());
    if found != expected {
        return Err(Error::Length { expected, found });
    }
    file.write_at(0, &[blocks.freeze()]).await?;
    Ok(header)
}

/// Writes the first checkpoint of a new ring to both header blocks, and syncs it.
async fn create_header(
    file: &File,
    pool: &Pool,
    entropy: &Entropy,
    layout: Layout,
) -> Result<Header, Error> {
    let header = Header::new(layout, random(entropy));
    let mut block = pool.alloc(ALIGN)?;
    block.copy_from_slice(&header.encode());
    let block = block.freeze();
    file.write_at(0, &[block.clone(), block]).await?;
    file.sync().await?;
    Ok(header)
}

/// Recovers the tail of every path from the records after the tail of `header`,
/// and writes each window that holds them again, as read. Returns the cursor at
/// the end of the walk and the logs.
///
/// A read can see writes that a failed sync lost, from the cache. Written again,
/// they read the same until the open's sync makes them durable.
async fn walk(
    file: &File,
    pool: &Pool,
    header: &Header,
    slots: &mut Slots,
) -> Result<(Cursor, Logs), Error> {
    let mut cursor = Cursor::new(header.layout, header.tail, pool.largest());
    let mut logs = Logs::new(header.layout.area());
    loop {
        let Window { place, len } = cursor.window();
        let bytes = file.read_at(AREA_START + place, pool.alloc(len)?).await?;
        let offset = cursor.offset();
        match cursor.next(&bytes)? {
            Step::Data(body) => {
                recover(body, offset, pool.largest(), slots, &mut logs)?;
            }
            Step::Carry(body) => recover_carried(body, offset, slots, &mut logs)?,
            Step::Moved | Step::More => {}
            Step::End => break,
        }
        let bytes = bytes.freeze();
        file.write_at(AREA_START + place, slice::from_ref(&bytes))
            .await?;
    }
    Ok((cursor, logs))
}

/// Feeds the logs the entries of a record body at `offset`, as appended and
/// synced. Fails when an entry is over `largest`, the pool's largest block.
fn recover(
    body: Body<'_>,
    offset: u64,
    largest: usize,
    slots: &mut Slots,
    logs: &mut Logs,
) -> Result<(), Error> {
    let unread = |_: entry::Invalid| Error::Invalid { offset };
    let misplaced = |_: log::Invalid| Error::Invalid { offset };
    for header in entry::parse(body.start, body.len).map_err(unread)? {
        let (header, _) = header.map_err(unread)?;
        let requested = usize::try_from(header.bytes).unwrap_or(usize::MAX);
        if requested > largest {
            return Err(Error::Pool(block::Error::TooLarge { requested, largest }));
        }
        let slot = slots.index(header.index);
        logs.append(slot, &header).map_err(misplaced)?;
        logs.sync(slot, &header, offset).map_err(misplaced)?;
    }
    Ok(())
}

/// Feeds the logs the tails of a carry record body at `offset`.
fn recover_carried(
    body: &[u8],
    offset: u64,
    slots: &mut Slots,
    logs: &mut Logs,
) -> Result<(), Error> {
    let unread = |_: entry::Invalid| Error::Invalid { offset };
    let misplaced = |_: log::Invalid| Error::Invalid { offset };
    for tail in carry::parse(body).map_err(unread)? {
        let tail = tail.map_err(unread)?;
        let slot = slots.index(tail.index);
        logs.carry(slot, &tail, offset).map_err(misplaced)?;
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

/// The commit task. It parks while the state idles; a push that takes an entry, a
/// push that the ring refuses, or the drop wakes it. Each deadline takes the closed
/// groups and the open one with the trim and the carry record of the commit, seals
/// the records in order from `chain`, the value of the restart record, writes them
/// and the checkpoint, syncs once, and wakes the waiters. A deadline with none of
/// these writes nothing. A failed file call ends the task, and so does the drop
/// once nothing is queued.
async fn run(shared: Rc<Shared>, clock: Clock, commit: Span, chain: u32) {
    let mut chain = chain;
    let mut taken: Vec<Closed> = Vec::new();
    let mut sealed: Vec<Sealed> = Vec::new();
    let mut woken: Vec<Waker> = Vec::new();
    let mut sleep = clock.sleep(commit);
    loop {
        let mut idled = false;
        let ended = poll_fn(|cx| {
            let mut state = shared.state.borrow_mut();
            if !state.idle() || state.starved {
                return Poll::Ready(false);
            }
            if state.closed {
                return Poll::Ready(true);
            }
            state.parked = Parked(Some(cx.waker().clone()));
            idled = true;
            Poll::Pending
        })
        .await;
        if ended {
            let mut state = shared.state.borrow_mut();
            state.end(&mut woken);
            drop(state);
            for waker in woken.drain(..) {
                waker.wake();
            }
            return;
        }
        // After an idle span a passed deadline restarts, so the first entry of a burst
        // waits for others. Without one it fires now.
        if idled && sleep.deadline() < clock.now() {
            sleep.reset(clock.now() + commit);
        }
        (&mut sleep).await;
        sleep.reset(clock.now() + commit);
        let trim = shared.state.borrow_mut().take(&mut taken, &shared.pool);
        if taken.is_empty() && trim.checkpoint.is_none() && trim.carry.is_none() {
            continue;
        }
        let carrying;
        (carrying, chain) = seal(&mut taken, &mut sealed, trim.carry, chain);
        let checkpoint = trim.checkpoint;
        let result =
            write(&shared, &sealed, carrying.as_ref(), checkpoint.as_ref()).await;
        let failed = result.is_err();
        let mut state = shared.state.borrow_mut();
        match result {
            Ok(()) => state.synced(
                &mut sealed,
                carrying.map(|carrying| (carrying.offset, carrying.ends)),
                checkpoint.map(|checkpoint| checkpoint.tail),
            ),
            Err(error) => {
                state.failed = Some(error);
                state.end(&mut woken);
            }
        }
        state.committing.drain(&mut woken);
        drop(state);
        for waker in woken.drain(..) {
            waker.wake();
        }
        if failed {
            return;
        }
    }
}

/// Seals the `taken` groups into `sealed`, then `carry`, in order from `chain`, the
/// chain value of the record before. Gives the sealed carry record and the chain
/// value after the last record.
fn seal(
    taken: &mut Vec<Closed>,
    sealed: &mut Vec<Sealed>,
    carry: Option<Carry>,
    mut chain: u32,
) -> (Option<Carrying>, u32) {
    for closed in taken.drain(..) {
        let record = closed.seal(chain);
        chain = record.ends().record.chain();
        sealed.push(record);
    }
    let carrying = carry.map(|carry| carry.seal(chain));
    let chain = carrying
        .as_ref()
        .map_or(chain, |carrying| carrying.ends.record.chain());
    (carrying, chain)
}

/// Writes each sealed group's records, then the carry record and the checkpoint of
/// the commit, and syncs once.
async fn write(
    shared: &Shared,
    sealed: &[Sealed],
    carrying: Option<&Carrying>,
    checkpoint: Option<&Checkpoint>,
) -> Result<(), files::Error> {
    let file = &shared.file;
    for record in sealed {
        for (place, blocks) in record.writes() {
            file.write_at(AREA_START + place, blocks).await?;
        }
    }
    if let Some(carrying) = carrying {
        if let Some((place, block)) = &carrying.wrap {
            file.write_at(AREA_START + place, slice::from_ref(block))
                .await?;
        }
        file.write_at(
            AREA_START + carrying.place,
            slice::from_ref(&carrying.block),
        )
        .await?;
    }
    if let Some(checkpoint) = checkpoint {
        file.write_at(checkpoint.place, slice::from_ref(&checkpoint.block))
            .await?;
    }
    file.sync().await
}

/// The future of [`Buffer::committed`] and [`Buffer::next_commit`]. It does not borrow
/// the buffer, and it holds the ring open until it drops. Held past the drop of the
/// buffer, it resolves once the task ended: with the error that ended the task when
/// that came before what it waits for was durable, else with `Ok`.
#[derive(Debug)]
pub struct Commit {
    waiter: Waiter,
    /// The count of `commits` that resolves it.
    until: u64,
}

impl Future for Commit {
    type Output = Result<(), files::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let until = self.until;
        self.waiter.poll(cx, |state| {
            // Before `outcome`: a commit that synced stays well after a later sync
            // fails.
            if state.commits >= until && (!state.closed || state.ended) {
                return Some(Ok(()));
            }
            state.outcome()
        })
    }
}

/// The future of [`Buffer::ended`]. It does not borrow the buffer, and it holds the
/// ring open until it drops.
#[derive(Debug)]
pub struct End {
    waiter: Waiter,
}

impl Future for End {
    type Output = Result<(), files::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.waiter.poll(cx, State::outcome)
    }
}

/// A record that a read in progress holds, which no trim frees, until the drop.
struct Hold<'a> {
    shared: &'a Shared,
    offset: u64,
}

impl<'a> Hold<'a> {
    fn new(shared: &'a Shared, offset: u64) -> Self {
        shared.state.borrow_mut().held.push(offset);
        Self { shared, offset }
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        let held = &mut self.shared.state.borrow_mut().held;
        let at = held.iter().position(|&offset| offset == self.offset);
        held.swap_remove(at.expect("invariant: a hold is in `held`"));
    }
}

/// What a [`Waiter`] waits for.
#[derive(Clone, Copy, Debug)]
enum Event {
    Commit,
    End,
}

impl Event {
    /// The set of `state` that keeps the wakers of this event.
    fn set(self, state: &mut State) -> &mut wait::Set {
        match self {
            Self::Commit => &mut state.committing,
            Self::End => &mut state.ending,
        }
    }
}

/// The part of a [`Commit`] or an [`End`] that keeps its waker in the set of its
/// event. Its drop takes the waker out.
struct Waiter {
    shared: Rc<Shared>,
    event: Event,
    key: u64,
}

impl Waiter {
    fn new(shared: &Rc<Shared>, event: Event) -> Self {
        Self {
            key: shared.state.borrow_mut().key(),
            shared: Rc::clone(shared),
            event,
        }
    }

    /// Gives what `ready` reads from the state, else keeps the waker of `cx`.
    fn poll<T>(
        &self,
        cx: &Context<'_>,
        ready: impl FnOnce(&State) -> Option<T>,
    ) -> Poll<T> {
        let mut state = self.shared.state.borrow_mut();
        if let Some(output) = ready(&state) {
            return Poll::Ready(output);
        }
        let replaced = self.event.set(&mut state).insert(self.key, cx.waker());
        // A waker's drop can drop another waiter, which borrows the state.
        drop(state);
        drop(replaced);
        Poll::Pending
    }
}

/// Prints what the waiter waits for, not the state of the buffer.
impl fmt::Debug for Waiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Waiter")
            .field("event", &self.event)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        let mut state = self.shared.state.borrow_mut();
        let held = self.event.set(&mut state).remove(self.key);
        // As in `poll`.
        drop(state);
        drop(held);
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::pin::pin;
    use std::sync::{Arc, Mutex};

    use block::Heap;
    use types::time::Stamp;

    use super::*;
    use crate::log::Run;

    const COMMIT: Span = Span::from_nanos(1_000_000);

    fn entry(index: u32, slot: Slot, path: Path, first: u64, part: &Block) -> Entry {
        Entry {
            index: channel::Key::from_u128(u128::from(index)),
            slot,
            path,
            first,
            len: 3,
            stored_at: Stamp::from_nanos(7),
            last: Some(Stamp::from_nanos(9)),
            tag: 0,
            parts: part.clone().into(),
        }
    }

    /// Runs `main` on a shard of `node` with a buffer on the node's files, then
    /// gives the logs the buffer ended with.
    fn with_buffer<F>(
        sim: &mut sim::Sim,
        node: &sim::node::Node,
        name: &str,
        main: impl FnOnce(Buffer, Slots, Rc<Pool>) -> F + Send + 'static,
    ) -> Logs
    where
        F: Future<Output = Buffer> + 'static,
    {
        let logs = Arc::new(Mutex::new(None));
        let ended = Arc::clone(&logs);
        let config = env::shards::Config {
            name: name.into(),
            core: None,
        };
        let shard = node.clone();
        let handle = node
            .shards()
            .start(config, move |tasks| async move {
                let config = block::Config::new(1 << 20).expect("the budget fits");
                let pool =
                    Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
                let config = Config {
                    files: shard.files(),
                    dir: PathBuf::from("ring"),
                    pool: Rc::clone(&pool),
                    clock: shard.clock(),
                    tasks,
                    entropy: shard.entropy(),
                    layout: Layout::new(64 * 4096, 8000)
                        .expect("the sizes make a ring"),
                    commit: COMMIT,
                };
                let mut slots = Slots::new();
                let buffer = Buffer::open(config, &mut slots).await.expect("opens");
                let buffer = main(buffer, slots, pool).await;
                let logs = buffer.shared.state.borrow().logs.clone();
                *ended.lock().expect("no panic held the lock") = Some(logs);
            })
            .expect("the shard starts");
        sim.run().expect("the run ends");
        handle.join().expect("the shard ended");
        let mut logs = logs.lock().expect("no panic held the lock");
        logs.take().expect("the shard gave the logs")
    }

    /// Three commits, the first with two appends in one record, make one run per
    /// record and path, each from the mark after the record before. An open of the
    /// same ring walks the records into the same runs.
    #[test]
    fn the_walk_makes_the_runs_the_syncs_made() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let written = with_buffer(
            &mut sim,
            &node,
            "write",
            |buffer, mut slots, pool| async move {
                let one = slots.index(channel::Key::from_u128(1));
                let two = slots.index(channel::Key::from_u128(2));
                let part = pool.alloc(100).expect("a block").freeze();
                for commit in 0..3 {
                    let seq = 6 * commit;
                    let batch = [
                        entry(1, one, Path::Live, seq, &part),
                        entry(2, two, Path::Backfill, 3 * commit, &part),
                    ];
                    buffer.append(batch).expect("the ring has room");
                    if commit == 0 {
                        let more = [entry(1, one, Path::Live, 3, &part)];
                        buffer.append(more).expect("the ring has room");
                    }
                    buffer.committed().await.expect("commits");
                }
                buffer
            },
        );
        let run = |seq, offset| Run {
            start: Mark::at(seq),
            offset,
        };
        let live: Vec<Run> = written.runs(Slot::new(0), Path::Live).collect();
        assert_eq!(live, [run(0, 4096), run(6, 8192), run(9, 12288)]);
        let backfill: Vec<Run> = written.runs(Slot::new(1), Path::Backfill).collect();
        assert_eq!(backfill, [run(0, 4096), run(3, 8192), run(6, 12288)]);
        assert_eq!(written.durable(Slot::new(0), Path::Live).seq, 15);
        assert_eq!(written.appended(Slot::new(0), Path::Live).seq, 15);
        let walked =
            with_buffer(
                &mut sim,
                &node,
                "walk",
                |buffer, _, _| async move { buffer },
            );
        assert_eq!(walked, written);
    }

    /// A commit gives the writer the boundaries of its records. No public call
    /// shows them, so the test asks the writer for its trim, or the checkpoint for
    /// the tail when the commits trimmed.
    #[test]
    fn a_commit_gives_the_writer_the_boundaries_of_its_records() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let tail = Arc::new(Mutex::new(None));
        let found = Arc::clone(&tail);
        with_buffer(
            &mut sim,
            &node,
            "write",
            |buffer, mut slots, pool| async move {
                let one = slots.index(channel::Key::from_u128(1));
                let part = pool.alloc(100).expect("a block").freeze();
                for commit in 0..60 {
                    let batch = [entry(1, one, Path::Live, 3 * commit, &part)];
                    buffer.append(batch).expect("the ring has room");
                    buffer.committed().await.expect("commits");
                }
                let state = buffer.shared.state.borrow();
                let trimmed = state.writer.trimmed(|offset| offset);
                *found.lock().expect("no panic held the lock") =
                    Some(trimmed.unwrap_or(state.header.tail));
                drop(state);
                buffer
            },
        );
        // The restart record and 60 records of one block are synced. The headroom
        // is three records of two blocks and the two blocks that each record keeps
        // free.
        let tail = tail.lock().expect("no panic held the lock");
        assert_eq!(tail.map(crate::wal::Position::offset), Some(5 * 4096));
    }

    /// Three commits put [0, 3), [3, 6), and [6, 9) of one path in the records at
    /// 4096, 8192, and 12288. Gives the slot of the path.
    async fn create_three_records(
        buffer: &Buffer,
        slots: &mut Slots,
        pool: &Pool,
    ) -> Slot {
        let one = slots.index(channel::Key::from_u128(1));
        let part = pool.alloc(100).expect("a block").freeze();
        for commit in 0..3 {
            let batch = [entry(1, one, Path::Live, 3 * commit, &part)];
            buffer.append(batch).expect("the ring has room");
            buffer.committed().await.expect("commits");
        }
        one
    }

    /// The gap, the first seq of each entry, and the next mark of a read of the
    /// live path at `slot` from `from`.
    async fn read_from(
        buffer: &Buffer,
        slot: Slot,
        from: u64,
    ) -> (Option<Range<u64>>, Vec<u64>, Mark) {
        let read = buffer
            .read(slot, Path::Live, Mark::at(from), usize::MAX)
            .await
            .expect("reads");
        let firsts = read.entries.iter().map(|entry| entry.first).collect();
        (read.gap, firsts, read.next)
    }

    /// The test hides the records by hand, so that it picks the tail of each read.
    #[test]
    fn a_read_gives_the_seqs_of_the_records_that_a_trim_hid_as_its_gap() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let reads = Arc::new(Mutex::new(Vec::new()));
        let found = Arc::clone(&reads);
        with_buffer(
            &mut sim,
            &node,
            "write",
            |buffer, mut slots, pool| async move {
                let one = create_three_records(&buffer, &mut slots, &pool).await;
                let mut reads = Vec::new();
                buffer.shared.state.borrow_mut().logs.hide(8192);
                reads.push(read_from(&buffer, one, 0).await);
                reads.push(read_from(&buffer, one, 1).await);
                reads.push(read_from(&buffer, one, 3).await);
                buffer.shared.state.borrow_mut().logs.hide(16384);
                reads.push(read_from(&buffer, one, 0).await);
                reads.push(read_from(&buffer, one, 8).await);
                reads.push(read_from(&buffer, one, 9).await);
                *found.lock().expect("no panic held the lock") = reads;
                buffer
            },
        );
        let reads = reads.lock().expect("no panic held the lock");
        let expected = [
            (Some(0..3), vec![3, 6], Mark::at(9)),
            (Some(1..3), vec![3, 6], Mark::at(9)),
            (None, vec![3, 6], Mark::at(9)),
            (Some(0..9), vec![], Mark::at(9)),
            (Some(8..9), vec![], Mark::at(9)),
            (None, vec![], Mark::at(9)),
        ];
        assert_eq!(*reads, expected);
    }

    /// The test hides the records by hand. `newest` gives the tagged entry of the
    /// oldest record that is not hidden, and skips it once that record is hidden,
    /// also when later records of its path are not hidden.
    #[test]
    fn newest_skips_the_records_that_a_trim_hid() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let found = Arc::new(Mutex::new(Vec::new()));
        let given = Arc::clone(&found);
        with_buffer(
            &mut sim,
            &node,
            "write",
            |buffer, mut slots, pool| async move {
                let one = slots.index(channel::Key::from_u128(1));
                let part = pool.alloc(100).expect("a block").freeze();
                let tagged = Entry {
                    len: 0,
                    tag: 1,
                    ..entry(1, one, Path::Live, 0, &part)
                };
                buffer.append([tagged]).expect("the ring has room");
                buffer.committed().await.expect("commits");
                for first in [0, 3] {
                    let batch = [entry(1, one, Path::Live, first, &part)];
                    buffer.append(batch).expect("the ring has room");
                    buffer.committed().await.expect("commits");
                }
                let firsts = async || {
                    let newest = buffer
                        .newest(Path::Live, NonZeroU8::MIN)
                        .await
                        .expect("reads");
                    newest
                        .iter()
                        .map(|(slot, stored)| (*slot, stored.first))
                        .collect()
                };
                let mut found: Vec<Vec<(Slot, u64)>> = vec![firsts().await];
                for offset in [4096, 8192] {
                    buffer.shared.state.borrow_mut().logs.hide(offset);
                    found.push(firsts().await);
                }
                *given.lock().expect("no panic held the lock") = found;
                buffer
            },
        );
        let found = found.lock().expect("no panic held the lock");
        let tagged = vec![(Slot::new(0), 0)];
        assert_eq!(*found, [tagged.clone(), tagged, vec![]]);
    }

    /// A read holds the record it waits on, and `newest` holds the oldest record it
    /// reads, until the read ends or drops. The tagged entry and [0, 3), [3, 6),
    /// and [6, 9) of one path are in the records at 4096, 8192, and 12288, so the
    /// tail of the path holds 12288.
    #[test]
    fn a_read_holds_the_record_it_waits_on_until_it_ends_or_drops() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let kept = Arc::new(Mutex::new(Vec::new()));
        let given = Arc::clone(&kept);
        with_buffer(
            &mut sim,
            &node,
            "write",
            |buffer, mut slots, pool| async move {
                let one = slots.index(channel::Key::from_u128(1));
                let part = pool.alloc(100).expect("a block").freeze();
                let tagged = Entry {
                    len: 0,
                    tag: 1,
                    ..entry(1, one, Path::Live, 0, &part)
                };
                for batch in [
                    vec![tagged, entry(1, one, Path::Live, 0, &part)],
                    vec![entry(1, one, Path::Live, 3, &part)],
                    vec![entry(1, one, Path::Live, 6, &part)],
                ] {
                    buffer.append(batch).expect("the ring has room");
                    buffer.committed().await.expect("commits");
                }
                let kept_now = || buffer.shared.state.borrow_mut().kept();
                let mut kept = vec![kept_now()];
                {
                    let mut read = pin!(read_from(&buffer, one, 0));
                    let first = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
                    assert!(first.is_pending(), "the read waits on a file read");
                    kept.push(kept_now());
                    read.await;
                }
                kept.push(kept_now());
                {
                    let mut read = pin!(read_from(&buffer, one, 3));
                    let first = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
                    assert!(first.is_pending(), "the read waits on a file read");
                    kept.push(kept_now());
                }
                kept.push(kept_now());
                {
                    let mut newest = pin!(buffer.newest(Path::Live, NonZeroU8::MIN));
                    let first =
                        poll_fn(|cx| Poll::Ready(newest.as_mut().poll(cx))).await;
                    assert!(first.is_pending(), "newest waits on a file read");
                    kept.push(kept_now());
                    newest.await.expect("reads");
                }
                kept.push(kept_now());
                {
                    let mut newest = pin!(buffer.newest(Path::Live, NonZeroU8::MIN));
                    let first =
                        poll_fn(|cx| Poll::Ready(newest.as_mut().poll(cx))).await;
                    assert!(first.is_pending(), "newest waits on a file read");
                    kept.push(kept_now());
                }
                kept.push(kept_now());
                *given.lock().expect("no panic held the lock") = kept;
                buffer
            },
        );
        let kept = kept.lock().expect("no panic held the lock");
        let expected = [12288, 4096, 12288, 8192, 12288, 4096, 12288, 4096, 12288];
        assert_eq!(*kept, expected.map(Some));
    }

    /// A read from (0, 1) holds the record at 8192, whose entry with no samples at
    /// seq 0 comes after the one at 4096. A walk from 8192 would count it from
    /// (0, 0), so the trim lands at 4096, though 59 more records want more room.
    /// Once the read drops, the trim goes on to the room they want. The test asks
    /// the writer for its trim, or the checkpoint for the tail when the commits
    /// trimmed.
    #[test]
    fn a_trim_held_after_entries_with_no_samples_at_a_seq_lands_before_them() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let tails = Arc::new(Mutex::new(Vec::new()));
        let found = Arc::clone(&tails);
        with_buffer(
            &mut sim,
            &node,
            "write",
            |buffer, mut slots, pool| async move {
                let one = slots.index(channel::Key::from_u128(1));
                let part = pool.alloc(100).expect("a block").freeze();
                let empty = Entry {
                    len: 0,
                    last: None,
                    ..entry(1, one, Path::Live, 0, &part)
                };
                for _ in 0..2 {
                    buffer.append([empty.clone()]).expect("the ring has room");
                    buffer.committed().await.expect("commits");
                }
                let trimmed = || {
                    let mut state = buffer.shared.state.borrow_mut();
                    let kept = state.kept();
                    let tail = state.trimmed().unwrap_or(state.header.tail);
                    (kept, tail.offset())
                };
                let mut tails = Vec::new();
                {
                    let from = Mark::at(0).after(0, 0);
                    let mut read = pin!(buffer.read(one, Path::Live, from, usize::MAX));
                    let first = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
                    assert!(first.is_pending(), "the read waits on a file read");
                    for commit in 0..59 {
                        let batch = [entry(1, one, Path::Live, 3 * commit, &part)];
                        buffer.append(batch).expect("the ring has room");
                        buffer.committed().await.expect("commits");
                    }
                    tails.push(trimmed());
                }
                tails.push(trimmed());
                *found.lock().expect("no panic held the lock") = tails;
                buffer
            },
        );
        let tails = tails.lock().expect("no panic held the lock");
        assert_eq!(*tails, [(Some(8192), 4096), (Some(61 * 4096), 6 * 4096)]);
    }

    /// The durable end counts the entries with no samples at its seq. A read that
    /// gives only a gap goes on after them. A read from their seq loses no sample:
    /// it gives no gap and does not move.
    #[test]
    fn a_read_goes_on_after_the_hidden_entries_with_no_samples() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let reads = Arc::new(Mutex::new(Vec::new()));
        let found = Arc::clone(&reads);
        with_buffer(
            &mut sim,
            &node,
            "write",
            |buffer, mut slots, pool| async move {
                let one = slots.index(channel::Key::from_u128(1));
                let part = pool.alloc(100).expect("a block").freeze();
                let empty = || Entry {
                    len: 0,
                    ..entry(1, one, Path::Live, 3, &part)
                };
                let batch = [entry(1, one, Path::Live, 0, &part), empty(), empty()];
                buffer.append(batch).expect("the ring has room");
                buffer.committed().await.expect("commits");
                buffer.shared.state.borrow_mut().logs.hide(8192);
                let reads = vec![
                    read_from(&buffer, one, 0).await,
                    read_from(&buffer, one, 3).await,
                ];
                *found.lock().expect("no panic held the lock") = reads;
                buffer
            },
        );
        let reads = reads.lock().expect("no panic held the lock");
        let end = Mark { seq: 3, given: 2 };
        let expected = [(Some(0..3), vec![], end), (None, vec![], Mark::at(3))];
        assert_eq!(*reads, expected);
    }

    /// The test hides the later records while a read waits on its first record,
    /// which no trim does: the hold of the read keeps them.
    #[test]
    #[should_panic(
        expected = "invariant: a read's hold keeps a trim from hiding the records after \
                    its entries"
    )]
    fn a_gap_after_held_entries_is_a_broken_invariant() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        with_buffer(
            &mut sim,
            &node,
            "write",
            |buffer, mut slots, pool| async move {
                let one = create_three_records(&buffer, &mut slots, &pool).await;
                {
                    let mut read = pin!(read_from(&buffer, one, 0));
                    let first = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
                    assert!(first.is_pending(), "the read waits on a file read");
                    buffer.shared.state.borrow_mut().logs.hide(16384);
                    read.await;
                }
                buffer
            },
        );
    }

    /// The tests of the trim and the carry of each commit.
    #[cfg(feature = "trim")]
    mod trims {
        use super::*;

        /// Commits one entry of 3 seqs of `index` on the live path at `slot` for each
        /// `n` of `firsts`, at seq `3 * n`, one commit each.
        async fn commit_each(
            buffer: &Buffer,
            (index, slot): (u32, Slot),
            part: &Block,
            firsts: Range<u64>,
        ) {
            for first in firsts {
                let batch = [entry(index, slot, Path::Live, 3 * first, part)];
                buffer.append(batch).expect("the ring has room");
                buffer.committed().await.expect("commits");
            }
        }

        /// A read waits on the file read of the record at 4096 while 57 commits of
        /// another path want the room of the ring of 64 blocks. The trims stop before
        /// that record, and the read gives its entries. Once the read drops, the trims
        /// go on, so 100 more commits fit.
        #[test]
        fn a_read_holds_back_the_trim_of_the_record_it_waits_on() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            let found = Arc::new(Mutex::new(None));
            let given = Arc::clone(&found);
            with_buffer(
                &mut sim,
                &node,
                "write",
                |buffer, mut slots, pool| async move {
                    let one = create_three_records(&buffer, &mut slots, &pool).await;
                    let two = slots.index(channel::Key::from_u128(2));
                    let part = pool.alloc(100).expect("a block").freeze();
                    let read = {
                        let mut read = pin!(read_from(&buffer, one, 0));
                        let first =
                            poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
                        assert!(first.is_pending(), "the read waits on a file read");
                        commit_each(&buffer, (2, two), &part, 0..57).await;
                        read.await
                    };
                    commit_each(&buffer, (2, two), &part, 57..157).await;
                    *given.lock().expect("no panic held the lock") = Some(read);
                    buffer
                },
            );
            let found = found.lock().expect("no panic held the lock");
            assert_eq!(*found, Some((None, vec![0, 3, 6], Mark::at(9))));
        }

        /// The live path of index 1 writes [0, 3) once, then 100 commits of index 2
        /// turn the ring of 64 blocks. A carry record keeps the tail of index 1, and
        /// the trims pass its record, so its log keeps no run, and a read gives the
        /// seqs as a gap and no entry.
        #[test]
        fn a_trim_past_the_newest_record_of_a_path_drops_its_runs() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            let found = Arc::new(Mutex::new(None));
            let given = Arc::clone(&found);
            let logs = with_buffer(
                &mut sim,
                &node,
                "write",
                |buffer, mut slots, pool| async move {
                    let one = slots.index(channel::Key::from_u128(1));
                    let two = slots.index(channel::Key::from_u128(2));
                    let part = pool.alloc(100).expect("a block").freeze();
                    commit_each(&buffer, (1, one), &part, 0..1).await;
                    commit_each(&buffer, (2, two), &part, 0..100).await;
                    let read = read_from(&buffer, one, 0).await;
                    *given.lock().expect("no panic held the lock") =
                        Some((read, buffer.durable(one, Path::Live)));
                    buffer
                },
            );
            assert_eq!(logs.runs(Slot::new(0), Path::Live).count(), 0);
            let found = found.lock().expect("no panic held the lock");
            let tail = Tail {
                seq: 3,
                stamp: Some(Stamp::from_nanos(9)),
            };
            assert_eq!(*found, Some(((Some(0..3), vec![], Mark::at(3)), tail)));
        }

        /// One record holds an entry of each of indexes 1 to 100, after 6 records of
        /// index 101. The commits of index 101 turn the ring of 64 blocks, and the
        /// carry record of the 100 tails takes 2 blocks where the area has one left,
        /// so it wraps, and later trims pass the record of the 100 entries. After a
        /// cut of the power, the open walks past the wrap and finds each tail.
        #[test]
        fn a_carry_record_that_wraps_gives_its_tails_after_a_reopen() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            let skipped = Arc::new(Mutex::new(None));
            let given = Arc::clone(&skipped);
            with_buffer(
                &mut sim,
                &node,
                "write",
                |buffer, mut slots, pool| async move {
                    let byte = pool.alloc(1).expect("a block").freeze();
                    let part = pool.alloc(100).expect("a block").freeze();
                    let filler = slots.index(channel::Key::from_u128(101));
                    commit_each(&buffer, (101, filler), &part, 0..6).await;
                    let batch: Vec<Entry> = (1..=100)
                        .map(|index| {
                            let slot =
                                slots.index(channel::Key::from_u128(index.into()));
                            entry(index, slot, Path::Live, 0, &byte)
                        })
                        .collect();
                    buffer.append(batch).expect("the ring has room");
                    buffer.committed().await.expect("commits");
                    commit_each(&buffer, (101, filler), &part, 6..70).await;
                    let skip = buffer.shared.state.borrow().writer.skip();
                    *given.lock().expect("no panic held the lock") = Some(skip);
                    buffer
                },
            );
            let skip = skipped.lock().expect("no panic held the lock").clone();
            assert_eq!(skip, Some(63 * 4096..64 * 4096), "the carry record wraps");
            sim.crash(&node, sim::Crash::Power);
            let found = Arc::new(Mutex::new(Vec::new()));
            let given = Arc::clone(&found);
            with_buffer(
                &mut sim,
                &node,
                "reopen",
                |buffer, mut slots, _| async move {
                    let tails = (1..=101)
                        .map(|index| {
                            let slot = slots.index(channel::Key::from_u128(index));
                            buffer.durable(slot, Path::Live).seq
                        })
                        .collect();
                    *given.lock().expect("no panic held the lock") = tails;
                    buffer
                },
            );
            let mut seqs = vec![3; 100];
            seqs.push(210);
            assert_eq!(*found.lock().expect("no panic held the lock"), seqs);
        }

        /// The live path of index 1 writes [0, 3) once, then 100 commits of index 2
        /// turn the ring of 64 blocks, and the power cuts. The open finds the seq and
        /// stamp of index 1 in a carry record, and the path goes on from there.
        #[test]
        fn an_index_that_writes_once_continues_after_the_ring_turns() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            with_buffer(
                &mut sim,
                &node,
                "write",
                |buffer, mut slots, pool| async move {
                    let one = slots.index(channel::Key::from_u128(1));
                    let two = slots.index(channel::Key::from_u128(2));
                    let part = pool.alloc(100).expect("a block").freeze();
                    commit_each(&buffer, (1, one), &part, 0..1).await;
                    commit_each(&buffer, (2, two), &part, 0..100).await;
                    buffer
                },
            );
            sim.crash(&node, sim::Crash::Power);
            let found = Arc::new(Mutex::new(None));
            let given = Arc::clone(&found);
            with_buffer(
                &mut sim,
                &node,
                "reopen",
                |buffer, mut slots, pool| async move {
                    let one = slots.index(channel::Key::from_u128(1));
                    let part = pool.alloc(100).expect("a block").freeze();
                    let tail = buffer.durable(one, Path::Live);
                    commit_each(&buffer, (1, one), &part, 1..2).await;
                    let read = read_from(&buffer, one, 0).await;
                    *given.lock().expect("no panic held the lock") = Some((tail, read));
                    buffer
                },
            );
            let found = found.lock().expect("no panic held the lock");
            let tail = Tail {
                seq: 3,
                stamp: Some(Stamp::from_nanos(9)),
            };
            let read = (Some(0..3), vec![3], Mark::at(6));
            assert_eq!(*found, Some((tail, read)));
        }

        /// Records 1 and 2 hold an entry with no samples at seq 0, and record 3 holds
        /// [0, 3). 54 more records want the room of record 1, but a walk from record 2
        /// would count its entry from (0, 0), so the trim lands at record 1. After a
        /// cut of the power, the open walks from there, and a read from (0, 1) gives
        /// both later entries.
        #[test]
        fn a_trim_after_entries_with_no_samples_at_a_seq_keeps_both_after_a_reopen() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            let wanted = Arc::new(Mutex::new(None));
            let found = Arc::clone(&wanted);
            with_buffer(
                &mut sim,
                &node,
                "write",
                |buffer, mut slots, pool| async move {
                    let one = slots.index(channel::Key::from_u128(1));
                    let part = pool.alloc(100).expect("a block").freeze();
                    let empty = Entry {
                        len: 0,
                        last: None,
                        ..entry(1, one, Path::Live, 0, &part)
                    };
                    for _ in 0..2 {
                        buffer.append([empty.clone()]).expect("the ring has room");
                        buffer.committed().await.expect("commits");
                    }
                    commit_each(&buffer, (1, one), &part, 0..55).await;
                    let mut state = buffer.shared.state.borrow_mut();
                    let kept = state.kept().unwrap_or(u64::MAX);
                    let want = state.writer.trimmed(|offset| offset.min(kept));
                    *found.lock().expect("no panic held the lock") = Some((
                        want.map(crate::wal::Position::offset),
                        state.header.tail.offset(),
                    ));
                    drop(state);
                    buffer
                },
            );
            sim.crash(&node, sim::Crash::Power);
            let found = Arc::new(Mutex::new(None));
            let given = Arc::clone(&found);
            with_buffer(
                &mut sim,
                &node,
                "reopen",
                |buffer, mut slots, _| async move {
                    let one = slots.index(channel::Key::from_u128(1));
                    let from = Mark::at(0).after(0, 0);
                    let read = buffer
                        .read(one, Path::Live, from, usize::MAX)
                        .await
                        .expect("reads");
                    let lens: Vec<(u64, u32)> = read
                        .entries
                        .iter()
                        .take(2)
                        .map(|entry| (entry.first, entry.len))
                        .collect();
                    *given.lock().expect("no panic held the lock") =
                        Some((read.gap, lens));
                    buffer
                },
            );
            let wanted = wanted.lock().expect("no panic held the lock");
            assert_eq!(*wanted, Some((Some(8192), 4096)));
            let found = found.lock().expect("no panic held the lock");
            assert_eq!(*found, Some((None, vec![(0, 0), (0, 3)])));
        }

        /// While a read holds the record at 4096, appends of index 2 fill the ring
        /// until it refuses one. After the read drops, nothing is queued, so only the
        /// next refused append wakes the commit task. That commit has no data record,
        /// and its trim gives the room for the append after it.
        #[test]
        fn a_push_that_the_ring_refuses_wakes_a_commit_that_trims() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            let own = node.clone();
            let found = Arc::new(Mutex::new(None));
            let given = Arc::clone(&found);
            with_buffer(
                &mut sim,
                &node,
                "write",
                |buffer, mut slots, pool| async move {
                    let one = create_three_records(&buffer, &mut slots, &pool).await;
                    let two = slots.index(channel::Key::from_u128(2));
                    let part = pool.alloc(100).expect("a block").freeze();
                    let next =
                        |first: u64| [entry(2, two, Path::Live, 3 * first, &part)];
                    let mut first = 0;
                    {
                        let mut read = pin!(read_from(&buffer, one, 0));
                        let polled =
                            poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
                        assert!(polled.is_pending(), "the read waits on a file read");
                        while buffer.append(next(first)).is_ok() {
                            buffer.committed().await.expect("commits");
                            first += 1;
                        }
                        read.await;
                    }
                    let commits = buffer.commits();
                    let refused = buffer.append(next(first));
                    own.clock().sleep(Span::from_nanos(3_000_000)).await;
                    let fits = buffer.append(next(first)).is_ok();
                    let taken = (commits, buffer.commits());
                    *given.lock().expect("no panic held the lock") = Some((
                        matches!(refused, Err(Rejected::Full { .. })),
                        taken,
                        fits,
                    ));
                    buffer
                },
            );
            let found = found.lock().expect("no panic held the lock");
            let (refused, (before, after), fits) =
                found.expect("the shard gave the result");
            assert!(refused, "the ring refuses the append after the read drops");
            assert_eq!(before, after, "the commit of the trim has no data record");
            assert!(fits, "the commit that the refusal woke trims");
        }

        /// 70 commits of one path trim the ring of 64 blocks, and the power cuts after
        /// the last. The open walks from the tail that the last commit synced, so a
        /// read gives the same gap and entries as before the cut. The test does not pin
        /// the tail, which the room that each record keeps free sets.
        #[test]
        fn the_new_tail_is_durable_in_the_sync_of_its_commit() {
            let mut sim = sim::Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            let reads = Arc::new(Mutex::new(Vec::new()));
            let before = Arc::clone(&reads);
            with_buffer(
                &mut sim,
                &node,
                "write",
                |buffer, mut slots, pool| async move {
                    let one = slots.index(channel::Key::from_u128(1));
                    let part = pool.alloc(100).expect("a block").freeze();
                    commit_each(&buffer, (1, one), &part, 0..70).await;
                    let read = read_from(&buffer, one, 0).await;
                    before.lock().expect("no panic held the lock").push(read);
                    buffer
                },
            );
            sim.crash(&node, sim::Crash::Power);
            let after = Arc::clone(&reads);
            with_buffer(
                &mut sim,
                &node,
                "reopen",
                |buffer, mut slots, _| async move {
                    let one = slots.index(channel::Key::from_u128(1));
                    let read = read_from(&buffer, one, 0).await;
                    after.lock().expect("no panic held the lock").push(read);
                    buffer
                },
            );
            let reads = reads.lock().expect("no panic held the lock");
            let [before, after] = &reads[..] else {
                panic!("two reads, not {}", reads.len());
            };
            assert_eq!(before, after);
            let (gap, firsts, next) = before;
            let trimmed = firsts.first().copied().expect("the read gives entries");
            assert!(trimmed > 0, "the commits trim");
            assert_eq!(gap.clone(), Some(0..trimmed));
            assert_eq!(*next, Mark::at(210));
        }
    }
}
