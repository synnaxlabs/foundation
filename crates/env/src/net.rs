//! The network of one node: UDP sockets that move batches of datagrams, TCP streams,
//! TCP listeners, and name lookups.
//!
//! Every wait registers the waker of its [`Context`] and returns [`Poll::Pending`], so
//! `sim` controls it. "A thread" below means a thread that `env` started; under `sim`
//! it is a simulated thread.

pub mod listener;
pub mod tcp;
pub mod udp;

use std::fmt;
use std::io::IoSlice;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// The network of one node. Clones use the same network, and any thread may use
/// them.
///
/// ```
/// use std::net::SocketAddr;
///
/// fn bind(net: &env::net::Net, local: SocketAddr) -> Result<(), env::net::Error> {
///     let config = env::net::udp::Config {
///         local,
///         send_buffer_bytes: 1 << 21,
///         recv_buffer_bytes: 1 << 21,
///     };
///     let (_sender, _receiver) = net.udp(&config)?;
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct Net(Arc<dyn Driver>);

impl Net {
    /// Wraps a driver from `os` or `sim`.
    ///
    /// ```
    /// fn wrap(driver: impl env::net::Driver + 'static) -> env::net::Net {
    ///     env::net::Net::new(driver)
    /// }
    /// ```
    pub fn new(driver: impl Driver + 'static) -> Self {
        Self(Arc::new(driver))
    }

    /// Binds a UDP socket and gives its two halves. Port 0 binds a free port, which
    /// [`udp::Sender::local`] shows. The socket closes when both halves and every
    /// clone of the sender are dropped.
    ///
    /// # Errors
    ///
    /// [`Error::AddressInUse`] when the address is taken, and [`Error::Io`] for other
    /// failures.
    ///
    /// ```
    /// use env::net::udp::{Receiver, Sender};
    ///
    /// fn bind(
    ///     net: &env::net::Net,
    ///     config: &env::net::udp::Config,
    /// ) -> Result<(Sender, Receiver), env::net::Error> {
    ///     net.udp(config)
    /// }
    /// ```
    pub fn udp(
        &self,
        config: &udp::Config,
    ) -> Result<(udp::Sender, udp::Receiver), Error> {
        let (socket, receiver) = self.0.udp(config)?;
        let socket: Arc<dyn udp::Driver> = Arc::from(socket);
        Ok((
            udp::Sender::new(Arc::clone(&socket)),
            udp::Receiver::new(socket, receiver),
        ))
    }

    /// Connects a TCP stream to `config.remote`. Dropping the future stops the
    /// connect.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`], [`Error::Unreachable`], or [`Error::TimedOut`] when the
    /// remote cannot be reached, and [`Error::Io`] for other failures.
    ///
    /// ```
    /// async fn dial(
    ///     net: &env::net::Net,
    ///     config: &env::net::tcp::Config,
    /// ) -> Result<env::net::Tcp, env::net::Error> {
    ///     net.connect(config).await
    /// }
    /// ```
    pub async fn connect(&self, config: &tcp::Config) -> Result<Tcp, Error> {
        Ok(Tcp(self.0.connect(config).await?))
    }

    /// Listens for TCP streams. Port 0 binds a free port, which [`Listener::local`]
    /// shows.
    ///
    /// # Errors
    ///
    /// [`Error::AddressInUse`] when the address is taken, and [`Error::Io`] for other
    /// failures.
    ///
    /// ```
    /// fn listen(
    ///     net: &env::net::Net,
    ///     config: &env::net::tcp::Listen,
    /// ) -> Result<env::net::Listener, env::net::Error> {
    ///     net.listen(config)
    /// }
    /// ```
    pub fn listen(&self, config: &tcp::Listen) -> Result<Listener, Error> {
        Ok(Listener(self.0.listen(config)?))
    }

