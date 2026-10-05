//! The local monotonic clock. Mesh time comes from the `clock` crate, not from here.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use types::time::{Monotonic, Span};

/// The monotonic clock of one node. Clones read the same clock. Any thread may read
/// it; only a thread that `env` started may sleep on it.
///
/// ```
/// use types::time::Span;
///
/// async fn tick(clock: &env::clock::Clock) {
///     let start = clock.now();
///     clock.sleep(Span::MILLISECOND).await;
///     assert!(clock.now() >= start, "the clock went backwards");
/// }
/// ```
#[derive(Clone)]
pub struct Clock(Arc<dyn Driver>);

impl Clock {
    /// Wraps a driver from `os` or `sim`.
    ///
    /// ```
    /// fn wrap(driver: impl env::clock::Driver + 'static) -> env::clock::Clock {
    ///     env::clock::Clock::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// Reads the clock. It never goes backwards, and it means nothing on another node.
    ///
    /// ```
    /// fn read(clock: &env::clock::Clock) -> types::time::Monotonic {
    ///     clock.now()
    /// }
    /// ```
    #[must_use]
    pub fn now(&self) -> Monotonic {
        self.0.now()
    }

    /// Returns a future that completes at `deadline` or later, never before. A
    /// deadline that has passed completes at once.
    ///
    /// # Panics
    ///
    /// On a thread that `env` did not start. Make and poll the future on one thread.
    ///
    /// ```
    /// async fn wait(clock: &env::clock::Clock, deadline: types::time::Monotonic) {
    ///     clock.sleep_until(deadline).await;
    /// }
    /// ```
    #[must_use]
    pub fn sleep_until(&self, deadline: Monotonic) -> Sleep {
        Sleep {
            deadline,
            timer: self.0.timer(),
        }
    }

    /// Returns a future that completes when `span` has passed. A span of zero or less
    /// completes at once.
    ///
    /// # Panics
    ///
    /// On a thread that `env` did not start. Make and poll the future on one thread.
    ///
    /// ```
    /// async fn pause(clock: &env::clock::Clock) {
    ///     clock.sleep(types::time::Span::SECOND).await;
    /// }
    /// ```
    #[must_use]
    pub fn sleep(&self, span: Span) -> Sleep {
        self.sleep_until(self.now() + span.max(Span::ZERO))
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clock").finish_non_exhaustive()
    }
}

/// What `os` and `sim` implement to run a [`Clock`]. Only they implement it.
///
/// ```
/// fn wrap(driver: impl env::clock::Driver + 'static) -> env::clock::Clock {
///     env::clock::Clock::new(driver)
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Reads the clock. Reads on any thread never go backwards.
    fn now(&self) -> Monotonic;

    /// Makes a timer bound to the executor of the calling thread.
    ///
    /// # Panics
    ///
    /// On a thread that `env` did not start, in every driver.
    fn timer(&self) -> Pin<Box<dyn Timer>>;
}

/// One reusable timer on one thread, polled through a [`Sleep`]. Only `os` and `sim`
/// implement it.
///
/// ```
/// use std::pin::Pin;
/// use std::task::{Context, Poll};
///
/// fn poll(timer: Pin<&mut dyn env::clock::Timer>, cx: &mut Context<'_>) -> Poll<()> {
///     timer.poll_until(types::time::Monotonic(1_000), cx)
/// }
/// ```
pub trait Timer {
    /// Completes when the clock reaches `deadline`: at the deadline or later, never
    /// before. How late depends on the driver; Tokio's timer works in milliseconds.
    /// Until then it returns `Pending` and wakes `cx` when it is due. The deadline
    /// may change between polls.
    fn poll_until(
        self: Pin<&mut Self>,
        deadline: Monotonic,
        cx: &mut Context<'_>,
    ) -> Poll<()>;
}

/// A future that completes at a deadline. [`Sleep::reset`] reuses it without an
/// allocation. It stays on the thread that made it.
///
/// ```
/// use types::time::Monotonic;
///
/// async fn tick_at(clock: &env::clock::Clock, deadlines: &[Monotonic]) {
///     let mut sleep = clock.sleep_until(deadlines[0]);
///     for &deadline in deadlines {
///         sleep.reset(deadline);
///         (&mut sleep).await;
///     }
/// }
/// ```
pub struct Sleep {
    deadline: Monotonic,
    timer: Pin<Box<dyn Timer>>,
}

impl Sleep {
    /// The time this future completes.
    ///
    /// ```
    /// fn due(sleep: &env::clock::Sleep) -> types::time::Monotonic {
    ///     sleep.deadline()
    /// }
    /// ```
    #[must_use]
    pub fn deadline(&self) -> Monotonic {
        self.deadline
    }

    /// Moves the deadline. A completed future completes again at the new deadline.
    /// The timer sees the new deadline at the next poll.
    ///
    /// ```
    /// fn later(sleep: &mut env::clock::Sleep, next: types::time::Monotonic) {
    ///     sleep.reset(next);
    /// }
    /// ```
    pub fn reset(&mut self, deadline: Monotonic) {
        self.deadline = deadline;
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        this.timer.as_mut().poll_until(this.deadline, cx)
    }
}

impl fmt::Debug for Sleep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sleep")
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::task::Waker;

    use super::*;

    type Polls = Arc<Mutex<Vec<Monotonic>>>;

    /// Reads a fixed time. Its timers never complete and record each deadline.
    struct Fixed {
        now: Monotonic,
        polls: Polls,
    }

    impl Driver for Fixed {
        fn now(&self) -> Monotonic {
            self.now
        }

        fn timer(&self) -> Pin<Box<dyn Timer>> {
            Box::pin(Recording(Arc::clone(&self.polls)))
        }
    }

    struct Recording(Polls);

    impl Timer for Recording {
        fn poll_until(
            self: Pin<&mut Self>,
            deadline: Monotonic,
            _: &mut Context<'_>,
        ) -> Poll<()> {
            self.0
                .lock()
                .expect("no test panics while locked")
                .push(deadline);
            Poll::Pending
        }
    }

    fn clock_at(now: u64) -> (Clock, Polls) {
        let polls = Polls::default();
        let driver = Fixed {
            now: Monotonic(now),
            polls: Arc::clone(&polls),
        };
        (Clock::new(driver), polls)
    }

    fn poll(sleep: &mut Sleep) {
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(
            Pin::new(sleep).poll(&mut cx),
            Poll::Pending,
            "timer completed"
        );
    }

    mod sleep {
        use super::*;

        #[test]
        fn polls_the_timer_at_now_plus_span_then_at_the_reset_deadline() {
            let (clock, polls) = clock_at(100);
            let mut sleep = clock.sleep(Span::from_nanos(5));
            poll(&mut sleep);
            sleep.reset(Monotonic(200));
            poll(&mut sleep);
            assert_eq!(
                *polls.lock().expect("no panic"),
                [Monotonic(105), Monotonic(200)]
            );
        }

        #[test]
        fn ends_now_for_a_negative_span() {
            let (clock, _) = clock_at(100);
            assert_eq!(
                clock.sleep(Span::from_nanos(-20)).deadline(),
                Monotonic(100)
            );
        }

        #[test]
        #[should_panic(
            expected = "monotonic overflow: 18446744073709551614 ns + 1000000000 ns"
        )]
        fn panics_past_the_end_of_the_clock() {
            let (clock, _) = clock_at(u64::MAX - 1);
            drop(clock.sleep(Span::SECOND));
        }
    }
}
