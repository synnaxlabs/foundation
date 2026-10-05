//! Bounded single-producer, single-consumer rings that carry values between shards,
//! and the wake protocol for a consumer that has nothing to do.
//!
//! A producer never waits: a full ring gives the value back. A consumer spins for a
//! set number of checks before it parks, and the producer wakes it.

use std::fmt;

/// Settings for one ring.
#[derive(Clone, Debug)]
pub struct Config {
    /// The most values the ring holds at once.
    pub capacity: usize,
    /// Checks a waiting consumer makes before it parks. Use 0 on a single core.
    pub spins: u32,
}

/// Creates a ring and returns its two ends.
#[must_use]
#[expect(clippy::needless_pass_by_value, reason = "stub until implemented")]
pub fn new<T: Send>(config: Config) -> (Producer<T>, Consumer<T>) {
    let _ = config;
    todo!()
}

/// The sending end of a ring.
pub struct Producer<T> {
    _value: std::marker::PhantomData<T>,
}

impl<T: Send> Producer<T> {
    /// Adds a value without waiting, and wakes a parked consumer.
    ///
    /// # Errors
    ///
    /// [`Full`] holds the value when the ring has no room.
    pub fn push(&mut self, value: T) -> Result<(), Full<T>> {
        drop(value);
        todo!()
    }

    /// Values in the ring now.
    #[must_use]
    pub fn len(&self) -> usize {
        todo!()
    }

    /// Reports whether the ring holds no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        todo!()
    }
}

/// The receiving end of a ring.
pub struct Consumer<T> {
    _value: std::marker::PhantomData<T>,
}

impl<T: Send> Consumer<T> {
    /// Takes the next value if there is one, without waiting.
    pub fn try_pop(&mut self) -> Option<T> {
        todo!()
    }

    /// Waits for the next value. Returns `None` when the producer is gone and the ring
    /// is empty.
    pub async fn pop(&mut self) -> Option<T> {
        todo!()
    }

    /// Values in the ring now.
    #[must_use]
    pub fn len(&self) -> usize {
        todo!()
    }

    /// Reports whether the ring holds no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        todo!()
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

// Deliberate break: this crate names unsafe_code but runs no tests under Miri.

#[cfg(all(
    test,
    loom,
    not(miri),
    not(shuttle),
    not(fuzzing),
    target_pointer_width = "64"
))]
mod loom_break {
    #[test]
    fn fails_under_loom() {
        panic!("deliberate break: a loom test whose cfg rustfmt splits");
    }
}

#[cfg(all(test, shuttle))]
mod shuttle_break {
    #[test]
    fn fails_under_shuttle() {
        panic!("deliberate break: a shuttle test");
    }
}
