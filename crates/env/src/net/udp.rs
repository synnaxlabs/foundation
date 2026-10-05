//! UDP sockets that move batches of datagrams. A socket has two halves: a
//! [`Sender`] that each sending thread clones, and a [`Receiver`] with one owner.

pub mod sender;

use std::fmt;
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::task::{Context, Poll};

use super::{Ecn, Error};

/// The most bytes one [`Transmit`] may carry: the largest UDP payload over IPv4.
/// IPv6 allows 65,527, but an IPv4 peer on a dual-stack socket takes the IPv4 path.
pub const TRANSMIT_BYTES_MAX: usize = 65_507;

/// Settings for one UDP socket.
///
/// ```
/// let config = env::net::udp::Config {
///     local: "0.0.0.0:4433".parse().expect("an address"),
///     send_buffer_bytes: 1 << 21,
///     recv_buffer_bytes: 1 << 21,
/// };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The local address. Port 0 binds a free port. `[::]` binds one socket for IPv4
    /// and IPv6 (`IPV6_V6ONLY` off).
    pub local: SocketAddr,
    /// The size of the OS send buffer.
    pub send_buffer_bytes: usize,
    /// The size of the OS receive buffer.
    pub recv_buffer_bytes: usize,
}

/// The send half of a UDP socket. Each thread that sends keeps its own clone, and
/// clones send on the same socket. A clone may move to another thread before its
/// first poll. The first poll binds it to the thread that polls, and a poll on any
/// other thread then panics.
///
/// ```
/// use std::future::poll_fn;
///
/// use env::net::Error;
/// use env::net::udp::{Sender, Transmit};
///
/// async fn send(sender: &mut Sender, transmit: &Transmit<'_>) -> Result<(), Error> {
///     poll_fn(|cx| sender.poll_send(cx, transmit)).await
/// }
/// ```
pub struct Sender {
    socket: Arc<dyn Driver>,
    driver: Box<dyn sender::Driver>,
}

impl Sender {
    pub(super) fn new(socket: Arc<dyn Driver>) -> Self {
        Self {
            driver: socket.sender(),
            socket,
        }
    }

    /// The local address of the socket.
    ///
    /// ```
    /// fn local(sender: &env::net::udp::Sender) -> std::net::SocketAddr {
    ///     sender.local()
    /// }
    /// ```
    #[must_use]
    pub fn local(&self) -> SocketAddr {
        self.socket.local()
    }

    /// The most datagrams that one [`Transmit`] may hold: the GSO segment count, or 1
    /// without GSO. It does not change. The bytes stay within [`TRANSMIT_BYTES_MAX`].
    ///
    /// ```
    /// fn max(sender: &env::net::udp::Sender) -> usize {
    ///     sender.batch_max().get()
    /// }
    /// ```
    #[must_use]
    pub fn batch_max(&self) -> NonZeroUsize {
        self.socket.send_batch_max()
    }

    /// Sends every datagram of `transmit`. It is pending while the OS send buffer is
    /// full. Some datagrams may have gone out before a `Pending` or an error, and a
    /// retry sends them again.
    ///
    /// # Errors
    ///
    /// [`Error::Unreachable`] when no route reaches the destination, and
    /// [`Error::Io`] for other failures. An error affects only this transmit, and the
    /// socket stays usable.
    ///
    /// # Panics
    ///
    /// When `transmit` holds more datagrams than [`Sender::batch_max`], or more bytes
    /// than [`TRANSMIT_BYTES_MAX`], and on a thread other than the one of the first
    /// poll.
    ///
    /// ```
    /// use std::task::{Context, Poll};
    ///
    /// use env::net::udp::{Sender, Transmit};
    ///
    /// fn send(
    ///     sender: &mut Sender,
    ///     cx: &mut Context<'_>,
    ///     transmit: &Transmit<'_>,
    /// ) -> Poll<Result<(), env::net::Error>> {
    ///     sender.poll_send(cx, transmit)
    /// }
    /// ```
    pub fn poll_send(
        &mut self,
        cx: &mut Context<'_>,
        transmit: &Transmit<'_>,
    ) -> Poll<Result<(), Error>> {
        let len = transmit.contents.len();
        assert!(
            len <= TRANSMIT_BYTES_MAX,
            "a transmit of {len} bytes is over the max of {TRANSMIT_BYTES_MAX} bytes"
        );
        if let Some(segment) = transmit.segment {
            let max = self.batch_max().get();
            assert!(
                segment
                    .get()
                    .checked_mul(max)
                    .is_none_or(|bytes| len <= bytes),
                "a transmit of {len} bytes in segments of {segment} bytes holds {} \
                 datagrams, but the batch max is {max}",
                len.div_ceil(segment.get())
            );
        }
        self.driver.poll_send(cx, transmit)
    }
}

