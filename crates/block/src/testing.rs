//! Memory for the tests of crates above `block`. Needs the `sim` feature.

use std::collections::BTreeSet;
use std::ops::Range;
use std::ptr::NonNull;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};

use crate::{ALIGN, Heap, Memory, Refused};

/// Heap memory whose commits a test can make the system refuse.
#[derive(Debug)]
pub struct Scarce {
    heap: Heap,
    system: Arc<System>,
}

impl Scarce {
    /// Memory of `len` bytes, and the switch that makes its commits refuse.
    ///
    /// # Panics
    ///
    /// If `len` is 0 or too large to allocate.
    #[must_use]
    pub fn new(len: usize) -> (Self, Switch) {
        let system = Arc::new(System::default());
        let switch = Switch {
            system: Arc::clone(&system),
        };
        let memory = Self {
            heap: Heap::new(len),
            system,
        };
        (memory, switch)
    }
}

// SAFETY: each method is the one of `Heap`, or a `commit` that refuses and changes
// no byte.
unsafe impl Memory for Scarce {
    fn base(&self) -> NonNull<u8> {
        self.heap.base()
    }

    fn len(&self) -> usize {
        self.heap.len()
    }

    fn commit(&self, offset: usize, len: usize) -> Result<(), Refused> {
        if !self.system.charge(offset, len) {
            return Err(Refused);
        }
        self.heap.commit(offset, len)
    }

    fn purge(&self, offset: usize, len: usize) {
        self.system.discharge(offset, len);
        self.heap.purge(offset, len);
    }
}

/// Makes the commits of one [`Scarce`] refuse or succeed. A clone controls the same
/// memory.
///
/// A pool commits only when it cuts a new block. An alloc that it serves from a block
/// it already holds succeeds while the memory refuses.
#[derive(Clone, Debug)]
pub struct Switch {
    system: Arc<System>,
}

impl Switch {
    /// From now on each commit that needs memory the system does not hold yet
    /// refuses and changes nothing. A commit of no bytes, of the first [`ALIGN`]
    /// bytes, or of bytes still committed needs no memory.
    pub fn refuse(&self) {
        self.limit(0);
    }

    /// From now on each commit succeeds.
    pub fn allow(&self) {
        self.limit(usize::MAX);
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
    /// Charges each unit that `len` bytes at `offset` touch and the system does not
    /// hold yet. False, with nothing charged, when those units go over `limit`.
    fn charge(&self, offset: usize, len: usize) -> bool {
        let end = if len == 0 {
            0
        } else {
            (offset + len).div_ceil(ALIGN)
        };
        let mut charged = self.charged.lock().expect("no test panicked");
        let new: Vec<usize> = units(offset / ALIGN..end)
            .filter(|unit| !charged.contains(unit))
            .collect();
        let total = (charged.len() + new.len()).saturating_mul(ALIGN);
        let fits = new.is_empty() || total <= self.limit.load(Relaxed);
        if fits {
            charged.extend(new);
        }
        fits
    }

    /// Gives back each unit that lies fully in `len` bytes at `offset`.
    fn discharge(&self, offset: usize, len: usize) {
        let mut charged = self.charged.lock().expect("no test panicked");
        for unit in units(offset.div_ceil(ALIGN)..(offset + len) / ALIGN) {
            charged.remove(&unit);
        }
    }
}

/// The units of `range` past the first, whose bytes are usable from the start.
fn units(range: Range<usize>) -> Range<usize> {
    range.start.max(1)..range.end
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use super::*;

    #[test]
    fn a_clone_controls_the_same_memory() {
        let (memory, switch) = Scarce::new(256);
        switch.clone().refuse();
        assert_eq!(memory.commit(64, 64), Err(Refused));
        switch.clone().allow();
        assert_eq!(memory.commit(64, 64), Ok(()));
    }

    #[test]
    fn while_refusing_succeeds_a_commit_that_needs_no_memory() {
        let (memory, switch) = Scarce::new(256);
        assert_eq!(memory.commit(64, 64), Ok(()));
        switch.refuse();
        assert_eq!(memory.commit(200, 0), Ok(()), "no bytes");
        assert_eq!(memory.commit(0, 64), Ok(()), "usable from the start");
        assert_eq!(memory.commit(64, 64), Ok(()), "still committed");
        assert_eq!(memory.commit(128, 1), Err(Refused));
    }

    #[test]
    fn charges_each_unit_a_range_touches_after_the_first() {
        let (memory, switch) = Scarce::new(512);
        switch.limit(64);
        assert_eq!(memory.commit(96, 64), Err(Refused), "needs two units");
        assert_eq!(memory.commit(0, 100), Ok(()), "needs one unit");
        assert_eq!(memory.commit(64, 64), Ok(()), "needs no unit");
    }

    #[test]
    fn a_refused_commit_charges_nothing() {
        let (memory, switch) = Scarce::new(512);
        switch.limit(128);
        assert_eq!(memory.commit(64, 64), Ok(()));
        assert_eq!(memory.commit(128, 128), Err(Refused));
        assert_eq!(memory.commit(256, 64), Ok(()));
    }

    #[test]
    fn a_purge_gives_back_only_the_units_that_lie_fully_in_its_range() {
        let (memory, switch) = Scarce::new(512);
        switch.limit(192);
        assert_eq!(memory.commit(64, 192), Ok(()));
        memory.purge(100, 100);
        assert_eq!(memory.commit(256, 64), Ok(()), "one unit went back");
        assert_eq!(memory.commit(320, 64), Err(Refused), "only one");
    }
}
