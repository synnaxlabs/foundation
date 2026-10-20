//! The addresses on a card: where to dial a node.

use std::fmt;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};

use transport::Address;

use crate::bytes;

const UDP: u8 = 0;
const TCP: u8 = 1;
const RELAY: u8 = 2;

const V4: u8 = 4;
const V6: u8 = 6;

// Every member keeps every card, so one node's card must not set the size of each
// member's state.
pub(crate) const MAX: u8 = 32;

/// Where to dial a node: at most 32 addresses, and no IPv6 address with flow info or
/// a scope, since each means something only on the node that sets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addresses(Vec<Address>);

impl Addresses {
    /// The addresses of `list`, in its order.
    ///
    /// # Errors
    ///
    /// [`Invalid::Count`] for more than 32 addresses, and [`Invalid::Scoped`] for the
    /// first IPv6 address with flow info or a scope.
    pub fn new(list: Vec<Address>) -> Result<Self, Invalid> {
        let count = list.len();
        if count > usize::from(MAX) {
            return Err(Invalid::Count { count });
        }
        let scoped = list
            .iter()
            .find(|&&address| flow_and_scope(address) != (0, 0));
        if let Some(&address) = scoped {
            return Err(Invalid::Scoped { address });
        }
        Ok(Self(list))
    }

    /// The addresses of `bound` that a peer can dial, in order: each but an
    /// unspecified address and an IPv6 address with flow info or a scope, at most 32.
    #[must_use]
    pub fn dialable(mut bound: Vec<Address>) -> Self {
        // Linux gives `[::ffff:0.0.0.0]` for a dual-stack socket bound there.
        bound.retain(|&address| {
            let ip = socket(address).ip().to_canonical();
            !ip.is_unspecified() && flow_and_scope(address) == (0, 0)
        });
        bound.truncate(usize::from(MAX));
        Self(bound)
    }

    /// The addresses, in order.
    #[must_use]
    pub fn as_slice(&self) -> &[Address] {
        &self.0
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        bytes::put_count(self.0.len(), out);
        for &address in &self.0 {
            put(address, out);
        }
    }

    // Needs no check of `new`: the count is checked first, and the byte form holds no
    // flow info and no scope.
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        let count = bytes::take_count(bytes)?;
        if count > u64::from(MAX) {
            return None;
        }
        let mut list = Vec::new();
        for _ in 0..count {
            list.push(take(bytes)?);
        }
        Some(Self(list))
    }
}

/// Why a list of addresses cannot go on a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// More than 32 addresses.
    Count {
        /// The addresses in the list.
        count: usize,
    },
    /// An IPv6 address with flow info or a scope.
    Scoped {
        /// The address.
        address: Address,
    },
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count { count } => {
                write!(f, "a card holds {count} addresses, more than {MAX}")
            }
            Self::Scoped { address } => {
                let (flowinfo, scope_id) = flow_and_scope(*address);
                write!(
                    f,
                    "the address {address:?} has IPv6 flow info {flowinfo} and scope \
                     {scope_id}, which mean something only on the node that sets it"
                )
            }
        }
    }
}

const fn socket(address: Address) -> SocketAddr {
    let (Address::Udp(at) | Address::Tcp(at) | Address::Relay { at, .. }) = address;
    at
}

// The flow info and scope of an IPv6 address; 0 and 0 for IPv4.
fn flow_and_scope(address: Address) -> (u32, u32) {
    match socket(address) {
        SocketAddr::V4(_) => (0, 0),
        SocketAddr::V6(at) => (at.flowinfo(), at.scope_id()),
    }
}

impl std::error::Error for Invalid {}

// A kind byte, then the socket; a relay puts its public key before the socket.
fn put(address: Address, out: &mut Vec<u8>) {
    match address {
        Address::Udp(at) => {
            out.push(UDP);
            put_socket(at, out);
        }
        Address::Tcp(at) => {
            out.push(TCP);
            put_socket(at, out);
        }
        Address::Relay { node, at } => {
            out.push(RELAY);
            bytes::put_public_key(node, out);
            put_socket(at, out);
        }
    }
}

