//! Pools of preallocated, aligned buffers, and the blocks they hand out.
//!
//! One block holds one frame. A writer fills a [`Unique`], then freezes it into a
//! [`Block`] that many holders share through one reference count. When the last holder
//! drops a block, it returns to the pool that made it, from any thread.
//!
//! Blocks hold offsets, never pointers, so a block's bytes stay valid when they are
//! shared with another process.

#![expect(unsafe_code, reason = "blocks are raw memory that threads share")]

mod memory;
mod sync;

use std::cell::Cell;
use std::fmt;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::slice;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};

pub use memory::{Heap, Memory, Refused};
use sync::{AtomicUsize, Track};

/// Byte alignment of every block.
pub const ALIGN: usize = 64;

/// Bytes before the payload of a block, and before the first block of a region.
const HEADER: usize = 64;
/// The end of a list of blocks. No block is at offset 0.
const NONE: usize = 0;
/// In `Region::returned`: the pool is gone.
const CLOSED: usize = usize::MAX;
/// The most size classes a pool has. The largest payload is then 2 GiB, so a length
/// or a position fits in a `u32` and a handle stays 16 bytes.
const CLASSES_MAX: usize = 96;

/// Settings for one [`Pool`].
#[derive(Clone, Debug)]
pub struct Config {
    /// The most bytes the pool may commit at once. Resident memory can pass it by up
    /// to two partial pages per size class, because a purge rounds in to page bounds.
    pub budget: usize,
}

impl Config {
    /// Bytes of address space that a pool with these settings needs from its
    /// [`Memory`]. Each size class can grow to the full budget, so this is up to 96
    /// times the budget.
    ///
    /// # Panics
    ///
    /// If the result does not fit in a `usize`.
    #[must_use]
    pub fn reservation(&self) -> usize {
        self.budget
            .checked_next_multiple_of(ALIGN)
            .and_then(|span| span.checked_mul(classes(self.budget)))
            .and_then(|spans| spans.checked_add(HEADER))
            .unwrap_or_else(|| panic!("pool budget {} is too large", self.budget))
    }
}

/// How many size classes fit in `budget`, at most `CLASSES_MAX`.
fn classes(budget: usize) -> usize {
    match budget.checked_sub(HEADER) {
        Some(room) => class_of(room + 1).min(CLASSES_MAX),
        None => 0,
    }
}

/// The smallest class with a payload of at least `len` bytes. It can be past the
/// last class.
const fn class_of(len: usize) -> usize {
    let last = if len <= ALIGN { ALIGN - 1 } else { len - 1 };
    let doubling = last.ilog2().saturating_sub(8);
    4 * (doubling as usize) + (last >> (doubling + 6))
}

/// Bytes of payload in a block of class `index`.
const fn class_payload(index: usize) -> usize {
    if index < 4 {
        ALIGN * (index + 1)
    } else {
        (ALIGN << ((index - 4) / 4)) * (5 + index % 4)
    }
}

/// Bytes that one block of class `index` takes.
const fn class_footprint(index: usize) -> usize {
    HEADER + class_payload(index)
}

/// Bytes that a block with a payload of `len` bytes takes from its pool: the header
/// plus the payload of the smallest size class that holds `len`. Payloads step by
/// 64 bytes up to 256, then by a quarter of the lower power of two, so the payload
/// is at most 64 bytes or a quarter of `len` above it.
///
/// # Panics
///
/// If `len` passes the largest payload, 2 GiB.
#[must_use]
pub const fn footprint(len: usize) -> usize {
    let index = class_of(len);
    assert!(index < CLASSES_MAX, "no size class holds that many bytes");
    class_footprint(index)
}

/// The start of a pool's memory. Any thread that drops a block reaches it.
#[repr(C, align(64))]
struct Region {
    /// A stack of the blocks that holders dropped, or `CLOSED`.
    returned: AtomicUsize,
    /// After the pool is gone: the blocks that holders still have. It wraps below 0
    /// while a drop is ahead of the pool's count.
    owed: AtomicUsize,
    memory: Box<dyn Memory>,
    track: Track,
}

/// The start of each block.
#[repr(C, align(64))]
struct Header {
    refs: AtomicUsize,
    /// The next block in the list that holds this one.
    next: AtomicUsize,
    /// Bytes from the region to this header.
    offset: usize,
    class: usize,
    payload: Track,
}

const _: () = assert!(
    size_of::<Region>() <= HEADER && size_of::<Header>() <= HEADER,
    "the headers fit in the bytes before a payload"
);
const _: () = assert!(
    HEADER <= ALIGN,
    "the region header fits in the bytes `Memory` makes usable from the start"
);
const _: () = assert!(
    class_payload(CLASSES_MAX - 1) <= u32::MAX as usize,
    "the largest payload fits in a u32"
);
const _: () = assert!(
    size_of::<Unique>() == 16 && size_of::<Block>() == 16,
    "a handle is 16 bytes"
);

#[derive(Default)]
struct Class {
    /// Bytes of this class's span that are cut into blocks.
    carved: Cell<usize>,
    /// The most bytes `carved` ever was: the range a release purges.
    reached: Cell<usize>,
    free: Cell<usize>,
    /// Blocks that holders have, or that wait in `Region::returned`.
    lent: Cell<usize>,
    /// `Pool::purges` when a block last returned.
    returned_at: Cell<u64>,
}

impl Class {
    /// Carved, with no block in use.
    fn idle(&self) -> bool {
        self.lent.get() == 0 && self.carved.get() != 0
    }
}

/// A pool of blocks owned by one shard.
///
/// A pool is not `Sync`: only its owner shard allocates from it. Blocks it hands out
/// may move to and drop on any thread, and stay valid after the pool drops.
///
/// Blocks come in sizes, 64-byte steps up to 256 bytes and then four sizes per
/// doubling up to 2 GiB, each with 64 bytes in front. A free block keeps its budget
/// for its own size until another size needs the budget and no block of that size
/// is in use: `alloc` then gives the pages of the whole size back and takes the
/// budget. `purge` gives back the pages of a size that stayed idle across two of
/// its calls.
pub struct Pool {
    region: NonNull<Region>,
    budget: usize,
    span: usize,
    committed: Cell<usize>,
    /// Calls of `purge` so far: the clock that `Class::returned_at` reads.
    purges: Cell<u64>,
    classes: Box<[Class]>,
}

// SAFETY: a pool moves to its owner shard as a whole. `Memory` is `Send`, and the one
// part that other threads reach, the region header, is atomic.
unsafe impl Send for Pool {}

impl Pool {
    /// Creates a pool that cuts its blocks from `memory`. It commits pages only as
    /// blocks need them.
    ///
    /// # Panics
    ///
    /// If `memory` is shorter than [`Config::reservation`], or is not aligned to
    /// [`ALIGN`].
    #[must_use]
    #[expect(clippy::needless_pass_by_value, reason = "settings move into a pool")]
    pub fn new(config: Config, memory: impl Memory + 'static) -> Self {
        let need = config.reservation();
        let Config { budget } = config;
        let have = memory.len();
        assert!(
            have >= need,
            "pool needs {need} bytes of memory, got {have}"
        );
        let region = memory.base().cast::<Region>();
        assert!(
            region.is_aligned(),
            "pool memory must be aligned to 64 bytes"
        );
        let first = Region {
            returned: AtomicUsize::new(NONE),
            owed: AtomicUsize::new(0),
            memory: Box::new(memory),
            track: Track::new(),
        };
        // SAFETY: `Memory` makes the first `ALIGN` bytes usable from the start, and
        // they hold the `HEADER` bytes. The memory is aligned and unused.
        unsafe { region.write(first) };
        Self {
            region,
            budget,
            span: budget.next_multiple_of(ALIGN),
            committed: Cell::new(0),
            purges: Cell::new(0),
            classes: (0..classes(budget)).map(|_| Class::default()).collect(),
        }
    }

    /// The most bytes one block can hold.
    #[must_use]
    pub fn largest(&self) -> usize {
        self.classes.len().checked_sub(1).map_or(0, class_payload)
    }

