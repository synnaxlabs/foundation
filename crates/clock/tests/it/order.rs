//! Reads that overlap an update, on real threads with a clock that pauses at one read.
//! The simulation polls one task at a time, so its reads never overlap an update.

use std::pin::Pin;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicU8, AtomicU64};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use clock::{Clock, Reader, Status};
use estimate::Measurement;
use types::time::{Interval, Monotonic, Span};

const SECOND: u64 = 1_000_000_000;
const MILLISECOND: u64 = 1_000_000;
/// The time of the push that [`slewing`] makes ready.
const PUSHED: u64 = SECOND + 400 * MILLISECOND;

const IDLE: u8 = 0;
const ARMED: u8 = 1;
const PAUSED: u8 = 2;
const RELEASED: u8 = 3;

/// A monotonic clock that the test sets and that counts its reads. Once armed, the
/// first read on any thread after `skip` reads takes the time, then waits until the
/// test releases it.
#[derive(Default)]
struct Paused {
    time: AtomicU64,
    state: AtomicU8,
    skip: AtomicU64,
    reads: AtomicU64,
}

struct Driver(Arc<Paused>);

impl env::clock::Driver for Driver {
    fn now(&self) -> Monotonic {
        self.0.reads.fetch_add(1, SeqCst);
        let time = self.0.time.load(SeqCst);
        if self.0.state.load(SeqCst) == ARMED
            && self
                .0
                .skip
                .fetch_update(SeqCst, SeqCst, |skip| skip.checked_sub(1))
                .is_ok()
        {
            return Monotonic(time);
        }
        if self
            .0
            .state
            .compare_exchange(ARMED, PAUSED, SeqCst, SeqCst)
            .is_ok()
        {
            while self.0.state.load(SeqCst) != RELEASED {
                thread::yield_now();
            }
        }
        Monotonic(time)
    }

    fn epoch(&self) -> Instant {
        unreachable!("the clock reads only now")
    }

    fn timer(&self) -> Pin<Box<dyn env::clock::Timer>> {
        unreachable!("the clock reads only now")
    }
}

/// A clock at one second, with one source.
fn clock() -> (Arc<Paused>, Clock, Reader, clock::source::Key) {
    let paused = Arc::new(Paused::default());
    paused.time.store(SECOND, SeqCst);
    let (mut clock, reader) =
        Clock::new(env::clock::Clock::new(Driver(Arc::clone(&paused))));
    let source = clock.add();
    (paused, clock, reader, source)
}

/// A clock at [`PUSHED`] whose one source pushed twice, so that a push of offset zero
/// at [`PUSHED`] slews it back.
fn slewing() -> (Arc<Paused>, Clock, Reader, clock::source::Key) {
    let (paused, mut clock, reader, source) = clock();
    clock.push(source, exact(SECOND, 0));
    clock.push(source, exact(SECOND, 400));
    paused.time.store(PUSHED, SeqCst);
    (paused, clock, reader, source)
}

/// Runs `op` on another thread. Its first clock read takes the time, then waits while
/// `during` runs on this thread.
#[expect(
    clippy::disallowed_methods,
    reason = "the read must overlap an update from another thread"
)]
fn overlap<A: Send, B>(
    paused: &Paused,
    op: impl FnOnce() -> A + Send,
    during: impl FnOnce() -> B,
) -> (A, B) {
    assert_eq!(
        paused.state.swap(ARMED, SeqCst),
        IDLE,
        "one pause per clock"
    );
    thread::scope(|scope| {
        let op = scope.spawn(op);
        while paused.state.load(SeqCst) != PAUSED {
            thread::yield_now();
        }
        let during = during();
        paused.state.store(RELEASED, SeqCst);
        (op.join().expect("the thread ends"), during)
    })
}

/// A measurement with no error at `at`.
fn exact(at: u64, offset_us: i64) -> Measurement {
    let offset = Span::from_nanos(offset_us * 1_000);
    Measurement::new(Monotonic(at), offset, Span::ZERO).expect("no error")
}

/// The midpoint of `interval`, in nanoseconds.
fn center(interval: Interval) -> i64 {
    interval.earliest.nanos().midpoint(interval.latest.nanos())
}

/// The midpoint of mesh time now, in nanoseconds.
fn midpoint(reader: &Reader) -> i64 {
    center(reader.now().expect("synced"))
}

/// The midpoint of mesh time in the status now, in nanoseconds.
fn status_midpoint(reader: &Reader) -> i64 {
    match reader.status() {
        Status::Synced(m) => center(m.interval()),
        status => panic!("{status:?}"),
    }
}