fn take(bytes: &mut &[u8]) -> Option<Address> {
    let [kind] = bytes::take(bytes)?;
    Some(match kind {
        UDP => Address::Udp(take_socket(bytes)?),
        TCP => Address::Tcp(take_socket(bytes)?),
        RELAY => Address::Relay {
            node: bytes::take_public_key(bytes)?,
            at: take_socket(bytes)?,
        },
        _ => return None,
    })
}

// A family byte, the IP, then the port as 2 little-endian bytes.
fn put_socket(at: SocketAddr, out: &mut Vec<u8>) {
    match at {
        SocketAddr::V4(at) => {
            out.push(V4);
            out.extend(at.ip().octets());
            out.extend(at.port().to_le_bytes());
        }
        SocketAddr::V6(at) => {
            out.push(V6);
            out.extend(at.ip().octets());
            out.extend(at.port().to_le_bytes());
        }
    }
}

fn take_socket(bytes: &mut &[u8]) -> Option<SocketAddr> {
    let [family] = bytes::take(bytes)?;
    Some(match family {
        V4 => {
            let ip = <[u8; 4]>::into(bytes::take(bytes)?);
            SocketAddr::V4(SocketAddrV4::new(
                ip,
                u16::from_le_bytes(bytes::take(bytes)?),
            ))
        }
        V6 => {
            let ip = <[u8; 16]>::into(bytes::take(bytes)?);
            let port = u16::from_le_bytes(bytes::take(bytes)?);
            SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use proptest::prelude::*;

    use super::*;
    use crate::common::public;

    // A count of 0 to 40, then that many addresses of any kind and family, then any
    // bytes, so that a byte form that reads more takes arbitrary values.
    fn encoded() -> impl Strategy<Value = Vec<u8>> {
        let address = (0..3u8, any::<bool>(), any::<[u8; 18]>(), 1..4u8);
        let tail = prop::collection::vec(any::<u8>(), 0..64);
        (prop::collection::vec(address, 0..=40), tail).prop_map(|(list, tail)| {
            let mut out = Vec::new();
            bytes::put_count(list.len(), &mut out);
            for (kind, v6, ip, node) in list {
                out.push(kind);
                if kind == RELAY {
                    out.extend(public(node).to_bytes());
                }
                out.push(if v6 { V6 } else { V4 });
                let ip_len = if v6 { 16 } else { 4 };
                out.extend(&ip[..ip_len]);
                out.extend(&ip[16..]);
            }
            out.extend(tail);
            out
        })
    }

    proptest! {
        #[test]
        fn decode_keeps_the_rules(bytes in encoded()) {
            if let Some(addresses) = Addresses::decode(&mut bytes.as_slice()) {
                let list = addresses.as_slice().to_vec();
                prop_assert_eq!(Addresses::new(list), Ok(addresses));
            }
        }
    }

    fn bound() -> impl Strategy<Value = Address> {
        let ip = prop_oneof![
            Just(IpAddr::from([0, 0, 0, 0])),
            Just(IpAddr::from([0_u16; 8])),
            Just(IpAddr::from([0, 0, 0, 0, 0, 0xffff, 0, 0])),
            any::<[u8; 4]>().prop_map(IpAddr::from),
            any::<[u8; 16]>().prop_map(IpAddr::from),
        ];
        let at = (ip, any::<u16>(), 0..2_u32, 0..3_u32).prop_map(
            |(ip, port, flowinfo, scope_id)| match ip {
                IpAddr::V4(ip) => SocketAddr::from((ip, port)),
                IpAddr::V6(ip) => {
                    SocketAddr::V6(SocketAddrV6::new(ip, port, flowinfo, scope_id))
                }
            },
        );
        (0..3_u8, at).prop_map(|(kind, at)| match kind {
            UDP => Address::Udp(at),
            TCP => Address::Tcp(at),
            _ => Address::Relay {
                node: public(2),
                at,
            },
        })
    }

    proptest! {
        // Each address alone is dialable when `new` takes it and its IP is not
        // unspecified.
        #[test]
        fn dialable_keeps_the_first_32_that_a_peer_can_dial(
            list in prop::collection::vec(bound(), 0..48),
        ) {
            let kept: Vec<_> = list
                .iter()
                .copied()
                .filter(|&address| {
                    let ip = socket(address).ip().to_canonical();
                    Addresses::new(vec![address]).is_ok() && !ip.is_unspecified()
                })
                .take(usize::from(MAX))
                .collect();
            prop_assert_eq!(Addresses::dialable(list), Addresses::new(kept).unwrap());
        }
    }

    #[test]
    fn dialable_drops_an_unspecified_or_scoped_address() {
        let v4 = Address::Udp("10.0.0.1:4100".parse().unwrap());
        let v6 = Address::Udp("[2001:db8::1]:4100".parse().unwrap());
        let bound = vec![
            Address::Udp("0.0.0.0:4100".parse().unwrap()),
            v4,
            Address::Tcp("[::]:4100".parse().unwrap()),
            Address::Udp("[fe80::1%3]:4100".parse().unwrap()),
            v6,
        ];
        assert_eq!(Addresses::dialable(bound).as_slice(), [v4, v6]);
        let udp = |port| Address::Udp(SocketAddr::from(([10, 0, 0, 1], port)));
        let many: Vec<_> = (0..40).map(udp).collect();
        let first: Vec<_> = (0..32).map(udp).collect();
        assert_eq!(Addresses::dialable(many).as_slice(), first);
    }

    #[test]
    fn dialable_drops_an_ipv4_mapped_unspecified_address() {
        let mapped: SocketAddr = "[::ffff:0.0.0.0]:4100".parse().unwrap();
        let bound = vec![Address::Udp(mapped), Address::Tcp(mapped)];
        assert_eq!(Addresses::dialable(bound).as_slice(), []);
    }

    #[test]
    fn new_refuses_an_ipv6_address_with_a_scope_or_flow_info() {
        let scoped = Address::Relay {
            node: public(2),
            at: "[fe80::1%3]:4100".parse().unwrap(),
        };
        let mut flowing: SocketAddrV6 = "[2001:db8::1]:4100".parse().unwrap();
        flowing.set_flowinfo(1);
        let flowing = Address::Udp(SocketAddr::V6(flowing));
        let plain = Address::Tcp("[2001:db8::1]:4100".parse().unwrap());
        for address in [scoped, flowing] {
            assert_eq!(
                Addresses::new(vec![plain, address, scoped]),
                Err(Invalid::Scoped { address })
            );
        }
    }

    #[test]
    fn invalid_says_what_is_wrong() {
        let mut at: SocketAddrV6 = "[fe80::1%3]:4100".parse().unwrap();
        at.set_flowinfo(7);
        let address = Address::Tcp(SocketAddr::V6(at));
        assert_eq!(
            Invalid::Count { count: 33 }.to_string(),
            "a card holds 33 addresses, more than 32"
        );
        assert_eq!(
            Invalid::Scoped { address }.to_string(),
            "the address Tcp([fe80::1%3]:4100) has IPv6 flow info 7 and scope 3, which \
             mean something only on the node that sets it"
        );
    }

    #[test]
    fn new_takes_at_most_32() {
        let udp = Address::Udp("10.0.0.1:4100".parse().unwrap());
        assert_eq!(
            Addresses::new(vec![udp; 32]).map(|a| a.as_slice().len()),
            Ok(32)
        );
        assert_eq!(
            Addresses::new(vec![udp; 33]),
            Err(Invalid::Count { count: 33 })
        );
    }
}
