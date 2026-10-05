//! Pools of preallocated, aligned buffers, and the blocks they hand out.
//!
//! One block holds one frame. A writer fills a [`Unique`], then freezes it into a
//! [`Block`] that many holders share through one reference count. When the last holder
//! drops a block, it returns to the pool that made it, from any thread.
//!
//! Blocks hold offsets, never pointers, so a block's bytes stay valid when they are
//! shared with another process.

use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

/// Byte alignment of every block.
pub const ALIGN: usize = 64;

/// Settings for one [`Pool`].
#[derive(Clone, Debug)]
pub struct Config {
    /// The most bytes the pool may commit at once.
    pub budget: usize,
}

/// A pool of blocks owned by one shard.
///
/// A pool is not `Sync`: only its owner shard allocates from it. Blocks it hands out
/// may move to and drop on any thread.
#[derive(Debug)]
pub struct Pool {
    _owner: PhantomData<std::cell::Cell<()>>,
}

impl Pool {
    /// Creates a pool. It reserves address space for `config.budget` bytes and commits
    /// pages only as blocks need them.
    #[must_use]
    #[expect(clippy::needless_pass_by_value, reason = "stub until implemented")]
    pub fn new(config: Config) -> Self {
        let _ = config;
        todo!()
    }

    /// Returns a writable block of at least `len` bytes, aligned to [`ALIGN`]. It never
    /// waits.
    ///
    /// # Errors
    ///
    /// [`Error::Exhausted`] when the budget has no room for `len` bytes.
    pub fn alloc(&self, len: usize) -> Result<Unique, Error> {
        let _ = len;
        todo!()
    }

    /// Takes back blocks that holders dropped on other threads, and releases pages that
    /// have been idle. The owner shard calls it from its loop.
    pub fn reclaim(&self) {
        todo!()
    }

    /// Bytes committed now.
    #[must_use]
    pub fn committed(&self) -> usize {
        todo!()
    }
}

/// A block with one owner, which may write to it.
#[derive(Debug)]
pub struct Unique {
    _private: (),
}

impl Unique {
    /// Makes the block immutable and shareable. It keeps the same bytes.
    #[must_use]
    pub fn freeze(self) -> Block {
        todo!()
    }
}

impl Deref for Unique {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        todo!()
    }
}

impl DerefMut for Unique {
    fn deref_mut(&mut self) -> &mut [u8] {
        todo!()
    }
}

/// An immutable block shared by reference count. Cloning it adds one reference.
#[derive(Clone, Debug)]
pub struct Block {
    _private: (),
}

impl Deref for Block {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        todo!()
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

#[cfg(all(test, miri))]
mod miri_break {
    #[test]
    #[expect(unsafe_code, reason = "deliberate break: the Miri job must fail")]
    fn reads_past_the_end() {
        let bytes = [0_u8; 4];
        // SAFETY: none. This read is out of bounds on purpose.
        let byte = unsafe { bytes.as_ptr().add(8).read() };
        std::hint::black_box(byte);
    }
}
