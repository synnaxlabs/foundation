//! Bounded single-producer, single-consumer rings that carry values between shards,
//! and the wake protocol for a consumer that has nothing to do.
//!
//! A producer never waits: a full ring gives the value back. A consumer parks when
//! the ring is empty, and the producer wakes it.
//!
//! [`latest`] is a cell of words that one shard replaces and every shard reads.

#![expect(unsafe_code, reason = "two threads share the slots and the waker cell")]

pub mod latest;
mod slots;
mod sync;
mod wake;

use std::fmt;
use std::future::poll_fn;
use std::num::NonZeroUsize;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::task::{Context, Poll};

use crate::slots::Slots;
use crate::sync::{Arc, AtomicBool, AtomicUsize, spin_loop};
use crate::wake::Parker;

/// Settings for one ring.
#[derive(Clone, Debug)]
pub struct Config {
    /// The most values the ring holds at once.
    pub capacity: NonZeroUsize,
}

/// Creates a ring and returns its two ends.
///
/// # Panics
///
/// When the slots for `config.capacity` cannot be allocated.
#[must_use]
#[expect(
    clippy::needless_pass_by_value,
    reason = "callers build a config per ring"
)]
pub fn new<T: Send>(config: Config) -> (Producer<T>, Consumer<T>) {
    with_origin(&config, 0)
}

