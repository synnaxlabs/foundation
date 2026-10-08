use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr;

use env::rng::Rng;
use sim::Sim;
use types::time::{Monotonic, Span};

use super::Loop;
use crate::ffi::{self, DelayedCallback, EventLoop, Status};

/// Ticks of 100 ns from 1601 to 1970, the epoch of `dateTime_now`.
const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

/// A loop on a simulated clock, and the record of the callbacks it ran.
struct Fixture {
    sim: Sim,
    clock: env::clock::Clock,
    events: Loop,
    probe: Box<Probe>,
}

/// What the callbacks write, and the loop that they call.
struct Probe {
    ran: RefCell<Vec<usize>>,
    raw: *mut EventLoop,
}

impl Probe {
    fn members(&self) -> &EventLoop {
        // SAFETY: the loop outlives each run that calls a callback.
        unsafe { &*self.raw }
    }
}

impl Fixture {
    fn new() -> Self {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let clock = node.clock();
        let events =
            Loop::new(env::clock::Clock::clone(&clock), &mut Rng::from_seed(0));
        let probe = Box::new(Probe {
            ran: RefCell::new(Vec::new()),
            raw: events.raw(),
        });
        Self {
            sim,
            clock,
            events,
            probe,
        }
    }

    fn members(&self) -> &EventLoop {
        self.events.members()
    }

    fn application(&self) -> *mut c_void {
        ptr::from_ref(&*self.probe).cast_mut().cast()
    }

    fn start(&self) -> Status {
        // SAFETY: the member takes its own loop.
        Status(unsafe { (self.members().start)(self.events.raw()) })
    }

    fn run(&self) -> Status {
        // SAFETY: as above.
        Status(unsafe { (self.members().run)(self.events.raw(), 0) })
    }

    /// Adds a timer that records `n`, and gives its key.
    fn add(&self, n: usize, interval_ms: f64, policy: ffi::Policy) -> u64 {
        let mut key = 0;
        // SAFETY: `record` reads the probe, which lives as long as the loop.
        let status = Status(unsafe {
            (self.members().add_timer)(
                self.events.raw(),
                record,
                self.application(),
                ptr::without_provenance_mut(n),
                interval_ms,
                ptr::null_mut(),
                policy,
                &raw mut key,
            )
        });
        assert_eq!(status, Status::GOOD);
        key
    }

    /// Gives a delayed callback that runs `callback` with `context`.
    fn delayed(
        &self,
        callback: ffi::Callback,
        context: *mut c_void,
    ) -> DelayedCallback {
        DelayedCallback {
            next: ptr::null_mut(),
            callback,
            application: self.application(),
            context,
        }
    }

    fn queue(&self, dc: &mut DelayedCallback) {
        // SAFETY: `dc` outlives each run of the test.
        unsafe { (self.members().add_delayed)(self.events.raw(), dc) };
    }

    fn unqueue(&self, dc: &mut DelayedCallback) {
        // SAFETY: as above.
        unsafe { (self.members().remove_delayed)(self.events.raw(), dc) };
    }

    fn advance(&mut self, span: Span) {
        self.sim.run_for(span).unwrap();
    }

    fn now(&self) -> Monotonic {
        self.clock.now()
    }

    /// Takes the record of the callbacks that ran.
    fn ran(&self) -> Vec<usize> {
        self.probe.ran.take()
    }
}

fn at(start: Monotonic, span: Span) -> Monotonic {
    Monotonic(start.0 + u64::try_from(span.nanos()).unwrap())
}

fn probe<'a>(application: *mut c_void) -> &'a Probe {
    // SAFETY: each test gives its fixture's probe, which outlives the loop's runs.
    unsafe { &*application.cast::<Probe>() }
}

/// Records the number in `data`.
unsafe extern "C" fn record(application: *mut c_void, data: *mut c_void) {
    probe(application).ran.borrow_mut().push(data.addr());
}

/// Queues the delayed callback at `data`, and records 0.
unsafe extern "C" fn queue(application: *mut c_void, data: *mut c_void) {
    let probe = probe(application);
    probe.ran.borrow_mut().push(0);
    // SAFETY: `data` is a delayed callback of the test, and the loop lives.
    unsafe { (probe.members().add_delayed)(probe.raw, data.cast()) };
}

