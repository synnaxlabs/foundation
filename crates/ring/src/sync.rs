//! Atomics and cells. The model tests swap in loom's checked versions.

#[cfg(all(test, loom))]
pub(crate) use loom::{
    cell::UnsafeCell,
    hint::spin_loop,
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, fence},
};
#[cfg(not(all(test, loom)))]
pub(crate) use std::{
    hint::spin_loop,
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, fence},
};

/// A cell with loom's closure interface, so one body serves both builds.
#[cfg(not(all(test, loom)))]
pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

#[cfg(not(all(test, loom)))]
impl<T> UnsafeCell<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(std::cell::UnsafeCell::new(value))
    }

    #[inline]
    pub(crate) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        f(self.0.get())
    }

    #[inline]
    pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        f(self.0.get())
    }
}
