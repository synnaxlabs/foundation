//! A UDP socket: the kernel's socket, with the batch calls of `noq-udp`, polled
//! through Tokio.

use std::io::{self, IoSliceMut};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket};
use std::num::NonZeroUsize;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, ready};
use std::thread::{self, ThreadId};

use env::net::udp::{self, Meta, Transmit, sender};
use env::net::{Ecn, Error};
use noq_udp::{EcnCodepoint, RecvMeta, UdpSocketState};
use rustix::io::Errno;
use rustix::net::{SocketType, ipproto, sockopt};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use super::socket;
use super::{bind, canonical, errno, from_io, io_error};

/// A bound UDP socket, and the receive half of it.
pub(super) struct Udp {
    bound: Arc<Bound>,
    /// The thread of the receiver's first poll.
    thread: OnceLock<ThreadId>,
    /// The receiver's `dup` of the socket, registered for readable by the first poll
    /// that succeeds.
    readable: OnceLock<AsyncFd<UdpSocket>>,
}

/// What each half of one socket reads.
struct Bound {
    /// Each half sends and receives on a `dup` of it, so two halves on one thread are
    /// two registrations.
    socket: UdpSocket,
    state: UdpSocketState,
    local: SocketAddr,
    send_batch_max: NonZeroUsize,
}

impl Udp {
    /// Binds a socket to `config.local`, with don't-fragment set. Needs no runtime.
    pub(super) fn bind(config: &udp::Config) -> Result<Self, Error> {
        let local = config.local;
        let fd =
            super::socket(local, SocketType::DGRAM, ipproto::UDP).map_err(io_error)?;
        configure(fd.as_fd(), config).map_err(io_error)?;
        bind(fd.as_fd(), local)?;
        let socket = UdpSocket::from(fd);
        let state = UdpSocketState::new((&socket).into()).map_err(|e| from_io(&e))?;
        // Linux and macOS each set don't-fragment, so this fails only on an OS that
        // cannot keep the contract.
        if state.may_fragment() {
            return Err(io_error(Errno::NOPROTOOPT));
        }
        let local = socket.local_addr().map_err(|e| from_io(&e))?;
        let bound = Bound {
            send_batch_max: state.max_gso_segments(),
            socket,
            state,
            local,
        };
        Ok(Self {
            bound: Arc::new(bound),
            thread: OnceLock::new(),
            readable: OnceLock::new(),
        })
    }
}

/// Sets the buffer sizes of `config` on `fd`, and lets an IPv6 socket take IPv4,
/// whatever the host's default.
fn configure(fd: BorrowedFd<'_>, config: &udp::Config) -> Result<(), Errno> {
    sockopt::set_socket_send_buffer_size(fd, config.send_buffer_bytes)?;
    sockopt::set_socket_recv_buffer_size(fd, config.recv_buffer_bytes)?;
    if config.local.is_ipv6() {
        sockopt::set_ipv6_v6only(fd, false)?;
    }
    Ok(())
}

impl udp::Driver for Udp {
    fn local(&self) -> SocketAddr {
        self.bound.local
    }

    fn send_batch_max(&self) -> NonZeroUsize {
        self.bound.send_batch_max
    }

    fn recv_batch_max(&self) -> NonZeroUsize {
        self.bound.state.gro_segments()
    }

