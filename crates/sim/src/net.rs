//! The network of a run: node addresses, the wire between them, and a module for each
//! protocol.

pub(crate) mod tcp;
pub(crate) mod udp;
mod wire;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::task::Waker;

use env::net::Error;
use env::rng::Rng;
use types::time::Monotonic;

use crate::{Crash, link};
use wire::{Packet, Wire};

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
pub(crate) fn node(ip: IpAddr) -> Option<usize> {
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

/// Whether a socket of `node` bound to `local` receives a packet to `destination`.
fn receives(node: usize, local: SocketAddr, destination: SocketAddr) -> bool {
    let ip = destination.ip();
    local.port() == destination.port()
        && covers(local.ip(), ip)
        && self::node(ip) == Some(node)
}

/// The address that a bind of `local` on `node` takes, where `bound` gives the local
/// address of each socket of the node of the same kind. Port 0 takes the first free
/// port from 49152.
///
/// # Errors
///
/// - [`Error::Io`] with code 99 (`EADDRNOTAVAIL`) when the IP of `local` is not
///   unspecified and not an address of `node`.
/// - [`Error::AddressInUse`] when a socket in `bound` takes the port, or when no
///   port is free.
fn bind(
    node: usize,
    local: SocketAddr,
    bound: &(impl Iterator<Item = SocketAddr> + Clone),
) -> Result<SocketAddr, Error> {
    let ip = local.ip();
    if !ip.is_unspecified() && !addresses(node).contains(&ip) {
        return Err(Error::Io {
            code: NOT_AVAILABLE,
        });
    }
    let taken = |port: u16| {
        bound.clone().any(|other| {
            other.port() == port && (covers(ip, other.ip()) || covers(other.ip(), ip))
        })
    };
    let port = match local.port() {
        0 => (EPHEMERAL..=u16::MAX).find(|&port| !taken(port)),
        port => Some(port).filter(|&port| !taken(port)),
    };
    let Some(port) = port else {
        return Err(Error::AddressInUse { local });
    };
    Ok(SocketAddr::new(ip, port))
}

/// What happens to a packet, for the digest.
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
    /// It arrived at a TCP end or listener.
    Arrived,
}

/// The network of a run.
pub(crate) struct Network {
    wire: Wire,
    udp: udp::Sockets,
    tcp: tcp::Sockets,
}

impl Network {
    pub(crate) fn new(default: link::Config, rng: Rng) -> Self {
        Self {
            wire: Wire::new(default, rng),
            udp: udp::Sockets::default(),
            tcp: tcp::Sockets::default(),
        }
    }

    /// Sets the link from node `from` to node `to`.
    pub(crate) fn link(&mut self, from: usize, to: usize, config: link::Config) {
        self.wire.link(from, to, config);
    }

    /// The UDP sockets.
    pub(crate) fn udp(&mut self) -> udp::Udp<'_> {
        udp::Udp::new(&mut self.udp, &mut self.wire)
    }

    /// The TCP streams and listeners.
    pub(crate) fn tcp(&mut self) -> tcp::Tcp<'_> {
        tcp::Tcp::new(&mut self.tcp, &mut self.wire)
    }

    /// Ends the TCP streams and listeners of `node` with no segment when its power is
    /// cut, which comes before the drop of its futures. Returns their wakers, for the
    /// caller to drop after it releases the lock.
    pub(crate) fn crash(&mut self, node: usize, crash: Crash) -> Vec<Waker> {
        match crash {
            Crash::Process => Vec::new(),
            Crash::Power => self.tcp().cut_power(node),
        }
    }

    /// Takes the first case met that sim does not simulate yet.
    pub(crate) fn yet(&mut self) -> Option<&'static str> {
        self.tcp.yet()
    }

    /// The true time of the first arrival.
    pub(crate) fn first(&self) -> Option<Monotonic> {
        self.wire.first()
    }

    /// Delivers the packets that arrive by true time `at`, and returns the wakers of
    /// the ends that receive them.
    pub(crate) fn deliver(&mut self, at: Monotonic) -> Vec<Waker> {
        let mut wakers = Vec::new();
        while let Some(packet) = self.wire.pop(at) {
            match packet {
                Packet::Datagram(datagram) => {
                    wakers.extend(self.udp().queue(at, datagram));
                }
                Packet::Segment(segment) => {
                    wakers.extend(self.tcp().arrive(at, segment));
                }
            }
        }
        wakers
    }

    /// A hash of every send and arrival so far.
    pub(crate) fn digest(&self) -> u64 {
        self.wire.digest()
    }
}
