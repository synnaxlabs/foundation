//! Bounded single-producer, single-consumer rings that carry values between shards,
//! and the wake protocol for a consumer that has nothing to do.
//!
//! A producer never waits: a full ring gives the value back. A consumer spins for a
//! set number of checks before it parks, and the producer wakes it.

#![expect(unsafe_code, reason = "two threads share the slots and the waker cell")]

mod slots;
mod sync;
mod wake;

use std::fmt;
use std::future::poll_fn;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::task::{Context, Poll};

use crate::slots::Slots;
use crate::sync::{Arc, AtomicBool, AtomicUsize, spin_loop};
use crate::wake::Parker;

/// Settings for one ring.
#[derive(Clone, Debug)]
pub struct Config {
    /// The most values the ring holds at once.
    pub capacity: usize,
    /// Checks a waiting consumer makes before it parks. Use 0 on a single core.
    pub spins: u32,
}

/// Creates a ring and returns its two ends.
///
/// # Panics
///
/// When `config.capacity` is 0 or too large to allocate.
#[must_use]
#[expect(clippy::needless_pass_by_value, reason = "callers build a config per ring")]
pub fn new<T: Send>(config: Config) -> (Producer<T>, Consumer<T>) {
    with_origin(&config, 0)
}

/// Creates a ring whose positions start at `origin`.
fn with_origin<T: Send>(config: &Config, origin: usize) -> (Producer<T>, Consumer<T>) {
    assert!(config.capacity > 0, "ring capacity must be more than 0");
    let shared = Arc::new(Shared {
        slots: Slots::new(config.capacity),
        capacity: config.capacity,
        spins: config.spins,
        closed: AtomicBool::new(false),
        parker: Parker::new(),
        head: Padded(AtomicUsize::new(origin)),
        tail: Padded(AtomicUsize::new(origin)),
    });
    let producer = Producer {
        shared: shared.clone(),
        tail: origin,
        head: origin,
    };
    let consumer = Consumer {
        shared,
        head: origin,
        tail: origin,
    };
    (producer, consumer)
}

/// Keeps a value on its own cache lines. 128 bytes covers the line size on Apple
/// silicon and the adjacent-line prefetch on x86-64.
#[repr(align(128))]
struct Padded<T>(T);

/// The state both ends share. `head` and `tail` are positions that only grow and
/// wrap; the values in the ring are those in `head..tail`.
struct Shared<T> {
    slots: Slots<T>,
    capacity: usize,
    spins: u32,
    /// Set when the producer is gone.
    closed: AtomicBool,
    parker: Parker,
    /// Written only by the consumer.
    head: Padded<AtomicUsize>,
    /// Written only by the producer.
    tail: Padded<AtomicUsize>,
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        let tail = self.tail.0.load(Relaxed);
        let mut head = self.head.0.load(Relaxed);
        while head != tail {
            // SAFETY: both ends are gone, and `head..tail` holds the values left.
            drop(unsafe { self.slots.read(head) });
            head = head.wrapping_add(1);
        }
    }
}

/// The sending end of a ring.
pub struct Producer<T> {
    shared: Arc<Shared<T>>,
    tail: usize,
    /// The consumer's position when this end last looked.
    head: usize,
}

impl<T: Send> Producer<T> {
    /// Adds a value without waiting, and wakes a parked consumer.
    ///
    /// # Errors
    ///
    /// [`Full`] holds the value when the ring has no room.
    pub fn push(&mut self, value: T) -> Result<(), Full<T>> {
        let shared = &*self.shared;
        if self.tail.wrapping_sub(self.head) == shared.capacity {
            self.head = shared.head.0.load(Acquire);
            if self.tail.wrapping_sub(self.head) == shared.capacity {
                return Err(Full(value));
            }
        }
        // SAFETY: the ring is not full, so the slot at `tail` is empty, and the
        // consumer does not read it before the store below.
        unsafe { shared.slots.write(self.tail, value) };
        self.tail = self.tail.wrapping_add(1);
        shared.tail.0.store(self.tail, Release);
        shared.parker.wake();
        Ok(())
    }

    /// Values in the ring now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tail.wrapping_sub(self.shared.head.0.load(Acquire))
    }

    /// Reports whether the ring holds no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for Producer<T> {
    fn drop(&mut self) {
        self.shared.closed.store(true, Release);
        self.shared.parker.wake();
    }
}

/// The receiving end of a ring.
pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
    head: usize,
    /// The producer's position when this end last looked.
    tail: usize,
}

