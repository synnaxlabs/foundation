//! The serial ports of one node. A port moves bytes at the line rate of its settings;
//! framing belongs to the protocol above it.
//!
//! Every wait registers the waker of its [`Context`] and returns [`Poll::Pending`], so
//! `sim` controls it. "A thread" below means a thread that `env` started; under `sim`
//! it is a simulated thread.

pub mod port;

use std::fmt;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context, Poll};

/// The serial ports of one node. Clones use the same driver, and any thread may use
/// them.
///
/// ```
/// use std::path::PathBuf;
///
/// fn open(serial: &env::serial::Serial) -> Result<(), env::serial::Error> {
///     let config = env::serial::Config {
///         path: PathBuf::from("/dev/ttyUSB0"),
///         baud: 9_600.try_into().expect("not 0"),
///         parity: env::serial::Parity::Even,
///         stop_bits: env::serial::StopBits::One,
///     };
///     let _port = serial.open(&config)?;
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct Serial(Arc<dyn Driver>);

impl Serial {
    /// Wraps a driver from `os` or `sim`.
    ///
    /// ```
    /// fn wrap(driver: impl env::serial::Driver + 'static) -> env::serial::Serial {
    ///     env::serial::Serial::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// Opens the port at `config.path` with the line settings of `config`. The open
    /// does not wait. Bytes that arrived before it are lost. One handle at a time
    /// holds a port, in this process or another.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] when no port is at the path.
    /// - [`Error::Busy`] when another handle holds the port.
    /// - [`Error::Io`] for other failures, such as settings that the port does not
    ///   take.
    ///
    /// ```
    /// fn open(
    ///     serial: &env::serial::Serial,
    ///     config: &env::serial::Config,
    /// ) -> Result<env::serial::Port, env::serial::Error> {
    ///     serial.open(config)
    /// }
    /// ```
    pub fn open(&self, config: &Config) -> Result<Port, Error> {
        self.0.open(config).map(Port)
    }
}

impl fmt::Debug for Serial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Serial").finish_non_exhaustive()
    }
}

/// The path of a port and its line settings. A character is 1 start bit, 8 data
/// bits, the parity bit if any, and the stop bits.
///
/// ```
/// use std::path::PathBuf;
///
/// let config = env::serial::Config {
///     path: PathBuf::from("/dev/ttyUSB0"),
///     baud: 19_200.try_into().expect("not 0"),
///     parity: env::serial::Parity::None,
///     stop_bits: env::serial::StopBits::Two,
/// };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The path of the port, such as `/dev/ttyUSB0`.
    pub path: PathBuf,
    /// The bits per second.
    pub baud: NonZeroU32,
    /// The parity bit.
    pub parity: Parity,
    /// The stop bits.
    pub stop_bits: StopBits,
}

/// The parity bit of each character.
///
/// ```
/// let parity = env::serial::Parity::Even;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parity {
    /// No parity bit.
    None,
    /// The data bits and the parity bit hold an even count of ones.
    Even,
    /// The data bits and the parity bit hold an odd count of ones.
    Odd,
}

/// The stop bits of each character.
///
/// ```
/// let stop_bits = env::serial::StopBits::One;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopBits {
    /// One stop bit.
    One,
    /// Two stop bits.
    Two,
}

/// An open serial port. It may move to another thread before its first poll. The
/// first poll binds it to the thread that polls, and a poll on any other thread then
/// panics. Dropping it closes the port and loses the bytes not yet sent.
///
/// ```
/// use std::future::poll_fn;
///
/// async fn echo(port: &mut env::serial::Port) -> Result<(), env::serial::Error> {
///     let mut buffer = [0u8; 256];
///     let n = poll_fn(|cx| port.poll_read(cx, &mut buffer)).await?;
///     let mut sent = 0;
///     while sent < n {
///         sent += poll_fn(|cx| port.poll_write(cx, &buffer[sent..n])).await?;
///     }
///     Ok(())
/// }
/// ```
pub struct Port(Box<dyn port::Driver>);

