//! Implements the `env` seams on the real operating system: monotonic and wall clocks,
//! files, randomness, and threads. The only crate allowed to call them.

use std::fmt;

mod cores;
mod shards;

/// Shards on OS threads, each with its own Tokio runtime. The core count is read once
/// from the thread that calls this. On Linux it is the size of the affinity set, and
/// core `i` of [`env::shards::Config::core`] pins to the `i`-th CPU of the set. Only
/// Linux can pin: elsewhere [`env::shards::Shards::pinnable`] is `false`.
///
/// A panic ends the shard only where panics unwind, as in tests. A release build
/// aborts the process at a panic.
///
/// # Errors
///
/// [`Error::Cores`] when the OS cannot give the cores of this thread.
pub fn shards() -> Result<env::shards::Shards, Error> {
    let cores = cores::Cores::read().map_err(Error::Cores)?;
    Ok(env::shards::Shards::new(shards::Driver::new(cores)))
}

/// Why `os` could not build a seam.
#[derive(Debug)]
pub enum Error {
    /// The OS could not give the cores of the calling thread.
    Cores(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cores(e) => write!(f, "cannot read the cores of this thread: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cores(e) => Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cores_error_names_the_os_error_as_its_source() {
        let e = Error::Cores(std::io::Error::other("no affinity"));
        assert_eq!(
            e.to_string(),
            "cannot read the cores of this thread: no affinity"
        );
        let source = std::error::Error::source(&e).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("no affinity"));
    }
}
