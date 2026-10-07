//! `Scope`, for the bench target only. Not a stable surface.

use std::fmt;

/// Futures that run on `env::tasks` and drop together, as `node` runs each session and
/// stream.
pub struct Scope(crate::scope::Scope);

impl Scope {
    /// An empty scope whose futures run on `tasks`.
    #[must_use]
    pub fn new(tasks: env::tasks::Tasks) -> Self {
        Self(crate::scope::Scope::new(tasks))
    }

    /// Runs `future` on the scope's tasks until it completes or the scope drops.
    pub fn spawn(&mut self, future: env::tasks::Task) {
        self.0.spawn(future);
    }
}

impl fmt::Debug for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Scope")
    }
}