    /// Returns a writable block of `len` bytes, aligned to [`ALIGN`]. It never waits.
    /// The bytes are not cleared: a block that is used again keeps its old bytes.
    ///
    /// # Errors
    ///
    /// - [`Error::TooLarge`] when no block of the pool holds `len` bytes. It never
    ///   succeeds later.
    /// - [`Error::Exhausted`] when the budget has no room for the block now.
    /// - [`Error::Refused`] when the system refused memory for the block. The sizes
    ///   the pool gave back to make room stay given back.
    pub fn alloc(&self, len: usize) -> Result<Unique, Error> {
        let index = class_of(len);
        if index >= self.classes.len() {
            return Err(Error::TooLarge {
                requested: len,
                largest: self.largest(),
            });
        }
        let class = &self.classes[index];
        if class.free.get() == NONE {
            self.reclaim();
        }
        let header = if class.free.get() == NONE {
            let size = class_footprint(index);
            let available = self.press(index);
            if size > available {
                return Err(Error::Exhausted {
                    requested: len,
                    available,
                });
            }
            let offset = self.span_start(index) + class.carved.get();
            if let Err(Refused) = self.shared().memory.commit(offset, size) {
                return Err(Error::Refused { requested: len });
            }
            let carved = class.carved.get() + size;
            class.carved.set(carved);
            class.reached.set(class.reached.get().max(carved));
            self.committed.set(self.committed.get() + size);
            // SAFETY: the carved bytes of a class are at most `committed`, which the
            // room check keeps at or below the budget, and a span is at least that.
            let header = unsafe { self.header(offset) };
            let fresh = Header {
                refs: AtomicUsize::new(1),
                next: AtomicUsize::new(NONE),
                offset,
                class: index,
                payload: Track::new(),
            };
            // SAFETY: the block is committed and aligned, and no holder has it.
            unsafe { header.write(fresh) };
            header
        } else {
            // SAFETY: a block on a free list has a header, and only the pool uses it.
            let header = unsafe { self.header(class.free.get()) };
            // SAFETY: as above.
            let free = unsafe { header.as_ref() };
            class.free.set(free.next.load(Relaxed));
            free.refs.store(1, Relaxed);
            free.payload.write();
            header
        };
        class.lent.set(class.lent.get() + 1);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the class check above keeps `len` at or below 2 GiB"
        )]
        Ok(Unique {
            header,
            len: len as u32,
        })
    }

    /// Takes back the blocks that holders dropped, so that `alloc` can use them
    /// again. `alloc` calls it when it has no free block; the owner shard may call it
    /// from its loop to spread the cost.
    pub fn reclaim(&self) {
        let returned = &self.shared().returned;
        if returned.load(Relaxed) != NONE {
            self.take(returned.swap(NONE, Acquire));
        }
    }

    /// Gives the pages of each idle size back to the system.
    ///
    /// A size is idle when no block of it is in use now and no block of it returned
    /// since the previous `purge` call. The shard calls it on its own timer: a size
    /// keeps its pages for one to two timer intervals after its last block returned,
    /// and `alloc` carves it again on demand. It takes back returned blocks first, as
    /// `reclaim` does. Returns the bytes of the budget given back.
    pub fn purge(&self) -> usize {
        self.reclaim();
        let purges = self.purges.get();
        let before = self.committed.get();
        for (index, class) in self.classes.iter().enumerate() {
            if class.idle() && class.returned_at.get() < purges {
                self.release(index);
            }
        }
        self.purges.set(purges + 1);
        before - self.committed.get()
    }

    /// Bytes of the budget in use: the carved range of each size class, which holds
    /// the blocks that holders have and the free blocks that keep their pages. A size
    /// whose blocks are all free gives its range back when another size needs the
    /// budget.
    #[must_use]
    pub fn committed(&self) -> usize {
        self.committed.get()
    }

    /// Gives back the carved range of each class with no block in use until the
    /// budget has room for one block of class `index`. Returns the room. `alloc`
    /// calls it when `index` has no free block.
    ///
    /// It walks nothing when the budget has the room already, and purges nothing
    /// when the idle classes cannot cover the need together. The
    /// classes above `index` go first, smallest first, so one purge covers the
    /// need. Then the classes below it, largest first. The walk starts at `index`
    /// itself: it has no free block, so it has nothing idle, and the range reads
    /// the same for the mutation check either way.
    fn press(&self, index: usize) -> usize {
        let size = class_footprint(index);
        let mut available = self.budget - self.committed.get();
        if available >= size {
            return available;
        }
        let spare: usize = self
            .classes
            .iter()
            .filter(|class| class.idle())
            .map(|class| class.carved.get())
            .sum();
        if available + spare < size {
            return available;
        }
        let above = index..self.classes.len();
        let below = (0..index).rev();
        for other in above.chain(below) {
            if self.classes[other].idle() {
                available += self.release(other);
                if available >= size {
                    break;
                }
            }
        }
        available
    }

    /// First byte of the span of class `index`.
    fn span_start(&self, index: usize) -> usize {
        HEADER + index * self.span
    }

    /// Gives back the carved range of class `index`, which has no block in use, and
    /// returns the bytes it held. The purge covers `reached`, so pages that an
    /// earlier, longer carve touched go back too.
    fn release(&self, index: usize) -> usize {
        let class = &self.classes[index];
        let carved = class.carved.get();
        self.shared()
            .memory
            .purge(self.span_start(index), class.reached.get());
        class.carved.set(0);
        class.free.set(NONE);
        self.committed.set(self.committed.get() - carved);
        carved
    }

    /// Blocks that holders have, or that wait in `Region::returned`.
    fn lent(&self) -> usize {
        self.classes.iter().map(|class| class.lent.get()).sum()
    }

    fn shared(&self) -> &Region {
        // SAFETY: `new` wrote the region, and it lives until the pool and each block
        // are gone.
        unsafe { self.region.as_ref() }
    }

    /// The header of the block at `offset`.
    ///
    /// # Safety
    ///
    /// `offset` is the offset of a block in the region.
    unsafe fn header(&self, offset: usize) -> NonNull<Header> {
        // SAFETY: the caller keeps the result inside the region.
        unsafe { self.region.cast::<u8>().add(offset) }.cast()
    }

    /// Moves a list of returned blocks to the free lists.
    fn take(&self, mut offset: usize) {
        let purges = self.purges.get();
        while offset != NONE {
            // SAFETY: a returned block has a header. Its last holder gave it up with
            // the store that the swap of `returned` read.
            let header = unsafe { self.header(offset) };
            // SAFETY: as above.
            let header = unsafe { header.as_ref() };
            let next = header.next.load(Relaxed);
            let class = &self.classes[header.class];
            header.next.store(class.free.get(), Relaxed);
            class.free.set(offset);
            class.lent.set(class.lent.get() - 1);
            class.returned_at.set(purges);
            offset = next;
        }
    }
}

#[expect(clippy::missing_fields_in_debug, reason = "no pointer in the output")]
impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("budget", &self.budget)
            .field("committed", &self.committed.get())
            .field("lent", &self.lent())
            .finish()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let region = self.shared();
        self.take(region.returned.swap(CLOSED, Acquire));
        let lent = self.lent();
        region.track.read();
        if region.owed.fetch_add(lent, AcqRel).wrapping_add(lent) == 0 {
            // SAFETY: the pool is gone and no holder has a block.
            unsafe { release(self.region) };
        }
    }
}

/// Frees the memory of a region.
///
/// # Safety
///
/// The pool is gone, each block is back, and no thread uses the region again.
unsafe fn release(region: NonNull<Region>) {
    // SAFETY: the region is valid until `memory` drops.
    let shared = unsafe { region.as_ref() };
    shared.track.write();
    let memory = &raw const shared.memory;
    // SAFETY: the caller is the last user, so the box is read one time.
    drop(unsafe { memory.read() });
}

/// Returns a block that has no holder to its pool, or to the system when the pool is
/// gone.
///
/// # Safety
///
/// `header` is a block from `Pool::alloc` and the caller was its last holder.
unsafe fn give_back(header: NonNull<Header>) {
    // SAFETY: the block is valid until the push below succeeds.
    let block = unsafe { header.as_ref() };
    let offset = block.offset;
    // SAFETY: the block lies `offset` bytes into its region.
    let region = unsafe { header.cast::<u8>().sub(offset) }.cast::<Region>();
    // SAFETY: the region outlives each block that is not back.
    let shared = unsafe { region.as_ref() };
    shared.track.read();
    let mut head = shared.returned.load(Relaxed);
    loop {
        if head == CLOSED {
            if shared.owed.fetch_sub(1, AcqRel) == 1 {
                // SAFETY: the pool is gone and this was the last block.
                unsafe { release(region) };
            }
            return;
        }
        block.next.store(head, Relaxed);
        match shared
            .returned
            .compare_exchange_weak(head, offset, Release, Relaxed)
        {
            Ok(_) => return,
            Err(now) => head = now,
        }
    }
}

