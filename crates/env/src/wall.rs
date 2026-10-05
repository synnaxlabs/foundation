//! The operating system's wall clock.

use std::fmt;
use std::sync::Arc;

use types::time::Stamp;

/// The operating system's wall clock. Clones read the same clock.
///
/// `node` hands it only to `clock`, which turns it into mesh time. Its readings are
/// the OS's guess at UTC: another program may step them in either direction.
///
/// ```
/// fn read(wall: &env::wall::Wall) -> types::time::Stamp {
///     wall.now()
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

    /// Reads the OS wall clock. Only `clock` calls it; a lint denies it elsewhere.
    ///
    /// ```
    /// fn read(wall: &env::wall::Wall) -> i64 {
    ///     wall.now().nanos()
    /// }
    /// ```
    #[must_use]
    pub fn now(&self) -> Stamp {
        self.0.now()
    }
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
    /// Reads the OS wall clock.
    fn now(&self) -> Stamp;
}
