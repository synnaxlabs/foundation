//! The wake protocol between a consumer that parks and the producer that wakes it.
//!
//! The producer publishes, then calls [`Parker::wake`]. The consumer calls
//! [`Parker::park`], then looks again for what the producer published. A fence on each
//! side makes one of the two see the other, so no wakeup is lost.

use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use std::task::Waker;

use crate::sync::{AtomicU8, UnsafeCell, fence};

/// The consumer runs. Only the consumer may use the waker cell.
const AWAKE: u8 = 0;
/// The consumer left a waker and waits for the producer.
const PARKED: u8 = 1;
/// The producer takes the waker. Only the producer may use the waker cell.
const WAKING: u8 = 2;

/// Where a consumer leaves its waker before it parks.
pub(crate) struct Parker {
    state: AtomicU8,
    waker: UnsafeCell<Option<Waker>>,
}

// SAFETY: the consumer thread owns the waker cell in `AWAKE` and the producer thread
// owns it in `WAKING`. Only a `Waker` crosses, and it is `Send + Sync`.
unsafe impl Sync for Parker {}

impl Parker {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(AWAKE),
            waker: UnsafeCell::new(None),
        }
    }

    /// Leaves `waker` for the producer. The caller must look again for published work
    /// before it waits.
    ///
    /// Returns `false` when the producer is in the middle of a wake. The consumer is
    /// then not parked and must try again.
    ///
    /// # Safety
    ///
    /// Only the one consumer calls this, from one thread at a time.
    pub(crate) unsafe fn park(&self, waker: &Waker) -> bool {
        match self.state.compare_exchange(PARKED, AWAKE, Acquire, Acquire) {
            Ok(_) | Err(AWAKE) => {}
            Err(WAKING) => return false,
            Err(state) => unreachable!("invariant: park state {state} does not exist"),
        }
        self.waker.with_mut(|cell| {
            // SAFETY: the state is `AWAKE` and the producer only leaves `PARKED`, so
            // the consumer has sole access to the cell.
            match unsafe { &mut *cell } {
                Some(left) => left.clone_from(waker),
                empty => *empty = Some(waker.clone()),
            }
        });
        self.state.store(PARKED, Release);
        fence(SeqCst);
        true
    }

    /// Takes back a park when the consumer found work without a wake.
    pub(crate) fn cancel(&self) {
        // A failure means the producer saw the park, and it wakes the consumer.
        let (Ok(_) | Err(_)) =
            self.state.compare_exchange(PARKED, AWAKE, Relaxed, Relaxed);
    }

    /// Drops the waker that a park left, when the consumer goes.
    ///
    /// # Safety
    ///
    /// Only the one consumer calls this, from one thread at a time.
    pub(crate) unsafe fn clear(&self) {
        // In `WAKING` the producer takes the waker out itself.
        if let Ok(_) | Err(AWAKE) =
            self.state.compare_exchange(PARKED, AWAKE, Acquire, Acquire)
        {
            // SAFETY: the state is `AWAKE` and the producer only leaves `PARKED`, so
            // the consumer has sole access to the cell.
            self.waker.with_mut(|cell| unsafe { *cell = None });
        }
    }

    /// Wakes the consumer if it parked. The caller must publish its work first.
    pub(crate) fn wake(&self) {
        fence(SeqCst);
        if self.state.load(Relaxed) != PARKED {
            return;
        }
        if self
            .state
            .compare_exchange(PARKED, WAKING, Acquire, Relaxed)
            .is_err()
        {
            return;
        }
        // SAFETY: this call moved the state to `WAKING`, so it has sole access to the
        // cell until it stores `AWAKE`.
        let waker = self.waker.with_mut(|cell| unsafe { (*cell).take() });
        self.state.store(AWAKE, Release);
        waker
            .expect("invariant: a parked consumer left a waker")
            .wake();
    }
}