impl Port {
    /// Reads the bytes that arrived, at most `buffer.len()`, and gives the count. It
    /// is ready when at least one byte arrived. A byte with a parity or framing error
    /// is lost, so a check over the bytes, such as a CRC, finds it.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the port failed, such as a USB adapter that was pulled out.
    ///
    /// # Panics
    ///
    /// When `buffer` is empty, and on a thread other than the one of the first poll.
    ///
    /// ```
    /// use std::task::{Context, Poll};
    ///
    /// fn read(
    ///     port: &mut env::serial::Port,
    ///     cx: &mut Context<'_>,
    ///     buffer: &mut [u8],
    /// ) -> Poll<Result<usize, env::serial::Error>> {
    ///     port.poll_read(cx, buffer)
    /// }
    /// ```
    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        assert!(
            !buffer.is_empty(),
            "poll_read needs a buffer of at least one byte"
        );
        self.0.poll_read(cx, buffer)
    }

    /// Queues bytes from `bytes` to send, and gives the count queued, which may be
    /// less than all. It is ready when the send queue has room. The bytes go out at
    /// the line rate, after the bytes queued before them.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the port failed.
    ///
    /// # Panics
    ///
    /// When `bytes` is empty, and on a thread other than the one of the first poll.
    ///
    /// ```
    /// use std::task::{Context, Poll};
    ///
    /// fn write(
    ///     port: &mut env::serial::Port,
    ///     cx: &mut Context<'_>,
    ///     bytes: &[u8],
    /// ) -> Poll<Result<usize, env::serial::Error>> {
    ///     port.poll_write(cx, bytes)
    /// }
    /// ```
    pub fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize, Error>> {
        assert!(!bytes.is_empty(), "poll_write needs at least one byte");
        self.0.poll_write(cx, bytes)
    }
}

impl fmt::Debug for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Port").finish_non_exhaustive()
    }
}