    /// Gives one or more addresses of `host`, each with `port`, in the order that the
    /// resolver gives them. A host that parses as an [`IpAddr`], or an IPv6 address
    /// in brackets as in a URI, gives that address and does no lookup. Each call
    /// looks up again: `resolve` keeps no cache. Dropping the future stops the wait,
    /// not the lookup.
    ///
    /// # Errors
    ///
    /// - [`Error::NotFound`] when the name has no address. A retry gives the same
    ///   answer until the name changes.
    /// - [`Error::Io`] when the lookup fails, for example when no name server
    ///   answers. A retry may give an answer. The code differs between systems, so
    ///   match the variant, not the code.
    ///
    /// ```
    /// use std::net::SocketAddr;
    ///
    /// async fn find(net: &env::net::Net) -> Result<Vec<SocketAddr>, env::net::Error> {
    ///     net.resolve("historian.local", 4433).await
    /// }
    /// ```
    pub async fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, Error> {
        let bracketed = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
        let literal = match bracketed {
            Some(v6) => v6.parse::<Ipv6Addr>().map(IpAddr::V6),
            None => host.parse::<IpAddr>(),
        };
        if let Ok(ip) = literal {
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        self.0.resolve(host, port).await
    }
}

impl fmt::Debug for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Net").finish_non_exhaustive()
    }
}

/// A TCP stream. It may move to another thread before its first poll. The first poll
/// binds it to the thread that polls, and a poll on any other thread then panics.
///
/// A drop never blocks. A drop before [`Tcp::poll_close`] is ready aborts the stream:
/// the peer gets a reset (RST), and the queued bytes are lost. To deliver every byte,
/// wait for `poll_close` first; it sends FIN after the queued bytes. A drop with
/// received bytes unread also resets, even after `poll_close`. A stream that has
/// ended sends no reset: a reset arrived, or each end has the other's bytes and FIN.
///
/// ```
/// use std::future::poll_fn;
///
/// async fn echo(tcp: &mut env::net::Tcp) -> Result<(), env::net::Error> {
///     let mut buffer = [0u8; 4_096];
///     let n = poll_fn(|cx| tcp.poll_read(cx, &mut buffer)).await?;
///     let parts = [std::io::IoSlice::new(&buffer[..n])];
///     poll_fn(|cx| tcp.poll_write(cx, &parts)).await?;
///     Ok(())
/// }
/// ```
pub struct Tcp(Box<dyn tcp::Driver>);

impl Tcp {
    /// The local address of the stream.
    ///
    /// ```
    /// fn local(tcp: &env::net::Tcp) -> std::net::SocketAddr {
    ///     tcp.local()
    /// }
    /// ```
    #[must_use]
    pub fn local(&self) -> SocketAddr {
        self.0.local()
    }

    /// The address of the peer.
    ///
    /// ```
    /// fn peer(tcp: &env::net::Tcp) -> std::net::SocketAddr {
    ///     tcp.peer()
    /// }
    /// ```
    #[must_use]
    pub fn peer(&self) -> SocketAddr {
        self.0.peer()
    }

    /// Reads bytes into `buffer` and gives the count. A count of 0 means the peer
    /// closed its side.
    ///
    /// # Errors
    ///
    /// [`Error::Reset`] when the peer reset the stream, and [`Error::Io`] for other
    /// failures.
    ///
    /// # Panics
    ///
    /// When `buffer` is empty, and on a thread other than the one of the first poll.
    ///
    /// ```
    /// use std::task::{Context, Poll};
    ///
    /// fn read(
    ///     tcp: &mut env::net::Tcp,
    ///     cx: &mut Context<'_>,
    ///     buffer: &mut [u8],
    /// ) -> Poll<Result<usize, env::net::Error>> {
    ///     tcp.poll_read(cx, buffer)
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

    /// Writes from `buffers` in order, as one vectored write, and gives the count of
    /// bytes written, which may be less than all. It is ready when the bytes not yet
    /// sent are fewer than [`tcp::Options::unsent_bytes_max`].
    ///
    /// A write of no bytes gives `Ok(0)` at once, also after a reset or
    /// [`Tcp::poll_close`].
    ///
    /// # Errors
    ///
    /// [`Error::Reset`] when the peer reset the stream, also after this end's
    /// [`Tcp::poll_close`]. [`Error::Io`] with the code of `EPIPE` for a write after
    /// this end's `poll_close` to a stream that the peer did not reset, and
    /// [`Error::Io`] for other failures.
    ///
    /// # Panics
    ///
    /// On a thread other than the one of the first poll.
    ///
    /// ```
    /// use std::io::IoSlice;
    /// use std::task::{Context, Poll};
    ///
    /// fn write(
    ///     tcp: &mut env::net::Tcp,
    ///     cx: &mut Context<'_>,
    ///     parts: &[IoSlice<'_>],
    /// ) -> Poll<Result<usize, env::net::Error>> {
    ///     tcp.poll_write(cx, parts)
    /// }
    /// ```
    pub fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Error>> {
        self.0.poll_write(cx, buffers)
    }

