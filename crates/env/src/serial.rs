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
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use types::time::Rate;

/// The serial ports of one node. Clones use the same driver, and any thread may use
/// them.
///
/// ```
/// use std::path::PathBuf;
///
/// async fn open(serial: &env::serial::Serial) -> Result<(), env::serial::Error> {
///     let config = env::serial::Config {
///         path: PathBuf::from("/dev/ttyUSB0"),
///         settings: env::serial::Settings {
///             baud: 9_600.try_into().expect("not 0"),
///             parity: Some(env::serial::Parity::Even),
///             stop_bits: env::serial::StopBits::One,
///         },
///     };
///     let _port = serial.open(&config).await?;
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

    /// Opens the port at `config.path` with the line settings of `config`. Dropping
    /// the future stops the open. Bytes that arrived before the open are lost. While a
    /// handle holds a port, another open of it gets [`Error::Busy`]. Under `os`, a
    /// process with root rights, or one that opened the port first with no lock, can
    /// still use it.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] when no port is at the path.
    /// - [`Error::Busy`] when another handle holds the port.
    /// - [`Error::Io`] for other failures, such as settings that the port does not
    ///   take.
    ///
    /// ```
    /// async fn open(
    ///     serial: &env::serial::Serial,
    ///     config: &env::serial::Config,
    /// ) -> Result<env::serial::Port, env::serial::Error> {
    ///     serial.open(config).await
    /// }
    /// ```
    pub async fn open(&self, config: &Config) -> Result<Port, Error> {
        let driver = self.0.open(config).await?;
        let path = config.path.clone();
        Ok(Port { path, driver })
    }
}

impl fmt::Debug for Serial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Serial").finish_non_exhaustive()
    }
}

/// The path of a port and its line settings.
///
/// ```
/// use std::path::PathBuf;
///
/// let config = env::serial::Config {
///     path: PathBuf::from("/dev/ttyUSB0"),
///     settings: env::serial::Settings {
///         baud: 19_200.try_into().expect("not 0"),
///         parity: None,
///         stop_bits: env::serial::StopBits::Two,
///     },
/// };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The path of the port, such as `/dev/ttyUSB0`.
    pub path: PathBuf,
    /// The line settings.
    pub settings: Settings,
}

/// The line settings of a port. A character is 1 start bit, 8 data bits, the parity
/// bit if any, and the stop bits.
///
/// ```
/// let settings = env::serial::Settings {
///     baud: 9_600.try_into().expect("not 0"),
///     parity: Some(env::serial::Parity::Even),
///     stop_bits: env::serial::StopBits::One,
/// };
/// assert_eq!(settings.rate().span(1).nanos(), 1_145_833);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Settings {
    /// The bits per second.
    pub baud: NonZeroU32,
    /// The parity bit, or `None` for none.
    pub parity: Option<Parity>,
    /// The stop bits.
    pub stop_bits: StopBits,
}

impl Settings {
    /// The characters per second. `rate().span(n)` is the time that `n` characters
    /// sent back to back take.
    #[must_use]
    #[expect(clippy::missing_panics_doc, reason = "a character takes 2 ns to 12 s")]
    pub fn rate(self) -> Rate {
        let parity = u64::from(self.parity.is_some());
        let stop = match self.stop_bits {
            StopBits::One => 1,
            StopBits::Two => 2,
        };
        let bits = 1 + 8 + parity + stop;
        Rate::new(u64::from(self.baud.get()), bits)
            .expect("invariant: a character takes from 2 ns to 12 s")
    }
}

/// The parity bit of each character.
///
/// ```
/// let parity = env::serial::Parity::Even;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Parity {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StopBits {
    /// One stop bit.
    One,
    /// Two stop bits.
    Two,
}

/// An open serial port. It may move to another thread before its first poll. The
/// first poll binds it to the thread that polls, and a poll on any other thread then
/// panics. Dropping it closes the port without a wait and loses the bytes not yet
/// sent.
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
pub struct Port {
    path: PathBuf,
    driver: Box<dyn port::Driver>,
}

