//! Implements the `env` seams on the real operating system: monotonic and wall clocks,
//! files, randomness, and threads, and the memory of block pools. The only crate
//! allowed to call them. It also holds the signals that ask the process to stop.
//!
//! # Panics in shards and threads
//!
//! A panic ends its shard or dedicated thread only where panics unwind, as in tests. A
//! release build aborts the process at a panic. As anywhere in Rust, a panic that
//! unwinds into the unwind of another panic aborts the process. Tokio catches a panic
//! in the poll or the drop of a task that code spawns with `tokio::spawn`, or on a
//! shard with `tokio::task::spawn_local`, not through [`env::tasks::Tasks`], and the
//! shard or thread runs on. Tokio drops such a task during the unwind of a panic in its
//! poll, so a panic that unwinds out of that drop aborts the process. A panic in the
//! drop of the payload of a panic can escape Tokio's catches and end the shard or
//! thread.

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
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[expect(unsafe_code, reason = "a signal mask is an OS call")]
mod signal;
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
/// A panic ends a shard as the crate doc states
/// ([panics](crate#panics-in-shards-and-threads)).
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
/// start and block on a Tokio runtime of its own. A panic in the call, the poll, or the
/// drop of a body ends its thread as the crate doc states
/// ([panics](crate#panics-in-shards-and-threads)).
///
/// # Errors
///
/// [`Error::Cores`] when the OS cannot give the cores of this thread.
pub fn threads() -> Result<env::threads::Threads, Error> {
    let cores = cores::Cores::read().map_err(Error::Cores)?;
    Ok(env::threads::Threads::new(threads::Driver::new(cores)))
}

/// The network of this machine. A stream, listener, UDP sender, or UDP receiver
/// registers with the I/O driver of the Tokio runtime current on the thread of its
/// first poll, at the first poll that needs the socket. A stream write of no bytes
/// does not, and a UDP poll that fails before it registers leaves that to the next
/// poll. A UDP sender drops its registration at each send that ends, with or without
/// an error, and registers again at the next poll that finds the send buffer full. A
/// send that the caller drops while it waits keeps the registration until the next
/// send of that sender ends. Each thread that `os` starts has a runtime with an I/O
/// driver. Needs the cargo feature `net`.
///
/// Each socket that `os` opens is closed on exec. On macOS, a child that another
/// thread spawns while `os` opens or accepts a socket may hold it, and its port, until
/// the child ends.
///
/// [`env::net::Net::resolve`] looks up a host name as each other program on this
/// machine does, on an OS thread of its own for each lookup. On macOS, a child that
/// another thread spawns during a lookup may hold the sockets that the C library opens
/// for it, and a child spawned after a lookup may hold a socket that the C library
/// keeps open.
///
/// # Panics
///
/// A poll of [`env::net::Net::connect`], or a poll that registers a socket, on a
/// thread with no Tokio runtime or with no I/O driver.
#[cfg(all(feature = "net", any(target_os = "linux", target_os = "macos")))]
#[must_use]
pub fn net() -> env::net::Net {
    env::net::Net::new(net::Driver)
}

/// The real disk under `dir/data`, and the handle of its I/O thread. It makes `dir`
/// and `dir/data` when they are not there, but not the parents of `dir`. `os` keeps
/// its own entries in `dir`, so give it a directory that nothing else uses. `threads`
/// starts I/O thread `name`, which runs each call of the disk and of its files in the
/// order they reach it, and ends after the disk and its files drop. Give each shard
/// a disk of its own. The mode of each file and directory that it makes gives the
/// group and other users no access. It does not change the mode of a file or
/// directory that is there.
///
/// # Errors
///
/// - [`Error::Dir`] when the OS cannot open or make `dir` or `dir/data`, or cannot open
///   or sync the directory that holds each.
/// - [`Error::Thread`] when the I/O thread cannot start.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn files(
    dir: &Path,
    threads: &env::threads::Threads,
    name: &str,
) -> Result<(Disk, env::thread::Handle), Error> {
    Disk::new(dir, threads, name)
}

/// Holds SIGINT and SIGTERM: the first that comes does not end the process, and the
/// future completes at it, also when it came before the first poll. A second one
/// ends the process as it does with no hold, so a stop that hangs can still be
/// ended. Call it once, on the main thread, before the process starts any other
/// thread: a thread that was there before still takes them, and ends the process
/// on one. It takes a signal sent to the process, as Ctrl-C and `kill` send it, not
/// one sent to a single thread.
///
/// # Errors
///
/// [`Error::Thread`] when the thread that waits for them cannot start.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn interrupt() -> Result<impl Future<Output = ()> + Send + 'static, Error> {
    let (fire, fired) = tokio::sync::oneshot::channel();
    // The future may be gone, as when the process stops on its own.
    signal::hold(move || fire.send(()).unwrap_or(())).map_err(Error::Thread)?;
    Ok(wait(fired))
}

/// Completes when the signal thread fires `fired`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wait(fired: tokio::sync::oneshot::Receiver<()>) {
    fired
        .await
        .expect("invariant: the signal thread fires before it ends");
}

/// Why `os` could not build a seam or hold the signals.
#[derive(Debug)]
pub enum Error {
    /// The OS could not give the cores of the calling thread.
    Cores(std::io::Error),
    /// The OS could not open, make, or sync the data directory or its parent.
    Dir(std::io::Error),
    /// The OS could not give the available memory.
    Memory(std::io::Error),
    /// A thread of `os` could not start: the I/O thread of [`files`] or the thread
    /// of [`interrupt`].
    Thread(env::thread::Error),
    /// The OS refused a read of its wall clock.
    Wall(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cores(e) => write!(f, "cannot read the cores of this thread: {e}"),
            Self::Dir(e) => {
                write!(
                    f,
                    "cannot open, make, or sync the data directory or its parent: {e}"
                )
            }
            Self::Memory(e) => write!(f, "cannot read the available memory: {e}"),
            Self::Thread(e) => write!(f, "{e}"),
            Self::Wall(e) => write!(f, "cannot read the wall clock: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cores(e) | Self::Dir(e) | Self::Memory(e) | Self::Wall(e) => Some(e),
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
    fn a_memory_error_names_the_os_error_as_its_source() {
        let e = Error::Memory(std::io::Error::other("no meminfo"));
        assert_eq!(
            e.to_string(),
            "cannot read the available memory: no meminfo"
        );
        let source = std::error::Error::source(&e).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("no meminfo"));
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