/// Why a serial call failed. Each case names the path of the port.
///
/// ```
/// use std::path::PathBuf;
///
/// let e = env::serial::Error::NotFound { path: PathBuf::from("/dev/ttyS4") };
/// assert_eq!(e.to_string(), "no serial port is at /dev/ttyS4");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No port is at the path.
    NotFound {
        /// The path of the port.
        path: PathBuf,
    },
    /// Another handle holds the port.
    Busy {
        /// The path of the port.
        path: PathBuf,
    },
    /// The OS or the simulation reported another failure.
    Io {
        /// The path of the port.
        path: PathBuf,
        /// The OS error code.
        code: i32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => {
                write!(f, "no serial port is at {}", path.display())
            }
            Self::Busy { path } => {
                write!(
                    f,
                    "serial port {} is open in another handle",
                    path.display()
                )
            }
            Self::Io { path, code } => write!(
                f,
                "serial port {} failed with OS error {code}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for Error {}

/// What `os` and `sim` implement to run a [`Serial`]. Only they implement it.
///
/// Every port it opens follows the thread rules of [`Port`].
///
/// ```
/// fn wrap(driver: impl env::serial::Driver + 'static) -> env::serial::Serial {
///     env::serial::Serial::new(driver)
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Opens a port, with the rules of [`Serial::open`].
    ///
    /// # Errors
    ///
    /// As [`Serial::open`].
    fn open(&self, config: &Config) -> Result<Box<dyn port::Driver>, Error>;
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::task::Waker;

    use super::*;

    /// Gives back each written byte to the next read, and fails a read after
    /// `failed` is set.
    struct Loopback {
        path: PathBuf,
        bytes: Vec<u8>,
        failed: bool,
    }

    impl port::Driver for Loopback {
        fn poll_read(
            &mut self,
            _: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<Result<usize, Error>> {
            if self.failed {
                let path = self.path.clone();
                return Poll::Ready(Err(Error::Io { path, code: 5 }));
            }
            if self.bytes.is_empty() {
                return Poll::Pending;
            }
            let n = buffer.len().min(self.bytes.len());
            buffer[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes.drain(..n);
            Poll::Ready(Ok(n))
        }

        fn poll_write(
            &mut self,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<Result<usize, Error>> {
            self.bytes.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }
    }

    /// Has one port, at `/dev/ttyS0`, and only at 9,600 baud.
    struct Ports;

    impl Driver for Ports {
        fn open(&self, config: &Config) -> Result<Box<dyn port::Driver>, Error> {
            let path = config.path.clone();
            if path != Path::new("/dev/ttyS0") {
                return Err(Error::NotFound { path });
            }
            if config.baud.get() != 9_600 {
                return Err(Error::Io { path, code: 22 });
            }
            Ok(Box::new(Loopback {
                path,
                bytes: Vec::new(),
                failed: false,
            }))
        }
    }

    fn config(path: &str, baud: u32) -> Config {
        Config {
            path: PathBuf::from(path),
            baud: NonZeroU32::new(baud).expect("not 0"),
            parity: Parity::Even,
            stop_bits: StopBits::One,
        }
    }

    /// A port at `/dev/ttyS0` that holds `bytes` to read.
    fn port(bytes: Vec<u8>, failed: bool) -> Port {
        let path = PathBuf::from("/dev/ttyS0");
        Port(Box::new(Loopback {
            path,
            bytes,
            failed,
        }))
    }

    fn cx() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    #[test]
    fn moves_ports_between_threads() {
        fn movable<T: Send>() {}
        fn shared<T: Send + Sync + Clone>() {}
        movable::<Port>();
        shared::<Serial>();
    }

    mod open {
        use super::*;

        #[test]
        fn gives_the_port_at_the_path() {
            let serial = Serial::new(Ports);
            let mut port = serial.open(&config("/dev/ttyS0", 9_600)).expect("a port");
            assert_eq!(port.poll_write(&mut cx(), &[1, 2, 3]), Poll::Ready(Ok(3)));
            let mut buffer = [0; 8];
            assert_eq!(port.poll_read(&mut cx(), &mut buffer), Poll::Ready(Ok(3)));
            assert_eq!(buffer[..3], [1, 2, 3]);
        }

        #[test]
        fn gives_the_error_of_the_driver() {
            let serial = Serial::new(Ports);
            let e = serial
                .open(&config("/dev/ttyS1", 9_600))
                .expect_err("no port");
            let path = PathBuf::from("/dev/ttyS1");
            assert_eq!(e, Error::NotFound { path });
            let e = serial.open(&config("/dev/ttyS0", 7)).expect_err("bad baud");
            let path = PathBuf::from("/dev/ttyS0");
            assert_eq!(e, Error::Io { path, code: 22 });
        }
    }

    mod polls {
        use super::*;

        #[test]
        fn read_at_most_the_buffer() {
            let mut port = port(vec![7, 8, 9], false);
            let mut buffer = [0; 2];
            assert_eq!(port.poll_read(&mut cx(), &mut buffer), Poll::Ready(Ok(2)));
            assert_eq!(buffer, [7, 8]);
            assert_eq!(port.poll_read(&mut cx(), &mut buffer), Poll::Ready(Ok(1)));
            assert_eq!(buffer[0], 9);
        }

        #[test]
        fn wait_while_the_driver_waits() {
            let mut port = port(Vec::new(), false);
            assert_eq!(port.poll_read(&mut cx(), &mut [0; 8]), Poll::Pending);
        }

        #[test]
        fn give_the_error_of_the_driver() {
            let mut port = port(Vec::new(), true);
            let path = PathBuf::from("/dev/ttyS0");
            let e = Error::Io { path, code: 5 };
            assert_eq!(port.poll_read(&mut cx(), &mut [0; 8]), Poll::Ready(Err(e)));
        }

        #[test]
        #[should_panic(expected = "poll_read needs a buffer of at least one byte")]
        fn panic_on_a_read_into_an_empty_buffer() {
            drop(port(Vec::new(), false).poll_read(&mut cx(), &mut []));
        }

        #[test]
        #[should_panic(expected = "poll_write needs at least one byte")]
        fn panic_on_a_write_of_no_bytes() {
            drop(port(Vec::new(), false).poll_write(&mut cx(), &[]));
        }
    }

    mod error {
        use super::*;

        #[test]
        fn names_the_path() {
            let path = PathBuf::from("/dev/ttyUSB0");
            let cases = [
                (
                    Error::NotFound { path: path.clone() },
                    "no serial port is at /dev/ttyUSB0",
                ),
                (
                    Error::Busy { path: path.clone() },
                    "serial port /dev/ttyUSB0 is open in another handle",
                ),
                (
                    Error::Io { path, code: 5 },
                    "serial port /dev/ttyUSB0 failed with OS error 5",
                ),
            ];
            for (e, message) in cases {
                assert_eq!(e.to_string(), message);
            }
        }
    }

    mod debug {
        use super::*;

        #[test]
        fn hides_the_driver() {
            assert_eq!(format!("{:?}", Serial::new(Ports)), "Serial { .. }");
            assert_eq!(format!("{:?}", port(Vec::new(), false)), "Port { .. }");
        }
    }
}
