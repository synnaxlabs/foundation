//! The event loop that open62541 runs on. It reads an injected clock, runs the timers
//! and the delayed callbacks of the copy, and does no I/O.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::c_void;
use std::ptr::NonNull;

use env::clock::Clock;
use env::rng::Rng;
use types::time::Monotonic;

use crate::ffi;

/// An open62541 event loop, on the thread that made it.
pub(crate) struct Loop {
    raw: NonNull<ffi::EventLoop>,
    /// The C loop reads it until `drop` frees the loop.
    clock: NonNull<Clock>,
}

impl Loop {
    /// Makes a loop whose time is `clock`. It sets the random values of open62541 on
    /// this thread from `rng`, so a run on a simulated clock replays.
    ///
    /// # Panics
    ///
    /// When the C allocation of the loop fails.
    pub(crate) fn new(clock: Clock, rng: &mut Rng) -> Self {
        // SAFETY: it writes only the generator of this thread.
        unsafe { ffi::UA_random_seed_deterministic(rng.next_u64()) };
        let clock = NonNull::from(Box::leak(Box::new(clock)));
        // SAFETY: `now` reads the `Clock` at `clock`, which lives until `drop`.
        let raw = unsafe { ffi::shim_loop_new(now, clock.as_ptr().cast()) };
        let raw =
            NonNull::new(raw).expect("open62541: out of memory for an event loop");
        Self { raw, clock }
    }

    /// Gives the loop for a client or server config with `externalEventLoop`. Delete
    /// that client or server before the loop drops.
    pub(crate) fn raw(&self) -> *mut ffi::EventLoop {
        self.raw.as_ptr()
    }

    /// Gives the members of the loop, each of which takes `raw`.
    pub(crate) fn members(&self) -> &ffi::EventLoop {
        // SAFETY: the loop lives as long as `self`, and C writes it only in a call
        // that takes `raw`, which no shared borrow spans.
        unsafe { self.raw.as_ref() }
    }

    /// Gives the due time of the next timer, now rounded down to 100 ns when a delayed
    /// callback waits, or `None` when nothing waits or the next timer is due after the
    /// clock ends. A due time before the clock's epoch comes as the epoch: a once timer
    /// may have a date that has passed.
    pub(crate) fn next(&self) -> Option<Monotonic> {
        // SAFETY: the member takes its own loop.
        let ticks = unsafe { (self.members().next_timer)(self.raw()) };
        // Also `None` for the `i64::MAX` of an empty loop.
        ticks.max(0).unsigned_abs().checked_mul(100).map(Monotonic)
    }

    /// Gives whether a timer or a delayed callback is due now.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the stop of a server of #435 calls it")
    )]
    pub(crate) fn due(&self) -> bool {
        // SAFETY: the member takes its own loop.
        let next = unsafe { (self.members().next_timer)(self.raw()) };
        // SAFETY: the `Clock` at `clock` lives until `drop`. The loop gives the time
        // of a delayed callback in the same ticks.
        next <= unsafe { now(self.clock.as_ptr().cast()) }
    }
}

impl Drop for Loop {
    /// Runs the queued delayed callbacks, which free what they hold, and those that
    /// they queue, and frees the loop. Aborts the process when callbacks still wait
    /// after 64 passes.
    fn drop(&mut self) {
        // SAFETY: the loop lives, and `raw` tells the caller to delete each client
        // and server on it first.
        unsafe { ffi::shim_loop_free(self.raw.as_ptr()) };
        // SAFETY: `new` leaked the box, and the loop that read it is gone.
        drop(unsafe { Box::from_raw(self.clock.as_ptr()) });
    }
}

/// Gives the time of the `Clock` at `clock` in ticks of 100 ns.
///
/// # Safety
///
/// `clock` points at a live `Clock`.
unsafe extern "C" fn now(clock: *mut c_void) -> i64 {
    // SAFETY: the caller keeps the clock live.
    let clock = unsafe { &*clock.cast::<Clock>() };
    i64::try_from(clock.now().0 / 100).expect("invariant: u64::MAX / 100 fits in i64")
}

#[cfg(test)]
mod tests;