/// Creates a ring whose positions start at `origin`.
fn with_origin<T: Send>(config: &Config, origin: usize) -> (Producer<T>, Consumer<T>) {
    let shared = Arc::new(Shared {
        slots: Slots::new(config.capacity.get()),
        capacity: config.capacity.get(),
        closed: AtomicBool::new(false),
        parker: Parker::new(),
        head: Padded(AtomicUsize::new(origin)),
        tail: Padded(AtomicUsize::new(origin)),
    });
    let producer = Producer {
        shared: Arc::clone(&shared),
        tail: origin,
        published: origin,
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
    /// The position after the last staged value.
    tail: usize,
    /// The position the consumer may read up to.
    published: usize,
    /// The consumer's position when this end last looked.
    head: usize,
}

impl<T: Send> Producer<T> {
    /// Adds a value without waiting, and wakes a parked consumer. It also shows the
    /// staged values. It succeeds when the consumer is gone; the value drops with the
    /// ring.
    ///
    /// # Errors
    ///
    /// [`Full`] holds the value when the ring has no room.
    pub fn push(&mut self, value: T) -> Result<(), Full<T>> {
        self.stage(value)?;
        self.publish();
        Ok(())
    }

    /// Adds a value that the consumer does not see before [`publish`](Self::publish).
    /// It never waits.
    ///
    /// # Errors
    ///
    /// [`Full`] holds the value when the ring has no room. Staged values take room,
    /// and room comes back only after [`publish`](Self::publish): the consumer cannot
    /// take staged values.
    pub fn stage(&mut self, value: T) -> Result<(), Full<T>> {
        let shared = &*self.shared;
        if self.tail.wrapping_sub(self.head) == shared.capacity {
            self.head = shared.head.0.load(Acquire);
            if self.tail.wrapping_sub(self.head) == shared.capacity {
                return Err(Full(value));
            }
        }
        // SAFETY: the ring is not full, so the slot at `tail` is empty, and the
        // consumer does not read it before `publish` stores the position.
        unsafe { shared.slots.write(self.tail, value) };
        self.tail = self.tail.wrapping_add(1);
        Ok(())
    }

    /// Shows the staged values to the consumer, and wakes it if it parked. Nothing
    /// happens when nothing is staged.
    pub fn publish(&mut self) {
        if self.tail == self.published {
            return;
        }
        self.published = self.tail;
        self.shared.tail.0.store(self.tail, Release);
        self.shared.parker.wake();
    }

    /// Values in the ring now, staged ones included.
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
        self.shared.tail.0.store(self.tail, Release);
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
        poll_fn(|cx| self.poll_pop(cx)).await
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

    fn poll_pop(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if self.head != self.tail {
            return Poll::Ready(Some(self.take()));
        }
        if let Poll::Ready(end) = self.look() {
            return Poll::Ready(end);
        }
        // SAFETY: `&mut self` makes this the only consumer call.
        let parked = unsafe { self.shared.parker.park(cx.waker()) };
        if let Poll::Ready(end) = self.look() {
            self.shared.parker.cancel();
            return Poll::Ready(end);
        }
        if !parked {
            spin_loop();
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }

    /// Looks at the producer's side. It is ready with the next value, or with `None`
    /// when the producer is gone and the ring is empty.
    fn look(&mut self) -> Poll<Option<T>> {
        // `closed` first: the producer's last publish comes before its close, so a ring
        // that reads closed and then empty is empty for good.
        let closed = self.shared.closed.load(Acquire);
        self.tail = self.shared.tail.0.load(Acquire);
        if self.head != self.tail {
            Poll::Ready(Some(self.take()))
        } else if closed {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    /// Takes the value at `head`. The ring must hold one.
    fn take(&mut self) -> T {
        // SAFETY: `head != tail`, so the producer filled the slot at `head` and does
        // not write it again before the store below.
        let value = unsafe { self.shared.slots.read(self.head) };
        self.head = self.head.wrapping_add(1);
        self.shared.head.0.store(self.head, Release);
        value
    }
}

impl<T> Drop for Consumer<T> {
    fn drop(&mut self) {
        // SAFETY: `&mut self` makes this the only consumer call.
        unsafe { self.shared.parker.clear() };
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

#[cfg(test)]
fn config(capacity: usize) -> Config {
    Config {
        capacity: NonZeroUsize::new(capacity).unwrap(),
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::collections::VecDeque;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::Relaxed;
    use std::task::{Context, Poll, Wake, Waker};

    use proptest::prelude::*;

    use super::{Consumer, Full, Producer, config, new, with_origin};

    /// Counts the wakes it gets.
    #[derive(Default)]
    struct Tally(AtomicUsize);

    impl Tally {
        fn count(&self) -> usize {
            self.0.load(Relaxed)
        }
    }

    impl Wake for Tally {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Relaxed);
        }
    }

    fn create_waker() -> (Arc<Tally>, Waker) {
        let tally = Arc::new(Tally::default());
        (Arc::clone(&tally), Waker::from(tally))
    }

    /// Polls a new `pop` one time, then drops it.
    fn poll_pop<T: Send>(consumer: &mut Consumer<T>, waker: &Waker) -> Poll<Option<T>> {
        pin!(consumer.pop()).poll(&mut Context::from_waker(waker))
    }

    mod new {
        use super::*;

        #[test]
        #[should_panic(expected = "ring capacity 18446744073709551615 is too large")]
        fn panics_when_capacity_has_no_power_of_two() {
            drop(new::<()>(config(usize::MAX)));
        }

        #[test]
        #[should_panic(expected = "ring capacity 1125899906842624 is too large")]
        fn panics_when_the_slots_do_not_fit_in_memory() {
            drop(new::<[u64; 1024]>(config(1 << 50)));
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
            let (tally, waker) = create_waker();
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(1)));
            assert_eq!(producer.push(2), Ok(()));
            assert_eq!(tally.count(), 0);
        }

        #[test]
        fn succeeds_when_the_consumer_is_gone() {
            let (mut producer, consumer) = new(config(1));
            drop(consumer);
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(producer.push(2), Err(Full(2)));
        }
    }

    mod stage {
        use super::*;

        #[test]
        fn hides_the_values_until_publish() {
            let (mut producer, mut consumer) = new(config(4));
            assert_eq!(producer.stage(1), Ok(()));
            assert_eq!(producer.stage(2), Ok(()));
            assert_eq!(producer.len(), 2);
            assert_eq!(consumer.len(), 0);
            assert_eq!(consumer.try_pop(), None);
            producer.publish();
            assert_eq!(consumer.len(), 2);
            assert_eq!(consumer.try_pop(), Some(1));
            assert_eq!(consumer.try_pop(), Some(2));
        }

        #[test]
        fn takes_room_before_publish() {
            let (mut producer, mut consumer) = new(config(2));
            assert_eq!(producer.stage(1), Ok(()));
            assert_eq!(producer.stage(2), Ok(()));
            assert_eq!(producer.stage(3), Err(Full(3)));
            assert_eq!(consumer.try_pop(), None);
            producer.publish();
            assert_eq!(consumer.try_pop(), Some(1));
            assert_eq!(producer.stage(3), Ok(()));
        }

        #[test]
        fn publishes_when_the_producer_drops() {
            let (mut producer, mut consumer) = new(config(2));
            assert_eq!(producer.stage(1), Ok(()));
            drop(producer);
            assert_eq!(consumer.try_pop(), Some(1));
            assert_eq!(consumer.try_pop(), None);
        }
    }

    mod publish {
        use super::*;

        #[test]
        fn shows_the_staged_values_through_push() {
            let (mut producer, mut consumer) = new(config(4));
            assert_eq!(producer.stage(1), Ok(()));
            assert_eq!(consumer.try_pop(), None);
            assert_eq!(producer.push(2), Ok(()));
            assert_eq!(consumer.try_pop(), Some(1));
            assert_eq!(consumer.try_pop(), Some(2));
        }

        #[test]
        fn wakes_a_parked_consumer_one_time_for_many_values() {
            let (mut producer, mut consumer) = new(config(4));
            let (tally, waker) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
            assert_eq!(producer.stage(1), Ok(()));
            assert_eq!(producer.stage(2), Ok(()));
            assert_eq!(tally.count(), 0);
            producer.publish();
            assert_eq!(tally.count(), 1);
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(1)));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(2)));
        }

        #[test]
        fn does_nothing_a_second_time_with_nothing_new_staged() {
            let (mut producer, mut consumer) = new(config(2));
            let (tally, waker) = create_waker();
            assert_eq!(producer.stage(1), Ok(()));
            producer.publish();
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(1)));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
            producer.publish();
            assert_eq!(tally.count(), 0);
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
        }

        #[test]
        fn does_nothing_with_nothing_staged() {
            let (mut producer, mut consumer) = new::<u8>(config(2));
            let (tally, waker) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
            producer.publish();
            assert_eq!(tally.count(), 0);
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
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
            let (tally, waker) = create_waker();
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(producer.push(2), Ok(()));
            drop(producer);
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(1)));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(2)));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(None));
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(None));
            assert_eq!(tally.count(), 0);
        }

        #[test]
        fn parks_when_empty_and_wakes_once_on_push() {
            let (mut producer, mut consumer) = new(config(4));
            let (tally, waker) = create_waker();
            {
                let mut pop = pin!(consumer.pop());
                let mut cx = Context::from_waker(&waker);
                assert_eq!(pop.as_mut().poll(&mut cx), Poll::Pending);
                assert_eq!(tally.count(), 0);
                assert_eq!(producer.push(1), Ok(()));
                assert_eq!(tally.count(), 1);
                assert_eq!(producer.push(2), Ok(()));
                assert_eq!(tally.count(), 1);
                assert_eq!(pop.as_mut().poll(&mut cx), Poll::Ready(Some(1)));
            }
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(2)));
        }

        #[test]
        fn wakes_with_none_when_the_producer_goes_while_parked() {
            let (producer, mut consumer) = new::<u8>(config(4));
            let (tally, waker) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
            drop(producer);
            assert_eq!(tally.count(), 1);
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(None));
        }

        #[test]
        fn wakes_only_the_last_waker_it_was_polled_with() {
            let (mut producer, mut consumer) = new(config(4));
            let (first_tally, first) = create_waker();
            let (last_tally, last) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &first), Poll::Pending);
            assert_eq!(poll_pop(&mut consumer, &last), Poll::Pending);
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!((first_tally.count(), last_tally.count()), (0, 1));
        }

        #[test]
        fn parks_again_after_a_wake() {
            let (mut producer, mut consumer) = new(config(4));
            let (tally, waker) = create_waker();
            for round in 1..=3 {
                assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
                assert_eq!(producer.push(round), Ok(()));
                assert_eq!(tally.count(), round);
                assert_eq!(poll_pop(&mut consumer, &waker), Poll::Ready(Some(round)));
            }
        }

        #[test]
        fn leaves_the_ring_usable_when_dropped_while_parked() {
            let (mut producer, mut consumer) = new(config(4));
            let (_tally, waker) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
            assert_eq!(producer.push(1), Ok(()));
            assert_eq!(consumer.try_pop(), Some(1));
            assert_eq!(consumer.try_pop(), None);
        }
    }

    mod consumer {
        use super::*;

        #[test]
        fn drops_its_waker_when_it_goes_while_parked() {
            let (producer, mut consumer) = new::<u8>(config(4));
            let (tally, waker) = create_waker();
            assert_eq!(poll_pop(&mut consumer, &waker), Poll::Pending);
            drop(waker);
            assert_eq!(Arc::strong_count(&tally), 2);
            drop(consumer);
            assert_eq!(Arc::strong_count(&tally), 1);
            drop(producer);
            assert_eq!(tally.count(), 0);
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
                assert_eq!(producer.push(Arc::clone(&value)), Ok(()));
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
            let mut config = ProptestConfig::default();
            if cfg!(miri) {
                // Miri has no file access for the failure files.
                config.cases = 16;
                config.failure_persistence = None;
            } else {
                config.cases = 512;
            }
            config
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
        use std::thread::{self, Thread};
        use std::time::Duration;

        use super::*;

        struct Unpark(Thread);

        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        /// Drives `future` on this thread. It panics after `POLLS` polls that each
        /// waited up to `WAIT`, so a lost wake fails the test in place of a hang.
        #[expect(
            clippy::disallowed_methods,
            reason = "this test drives the future on a real thread; `ring` has no `env`"
        )]
        fn block_on<F: Future>(future: F) -> F::Output {
            const POLLS: u32 = 100;
            const WAIT: Duration = Duration::from_millis(100);
            let waker = Waker::from(Arc::new(Unpark(thread::current())));
            let mut cx = Context::from_waker(&waker);
            let mut future = pin!(future);
            for _ in 0..POLLS {
                if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                    return output;
                }
                thread::park_timeout(WAIT);
            }
            panic!("no result after {POLLS} polls");
        }

        /// Pushes `value`, and yields while the ring is full. It panics after `YIELDS`
        /// yields (about 10 seconds on an M3 Max), so a stuck consumer fails the test
        /// in place of a hang.
        fn push_yielding(producer: &mut Producer<usize>, mut value: usize) {
            const YIELDS: u32 = 50_000_000;
            for _ in 0..YIELDS {
                match producer.push(value) {
                    Ok(()) => return,
                    Err(Full(back)) => value = back,
                }
                thread::yield_now();
            }
            panic!("the ring stayed full for {YIELDS} yields");
        }

        #[test]
        #[expect(
            clippy::disallowed_methods,
            reason = "a thread test; `ring` has no `env`"
        )]
        fn carries_every_value_in_order() {
            let count = if cfg!(miri) { 300 } else { 300_000 };
            let (mut producer, mut consumer) = new(config(8));
            thread::scope(|scope| {
                scope.spawn(move || {
                    for value in 0..count {
                        push_yielding(&mut producer, value);
                    }
                });
                for expected in 0..count {
                    assert_eq!(block_on(consumer.pop()), Some(expected));
                }
                assert_eq!(block_on(consumer.pop()), None);
            });
        }
    }
}

