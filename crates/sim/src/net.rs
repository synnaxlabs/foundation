//! The network of a run: node addresses, links, and packets in flight. Each protocol
//! has its own module.

mod udp;

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::task::Waker;

use env::rng::Rng;
use types::time::{Monotonic, Span};

use crate::link;
use udp::{Binding, Datagram};

pub(crate) use udp::Bound;

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

/// The network of a run. Each fate comes from the network's own stream of the seed.
pub(crate) struct Network {
    /// The link of each ordered pair of nodes that has no link of its own.
    default: link::Config,
    links: BTreeMap<(usize, usize), link::Config>,
    bindings: BTreeMap<u64, Binding>,
    /// Datagrams by true arrival time, then by a key in the order they were sent.
    flights: BTreeMap<(Monotonic, u64), Datagram>,
    rng: Rng,
    next: u64,
    /// A hash of every send and arrival, in order.
    digest: DefaultHasher,
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
            digest: DefaultHasher::new(),
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

    /// The link from node `from` to `ip`.
    fn route(&self, from: usize, ip: IpAddr) -> link::Config {
        let to = node(ip);
        *(to.and_then(|to| self.links.get(&(from, to)))).unwrap_or(&self.default)
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

    /// Delivers the packets that arrive by true time `at`, and returns the wakers of
    /// the ends that receive them.
    pub(crate) fn deliver(&mut self, at: Monotonic) -> Vec<Waker> {
        let mut wakers = Vec::new();
        while let Some(flight) = self.flights.first_entry() {
            if flight.key().0 > at {
                break;
            }
            let datagram = flight.remove();
            wakers.extend(self.receive(at, datagram));
        }
        wakers
    }

    /// A hash of every send and arrival so far.
    pub(crate) fn digest(&self) -> u64 {
        self.digest.finish()
    }
}
