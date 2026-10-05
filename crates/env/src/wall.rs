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
/// fn read(wall: &env::Wall) -> types::time::Stamp {
///     wall.now()
/// }
/// ```
#[derive(Clone)]
pub struct Wall(Arc<dyn Driver>);

impl Wall {
    /// Wraps a driver.
    ///
    /// ```
    /// # struct Epoch;
    /// # impl env::wall::Driver for Epoch {
    /// #     fn now(&self) -> types::time::Stamp { types::time::Stamp::EPOCH }
    /// # }
    /// let wall = env::Wall::new(Epoch);
    /// assert_eq!(wall.now(), types::time::Stamp::EPOCH);
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// Reads the OS wall clock.
    ///
    /// ```
    /// fn read(wall: &env::Wall) -> i64 {
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

/// What `os` and `sim` implement to run a [`Wall`].
///
/// ```
/// use types::time::Stamp;
///
/// /// A wall clock stuck at the epoch.
/// struct Epoch;
///
/// impl env::wall::Driver for Epoch {
///     fn now(&self) -> Stamp {
///         Stamp::EPOCH
///     }
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Reads the OS wall clock.
    fn now(&self) -> Stamp;
}