#[cfg(test)]
#[cfg(loom)]
mod model {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use loom::future::block_on;
    use loom::model::Builder;
    use loom::thread;

    use super::{Full, config, new};

    /// Checks schedules with at most five forced thread switches. The models with
    /// more steps are too large to check in full.
    fn bounded(model: impl Fn() + Send + Sync + 'static) {
        let mut builder = Builder::new();
        builder.preemption_bound = Some(5);
        builder.check(model);
    }

    // In each model a lost wakeup shows as a deadlock.

    #[test]
    fn loses_no_wakeup_when_the_producer_pushes() {
        loom::model(|| {
            let (mut producer, mut consumer) = new(config(1));
            let pushes = thread::spawn(move || {
                producer.push(1).unwrap();
                producer
            });
            assert_eq!(block_on(consumer.pop()), Some(1));
            drop(pushes.join().unwrap());
        });
    }

    #[test]
    fn loses_no_wakeup_when_the_producer_publishes_two_staged_values() {
        bounded(|| {
            let (mut producer, mut consumer) = new(config(2));
            let publishes = thread::spawn(move || {
                producer.stage(1).unwrap();
                producer.stage(2).unwrap();
                producer.publish();
                producer
            });
            assert_eq!(block_on(consumer.pop()), Some(1));
            assert_eq!(block_on(consumer.pop()), Some(2));
            drop(publishes.join().unwrap());
        });
    }

