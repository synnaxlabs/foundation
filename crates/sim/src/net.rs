//! The network of a run: node addresses, links, UDP sockets, and datagrams in flight.

use std::collections::{BTreeMap, VecDeque};
use std::hash::{DefaultHasher, Hash};
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::task::{Poll, Waker};

use env::net::udp::{Config, Meta, Transmit};
use env::net::{Ecn, Error};
use env::rng::Rng;
use types::time::{Monotonic, Span};

use crate::link;

/// `10.0.0.0`: node `k` has `10.0.0.0` plus `k + 1`.
const V4: u32 = 0x0a00_0000;
/// `fd00::`: node `k` has `fd00::` plus `k + 1`.
const V6: u128 = 0xfd00 << 112;
/// The broadcast host of `10.0.0.0/8`, which no node has.
const BROADCAST: u32 = 0x00ff_ffff;
/// The first port that port 0 binds.
const EPHEMERAL: u16 = 49_152;
/// The Linux code for an address that is not on the node (`EADDRNOTAVAIL`).
const NOT_AVAILABLE: i32 = 99;
/// The IPv4 and UDP header bytes of a datagram.
const V4_HEADERS: usize = 28;
/// The IPv6 and UDP header bytes of a datagram.
const V6_HEADERS: usize = 48;
/// The bytes of a receive queue that a datagram takes past its length. Linux also
/// charges each datagram for its buffer (`truesize`), about this much for a small one.
const OVERHEAD: usize = 768;
/// The batch maxes that each socket draws from, for sends and for receives.
const BATCH_MAXES: [usize; 3] = [1, 8, 64];

/// The IPv4 and IPv6 addresses of node `node`.
///
/// # Panics
///
/// When `node` is past the last host of `10.0.0.0/8`.
pub(crate) fn addresses(node: usize) -> [IpAddr; 2] {
    let host = u32::try_from(node + 1)
        .ok()
        .filter(|&host| host < BROADCAST);
    let Some(host) = host else {
        panic!("node {node} has no address: 10.0.0.0/8 holds 16,777,214 nodes")
    };
    let v6 = Ipv6Addr::from_bits(V6 + u128::from(host));
    [IpAddr::V4(Ipv4Addr::from_bits(V4 + host)), IpAddr::V6(v6)]
}

/// The node whose address is `ip`, if `ip` is an address that a node can have.
fn node(ip: IpAddr) -> Option<usize> {
    let host = match ip {
        IpAddr::V4(ip) => u128::from(ip.to_bits()).checked_sub(u128::from(V4)),
        IpAddr::V6(ip) => ip.to_bits().checked_sub(V6),
    };
    let host = host.and_then(|host| u32::try_from(host).ok());
    let host = host.filter(|host| (1..BROADCAST).contains(host))?;
    usize::try_from(host - 1).ok()
}

/// Whether a socket bound to `local` receives at `ip`, an address of its node. `[::]`
/// receives IPv4 too.
fn covers(local: IpAddr, ip: IpAddr) -> bool {
    local == ip
        || local == IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        || (local == IpAddr::V4(Ipv4Addr::UNSPECIFIED) && ip.is_ipv4())
}

/// What a bind gives the driver of the socket.
pub(crate) struct Bound {
    pub(crate) key: u64,
    pub(crate) local: SocketAddr,
    pub(crate) send_batch_max: NonZeroUsize,
    pub(crate) recv_batch_max: NonZeroUsize,
}

/// What happens to a datagram, for the digest.
#[derive(Clone, Copy, Hash)]
enum Fate {
    /// The link lost it, or it was over the MTU.
    Lost,
    /// It is in flight.
    Sent,
    /// It is in flight twice.
    Duplicated,
    /// It arrived in a receive queue.
    Queued,
    /// It arrived where nothing is bound, or at a full queue.
    Dropped,
}

struct Datagram {
    source: SocketAddr,
    destination: SocketAddr,
    ecn: Option<Ecn>,
    contents: Vec<u8>,
}

impl Datagram {
    /// The bytes of a receive queue that it takes.
    fn charge(&self) -> usize {
        self.contents.len() + OVERHEAD
    }
}

