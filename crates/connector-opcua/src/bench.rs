//! Runs the event loop of open62541 alone, for benchmarks and allocation tests. Not
//! part of the contract of the crate.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use env::clock::Clock;
use env::rng::Rng;
use types::time::Monotonic;

use crate::event::Loop;
use crate::ffi::{self, Status};

/// A client of open62541 on its own event loop, which also runs a count of repeated
/// timers that do nothing.
pub struct Client {
    raw: NonNull<ffi::Client>,
    events: Loop,
}

impl Client {
    /// Makes and starts a loop on `clock` with a client and `timers` repeated timers
    /// of 1 ms.
    ///
    /// # Panics
    ///
    /// Panics if open62541 refuses the client, or gives a status other than `Good` for
    /// the first run or a timer. Only the second panic gives the name of the status.
    #[must_use]
    pub fn new(clock: Clock, timers: usize) -> Self {
        let events = Loop::new(clock, &mut Rng::from_seed(0));
        // SAFETY: the loop outlives the client, which `drop` deletes first.
        let client = unsafe { ffi::shim_client_new(events.raw()) };
        let raw = NonNull::new(client).expect("open62541 refused the client");
        let mut client = Self { raw, events };
        client.run();
        let events = &client.events;
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
        client
    }

    /// Gives the due time of the next timer, or `None` when no timer waits or the next
    /// timer is due after the clock ends.
    #[must_use]
    pub fn next(&self) -> Option<Monotonic> {
        self.events.next()
    }

    /// Runs the due timers and delayed callbacks once.
    ///
    /// # Panics
    ///
    /// If open62541 gives a status other than `Good`, with its name.
    pub fn run(&mut self) {
        // SAFETY: the client and its loop live.
        let status =
            Status(unsafe { ffi::UA_Client_run_iterate(self.raw.as_ptr(), 0) });
        assert_eq!(status, Status::GOOD, "open62541 failed a run");
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("next", &self.next())
            .finish_non_exhaustive()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: the client lives, and its loop outlives it.
        unsafe { ffi::UA_Client_delete(self.raw.as_ptr()) };
    }
}

/// A timer callback that does nothing.
unsafe extern "C" fn idle(_: *mut c_void, _: *mut c_void) {}

#[cfg(test)]
mod tests {
    use types::time::Span;

    use super::Client;

    #[test]
    fn a_run_runs_the_due_timers() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let clock = sim.node(sim::node::Config::default()).clock();
        let mut client = Client::new(clock, 1);
        let first = client.next().expect("a timer waits");
        sim.run_for(Span::MILLISECOND).expect("the run has no task");
        client.run();
        assert_eq!(client.next(), Some(first + Span::MILLISECOND), "{client:?}");
    }

    #[test]
    fn the_debug_gives_the_next_timer() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let clock = sim.node(sim::node::Config::default()).clock();
        let client = Client::new(clock, 0);
        assert_eq!(
            format!("{client:?}"),
            "Client { next: Some(Monotonic(3601000000000)), .. }"
        );
    }
}
