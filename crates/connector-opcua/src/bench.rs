//! Runs the event loop of open62541 alone, for benchmarks and allocation tests. Not
//! part of the contract of the crate.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use env::clock::Clock;
use env::rng::Rng;

use crate::event::Loop;
use crate::ffi::{self, Status};

/// An event loop with the housekeeping timer of a client and a count of repeated
/// timers that do nothing.
pub struct Iteration {
    client: NonNull<ffi::Client>,
    events: Loop,
}

impl Iteration {
    /// Makes and starts a loop on `clock` with a client and `timers` repeated timers
    /// of 1 ms.
    ///
    /// # Panics
    ///
    /// If open62541 refuses a step, with its status name.
    #[must_use]
    pub fn new(clock: Clock, timers: usize) -> Self {
        let events = Loop::new(clock, &mut Rng::from_seed(0));
        // SAFETY: the loop outlives the client, which `drop` deletes first.
        let client = unsafe { ffi::shim_client_new(events.raw()) };
        let client = NonNull::new(client).expect("open62541 refused the client");
        let mut iteration = Self { client, events };
        iteration.run();
        let events = &iteration.events;
        for _ in 0..timers {
            // SAFETY: the member takes its own loop, and `idle` reads nothing.
            let status = Status(unsafe {
                (events.members().add_timer)(
                    events.raw(),
                    idle,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    1.0,
                    ptr::null_mut(),
                    ffi::CURRENT_TIME,
                    ptr::null_mut(),
                )
            });
            assert_eq!(status, Status::GOOD, "open62541 refused a timer");
        }
        iteration
    }

    /// Runs the due timers and delayed callbacks once.
    ///
    /// # Panics
    ///
    /// If open62541 gives a status other than `Good`, with its name.
    pub fn run(&mut self) {
        // SAFETY: the client and its loop live.
        let status =
            Status(unsafe { ffi::UA_Client_run_iterate(self.client.as_ptr(), 0) });
        assert_eq!(status, Status::GOOD, "open62541 failed a run");
    }
}

impl std::fmt::Debug for Iteration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Iteration")
            .field("next", &self.events.next())
            .finish_non_exhaustive()
    }
}

impl Drop for Iteration {
    fn drop(&mut self) {
        // SAFETY: the client lives, and its loop outlives it.
        unsafe { ffi::UA_Client_delete(self.client.as_ptr()) };
    }
}

/// A timer callback that does nothing.
unsafe extern "C" fn idle(_: *mut c_void, _: *mut c_void) {}

#[cfg(test)]
mod tests {
    use types::time::Span;

    use super::Iteration;

    #[test]
    fn a_run_runs_the_due_timers() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let clock = sim.node(sim::node::Config::default()).clock();
        let mut iteration = Iteration::new(clock, 1);
        let first = iteration.events.next().expect("a timer waits");
        sim.run_for(Span::MILLISECOND).expect("the run has no task");
        iteration.run();
        assert_eq!(
            iteration.events.next(),
            Some(first + Span::MILLISECOND),
            "{iteration:?}"
        );
    }

    #[test]
    fn the_debug_gives_the_next_timer() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let clock = sim.node(sim::node::Config::default()).clock();
        let iteration = Iteration::new(clock, 0);
        assert_eq!(
            format!("{iteration:?}"),
            "Iteration { next: Some(Monotonic(3601000000000)), .. }"
        );
    }
}