    #[test]
    fn loses_no_wakeup_when_the_producer_goes_with_a_staged_value() {
        bounded(|| {
            let (mut producer, mut consumer) = new(config(1));
            let goes = thread::spawn(move || {
                producer.stage(1).unwrap();
                drop(producer);
            });
            assert_eq!(block_on(consumer.pop()), Some(1));
            assert_eq!(block_on(consumer.pop()), None);
            goes.join().unwrap();
        });
    }

    #[test]
    fn loses_no_wakeup_when_the_producer_goes() {
        loom::model(|| {
            let (producer, mut consumer) = new::<u8>(config(1));
            let goes = thread::spawn(move || drop(producer));
            assert_eq!(block_on(consumer.pop()), None);
            goes.join().unwrap();
        });
    }

    #[test]
    fn loses_no_wakeup_over_two_pushes_and_a_close() {
        bounded(move || {
            let (mut producer, mut consumer) = new(config(2));
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

    /// The ring is full for the second push, so the producer reads `head` and uses the
    /// one slot again.
    #[test]
    fn hands_each_slot_over_when_the_ring_is_full() {
        bounded(|| {
            let (mut producer, mut consumer) = new(config(1));
            let pushes = thread::spawn(move || {
                for mut value in 1..=2 {
                    while let Err(Full(back)) = producer.push(value) {
                        value = back;
                        thread::yield_now();
                    }
                }
            });
            assert_eq!(block_on(consumer.pop()), Some(1));
            assert_eq!(block_on(consumer.pop()), Some(2));
            pushes.join().unwrap();
        });
    }

    /// The consumer parks with one waker, then polls again with another while the
    /// producer wakes the first.
    #[test]
    fn loses_no_wakeup_when_the_consumer_changes_its_waker() {
        bounded(|| {
            let (mut producer, mut consumer) = new(config(1));
            let pushes = thread::spawn(move || producer.push(1).unwrap());
            let mut cx = Context::from_waker(Waker::noop());
            let first = pin!(consumer.pop()).poll(&mut cx);
            let value = match first {
                Poll::Ready(value) => value,
                Poll::Pending => block_on(consumer.pop()),
            };
            assert_eq!(value, Some(1));
            assert_eq!(block_on(consumer.pop()), None);
            pushes.join().unwrap();
        });
    }
}
