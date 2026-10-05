//! The operating system's wall clock.

use std::fmt;
use std::sync::Arc;

use types::time::{Span, Stamp};

/// The operating system's wall clock. Clones read the same clock.
///
/// `node` hands it only to `clock`, which turns it into mesh time. Its readings are
/// the OS's guess at UTC: another program may step them in either direction.
///
/// ```
/// fn read(wall: &env::wall::Wall) -> types::time::Stamp {
///     wall.now().time
/// }
/// ```
#[derive(Clone)]
pub struct Wall(Arc<dyn Driver>);

impl Wall {
    /// Wraps a driver from `os` or `sim`.
    ///
    /// ```
    /// fn wrap(driver: impl env::wall::Driver + 'static) -> env::wall::Wall {
    ///     env::wall::Wall::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// Reads the OS wall clock and its error bound in one call. Only `clock` calls
    /// it; a lint denies it elsewhere.
    ///
    /// ```
    /// fn read(wall: &env::wall::Wall) -> Option<types::time::Span> {
    ///     wall.now().error
    /// }
    /// ```
    #[must_use]
    pub fn now(&self) -> Reading {
        self.0.now()
    }
}

/// One reading of the OS wall clock.
///
/// ```
/// /// A reading from an OS that gives no error bound.
/// fn unbounded(time: types::time::Stamp) -> env::wall::Reading {
///     env::wall::Reading { time, error: None }
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// The OS's guess at UTC.
    pub time: Stamp,
    /// The most that `time` can be from UTC, by the OS's own count, or `None` when
    /// the OS gives no bound.
    pub error: Option<Span>,
}

impl fmt::Debug for Wall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wall").finish_non_exhaustive()
    }
}

/// What `os` and `sim` implement to run a [`Wall`]. Only they implement it.
///
/// ```
/// fn wrap(driver: impl env::wall::Driver + 'static) -> env::wall::Wall {
///     env::wall::Wall::new(driver)
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Reads the OS wall clock and its error bound in one call.
    fn now(&self) -> Reading;
}