    /// Closes this side for writing, after the bytes written before it. The peer
    /// then reads a count of 0. Reads still work.
    ///
    /// # Errors
    ///
    /// [`Error::Reset`] when the peer reset the stream, and [`Error::Io`] for other
    /// failures.
    ///
    /// # Panics
    ///
    /// On a thread other than the one of the first poll.
    ///
    /// ```
    /// use std::future::poll_fn;
    ///
    /// async fn finish(tcp: &mut env::net::Tcp) -> Result<(), env::net::Error> {
    ///     poll_fn(|cx| tcp.poll_close(cx)).await
    /// }
    /// ```
    pub fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.0.poll_close(cx)
    }
}

impl fmt::Debug for Tcp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tcp")
            .field("local", &self.local())
            .field("peer", &self.peer())
            .finish()
    }
}

/// A TCP listener. It may move to another thread before its first poll. The first
/// poll binds it to the thread that polls, and a poll on any other thread then
/// panics. Dropping it stops the listen.
///
/// ```
/// use std::future::poll_fn;
///
/// async fn next(listener: &mut env::net::Listener) -> Option<env::net::Tcp> {
///     poll_fn(|cx| listener.poll_accept(cx)).await.ok()
/// }
/// ```
pub struct Listener(Box<dyn listener::Driver>);

impl Listener {
    /// The local address of the listener.
    ///
    /// ```
    /// fn local(listener: &env::net::Listener) -> std::net::SocketAddr {
    ///     listener.local()
    /// }
    /// ```
    #[must_use]
    pub fn local(&self) -> SocketAddr {
        self.0.local()
    }

    /// Accepts the next TCP stream, with the options of [`tcp::Listen::options`].
    /// The stream is not bound to a thread yet, so it may move to the thread that
    /// will own it.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the OS cannot accept one stream, for example `EMFILE` or
    /// `ECONNABORTED`, and the listener stays usable; or when the listener is broken,
    /// and each later accept gives the error too.
    ///
    /// # Panics
    ///
    /// On a thread other than the one of the first poll.
    ///
    /// ```
    /// use std::task::{Context, Poll};
    ///
    /// fn accept(
    ///     listener: &mut env::net::Listener,
    ///     cx: &mut Context<'_>,
    /// ) -> Poll<Result<env::net::Tcp, env::net::Error>> {
    ///     listener.poll_accept(cx)
    /// }
    /// ```
    pub fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<Result<Tcp, Error>> {
        self.0.poll_accept(cx).map_ok(Tcp)
    }
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listener")
            .field("local", &self.local())
            .finish()
    }
}

/// An ECN codepoint. `None` in its place means the packet is not ECN-capable.
///
/// ```
/// let ecn = Some(env::net::Ecn::Ect0);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ecn {
    /// ECN-capable transport, codepoint 0.
    Ect0,
    /// ECN-capable transport, codepoint 1.
    Ect1,
    /// Congestion experienced.
    Ce,
}

