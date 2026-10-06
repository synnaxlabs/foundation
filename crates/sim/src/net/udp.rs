//! UDP sockets and their datagrams.

use std::collections::{BTreeMap, VecDeque};
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::task::{Poll, Waker};

use env::net::udp::{Config, Meta, Transmit};
use env::net::{Ecn, Error};
use types::time::Monotonic;

use super::wire::Wire;
use super::{Fate, NOT_AVAILABLE, addresses, ip_header, receives};
use crate::EIO;

/// The bytes of the UDP header of a datagram.
const HEADER: usize = 8;
/// The bytes of a send buffer or a receive queue that a datagram takes past its length.
/// Linux also charges each datagram for its buffer (`truesize`), about this much for a
/// small one.
const OVERHEAD: usize = 768;
/// The batch maxes that each socket draws from, for sends and for receives.
const BATCH_MAXES: [usize; 3] = [1, 8, 64];

/// What a bind gives the driver of the socket.
pub(crate) struct Bound {
    pub(crate) key: u64,
    pub(crate) local: SocketAddr,
    pub(crate) send_batch_max: NonZeroUsize,
    pub(crate) recv_batch_max: NonZeroUsize,
}

#[derive(Clone)]
pub(super) struct Datagram {
    source: SocketAddr,
    destination: SocketAddr,
    ecn: Option<Ecn>,
    contents: Vec<u8>,
}

impl Datagram {
    /// The bytes of a send buffer or a receive queue that it takes.
    fn charge(&self) -> usize {
        self.contents.len() + OVERHEAD
    }
}

/// The bound UDP sockets of a run.
#[derive(Default)]
pub(super) struct Sockets {
    bindings: BTreeMap<u64, Binding>,
    next: u64,
    /// The bytes of a send buffer that datagrams take until they leave their link, by
    /// the true time they leave it, then the socket.
    departures: BTreeMap<(Monotonic, u64), usize>,
}

impl Sockets {
    /// The true time at which the first datagram that takes a send buffer leaves its
    /// link.
    pub(super) fn first(&self) -> Option<Monotonic> {
        (self.departures.first_key_value()).map(|(&(at, _), _)| at)
    }
}

/// The UDP sockets of a run, with the wire they send on.
pub(crate) struct Udp<'a> {
    sockets: &'a mut Sockets,
    wire: &'a mut Wire,
}

/// A bound UDP socket, its send buffer, and its receive queue.
struct Binding {
    node: usize,
    local: SocketAddr,
    recv_batch_max: NonZeroUsize,
    /// The most bytes that the send buffer takes.
    send_capacity: usize,
    /// The bytes that the send buffer takes: those of each datagram that has not left
    /// its link.
    unsent: usize,
    /// The wakers of the sends that found the send buffer full.
    senders: Vec<Waker>,
    /// The most bytes that `queue` takes.
    recv_capacity: usize,
    queue: VecDeque<Datagram>,
    /// The bytes that `queue` takes.
    queued: usize,
    /// The waker of the last receive that found the queue empty.
    waker: Option<Waker>,
    /// From a fault until the socket drops: each receive gives `EIO`, and each
    /// arrival drops.
    failed: bool,
}

impl Binding {
    /// Queues `datagram` while the socket works and the queue takes at most
    /// `recv_capacity` bytes, so that, as on Linux, the last datagram may go past it.
    /// Returns its fate, and the waker of a receive to wake.
    fn push(&mut self, datagram: Datagram) -> (Fate, Option<Waker>) {
        if self.failed || self.queued > self.recv_capacity {
            return (Fate::Dropped, None);
        }
        self.queued += datagram.charge();
        self.queue.push_back(datagram);
        (Fate::Queued, self.waker.take())
    }

    /// Takes the next batch from the queue into `buffer`: the first datagram, then
    /// each next one of the same source, destination, ECN, and size, as GRO joins
    /// them. A shorter datagram ends the batch, and so does the end of the buffer.
    ///
    /// # Panics
    ///
    /// When the queue is empty.
    fn batch(&mut self, buffer: &mut [u8]) -> Meta {
        let first = (self.queue.pop_front()).expect("invariant: the caller checks");
        let stride = first.contents.len().min(buffer.len());
        buffer[..stride].copy_from_slice(&first.contents[..stride]);
        self.queued -= first.charge();
        let mut meta = Meta {
            source: first.source,
            destination: Some(first.destination.ip()),
            ecn: first.ecn,
            len: stride,
            stride,
        };
        let mut count = 1;
        let mut ended = false;
        while let Some(next) = self.queue.front().filter(|_| !ended) {
            let len = next.contents.len();
            let joins = next.source == first.source
                && next.destination == first.destination
                && next.ecn == first.ecn
                && (1..=stride).contains(&len)
                && meta.len + len <= buffer.len()
                && count < self.recv_batch_max.get();
            if !joins {
                break;
            }
            buffer[meta.len..meta.len + len].copy_from_slice(&next.contents);
            (meta.len, count, ended) = (meta.len + len, count + 1, len < stride);
            self.queued -= next.charge();
            self.queue.pop_front();
        }
        meta
    }
}