impl Clone for Sender {
    /// Gives a sender on the same socket, not bound to a thread yet.
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.socket))
    }
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("local", &self.local())
            .finish()
    }
}

/// The receive half of a UDP socket. It may move to another thread before its first
/// poll. The first poll binds it to the thread that polls, and a poll on any other
/// thread then panics.
///
/// ```
/// use std::future::poll_fn;
/// use std::io::IoSliceMut;
///
/// use env::net::udp::{Meta, Receiver};
///
/// async fn receive(
///     receiver: &mut Receiver,
///     buffer: &mut [u8],
/// ) -> Result<Meta, env::net::Error> {
///     let mut meta = [Meta::default()];
///     poll_fn(|cx| receiver.poll_recv(cx, &mut [IoSliceMut::new(buffer)], &mut meta))
///         .await?;
///     Ok(meta[0])
/// }
/// ```
pub struct Receiver(Arc<dyn Driver>);

impl Receiver {
    pub(super) fn new(socket: Arc<dyn Driver>) -> Self {
        Self(socket)
    }

    /// The local address of the socket.
    ///
    /// ```
    /// fn local(receiver: &env::net::udp::Receiver) -> std::net::SocketAddr {
    ///     receiver.local()
    /// }
    /// ```
    #[must_use]
    pub fn local(&self) -> SocketAddr {
        self.0.local()
    }

    /// The most datagrams that one buffer may receive: the GRO segment count, or 1
    /// without GRO. It does not change.
    ///
    /// ```
    /// fn max(receiver: &env::net::udp::Receiver) -> usize {
    ///     receiver.batch_max().get()
    /// }
    /// ```
    #[must_use]
    pub fn batch_max(&self) -> NonZeroUsize {
        self.0.recv_batch_max()
    }

    /// Receives batches into `buffers` in order, one batch per buffer, and fills the
    /// [`Meta`] at the same index. Gives the count of batches, at least 1.
    ///
    /// Size each buffer for [`Receiver::batch_max`] datagrams of the largest size
    /// you accept. The bytes of a datagram past the end of its buffer are lost, and
    /// that datagram is the last of its batch.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the socket is broken. The driver absorbs the errors of one
    /// datagram, such as `WSAECONNRESET` after an ICMP port unreachable.
    ///
    /// # Panics
    ///
    /// When `buffers` is empty or `meta` has another length, and on a thread other
    /// than the one of the first poll.
    ///
    /// ```
    /// use std::io::IoSliceMut;
    /// use std::task::{Context, Poll};
    ///
    /// use env::net::udp::{Meta, Receiver};
    ///
    /// fn receive(
    ///     receiver: &mut Receiver,
    ///     cx: &mut Context<'_>,
    ///     buffers: &mut [IoSliceMut<'_>],
    ///     meta: &mut [Meta],
    /// ) -> Poll<Result<usize, env::net::Error>> {
    ///     receiver.poll_recv(cx, buffers, meta)
    /// }
    /// ```
    pub fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [Meta],
    ) -> Poll<Result<usize, Error>> {
        assert!(!buffers.is_empty(), "poll_recv needs at least one buffer");
        assert_eq!(
            buffers.len(),
            meta.len(),
            "poll_recv needs one Meta per buffer"
        );
        self.0.poll_recv(cx, buffers, meta)
    }
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("local", &self.local())
            .finish()
    }
}