    fn sender(&self) -> Box<dyn sender::Driver> {
        Box::new(Sender {
            bound: Arc::clone(&self.bound),
            thread: None,
            writer: None,
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [Meta],
    ) -> Poll<Result<usize, Error>> {
        let bound = &self.bound;
        let thread = self.thread.get_or_init(|| thread::current().id());
        socket::on_thread("UDP receiver", *thread);
        let socket = if let Some(socket) = self.readable.get() {
            socket
        } else {
            let socket = (bound.socket.try_clone())
                .and_then(|fd| AsyncFd::with_interest(fd, Interest::READABLE))
                .map_err(|e| from_io(&e))?;
            self.readable.get_or_init(|| socket)
        };
        loop {
            let mut guard =
                ready!(socket.poll_read_ready(cx)).map_err(|e| from_io(&e))?;
            if let Ok(received) =
                guard.try_io(|fd| bound.receive(fd.get_ref(), buffers, meta))
            {
                return Poll::Ready(received.map_err(|e| from_io(&e)));
            }
        }
    }
}

impl Bound {
    /// Receives into `buffers` with one call. An unconnected socket gets no ICMP
    /// errors on Linux and macOS, so each error is the socket's.
    fn receive(
        &self,
        fd: &UdpSocket,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [Meta],
    ) -> io::Result<usize> {
        let mut received = [RecvMeta::default(); noq_udp::BATCH_SIZE];
        let count = buffers.len().min(received.len());
        let count = self.state.recv(
            fd.into(),
            &mut buffers[..count],
            &mut received[..count],
        )?;
        for (meta, received) in meta.iter_mut().zip(&received[..count]) {
            *meta = Meta {
                source: canonical(received.addr),
                destination: received.dst_ip.map(|ip| ip.to_canonical()),
                ecn: received.ecn.map(from_codepoint),
                len: received.len,
                stride: received.stride,
            };
        }
        Ok(count)
    }
}

/// The driver of one `Sender` clone.
struct Sender {
    bound: Arc<Bound>,
    /// The thread of the first poll.
    thread: Option<ThreadId>,
    /// Set by the first poll that succeeds.
    writer: Option<Writer>,
}

/// The descriptor of one sender. It has a registration for writable only while the
/// OS send buffer is full: Linux wakes each such registration of a socket for each
/// datagram that any descriptor of the socket sends.
struct Writer {
    /// A registration of `fd`. It drops first, so it never outlives `fd`.
    full: Option<AsyncFd<RawFd>>,
    fd: UdpSocket,
}

impl Writer {
    /// A registration of `fd`, for writable.
    fn register(fd: &UdpSocket) -> io::Result<AsyncFd<RawFd>> {
        AsyncFd::with_interest(fd.as_raw_fd(), Interest::WRITABLE)
    }

    /// Runs `send` on the descriptor until it is ready, and waits for writable while
    /// it is `Pending`.
    fn poll_send(
        &mut self,
        cx: &mut Context<'_>,
        mut send: impl FnMut(&UdpSocket) -> Poll<Result<(), Error>>,
    ) -> Poll<Result<(), Error>> {
        loop {
            if let Poll::Ready(sent) = send(&self.fd) {
                self.full = None;
                return Poll::Ready(sent);
            }
            let full = match &mut self.full {
                Some(full) => full,
                none @ None => {
                    let full = Self::register(&self.fd).map_err(|e| from_io(&e))?;
                    none.insert(full)
                }
            };
            let mut guard =
                ready!(full.poll_write_ready(cx)).map_err(|e| from_io(&e))?;
            guard.clear_ready();
        }
    }
}

impl sender::Driver for Sender {
    fn poll_send(
        &mut self,
        cx: &mut Context<'_>,
        transmit: &Transmit<'_>,
    ) -> Poll<Result<(), Error>> {
        let bound = &*self.bound;
        let thread = self.thread.get_or_insert_with(|| thread::current().id());
        socket::on_thread("UDP sender", *thread);
        let writer = match &mut self.writer {
            Some(writer) => writer,
            none @ None => {
                // It registers, so it panics on a thread with no I/O driver as a TCP
                // stream does.
                let fd = bound.socket.try_clone().map_err(|e| from_io(&e))?;
                let full = Some(Writer::register(&fd).map_err(|e| from_io(&e))?);
                none.insert(Writer { full, fd })
            }
        };
        writer.poll_send(cx, |fd| {
            send_all(bound, transmit, |datagram| {
                bound.state.try_send(fd.into(), datagram)
            })
        })
    }
}

/// Sends each datagram of `transmit` from `bound` with `send`. Gives `Pending` when
/// the OS send buffer is full, with no waker kept.
fn send_all(
    bound: &Bound,
    transmit: &Transmit<'_>,
    mut send: impl FnMut(&noq_udp::Transmit<'_>) -> io::Result<()>,
) -> Poll<Result<(), Error>> {
    let remote = transmit.destination;
    let (destination, source) = route(bound.local, transmit)?;
    let datagram = |contents, segment_size| noq_udp::Transmit {
        destination,
        ecn: transmit.ecn.map(to_codepoint),
        contents,
        segment_size,
        src_ip: source,
    };
    let contents = transmit.contents;
    let segment = (transmit.segment)
        .map(NonZeroUsize::get)
        .filter(|&segment| segment < contents.len());
    let Some(segment) = segment else {
        return sent(send(&datagram(contents, None)), remote);
    };
    // When the kernel or the card cannot segment, `noq-udp` sends the batch a
    // datagram at a time and turns GSO off for the socket.
    if bound.state.max_gso_segments().get() > 1 {
        return sent(send(&datagram(contents, Some(segment))), remote);
    }
    for contents in contents.chunks(segment) {
        ready!(sent(send(&datagram(contents, None)), remote))?;
    }
    Poll::Ready(Ok(()))
}

/// The outcome of a send to `remote`. A datagram over the path MTU is lost, not an
/// error.
fn sent(outcome: io::Result<()>, remote: SocketAddr) -> Poll<Result<(), Error>> {
    let Err(e) = outcome else {
        return Poll::Ready(Ok(()));
    };
    Poll::Ready(match errno(&e) {
        Errno::AGAIN => return Poll::Pending,
        Errno::MSGSIZE => Ok(()),
        Errno::NETUNREACH | Errno::HOSTUNREACH => Err(Error::Unreachable { remote }),
        code => Err(io_error(code)),
    })
}

/// The destination and source of `transmit` from a socket bound to `local`, as the
/// kernel takes them, with the rules of `sim`: a socket on `::` takes IPv4 in either
/// form, and any other socket reaches only its own family.
///
/// # Errors
///
/// - [`Error::Unreachable`] with the destination as given when the socket's family
///   cannot reach it.
/// - [`Error::Io`] with `EINVAL` for an IPv6 source on an IPv4 socket, or for an
///   unspecified source in any form. Linux skips the `IPV6_PKTINFO` of the first and
///   reads the second as no source, and sends each from an address of its choice.
fn route(
    local: SocketAddr,
    transmit: &Transmit<'_>,
) -> Result<(SocketAddr, Option<IpAddr>), Error> {
    let remote = transmit.destination;
    let any = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
    let destination = if local.is_ipv6() {
        canonical(remote)
    } else {
        remote
    };
    if local.ip() != any && local.is_ipv4() != destination.is_ipv4() {
        return Err(Error::Unreachable { remote });
    }
    if transmit.source.is_some_and(|source| {
        source.to_canonical().is_unspecified() || local.is_ipv4() && source.is_ipv6()
    }) {
        return Err(io_error(Errno::INVAL));
    }
    if local.is_ipv4() {
        return Ok((remote, transmit.source));
    }
    let mapped = |ip: IpAddr| match ip {
        IpAddr::V4(v4) => IpAddr::V6(v4.to_ipv6_mapped()),
        IpAddr::V6(_) => ip,
    };
    let destination = match destination {
        SocketAddr::V4(v4) => SocketAddr::new(mapped(IpAddr::V4(*v4.ip())), v4.port()),
        SocketAddr::V6(_) => destination,
    };
    Ok((destination, transmit.source.map(mapped)))
}

fn to_codepoint(ecn: Ecn) -> EcnCodepoint {
    match ecn {
        Ecn::Ect0 => EcnCodepoint::Ect0,
        Ecn::Ect1 => EcnCodepoint::Ect1,
        Ecn::Ce => EcnCodepoint::Ce,
    }
}

fn from_codepoint(ecn: EcnCodepoint) -> Ecn {
    match ecn {
        EcnCodepoint::Ect0 => Ecn::Ect0,
        EcnCodepoint::Ect1 => Ecn::Ect1,
        EcnCodepoint::Ce => Ecn::Ce,
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    const V4: Ipv4Addr = Ipv4Addr::LOCALHOST;

    fn config(local: SocketAddr) -> udp::Config {
        udp::Config {
            local,
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 15,
        }
    }

    fn loopback() -> Udp {
        Udp::bind(&config(SocketAddr::new(V4.into(), 0))).unwrap()
    }

    fn transmit(destination: SocketAddr, contents: &[u8]) -> Transmit<'_> {
        Transmit {
            destination,
            source: None,
            ecn: None,
            contents,
            segment: None,
        }
    }

    fn v4(port: u16) -> SocketAddr {
        SocketAddr::new(V4.into(), port)
    }

    fn mapped(port: u16) -> SocketAddr {
        SocketAddr::new(V4.to_ipv6_mapped().into(), port)
    }

    fn v6(port: u16) -> SocketAddr {
        SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port)
    }

    fn any_v6() -> SocketAddr {
        SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 9)
    }

    mod bind {
        use super::*;

        #[test]
        fn sets_the_buffer_sizes() {
            let udp = loopback();
            let fd = udp.bound.socket.as_fd();
            let kept = super::super::super::tests::kept;
            assert_eq!(sockopt::socket_send_buffer_size(fd), Ok(kept(1 << 16)));
            assert_eq!(sockopt::socket_recv_buffer_size(fd), Ok(kept(1 << 15)));
        }

        /// Linux takes the default from `net.ipv6.bindv6only`, and macOS from
        /// `net.inet6.ip6.v6only`.
        #[test]
        fn takes_ipv4_on_an_ipv6_socket_on_a_v6_only_host() {
            let local = SocketAddr::new(any_v6().ip(), 0);
            let fd =
                crate::net::socket(local, SocketType::DGRAM, ipproto::UDP).unwrap();
            sockopt::set_ipv6_v6only(&fd, true).unwrap();
            assert_eq!(configure(fd.as_fd(), &config(local)), Ok(()));
            assert_eq!(sockopt::ipv6_v6only(&fd), Ok(false));
        }

        #[test]
        fn gives_the_bound_port() {
            let udp = loopback();
            assert_eq!(udp.bound.local.ip(), V4);
            assert_ne!(udp.bound.local.port(), 0);
        }

        #[test]
        fn a_held_address_is_in_use() {
            let first = loopback();
            let local = first.bound.local;
            let held = Udp::bind(&config(local));
            assert_eq!(held.map(drop), Err(Error::AddressInUse { local }));
        }
    }

    mod route {
        use super::*;

        fn route(
            local: SocketAddr,
            transmit: &Transmit<'_>,
        ) -> Result<SocketAddr, Error> {
            super::route(local, transmit).map(|(destination, _)| destination)
        }

        #[test]
        fn a_v4_socket_reaches_only_ipv4() {
            assert_eq!(route(v4(1), &transmit(v4(2), b"")), Ok(v4(2)));
            for remote in [mapped(2), v6(2)] {
                let routed = route(v4(1), &transmit(remote, b""));
                assert_eq!(routed, Err(Error::Unreachable { remote }));
            }
        }

        #[test]
        fn a_v6_socket_on_an_address_reaches_only_ipv6() {
            assert_eq!(route(v6(1), &transmit(v6(2), b"")), Ok(v6(2)));
            for remote in [mapped(2), v4(2)] {
                let routed = route(v6(1), &transmit(remote, b""));
                assert_eq!(routed, Err(Error::Unreachable { remote }));
            }
        }

        #[test]
        fn an_any_v6_socket_sends_ipv4_mapped() {
            for remote in [mapped(2), v4(2)] {
                let routed = route(any_v6(), &transmit(remote, b""));
                assert_eq!(routed, Ok(mapped(2)));
            }
            assert_eq!(route(any_v6(), &transmit(v6(2), b"")), Ok(v6(2)));
        }

        #[test]
        fn an_any_v6_socket_maps_an_ipv4_source() {
            let from = |source: IpAddr| Transmit {
                source: Some(source),
                ..transmit(v4(2), b"")
            };
            let routed = super::route(any_v6(), &from(V4.into()));
            assert_eq!(routed, Ok((mapped(2), Some(V4.to_ipv6_mapped().into()))));
            let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
            assert_eq!(super::route(any_v6(), &from(v6)), Ok((mapped(2), Some(v6))));
        }

        #[test]
        fn a_v4_socket_keeps_the_source() {
            let to = Transmit {
                source: Some(V4.into()),
                ..transmit(v4(2), b"")
            };
            assert_eq!(super::route(v4(1), &to), Ok((v4(2), Some(V4.into()))));
        }
    }

    mod sent {
        use super::*;

        fn failed(code: Errno) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(code.raw_os_error()))
        }