impl<'a> Udp<'a> {
    pub(super) fn new(sockets: &'a mut Sockets, wire: &'a mut Wire) -> Self {
        Self { sockets, wire }
    }

    /// Binds a UDP socket on `node`.
    pub(crate) fn bind(
        &mut self,
        node: usize,
        config: &Config,
    ) -> Result<Bound, Error> {
        let bound = (self.sockets.bindings.values())
            .filter(|binding| binding.node == node)
            .map(|binding| binding.local);
        let local = super::bind(node, config.local, &bound)?;
        let [send_batch_max, recv_batch_max] = [(); 2].map(|()| {
            let index = self.wire.rng().below(3);
            let index = usize::try_from(index).expect("invariant: below 3 fits usize");
            NonZeroUsize::new(BATCH_MAXES[index]).expect("invariant: not zero")
        });
        let binding = Binding {
            node,
            local,
            recv_batch_max,
            send_capacity: config.send_buffer_bytes,
            unsent: 0,
            senders: Vec::new(),
            recv_capacity: config.recv_buffer_bytes,
            queue: VecDeque::new(),
            queued: 0,
            waker: None,
            failed: false,
        };
        self.sockets.next += 1;
        let key = self.sockets.next;
        self.sockets.bindings.insert(key, binding);
        Ok(Bound {
            key,
            local,
            send_batch_max,
            recv_batch_max,
        })
    }

    /// Removes socket `key`, and returns its wakers for the caller to drop after it
    /// releases the lock. Its datagrams still leave their links.
    pub(crate) fn close(&mut self, key: u64) -> Vec<Waker> {
        let binding = self.sockets.bindings.remove(&key);
        (self.sockets.departures).retain(|&(_, socket), _| socket != key);
        (binding.into_iter())
            .flat_map(|binding| binding.waker.into_iter().chain(binding.senders))
            .collect()
    }

    /// Makes the socket of `node` bound at `local` fail. Returns a waker for the
    /// caller to wake after it releases the lock: that of a receive that waits, or
    /// one that does nothing. Returns `None` when no socket of `node` is bound at
    /// `local`.
    pub(crate) fn fail(&mut self, node: usize, local: SocketAddr) -> Option<Waker> {
        let binding = (self.sockets.bindings.values_mut())
            .find(|binding| binding.node == node && binding.local == local)?;
        binding.failed = true;
        Some((binding.waker.take()).unwrap_or_else(|| Waker::noop().clone()))
    }

    /// Sends the datagrams of `transmit` from socket `key` at true time `now`, or keeps
    /// `waker` when the send buffer is full. As on Linux, the send buffer is full when
    /// it takes `send_buffer_bytes` or more, so the datagrams of a send may go past it.
    pub(crate) fn send(
        &mut self,
        now: Monotonic,
        key: u64,
        waker: &Waker,
        transmit: &Transmit<'_>,
    ) -> Poll<Result<(), Error>> {
        let Sockets {
            bindings,
            departures,
            ..
        } = &mut *self.sockets;
        let binding = (bindings.get_mut(&key))
            .expect("invariant: a socket lives while its driver does");
        let (source, destination) = match route(binding.node, binding.local, transmit) {
            Ok(addresses) => addresses,
            Err(error) => return Poll::Ready(Err(error)),
        };
        if binding.unsent >= binding.send_capacity {
            if !binding.senders.iter().any(|sender| sender.will_wake(waker)) {
                binding.senders.push(waker.clone());
            }
            return Poll::Pending;
        }
        let path = self.wire.path(binding.node, destination.ip());
        let header = HEADER + ip_header(destination.ip());
        let contents = transmit.contents;
        let size = transmit.segment.map_or(usize::MAX, NonZeroUsize::get);
        for n in 0..contents.len().div_ceil(size).max(1) {
            let start = n * size;
            let part = &contents[start..start.saturating_add(size).min(contents.len())];
            let datagram = Datagram {
                source,
                destination,
                ecn: transmit.ecn,
                contents: part.to_vec(),
            };
            let bytes = part.len() + header;
            let departure = if bytes > path.link.mtu {
                None
            } else {
                self.wire.depart(now, &path, bytes)
            };
            if let Some(departure) = departure.filter(|&departure| departure > now) {
                *departures.entry((departure, key)).or_default() += datagram.charge();
                binding.unsent += datagram.charge();
            }
            let fate = match departure {
                Some(departure) => self.wire.fly(&path, departure, datagram),
                None => Fate::Lost,
            };
            (self.wire).record((now, source, destination, part.len(), fate));
        }
        Poll::Ready(Ok(()))
    }

