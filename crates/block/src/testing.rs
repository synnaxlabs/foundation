//! Memory for the tests of crates above `block`. Needs the `sim` feature.

use std::collections::BTreeSet;
use std::ptr::NonNull;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

use crate::{ALIGN, Heap, Memory, Refused};

/// Heap memory whose commits a test can make the system refuse. Its first [`ALIGN`]
/// bytes are usable from the start, as the bytes of [`Heap`] are.
#[derive(Debug)]
pub struct Scarce {
    heap: Heap,
    switch: Switch,
}

impl Scarce {
    /// Memory of `len` bytes, and the switch that makes its commits refuse.
    ///
    /// # Panics
    ///
    /// If `len` is 0 or too large to allocate.
    #[must_use]
    pub fn new(len: usize) -> (Self, Switch) {
        let switch = Switch {
            system: Arc::new(System::default()),
        };
        let memory = Self {
            heap: Heap::new(len),
            switch: switch.clone(),
        };
        (memory, switch)
    }
}

// SAFETY: each method is the one of `Heap`, or a `commit` that refuses and changes
// nothing.
unsafe impl Memory for Scarce {
    fn base(&self) -> NonNull<u8> {
        self.heap.base()
    }

    fn len(&self) -> usize {
        self.heap.len()
    }

    fn commit(&self, offset: usize, len: usize) -> Result<(), Refused> {
        if !self.switch.system.charge(offset, len) {
            return Err(Refused);
        }
        self.heap.commit(offset, len)
    }

    fn purge(&self, offset: usize, len: usize) {
        self.switch.system.discharge(offset, len);
        self.heap.purge(offset, len);
    }
}

/// Makes the commits of one [`Scarce`] refuse or succeed. A clone moves the same
/// switch.
#[derive(Clone, Debug)]
pub struct Switch {
    system: Arc<System>,
}

impl Switch {
    /// From now on each commit refuses when `refusing` is true, and succeeds when it
    /// is false. A refused commit changes nothing.
    pub fn set(&self, refusing: bool) {
        self.limit(if refusing { 0 } else { usize::MAX });
    }

    /// From now on the system holds at most `bytes` committed after the first
    /// [`ALIGN`]. A purge gives its bytes back.
    pub(crate) fn limit(&self, bytes: usize) {
        self.system.limit.store(bytes, Relaxed);
    }
}

/// What the system holds committed, in units of [`ALIGN`] bytes.
#[derive(Debug)]
struct System {
    limit: AtomicUsize,
    charged: Mutex<BTreeSet<usize>>,
}

impl Default for System {
    fn default() -> Self {
        Self {
            limit: AtomicUsize::new(usize::MAX),
            charged: Mutex::default(),
        }
    }
}

impl System {
    /// Charges the bytes to the system. False when they go over `limit`.
    fn charge(&self, offset: usize, len: usize) -> bool {
        let units = offset / ALIGN..(offset + len).div_ceil(ALIGN);
        let mut charged = self.charged.lock().expect("no test panicked");
        let new = units.clone().filter(|unit| !charged.contains(unit)).count();
        let total = (charged.len() + new).saturating_mul(ALIGN);
        let fits = total <= self.limit.load(Relaxed);
        if fits {
            charged.extend(units);
        }
        fits
    }

    fn discharge(&self, offset: usize, len: usize) {
        let mut charged = self.charged.lock().expect("no test panicked");
        for unit in offset.div_ceil(ALIGN)..(offset + len) / ALIGN {
            charged.remove(&unit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Error, Pool};

    fn create_pool(budget: usize) -> (Pool, Switch) {
        let config = Config { budget };
        let (memory, switch) = Scarce::new(config.reservation());
        (Pool::new(config, memory), switch)
    }

    fn refused(requested: usize) -> Error {
        Error::Refused { requested }
    }

    #[test]
    fn refuses_each_commit_while_the_switch_is_on() {
        let (pool, switch) = create_pool(256);
        switch.set(true);
        assert_eq!(pool.alloc(64).map(drop), Err(refused(64)));
        assert_eq!(pool.alloc(64).map(drop), Err(refused(64)));
        assert_eq!(pool.committed(), 0, "a refused commit changes nothing");
        switch.set(false);
        let block = pool.alloc(64).expect("the system has memory again");
        assert_eq!(block.len(), 64);
        assert_eq!(pool.committed(), 128);
    }

    #[test]
    fn a_clone_moves_the_same_switch() {
        let (pool, switch) = create_pool(256);
        let other = switch.clone();
        other.set(true);
        assert_eq!(pool.alloc(64).map(drop), Err(refused(64)));
        switch.set(false);
        assert_eq!(pool.alloc(64).map(drop), Ok(()));
    }

    #[test]
    fn a_limit_counts_the_bytes_a_purge_gives_back() {
        let (pool, switch) = create_pool(1024);
        switch.limit(192);
        let block = pool.alloc(64).expect("the first block fits the limit");
        assert_eq!(pool.alloc(128).map(drop), Err(refused(128)));
        drop(block);
        pool.reclaim();
        assert_eq!(pool.alloc(128).map(drop), Ok(()), "the idle size went back");
        assert_eq!(pool.committed(), 192);
    }
}