/// A bound UDP socket and its receive queue.
struct Binding {
    node: usize,
    local: SocketAddr,
    recv_batch_max: NonZeroUsize,
    /// The most bytes that `queue` takes.
    capacity: usize,
    queue: VecDeque<Datagram>,
    /// The bytes that `queue` takes.
    queued: usize,
    /// The waker of the last receive that found the queue empty.
    waker: Option<Waker>,
}

impl Binding {
    /// Whether the socket receives a datagram to `destination`.
    fn receives(&self, destination: SocketAddr) -> bool {
        let (local, ip) = (self.local.ip(), destination.ip());
        self.local.port() == destination.port()
            && covers(local, ip)
            && node(ip) == Some(self.node)
    }

    /// Queues `datagram` while the queue takes at most `capacity` bytes, so that, as
    /// on Linux, the last datagram may go past it. Returns its fate, and the waker of
    /// a receive to wake.
    fn push(&mut self, datagram: Datagram) -> (Fate, Option<Waker>) {
        if self.queued > self.capacity {
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

/// The network of a run. Each datagram's fate comes from the network's own stream of
/// the seed.
pub(crate) struct Network {
    /// The link of each ordered pair of nodes that has no link of its own.
    default: link::Config,
    links: BTreeMap<(usize, usize), link::Config>,
    bindings: BTreeMap<u64, Binding>,
    /// Datagrams by true arrival time, then by a key in the order they were sent.
    flights: BTreeMap<(Monotonic, u64), Datagram>,
    rng: Rng,
    next: u64,
}

impl Network {
    pub(crate) fn new(default: link::Config, rng: Rng) -> Self {
        Self {
            default,
            links: BTreeMap::new(),
            bindings: BTreeMap::new(),
            flights: BTreeMap::new(),
            rng,
            next: 0,
        }
    }

    fn key(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Sets the link from node `from` to node `to`.
    pub(crate) fn link(&mut self, from: usize, to: usize, config: link::Config) {
        self.links.insert((from, to), config);
    }

    /// Binds a UDP socket on `node`.
    pub(crate) fn bind(
        &mut self,
        node: usize,
        config: &Config,
    ) -> Result<Bound, Error> {
        let local = config.local;
        let ip = local.ip();
        if !ip.is_unspecified() && !addresses(node).contains(&ip) {
            return Err(Error::Io {
                code: NOT_AVAILABLE,
            });
        }
        let taken = |port: u16| {
            (self.bindings.values()).any(|binding| {
                let other = binding.local.ip();
                binding.node == node
                    && binding.local.port() == port
                    && (covers(ip, other) || covers(other, ip))
            })
        };
        let port = match local.port() {
            0 => (EPHEMERAL..=u16::MAX).find(|&port| !taken(port)),
            port => Some(port).filter(|&port| !taken(port)),
        };
        let Some(port) = port else {
            return Err(Error::AddressInUse { local });
        };
        let local = SocketAddr::new(ip, port);
        let [send_batch_max, recv_batch_max] = [(); 2].map(|()| {
            let index = self.rng.below(3);
            let index = usize::try_from(index).expect("invariant: below 3 fits usize");
            NonZeroUsize::new(BATCH_MAXES[index]).expect("invariant: not zero")
        });
        let binding = Binding {
            node,
            local,
            recv_batch_max,
            capacity: config.recv_buffer_bytes,
            queue: VecDeque::new(),
            queued: 0,
            waker: None,
        };
        let key = self.key();
        self.bindings.insert(key, binding);
        Ok(Bound {
            key,
            local,
            send_batch_max,
            recv_batch_max,
        })
    }

    /// Removes socket `key`, and returns its waker for the caller to drop after it
    /// releases the lock.
    pub(crate) fn close(&mut self, key: u64) -> Option<Waker> {
        self.bindings.remove(&key).and_then(|binding| binding.waker)
    }

    /// Sends the datagrams of `transmit` from socket `key` at true time `now`.
    pub(crate) fn send(
        &mut self,
        now: Monotonic,
        digest: &mut DefaultHasher,
        key: u64,
        transmit: &Transmit<'_>,
    ) -> Result<(), Error> {
        let binding = &self.bindings[&key];
        let from = binding.node;
        let (source, destination) = route(from, binding.local, transmit)?;
        let to = node(destination.ip());
        let link =
            *(to.and_then(|to| self.links.get(&(from, to)))).unwrap_or(&self.default);
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
            let fate = self.fly(now, &link, datagram);
            (now, source, destination, part.len(), fate).hash(digest);
        }
        Ok(())
    }

    /// Puts `datagram` in flight on `link`, as the link's faults decide.
    fn fly(&mut self, now: Monotonic, link: &link::Config, datagram: Datagram) -> Fate {
        let header = if datagram.destination.is_ipv4() {
            V4_HEADERS
        } else {
            V6_HEADERS
        };
        if datagram.contents.len() + header > link.mtu || roll(&mut self.rng, link.loss)
        {
            return Fate::Lost;
        }
        let duplicated = roll(&mut self.rng, link.duplication);
        if duplicated {
            let copy = Datagram {
                contents: datagram.contents.clone(),
                ..datagram
            };
            self.arrive(now, link, copy);
        }
        self.arrive(now, link, datagram);
        if duplicated {
            Fate::Duplicated
        } else {
            Fate::Sent
        }
    }

    /// Schedules the arrival of `datagram` after the delay of `link` and a draw of its
    /// jitter. One that would arrive past `u64` nanoseconds never arrives.
    fn arrive(&mut self, now: Monotonic, link: &link::Config, datagram: Datagram) {
        let jitter = u64::try_from(link.jitter.nanos())
            .expect("invariant: a checked link has no negative jitter");
        let extra = i64::try_from(self.rng.below(jitter + 1))
            .expect("invariant: a draw up to a jitter fits i64");
        let at = (now.checked_add(link.delay))
            .and_then(|at| at.checked_add(Span::from_nanos(extra)));
        if let Some(at) = at {
            let key = self.key();
            self.flights.insert((at, key), datagram);
        }
    }

    /// The true time of the first arrival.
    pub(crate) fn first(&self) -> Option<Monotonic> {
        self.flights.first_key_value().map(|(&(at, _), _)| at)
    }

    /// Delivers the datagrams that arrive by true time `at`, and returns the wakers
    /// of the sockets that receive them.
    pub(crate) fn deliver(
        &mut self,
        at: Monotonic,
        digest: &mut DefaultHasher,
    ) -> Vec<Waker> {
        let mut wakers = Vec::new();
        while let Some(flight) = self.flights.first_entry() {
            if flight.key().0 > at {
                break;
            }
            let datagram = flight.remove();
            let (source, destination) = (datagram.source, datagram.destination);
            let len = datagram.contents.len();
            let binding = (self.bindings.values_mut())
                .find(|binding| binding.receives(destination));
            let fate = match binding {
                Some(binding) => {
                    let (fate, waker) = binding.push(datagram);
                    wakers.extend(waker);
                    fate
                }
                None => Fate::Dropped,
            };
            (at, source, destination, len, fate).hash(digest);
        }
        wakers
    }

    /// Receives batches from socket `key` into `buffers`, one per buffer, or keeps
    /// `waker` when the queue is empty. Returns the count of batches, and a waker for
    /// the caller to drop after it releases the lock.
    pub(crate) fn recv(
        &mut self,
        key: u64,
        waker: Waker,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [Meta],
    ) -> (Poll<usize>, Option<Waker>) {
        let binding = (self.bindings.get_mut(&key))
            .expect("invariant: a socket lives while its driver does");
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
        (Poll::Ready(count), Some(waker))
    }
}

/// Draws from `rng`, and gives whether the draw falls under `chance`, from 0 to 1.
pub(crate) fn roll(rng: &mut Rng, chance: f64) -> bool {
    let draw = u32::try_from(rng.next_u64() >> 32)
        .expect("invariant: the high half of a u64 fits u32");
    under(draw, chance)
}

/// Whether `draw`, uniform over `u32`, falls under `chance`, from 0 to 1. A chance of
/// 0 never holds a draw, and a chance of 1 holds every draw.
pub(crate) fn under(draw: u32, chance: f64) -> bool {
    f64::from(draw) < chance * 2f64.powi(32)
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