impl Port {
    /// Reads the bytes that arrived, at most `buffer.len()`, and gives the count. It
    /// is ready when at least one byte arrived, so a ready count is at least 1. A
    /// byte with a parity or framing error, or a break, is lost, so a check over the
    /// bytes, such as a CRC, finds it.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the port failed. A hangup, such as a USB adapter that was
    /// pulled out, gives `EIO`.
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
        self.driver.poll_read(cx, buffer)
    }

    /// Queues bytes from `bytes` to send, and gives the count queued, which may be
    /// less than all. It is ready when the send queue has room, so a ready count is
    /// at least 1, except for empty `bytes`, which give 0 at once. The bytes go out at
    /// the line rate, after the bytes queued before them.
    ///
    /// # Errors
    ///
    /// As [`Port::poll_read`].
    ///
    /// # Panics
    ///
    /// On a thread other than the one of the first poll.
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
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        self.driver.poll_write(cx, bytes)
    }
}

impl fmt::Debug for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Port")
            .field("path", &self.path)
            .finish_non_exhaustive()
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

/// An open in flight, as a [`Driver`] gives it.
///
/// ```
/// fn absent(path: std::path::PathBuf) -> env::serial::Open<'static> {
///     Box::pin(async move { Err(env::serial::Error::NotFound { path }) })
/// }
/// ```
pub type Open<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn port::Driver>, Error>> + 'a>>;

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
    fn open<'a>(&'a self, config: &'a Config) -> Open<'a>;
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::task::Waker;

    use super::*;

    /// Gives back each written byte to the next read. It holds at most `room` bytes
    /// not read, and fails each poll when `failed` is set.
    struct Loopback {
        path: PathBuf,
        bytes: Vec<u8>,
        room: usize,
        failed: bool,
    }

    impl Loopback {
        fn failure(&self) -> Option<Poll<Result<usize, Error>>> {
            let path = self.path.clone();
            self.failed
                .then_some(Poll::Ready(Err(Error::Io { path, code: 5 })))
        }
    }

    impl port::Driver for Loopback {
        fn poll_read(
            &mut self,
            _: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<Result<usize, Error>> {
            if let Some(failure) = self.failure() {
                return failure;
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
            assert!(!bytes.is_empty(), "the driver got no bytes to write");
            if let Some(failure) = self.failure() {
                return failure;
            }
            let n = bytes.len().min(self.room - self.bytes.len());
            if n == 0 {
                return Poll::Pending;
            }
            self.bytes.extend_from_slice(&bytes[..n]);
            Poll::Ready(Ok(n))
        }
    }

    /// Has one port, at `/dev/ttyS0`, and only at 9,600 baud. An open at 1 baud never
    /// ends.
    struct Ports;

    impl Driver for Ports {
        fn open<'a>(&'a self, config: &'a Config) -> Open<'a> {
            Box::pin(async move {
                let path = config.path.clone();
                if path != Path::new("/dev/ttyS0") {
                    return Err(Error::NotFound { path });
                }
                match config.settings.baud.get() {
                    9_600 => {}
                    1 => std::future::pending().await,
                    _ => return Err(Error::Io { path, code: 22 }),
                }
                let port: Box<dyn port::Driver> = Box::new(Loopback {
                    path,
                    bytes: Vec::new(),
                    room: 8,
                    failed: false,
                });
                Ok(port)
            })
        }
    }

    fn settings(baud: u32, parity: Option<Parity>, stop_bits: StopBits) -> Settings {
        Settings {
            baud: NonZeroU32::new(baud).expect("not 0"),
            parity,
            stop_bits,
        }
    }

    fn config(path: &str, baud: u32) -> Config {
        Config {
            path: PathBuf::from(path),
            settings: settings(baud, Some(Parity::Even), StopBits::One),
        }
    }

    /// A port at `/dev/ttyS0` that holds `bytes` to read, with room for 4 bytes.
    fn port(bytes: Vec<u8>, failed: bool) -> Port {
        let path = PathBuf::from("/dev/ttyS0");
        let driver = Box::new(Loopback {
            path: path.clone(),
            bytes,
            room: 4,
            failed,
        });
        Port { path, driver }
    }

    fn cx() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    fn open(serial: &Serial, config: &Config) -> Poll<Result<Port, Error>> {
        std::pin::pin!(serial.open(config)).poll(&mut cx())
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
            let Poll::Ready(Ok(mut port)) = open(&serial, &config("/dev/ttyS0", 9_600))
            else {
                panic!("the open gave no port");
            };
            assert_eq!(port.poll_write(&mut cx(), &[1, 2, 3]), Poll::Ready(Ok(3)));
            let mut buffer = [0; 8];
            assert_eq!(port.poll_read(&mut cx(), &mut buffer), Poll::Ready(Ok(3)));
            assert_eq!(buffer[..3], [1, 2, 3]);
        }

        #[test]
        fn gives_the_error_of_the_driver() {
            let serial = Serial::new(Ports);
            let path = PathBuf::from("/dev/ttyS1");
            let e = Error::NotFound { path };
            let absent = open(&serial, &config("/dev/ttyS1", 9_600));
            assert!(matches!(absent, Poll::Ready(Err(ref found)) if *found == e));
            let path = PathBuf::from("/dev/ttyS0");
            let e = Error::Io { path, code: 22 };
            let refused = open(&serial, &config("/dev/ttyS0", 7));
            assert!(matches!(refused, Poll::Ready(Err(ref found)) if *found == e));
        }

        #[test]
        fn waits_while_the_driver_waits() {
            let serial = Serial::new(Ports);
            let opening = open(&serial, &config("/dev/ttyS0", 1));
            assert!(opening.is_pending());
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
        fn write_at_most_the_room_of_the_driver() {
            let mut port = port(vec![7], false);
            assert_eq!(
                port.poll_write(&mut cx(), &[1, 2, 3, 4]),
                Poll::Ready(Ok(3))
            );
        }

        #[test]
        fn write_no_bytes_at_once_without_the_driver() {
            let mut port = port(vec![1, 2, 3, 4], true);
            assert_eq!(port.poll_write(&mut cx(), &[]), Poll::Ready(Ok(0)));
        }

        #[test]
        fn wait_while_the_driver_waits() {
            let mut port = port(vec![1, 2, 3, 4], false);
            assert_eq!(port.poll_write(&mut cx(), &[5]), Poll::Pending);
            assert_eq!(port.poll_read(&mut cx(), &mut [0; 8]), Poll::Ready(Ok(4)));
            assert_eq!(port.poll_read(&mut cx(), &mut [0; 8]), Poll::Pending);
        }

        #[test]
        fn give_the_error_of_the_driver() {
            let mut port = port(Vec::new(), true);
            let e = Error::Io {
                path: PathBuf::from("/dev/ttyS0"),
                code: 5,
            };
            let read = port.poll_read(&mut cx(), &mut [0; 8]);
            assert_eq!(read, Poll::Ready(Err(e.clone())));
            assert_eq!(port.poll_write(&mut cx(), &[1]), Poll::Ready(Err(e)));
        }

        #[test]
        #[should_panic(expected = "poll_read needs a buffer of at least one byte")]
        fn panic_on_a_read_into_an_empty_buffer() {
            drop(port(Vec::new(), false).poll_read(&mut cx(), &mut []));
        }
    }

    mod rate {
        use super::*;

        #[test]
        fn counts_each_bit_of_a_character() {
            let cases = [
                (None, StopBits::One, 10),
                (Some(Parity::Even), StopBits::One, 11),
                (Some(Parity::Odd), StopBits::Two, 12),
            ];
            for (parity, stop_bits, bits) in cases {
                let rate = settings(1_000, parity, stop_bits).rate();
                let nanos = bits * 1_000_000;
                assert_eq!(rate.span(1).nanos(), nanos, "{bits} bits");
                assert_eq!(rate.span(3).nanos(), 3 * nanos, "{bits} bits");
            }
        }

        #[test]
        fn holds_at_each_baud() {
            let slowest = settings(1, Some(Parity::Odd), StopBits::Two).rate();
            assert_eq!(slowest.span(1).nanos(), 12_000_000_000);
            let fastest = settings(u32::MAX, None, StopBits::One).rate();
            assert_eq!(fastest.span(1).nanos(), 2);
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
        fn hides_the_driver_and_shows_the_path() {
            assert_eq!(format!("{:?}", Serial::new(Ports)), "Serial { .. }");
            let port = format!("{:?}", port(Vec::new(), false));
            assert_eq!(port, r#"Port { path: "/dev/ttyS0", .. }"#);
        }
    }
}