/// Why a network call failed. Each case names the address or name a caller acts on.
///
/// ```
/// let local = "127.0.0.1:9000".parse().expect("an address");
/// let e = env::net::Error::AddressInUse { local };
/// assert_eq!(e.to_string(), "address 127.0.0.1:9000 is in use");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Another socket holds the local address.
    AddressInUse {
        /// The address of the bind.
        local: SocketAddr,
    },
    /// The remote refused the connection.
    Refused {
        /// The remote address.
        remote: SocketAddr,
    },
    /// No route reaches the remote.
    Unreachable {
        /// The remote address.
        remote: SocketAddr,
    },
    /// The peer reset the stream.
    Reset {
        /// The address of the peer.
        remote: SocketAddr,
    },
    /// The remote did not answer in time.
    TimedOut {
        /// The remote address.
        remote: SocketAddr,
    },
    /// The name has no address.
    NotFound {
        /// The name.
        host: String,
    },
    /// The OS or the simulation reported another failure.
    Io {
        /// The OS error code.
        code: i32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AddressInUse { local } => write!(f, "address {local} is in use"),
            Self::Refused { remote } => write!(f, "{remote} refused the connection"),
            Self::Unreachable { remote } => write!(f, "{remote} is unreachable"),
            Self::Reset { remote } => write!(f, "{remote} reset the stream"),
            Self::TimedOut { remote } => write!(f, "{remote} did not answer in time"),
            Self::NotFound { host } => write!(f, "name {host} has no address"),
            Self::Io { code } => write!(f, "network call failed with OS error {code}"),
        }
    }
}

impl std::error::Error for Error {}

/// A connect in flight, as a [`Driver`] gives it.
///
/// ```
/// fn refused(remote: std::net::SocketAddr) -> env::net::Connect<'static> {
///     Box::pin(async move { Err(env::net::Error::Refused { remote }) })
/// }
/// ```
pub type Connect<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn tcp::Driver>, Error>> + 'a>>;

/// A lookup in flight, as a [`Driver`] gives it.
///
/// ```
/// fn missing(host: &str) -> env::net::Resolve<'_> {
///     Box::pin(async move { Err(env::net::Error::NotFound { host: host.into() }) })
/// }
/// ```
pub type Resolve<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<SocketAddr>, Error>> + 'a>>;

/// What `os` and `sim` implement to run a [`Net`]. Only they implement it.
///
/// Every socket it makes follows the thread rules of its handle: [`udp::Sender`],
/// [`udp::Receiver`], [`Tcp`], and [`Listener`].
///
/// ```
/// fn wrap(driver: impl env::net::Driver + 'static) -> env::net::Net {
///     env::net::Net::new(driver)
/// }
/// ```
pub trait Driver: Send + Sync {
    /// Binds a UDP socket, with the rules of [`Net::udp`]. Gives the socket, which
    /// gives the driver of each [`udp::Sender`] clone, and the driver of its one
    /// [`udp::Receiver`], not bound to a thread yet.
    ///
    /// # Errors
    ///
    /// As [`Net::udp`].
    #[expect(clippy::type_complexity, reason = "the two drivers of one bind")]
    fn udp(
        &self,
        config: &udp::Config,
    ) -> Result<(Box<dyn udp::Driver>, Box<dyn udp::receiver::Driver>), Error>;

    /// Connects a TCP stream, with the rules of [`Net::connect`].
    fn connect<'a>(&'a self, config: &'a tcp::Config) -> Connect<'a>;

    /// Listens for TCP streams, with the rules of [`Net::listen`].
    ///
    /// # Errors
    ///
    /// As [`Net::listen`].
    fn listen(&self, config: &tcp::Listen) -> Result<Box<dyn listener::Driver>, Error>;

    /// Looks up a host that is not an IP literal, with the rules of [`Net::resolve`].
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> Resolve<'a>;
}

#[cfg(test)]
mod tests {
    use std::task::Waker;

    use super::*;

    fn address(text: &str) -> SocketAddr {
        text.parse().expect("an address")
    }

    /// A stream from `127.0.0.1:5000` to `peer` that never has bytes.
    struct Stream {
        peer: SocketAddr,
    }

    impl tcp::Driver for Stream {
        fn local(&self) -> SocketAddr {
            address("127.0.0.1:5000")
        }

        fn peer(&self) -> SocketAddr {
            self.peer
        }

        fn poll_read(
            &mut self,
            _: &mut Context<'_>,
            _: &mut [u8],
        ) -> Poll<Result<usize, Error>> {
            Poll::Pending
        }