impl<T: Send> Consumer<T> {
    /// Takes the next value if there is one, without waiting.
    pub fn try_pop(&mut self) -> Option<T> {
        if self.head == self.tail {
            self.tail = self.shared.tail.0.load(Acquire);
            if self.head == self.tail {
                return None;
            }
        }
        Some(self.take())
    }

    /// Waits for the next value. Returns `None` when the producer is gone and the ring
    /// is empty.
    pub async fn pop(&mut self) -> Option<T> {
        let mut spins = self.shared.spins;
        poll_fn(|cx| self.poll_pop(cx, &mut spins)).await
    }

    /// Values in the ring now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shared.tail.0.load(Acquire).wrapping_sub(self.head)
    }

    /// Reports whether the ring holds no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn poll_pop(&mut self, cx: &mut Context<'_>, spins: &mut u32) -> Poll<Option<T>> {
        if self.head != self.tail {
            return Poll::Ready(Some(self.take()));
        }
        loop {
            if let Some(end) = self.look() {
                return Poll::Ready(end);
            }
            if *spins == 0 {
                break;
            }
            *spins -= 1;
            spin_loop();
        }
        // SAFETY: `&mut self` makes this the only consumer call.
        let parked = unsafe { self.shared.parker.park(cx.waker()) };
        if let Some(end) = self.look() {
            self.shared.parker.cancel();
            return Poll::Ready(end);
        }
        if !parked {
            spin_loop();
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }

    /// Looks at the producer's side. Returns the next value, `Some(None)` when the
    /// producer is gone and the ring is empty, or `None` when there is nothing yet.
    fn look(&mut self) -> Option<Option<T>> {
        // `closed` first: the producer's last push comes before its close, so a ring
        // that reads closed and then empty is empty for good.
        let closed = self.shared.closed.load(Acquire);
        self.tail = self.shared.tail.0.load(Acquire);
        if self.head != self.tail {
            return Some(Some(self.take()));
        }
        closed.then_some(None)
    }

    /// Takes the value at `head`. The ring must hold one.
    fn take(&mut self) -> T {
        debug_assert_ne!(self.head, self.tail);
        // SAFETY: `head != tail`, so the producer filled the slot at `head` and does
        // not write it again before the store below.
        let value = unsafe { self.shared.slots.read(self.head) };
        self.head = self.head.wrapping_add(1);
        self.shared.head.0.store(self.head, Release);
        value
    }
}

impl<T> fmt::Debug for Producer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Producer").finish_non_exhaustive()
    }
}

impl<T> fmt::Debug for Consumer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Consumer").finish_non_exhaustive()
    }
}

/// A value that did not fit because the ring was full.
#[derive(Debug, PartialEq, Eq)]
pub struct Full<T>(pub T);

#[cfg(all(test, not(loom)))]
mod tests {
    use std::collections::VecDeque;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::Relaxed;
    use std::task::{Context, Poll, Wake, Waker};

    use proptest::prelude::*;

    use super::{Config, Consumer, Full, new, with_origin};

    /// Counts the wakes it gets.
    #[derive(Default)]
    struct Wakes(AtomicUsize);