/// Removes the delayed callback at `data`.
unsafe extern "C" fn unqueue(application: *mut c_void, data: *mut c_void) {
    let probe = probe(application);
    // SAFETY: as above.
    unsafe { (probe.members().remove_delayed)(probe.raw, data.cast()) };
}

/// Runs the loop inside its own run, and records the status.
unsafe extern "C" fn nest(application: *mut c_void, _: *mut c_void) {
    let probe = probe(application);
    // SAFETY: the loop lives.
    let status = unsafe { (probe.members().run)(probe.raw, 0) };
    probe
        .ran
        .borrow_mut()
        .push(usize::try_from(status).unwrap());
}

fn number(n: usize) -> *mut c_void {
    ptr::without_provenance_mut(n)
}

#[test]
fn the_loop_reads_its_clock() {
    let mut f = Fixture::new();
    f.advance(Span::from_nanos(1_234_567));
    let ticks = i64::try_from(f.now().0).unwrap() / 100;
    assert_eq!(
        ticks % 100_000,
        12_345,
        "a tick is 100 ns, and the rest drops"
    );
    let raw = f.events.raw();
    let members = f.members();
    // SAFETY: each member takes its own loop.
    let monotonic = unsafe { (members.now_monotonic)(raw) };
    // SAFETY: as above.
    let now = unsafe { (members.now)(raw) };
    // SAFETY: as above.
    let offset = unsafe { (members.utc_offset)(raw) };
    assert_eq!(
        (monotonic, now, offset),
        (ticks, ticks + UNIX_EPOCH_TICKS, 0)
    );
}

#[test]
fn a_run_before_the_start_fails() {
    let f = Fixture::new();
    assert_eq!(f.members().state, ffi::FRESH);
    assert_eq!(f.run(), Status::BAD_INTERNAL_ERROR);
    assert_eq!(f.start(), Status::GOOD);
    assert_eq!(f.members().state, ffi::STARTED);
    assert_eq!(f.start(), Status::BAD_INTERNAL_ERROR);
    assert_eq!(f.run(), Status::GOOD);
}

#[test]
fn a_run_inside_a_run_fails() {
    let f = Fixture::new();
    f.start();
    let mut dc = f.delayed(nest, ptr::null_mut());
    f.queue(&mut dc);
    assert_eq!(f.run(), Status::GOOD);
    assert_eq!(f.ran(), [0x8002_0000]);
}

#[test]
fn a_once_timer_runs_once_at_its_due_time() {
    let mut f = Fixture::new();
    f.start();
    let start = f.now();
    f.add(1, 10.0, ffi::ONCE);
    assert_eq!(f.events.next(), Some(at(start, ms(10))));
    f.advance(ms(9));
    f.run();
    assert_eq!(f.ran(), []);
    f.advance(ms(1));
    f.run();
    assert_eq!(f.ran(), [1]);
    assert_eq!(f.events.next(), None);
    f.advance(ms(1000));
    f.run();
    assert_eq!(f.ran(), []);
}

#[test]
fn a_current_time_timer_counts_from_the_run_after_a_missed_cycle() {
    let mut f = Fixture::new();
    f.start();
    let start = f.now();
    f.add(1, 10.0, ffi::CURRENT_TIME);
    f.advance(ms(25));
    f.run();
    assert_eq!(f.ran(), [1]);
    assert_eq!(f.events.next(), Some(at(start, ms(35))));
}

#[test]
fn a_base_time_timer_keeps_its_phase_after_a_missed_cycle() {
    let mut f = Fixture::new();
    f.start();
    let start = f.now();
    f.add(1, 10.0, ffi::BASE_TIME);
    f.advance(ms(25));
    f.run();
    assert_eq!(f.ran(), [1]);
    assert_eq!(f.events.next(), Some(at(start, ms(30))));
}