/// The first byte of the payload of a block.
///
/// # Safety
///
/// `header` is a block from `Pool::alloc` that a holder still has.
unsafe fn payload(header: NonNull<Header>) -> NonNull<u8> {
    // SAFETY: the payload starts `HEADER` bytes after the header, in the same block.
    unsafe { header.cast::<u8>().add(HEADER) }
}

/// A block with one owner, which may write to it. It moves between threads, and its
/// drop on any thread returns the block to its pool.
pub struct Unique {
    header: NonNull<Header>,
    len: u32,
}

// SAFETY: a `Unique` is the one holder of its block, and the return path is atomic.
unsafe impl Send for Unique {}
// SAFETY: a shared `Unique` only gives shared access to its bytes.
unsafe impl Sync for Unique {}

impl Unique {
    /// Makes the block immutable and shareable. It keeps the same bytes.
    #[must_use]
    pub fn freeze(self) -> Block {
        let unique = ManuallyDrop::new(self);
        Block {
            header: unique.header,
            start: 0,
            len: unique.len,
        }
    }

    fn block(&self) -> &Header {
        // SAFETY: the header is valid while a holder has the block.
        unsafe { self.header.as_ref() }
    }
}

#[expect(clippy::missing_fields_in_debug, reason = "no pointer in the output")]
impl fmt::Debug for Unique {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Unique")
            .field("offset", &self.block().offset)
            .field("len", &self.len)
            .finish()
    }
}

impl Deref for Unique {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.block().payload.read();
        // SAFETY: `self` holds the block.
        let start = unsafe { payload(self.header) };
        // SAFETY: the payload is committed, so it is initialized, and no one writes
        // to it while `self` is borrowed.
        unsafe { slice::from_raw_parts(start.as_ptr(), self.len as usize) }
    }
}

impl DerefMut for Unique {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.block().payload.write();
        // SAFETY: `self` holds the block.
        let start = unsafe { payload(self.header) };
        // SAFETY: the payload is committed, and `self` is its one holder.
        unsafe { slice::from_raw_parts_mut(start.as_ptr(), self.len as usize) }
    }
}

impl Drop for Unique {
    fn drop(&mut self) {
        // SAFETY: a `Unique` is the one holder of its block.
        unsafe { give_back(self.header) };
    }
}

/// An immutable block shared by reference count. Cloning it adds one reference. It
/// moves between threads, and the last drop on any thread returns the block to its
/// pool.
pub struct Block {
    header: NonNull<Header>,
    /// Bytes of the payload before this view. `start + len` never exceeds the length
    /// that `alloc` gave.
    start: u32,
    len: u32,
}

// SAFETY: the bytes are immutable, and the count and the return path are atomic.
unsafe impl Send for Block {}
// SAFETY: as for `Send`.
unsafe impl Sync for Block {}

impl Block {
    /// Returns a block that shares this one's buffer and starts `count` bytes later.
    /// Check the length first when the bytes came from outside.
    ///
    /// # Panics
    ///
    /// If `count` is more than the block's length.
    #[must_use]
    pub fn skip(mut self, count: usize) -> Block {
        let skipped = u32::try_from(count)
            .ok()
            .filter(|&skipped| skipped <= self.len)
            .unwrap_or_else(|| {
                panic!("cannot skip {count} bytes of a block of {} bytes", self.len)
            });
        self.start += skipped;
        self.len -= skipped;
        self
    }

    fn block(&self) -> &Header {
        // SAFETY: the header is valid while a holder has the block.
        unsafe { self.header.as_ref() }
    }
}

#[expect(clippy::missing_fields_in_debug, reason = "no pointer in the output")]
impl fmt::Debug for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Block")
            .field("offset", &self.block().offset)
            .field("start", &self.start)
            .field("len", &self.len)
            .finish()
    }
}

impl Clone for Block {
    fn clone(&self) -> Self {
        let before = self.block().refs.fetch_add(1, Relaxed);
        assert!(before < usize::MAX / 2, "a block has too many holders");
        Self {
            header: self.header,
            start: self.start,
            len: self.len,
        }
    }
}

impl Deref for Block {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.block().payload.read();
        // SAFETY: `self` holds the block.
        let first = unsafe { payload(self.header) };
        // SAFETY: `start + len` is at most the length `alloc` gave (field invariant),
        // so the add stays in the block.
        let start = unsafe { first.add(self.start as usize) };
        // SAFETY: the payload is committed, and no one writes to a frozen block.
        unsafe { slice::from_raw_parts(start.as_ptr(), self.len as usize) }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        if self.block().refs.fetch_sub(1, AcqRel) == 1 {
            // SAFETY: the count reached 0, so this was the last holder.
            unsafe { give_back(self.header) };
        }
    }
}