    impl Wakes {
        fn count(&self) -> usize {
            self.0.load(Relaxed)
        }
    }

    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Relaxed);
        }
    }

    fn create_waker() -> (Arc<Wakes>, Waker) {
        let wakes = Arc::new(Wakes::default());
        (wakes.clone(), Waker::from(wakes))
    }

    fn config(capacity: usize) -> Config {
        Config { capacity, spins: 0 }
    }

    /// Polls a new `pop` one time, then drops it.
    fn poll_pop<T: Send>(consumer: &mut Consumer<T>, waker: &Waker) -> Poll<Option<T>> {
        pin!(consumer.pop()).poll(&mut Context::from_waker(waker))
    }

    mod new {
        use super::*;

        #[test]
        #[should_panic(expected = "ring capacity must be more than 0")]
        fn panics_when_capacity_is_zero() {
            let _ = new::<u8>(config(0));
        }

        #[test]
        #[should_panic(expected = "ring capacity is too large")]
        fn panics_when_capacity_does_not_fit() {
            let _ = new::<()>(config(usize::MAX));
        }
    }

    mod push {
        use super::*;

        #[test]
        fn returns_the_same_value_when_full() {
            let (mut producer, mut consumer) = new(config(2));
            assert_eq!(producer.push("a".to_string()), Ok(()));
            assert_eq!(producer.push("b".to_string()), Ok(()));
            assert_eq!(producer.push("c".to_string()), Err(Full("c".to_string())));
            assert_eq!(consumer.try_pop(), Some("a".to_string()));
            assert_eq!(producer.push("c".to_string()), Ok(()));
        }

        #[test]
        fn holds_exactly_the_capacity_when_not_a_power_of_two() {
            let (mut producer, _consumer) = new(config(3));
            for value in 0..3 {
                assert_eq!(producer.push(value), Ok(()));
            }
            assert_eq!(producer.push(3), Err(Full(3)));
            assert_eq!(producer.len(), 3);
        }

        #[test]
        fn does_not_wake_a_consumer_that_never_parked() {
            let (mut producer, mut consumer) = new(config(2));
            let (wakes, waker) = create_waker();
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(1)));
            assert_eq!(producer.push(2), Ok(()));
            assert_eq!(wakes.count(), 0);
        }

        #[test]
        fn succeeds_when_the_consumer_is_gone() {
            let (mut producer, consumer) = new(config(1));
            drop(consumer);
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(producer.push(2), Err(Full(2)));
        }
    }

    mod try_pop {
        use super::*;

        #[test]
        fn returns_none_when_empty() {
            let (mut producer, mut consumer) = new(config(2));
            assert_eq!(consumer.try_pop(), None);
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(consumer.try_pop(), Some(1));
            assert_eq!(consumer.try_pop(), None);
        }

        #[test]
        fn returns_the_values_left_when_the_producer_is_gone() {
            let (mut producer, mut consumer) = new(config(2));
            assert_eq!(producer.push(1), Ok(()));
            drop(producer);
            assert_eq!(consumer.try_pop(), Some(1));
            assert_eq!(consumer.try_pop(), None);
        }
    }

    mod pop {
        use super::*;

        #[test]
        fn returns_the_values_left_then_none_when_the_producer_is_gone() {
            let (mut producer, mut consumer) = new(config(4));
            let (wakes, waker) = create_waker();
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(producer.push(2), Ok(()));
            drop(producer);
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(1)));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(2)));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(None));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(None));
            assert_eq!(wakes.count(), 0);
        }

        #[test]
        fn parks_when_empty_and_wakes_once_on_push() {
            let (mut producer, mut consumer) = new(config(4));
            let (wakes, waker) = create_waker();
            {
                let mut pop = pin!(consumer.pop());
                let mut cx = Context::from_waker(&waker);
                assert_eq!(pop.as_mut().poll(&mut cx), Poll::Pending);
                assert_eq!(wakes.count(), 0);
                assert_eq!(producer.push(1), Ok(()));
                assert_eq!(wakes.count(), 1);
                assert_eq!(producer.push(2), Ok(()));
                assert_eq!(wakes.count(), 1);
                assert_eq!(pop.as_mut().poll(&mut cx), Poll::Ready(Some(1)));
            }
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(2)));
        }

        #[test]
        fn parks_after_its_spins() {
            let (mut producer, mut consumer) = new(Config {
                capacity: 4,
                spins: 100,
            });
            let (wakes, waker) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(wakes.count(), 1);
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(1)));
        }

        #[test]
        fn wakes_with_none_when_the_producer_goes_while_parked() {
            let (producer, mut consumer) = new::<u8>(config(4));
            let (wakes, waker) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
            drop(producer);
            assert_eq!(wakes.count(), 1);
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(None));
        }

        #[test]
        fn wakes_only_the_last_waker_it_was_polled_with() {
            let (mut producer, mut consumer) = new(config(4));
            let (first_wakes, first) = create_waker();
            let (last_wakes, last) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &first), Poll::Pending);
            assert_eq!(poll_pop(&mut consumer, &last), Poll::Pending);
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!((first_wakes.count(), last_wakes.count()), (0, 1));
        }

        #[test]
        fn parks_again_after_a_wake() {
            let (mut producer, mut consumer) = new(config(4));
            let (wakes, waker) = create_waker();
            for round in 1..=3 {
                assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
                assert_eq!(producer.push(round), Ok(()));
                assert_eq!(wakes.count(), round);
                assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(round)));
            }
        }

        #[test]
        fn leaves_the_ring_usable_when_dropped_while_parked() {
            let (mut producer, mut consumer) = new(config(4));
            let (_wakes, waker) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(consumer.try_pop(), Some(1));
            assert_eq!(consumer.try_pop(), None);
        }
    }

    mod len {
        use super::*;

        #[test]
        fn counts_the_values_from_both_ends() {
            let (mut producer, mut consumer) = new(config(4));
            assert!(producer.is_empty() && consumer.is_empty());
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(producer.push(2), Ok(()));
            assert_eq!((producer.len(), consumer.len()), (2, 2));
            assert_eq!(consumer.try_pop(), Some(1));
            assert_eq!((producer.len(), consumer.len()), (1, 1));
            assert!(!producer.is_empty() && !consumer.is_empty());
        }
    }

    mod drop {
        use super::*;

        #[test]
        fn drops_each_value_left_one_time() {
            let value = Arc::new(());
            let (mut producer, mut consumer) = new(config(4));
            for _ in 0..3 {
                assert!(producer.push(value.clone()).is_ok());
            }
            drop(consumer.try_pop());
            assert_eq!(Arc::strong_count(&value), 3);
            drop(producer);
            assert_eq!(Arc::strong_count(&value), 3);
            drop(consumer);
            assert_eq!(Arc::strong_count(&value), 1);
        }
    }

    mod order {
        use super::*;

        #[derive(Clone, Debug)]
        enum Op {
            Push(u32),
            TryPop,
        }

        fn ops() -> impl Strategy<Value = Vec<Op>> {
            let op = prop_oneof![any::<u32>().prop_map(Op::Push), Just(Op::TryPop)];
            prop::collection::vec(op, 0..256)
        }

        /// Origins that make the positions wrap past `usize::MAX`.
        fn origins() -> impl Strategy<Value = usize> {
            prop_oneof![Just(0), (0..16usize).prop_map(|back| usize::MAX - back)]
        }

        fn cases() -> ProptestConfig {
            ProptestConfig {
                cases: if cfg!(miri) { 16 } else { 512 },
                failure_persistence: None,
                ..ProptestConfig::default()
            }
        }

        proptest! {
            #![proptest_config(cases())]

            #[test]
            fn is_first_in_first_out_with_no_loss_and_no_duplicate(
                capacity in 1..9usize,
                origin in origins(),
                ops in ops(),
            ) {
                let (mut producer, mut consumer) = with_origin(&config(capacity), origin);
                let mut model = VecDeque::new();
                for op in ops {
                    match op {
                        Op::Push(value) if model.len() == capacity => {
                            prop_assert_eq!(producer.push(value), Err(Full(value)));
                        }
                        Op::Push(value) => {
                            prop_assert_eq!(producer.push(value), Ok(()));
                            model.push_back(value);
                        }
                        Op::TryPop => {
                            prop_assert_eq!(consumer.try_pop(), model.pop_front());
                        }
                    }
                    prop_assert_eq!(producer.len(), model.len());
                    prop_assert_eq!(consumer.len(), model.len());
                }
                drop(producer);
                while let Some(value) = consumer.try_pop() {
                    prop_assert_eq!(Some(value), model.pop_front());
                }
                prop_assert!(model.is_empty());
            }
        }
    }

    mod threads {
        use std::future::Future;
        use std::thread::{self, Thread};

        use super::*;

        struct Unpark(Thread);

        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        fn block_on<F: Future>(future: F) -> F::Output {
            let waker = Waker::from(Arc::new(Unpark(thread::current())));
            let mut cx = Context::from_waker(&waker);
            let mut future = pin!(future);
            loop {
                if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                    return output;
                }
                thread::park();
            }
        }

        fn carries_every_value_in_order(spins: u32) {
            let count = if cfg!(miri) { 300 } else { 300_000 };
            let (mut producer, mut consumer) = new(Config { capacity: 8, spins });
            thread::scope(|scope| {
                scope.spawn(move || {
                    for mut value in 0..count {
                        while let Err(Full(back)) = producer.push(value) {
                            value = back;
                            thread::yield_now();
                        }
                    }
                });
                for expected in 0..count {
                    assert_eq!(block_on(consumer.pop()), Some(expected));
                }
                assert_eq!(block_on(consumer.pop()), None);
            });
        }

        #[test]
        fn carry_every_value_in_order_when_the_consumer_parks_at_once() {
            carries_every_value_in_order(0);
        }

        #[test]
        fn carry_every_value_in_order_when_the_consumer_spins_first() {
            carries_every_value_in_order(64);
        }
    }

    mod alloc {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        use super::*;

        thread_local! {
            static COUNT: Cell<u64> = const { Cell::new(0) };
        }

        /// Counts the allocations of each thread.
        struct Counting;

        // SAFETY: every call goes to `System` unchanged.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                COUNT.with(|count| count.set(count.get() + 1));
                // SAFETY: the caller keeps the contract of `GlobalAlloc::alloc`.
                unsafe { System.alloc(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                // SAFETY: the caller keeps the contract of `GlobalAlloc::dealloc`.
                unsafe { System.dealloc(ptr, layout) }
            }
        }

        #[global_allocator]
        static ALLOCATOR: Counting = Counting;

        /// Allocations this thread makes while `f` runs.
        fn count(f: impl FnOnce()) -> u64 {
            let before = COUNT.with(Cell::get);
            f();
            COUNT.with(Cell::get) - before
        }

        #[test]
        fn counts_an_allocation() {
            assert_eq!(count(|| drop(Box::new(1u8))), 1);
        }

        #[test]
        fn push_and_try_pop_do_not_allocate() {
            let (mut producer, mut consumer) = new(config(4));
            let allocations = count(|| {
                for value in 0..64u64 {
                    assert_eq!(producer.push(value), Ok(()));
                    assert_eq!(producer.push(value), Ok(()));
                    assert_eq!(consumer.try_pop(), Some(value));
                    assert_eq!(consumer.try_pop(), Some(value));
                    assert_eq!(consumer.try_pop(), None);
                }
                for value in 0..5u64 {
                    let _ = producer.push(value);
                }
            });
            assert_eq!(allocations, 0);
        }

        #[test]
        fn pop_does_not_allocate_when_it_parks_and_wakes() {
            let (mut producer, mut consumer) = new(config(4));
            let (wakes, waker) = create_waker();
            let allocations = count(|| {
                for value in 0..64u64 {
                    assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
                    assert_eq!(producer.push(value), Ok(()));
                    assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(value)));
                }
            });
            assert_eq!(allocations, 0);
            assert_eq!(wakes.count(), 64);
        }
    }
}

