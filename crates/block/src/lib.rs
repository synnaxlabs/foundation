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

use std::cell::Cell;
use std::fmt;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::slice;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};

#[cfg(loom)]
use loom::sync::atomic::AtomicUsize;
#[cfg(not(loom))]
use std::sync::atomic::AtomicUsize;

pub use memory::{Heap, Memory};

/// Byte alignment of every block.
pub const ALIGN: usize = 64;

/// Bytes before the payload of a block, and before the first block of a region.
const HEADER: usize = 64;
/// The end of a list of blocks. No block is at offset 0.
const NONE: usize = 0;
/// In `Region::returned`: the pool is gone.
const CLOSED: usize = usize::MAX;

/// Settings for one [`Pool`].
#[derive(Clone, Debug)]
pub struct Config {
    /// The most bytes the pool may commit at once.
    pub budget: usize,
}

impl Config {
    /// Bytes of address space that a pool with these settings needs from its
    /// [`Memory`]. Each size class can grow to the full budget, so this is many times
    /// the budget.
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

/// How many size classes fit in `budget`. Class `i` has a payload of `64 << i` bytes.
fn classes(budget: usize) -> usize {
    match budget.checked_sub(HEADER) {
        Some(room) if room >= ALIGN => (room / ALIGN).ilog2() as usize + 1,
        _ => 0,
    }
}

/// The smallest class with a payload of at least `len` bytes.
fn class_of(len: usize) -> Option<usize> {
    let payload = len.max(ALIGN).checked_next_power_of_two()?;
    Some((payload.trailing_zeros() - ALIGN.trailing_zeros()) as usize)
}

/// Bytes that one block of class `index` takes.
const fn footprint(index: usize) -> usize {
    HEADER + (ALIGN << index)
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
}

/// The start of each block.
#[repr(C, align(64))]
struct Header {
    refs: AtomicUsize,
    /// The next block in the list that holds this one.
    next: AtomicUsize,
    len: usize,
    /// Bytes from the region to this header.
    offset: usize,
}

const _: () = assert!(
    size_of::<Region>() <= HEADER && size_of::<Header>() <= HEADER,
    "the headers fit in the bytes before a payload"
);

#[derive(Debug, Default)]
struct Class {
    /// Bytes of this class's span that are cut into blocks.
    carved: Cell<usize>,
    free: Cell<usize>,
}

/// A pool of blocks owned by one shard.
///
/// A pool is not `Sync`: only its owner shard allocates from it. Blocks it hands out
/// may move to and drop on any thread, and stay valid after the pool drops.
#[derive(Debug)]
pub struct Pool {
    region: NonNull<Region>,
    budget: usize,
    span: usize,
    committed: Cell<usize>,
    /// Blocks handed out and not yet taken back.
    lent: Cell<usize>,
    classes: Box<[Class]>,
}

// SAFETY: a pool owns its region. The parts that other threads reach are atomics.
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
        memory.commit(0, HEADER);
        let first = Region {
            returned: AtomicUsize::new(NONE),
            owed: AtomicUsize::new(0),
            memory: Box::new(memory),
        };
        // SAFETY: the memory is committed for `HEADER` bytes, aligned, and unused.
        unsafe { region.write(first) };
        Self {
            region,
            budget,
            span: budget.next_multiple_of(ALIGN),
            committed: Cell::new(0),
            lent: Cell::new(0),
            classes: (0..classes(budget)).map(|_| Class::default()).collect(),
        }
    }

    /// Returns a writable block of `len` bytes, aligned to [`ALIGN`]. It never waits.
    ///
    /// # Errors
    ///
    /// [`Error::Exhausted`] when the budget has no room for `len` bytes.
    pub fn alloc(&self, len: usize) -> Result<Unique, Error> {
        let available = self.budget - self.committed.get();
        let exhausted = Error::Exhausted {
            requested: len,
            available,
        };
        let Some(index) = class_of(len) else {
            return Err(exhausted);
        };
        let Some(class) = self.classes.get(index) else {
            return Err(exhausted);
        };
        let offset = class.free.get();
        let offset = if offset == NONE {
            let size = footprint(index);
            if size > available {
                return Err(exhausted);
            }
            let offset = HEADER + index * self.span + class.carved.get();
            self.shared().memory.commit(offset, size);
            class.carved.set(class.carved.get() + size);
            self.committed.set(self.committed.get() + size);
            offset
        } else {
            // SAFETY: a block on a free list has a header, and only the pool reads it.
            let free = unsafe { self.header(offset).as_ref() };
            class.free.set(free.next.load(Relaxed));
            offset
        };
        let header = self.header(offset);
        let fresh = Header {
            refs: AtomicUsize::new(0),
            next: AtomicUsize::new(NONE),
            len,
            offset,
        };
        // SAFETY: the block is committed and aligned, and no other holder has it.
        unsafe { header.write(fresh) };
        self.lent.set(self.lent.get() + 1);
        Ok(Unique { header })
    }

    /// Takes back the blocks that holders dropped, so that `alloc` can use them
    /// again. The owner shard calls it from its loop.
    pub fn reclaim(&self) {
        let returned = &self.shared().returned;
        if returned.load(Relaxed) != NONE {
            self.take(returned.swap(NONE, Acquire));
        }
    }

    /// Bytes committed now: the blocks that holders have, and the blocks that wait
    /// for the next `alloc`.
    #[must_use]
    pub fn committed(&self) -> usize {
        self.committed.get()
    }

    fn shared(&self) -> &Region {
        // SAFETY: `new` wrote the region, and it lives until the pool and each block
        // are gone.
        unsafe { self.region.as_ref() }
    }

    fn header(&self, offset: usize) -> NonNull<Header> {
        // SAFETY: each caller passes the offset of a block in the region.
        unsafe { self.region.cast::<u8>().add(offset) }.cast()
    }

    /// Moves a list of returned blocks to the free lists.
    fn take(&self, mut offset: usize) {
        while offset != NONE {
            // SAFETY: a returned block has a header. Its last holder gave it up with
            // the store that the swap of `returned` read.
            let header = unsafe { self.header(offset).as_ref() };
            let next = header.next.load(Relaxed);
            let class = self
                .classes
                .get((offset - HEADER) / self.span)
                .expect("invariant: a block lies in the span of its class");
            header.next.store(class.free.get(), Relaxed);
            class.free.set(offset);
            self.lent.set(self.lent.get() - 1);
            offset = next;
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let region = self.shared();
        self.take(region.returned.swap(CLOSED, Acquire));
        let lent = self.lent.get();
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
    let memory = unsafe { &raw mut (*region.as_ptr()).memory };
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

/// The payload of a block.
fn payload(header: NonNull<Header>) -> (NonNull<u8>, usize) {
    // SAFETY: each `Unique` and `Block` holds a valid header.
    let len = unsafe { header.as_ref() }.len;
    // SAFETY: the payload starts `HEADER` bytes after the header, in the same block.
    (unsafe { header.cast::<u8>().add(HEADER) }, len)
}

/// A block with one owner, which may write to it.
#[derive(Debug)]
pub struct Unique {
    header: NonNull<Header>,
}

// SAFETY: a `Unique` is the one holder of its block, and the return path is atomic.
unsafe impl Send for Unique {}
// SAFETY: a shared `Unique` only gives shared access to its bytes.
unsafe impl Sync for Unique {}

impl Unique {
    /// Makes the block immutable and shareable. It keeps the same bytes.
    #[must_use]
    pub fn freeze(self) -> Block {
        let header = ManuallyDrop::new(self).header;
        // SAFETY: the header is valid while a holder has the block.
        unsafe { header.as_ref() }.refs.store(1, Relaxed);
        Block { header }
    }
}

impl Deref for Unique {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let (start, len) = payload(self.header);
        // SAFETY: the payload is committed, so it is initialized, and no one writes
        // to it while `self` is borrowed.
        unsafe { slice::from_raw_parts(start.as_ptr(), len) }
    }
}

impl DerefMut for Unique {
    fn deref_mut(&mut self) -> &mut [u8] {
        let (start, len) = payload(self.header);
        // SAFETY: the payload is committed, and `self` is its one holder.
        unsafe { slice::from_raw_parts_mut(start.as_ptr(), len) }
    }
}

impl Drop for Unique {
    fn drop(&mut self) {
        // SAFETY: a `Unique` is the one holder of its block.
        unsafe { give_back(self.header) };
    }
}

/// An immutable block shared by reference count. Cloning it adds one reference.
#[derive(Debug)]
pub struct Block {
    header: NonNull<Header>,
}

// SAFETY: the bytes are immutable, and the count and the return path are atomic.
unsafe impl Send for Block {}
// SAFETY: as for `Send`.
unsafe impl Sync for Block {}

impl Block {
    fn refs(&self) -> &AtomicUsize {
        // SAFETY: the header is valid while a holder has the block.
        &unsafe { self.header.as_ref() }.refs
    }
}

impl Clone for Block {
    fn clone(&self) -> Self {
        let before = self.refs().fetch_add(1, Relaxed);
        assert!(before < usize::MAX / 2, "a block has too many holders");
        Self {
            header: self.header,
        }
    }
}

impl Deref for Block {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let (start, len) = payload(self.header);
        // SAFETY: the payload is committed, and no one writes to a frozen block.
        unsafe { slice::from_raw_parts(start.as_ptr(), len) }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        if self.refs().fetch_sub(1, AcqRel) == 1 {
            // SAFETY: the count reached 0, so this was the last holder.
            unsafe { give_back(self.header) };
        }
    }
}

