//! The storage of a ring: a power-of-two array of slots that may be empty.

use std::mem::MaybeUninit;

use crate::sync::UnsafeCell;

/// Slots addressed by a position that wraps onto the array.
pub(crate) struct Slots<T> {
    cells: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
}

// SAFETY: the producer thread owns a slot from `write` until it publishes it, then
// the consumer thread owns it for `read`. The contracts of `write` and `read` keep
// that order. Values of `T` cross, so `T: Send`.
unsafe impl<T: Send> Sync for Slots<T> {}

impl<T> Slots<T> {
    /// Creates at least `capacity` empty slots.
    ///
    /// # Panics
    ///
    /// When the slots for `capacity` cannot be allocated.
    pub(crate) fn new(capacity: usize) -> Self {
        let mut cells = Vec::new();
        let len = capacity
            .checked_next_power_of_two()
            .filter(|&len| cells.try_reserve_exact(len).is_ok());
        let Some(len) = len else {
            panic!("ring capacity {capacity} is too large to allocate");
        };
        cells.resize_with(len, || UnsafeCell::new(MaybeUninit::uninit()));
        Self {
            cells: cells.into_boxed_slice(),
            mask: len - 1,
        }
    }

    /// Moves `value` into the slot at `position`.
    ///
    /// # Safety
    ///
    /// The slot is empty, and no other call uses it at the same time.
    #[inline]
    pub(crate) unsafe fn write(&self, position: usize, value: T) {
        self.cells[position & self.mask].with_mut(|slot| {
            // SAFETY: the caller has sole access to the slot.
            unsafe { (*slot).write(value) };
        });
    }

    /// Moves the value out of the slot at `position` and leaves the slot empty.
    ///
    /// # Safety
    ///
    /// The slot holds a value, and no other call uses it at the same time.
    #[inline]
    pub(crate) unsafe fn read(&self, position: usize) -> T {
        self.cells[position & self.mask].with(|slot| {
            // SAFETY: the caller has sole access to the slot.
            let slot = unsafe { &*slot };
            // SAFETY: the caller says that the slot holds a value, and it treats the
            // slot as empty from here.
            unsafe { slot.assume_init_read() }
        })
    }
}
