//! Makes wakers for tests that stay on the thread of their test, so a test can give a
//! future a waker whose drop runs code ([`holding`]).

#![expect(
    unsafe_code,
    reason = "a waker over an `Rc` needs its own `RawWakerVTable`"
)]

use std::process;
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
/// process before it reads `value`, because `value` need not be `Send`.
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
    &RawWakerVTable::new(clone::<T>, release::<T>, stay::<T>, release::<T>)
}

/// Aborts unless this thread made the waker at `data`.
///
/// # Safety
///
/// `data` is the data of a live waker of [`holding`] of `T`.
#[expect(
    clippy::print_stderr,
    reason = "the process aborts next, so this is its only report"
)]
unsafe fn check<T>(data: *const ()) {
    // SAFETY: the waker holds a count, so `Inner` is live, and `thread` never changes,
    // so a read from any thread is no race. It reads only `thread`, never the counts.
    let thread = unsafe { (*data.cast::<Inner<T>>()).thread };
    if thread != thread::current().id() {
        let current = thread::current().id();
        eprintln!("a waker of `waker::holding` made on {thread:?} ran on {current:?}");
        process::abort();
    }
}

unsafe fn clone<T: 'static>(data: *const ()) -> RawWaker {
    // SAFETY: the vtable gets `data` from a live waker.
    unsafe { check::<T>(data) };
    // SAFETY: `data` is from `Rc::into_raw` of a live `Rc`, on the thread that made it.
    unsafe { Rc::increment_strong_count(data.cast::<Inner<T>>()) };
    RawWaker::new(data, vtable::<T>())
}

/// A wake by reference, which does nothing.
unsafe fn stay<T>(data: *const ()) {
    // SAFETY: the vtable gets `data` from a live waker.
    unsafe { check::<T>(data) };
}

/// A drop, or a wake by value.
unsafe fn release<T>(data: *const ()) {
    // SAFETY: the vtable gets `data` from a live waker.
    unsafe { check::<T>(data) };
    // SAFETY: as in `clone`. The waker gives up its one count.
    unsafe { Rc::decrement_strong_count(data.cast::<Inner<T>>()) };
}
