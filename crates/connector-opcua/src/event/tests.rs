use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString, c_void};
use std::ptr;

use env::rng::Rng;
use sim::Sim;
use types::time::{Monotonic, Span};

use super::Loop;
use crate::child;
use crate::ffi::{self, DelayedCallback, EventLoop, Status};

unsafe extern "C" {
    fn UA_UInt32_random() -> u32;
}

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
    /// The times that `again` queues itself.
    left: Cell<usize>,
}

impl Probe {
    fn members(&self) -> &EventLoop {
        // SAFETY: the loop outlives each run that calls a callback.
        unsafe { &*self.raw }
    }
}

impl Fixture {
    fn new() -> Self {
        Self::with(sim::node::Config::default())
    }

    fn with(node: sim::node::Config) -> Self {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node);
        let clock = node.clock();
        let events =
            Loop::new(env::clock::Clock::clone(&clock), &mut Rng::from_seed(0));
        let probe = Box::new(Probe {
            ran: RefCell::new(Vec::new()),
            raw: events.raw(),
            left: Cell::new(0),
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
        self.timer(record, number(n), interval_ms, policy)
    }

    /// Adds a timer that runs `callback` with `data`, and gives its key.
    fn timer(
        &self,
        callback: ffi::Callback,
        data: *mut c_void,
        interval_ms: f64,
        policy: ffi::Policy,
    ) -> u64 {
        self.try_timer(callback, data, interval_ms, None, policy)
            .unwrap_or_else(|status| panic!("{status:?}"))
    }

    /// Adds a timer that runs `callback` with `data`, timed from `base` when given,
    /// and gives its key, or the status of a refusal.
    fn try_timer(
        &self,
        callback: ffi::Callback,
        data: *mut c_void,
        interval_ms: f64,
        mut base: Option<i64>,
        policy: ffi::Policy,
    ) -> Result<u64, Status> {
        let mut key = 0;
        // SAFETY: each callback of the tests reads the probe, which lives as long as
        // the loop, and a `data` that outlives the run.
        let status = Status(unsafe {
            (self.members().add_timer)(
                self.events.raw(),
                callback,
                self.application(),
                data,
                interval_ms,
                base.as_mut().map_or(ptr::null_mut(), ptr::from_mut),
                policy,
                &raw mut key,
            )
        });
        (status == Status::GOOD).then_some(key).ok_or(status)
    }

    fn remove(&self, key: u64) {
        // SAFETY: the member takes its own loop and a key that it gave.
        unsafe { (self.members().remove_timer)(self.events.raw(), key) };
    }

    /// Changes the timer of `key`, timed from `base` when given, and gives the
    /// status.
    fn modify(
        &self,
        key: u64,
        interval_ms: f64,
        mut base: Option<i64>,
        policy: ffi::Policy,
    ) -> Status {
        // SAFETY: the member takes its own loop and a key that it gave.
        Status(unsafe {
            (self.members().modify_timer)(
                self.events.raw(),
                key,
                interval_ms,
                base.as_mut().map_or(ptr::null_mut(), ptr::from_mut),
                policy,
            )
        })
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

/// Records the times left, and queues the delayed callback at `data`, itself, again
/// while some are left.
unsafe extern "C" fn again(application: *mut c_void, data: *mut c_void) {
    let probe = probe(application);
    let left = probe.left.get();
    probe.ran.borrow_mut().push(left);
    if left > 0 {
        probe.left.set(left - 1);
        // SAFETY: `data` is a delayed callback of the test, and the loop lives.
        unsafe { (probe.members().add_delayed)(probe.raw, data.cast()) };
    }
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

/// Four values of open62541's generator after a loop with `rng` of `seed`.
fn draws(seed: u64) -> [u32; 4] {
    let mut sim = Sim::new(sim::Config::default());
    let clock = sim.node(sim::node::Config::default()).clock();
    let _events = Loop::new(clock, &mut Rng::from_seed(seed));
    // SAFETY: it draws from the generator of this thread.
    std::array::from_fn(|_| unsafe { UA_UInt32_random() })
}

#[test]
fn a_loop_sets_the_generator_of_open62541_from_its_rng() {
    assert_eq!(draws(1), draws(1));
    assert_ne!(draws(1), draws(2));
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

/// open62541 ranks timers with equal due times by address, so the second set takes the
/// memory of the first, which the allocator gives back in another order.
#[test]
fn timers_due_at_one_time_run_in_the_order_of_their_adds() {
    let mut f = Fixture::new();
    f.start();
    let order: Vec<usize> = (1..=32).collect();
    for _ in 0..2 {
        let keys: Vec<u64> = order
            .iter()
            .map(|&n| f.add(n, 10.0, ffi::BASE_TIME))
            .collect();
        for _ in 0..3 {
            f.advance(ms(10));
            f.run();
            assert_eq!(f.ran(), order);
        }
        for key in keys {
            f.remove(key);
        }
    }
}

/// A current-time timer due within a quarter of its interval of one with the same
/// interval runs with it, bounds included. No assert reads the order of the run, since
/// no code may depend on the order of a tie.
#[test]
fn a_current_time_timer_runs_with_one_of_its_interval_due_near_it() {
    for (due_ms, next_ms, ran) in [
        (34, 34, vec![1, 2]),
        (35, 60, vec![1, 2]),
        (85, 60, vec![1, 2]),
        (86, 60, vec![1]),
    ] {
        let mut f = Fixture::new();
        f.start();
        let start = f.now();
        let now = i64::try_from(start.0 / 100).unwrap();
        for (n, due_ms) in [(1, 60), (2, due_ms)] {
            let base = Some(now + due_ms * 10_000);
            f.try_timer(record, number(n), 100.0, base, ffi::CURRENT_TIME)
                .expect("a timer of 100 ms");
        }
        let due = Some(at(start, ms(next_ms)));
        assert_eq!(f.events.next(), due, "timer 2 due at {due_ms} ms");
        f.advance(ms(60));
        f.run();
        assert_eq!(sorted(f.ran()), ran, "timer 2 due at {due_ms} ms");
    }
}

/// open62541 searches the window of a batch in the wrong direction, so whether a timer
/// batches hangs on the shape of the timer tree, which the other timers and their
/// addresses set.
#[test]
fn a_batch_does_not_hang_on_the_other_timers() {
    let mut kept = Vec::new();
    for others in 0..64_u32 {
        let mut f = Fixture::new();
        f.start();
        for (n, i) in (100..).zip(0..others / 8) {
            f.add(n, 10.0 + f64::from(i), ffi::ONCE);
        }
        for (n, i) in (200..).zip(0..others % 8) {
            f.add(n, 10_000.0 * (1.0 + f64::from(i)), ffi::ONCE);
        }
        f.add(1, 100.0, ffi::CURRENT_TIME);
        f.advance(ms(5));
        f.add(2, 100.0, ffi::CURRENT_TIME);
        f.advance(ms(95));
        f.run();
        let ran = f.ran().into_iter().filter(|&n| n <= 2).collect();
        assert_eq!(sorted(ran), [1, 2], "with {others} other timers");
        // Held, so each pass gets new heap addresses.
        kept.push(f);
    }
}

#[test]
fn a_timer_changes_its_interval_and_goes() {
    let mut f = Fixture::new();
    f.start();
    let start = f.now();
    let key = f.add(1, 10.0, ffi::CURRENT_TIME);
    f.advance(ms(5));
    assert_eq!(f.modify(key, 50.0, None, ffi::ONCE), Status::GOOD);
    assert_eq!(f.events.next(), Some(at(start, ms(55))));
    f.remove(key);
    assert_eq!(f.events.next(), None);
    f.advance(ms(1000));
    f.run();
    assert_eq!(f.ran(), []);
}

/// Intervals whose due time or ticks leave the range of `i64` ticks, or that are
/// not a number. The ticks of the fourth are below `i64::MIN`, though its due time from
/// a clock past 1 s is not. The last is due within 1 s of the last date only after a
/// run at the last tick of the clock.
const OUT_OF_RANGE: [(f64, ffi::Policy); 6] = [
    (922_337_203_685_477.0, ffi::CURRENT_TIME),
    (f64::INFINITY, ffi::BASE_TIME),
    (f64::NAN, ffi::ONCE),
    (-922_337_203_690_000.0, ffi::ONCE),
    (f64::NEG_INFINITY, ffi::ONCE),
    (903_890_459_611_268.0, ffi::BASE_TIME),
];

#[test]
fn a_timer_whose_due_time_leaves_the_range_of_the_clock_is_refused() {
    let mut f = Fixture::new();
    f.advance(ms(1000));
    f.start();
    for (interval_ms, policy) in OUT_OF_RANGE {
        assert_eq!(
            f.try_timer(record, number(1), interval_ms, None, policy),
            Err(Status::BAD_OUT_OF_RANGE),
            "{interval_ms}"
        );
    }
    assert_eq!(f.events.next(), None);
    f.run();
    assert_eq!(f.ran(), []);
}

/// The interval, in ms, of a timer due `before` ticks before the last date, at
/// `i64::MAX` ticks.
#[expect(
    clippy::cast_precision_loss,
    reason = "the loss is under 2,048 ticks, far inside the margins of the tests"
)]
fn due_before_the_last_date(f: &Fixture, before: f64) -> f64 {
    let now = (f.now().0 / 100) as f64;
    (i64::MAX as f64 - now - before) / 1.0e4
}

#[test]
fn a_once_timer_due_within_1_s_of_the_last_date_is_refused() {
    let mut f = Fixture::new();
    f.advance(ms(1000));
    f.start();
    let within = due_before_the_last_date(&f, 5.0e6);
    assert_eq!(
        f.try_timer(record, number(1), within, None, ffi::ONCE),
        Err(Status::BAD_OUT_OF_RANGE)
    );
    let outside = due_before_the_last_date(&f, 2.0e7);
    f.try_timer(record, number(2), outside, None, ffi::ONCE)
        .expect("a timer due 2 s before the last date is in range");
    assert_eq!(f.events.next(), None, "it is due after the clock ends");
    f.run();
    assert_eq!(f.ran(), []);
}

/// The interval, in ms, of a repeated timer that is due `before` ticks before the last
/// date when it runs at the last tick of the clock.
#[expect(
    clippy::cast_precision_loss,
    reason = "the loss is under 2,048 ticks, far inside the margins of the tests"
)]
fn repeated_before_the_last_date(before: f64) -> f64 {
    (i64::MAX as f64 - (u64::MAX / 100) as f64 - before) / 1.0e4
}

/// A repeated timer is next due one interval after its run, which may come at the last
/// tick of the clock, and its first due time may come long before an interval from
/// now.
#[test]
fn a_repeated_timer_due_within_1_s_of_the_last_date_after_a_run_is_refused() {
    let mut f = Fixture::new();
    f.start();
    let (now, _) = earliest_base(&f);
    let within = repeated_before_the_last_date(5.0e6);
    for policy in [ffi::CURRENT_TIME, ffi::BASE_TIME] {
        for base in [None, Some(now + 30_000_000), Some(now - 10_000_000)] {
            assert_eq!(
                f.try_timer(record, number(1), within, base, policy),
                Err(Status::BAD_OUT_OF_RANGE),
                "{base:?}"
            );
        }
    }
    let outside = repeated_before_the_last_date(2.0e7);
    // Distinct due times, since no code may depend on the order of a tie.
    for (n, policy, base) in [(1, ffi::CURRENT_TIME, 0), (2, ffi::BASE_TIME, 10_000)] {
        let base = Some(now + 30_000_000 + base);
        f.try_timer(record, number(n), outside, base, policy)
            .expect("a repeated timer due 2 s before the last date is in range");
    }
    f.advance(ms(3001));
    f.run();
    assert_eq!(f.ran(), [1, 2]);
    assert_eq!(f.events.next(), None, "each is due after the clock ends");
    f.run();
    assert_eq!(f.ran(), []);
}

/// The latest base in range of the last date.
const LATEST_BASE: i64 = i64::MAX - 10_000_001;

/// The ticks of the clock, and the earliest base in range of them.
fn earliest_base(f: &Fixture) -> (i64, i64) {
    let now = i64::try_from(f.now().0 / 100).unwrap();
    (now, now - (i64::MAX - 10_000_000))
}

#[test]
fn a_timer_from_a_base_out_of_range_of_the_clock_is_refused() {
    let mut f = Fixture::new();
    f.advance(ms(1000));
    f.start();
    let (now, earliest) = earliest_base(&f);
    for base in [i64::MIN, earliest - 1, LATEST_BASE + 1, i64::MAX] {
        for (interval_ms, policy) in [(3.0, ffi::BASE_TIME), (0.0, ffi::ONCE)] {
            assert_eq!(
                f.try_timer(record, number(1), interval_ms, Some(base), policy),
                Err(Status::BAD_OUT_OF_RANGE),
                "{base} {interval_ms}"
            );
        }
    }
    assert_eq!(f.events.next(), None);
    f.try_timer(record, number(1), 3.0, Some(earliest), ffi::BASE_TIME)
        .expect("the earliest base is in range");
    let into = (i128::from(now) - i128::from(earliest)) % 30_000;
    let due = i128::from(now) + 30_000 - into;
    assert_eq!(
        f.events.next(),
        Some(Monotonic(u64::try_from(due * 100).unwrap())),
        "the due time keeps the phase of the base"
    );
}

/// A driver that runs the loop whenever the next due time has come finishes at the end
/// of the clock, where the timers past it never run.
#[test]
fn a_timer_due_after_the_end_of_the_clock_is_never_next() {
    let mut f = Fixture::with(sim::node::Config {
        monotonic: Monotonic(u64::MAX - 1000),
        ..sim::node::Config::default()
    });
    f.start();
    let last = i64::try_from(u64::MAX / 100).unwrap();
    for (n, base) in [(1, LATEST_BASE), (2, last + 1)] {
        f.try_timer(record, number(n), 0.0, Some(base), ffi::ONCE)
            .expect("the base is in range");
    }
    assert_eq!(f.events.next(), None);
    f.try_timer(record, number(3), 0.0, Some(last), ffi::ONCE)
        .expect("the last tick is in range");
    assert_eq!(f.events.next(), Some(Monotonic(u64::MAX / 100 * 100)));
    f.advance(Span::from_nanos(1000));
    f.run();
    assert_eq!(f.ran(), [3]);
    assert_eq!(f.events.next(), None);
}

#[test]
fn a_once_timer_from_the_earliest_base_or_a_past_interval_runs_at_the_next_run() {
    let mut f = Fixture::new();
    f.advance(ms(1000));
    f.start();
    let (_, earliest) = earliest_base(&f);
    f.try_timer(record, number(1), 0.0, Some(earliest), ffi::ONCE)
        .expect("the earliest base is in range");
    f.add(2, -1000.0, ffi::ONCE);
    f.add(3, -9.0e14, ffi::ONCE);
    let key = f.add(4, 10.0, ffi::ONCE);
    assert_eq!(f.modify(key, -1000.0, None, ffi::ONCE), Status::GOOD);
    let key = f.add(5, 10.0, ffi::ONCE);
    assert_eq!(f.modify(key, 0.0, Some(earliest), ffi::ONCE), Status::GOOD);
    f.run();
    let mut ran = f.ran();
    ran.sort_unstable();
    assert_eq!(ran, [1, 2, 3, 4, 5]);
}

#[test]
fn a_change_to_a_base_out_of_range_is_refused_and_keeps_the_timer() {
    let mut f = Fixture::new();
    f.advance(ms(1000));
    f.start();
    let start = f.now();
    let (_, earliest) = earliest_base(&f);
    let key = f.add(1, 10.0, ffi::ONCE);
    for (interval_ms, policy) in [(3.0, ffi::BASE_TIME), (0.0, ffi::ONCE)] {
        assert_eq!(
            f.modify(key, interval_ms, Some(earliest - 1), policy),
            Status::BAD_OUT_OF_RANGE,
            "{interval_ms}"
        );
    }
    assert_eq!(f.events.next(), Some(at(start, ms(10))));
}

#[test]
fn a_change_to_an_interval_out_of_range_is_refused_and_keeps_the_timer() {
    let mut f = Fixture::new();
    f.start();
    let start = f.now();
    let key = f.add(1, 10.0, ffi::ONCE);
    for (interval_ms, policy) in OUT_OF_RANGE {
        assert_eq!(
            f.modify(key, interval_ms, None, policy),
            Status::BAD_OUT_OF_RANGE,
            "{interval_ms}"
        );
    }
    assert_eq!(f.events.next(), Some(at(start, ms(10))));
    f.advance(ms(10));
    f.run();
    assert_eq!(f.ran(), [1]);
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
fn a_delayed_callback_that_a_timer_queues_runs_after_the_due_timers_of_its_run() {
    let mut f = Fixture::new();
    f.start();
    let mut later = f.delayed(record, number(1));
    f.timer(queue, ptr::from_mut(&mut later).cast(), 1.0, ffi::ONCE);
    f.add(2, 2.0, ffi::ONCE);
    f.advance(ms(2));
    f.run();
    assert_eq!(f.ran(), [0, 2, 1]);
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
fn the_drop_runs_a_callback_that_a_queued_callback_queues() {
    let f = Fixture::new();
    let mut later = f.delayed(record, number(1));
    let mut first = f.delayed(queue, ptr::from_mut(&mut later).cast());
    f.queue(&mut first);
    let Fixture { events, probe, .. } = f;
    drop(events);
    assert_eq!(probe.ran.take(), [0, 1]);
}

/// Drops a loop whose delayed callback queues itself `left` times, and gives what ran.
fn drop_queuing(left: usize) -> Vec<usize> {
    let f = Fixture::new();
    f.probe.left.set(left);
    let mut dc = f.delayed(again, ptr::null_mut());
    dc.context = ptr::from_mut(&mut dc).cast();
    f.queue(&mut dc);
    let Fixture { events, probe, .. } = f;
    drop(events);
    probe.ran.take()
}

#[test]
fn the_drop_runs_64_passes_of_delayed_callbacks() {
    assert_eq!(drop_queuing(63), (0..=63).rev().collect::<Vec<_>>());
}

/// Runs a 65th pass of a drop, when `CHILD` is set.
#[test]
fn drop_65_passes() {
    if child::running() {
        drop_queuing(64);
    }
}

#[test]
#[cfg(unix)]
fn the_drop_aborts_after_64_passes() {
    use std::os::unix::process::ExitStatusExt;
    const SIGABRT: i32 = 6;
    let output = child::output("event::tests::drop_65_passes", &[]);
    assert_eq!(output.status.signal(), Some(SIGABRT));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "connector-opcua: open62541 queued delayed callbacks for 64 passes of a loop \
         free\n"
    );
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

unsafe extern "C" {
    fn UA_Client_addTimedCallback(
        client: *mut ffi::Client,
        callback: ffi::Callback,
        data: *mut c_void,
        date: i64,
        key: *mut u64,
    ) -> u32;
}

/// Counts the runs in the `Cell<usize>` at `data`.
unsafe extern "C" fn count(_: *mut c_void, data: *mut c_void) {
    // SAFETY: `data` is the cell of the test, which outlives the client.
    let runs = unsafe { &*data.cast::<Cell<usize>>() };
    runs.set(runs.get() + 1);
}

/// The copy runs a once timer whose date has passed at the next run. A date before
/// the epoch of the clock has passed too.
#[test]
fn a_timed_callback_at_a_past_date_runs_at_the_next_run() {
    let f = Fixture::new();
    // SAFETY: the loop lives until the client is deleted.
    let client = unsafe { ffi::shim_client_new(f.events.raw()) };
    assert!(!client.is_null());
    // SAFETY: the client lives.
    let status = Status(unsafe { ffi::UA_Client_run_iterate(client, 0) });
    assert_eq!(status, Status::GOOD);
    let runs = Cell::new(0_usize);
    let mut key = 0;
    // SAFETY: `count` reads `runs`, which outlives the client.
    let status = Status(unsafe {
        UA_Client_addTimedCallback(
            client,
            count,
            ptr::from_ref(&runs).cast_mut().cast(),
            -1_000_000_000_000,
            &raw mut key,
        )
    });
    assert_eq!(status, Status::GOOD);
    assert_eq!(f.events.next(), Some(Monotonic(0)));
    // SAFETY: the client lives.
    let status = Status(unsafe { ffi::UA_Client_run_iterate(client, 0) });
    assert_eq!(status, Status::GOOD);
    assert_eq!(runs.get(), 1);
    // SAFETY: the client lives, and the loop outlives it.
    unsafe { ffi::UA_Client_delete(client) };
}

fn sorted(mut ran: Vec<usize>) -> Vec<usize> {
    ran.sort_unstable();
    ran
}

fn ms(n: i64) -> Span {
    Span::from_nanos(n * Span::MILLISECOND.nanos())
}

/// Connects a client to `url`, which is not valid, when `CHILD` is set. The copy then
/// logs a warning and an info message.
fn connect(url: &CStr) {
    if !child::running() {
        return;
    }
    let f = Fixture::new();
    // SAFETY: the loop lives until the client is deleted.
    let client = unsafe { ffi::shim_client_new(f.events.raw()) };
    assert!(!client.is_null());
    // SAFETY: the client lives, and `url` ends with a NUL.
    let status =
        Status(unsafe { ffi::test::UA_Client_connectAsync(client, url.as_ptr()) });
    assert_eq!(status.name(), "BadTcpEndpointUrlInvalid");
    // SAFETY: the client lives, and the loop outlives it.
    unsafe { ffi::UA_Client_delete(client) };
}

#[test]
fn connect_to_a_bad_url() {
    connect(c"bad:url");
}

/// A URL of 600 bytes.
fn long() -> CString {
    CString::new(format!("bad:{}", "x".repeat(596))).unwrap()
}

#[test]
fn connect_to_a_long_url() {
    connect(&long());
}

/// Sends a request on a client with no channel, when `CHILD` is set. The copy then
/// logs an error.
#[test]
fn send_with_no_channel() {
    if !child::running() {
        return;
    }
    let f = Fixture::new();
    // SAFETY: the loop lives until the client is deleted.
    let client = unsafe { ffi::shim_client_new(f.events.raw()) };
    assert!(!client.is_null());
    // SAFETY: the client lives, and with no channel the copy reads no other argument.
    let status = Status(unsafe {
        ffi::test::__UA_Client_AsyncService(
            client,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    });
    assert_eq!(status.name(), "BadServerNotConnected");
    // SAFETY: the client lives, and the loop outlives it.
    unsafe { ffi::UA_Client_delete(client) };
}

#[test]
fn an_error_goes_to_stderr() {
    let output = child::output("event::tests::send_with_no_channel", &[]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "connector-opcua: open62541 error: SecureChannel must be connected to send \
         request\n"
    );
}

#[test]
fn a_warning_goes_to_stderr_and_an_info_message_does_not() {
    let output = child::output("event::tests::connect_to_a_bad_url", &[]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "connector-opcua: open62541 warning: Endpoint URL is invalid: bad:url\n"
    );
}

#[test]
fn a_long_line_is_cut_to_512_bytes_with_its_newline() {
    let output = child::output("event::tests::connect_to_a_long_url", &[]);
    assert!(output.status.success());
    let line = format!(
        "connector-opcua: open62541 warning: Endpoint URL is invalid: {}",
        long().to_str().unwrap()
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("{}\n", &line[..511])
    );
}