/// One send: one datagram, or a batch of datagrams to one destination. Each datagram
/// travels alone: the network may lose, delay, or reorder any one of them, and the
/// receiver may get them in other batches. The driver sets don't-fragment, so a
/// datagram over the path MTU is lost. On a socket bound to `[::]`, `destination` and
/// `source` may be IPv4.
///
/// ```
/// fn batch(contents: &[u8]) -> env::net::udp::Transmit<'_> {
///     env::net::udp::Transmit {
///         destination: "10.0.0.2:4433".parse().expect("an address"),
///         source: None,
///         ecn: Some(env::net::Ecn::Ect0),
///         contents,
///         segment: std::num::NonZeroUsize::new(1_200),
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transmit<'a> {
    /// Where the datagrams go.
    pub destination: SocketAddr,
    /// The local IP address to send from, or `None` for the OS's choice.
    pub source: Option<IpAddr>,
    /// The ECN codepoint of every datagram.
    pub ecn: Option<Ecn>,
    /// The bytes of the datagrams, back to back.
    pub contents: &'a [u8],
    /// The size of each datagram in `contents`; the last may be shorter. `None`
    /// makes `contents` one datagram.
    pub segment: Option<NonZeroUsize>,
}

/// What one received batch holds: `len` bytes in datagrams of `stride` bytes (the last
/// may be shorter), all from `source`. On a socket bound to `[::]`, an IPv4 address
/// in it is still `V4`, never `::ffff:a.b.c.d`.
///
/// ```
/// let mut meta = [env::net::udp::Meta::default(); 32];
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Where the datagrams came from.
    pub source: SocketAddr,
    /// The local IP address they came to, when the OS tells.
    pub destination: Option<IpAddr>,
    /// The ECN codepoint of the datagrams.
    pub ecn: Option<Ecn>,
    /// The bytes in the buffer.
    pub len: usize,
    /// The size of each datagram; the last may be shorter.
    pub stride: usize,
}

impl Default for Meta {
    /// An empty batch from `0.0.0.0:0`, to fill a buffer of metas before a receive.
    fn default() -> Self {
        Self {
            source: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            destination: None,
            ecn: None,
            len: 0,
            stride: 0,
        }
    }
}

/// What `os` and `sim` implement to run one UDP socket's [`Sender`] and
/// [`Receiver`]. Only they implement it.
///
/// It sets don't-fragment on every datagram (`os`: `IP_MTU_DISCOVER` set to probe, and
/// `IPV6_DONTFRAG`). A datagram over the path MTU is lost, not an error: `os` maps
/// `EMSGSIZE` to a loss, and `sim` drops it.
///
/// On a socket bound to `[::]`, `os` sets `IPV6_V6ONLY` off explicitly. The driver
/// gives each IPv4 address in a [`Meta`] as `V4` (`os` unmaps `::ffff:a.b.c.d`), and
/// sends a [`Transmit`] with an IPv4 `destination` and `source`. The `os` and `sim`
/// tests each need one case for this.
///
/// ```
/// fn local(socket: &dyn env::net::udp::Driver) -> std::net::SocketAddr {
///     socket.local()
/// }
/// ```
pub trait Driver: Send + Sync {
    /// The local address of the socket.
    fn local(&self) -> SocketAddr;

    /// The most datagrams one send carries. It does not change: when the OS refuses
    /// a batch, the driver sends its datagrams one at a time.
    fn send_batch_max(&self) -> NonZeroUsize;

    /// The most datagrams one buffer receives. It does not change.
    fn recv_batch_max(&self) -> NonZeroUsize;

    /// Gives the driver of one more [`Sender`] clone, not bound to a thread yet. It
    /// cannot fail, so a driver that needs a resource per clone takes it at the first
    /// poll.
    fn sender(&self) -> Box<dyn sender::Driver>;