#[test]
fn a_timer_changes_its_interval_and_goes() {
    let mut f = Fixture::new();
    f.start();
    let start = f.now();
    let key = f.add(1, 10.0, ffi::CURRENT_TIME);
    f.advance(ms(5));
    // SAFETY: the member takes its own loop and a key that it gave.
    let status = Status(unsafe {
        (f.members().modify_timer)(
            f.events.raw(),
            key,
            50.0,
            ptr::null_mut(),
            ffi::ONCE,
        )
    });
    assert_eq!(status, Status::GOOD);
    assert_eq!(f.events.next(), Some(at(start, ms(55))));
    // SAFETY: as above.
    unsafe { (f.members().remove_timer)(f.events.raw(), key) };
    assert_eq!(f.events.next(), None);
    f.advance(ms(1000));
    f.run();
    assert_eq!(f.ran(), []);
}

#[test]
fn delayed_callbacks_run_in_order_after_the_due_timers() {
    let mut f = Fixture::new();
    f.start();
    let start = f.now();
    let mut first = f.delayed(record, number(1));
    let mut second = f.delayed(record, number(2));
    f.queue(&mut first);
    f.queue(&mut second);
    f.add(3, 1.0, ffi::ONCE);
    assert_eq!(f.events.next(), Some(start));
    f.advance(ms(1));
    f.run();
    assert_eq!(f.ran(), [3, 1, 2]);
    assert_eq!(f.events.next(), None);
}

#[test]
fn a_callback_that_a_delayed_callback_queues_runs_at_the_next_run() {
    let f = Fixture::new();
    f.start();
    let mut later = f.delayed(record, number(1));
    let mut first = f.delayed(queue, ptr::from_mut(&mut later).cast());
    f.queue(&mut first);
    f.run();
    assert_eq!(f.ran(), [0]);
    assert_eq!(f.events.next(), Some(f.now()));
    f.run();
    assert_eq!(f.ran(), [1]);
}

#[test]
fn a_removed_delayed_callback_does_not_run() {
    let f = Fixture::new();
    f.start();
    let mut dcs: Vec<_> = (1..=3).map(|n| f.delayed(record, number(n))).collect();
    for dc in &mut dcs {
        f.queue(dc);
    }
    f.unqueue(&mut dcs[1]);
    f.unqueue(&mut dcs[2]);
    // The tail moved back, so a new callback follows the first.
    let mut fourth = f.delayed(record, number(4));
    f.queue(&mut fourth);
    f.run();
    assert_eq!(f.ran(), [1, 4]);
}

#[test]
fn a_delayed_callback_removed_in_a_run_does_not_run() {
    let f = Fixture::new();
    f.start();
    let mut second = f.delayed(record, number(2));
    let mut first = f.delayed(unqueue, ptr::from_mut(&mut second).cast());
    let mut third = f.delayed(record, number(3));
    f.queue(&mut first);
    f.queue(&mut second);
    f.queue(&mut third);
    f.run();
    assert_eq!(f.ran(), [3]);
}

#[test]
fn the_drop_runs_the_queued_delayed_callbacks() {
    let f = Fixture::new();
    let mut dc = f.delayed(record, number(1));
    f.queue(&mut dc);
    let Fixture { events, probe, .. } = f;
    drop(events);
    assert_eq!(probe.ran.take(), [1]);
}

#[test]
fn a_client_runs_its_housekeeping_on_the_loop() {
    let mut f = Fixture::new();
    let start = f.now();
    // SAFETY: the loop lives until the client is deleted.
    let client = unsafe { ffi::shim_client_new(f.events.raw()) };
    assert!(!client.is_null());
    assert_eq!(f.members().state, ffi::FRESH);
    // SAFETY: the client lives.
    let status = Status(unsafe { ffi::UA_Client_run_iterate(client, 0) });
    assert_eq!(status, Status::GOOD);
    assert_eq!(f.members().state, ffi::STARTED);
    assert_eq!(f.events.next(), Some(at(start, ms(1000))));
    f.advance(ms(1000));
    // SAFETY: as above.
    let status = Status(unsafe { ffi::UA_Client_run_iterate(client, 0) });
    assert_eq!(status, Status::GOOD);
    assert_eq!(f.events.next(), Some(at(start, ms(2000))));
    // SAFETY: as above, and the loop outlives it.
    unsafe { ffi::UA_Client_delete(client) };
    assert_eq!(f.events.next(), None);
}

fn ms(n: i64) -> Span {
    Span::from_nanos(n * Span::MILLISECOND.nanos())
}
