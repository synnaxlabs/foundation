//! Atomics, and a marker that lets the model tests see memory that is not atomic.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::AtomicUsize;
#[cfg(not(loom))]
pub(crate) use std::sync::atomic::AtomicUsize;

/// Stands for plain memory that threads hand to each other: a payload, or a region.
/// The model tests check each use for a data race. Other builds compile it to nothing.
pub(crate) struct Track(#[cfg(loom)] loom::cell::UnsafeCell<()>);

impl Track {
    pub(crate) fn new() -> Self {
        Self(
            #[cfg(loom)]
            loom::cell::UnsafeCell::new(()),
        )
    }

    /// Marks a read of the memory.
    #[inline]
    #[cfg_attr(not(loom), expect(clippy::unused_self, reason = "nothing to track"))]
    pub(crate) fn read(&self) {
        #[cfg(loom)]
        self.0.with(|_| ());
    }

    /// Marks a write of the memory, or its release.
    #[inline]
    #[cfg_attr(not(loom), expect(clippy::unused_self, reason = "nothing to track"))]
    pub(crate) fn write(&self) {
        #[cfg(loom)]
        self.0.with_mut(|_| ());
    }
}