        fn poll_write(
            &mut self,
            _: &mut Context<'_>,
            _: &[IoSlice<'_>],
        ) -> Poll<Result<usize, Error>> {
            Poll::Pending
        }

        fn poll_close(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
            Poll::Pending
        }
    }

    /// Accepts one stream from `10.0.0.3:6000`.
    struct Accepting;

    impl listener::Driver for Accepting {
        fn local(&self) -> SocketAddr {
            address("127.0.0.1:4433")
        }

        fn poll_accept(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Result<Box<dyn tcp::Driver>, Error>> {
            Poll::Ready(Ok(Box::new(Stream {
                peer: address("10.0.0.3:6000"),
            })))
        }
    }

    /// Connects to any remote at once, and gives `historian.local` one address. It
    /// has no UDP.
    struct Network;

    impl Driver for Network {
        fn udp(
            &self,
            _: &udp::Config,
        ) -> Result<(Box<dyn udp::Driver>, Box<dyn udp::receiver::Driver>), Error>
        {
            Err(Error::Io { code: 95 })
        }

        fn connect<'a>(&'a self, config: &'a tcp::Config) -> Connect<'a> {
            let stream: Box<dyn tcp::Driver> = Box::new(Stream {
                peer: config.remote,
            });
            Box::pin(async { Ok(stream) })
        }

        fn listen(&self, _: &tcp::Listen) -> Result<Box<dyn listener::Driver>, Error> {
            Ok(Box::new(Accepting))
        }

        fn resolve<'a>(&'a self, host: &'a str, port: u16) -> Resolve<'a> {
            Box::pin(async move {
                if host == "historian.local" {
                    return Ok(vec![SocketAddr::from(([10, 0, 0, 9], port))]);
                }
                Err(Error::NotFound { host: host.into() })
            })
        }
    }

    fn options() -> tcp::Options {
        tcp::Options {
            send_buffer_bytes: 1 << 20,
            recv_buffer_bytes: 1 << 20,
            unsent_bytes_max: 1 << 14,
            delayed: false,
        }
    }

    fn cx() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    #[test]
    fn moves_sockets_between_threads() {
        fn movable<T: Send>() {}
        fn cloned<T: Send + Clone>() {}
        fn shared<T: Send + Sync + Clone>() {}
        movable::<Tcp>();
        movable::<Listener>();
        movable::<udp::Receiver>();
        cloned::<udp::Sender>();
        shared::<Net>();
    }

    mod connect {
        use super::*;

        #[test]
        fn gives_a_stream_to_the_remote() {
            let net = Net::new(Network);
            let config = tcp::Config {
                remote: address("10.0.0.2:4433"),
                options: options(),
            };
            let mut connect = std::pin::pin!(net.connect(&config));
            let Poll::Ready(Ok(tcp)) = connect.as_mut().poll(&mut cx()) else {
                panic!("the connect did not end");
            };
            assert_eq!(tcp.peer(), address("10.0.0.2:4433"));
        }
    }

    mod resolve {
        use super::*;

        fn resolve(host: &str) -> Result<Vec<SocketAddr>, Error> {
            let net = Net::new(Network);
            let mut resolve = std::pin::pin!(net.resolve(host, 4433));
            let Poll::Ready(result) = resolve.as_mut().poll(&mut cx()) else {
                panic!("the lookup did not end");
            };
            result
        }

        #[test]
        fn gives_an_ipv4_literal_with_no_lookup() {
            assert_eq!(resolve("10.0.0.2"), Ok(vec![address("10.0.0.2:4433")]));
        }

        #[test]
        fn gives_an_ipv6_literal_with_no_lookup() {
            assert_eq!(resolve("fd00::2"), Ok(vec![address("[fd00::2]:4433")]));
        }

        #[test]
        fn gives_an_ipv6_literal_in_brackets_with_no_lookup() {
            assert_eq!(resolve("[fd00::2]"), Ok(vec![address("[fd00::2]:4433")]));
        }

        #[test]
        fn looks_up_an_ipv4_literal_in_brackets_as_a_name() {
            let host = "[10.0.0.2]".to_owned();
            assert_eq!(resolve("[10.0.0.2]"), Err(Error::NotFound { host }));
        }

