//! Makes wakers for tests that own a value that is not `Send`, so a test can give a
//! future a waker whose drop runs its code ([`holding`]). A waker used on another
//! thread aborts the process.

#![expect(
    unsafe_code,
    reason = "a waker over an `Rc` needs its own `RawWakerVTable`"
)]

use std::rc::Rc;
use std::task::{RawWaker, RawWakerVTable, Waker};
use std::thread::{self, ThreadId};

#[cfg(test)]
mod tests;

/// What a waker of [`holding`] points at.
struct Inner<T> {
    /// The thread that made the waker. It never changes.
    thread: ThreadId,
    #[expect(dead_code, reason = "the waker holds it for its drop")]
    value: T,
}

/// A waker that owns `value` and stays on the thread that calls this. Each clone
/// shares `value`, and the last drop of the waker or a clone drops it. A wake does
/// nothing.
///
/// A clone, a wake, or a drop of the waker or a clone on another thread aborts the
/// process before it reads `value`, because `value` need not be `Send`. Under libtest,
/// the message of the abort shows only with `--nocapture`.
pub fn holding<T: 'static>(value: T) -> Waker {
    let inner = Rc::new(Inner {
        thread: thread::current().id(),
        value,
    });
    let data = Rc::into_raw(inner).cast::<()>();
    // SAFETY: `data` is from `Rc::into_raw` and the waker holds its one count. Each
    // function of the vtable checks the thread before it uses the `Rc`.
    unsafe { Waker::from_raw(RawWaker::new(data, vtable::<T>())) }
}

fn vtable<T: 'static>() -> &'static RawWakerVTable {
    &RawWakerVTable::new(clone::<T>, release::<T>, wake_by_ref::<T>, release::<T>)
}

/// Aborts unless this thread made the waker at `data`. It panics, and the panic
/// aborts at the `extern "C"` boundary.
///
/// # Safety
///
/// `data` is the data of a live waker of [`holding`] of `T`.
unsafe extern "C" fn check<T>(data: *const ()) {
    // SAFETY: the waker holds a count, so `Inner` is live, and `thread` never changes,
    // so a read from any thread is no race. It reads only `thread`, never the counts.
    let thread = unsafe { (*data.cast::<Inner<T>>()).thread };
    let current = thread::current().id();
    assert!(
        thread == current,
        "a waker of `waker::holding` made on {thread:?} ran on {current:?}"
    );
}

/// A clone.
///
/// # Safety
///
/// `data` is the data of a live waker of [`holding`] of `T`.
unsafe fn clone<T: 'static>(data: *const ()) -> RawWaker {
    // SAFETY: the vtable gets `data` from a live waker.
    unsafe { check::<T>(data) };
    // SAFETY: `data` is from `Rc::into_raw` of a live `Rc`, on the thread that made it.
    unsafe { Rc::increment_strong_count(data.cast::<Inner<T>>()) };
    RawWaker::new(data, vtable::<T>())
}

/// A wake by reference, which does nothing.
///
/// # Safety
///
/// `data` is the data of a live waker of [`holding`] of `T`.
unsafe fn wake_by_ref<T>(data: *const ()) {
    // SAFETY: the vtable gets `data` from a live waker.
    unsafe { check::<T>(data) };
}

/// A drop, or a wake by value.
///
/// # Safety
///
/// `data` is the data of a live waker of [`holding`] of `T`.
unsafe fn release<T>(data: *const ()) {
    // SAFETY: the vtable gets `data` from a live waker.
    unsafe { check::<T>(data) };
    // SAFETY: as in `clone`. The waker gives up its one count.
    unsafe { Rc::decrement_strong_count(data.cast::<Inner<T>>()) };
}