        #[test]
        fn maps_each_outcome() {
            let remote = v4(2);
            assert_eq!(sent(Ok(()), remote), Poll::Ready(Ok(())));
            assert_eq!(sent(failed(Errno::AGAIN), remote), Poll::Pending);
            assert_eq!(sent(failed(Errno::MSGSIZE), remote), Poll::Ready(Ok(())));
            for code in [Errno::NETUNREACH, Errno::HOSTUNREACH] {
                let unreachable = Err(Error::Unreachable { remote });
                assert_eq!(sent(failed(code), remote), Poll::Ready(unreachable));
            }
            let io = Err(Error::Io {
                code: Errno::PERM.raw_os_error(),
            });
            assert_eq!(sent(failed(Errno::PERM), remote), Poll::Ready(io));
        }
    }

    mod send_all {
        use super::*;

        #[derive(Clone, Copy)]
        #[cfg_attr(
            not(target_os = "linux"),
            expect(dead_code, reason = "only a GSO test fails a send")
        )]
        enum Outcome {
            Fails(Errno),
            Sent,
        }

        /// The contents and segment size of each send, with the outcomes to give.
        struct Recorded {
            sends: Vec<(Vec<u8>, Option<usize>)>,
            outcomes: Vec<Outcome>,
        }

        impl Recorded {
            fn new(outcomes: &[Outcome]) -> Self {
                Self {
                    sends: Vec::new(),
                    outcomes: outcomes.iter().rev().copied().collect(),
                }
            }

            fn send(&mut self, datagram: &noq_udp::Transmit<'_>) -> io::Result<()> {
                let send = (datagram.contents.to_vec(), datagram.segment_size);
                self.sends.push(send);
                match self.outcomes.pop() {
                    Some(Outcome::Fails(code)) => {
                        Err(io::Error::from_raw_os_error(code.raw_os_error()))
                    }
                    Some(Outcome::Sent) | None => Ok(()),
                }
            }
        }

        /// Sends a batch of more than 64 or 128 segments, which Linux refuses with
        /// `EINVAL`. Its first datagram goes out alone, so `noq-udp` turns GSO off.
        fn turn_gso_off(bound: &Bound) {
            let refused = noq_udp::Transmit {
                destination: bound.local,
                ecn: None,
                contents: &[0; 200],
                segment_size: Some(1),
                src_ip: None,
            };
            let sent = bound.state.try_send((&bound.socket).into(), &refused);
            assert_eq!(sent.map_err(|e| errno(&e)), Ok(()));
            assert_eq!(bound.state.max_gso_segments().get(), 1);
        }

        fn batch(contents: &[u8], segment: usize) -> Transmit<'_> {
            Transmit {
                segment: NonZeroUsize::new(segment),
                ..transmit(v4(2), contents)
            }
        }

        fn run(
            udp: &Udp,
            transmit: &Transmit<'_>,
            recorded: &mut Recorded,
        ) -> Poll<Result<(), Error>> {
            send_all(&udp.bound, transmit, |d| recorded.send(d))
        }

        #[test]
        fn sends_one_datagram_without_a_segment() {
            let mut recorded = Recorded::new(&[]);
            let sent = run(&loopback(), &transmit(v4(2), b"abc"), &mut recorded);
            assert_eq!(sent, Poll::Ready(Ok(())));
            assert_eq!(recorded.sends, [(b"abc".to_vec(), None)]);
        }

        #[test]
        fn sends_one_datagram_when_the_segment_holds_all() {
            let mut recorded = Recorded::new(&[]);
            let sent = run(&loopback(), &batch(b"abc", 3), &mut recorded);
            assert_eq!(sent, Poll::Ready(Ok(())));
            assert_eq!(recorded.sends, [(b"abc".to_vec(), None)]);
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn sends_a_batch_in_one_call() {
            let mut recorded = Recorded::new(&[]);
            let sent = run(&loopback(), &batch(b"abcde", 2), &mut recorded);
            assert_eq!(sent, Poll::Ready(Ok(())));
            assert_eq!(recorded.sends, [(b"abcde".to_vec(), Some(2))]);
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn gives_a_failure_of_a_batch_that_leaves_gso_on() {
            let failed = Outcome::Fails(Errno::INVAL);
            let mut recorded = Recorded::new(&[failed]);
            let sent = run(&loopback(), &batch(b"abcde", 2), &mut recorded);
            let code = Errno::INVAL.raw_os_error();
            assert_eq!(sent, Poll::Ready(Err(Error::Io { code })));
            assert_eq!(recorded.sends.len(), 1);
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn a_refused_transmit_in_segments_of_0_bytes_leaves_gso_on() {
            let udp = loopback();
            let bound = &udp.bound;
            let refused = noq_udp::Transmit {
                destination: SocketAddr::new(bound.local.ip(), 0),
                ecn: None,
                contents: b"abc",
                segment_size: Some(0),
                src_ip: None,
            };
            let sent = bound.state.try_send((&bound.socket).into(), &refused);
            assert_eq!(sent.map_err(|e| errno(&e)), Err(Errno::INVAL));
            assert!(bound.state.max_gso_segments().get() > 1);
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn sends_each_datagram_alone_after_the_os_refused_a_batch() {
            let udp = loopback();
            turn_gso_off(&udp.bound);
            let mut recorded = Recorded::new(&[]);
            let sent = run(&udp, &batch(b"abc", 2), &mut recorded);
            assert_eq!(sent, Poll::Ready(Ok(())));
            let alone = |bytes: &[u8]| (bytes.to_vec(), None);
            assert_eq!(recorded.sends, [alone(b"ab"), alone(b"c")]);
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn gives_another_failure_of_a_batch() {
            let mut recorded = Recorded::new(&[Outcome::Fails(Errno::AGAIN)]);
            let sent = run(&loopback(), &batch(b"abcde", 2), &mut recorded);
            assert_eq!(sent, Poll::Pending);
            assert_eq!(recorded.sends.len(), 1);
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn stops_at_a_failure_of_one_datagram() {
            let udp = loopback();
            turn_gso_off(&udp.bound);
            let outcomes = [Outcome::Sent, Outcome::Fails(Errno::NETUNREACH)];
            let mut recorded = Recorded::new(&outcomes);
            let sent = run(&udp, &batch(b"abcde", 2), &mut recorded);
            let remote = v4(2);
            assert_eq!(sent, Poll::Ready(Err(Error::Unreachable { remote })));
            assert_eq!(recorded.sends.len(), 2);
        }

        #[test]
        fn sends_nothing_to_an_unreachable_family() {
            let mut recorded = Recorded::new(&[]);
            let remote = v6(2);
            let sent = run(&loopback(), &transmit(remote, b"x"), &mut recorded);
            assert_eq!(sent, Poll::Ready(Err(Error::Unreachable { remote })));
            assert!(recorded.sends.is_empty());
        }
    }

    mod writer {
        use std::task::Waker;

        use super::*;

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap()
        }

        #[test]
        fn waits_while_the_buffer_is_full_and_then_drops_the_registration() {
            runtime().block_on(async {
                let udp = loopback();
                let fd = udp.bound.socket.try_clone().unwrap();
                let mut writer = Writer { full: None, fd };
                let mut cx = Context::from_waker(Waker::noop());
                let mut sends = 0;
                let full = writer.poll_send(&mut cx, |_| {
                    sends += 1;
                    Poll::Pending
                });
                assert_eq!(full, Poll::Pending);
                assert_eq!(sends, 1);
                assert!(writer.full.is_some());
                let done = writer.poll_send(&mut cx, |_| Poll::Ready(Ok(())));
                assert_eq!(done, Poll::Ready(Ok(())));
                assert!(writer.full.is_none());
            });
        }

        /// The epoll entries of any runtime in this process for the socket of `inode`.
        #[cfg(target_os = "linux")]
        fn registrations(inode: u64) -> usize {
            let entry = format!("ino:{inode:x} ");
            std::fs::read_dir("/proc/self/fdinfo")
                .unwrap()
                .filter_map(|fd| std::fs::read_to_string(fd.ok()?.path()).ok())
                .map(|info| {
                    info.lines()
                        .filter(|line| {
                            line.starts_with("tfd:") && line.contains(&entry)
                        })
                        .count()
                })
                .sum()
        }

        /// While the socket stays open in `Bound`, epoll keeps a registration whose
        /// descriptor closed before it.
        #[cfg(target_os = "linux")]
        #[test]
        fn drop_removes_the_registration_from_epoll() {
            use std::os::unix::fs::MetadataExt;

            runtime().block_on(async {
                let udp = loopback();
                let fd = udp.bound.socket.try_clone().unwrap();
                let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
                let inode = std::fs::metadata(path).unwrap().ino();
                let mut writer = Writer { full: None, fd };
                let mut cx = Context::from_waker(Waker::noop());
                assert_eq!(registrations(inode), 0);
                let full = writer.poll_send(&mut cx, |_| Poll::Pending);
                assert_eq!(full, Poll::Pending);
                assert_ne!(registrations(inode), 0);
                drop(writer);
                assert_eq!(registrations(inode), 0);
            });
        }

        #[test]
        fn retries_a_pending_send_when_the_socket_is_writable() {
            runtime().block_on(async {
                let udp = loopback();
                let fd = udp.bound.socket.try_clone().unwrap();
                let mut writer = Writer { full: None, fd };
                let mut sends = 0;
                let sent = std::future::poll_fn(|cx| {
                    writer.poll_send(cx, |_| {
                        sends += 1;
                        if sends == 1 {
                            Poll::Pending
                        } else {
                            Poll::Ready(Ok(()))
                        }
                    })
                });
                let bound = std::time::Duration::from_secs(10);
                assert_eq!(tokio::time::timeout(bound, sent).await, Ok(Ok(())));
                assert_eq!(sends, 2);
            });
        }

        #[test]
        fn gives_the_outcome_of_a_send() {
            runtime().block_on(async {
                let udp = loopback();
                let fd = udp.bound.socket.try_clone().unwrap();
                let full = Some(Writer::register(&fd).unwrap());
                let mut writer = Writer { full, fd };
                let mut cx = Context::from_waker(Waker::noop());
                let failed = Err(Error::Io { code: 1 });
                let sent = writer.poll_send(&mut cx, |_| Poll::Ready(failed.clone()));
                assert_eq!(sent, Poll::Ready(failed));
                assert!(writer.full.is_none());
            });
        }
    }

    #[test]
    fn each_ecn_codepoint_round_trips() {
        for ecn in [Ecn::Ect0, Ecn::Ect1, Ecn::Ce] {
            assert_eq!(from_codepoint(to_codepoint(ecn)), ecn);
        }
        assert_eq!(to_codepoint(Ecn::Ce), EcnCodepoint::Ce);
    }
}