    /// Receives, with the rules of [`Receiver::poll_recv`]. It absorbs the errors of
    /// one datagram and gives an error only when the socket is broken. It panics on a
    /// thread other than the one of the first call.
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [Meta],
    ) -> Poll<Result<usize, Error>>;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Waker;

    use super::*;
    use crate::net::{self, Net, listener, tcp};

    /// Sends at once and records each send with the number of its sender driver.
    struct Sending {
        number: usize,
        sends: Arc<Mutex<Vec<String>>>,
    }

    impl sender::Driver for Sending {
        fn poll_send(
            &mut self,
            _: &mut Context<'_>,
            transmit: &Transmit<'_>,
        ) -> Poll<Result<(), Error>> {
            self.sends
                .lock()
                .expect("no test panics while locked")
                .push(format!(
                    "sender {}: {} bytes to {}",
                    self.number,
                    transmit.contents.len(),
                    transmit.destination
                ));
            Poll::Ready(Ok(()))
        }
    }

    /// Numbers its sender drivers from 0. Each receive gives one datagram of three
    /// bytes.
    struct Socket {
        senders: AtomicUsize,
        sends: Arc<Mutex<Vec<String>>>,
    }

    impl Driver for Socket {
        fn local(&self) -> SocketAddr {
            "127.0.0.1:4433".parse().expect("an address")
        }

        fn send_batch_max(&self) -> NonZeroUsize {
            NonZeroUsize::new(4).expect("not zero")
        }

        fn recv_batch_max(&self) -> NonZeroUsize {
            NonZeroUsize::new(2).expect("not zero")
        }

        fn sender(&self) -> Box<dyn sender::Driver> {
            Box::new(Sending {
                number: self.senders.fetch_add(1, Ordering::Relaxed),
                sends: Arc::clone(&self.sends),
            })
        }

        fn poll_recv(
            &self,
            _: &mut Context<'_>,
            _: &mut [IoSliceMut<'_>],
            meta: &mut [Meta],
        ) -> Poll<Result<usize, Error>> {
            meta[0] = Meta {
                source: "10.0.0.2:4433".parse().expect("an address"),
                len: 3,
                stride: 3,
                ..Meta::default()
            };
            Poll::Ready(Ok(1))
        }
    }

    /// Binds every UDP socket to one [`Socket`]. It has no TCP.
    struct Network {
        sends: Arc<Mutex<Vec<String>>>,
    }

    impl net::Driver for Network {
        fn udp(&self, _: &Config) -> Result<Box<dyn Driver>, Error> {
            Ok(Box::new(Socket {
                senders: AtomicUsize::new(0),
                sends: Arc::clone(&self.sends),
            }))
        }

        fn connect<'a>(&'a self, _: &'a tcp::Config) -> net::Connect<'a> {
            Box::pin(async { Err(Error::Io { code: 95 }) })
        }

        fn listen(&self, _: &tcp::Listen) -> Result<Box<dyn listener::Driver>, Error> {
            Err(Error::Io { code: 95 })
        }
    }

    fn bind() -> (Sender, Receiver, Arc<Mutex<Vec<String>>>) {
        let sends = Arc::default();
        let net = Net::new(Network {
            sends: Arc::clone(&sends),
        });
        let config = Config {
            local: "127.0.0.1:0".parse().expect("an address"),
            send_buffer_bytes: 1 << 20,
            recv_buffer_bytes: 1 << 20,
        };
        let (sender, receiver) = net.udp(&config).expect("the bind succeeds");
        (sender, receiver, sends)
    }

    fn transmit(contents: &[u8], segment: usize) -> Transmit<'_> {
        Transmit {
            destination: "10.0.0.2:4433".parse().expect("an address"),
            source: None,
            ecn: None,
            contents,
            segment: NonZeroUsize::new(segment),
        }
    }

    fn send(sender: &mut Sender, transmit: &Transmit<'_>) -> Poll<Result<(), Error>> {
        sender.poll_send(&mut Context::from_waker(Waker::noop()), transmit)
    }

    mod net_udp {
        use super::*;

        #[test]
        fn gives_two_halves_of_one_socket() {
            let (mut sender, receiver, sends) = bind();
            assert_eq!(sender.local(), receiver.local());
            assert_eq!(sender.batch_max().get(), 4);
            assert_eq!(receiver.batch_max().get(), 2);
            let mut clone = sender.clone();
            assert_eq!(clone.local(), sender.local());
            assert_eq!(send(&mut clone, &transmit(&[0; 5], 0)), Poll::Ready(Ok(())));
            assert_eq!(
                send(&mut sender, &transmit(&[0; 6], 0)),
                Poll::Ready(Ok(()))
            );
            assert_eq!(
                *sends.lock().expect("no panic"),
                [
                    "sender 1: 5 bytes to 10.0.0.2:4433",
                    "sender 0: 6 bytes to 10.0.0.2:4433"
                ]
            );
        }

        #[test]
        fn shows_the_local_address_of_each_half() {
            let (sender, receiver, _) = bind();
            assert_eq!(format!("{sender:?}"), "Sender { local: 127.0.0.1:4433 }");
            assert_eq!(
                format!("{receiver:?}"),
                "Receiver { local: 127.0.0.1:4433 }"
            );
        }
    }

    mod poll_send {
        use super::*;

        #[test]
        fn sends_a_full_batch() {
            let (mut sender, _, sends) = bind();
            let contents = [0; 4 * 1_200];
            assert_eq!(
                send(&mut sender, &transmit(&contents, 1_200)),
                Poll::Ready(Ok(()))
            );
            assert_eq!(
                *sends.lock().expect("no panic"),
                ["sender 0: 4800 bytes to 10.0.0.2:4433"]
            );
        }

        #[test]
        fn sends_a_batch_with_a_short_last_datagram() {
            let (mut sender, _, _) = bind();
            let contents = [0; 3 * 1_200 + 1];
            assert_eq!(
                send(&mut sender, &transmit(&contents, 1_200)),
                Poll::Ready(Ok(()))
            );
        }

        #[test]
        fn sends_one_datagram_of_the_byte_max_without_a_segment() {
            let (mut sender, _, _) = bind();
            let contents = vec![0; TRANSMIT_BYTES_MAX];
            assert_eq!(
                send(&mut sender, &transmit(&contents, 0)),
                Poll::Ready(Ok(()))
            );
        }

        #[test]
        #[should_panic(
            expected = "a transmit of 65508 bytes is over the max of 65507 bytes"
        )]
        fn panics_above_the_byte_max() {
            let (mut sender, _, _) = bind();
            let contents = vec![0; TRANSMIT_BYTES_MAX + 1];
            drop(send(&mut sender, &transmit(&contents, 0)));
        }

        #[test]
        #[should_panic(
            expected = "a transmit of 4801 bytes in segments of 1200 bytes holds 5 \
                        datagrams, but the batch max is 4"
        )]
        fn panics_above_the_batch_max() {
            let (mut sender, _, _) = bind();
            let contents = [0; 4 * 1_200 + 1];
            drop(send(&mut sender, &transmit(&contents, 1_200)));
        }

        #[test]
        fn sends_one_datagram_when_the_segment_times_the_max_overflows() {
            let (mut sender, _, _) = bind();
            let one = Transmit {
                segment: NonZeroUsize::new(usize::MAX),
                ..transmit(&[0; 5], 0)
            };
            assert_eq!(send(&mut sender, &one), Poll::Ready(Ok(())));
        }
    }

    mod poll_recv {
        use super::*;

        fn receive(
            receiver: &mut Receiver,
            buffers: &mut [IoSliceMut<'_>],
            meta: &mut [Meta],
        ) -> Poll<Result<usize, Error>> {
            receiver.poll_recv(&mut Context::from_waker(Waker::noop()), buffers, meta)
        }

        #[test]
        fn fills_the_meta_of_each_batch() {
            let (_, mut receiver, _) = bind();
            let mut buffer = [0; 64];
            let mut meta = [Meta::default()];
            let count = receive(
                &mut receiver,
                &mut [IoSliceMut::new(&mut buffer)],
                &mut meta,
            );
            assert_eq!(count, Poll::Ready(Ok(1)));
            assert_eq!((meta[0].len, meta[0].stride), (3, 3));
        }

        #[test]
        #[should_panic(expected = "poll_recv needs at least one buffer")]
        fn panics_without_a_buffer() {
            let (_, mut receiver, _) = bind();
            drop(receive(&mut receiver, &mut [], &mut []));
        }

        #[test]
        #[should_panic(expected = "poll_recv needs one Meta per buffer")]
        fn panics_when_the_metas_do_not_match_the_buffers() {
            let (_, mut receiver, _) = bind();
            let mut buffer = [0; 64];
            let mut meta = [Meta::default(); 2];
            drop(receive(
                &mut receiver,
                &mut [IoSliceMut::new(&mut buffer)],
                &mut meta,
            ));
        }
    }
}
