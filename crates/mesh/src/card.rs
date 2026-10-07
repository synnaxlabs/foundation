//! A node's card: what a node states about itself, signed with its own node key.

use std::fmt;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};

use aws_lc_rs::signature::KeyPair;
use transport::Address;
use types::name::Name;
use types::node::{self, PrivateKey, PublicKey, SealKey};

use crate::bytes::{put_count, put_key, take, take_count};
use crate::ed25519;

const TAG: &[u8] = b"foundation/card/1";

const UDP: u8 = 0;
const TCP: u8 = 1;
const RELAY: u8 = 2;

const V4: u8 = 4;
const V6: u8 = 6;

/// What a node states about itself. Its own Ed25519 key signs all of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    /// The full name, under the prefix of the node's region.
    pub name: Name,
    /// The Ed25519 key that `transport` pins.
    pub public_key: PublicKey,
    /// The key that callers seal secret values to.
    pub seal_key: SealKey,
    /// Where to dial the node. An IPv6 address has no flow info and no scope: each
    /// means something only on the node that sets it.
    pub addresses: Vec<Address>,
    /// 1 for the node's first card; each later card is higher.
    pub version: u64,
}

impl Card {
    /// Adds the one byte form of the card to `out`: the name behind a length byte, the
    /// public key, the seal key, a count of addresses as 8 little-endian bytes, each
    /// address, then the version as 8 little-endian bytes.
    ///
    /// # Panics
    ///
    /// When an IPv6 address has flow info or a scope.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        let name = self.name.as_str().as_bytes();
        out.push(
            u8::try_from(name.len()).expect("invariant: a name is at most 255 bytes"),
        );
        out.extend(name);
        out.extend(self.public_key.to_bytes());
        out.extend(self.seal_key.to_bytes());
        put_count(self.addresses.len(), out);
        for &address in &self.addresses {
            put_address(address, out);
        }
        out.extend(self.version.to_le_bytes());
    }

    /// Takes one card from the start of `bytes`. `None` when the bytes do not start
    /// with what [`Card::encode`] gives; `bytes` is then at no known place.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the `Join` change of #336 is the first user")
    )]
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        let [len] = take(bytes)?;
        let (name, rest) = bytes.split_at_checked(usize::from(len))?;
        *bytes = rest;
        let name = std::str::from_utf8(name).ok()?.parse().ok()?;
        let public_key = PublicKey::new(take(bytes)?).ok()?;
        let seal_key = SealKey::new(take(bytes)?).ok()?;
        let count = take_count(bytes)?;
        let mut addresses = Vec::new();
        for _ in 0..count {
            addresses.push(take_address(bytes)?);
        }
        let version = u64::from_le_bytes(take(bytes)?);
        Some(Self {
            name,
            public_key,
            seal_key,
            addresses,
            version,
        })
    }
}

/// A card that its own `public_key` signed, over `foundation/card/1`, the node key,
/// and the card's encoding. Only [`Signed::sign`] and [`Signed::check`] make one.
/// It proves only that the key in the card signed it. That the node owns the key
/// comes from its admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    card: Card,
    signature: [u8; 64],
}

impl Signed {
    /// Signs `card` of node `key`.
    ///
    /// # Panics
    ///
    /// When `card.public_key` is not the public half of `private_key`, or when an IPv6
    /// address has flow info or a scope.
    #[must_use]
    pub fn sign(key: node::Key, card: Card, private_key: &PrivateKey) -> Self {
        let pair = ed25519::pair(private_key);
        assert!(
            pair.public_key().as_ref() == card.public_key.to_bytes(),
            "the card's public key is not the public half of the private key"
        );
        let signature = ed25519::sign(&pair, &statement(key, &card));
        Self { card, signature }
    }

    /// Checks `signature` over `card` of node `key`.
    ///
    /// # Errors
    ///
    /// [`Forged`] when it does not hold for `card.public_key`.
    ///
    /// # Panics
    ///
    /// When an IPv6 address has flow info or a scope.
    pub fn check(
        key: node::Key,
        card: Card,
        signature: [u8; 64],
    ) -> Result<Self, Forged> {
        if !ed25519::holds(card.public_key, &statement(key, &card), &signature) {
            return Err(Forged { node: key });
        }
        Ok(Self { card, signature })
    }

    /// The card.
    #[must_use]
    pub const fn card(&self) -> &Card {
        &self.card
    }

    /// The signature over the card.
    #[must_use]
    pub const fn signature(&self) -> &[u8; 64] {
        &self.signature
    }
}

/// A card whose signature does not hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forged {
    /// The node the card names.
    pub node: node::Key,
}

impl fmt::Display for Forged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the card of node {} is forged", self.node)
    }
}

impl std::error::Error for Forged {}

// The bytes a node signs for its card. They name the node, so nodes that share a key
// cannot share a card.
fn statement(key: node::Key, card: &Card) -> Vec<u8> {
    let mut bytes = TAG.to_vec();
    put_key(key, &mut bytes);
    card.encode(&mut bytes);
    bytes
}

