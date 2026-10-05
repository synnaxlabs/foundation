//! The monotonic clock of one node.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use types::time::{Monotonic, Span};

/// The monotonic clock of one node. Clones read the same clock.
///
/// It gives local time only. Mesh time comes from `clock`.
///
/// ```
/// use types::time::Span;
///
/// async fn tick(clock: &env::Clock) {
///     let start = clock.now();
///     clock.sleep(Span::MILLISECOND).await;
///     assert!(clock.now() >= start);
/// }
/// ```
#[derive(Clone)]
pub struct Clock(Arc<dyn Driver>);

impl Clock {
    /// Wraps a driver.
    ///
    /// ```
    /// # struct Fixed;
    /// # impl env::clock::Driver for Fixed {
    /// #     fn now(&self) -> types::time::Monotonic { types::time::Monotonic(0) }
    /// #     fn timer(&self) -> Box<dyn env::clock::Timer> { unimplemented!() }
    /// #     fn block_until(&self, _: types::time::Monotonic) {}
    /// # }
    /// let clock = env::Clock::new(Fixed);
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// Reads the clock. It never goes backwards, and it means nothing on another node.
    ///
    /// ```
    /// fn elapsed(clock: &env::Clock, start: types::time::Monotonic) -> u64 {
    ///     clock.now().0 - start.0
    /// }
    /// ```
    #[must_use]
    pub fn now(&self) -> Monotonic {
        self.0.now()
    }

    /// Returns a future that completes at `deadline`, or at once if it has passed.
    /// Poll it only on a shard.
    ///
    /// ```
    /// async fn wait(clock: &env::Clock, deadline: types::time::Monotonic) {
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

    /// Returns a future that completes when `span` has passed. A negative span
    /// completes at once. Poll it only on a shard.
    ///
    /// ```
    /// async fn pause(clock: &env::Clock) {
    ///     clock.sleep(types::time::Span::SECOND).await;
    /// }
    /// ```
    #[must_use]
    pub fn sleep(&self, span: Span) -> Sleep {
        self.sleep_until(after(self.now(), span))
    }

    /// Blocks the calling thread until `deadline`. Call it only on a dedicated thread,
    /// never on a shard.
    ///
    /// ```
    /// fn pace(clock: &env::Clock, deadline: types::time::Monotonic) {
    ///     clock.block_until(deadline);
    /// }
    /// ```
    pub fn block_until(&self, deadline: Monotonic) {
        self.0.block_until(deadline);
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clock").finish_non_exhaustive()
    }
}

/// The time `span` after `start`, clamped to the clock's range.
fn after(start: Monotonic, span: Span) -> Monotonic {
    Monotonic(start.0.saturating_add_signed(span.nanos()))
}

/// What `os` and `sim` implement to run a [`Clock`].
///
/// ```
/// use std::sync::atomic::{AtomicU64, Ordering};
/// use types::time::Monotonic;
///
/// /// A clock that a test moves by hand.
/// struct Manual(AtomicU64);
///
/// impl env::clock::Driver for Manual {
///     fn now(&self) -> Monotonic {
///         Monotonic(self.0.load(Ordering::Relaxed))
///     }
///     fn timer(&self) -> Box<dyn env::clock::Timer> {
///         unimplemented!("this clock never sleeps")
///     }
///     fn block_until(&self, deadline: Monotonic) {
///         self.0.fetch_max(deadline.0, Ordering::Relaxed);
///     }
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Reads the clock. Successive reads never go backwards.
    fn now(&self) -> Monotonic;

    /// Makes a timer for one [`Sleep`].
    fn timer(&self) -> Box<dyn Timer>;

    /// Blocks the calling thread until `deadline`.
    fn block_until(&self, deadline: Monotonic);
}

/// One reusable timer, polled through a [`Sleep`].
///
/// ```
/// use std::task::{Context, Poll};
/// use types::time::Monotonic;
///
/// /// A timer that is always due.
/// struct Due;
///
/// impl env::clock::Timer for Due {
///     fn poll_until(&mut self, _: Monotonic, _: &mut Context<'_>) -> Poll<()> {
///         Poll::Ready(())
///     }
/// }
/// ```
pub trait Timer: Send {
    /// Completes when the clock reaches `deadline`. Until then it returns
    /// `Pending` and wakes `cx` at the deadline. The deadline may change between
    /// polls.
    fn poll_until(&mut self, deadline: Monotonic, cx: &mut Context<'_>) -> Poll<()>;
}

/// A future that completes at a deadline. [`Sleep::reset`] reuses it without an
/// allocation.
///
/// ```
/// use types::time::{Monotonic, Span};
///
/// async fn every_millisecond(clock: &env::Clock, ticks: u32) {
///     let mut sleep = clock.sleep(Span::MILLISECOND);
///     for _ in 0..ticks {
///         (&mut sleep).await;
///         sleep.reset(Monotonic(sleep.deadline().0 + 1_000_000));
///     }
/// }
/// ```
pub struct Sleep {
    deadline: Monotonic,
    timer: Box<dyn Timer>,
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
    ///
    /// ```
    /// fn later(sleep: &mut env::clock::Sleep) {
    ///     let next = types::time::Monotonic(sleep.deadline().0 + 1_000);
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
        this.timer.poll_until(this.deadline, cx)
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
    use super::*;

    mod after {
        use super::*;

        #[test]
        fn adds_a_positive_span() {
            assert_eq!(after(Monotonic(10), Span::from_nanos(5)), Monotonic(15));
        }

        #[test]
        fn clamps_at_the_start_of_the_clock() {
            assert_eq!(after(Monotonic(10), Span::from_nanos(-20)), Monotonic(0));
        }

        #[test]
        fn clamps_at_the_end_of_the_clock() {
            assert_eq!(
                after(Monotonic(u64::MAX - 1), Span::SECOND),
                Monotonic(u64::MAX)
            );
        }
    }
}