/// An error from a [`Pool`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The budget has no room for the request now. A request after a block frees may
    /// succeed.
    Exhausted {
        /// Bytes asked for.
        requested: usize,
        /// Bytes of the budget not committed. A block of `requested` bytes needs
        /// more.
        available: usize,
    },
    /// The system refused memory that the budget has room for. A request may succeed
    /// when the system has memory again.
    Refused {
        /// Bytes asked for.
        requested: usize,
    },
    /// No block of the pool is large enough. The request can never succeed.
    TooLarge {
        /// Bytes asked for.
        requested: usize,
        /// The most bytes one block of the pool holds.
        largest: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted {
                requested,
                available,
            } => write!(
                f,
                "pool is full: asked for {requested} bytes, {available} bytes free"
            ),
            Self::Refused { requested } => write!(
                f,
                "the system refused memory for a block of {requested} bytes"
            ),
            Self::TooLarge { requested, largest } => write!(
                f,
                "block of {requested} bytes is above the largest block of {largest} \
                 bytes"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Pools over heap memory for the tests and the models.
#[cfg(test)]
mod fixture {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A call a pool made on its memory.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Call {
        Commit { offset: usize, len: usize },
        Purge { offset: usize, len: usize },
    }

    /// What a pool did with its memory.
    #[derive(Default)]
    pub(crate) struct Watch {
        pub(crate) drops: AtomicUsize,
        /// The system has no memory: each `commit` refuses.
        pub(crate) refusing: AtomicBool,
        calls: Mutex<Vec<Call>>,
    }

    impl Watch {
        #[cfg(not(loom))]
        pub(crate) fn calls(&self) -> Vec<Call> {
            self.calls.lock().expect("no test panicked").clone()
        }

        fn record(&self, call: Call) {
            self.calls.lock().expect("no test panicked").push(call);
        }
    }

    /// Heap memory that records its calls and counts its drops.
    pub(crate) struct Watched {
        heap: Heap,
        watch: Arc<Watch>,
    }

    // SAFETY: each method is the one of `Heap`, or a `commit` that refuses.
    unsafe impl Memory for Watched {
        fn base(&self) -> NonNull<u8> {
            self.heap.base()
        }

        fn len(&self) -> usize {
            self.heap.len()
        }

        fn commit(&self, offset: usize, len: usize) -> Result<(), Refused> {
            self.watch.record(Call::Commit { offset, len });
            if self.watch.refusing.load(Relaxed) {
                return Err(Refused);
            }
            self.heap.commit(offset, len)
        }

        fn purge(&self, offset: usize, len: usize) {
            self.watch.record(Call::Purge { offset, len });
            self.heap.purge(offset, len);
        }
    }

    impl Drop for Watched {
        fn drop(&mut self) {
            self.watch.drops.fetch_add(1, Relaxed);
        }
    }

    pub(crate) fn create_pool(budget: usize) -> Pool {
        let config = Config { budget };
        let heap = Heap::new(config.reservation());
        Pool::new(config, heap)
    }

    /// Memory with pages, to check what stays resident.
    #[cfg(not(loom))]
    pub(crate) mod paged {
        use std::ops::Range;

        use super::*;

        /// Bytes per page.
        pub(crate) const PAGE: usize = 128;

        /// Which pages are resident.
        pub(crate) type Pages = Arc<Mutex<Vec<bool>>>;

        /// Heap memory with a set of resident pages: the pages of the first
        /// [`ALIGN`] bytes start resident, `commit` rounds out to [`PAGE`], and
        /// `purge` rounds in, as an OS mapping does.
        pub(crate) struct Paged {
            heap: Heap,
            pages: Pages,
        }

        // SAFETY: each method is the one of `Heap`.
        unsafe impl Memory for Paged {
            fn base(&self) -> NonNull<u8> {
                self.heap.base()
            }

            fn len(&self) -> usize {
                self.heap.len()
            }

            fn commit(&self, offset: usize, len: usize) -> Result<(), Refused> {
                self.mark(offset / PAGE..(offset + len).div_ceil(PAGE), true);
                self.heap.commit(offset, len)
            }

            fn purge(&self, offset: usize, len: usize) {
                self.mark(offset.div_ceil(PAGE)..(offset + len) / PAGE, false);
                self.heap.purge(offset, len);
            }
        }

        impl Paged {
            fn mark(&self, pages: Range<usize>, resident: bool) {
                let mut marks = self.pages.lock().expect("no test panicked");
                for page in pages {
                    marks[page] = resident;
                }
            }
        }

        pub(crate) fn create_paged_pool(budget: usize) -> (Pool, Pages) {
            let config = Config { budget };
            let len = config.reservation();
            let mut marks = vec![false; len.div_ceil(PAGE)];
            marks[..ALIGN.div_ceil(PAGE)].fill(true);
            let pages = Arc::new(Mutex::new(marks));
            let memory = Paged {
                heap: Heap::new(len),
                pages: Arc::clone(&pages),
            };
            (Pool::new(config, memory), pages)
        }

        /// Resident bytes among the pages in `range`.
        pub(crate) fn resident(pages: &Pages, range: Range<usize>) -> usize {
            let pages = pages.lock().expect("no test panicked");
            range.filter(|&page| pages[page]).count() * PAGE
        }
    }

    pub(crate) fn create_watched_pool(budget: usize) -> (Pool, Arc<Watch>) {
        let config = Config { budget };
        let watch = Arc::new(Watch::default());
        let memory = Watched {
            heap: Heap::new(config.reservation()),
            watch: Arc::clone(&watch),
        };
        (Pool::new(config, memory), watch)
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::thread;

    use proptest::prelude::*;

    use super::fixture::{create_pool, create_watched_pool};
    use super::*;

    fn exhausted(requested: usize, available: usize) -> Error {
        Error::Exhausted {
            requested,
            available,
        }
    }

    fn refused(requested: usize) -> Error {
        Error::Refused { requested }
    }

    fn too_large(requested: usize, largest: usize) -> Error {
        Error::TooLarge { requested, largest }
    }

    /// Heap memory with a `base` that is off by 8 bytes.
    struct Misaligned(Heap);

    // SAFETY: the range is 8 bytes shorter than the heap, and inside it.
    unsafe impl Memory for Misaligned {
        fn base(&self) -> NonNull<u8> {
            // SAFETY: the heap has more than 8 bytes.
            unsafe { self.0.base().add(8) }
        }

        fn len(&self) -> usize {
            self.0.len() - 8
        }

        fn commit(&self, _offset: usize, _len: usize) -> Result<(), Refused> {
            Ok(())
        }

        fn purge(&self, _offset: usize, _len: usize) {}
    }

    mod reservation {
        use super::*;

        #[test]
        fn is_one_span_for_each_class_and_a_header() {
            let reservation = |budget| Config { budget }.reservation();
            assert_eq!(reservation(0), 64);
            assert_eq!(reservation(127), 64);
            assert_eq!(reservation(128), 64 + 128);
            assert_eq!(reservation(200), 64 + 2 * 256);
            assert_eq!(reservation(64 + 319), 64 + 4 * 384);
            assert_eq!(reservation(64 + 320), 64 + 5 * 384);
            assert_eq!(reservation(1 << 16), 64 + 35 * (1 << 16));
        }

        #[test]
        #[cfg(target_pointer_width = "64")]
        fn stops_at_a_payload_of_two_gibibytes() {
            assert_eq!(class_payload(CLASSES_MAX - 1), 1 << 31);
            assert_eq!(class_of(1 << 31), CLASSES_MAX - 1);
            assert_eq!(class_of((1 << 31) + 1), CLASSES_MAX);
            assert_eq!(classes((1 << 31) + 63), CLASSES_MAX - 1);
            assert_eq!(classes((1 << 31) + 64), CLASSES_MAX);
            assert_eq!(classes(1 << 40), CLASSES_MAX);
            assert_eq!(classes(usize::MAX), CLASSES_MAX);
        }

        #[test]
        #[should_panic(expected = "pool budget 18446744073709551615 is too large")]
        fn panics_when_it_does_not_fit_in_a_usize() {
            assert_eq!(Config { budget: usize::MAX }.reservation(), 0);
        }
    }

    mod classes {
        use super::*;

        #[test]
        fn are_four_per_doubling_above_256_bytes() {
            let payloads: Vec<_> = (0..12).map(class_payload).collect();
            assert_eq!(
                payloads,
                [64, 128, 192, 256, 320, 384, 448, 512, 640, 768, 896, 1024]
            );
        }

        #[test]
        fn are_tight_and_in_order() {
            for index in 0..CLASSES_MAX {
                let payload = class_payload(index);
                assert_eq!(payload % ALIGN, 0, "class {index}");
                assert_eq!(class_of(payload), index, "class {index}");
                assert_eq!(class_of(payload + 1), index + 1, "class {index}");
                if index > 0 {
                    assert!(class_payload(index - 1) < payload, "class {index}");
                }
            }
        }
    }

    mod footprint {
        use super::*;

        const FRAME: usize = footprint(1000);

        #[test]
        fn is_the_header_and_the_class_payload() {
            assert_eq!(FRAME, 64 + 1024);
            assert_eq!(footprint(0), 128);
            assert_eq!(footprint(1), 128);
            assert_eq!(footprint(64), 128);
            assert_eq!(footprint(65), 192);
            assert_eq!(footprint(128), 192);
            assert_eq!(footprint(129), 256);
            assert_eq!(footprint(256), 320);
            assert_eq!(footprint(257), 384);
            assert_eq!(footprint(512), 576);
            assert_eq!(footprint(513), 704);
            assert_eq!(footprint(1025), 1344);
        }

        #[test]
        fn wastes_at_most_a_quarter_on_the_frames_from_the_issue() {
            assert_eq!(footprint(131_256), 64 + 163_840);
            assert_eq!(footprint(800_000), 64 + 917_504);
        }

        #[test]
        #[should_panic(expected = "no size class holds that many bytes")]
        fn panics_above_the_largest_payload() {
            assert_eq!(footprint((1 << 31) + 1), 0);
        }

        #[test]
        #[should_panic(expected = "no size class holds that many bytes")]
        fn panics_at_the_largest_length() {
            assert_eq!(footprint(usize::MAX), 0);
        }

        proptest! {
            #![proptest_config(cases())]

            #[test]
            fn wastes_at_most_a_quarter_of_the_length(len in 0..=1_usize << 31) {
                let payload = footprint(len) - HEADER;
                prop_assert!(payload >= len);
                prop_assert_eq!(payload % ALIGN, 0);
                prop_assert!(payload - len <= ALIGN.max(len / 4));
            }
        }
    }

    mod new {
        use super::*;

        #[test]
        #[should_panic(expected = "pool needs 192 bytes of memory, got 128")]
        fn panics_when_the_memory_is_too_short() {
            drop(Pool::new(Config { budget: 128 }, Heap::new(128)));
        }

        #[test]
        #[should_panic(expected = "pool memory must be aligned to 64 bytes")]
        fn panics_when_the_memory_is_misaligned() {
            let memory = Misaligned(Heap::new(256));
            drop(Pool::new(Config { budget: 128 }, memory));
        }
    }

    mod heap {
        use super::*;

        #[test]
        #[should_panic(expected = "heap memory must be more than 0 bytes")]
        fn panics_when_empty() {
            drop(Heap::new(0));
        }

        #[test]
        #[should_panic(
            expected = "heap memory of 18446744073709551615 bytes is too large"
        )]
        fn panics_when_too_large() {
            drop(Heap::new(usize::MAX));
        }

        #[test]
        fn prints_its_length() {
            assert_eq!(format!("{:?}", Heap::new(64)), "Heap { len: 64 }");
        }
    }

    fn cases() -> ProptestConfig {
        let mut config = ProptestConfig::default();
        if cfg!(miri) {
            config.cases = 16;
            config.failure_persistence = None;
        }
        config
    }

    mod alloc {
        use super::*;

        proptest! {
            #![proptest_config(cases())]

            #[test]
            fn gives_aligned_blocks_that_do_not_overlap(
                lens in prop::collection::vec(0_usize..=4096, 1..12),
            ) {
                let pool = create_pool(1 << 16);
                let mut blocks = Vec::new();
                for (len, fill) in lens.iter().copied().zip(1_u8..) {
                    let before = pool.committed();
                    let mut block = pool.alloc(len).expect("the budget has room");
                    prop_assert_eq!(pool.committed() - before, footprint(len));
                    prop_assert_eq!(block.len(), len);
                    prop_assert_eq!(block.as_ptr().addr() % ALIGN, 0);
                    block.fill(fill);
                    blocks.push((block, fill));
                }
                for (block, fill) in &blocks {
                    prop_assert!(block.iter().all(|byte| byte == fill));
                }
                prop_assert!(pool.committed() <= 1 << 16);
            }
        }

        #[test]
        fn fails_when_the_budget_has_no_room() {
            let pool = create_pool(256);
            let first = pool.alloc(64).expect("the budget has room");
            assert_eq!(pool.committed(), 128);
            let error = pool.alloc(128).expect_err("192 bytes do not fit in 128");
            assert_eq!(error, exhausted(128, 128));
            assert_eq!(
                error.to_string(),
                "pool is full: asked for 128 bytes, 128 bytes free"
            );
            drop(first);
        }

        #[test]
        fn fails_while_the_system_refuses_memory() {
            let (pool, watch) = create_watched_pool(256);
            watch.refusing.store(true, Relaxed);
            let error = pool.alloc(10).expect_err("the system refuses memory");
            assert_eq!(error, refused(10));
            assert_eq!(
                error.to_string(),
                "the system refused memory for a block of 10 bytes"
            );
            assert_eq!(pool.committed(), 0);
            watch.refusing.store(false, Relaxed);
            let block = pool.alloc(10).expect("the system has memory again");
            assert_eq!(block.len(), 10);
            assert_eq!(pool.committed(), 128);
        }

        #[test]
        fn fails_for_good_when_the_length_is_above_each_class() {
            let pool = create_pool(256);
            assert_eq!(pool.largest(), 192);
            let error = pool.alloc(193).expect_err("193 bytes do not fit in 192");
            assert_eq!(error, too_large(193, 192));
            assert_eq!(
                error.to_string(),
                "block of 193 bytes is above the largest block of 192 bytes"
            );
            assert_eq!(
                pool.alloc(usize::MAX).err(),
                Some(too_large(usize::MAX, 192))
            );
        }

        #[test]
        fn fails_for_good_when_the_budget_is_below_one_block() {
            let pool = create_pool(127);
            assert_eq!(pool.largest(), 0);
            assert_eq!(pool.alloc(0).err(), Some(too_large(0, 0)));
        }

        #[test]
        fn uses_a_dropped_block_again_with_its_old_bytes() {
            let pool = create_pool(128);
            let mut first = pool.alloc(64).expect("the budget has room");
            first.fill(0xAA);
            let address = first.as_ptr();
            assert_eq!(pool.alloc(64).err(), Some(exhausted(64, 0)));
            drop(first);
            let second = pool.alloc(4).expect("the block is back");
            assert_eq!(second.as_ptr(), address);
            assert_eq!(&*second, &[0xAA; 4]);
            assert_eq!(pool.committed(), 128);
        }
    }

    /// `alloc` moves the budget of an idle size to another size when it has to.
    mod pressure {
        use super::fixture::Call::{Commit, Purge};
        use super::*;

        #[test]
        fn carves_without_a_purge_when_the_budget_has_the_room() {
            let (pool, watch) = create_watched_pool(320);
            drop(pool.alloc(64).expect("the budget has room"));
            pool.reclaim();
            let block = pool.alloc(128).expect("the budget has room");
            assert_eq!(pool.committed(), 320, "the idle 64-byte class stays");
            assert_eq!(
                watch.calls().last(),
                Some(&Commit {
                    offset: 384,
                    len: 192
                })
            );
            drop(block);
        }

        #[test]
        fn keeps_its_counts_when_the_system_refuses_after_a_purge() {
            let (pool, watch) = create_watched_pool(256);
            drop(pool.alloc(64).expect("the budget has room"));
            pool.reclaim();
            watch.refusing.store(true, Relaxed);
            let error = pool.alloc(128).expect_err("the system refuses memory");
            assert_eq!(error, refused(128));
            assert_eq!(pool.committed(), 0, "the idle class went back first");
            watch.refusing.store(false, Relaxed);
            let block = pool.alloc(128).expect("the system has memory again");
            assert_eq!(pool.committed(), 192);
            let carve = Commit {
                offset: 320,
                len: 192,
            };
            assert_eq!(
                watch.calls(),
                [
                    Commit {
                        offset: 64,
                        len: 128
                    },
                    Purge {
                        offset: 64,
                        len: 128
                    },
                    carve,
                    carve,
                ],
                "the refused carve leaves its class to start at the same offset"
            );
            drop(block);
        }

        #[test]
        fn gives_back_only_what_it_carved_after_a_refusal() {
            let (pool, watch) = create_watched_pool(256);
            let held = pool.alloc(64).expect("the budget has room");
            watch.refusing.store(true, Relaxed);
            let error = pool.alloc(64).expect_err("the system refuses memory");
            assert_eq!(error, refused(64));
            drop(held);
            assert_eq!(pool.purge(), 0, "the first purge marks the idle class");
            assert_eq!(pool.purge(), 128);
            assert_eq!(pool.committed(), 0);
            assert_eq!(
                watch.calls().last(),
                Some(&Purge {
                    offset: 64,
                    len: 128
                }),
                "the refused carve is not in the range that goes back"
            );
        }

        #[test]
        fn moves_the_budget_of_a_free_block_to_another_class() {
            let pool = create_pool(256);
            drop(pool.alloc(64).expect("the budget has room"));
            pool.reclaim();
            assert_eq!(pool.committed(), 128);
            let block = pool.alloc(128).expect("the free block gives its budget");
            assert_eq!(block.len(), 128);
            assert_eq!(pool.committed(), 192);
        }

        #[test]
        fn gives_back_the_range_of_a_class_and_carves_it_again() {
            let (pool, watch) = create_watched_pool(256);
            let first = pool.alloc(64).expect("the budget has room");
            let address = first.as_ptr();
            drop(first);
            let second = pool.alloc(128).expect("the free block gives its budget");
            drop(second);
            let third = pool.alloc(64).expect("the free block gives its budget");
            assert_eq!(third.as_ptr(), address, "the class starts over at its span");
            assert_eq!(pool.committed(), 128);
            assert_eq!(
                watch.calls(),
                [
                    Commit {
                        offset: 64,
                        len: 128
                    },
                    Purge {
                        offset: 64,
                        len: 128
                    },
                    Commit {
                        offset: 320,
                        len: 192
                    },
                    Purge {
                        offset: 320,
                        len: 192
                    },
                    Commit {
                        offset: 64,
                        len: 128
                    },
                ],
                "a purge covers the carved range of the class, headers included"
            );
        }

        #[test]
        fn purges_nothing_when_the_room_equals_the_need() {
            let (pool, watch) = create_watched_pool(320);
            drop(pool.alloc(64).expect("the budget has room"));
            pool.reclaim();
            let block = pool.alloc(128).expect("the room is exact");
            assert_eq!(pool.committed(), 320);
            assert!(
                !watch
                    .calls()
                    .iter()
                    .any(|call| matches!(call, Purge { .. })),
                "the free block keeps its pages"
            );
            drop(block);
        }

        #[test]
        fn keeps_the_budget_of_a_class_with_a_block_in_use() {
            let (pool, watch) = create_watched_pool(448);
            let blocks: Vec<_> = (0..3)
                .map(|_| pool.alloc(64).expect("the budget has room"))
                .collect();
            let held = blocks.into_iter().next();
            pool.reclaim();
            assert_eq!(pool.committed(), 384);
            assert_eq!(pool.alloc(128).err(), Some(exhausted(128, 64)));
            assert_eq!(pool.committed(), 384, "the class keeps its free blocks");
            assert!(
                !watch
                    .calls()
                    .iter()
                    .any(|call| matches!(call, Purge { .. }))
            );
            drop(held);
        }

        #[test]
        fn purges_the_class_above_first() {
            let (pool, watch) = create_watched_pool(640);
            let held = pool.alloc(128).expect("the budget has room");
            drop(pool.alloc(64).expect("the budget has room"));
            drop(pool.alloc(256).expect("the budget has room"));
            pool.reclaim();
            assert_eq!(pool.committed(), 640);
            let second = pool.alloc(128).expect("the idle 256-byte class gives room");
            assert_eq!(pool.committed(), 512);
            let calls = watch.calls();
            assert_eq!(
                calls[calls.len() - 2..],
                [
                    Purge {
                        offset: 1984,
                        len: 320
                    },
                    Commit {
                        offset: 896,
                        len: 192
                    },
                ],
                "the class above goes first, and the idle 64-byte class stays"
            );
            drop((held, second));
        }

        #[test]
        fn purges_the_smallest_idle_class_above_first() {
            let (pool, watch) = create_watched_pool(1216);
            let held = pool.alloc(128).expect("the budget has room");
            drop(pool.alloc(256).expect("the budget has room"));
            drop(pool.alloc(512).expect("the budget has room"));
            pool.reclaim();
            assert_eq!(pool.committed(), 1088);
            let second = pool.alloc(128).expect("the idle 256-byte class gives room");
            assert_eq!(pool.committed(), 960);
            let calls = watch.calls();
            assert_eq!(
                calls[calls.len() - 2..],
                [
                    Purge {
                        offset: 3712,
                        len: 320
                    },
                    Commit {
                        offset: 1472,
                        len: 192
                    },
                ],
                "the smaller class above goes first, and the 512-byte class stays"
            );
            drop((held, second));
        }

        #[test]
        fn purges_the_largest_idle_class_below_first() {
            let (pool, watch) = create_watched_pool(576);
            drop(pool.alloc(64).expect("the budget has room"));
            drop(pool.alloc(128).expect("the budget has room"));
            pool.reclaim();
            assert_eq!(pool.committed(), 320);
            let large = pool.alloc(256).expect("the idle 128-byte class gives room");
            assert_eq!(pool.committed(), 448);
            let calls = watch.calls();
            assert_eq!(
                calls[calls.len() - 2..],
                [
                    Purge {
                        offset: 640,
                        len: 192
                    },
                    Commit {
                        offset: 1792,
                        len: 320
                    },
                ],
                "the largest idle class below goes first, and the 64-byte class stays"
            );
            drop(large);
        }

        #[test]
        fn purges_nothing_when_the_idle_classes_cannot_cover_the_need() {
            let (pool, watch) = create_watched_pool(384);
            drop(pool.alloc(64).expect("the budget has room"));
            let held = pool.alloc(128).expect("the budget has room");
            pool.reclaim();
            assert_eq!(pool.committed(), 320);
            assert_eq!(pool.alloc(256).err(), Some(exhausted(256, 64)));
            assert_eq!(pool.committed(), 320, "the idle class kept its budget");
            assert!(
                !watch
                    .calls()
                    .iter()
                    .any(|call| matches!(call, Purge { .. })),
                "a purge that cannot serve the alloc"
            );
            drop(held);
        }

        #[test]
        fn takes_the_budget_when_the_idle_classes_cover_the_need_exactly() {
            let pool = create_pool(384);
            drop(pool.alloc(64).expect("the budget has room"));
            let held = pool.alloc(128).expect("the budget has room");
            pool.reclaim();
            let second = pool.alloc(128).expect("the idle 64-byte class gives room");
            assert_eq!(pool.committed(), 384);
            drop((held, second));
        }

        #[test]
        fn serves_small_blocks_after_a_peer_freed_large_ones() {
            let pool = create_pool(2 * footprint(1024));
            let first = pool.alloc(1024).expect("the budget has room");
            let second = pool.alloc(1024).expect("the budget has room");
            assert_eq!(pool.alloc(10).err(), Some(exhausted(10, 0)));
            drop((first, second));
            let small = pool
                .alloc(10)
                .expect("the freed large blocks give their budget");
            assert_eq!(small.len(), 10);
            assert_eq!(pool.committed(), footprint(10));
        }

        #[test]
        fn keeps_resident_memory_within_two_partial_pages_per_class() {
            use super::fixture::paged::{PAGE, create_paged_pool, resident};

            const BUDGET: usize = 4096;
            let (pool, pages) = create_paged_pool(BUDGET);
            let classes = pool.classes.len();
            let span = pool.span;
            let held = pool.alloc(1024).expect("the budget has room");
            // Each round carves the 64-byte class to a shorter end than the round
            // before, drops it all, and lets a 2048-byte block press it out.
            for count in (8..=23).rev() {
                let small: Vec<_> = (0..count)
                    .map(|_| pool.alloc(64).expect("the budget has room"))
                    .collect();
                drop(small);
                pool.reclaim();
                let large =
                    pool.alloc(2048).expect("the idle 64-byte class gives room");
                assert_eq!(pool.classes[0].carved.get(), 0, "the class was purged");
                drop(large);
                pool.reclaim();
            }
            drop(held);
            pool.reclaim();
            let class_pages = |index: usize| {
                let start = HEADER + index * span;
                resident(&pages, start / PAGE..(start + span).div_ceil(PAGE))
            };
            let first = class_pages(0);
            let total: usize = (0..classes).map(class_pages).sum();
            assert_eq!(pool.committed(), 1088 + 2112);
            assert!(
                total <= pool.committed() + 2 * PAGE * classes,
                "resident {total} bytes pass committed {} by more than two partial \
                 pages per class ({})",
                pool.committed(),
                2 * PAGE * classes
            );
            assert!(
                first <= 2 * PAGE,
                "the idle 64-byte class keeps {first} resident bytes and nothing carved"
            );
        }

        proptest! {
            #![proptest_config(cases())]

            /// Each byte of the budget that no size in use holds is room for a block,
            /// and `committed` is the carved range of each size. A refused commit
            /// fails only an alloc that fits, and keeps both. No block is served
            /// over a refused commit.
            #[test]
            fn serves_each_alloc_that_fits_in_the_room_idle_sizes_leave(
                steps in prop::collection::vec(
                    (0_usize..=1024, prop::option::of(0_usize..8)),
                    1..24,
                ),
                // Drawn after `steps`, so a saved failure replays the same steps.
                refusals in prop::collection::vec(prop::bool::weighted(0.2), 24),
            ) {
                const BUDGET: usize = 4096;
                let (pool, watch) = create_watched_pool(BUDGET);
                let mut blocks: Vec<(Unique, u8)> = Vec::new();
                let mut lent = 0;
                let steps = steps.into_iter().zip(refusals).zip(1_u8..);
                for (((len, dropped), refusing), fill) in steps {
                    watch.refusing.store(refusing, Relaxed);
                    let calls = watch.calls().len();
                    match pool.alloc(len) {
                        Ok(mut block) => {
                            let commits = watch.calls()[calls..]
                                .iter()
                                .filter(|call| matches!(call, Commit { .. }))
                                .count();
                            prop_assert!(
                                !refusing || commits == 0,
                                "served {len} bytes after a refused commit"
                            );
                            block.fill(fill);
                            lent += footprint(len);
                            blocks.push((block, fill));
                        }
                        Err(error) => {
                            let held: usize = pool
                                .classes
                                .iter()
                                .filter(|class| class.lent.get() != 0)
                                .map(|class| class.carved.get())
                                .sum();
                            let room = BUDGET - held;
                            let need = footprint(len);
                            if need > room {
                                let available = BUDGET - pool.committed();
                                prop_assert_eq!(error, exhausted(len, available));
                            } else {
                                prop_assert!(refusing, "{error} with {room} room");
                                prop_assert_eq!(error, refused(len));
                            }
                        }
                    }
                    let carved: usize =
                        pool.classes.iter().map(|class| class.carved.get()).sum();
                    prop_assert_eq!(pool.committed(), carved);
                    prop_assert!(pool.committed() <= BUDGET);
                    prop_assert!(pool.committed() >= lent);
                    if let Some(index) = dropped.filter(|_| !blocks.is_empty()) {
                        let (block, _) = blocks.swap_remove(index % blocks.len());
                        lent -= footprint(block.len());
                    }
                }
                for (block, fill) in &blocks {
                    prop_assert!(block.iter().all(|byte| byte == fill));
                }
            }
        }
    }

    mod purge {
        use super::fixture::Call::{Commit, Purge};
        use super::fixture::{Call, Watch};
        use super::*;

        fn purges(watch: &Watch) -> Vec<Call> {
            watch
                .calls()
                .into_iter()
                .filter(|call| matches!(call, Purge { .. }))
                .collect()
        }

        #[test]
        fn releases_a_size_idle_across_two_purge_calls() {
            let (pool, watch) = create_watched_pool(256);
            drop(pool.alloc(64).expect("the budget has room"));
            pool.reclaim();
            assert_eq!(pool.purge(), 0, "the size returned a block this interval");
            assert_eq!(pool.committed(), 128);
            assert_eq!(purges(&watch), []);
            assert_eq!(pool.purge(), 128, "the size stayed idle for an interval");
            assert_eq!(pool.committed(), 0);
            assert_eq!(
                purges(&watch),
                [Purge {
                    offset: 64,
                    len: 128
                }]
            );
        }

        #[test]
        fn keeps_a_size_that_returned_a_block_since_the_last_purge() {
            let (pool, watch) = create_watched_pool(256);
            drop(pool.alloc(64).expect("the budget has room"));
            pool.reclaim();
            assert_eq!(pool.purge(), 0);
            drop(pool.alloc(64).expect("the free block serves"));
            pool.reclaim();
            assert_eq!(pool.purge(), 0, "a block returned since the last call");
            assert_eq!(purges(&watch), []);
            assert_eq!(pool.purge(), 128);
            assert_eq!(pool.committed(), 0);
        }

        #[test]
        fn releases_a_size_whose_block_returned_after_a_call() {
            let pool = create_pool(256);
            let held = pool.alloc(64).expect("the budget has room");
            assert_eq!(pool.purge(), 0);
            assert_eq!(pool.purge(), 0, "the block is in use");
            drop(held);
            pool.reclaim();
            assert_eq!(pool.purge(), 0, "the block returned this interval");
            assert_eq!(pool.purge(), 128);
            assert_eq!(pool.committed(), 0);
        }

        #[test]
        fn keeps_a_size_with_a_block_in_use() {
            let (pool, watch) = create_watched_pool(256);
            let held = pool.alloc(64).expect("the budget has room");
            drop(pool.alloc(64).expect("the budget has room"));
            pool.reclaim();
            assert_eq!(pool.purge(), 0);
            assert_eq!(pool.purge(), 0, "a block is in use");
            assert_eq!(pool.committed(), 256);
            assert_eq!(purges(&watch), []);
            drop(held);
        }

        #[test]
        #[expect(
            clippy::disallowed_methods,
            reason = "a thread test; `block` has no `env`"
        )]
        fn takes_back_returned_blocks_first() {
            let pool = create_pool(256);
            let block = pool.alloc(64).expect("the budget has room");
            thread::scope(|scope| {
                scope.spawn(move || std::mem::drop(block));
            });
            assert_eq!(pool.purge(), 0, "the block came back in this call");
            assert_eq!(pool.purge(), 128);
            assert_eq!(pool.committed(), 0);
        }

        #[test]
        fn releases_the_reached_range() {
            let (pool, watch) = create_watched_pool(1472);
            let blocks: Vec<_> = (0..11)
                .map(|_| pool.alloc(64).expect("the budget has room"))
                .collect();
            drop(blocks);
            pool.reclaim();
            assert_eq!(pool.purge(), 0);
            assert_eq!(pool.purge(), 1408);
            drop(pool.alloc(64).expect("the budget has room"));
            pool.reclaim();
            assert_eq!(pool.purge(), 0);
            assert_eq!(pool.purge(), 128);
            assert_eq!(
                purges(&watch),
                [
                    Purge {
                        offset: 64,
                        len: 1408
                    },
                    Purge {
                        offset: 64,
                        len: 1408
                    },
                ],
                "the second release covers the pages the first carve touched"
            );
        }

        #[test]
        fn carves_a_released_size_again() {
            let (pool, watch) = create_watched_pool(256);
            drop(pool.alloc(64).expect("the budget has room"));
            pool.reclaim();
            pool.purge();
            pool.purge();
            let block = pool.alloc(64).expect("the budget has room");
            assert_eq!(pool.committed(), 128);
            let calls = watch.calls();
            assert_eq!(
                calls[calls.len() - 1],
                Commit {
                    offset: 64,
                    len: 128
                },
                "the carve starts at the span again"
            );
            drop(block);
        }

        proptest! {
            #![proptest_config(cases())]

            /// Two purge calls with no alloc between leave only the sizes in use
            /// committed, and a purge gives back what it takes off `committed`.
            #[test]
            fn leaves_only_the_sizes_in_use_after_two_idle_calls(
                steps in proptest::collection::vec(
                    (1_usize..=1024, proptest::option::of(0_usize..8), any::<bool>()),
                    1..32,
                ),
            ) {
                const BUDGET: usize = 4096;
                let pool = create_pool(BUDGET);
                let mut blocks = Vec::new();
                for (len, drop_index, purge) in steps {
                    if let Ok(block) = pool.alloc(len) {
                        blocks.push(block);
                    }
                    if let Some(index) = drop_index.filter(|i| *i < blocks.len()) {
                        blocks.swap_remove(index);
                    }
                    if purge {
                        let before = pool.committed();
                        let released = pool.purge();
                        prop_assert_eq!(before - pool.committed(), released);
                    }
                    let carved: usize =
                        pool.classes.iter().map(|class| class.carved.get()).sum();
                    prop_assert_eq!(pool.committed(), carved);
                }
                pool.purge();
                pool.purge();
                let held: usize = pool
                    .classes
                    .iter()
                    .filter(|class| class.lent.get() != 0)
                    .map(|class| class.carved.get())
                    .sum();
                prop_assert_eq!(pool.committed(), held);
            }
        }
    }

    mod debug {
        use super::*;

        #[test]
        fn prints_offsets_and_counts_and_no_address() {
            let pool = create_pool(256);
            let unique = pool.alloc(3).expect("the budget has room");
            assert_eq!(format!("{unique:?}"), "Unique { offset: 64, len: 3 }");
            let block = unique.freeze();
            assert_eq!(
                format!("{block:?}"),
                "Block { offset: 64, start: 0, len: 3 }"
            );
            let block = block.skip(1);
            assert_eq!(
                format!("{block:?}"),
                "Block { offset: 64, start: 1, len: 2 }"
            );
            assert_eq!(
                format!("{pool:?}"),
                "Pool { budget: 256, committed: 128, lent: 1 }"
            );
        }
    }

    mod skip {
        use super::*;

        fn create_block(pool: &Pool) -> Block {
            let mut unique = pool.alloc(5).expect("the budget has room");
            unique.copy_from_slice(b"hello");
            unique.freeze()
        }

        #[test]
        fn starts_later_in_the_same_bytes() {
            let pool = create_pool(256);
            let block = create_block(&pool);
            assert_eq!(&*block.clone().skip(0), b"hello");
            assert_eq!(&*block.clone().skip(2), b"llo");
            assert_eq!(&*block.skip(5), b"");
        }

        #[test]
        fn shares_the_count_with_its_clones() {
            let pool = create_pool(256);
            let block = create_block(&pool);
            let rest = block.clone().skip(3);
            assert_eq!(&*rest, b"lo");
            assert_eq!(&*rest.clone().skip(1), b"o");
            drop(block);
            assert_eq!(&*rest, b"lo");
            drop(rest);
            pool.reclaim();
            assert_eq!(
                format!("{pool:?}"),
                "Pool { budget: 256, committed: 128, lent: 0 }"
            );
        }

        #[test]
        #[should_panic(expected = "cannot skip 6 bytes of a block of 5 bytes")]
        fn panics_past_the_end() {
            let pool = create_pool(256);
            drop(create_block(&pool).skip(6));
        }

        #[test]
        #[cfg(target_pointer_width = "64")]
        #[should_panic(expected = "cannot skip 4294967296 bytes of a block of 5 bytes")]
        fn panics_past_a_u32() {
            let pool = create_pool(256);
            drop(create_block(&pool).skip(1 << 32));
        }
    }

    mod clone {
        use super::*;

        /// Puts a forged count back when it drops, so the block returns to its pool.
        struct Reset<'a>(&'a Block);

        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.block().refs.store(1, Relaxed);
            }
        }

        #[test]
        #[should_panic(expected = "a block has too many holders")]
        fn panics_when_the_holders_do_not_fit_in_the_count() {
            let pool = create_pool(256);
            let block = pool.alloc(1).expect("the budget has room").freeze();
            let reset = Reset(&block);
            block.block().refs.store(usize::MAX / 2, Relaxed);
            let _clone = ManuallyDrop::new(block.clone());
            drop(reset);
        }
    }

    mod send {
        use super::*;

        const fn assert_send_and_sync<T: Send + Sync>() {}
        const fn assert_send<T: Send>() {}

        #[test]
        fn blocks_and_the_pool_cross_threads() {
            const { assert_send_and_sync::<Unique>() };
            const { assert_send_and_sync::<Block>() };
            const { assert_send::<Pool>() };
        }
    }

    mod freeze {
        use super::*;

        #[test]
        fn keeps_the_bytes_and_shares_them_with_each_clone() {
            let pool = create_pool(1024);
            let mut unique = pool.alloc(3).expect("the budget has room");
            unique.copy_from_slice(&[1, 2, 3]);
            let block = unique.freeze();
            let clone = block.clone();
            assert_eq!(&*block, &[1, 2, 3]);
            assert_eq!(clone.as_ptr(), block.as_ptr());
        }
    }

    mod drop {
        use super::*;

        #[test]
        #[expect(
            clippy::disallowed_methods,
            reason = "a thread test; `block` has no `env`"
        )]
        fn returns_a_block_one_time_when_each_thread_drops_a_clone() {
            let pool = create_pool(128);
            let rounds = if cfg!(miri) { 20 } else { 2000 };
            for fill in (0..=u8::MAX).cycle().take(rounds) {
                let mut unique = pool.alloc(64).expect("the block is back");
                unique.fill(fill);
                let block = unique.freeze();
                thread::scope(|scope| {
                    for _ in 0..4 {
                        let clone = block.clone();
                        scope.spawn(move || assert_eq!(clone[63], fill));
                    }
                    std::mem::drop(block);
                });
                let again = pool.alloc(64).expect("the block is back");
                assert_eq!(pool.alloc(64).err(), Some(exhausted(64, 0)));
                assert_eq!(pool.committed(), 128);
                std::mem::drop(again);
            }
        }

        #[test]
        #[expect(clippy::disallowed_methods, reason = "a test needs a thread to join")]
        fn hands_a_block_over_to_the_next_writer_with_no_join() {
            let pool = create_pool(128);
            let rounds = if cfg!(miri) { 20 } else { 1000 };
            for fill in (1..=u8::MAX).cycle().take(rounds) {
                let mut unique = pool.alloc(64).expect("the block is back");
                unique.fill(fill);
                let block = unique.freeze();
                let clone = block.clone();
                let reader = thread::spawn(move || {
                    let seen = clone[0];
                    std::mem::drop(clone);
                    seen
                });
                std::mem::drop(block);
                // The reader may still hold the block; the write happens when it is
                // back. Miri checks the runs where it does.
                if let Ok(mut next) = pool.alloc(64) {
                    next.fill(0);
                }
                assert_eq!(reader.join().expect("the reader read"), fill);
            }
        }

        #[test]
        fn frees_the_memory_with_the_pool_when_no_block_is_out() {
            let (pool, watch) = create_watched_pool(1024);
            let block = pool.alloc(64).expect("the budget has room").freeze();
            std::mem::drop(block);
            assert_eq!(watch.drops.load(Relaxed), 0);
            std::mem::drop(pool);
            assert_eq!(watch.drops.load(Relaxed), 1);
        }

        #[test]
        #[expect(
            clippy::disallowed_methods,
            reason = "a thread test; `block` has no `env`"
        )]
        fn keeps_the_memory_until_the_last_block_is_gone() {
            let (pool, watch) = create_watched_pool(1024);
            let mut unique = pool.alloc(8).expect("the budget has room");
            unique.fill(7);
            let block = unique.freeze();
            let clone = block.clone();
            let other = pool.alloc(8).expect("the budget has room");
            std::mem::drop(pool);
            assert_eq!(&*clone, &[7; 8]);
            std::mem::drop(block);
            std::mem::drop(other);
            assert_eq!(watch.drops.load(Relaxed), 0);
            thread::scope(|scope| {
                scope.spawn(move || std::mem::drop(clone));
            });
            assert_eq!(watch.drops.load(Relaxed), 1);
        }
    }
}