    /// Frees the send buffers of the datagrams that leave their links by true time
    /// `at`. Returns the wakers of the sends that then find room.
    pub(super) fn depart(&mut self, at: Monotonic) -> Vec<Waker> {
        let mut wakers = Vec::new();
        while let Some(departure) = self.sockets.departures.first_entry() {
            if departure.key().0 > at {
                break;
            }
            let ((_, key), charge) = departure.remove_entry();
            let binding = (self.sockets.bindings.get_mut(&key))
                .expect("invariant: a socket that closes drops its departures");
            binding.unsent -= charge;
            if binding.unsent < binding.send_capacity {
                wakers.append(&mut binding.senders);
            }
        }
        wakers
    }

    /// Queues `datagram`, which arrives at true time `at`, at the socket that
    /// receives it. Returns the waker of a receive to wake.
    pub(super) fn queue(&mut self, at: Monotonic, datagram: Datagram) -> Option<Waker> {
        let (source, destination) = (datagram.source, datagram.destination);
        let len = datagram.contents.len();
        let binding = (self.sockets.bindings.values_mut())
            .find(|binding| receives(binding.node, binding.local, destination));
        let (fate, waker) = match binding {
            Some(binding) => binding.push(datagram),
            None => (Fate::Dropped, None),
        };
        self.wire.record((at, source, destination, len, fate));
        waker
    }

    /// Receives batches from socket `key` into `buffers`, one per buffer, or keeps
    /// `waker` when the queue is empty. Returns the count of batches, or `EIO` when
    /// the socket failed, and a waker for the caller to drop after it releases the
    /// lock.
    pub(crate) fn recv(
        &mut self,
        key: u64,
        waker: Waker,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [Meta],
    ) -> (Poll<Result<usize, Error>>, Option<Waker>) {
        let binding = (self.sockets.bindings.get_mut(&key))
            .expect("invariant: a socket lives while its driver does");
        if binding.failed {
            return (Poll::Ready(Err(Error::Io { code: EIO })), Some(waker));
        }
        if binding.queue.is_empty() {
            return (Poll::Pending, binding.waker.replace(waker));
        }
        let mut count = 0;
        for (buffer, meta) in buffers.iter_mut().zip(meta) {
            if binding.queue.is_empty() {
                break;
            }
            *meta = binding.batch(buffer);
            count += 1;
        }
        (Poll::Ready(Ok(count)), Some(waker))
    }
}

/// The source and destination addresses of the datagrams of `transmit` from a
/// socket of `node` bound to `local`. As on Linux, a socket on IPv6 takes an
/// IPv4-mapped address as the IPv4 address.
///
/// # Errors
///
/// [`Error::Unreachable`] when the socket's family cannot reach the destination, and
/// [`Error::Io`] when the source is not an address of the socket.
fn route(
    node: usize,
    local: SocketAddr,
    transmit: &Transmit<'_>,
) -> Result<(SocketAddr, SocketAddr), Error> {
    let (mut destination, mut ip) = (transmit.destination, transmit.source);
    if local.is_ipv6() {
        if let SocketAddr::V6(v6) = destination
            && let Some(v4) = v6.ip().to_ipv4_mapped()
        {
            destination = SocketAddr::from((v4, v6.port()));
        }
        ip = ip.map(|ip| ip.to_canonical());
    }
    let any = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
    if local.ip() != any && local.is_ipv4() != destination.is_ipv4() {
        let remote = transmit.destination;
        return Err(Error::Unreachable { remote });
    }
    let [v4, v6] = addresses(node);
    let own = if destination.is_ipv4() { v4 } else { v6 };
    let ip = ip.unwrap_or(own);
    if ip != own {
        return Err(Error::Io {
            code: NOT_AVAILABLE,
        });
    }
    Ok((SocketAddr::new(ip, local.port()), destination))
}
