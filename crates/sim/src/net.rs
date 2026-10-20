//! The network of a run: node addresses, the wire between them, and a module for each
//! protocol.

pub(crate) mod tcp;
pub(crate) mod udp;
mod wire;

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

use env::net::Error;
use env::rng::Rng;
use types::time::Monotonic;

use crate::log::Waker;
use crate::name::key;
use crate::{Crash, link, name};
use wire::{Packet, Wire};

/// `10.0.0.0`: node `k` has `10.0.0.0` plus `k + 1`.
const V4: u32 = 0x0a00_0000;
/// `fd00::`: node `k` has `fd00::` plus `k + 1`.
const V6: u128 = 0xfd00 << 112;
/// `fe80::`: node `k` has the link-local address `fe80::` plus `k + 1`.
const LINK_LOCAL: u128 = 0xfe80 << 112;
/// The index of the one interface of each node, which holds its link-local address.
pub(crate) const INTERFACE: u32 = 2;
/// The broadcast host of `10.0.0.0/8`, which no node has.
const BROADCAST: u32 = 0x00ff_ffff;
/// The first port that port 0 binds.
const EPHEMERAL: u16 = 49_152;
/// The Linux code for an address that is not on the node (`EADDRNOTAVAIL`).
const NOT_AVAILABLE: i32 = 99;
/// The code that `os` gives on Linux (`EINVAL`) for a link-local bind with no scope, or
/// for a send from a source that the socket cannot send from and that is IPv6, not
/// unicast, or to an IPv6 address.
const INVALID: i32 = 22;
/// The Linux code for a scope that names no interface (`ENODEV`).
const NO_DEVICE: i32 = 19;

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

/// The link-local address of node `node`, on [`INTERFACE`].
///
/// # Panics
///
/// As [`addresses`].
pub(crate) fn link_local(node: usize) -> Ipv6Addr {
    let [_, IpAddr::V6(v6)] = addresses(node) else {
        unreachable!("the second address is IPv6")
    };
    Ipv6Addr::from_bits(LINK_LOCAL + (v6.to_bits() - V6))
}

/// The bytes of the IP header of a packet to `ip`: 20 for IPv4, 40 for IPv6.
fn ip_header(ip: IpAddr) -> usize {
    if ip.is_ipv4() { 20 } else { 40 }
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
/// port from 49152. As on Linux, only the link-local address keeps the scope of
/// `local`, and no IPv6 address keeps flow info. No packet reaches the link-local
/// address: the first test that sends to one adds it to [`node`] and a check of the
/// scope of a send.
///
/// # Errors
///
/// - [`Error::Io`] with code 22 (`EINVAL`) when `local` is link-local with scope 0.
/// - [`Error::Io`] with code 19 (`ENODEV`) when `local` is link-local with a scope
///   other than [`INTERFACE`].
/// - [`Error::Io`] with code 99 (`EADDRNOTAVAIL`) when the IP of `local` is not
///   unspecified and not an address of `node`, nor its link-local address.
/// - [`Error::AddressInUse`] when a socket in `bound` takes the port, or when no
///   port is free.
fn bind(
    node: usize,
    local: SocketAddr,
    bound: &(impl Iterator<Item = SocketAddr> + Clone),
) -> Result<SocketAddr, Error> {
    let ip = local.ip();
    let scope = match local {
        SocketAddr::V6(local) if local.ip().is_unicast_link_local() => {
            match local.scope_id() {
                0 => return Err(Error::Io { code: INVALID }),
                INTERFACE => INTERFACE,
                _ => return Err(Error::Io { code: NO_DEVICE }),
            }
        }
        _ => 0,
    };
    let held = addresses(node).contains(&ip) || ip == IpAddr::V6(link_local(node));
    if !ip.is_unspecified() && !held {
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
    Ok(match local {
        SocketAddr::V4(_) => SocketAddr::new(ip, port),
        SocketAddr::V6(local) => SocketAddrV6::new(*local.ip(), port, 0, scope).into(),
    })
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
    /// It arrived where nothing is bound, at a failed socket, or at a full queue.
    Dropped,
    /// It arrived at a TCP end or listener.
    Arrived,
}

/// The network of a run.
pub(crate) struct Network {
    wire: Wire,
    udp: udp::Sockets,
    tcp: tcp::Sockets,
    /// The names that [`Sim::name`](crate::Sim::name) gave.
    names: BTreeMap<String, name::Config>,
}

impl Network {
    pub(crate) fn new(default: link::Config, rng: Rng) -> Self {
        Self {
            wire: Wire::new(default, rng),
            udp: udp::Sockets::default(),
            tcp: tcp::Sockets::default(),
            names: BTreeMap::new(),
        }
    }

    /// Makes each lookup of `name`, a name of `Host`, from now on go as `config` says.
    pub(crate) fn name(&mut self, name: &str, config: name::Config) {
        self.names.insert(key(name).to_owned(), config);
    }

    /// How a lookup of `name`, a name of `Host`, that starts now goes.
    pub(crate) fn lookup(&self, name: &str) -> name::Config {
        self.names.get(key(name)).cloned().unwrap_or_default()
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

    /// Drops the packets of `node` that have not left their links by true time `now`,
    /// and ends its TCP streams and listeners with no segment, when its power is cut.
    /// This comes before the drop of its futures. Returns the wakers of the streams
    /// and listeners, for the caller to drop after it releases the lock.
    pub(crate) fn crash(
        &mut self,
        now: Monotonic,
        node: usize,
        crash: Crash,
    ) -> Vec<Waker> {
        match crash {
            Crash::Process => Vec::new(),
            Crash::Power => {
                self.wire.cut(now, node);
                self.udp().cut_power(node);
                self.tcp().cut_power(node)
            }
        }
    }

    /// Takes the first case met that sim does not simulate yet.
    pub(crate) fn yet(&mut self) -> Option<&'static str> {
        self.tcp.yet()
    }

    /// The true time of the first arrival, of the first departure that frees a send
    /// buffer, or of the first lost TCP segment that goes again.
    pub(crate) fn first(&self) -> Option<Monotonic> {
        let first = self.wire.first().into_iter().chain(self.udp.first());
        first.chain(self.tcp.first()).min()
    }

    /// Sends again the lost TCP segments due by true time `at`, delivers the packets
    /// that arrive by then, and frees the send buffers of the datagrams that leave by
    /// then. Returns the wakers of the ends that receive or time out and of the sends
    /// that find room.
    pub(crate) fn deliver(&mut self, at: Monotonic) -> Vec<Waker> {
        let mut wakers = self.udp().free(at);
        wakers.extend(self.tcp().retry(at));
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