#[cfg(test)]
#[cfg(loom)]
mod model {
    use loom::thread;

    use super::fixture::{create_pool, create_watched_pool};
    use super::*;

    #[test]
    fn returns_each_block_one_time_while_the_owner_reclaims() {
        loom::model(|| {
            let pool = create_pool(256);
            let first = pool.alloc(64).expect("the budget has room").freeze();
            let second = pool.alloc(64).expect("the budget has room");
            let clone = first.clone();
            let droppers = [
                thread::spawn(move || drop(first)),
                thread::spawn(move || {
                    drop(clone);
                    drop(second);
                }),
            ];
            pool.reclaim();
            for dropper in droppers {
                dropper.join().expect("the dropper does not panic");
            }
            let blocks = [pool.alloc(64), pool.alloc(64), pool.alloc(64)];
            assert!(
                blocks[0].is_ok() && blocks[1].is_ok(),
                "both blocks are back"
            );
            assert!(blocks[2].is_err(), "no block came back two times");
        });
    }

    #[test]
    fn loses_no_block_that_returns_during_a_reclaim() {
        loom::model(|| {
            let pool = create_pool(256);
            let first = pool.alloc(64).expect("the budget has room");
            let second = pool.alloc(64).expect("the budget has room");
            drop(first);
            let dropper = thread::spawn(move || drop(second));
            pool.reclaim();
            dropper.join().expect("the dropper does not panic");
            let blocks = [pool.alloc(64), pool.alloc(64), pool.alloc(64)];
            assert!(
                blocks[0].is_ok() && blocks[1].is_ok(),
                "both blocks are back"
            );
            assert!(blocks[2].is_err(), "no block came back two times");
        });
    }

