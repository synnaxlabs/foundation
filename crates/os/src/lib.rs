//! Implements the `env` seams on the real operating system: monotonic and wall clocks,
//! files, randomness, and threads, and the memory of block pools. The only crate
//! allowed to call them.

use std::fmt;
use std::path::Path;

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "rustix has no call that allocates all of a file"
)]
mod allocate;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[expect(unsafe_code, reason = "the clock is an OS call")]
mod clock;
mod cores;
mod entropy;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod files;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[expect(unsafe_code, reason = "a pool's memory is an OS mapping")]
pub mod memory;
#[cfg(all(feature = "net", any(target_os = "linux", target_os = "macos")))]
mod net;
mod shards;
mod thread;
mod threads;
mod unwind;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[expect(unsafe_code, reason = "the wall clock is an OS call")]
mod wall;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use files::Disk;

/// The monotonic clock of this node, which counts time while the machine sleeps. Each
/// call starts a new clock at `Monotonic(0)`. Clones read the same clock. Never
/// compare readings of clocks from two calls.
///
/// A sleep is made on the Tokio runtime current at the call, and panics when there is
/// none or it has no timer. Each thread that `os` starts has one with a timer, but a
/// runtime that its body starts may not. A sleep completes about 2 ms late on an idle
/// machine, and later under load. A sleep that waits across a suspend completes up to
/// 1 s late.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[must_use]
pub fn clock() -> env::clock::Clock {
    env::clock::Clock::new(clock::Driver::new())
}

/// The OS wall clock and its error bound, read in one call that needs no privilege.
/// The bound is `None` when the OS says its clock is not in sync, or gives a bound
/// that is negative or past the end of a `Span`.
///
/// # Errors
///
/// [`Error::Wall`] when the OS refuses a read, as a seccomp filter can.
///
/// # Panics
///
/// A read panics when the OS refuses a later call.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn wall() -> Result<env::wall::Wall, Error> {
    wall::read().map_err(Error::Wall)?;
    Ok(env::wall::Wall::new(wall::Driver))
}

/// The random source of the OS. Its bytes are fit for keys and nonces.
///
/// # Panics
///
/// A fill panics when the OS gives no random bytes.
#[must_use]
pub fn entropy() -> env::entropy::Entropy {
    env::entropy::Entropy::new(entropy::Driver)
}

/// Shards on OS threads, each with its own Tokio runtime. The core count is read once
/// from the thread that calls this. On Linux it is the size of the affinity set, and
/// core `i` of [`env::shards::Config::core`] pins to the `i`-th CPU of the set. Only
/// Linux can pin: elsewhere [`env::shards::Shards::pinnable`] is `false`.
///
/// A panic ends the shard only where panics unwind, as in tests. A release build
/// aborts the process at a panic. Tokio catches a panic in the poll or the drop of a
/// task that code spawns with `tokio::spawn` or `tokio::task::spawn_local`, not
/// through [`env::tasks::Tasks`], and the shard runs on. Tokio drops such a task
/// during the unwind of a panic in its poll, so a panic in that drop aborts the
/// process. A panic in the drop of the payload of a panic can escape Tokio's catches
/// and end the shard.
///
/// # Errors
///
/// [`Error::Cores`] when the OS cannot give the cores of this thread.
pub fn shards() -> Result<env::shards::Shards, Error> {
    let cores = cores::Cores::read().map_err(Error::Cores)?;
    Ok(env::shards::Shards::new(shards::Driver::new(cores)))
}

/// Dedicated threads, each on its own OS thread with a current-thread Tokio runtime.
/// The cores are read once, as in [`shards`]. On Linux each thread may run on every
/// CPU of the affinity set, whatever thread starts it.
///
/// The body runs in the context of the runtime but outside its `block_on`, so it may
/// start and block on a Tokio runtime of its own. A panic of a body, in its call, its
/// poll, or its drop, makes its join give [`env::thread::Panicked`], where panics
/// unwind, but a panic in a drop during the unwind of a panic aborts the process.
/// Tokio catches a panic in the poll or the drop of a task that the body spawns with
/// `tokio::spawn` or `tokio::task::spawn_local`, and the thread runs on. Tokio drops
/// such a task during the unwind of a panic in its poll, so a panic in that drop
/// aborts the process. A panic in the drop of the payload of a panic can escape
/// Tokio's catches and end the thread.
///
/// # Errors
///
/// [`Error::Cores`] when the OS cannot give the cores of this thread.
pub fn threads() -> Result<env::threads::Threads, Error> {
    let cores = cores::Cores::read().map_err(Error::Cores)?;
    Ok(env::threads::Threads::new(threads::Driver::new(cores)))
}

/// The network of this machine. A stream or listener registers at its first poll
/// with the I/O driver of the Tokio runtime current on that thread. Each thread that
/// `os` starts has one.
///
/// [`env::net::Net::resolve`] looks up a host name as each other program on this
/// machine does, on an OS thread of its own for each lookup.
///
/// # Panics
///
/// - A poll of [`env::net::Net::connect`], or the first poll of a stream or listener,
///   on a thread with no Tokio runtime or with no I/O driver.
/// - [`env::net::Net::udp`]: this driver has no UDP yet.
#[cfg(all(feature = "net", any(target_os = "linux", target_os = "macos")))]
#[must_use]
pub fn net() -> env::net::Net {
    env::net::Net::new(net::Driver)
}

/// The real disk under `dir/data`, which it makes when it is not there, and the
/// handle of its I/O thread. `os` keeps its own entries in `dir`, so give it a
/// directory that nothing else uses. `threads` starts I/O thread `name`, which runs
/// each call of the disk and of its files in the order they reach it, and ends after
/// the disk and its files drop. Give each shard a disk of its own.
///
/// # Errors
///
/// - [`Error::Dir`] when the OS cannot open `dir`, or open or make `dir/data`.
/// - [`Error::Thread`] when the I/O thread cannot start.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn files(
    dir: &Path,
    threads: &env::threads::Threads,
    name: &str,
) -> Result<(Disk, env::thread::Handle), Error> {
    Disk::new(dir, threads, name)
}

/// Why `os` could not build a seam.
#[derive(Debug)]
pub enum Error {
    /// The OS could not give the cores of the calling thread.
    Cores(std::io::Error),
    /// The OS could not open or make the data directory.
    Dir(std::io::Error),
    /// The I/O thread could not start.
    Thread(env::thread::Error),
    /// The OS refused a read of its wall clock.
    Wall(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cores(e) => write!(f, "cannot read the cores of this thread: {e}"),
            Self::Dir(e) => write!(f, "cannot open the data directory: {e}"),
            Self::Thread(e) => write!(f, "{e}"),
            Self::Wall(e) => write!(f, "cannot read the wall clock: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cores(e) | Self::Dir(e) | Self::Wall(e) => Some(e),
            Self::Thread(e) => std::error::Error::source(e),
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

    #[test]
    fn a_wall_error_names_the_os_error_as_its_source() {
        let e = Error::Wall(std::io::Error::other("no clock"));
        assert_eq!(e.to_string(), "cannot read the wall clock: no clock");
        let source = std::error::Error::source(&e).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("no clock"));
    }

    #[test]
    fn a_thread_error_shows_as_itself() {
        let name = "files".into();
        let reason = "no memory".into();
        let e = Error::Thread(env::thread::Error::Start { name, reason });
        assert_eq!(e.to_string(), "cannot start thread files: no memory");
        assert!(std::error::Error::source(&e).is_none());
    }
}