// A kind byte, then the socket; a relay puts its public key before the socket.
fn put_address(address: Address, out: &mut Vec<u8>) {
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
            out.extend(node.to_bytes());
            put_socket(at, out);
        }
    }
}

fn take_address(bytes: &mut &[u8]) -> Option<Address> {
    let [kind] = take(bytes)?;
    Some(match kind {
        UDP => Address::Udp(take_socket(bytes)?),
        TCP => Address::Tcp(take_socket(bytes)?),
        RELAY => Address::Relay {
            node: PublicKey::new(take(bytes)?).ok()?,
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
            assert!(
                at.flowinfo() == 0 && at.scope_id() == 0,
                "a card address has IPv6 flow info or a scope"
            );
            out.push(V6);
            out.extend(at.ip().octets());
            out.extend(at.port().to_le_bytes());
        }
    }
}

fn take_socket(bytes: &mut &[u8]) -> Option<SocketAddr> {
    let [family] = take(bytes)?;
    Some(match family {
        V4 => {
            let ip = <[u8; 4]>::into(take(bytes)?);
            SocketAddr::V4(SocketAddrV4::new(ip, u16::from_le_bytes(take(bytes)?)))
        }
        V6 => {
            let ip = <[u8; 16]>::into(take(bytes)?);
            let port = u16::from_le_bytes(take(bytes)?);
            SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::common::{key, private, public};

    fn seal_key() -> impl Strategy<Value = SealKey> {
        any::<[u8; 32]>()
            .prop_filter_map("a valid seal key", |bytes| SealKey::new(bytes).ok())
    }

    fn socket() -> impl Strategy<Value = SocketAddr> {
        any::<SocketAddr>().prop_map(|mut at| {
            if let SocketAddr::V6(v6) = &mut at {
                v6.set_flowinfo(0);
                v6.set_scope_id(0);
            }
            at
        })
    }

    fn address() -> impl Strategy<Value = Address> {
        prop_oneof![
            socket().prop_map(Address::Udp),
            socket().prop_map(Address::Tcp),
            (1..=u8::MAX, socket()).prop_map(|(id, at)| Address::Relay {
                node: public(id),
                at,
            }),
        ]
    }

    // The card of node `id`, with its own public key.
    fn card(id: u8) -> impl Strategy<Value = Card> {
        (
            "[a-z]{1,8}(\\.[a-z0-9_-]{1,8}){0,3}",
            seal_key(),
            prop::collection::vec(address(), 0..4),
            any::<u64>(),
        )
            .prop_map(move |(name, seal_key, addresses, version)| Card {
                name: name.parse().unwrap(),
                public_key: public(id),
                seal_key,
                addresses,
                version,
            })
    }

    fn fixed() -> Card {
        Card {
            name: "plant.node".parse().unwrap(),
            public_key: public(1),
            seal_key: SealKey::new([9; 32]).unwrap(),
            addresses: vec![
                Address::Udp("10.0.0.1:4100".parse().unwrap()),
                Address::Relay {
                    node: public(2),
                    at: "[fe80::1]:4100".parse().unwrap(),
                },
            ],
            version: 1,
        }
    }

    fn encoded(card: &Card) -> Vec<u8> {
        let mut bytes = Vec::new();
        card.encode(&mut bytes);
        bytes
    }

    fn decoded(bytes: &[u8]) -> Option<Card> {
        let mut rest = bytes;
        let card = Card::decode(&mut rest)?;
        rest.is_empty().then_some(card)
    }

    proptest! {
        #[test]
        fn a_card_round_trips(card in card(1)) {
            prop_assert_eq!(decoded(&encoded(&card)), Some(card));
        }

        #[test]
        fn no_prefix_of_a_card_decodes(card in card(1)) {
            let bytes = encoded(&card);
            for len in 0..bytes.len() {
                prop_assert_eq!(Card::decode(&mut &bytes[..len]), None);
            }
        }

        #[test]
        fn a_signed_card_checks(card in card(3)) {
            let signed = Signed::sign(key(3), card.clone(), &private(3));
            prop_assert_eq!(signed.card(), &card);
            let checked = Signed::check(key(3), card, *signed.signature());
            prop_assert_eq!(checked, Ok(signed));
        }
    }

    #[test]
    fn decode_takes_the_card_and_leaves_the_rest() {
        let mut bytes = encoded(&fixed());
        bytes.push(7);
        let mut rest = bytes.as_slice();
        assert_eq!(Card::decode(&mut rest), Some(fixed()));
        assert_eq!(rest, [7]);
    }

    #[test]
    fn decode_refuses_a_value_that_encode_never_gives() {
        // The name is 10 bytes behind its length byte. The public key is at 11, the
        // seal key at 43, the first address at 83, and the relay key at 92.
        let mut identity = [0; 32];
        identity[0] = 1;
        let cases: [(&str, usize, &[u8]); 8] = [
            ("an empty name", 0, &[0]),
            ("a name with an empty segment", 1, b"."),
            ("a name that is not UTF-8", 1, &[0xff]),
            ("a public key of small order", 11, &identity),
            ("a seal key of small order", 43, &[0; 32]),
            ("an unknown address kind", 83, &[3]),
            ("an unknown socket family", 84, &[5]),
            ("a relay key of small order", 92, &identity),
        ];
        for (case, at, value) in cases {
            let mut bytes = encoded(&fixed());
            bytes[at..at + value.len()].copy_from_slice(value);
            assert_eq!(decoded(&bytes), None, "{case}");
        }
    }

    #[test]
    fn check_refuses_a_change_to_the_card_or_the_key() {
        let signed = Signed::sign(key(1), fixed(), &private(1));
        let signature = *signed.signature();
        let mut renamed = fixed();
        renamed.name = "plant.other".parse().unwrap();
        let mut resealed = fixed();
        resealed.seal_key = SealKey::new([10; 32]).unwrap();
        let mut moved = fixed();
        moved.addresses.pop();
        let mut newer = fixed();
        newer.version = 2;
        for card in [renamed, resealed, moved, newer] {
            assert_eq!(
                Signed::check(key(1), card, signature),
                Err(Forged { node: key(1) })
            );
        }
        assert_eq!(
            Signed::check(key(2), fixed(), signature),
            Err(Forged { node: key(2) })
        );
        let mut flipped = signature;
        flipped[0] ^= 1;
        assert_eq!(
            Signed::check(key(1), fixed(), flipped),
            Err(Forged { node: key(1) })
        );
    }

    #[test]
    fn check_refuses_a_card_that_another_key_signed() {
        let signature = *Signed::sign(key(2), fixed_of(2), &private(2)).signature();
        assert_eq!(
            Signed::check(key(2), fixed(), signature),
            Err(Forged { node: key(2) })
        );
    }

    fn fixed_of(id: u8) -> Card {
        Card {
            public_key: public(id),
            ..fixed()
        }
    }

    #[test]
    fn forged_names_the_node() {
        let forged = Forged { node: key(7) };
        assert_eq!(
            forged.to_string(),
            format!("the card of node {} is forged", key(7))
        );
    }

    #[test]
    #[should_panic(expected = "the card's public key is not the public half of the \
                               private key")]
    fn sign_refuses_another_private_key() {
        let _signed = Signed::sign(key(1), fixed(), &private(2));
    }

    #[test]
    #[should_panic(expected = "a card address has IPv6 flow info or a scope")]
    fn sign_refuses_a_scoped_address() {
        let mut card = fixed();
        card.addresses = vec![Address::Tcp("[fe80::1%3]:4100".parse().unwrap())];
        let _signed = Signed::sign(key(1), card, &private(1));
    }

    #[test]
    #[should_panic(expected = "a card address has IPv6 flow info or a scope")]
    fn check_refuses_an_address_with_flow_info() {
        let mut card = fixed();
        let mut at: SocketAddrV6 = "[2001:db8::1]:4100".parse().unwrap();
        at.set_flowinfo(1);
        card.addresses = vec![Address::Udp(SocketAddr::V6(at))];
        let _checked = Signed::check(key(1), card, [0; 64]);
    }

    #[test]
    fn the_signed_bytes_are_the_tag_the_key_and_the_card() {
        let card = Card {
            name: "a.b".parse().unwrap(),
            public_key: public(1),
            seal_key: SealKey::new([9; 32]).unwrap(),
            addresses: vec![
                Address::Udp("5.6.7.8:1".parse().unwrap()),
                Address::Tcp("1.2.3.4:258".parse().unwrap()),
                Address::Relay {
                    node: public(2),
                    at: "[::1]:258".parse().unwrap(),
                },
            ],
            version: 3,
        };
        let mut expected = b"foundation/card/1".to_vec();
        expected.extend(1u128.to_le_bytes());
        expected.push(3);
        expected.extend(b"a.b");
        expected.extend(public(1).to_bytes());
        expected.extend([9; 32]);
        expected.extend(3u64.to_le_bytes());
        expected.extend([0, 4, 5, 6, 7, 8, 1, 0]);
        expected.extend([1, 4, 1, 2, 3, 4, 2, 1]);
        expected.push(2);
        expected.extend(public(2).to_bytes());
        expected.push(6);
        expected.extend(1u128.to_be_bytes());
        expected.extend([2, 1]);
        expected.extend(3u64.to_le_bytes());
        assert_eq!(statement(key(1), &card), expected);
    }

    #[test]
    fn each_one_byte_change_that_decodes_is_the_byte_form() {
        let bytes = encoded(&fixed());
        for at in 0..bytes.len() {
            for byte in 0..=u8::MAX {
                let mut changed = bytes.clone();
                changed[at] = byte;
                if let Some(found) = decoded(&changed) {
                    assert_eq!(encoded(&found), changed, "byte {at} set to {byte}");
                }
            }
        }
    }
}