    #[test]
    fn hands_the_payload_over_when_a_clone_races_the_last_drop() {
        loom::model(|| {
            let pool = create_pool(128);
            let mut unique = pool.alloc(64).expect("the budget has room");
            unique[0] = 1;
            let block = unique.freeze();
            let clone = block.clone();
            let reader = thread::spawn(move || {
                let seen = clone[0];
                let again = clone.clone();
                drop(clone);
                let seen_again = again[0];
                drop(again);
                (seen, seen_again)
            });
            drop(block);
            if let Ok(mut next) = pool.alloc(64) {
                next[0] = 2;
            }
            let seen = reader.join().expect("the reader read");
            assert_eq!(seen, (1, 1), "the reader saw the old bytes");
        });
    }

    #[test]
    fn frees_the_memory_one_time_when_the_pool_drops_during_the_returns() {
        loom::model(|| {
            let (pool, watch) = create_watched_pool(256);
            let first = pool.alloc(64).expect("the budget has room").freeze();
            let second = pool.alloc(64).expect("the budget has room");
            let clone = first.clone();
            let droppers = [
                thread::spawn(move || drop(first)),
                thread::spawn(move || {
                    drop(clone);
                    drop(second);
                }),
            ];
            drop(pool);
            for dropper in droppers {
                dropper.join().expect("the dropper does not panic");
            }
            assert_eq!(watch.drops.load(Relaxed), 1, "the memory is free one time");
        });
    }
}