#[cfg(all(test, loom))]
mod model {
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Wake, Waker};

    use loom::future::block_on;
    use loom::thread;

    use super::{Config, new};

    struct Ignore;

    impl Wake for Ignore {
        fn wake(self: Arc<Self>) {}
    }

    /// The producer pushes two values and goes. The consumer must get both, then
    /// `None`. A lost wakeup shows as a deadlock.
    fn loses_no_wakeup(spins: u32) {
        loom::model(move || {
            let (mut producer, mut consumer) = new(Config { capacity: 2, spins });
            let pushes = thread::spawn(move || {
                producer.push(1).unwrap();
                producer.push(2).unwrap();
            });
            assert_eq!(block_on(consumer.pop()), Some(1));
            assert_eq!(block_on(consumer.pop()), Some(2));
            assert_eq!(block_on(consumer.pop()), None);
            pushes.join().unwrap();
        });
    }

    #[test]
    fn loses_no_wakeup_when_the_consumer_parks_at_once() {
        loses_no_wakeup(0);
    }

    #[test]
    fn loses_no_wakeup_when_the_consumer_spins_first() {
        loses_no_wakeup(1);
    }

    #[test]
    fn loses_no_wakeup_when_the_producer_only_goes() {
        loom::model(|| {
            let (producer, mut consumer) = new::<u8>(Config {
                capacity: 1,
                spins: 0,
            });
            let goes = thread::spawn(move || drop(producer));
            assert_eq!(block_on(consumer.pop()), None);
            goes.join().unwrap();
        });
    }

    /// The consumer parks with one waker, then polls again with another while the
    /// producer wakes the first.
    #[test]
    fn loses_no_wakeup_when_the_consumer_changes_its_waker() {
        loom::model(|| {
            let (mut producer, mut consumer) = new(Config {
                capacity: 1,
                spins: 0,
            });
            let pushes = thread::spawn(move || producer.push(1).unwrap());
            let ignore = Waker::from(Arc::new(Ignore));
            let first = pin!(consumer.pop()).poll(&mut Context::from_waker(&ignore));
            let value = match first {
                std::task::Poll::Ready(value) => value,
                std::task::Poll::Pending => block_on(consumer.pop()),
            };
            assert_eq!(value, Some(1));
            assert_eq!(block_on(consumer.pop()), None);
            pushes.join().unwrap();
        });
    }
}