        #[test]
        fn looks_up_a_literal_with_a_final_dot_as_a_name() {
            let host = "10.0.0.2.".to_owned();
            assert_eq!(resolve("10.0.0.2."), Err(Error::NotFound { host }));
        }

        #[test]
        fn looks_up_a_name_with_the_port() {
            let found = resolve("historian.local");
            assert_eq!(found, Ok(vec![address("10.0.0.9:4433")]));
        }

        #[test]
        fn gives_the_error_of_the_lookup() {
            let host = "pump.local".to_owned();
            assert_eq!(resolve("pump.local"), Err(Error::NotFound { host }));
        }
    }

    mod tcp_polls {
        use super::*;

        #[test]
        #[should_panic(expected = "poll_read needs a buffer of at least one byte")]
        fn panics_on_a_read_into_an_empty_buffer() {
            let mut tcp = Tcp(Box::new(Stream {
                peer: address("10.0.0.2:4433"),
            }));
            drop(tcp.poll_read(&mut cx(), &mut []));
        }

        #[test]
        fn wait_while_the_driver_waits() {
            let mut tcp = Tcp(Box::new(Stream {
                peer: address("10.0.0.2:4433"),
            }));
            assert_eq!(tcp.poll_read(&mut cx(), &mut [0; 8]), Poll::Pending);
            assert_eq!(
                tcp.poll_write(&mut cx(), &[IoSlice::new(&[1])]),
                Poll::Pending
            );
            assert_eq!(tcp.poll_close(&mut cx()), Poll::Pending);
        }
    }

    mod debug {
        use super::*;

        #[test]
        fn shows_the_addresses_of_each_handle() {
            let tcp = Tcp(Box::new(Stream {
                peer: address("10.0.0.2:4433"),
            }));
            assert_eq!(
                format!("{tcp:?}"),
                "Tcp { local: 127.0.0.1:5000, peer: 10.0.0.2:4433 }"
            );
            let listener = Listener(Box::new(Accepting));
            assert_eq!(
                format!("{listener:?}"),
                "Listener { local: 127.0.0.1:4433 }"
            );
            assert_eq!(format!("{:?}", Net::new(Network)), "Net { .. }");
        }
    }

    mod poll_accept {
        use super::*;

        #[test]
        fn gives_the_stream_that_the_driver_accepted() {
            let net = Net::new(Network);
            let config = tcp::Listen {
                local: address("127.0.0.1:0"),
                backlog: 16,
                options: options(),
            };
            let mut listener = net.listen(&config).expect("the listen succeeds");
            let Poll::Ready(Ok(tcp)) = listener.poll_accept(&mut cx()) else {
                panic!("the accept did not end");
            };
            assert_eq!(tcp.peer(), address("10.0.0.3:6000"));
            assert_eq!(listener.local(), address("127.0.0.1:4433"));
        }
    }

    mod error {
        use super::*;

        fn remote() -> SocketAddr {
            address("10.0.0.2:4433")
        }

        #[test]
        fn names_the_remote_that_refused() {
            let e = Error::Refused { remote: remote() };
            assert_eq!(e.to_string(), "10.0.0.2:4433 refused the connection");
        }

        #[test]
        fn names_the_remote_that_is_unreachable() {
            let e = Error::Unreachable { remote: remote() };
            assert_eq!(e.to_string(), "10.0.0.2:4433 is unreachable");
        }

        #[test]
        fn names_the_peer_that_reset() {
            let e = Error::Reset { remote: remote() };
            assert_eq!(e.to_string(), "10.0.0.2:4433 reset the stream");
        }

        #[test]
        fn names_the_remote_that_timed_out() {
            let e = Error::TimedOut { remote: remote() };
            assert_eq!(e.to_string(), "10.0.0.2:4433 did not answer in time");
        }

        #[test]
        fn names_the_host_with_no_address() {
            let e = Error::NotFound {
                host: "historian.local".into(),
            };
            assert_eq!(e.to_string(), "name historian.local has no address");
        }

        #[test]
        fn names_the_os_code() {
            let e = Error::Io { code: 105 };
            assert_eq!(e.to_string(), "network call failed with OS error 105");
        }
    }
}