/// An error from a [`Pool`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The pool's budget has no room for the request.
    Exhausted {
        /// Bytes asked for.
        requested: usize,
        /// Bytes still free in the budget.
        available: usize,
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
        }
    }
}

impl std::error::Error for Error {}

/// Pools over heap memory for the tests and the models.
#[cfg(test)]
mod fixture {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize as Drops;

    use super::*;

    /// Heap memory that counts its drops.
    pub(crate) struct Watched {
        heap: Heap,
        drops: Arc<Drops>,
    }

    // SAFETY: each method is the one of `Heap`.
    unsafe impl Memory for Watched {
        fn base(&self) -> NonNull<u8> {
            self.heap.base()
        }

        fn len(&self) -> usize {
            self.heap.len()
        }

        fn commit(&self, offset: usize, len: usize) {
            self.heap.commit(offset, len);
        }

        fn purge(&self, offset: usize, len: usize) {
            self.heap.purge(offset, len);
        }
    }

    impl Drop for Watched {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Relaxed);
        }
    }

    pub(crate) fn create_pool(budget: usize) -> Pool {
        let config = Config { budget };
        let heap = Heap::new(config.reservation());
        Pool::new(config, heap)
    }

    pub(crate) fn create_watched_pool(budget: usize) -> (Pool, Arc<Drops>) {
        let config = Config { budget };
        let drops = Arc::new(Drops::new(0));
        let memory = Watched {
            heap: Heap::new(config.reservation()),
            drops: Arc::clone(&drops),
        };
        (Pool::new(config, memory), drops)
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

    mod reservation {
        use super::*;

        #[test]
        fn is_one_span_for_each_class_and_a_header() {
            let reservation = |budget| Config { budget }.reservation();
            assert_eq!(reservation(0), 64);
            assert_eq!(reservation(127), 64);
            assert_eq!(reservation(128), 64 + 128);
            assert_eq!(reservation(200), 64 + 2 * 256);
            assert_eq!(reservation(1 << 16), 64 + 10 * (1 << 16));
        }

        #[test]
        #[should_panic(expected = "pool budget 18446744073709551615 is too large")]
        fn panics_when_it_does_not_fit_in_a_usize() {
            assert_eq!(Config { budget: usize::MAX }.reservation(), 0);
        }
    }

    mod new {
        use super::*;

        #[test]
        #[should_panic(expected = "pool needs 192 bytes of memory, got 128")]
        fn panics_when_the_memory_is_too_short() {
            drop(Pool::new(Config { budget: 128 }, Heap::new(128)));
        }
    }

    mod alloc {
        use super::*;

        fn cases() -> ProptestConfig {
            let mut config = ProptestConfig::default();
            if cfg!(miri) {
                config.cases = 16;
                config.failure_persistence = None;
            }
            config
        }

        proptest! {
            #![proptest_config(cases())]

            #[test]
            fn gives_aligned_blocks_that_do_not_overlap(
                lens in prop::collection::vec(0_usize..=4096, 1..12),
            ) {
                let pool = create_pool(1 << 16);
                let mut blocks = Vec::new();
                for (len, fill) in lens.iter().copied().zip(1_u8..) {
                    let mut block = pool.alloc(len).expect("the budget has room");
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
        fn fails_when_the_length_is_above_each_class() {
            let pool = create_pool(256);
            assert_eq!(pool.alloc(129).err(), Some(exhausted(129, 256)));
            assert_eq!(
                pool.alloc(usize::MAX).err(),
                Some(exhausted(usize::MAX, 256))
            );
        }

        #[test]
        fn fails_when_the_budget_is_below_one_block() {
            let pool = create_pool(127);
            assert_eq!(pool.alloc(0).err(), Some(exhausted(0, 127)));
        }

        #[test]
        fn uses_a_dropped_unique_block_again_after_reclaim() {
            let pool = create_pool(128);
            let first = pool.alloc(64).expect("the budget has room");
            let address = first.as_ptr();
            assert_eq!(pool.alloc(64).err(), Some(exhausted(64, 0)));
            drop(first);
            assert_eq!(pool.alloc(64).err(), Some(exhausted(64, 0)));
            pool.reclaim();
            let second = pool.alloc(1).expect("the block is back");
            assert_eq!(second.as_ptr(), address);
            assert_eq!(pool.committed(), 128);
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
        fn returns_a_block_one_time_when_each_thread_drops_a_clone() {
            let pool = create_pool(128);
            let rounds = if cfg!(miri) { 20 } else { 2000 };
            for fill in (0..=u8::MAX).cycle().take(rounds) {
                pool.reclaim();
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
                assert_eq!(pool.alloc(64).err(), Some(exhausted(64, 0)));
            }
            pool.reclaim();
            let again = pool.alloc(64).expect("the block is back");
            assert_eq!(pool.alloc(64).err(), Some(exhausted(64, 0)));
            assert_eq!(pool.committed(), 128);
            std::mem::drop(again);
        }

        #[test]
        fn frees_the_memory_with_the_pool_when_no_block_is_out() {
            let (pool, drops) = create_watched_pool(1024);
            let block = pool.alloc(64).expect("the budget has room").freeze();
            std::mem::drop(block);
            assert_eq!(drops.load(Relaxed), 0);
            std::mem::drop(pool);
            assert_eq!(drops.load(Relaxed), 1);
        }

        #[test]
        fn keeps_the_memory_until_the_last_block_is_gone() {
            let (pool, drops) = create_watched_pool(1024);
            let mut unique = pool.alloc(8).expect("the budget has room");
            unique.fill(7);
            let block = unique.freeze();
            let clone = block.clone();
            let other = pool.alloc(8).expect("the budget has room");
            std::mem::drop(pool);
            assert_eq!(&*clone, &[7; 8]);
            std::mem::drop(block);
            std::mem::drop(other);
            assert_eq!(drops.load(Relaxed), 0);
            thread::scope(|scope| {
                scope.spawn(move || std::mem::drop(clone));
            });
            assert_eq!(drops.load(Relaxed), 1);
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
            pool.reclaim();
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
            pool.reclaim();
            let blocks = [pool.alloc(64), pool.alloc(64), pool.alloc(64)];
            assert!(
                blocks[0].is_ok() && blocks[1].is_ok(),
                "both blocks are back"
            );
            assert!(blocks[2].is_err(), "no block came back two times");
        });
    }

    #[test]
    fn frees_the_memory_one_time_when_the_pool_drops_during_the_returns() {
        loom::model(|| {
            let (pool, drops) = create_watched_pool(256);
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
            assert_eq!(drops.load(Relaxed), 1, "the memory is free one time");
        });
    }
}