/// A push that read the clock, then waited 200 ms before its update, slews from what
/// a read in those 200 ms saw.
#[test]
fn a_read_before_a_late_update_never_goes_back() {
    let (paused, mut clock, reader, source) = slewing();
    let ((), before) = overlap(
        &paused,
        || clock.push(source, exact(PUSHED, 0)),
        || {
            paused.time.store(PUSHED + 200 * MILLISECOND, SeqCst);
            midpoint(&reader)
        },
    );
    let after = midpoint(&reader);
    assert!(before <= after, "mesh time went back {} ns", before - after);
}

/// A read that took the clock before an update and returned after it gives no earlier
/// mesh time than a read that returned before it started.
#[test]
fn a_read_across_an_update_never_goes_back() {
    let (paused, mut clock, reader, source) = clock();
    clock.push(source, exact(SECOND, 0));
    clock.push(source, exact(SECOND, -1_000));
    paused.time.store(SECOND + 400 * MILLISECOND, SeqCst);
    let first = midpoint(&reader);
    let (second, ()) = overlap(
        &paused,
        || midpoint(&reader),
        || {
            let pushed = SECOND + 800 * MILLISECOND;
            paused.time.store(pushed, SeqCst);
            clock.push(source, exact(pushed, -1_000));
        },
    );
    assert!(first <= second, "mesh time went back {} ns", first - second);
}

/// A status read that took the clock before an update and returned after it gives no
/// earlier mesh time than a status read that returned before it started.
#[test]
fn a_status_read_across_an_update_never_goes_back() {
    let (paused, mut clock, reader, source) = clock();
    clock.push(source, exact(SECOND, 0));
    clock.push(source, exact(SECOND, -1_000));
    paused.time.store(SECOND + 400 * MILLISECOND, SeqCst);
    let first = status_midpoint(&reader);
    let (second, ()) = overlap(
        &paused,
        || status_midpoint(&reader),
        || {
            let pushed = SECOND + 800 * MILLISECOND;
            paused.time.store(pushed, SeqCst);
            clock.push(source, exact(pushed, -1_000));
        },
    );
    assert!(first <= second, "mesh time went back {} ns", first - second);
}

/// A status read while a push holds any one of its clock reads gives no later mesh
/// time than a read after the push, and the status agrees with `now`.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "a read on a third thread must overlap the held push"
)]
fn a_status_read_during_a_push_never_goes_back() {
    let (paused, mut clock, _, source) = slewing();
    let before = paused.reads.load(SeqCst);
    clock.push(source, exact(PUSHED, 0));
    let reads = paused.reads.load(SeqCst) - before;
    for skip in 0..reads {
        let (paused, mut clock, reader, source) = slewing();
        paused.skip.store(skip, SeqCst);
        let other = reader.clone();
        let started = Arc::new(Barrier::new(2));
        let ((), during) = overlap(
            &paused,
            || clock.push(source, exact(PUSHED, 0)),
            || {
                paused.time.store(PUSHED + 200 * MILLISECOND, SeqCst);
                let read = thread::spawn({
                    let started = Arc::clone(&started);
                    move || {
                        started.wait();
                        status_midpoint(&other)
                    }
                });
                started.wait();
                // A correct read waits for the push, so the test waits a bounded time.
                thread::sleep(Duration::from_millis(100));
                read
            },
        );
        let during = during.join().expect("the read ends");
        let after = status_midpoint(&reader);
        assert_eq!(
            after,
            midpoint(&reader),
            "status and now agree at read {skip}"
        );
        assert!(
            during <= after,
            "mesh time went back {} ns at read {skip}",
            during - after
        );
    }
}

/// A push in holdover that leaves the discipline as it is writes nothing, so a status
/// read that overlaps it does not run again.
#[test]
fn a_push_that_changes_nothing_does_not_make_a_read_run_again() {
    let (paused, mut clock, reader, source) = clock();
    clock.push(source, exact(SECOND, 0));
    let _ = clock.add();
    let before = paused.reads.load(SeqCst);
    let (status, ()) = overlap(
        &paused,
        || reader.status(),
        || clock.push(source, exact(SECOND, 0)),
    );
    let reads = paused.reads.load(SeqCst) - before;
    assert!(matches!(status, Status::Holdover(..)), "{status:?}");
    assert_eq!(reads, 2, "one read in the status and one in the push");
}
