//! A node's card: what a node states about itself, signed with its own node key.

use std::fmt;

use types::name::Name;
use types::node::{self, PrivateKey, PublicKey, SealKey};

use crate::bytes::{put_key, put_name, take, take_key, take_name};
use crate::ed25519;

pub mod addresses;

const TAG: &[u8] = b"foundation/card/1";

/// What a node states about itself. Its own Ed25519 key signs all of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    /// The full name, under the prefix of the node's region.
    pub name: Name,
    /// The Ed25519 key that `transport` pins.
    pub public_key: PublicKey,
    /// The key that callers seal secret values to.
    pub seal_key: SealKey,
    /// Where to dial the node.
    pub addresses: addresses::Addresses,
    /// 1 for the node's first card; each later card is higher.
    pub version: u64,
}

impl Card {
    /// Adds the one byte form of the card to `out`: the name behind a length byte, the
    /// public key, the seal key, a count of addresses as 8 little-endian bytes, each
    /// address, then the version as 8 little-endian bytes.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_name(&self.name, out);
        out.extend(self.public_key.to_bytes());
        out.extend(self.seal_key.to_bytes());
        self.addresses.encode(out);
        out.extend(self.version.to_le_bytes());
    }

    /// Takes one card from the start of `bytes`. `None` when the bytes do not start
    /// with what [`Card::encode`] gives, such as an address list that
    /// [`Addresses::new`](addresses::Addresses::new) refuses; `bytes` is then at no known
    /// place.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the streams of #471 are the first user")
    )]
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        let name = take_name(bytes)?;
        let public_key = PublicKey::new(take(bytes)?).ok()?;
        let seal_key = SealKey::new(take(bytes)?).ok()?;
        let addresses = addresses::Addresses::decode(bytes)?;
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
/// and the card's encoding. Only [`Signed::sign`] and [`Signed::check`] make one, and
/// each keeps the node key that the signature covers. It proves only that the public
/// key in the card signed it. That the node owns the public key comes from its
/// admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    key: node::Key,
    card: Card,
    signature: [u8; 64],
}

impl Signed {
    /// Signs `card` of node `key`.
    ///
    /// # Panics
    ///
    /// When `card.public_key` is not the public half of `private_key`.
    #[must_use]
    pub fn sign(key: node::Key, card: Card, private_key: &PrivateKey) -> Self {
        let pair = ed25519::pair(private_key);
        assert!(
            ed25519::public(&pair) == card.public_key,
            "the card's public key is not the public half of the private key"
        );
        let signature = ed25519::sign(&pair, &statement(TAG, key, &card));
        Self {
            key,
            card,
            signature,
        }
    }

    /// Checks `signature` over `card` of node `key`.
    ///
    /// # Errors
    ///
    /// [`Forged`] when it does not hold for `card.public_key`.
    pub fn check(
        key: node::Key,
        card: Card,
        signature: [u8; 64],
    ) -> Result<Self, Forged> {
        if !ed25519::holds(card.public_key, &statement(TAG, key, &card), &signature) {
            return Err(Forged { node: key });
        }
        Ok(Self {
            key,
            card,
            signature,
        })
    }

    /// Adds the one byte form of the signed card to `out`: the node key as 16
    /// little-endian bytes, the card, then the signature.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_key(self.key, out);
        self.card.encode(out);
        out.extend(self.signature);
    }

    /// Takes one signed card from the start of `bytes`. `None` when the bytes do not
    /// start with what [`Signed::encode`] gives, or when the signature does not hold;
    /// `bytes` is then at no known place.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the join answer of #336 is the first user")
    )]
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        let key = take_key(bytes)?;
        let card = Card::decode(bytes)?;
        Self::check(key, card, take(bytes)?).ok()
    }

    /// The node that the card is signed for. The signature covers it.
    #[must_use]
    pub const fn key(&self) -> node::Key {
        self.key
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

/// The bytes signed under `tag` for `card` of node `key`. They name the node, so a
/// signature holds for one node only.
pub(crate) fn statement(tag: &[u8], key: node::Key, card: &Card) -> Vec<u8> {
    let mut bytes = tag.to_vec();
    put_key(key, &mut bytes);
    card.encode(&mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use proptest::prelude::*;
    use transport::Address;

    use super::addresses::Addresses;
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

    // Valid names: segments up to a drawn length, kept while they fit the byte
    // limit. Shapes a draw seldom reaches are in the edge tests.
    fn name() -> impl Strategy<Value = Name> {
        (1..=Name::MAX_BYTES)
            .prop_flat_map(|longest| {
                let segment = format!("@?[A-Za-z0-9_-]{{1,{longest}}}");
                let segment = proptest::string::string_regex(&segment).unwrap();
                prop::collection::vec(segment, 1..=Name::MAX_BYTES.div_ceil(2))
            })
            .prop_map(|segments| {
                let mut segments = segments.into_iter();
                let mut name = segments.next().unwrap();
                name.truncate(Name::MAX_BYTES);
                for segment in segments {
                    let longer = format!("{name}.{segment}");
                    if longer.len() > Name::MAX_BYTES {
                        break;
                    }
                    name = longer;
                }
                name.parse().unwrap()
            })
    }

    // The card of node `id`, with its own public key.
    fn card(id: u8) -> impl Strategy<Value = Card> {
        (
            name(),
            seal_key(),
            prop::collection::vec(address(), 0..4),
            any::<u64>(),
        )
            .prop_map(move |(name, seal_key, addresses, version)| Card {
                name,
                public_key: public(id),
                seal_key,
                addresses: Addresses::new(addresses).unwrap(),
                version,
            })
    }

    fn fixed() -> Card {
        Card {
            name: "plant.node".parse().unwrap(),
            public_key: public(1),
            seal_key: SealKey::new([9; 32]).unwrap(),
            addresses: Addresses::new(vec![
                Address::Udp("10.0.0.1:4100".parse().unwrap()),
                Address::Relay {
                    node: public(2),
                    at: "[fe80::1]:4100".parse().unwrap(),
                },
            ])
            .unwrap(),
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
            prop_assert_eq!((signed.key(), signed.card()), (key(3), &card));
            let checked = Signed::check(key(3), card, *signed.signature());
            prop_assert_eq!(checked.as_ref().map(Signed::key), Ok(key(3)));
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
        moved.addresses =
            Addresses::new(fixed().addresses.as_slice()[..1].to_vec()).unwrap();
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
    fn the_signed_bytes_are_the_tag_the_key_and_the_card() {
        let card = Card {
            name: "ab.cd".parse().unwrap(),
            public_key: public(1),
            seal_key: SealKey::new([9; 32]).unwrap(),
            addresses: Addresses::new(vec![
                Address::Udp("5.6.7.8:1".parse().unwrap()),
                Address::Tcp("1.2.3.4:258".parse().unwrap()),
                Address::Relay {
                    node: public(2),
                    at: "[::1]:258".parse().unwrap(),
                },
            ])
            .unwrap(),
            version: 0x0102_0304_0506_0708,
        };
        let mut expected = b"foundation/card/1".to_vec();
        expected.extend(1u128.to_le_bytes());
        expected.push(5);
        expected.extend(b"ab.cd");
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
        expected.extend([8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(statement(TAG, key(1), &card), expected);
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

    #[test]
    fn a_name_of_255_bytes_is_one_length_byte_then_the_name() {
        let name = format!("{}.{}", "a".repeat(127), "b".repeat(127));
        let card = Card {
            name: name.parse().unwrap(),
            addresses: Addresses::new(Vec::new()).unwrap(),
            ..fixed()
        };
        let bytes = encoded(&card);
        assert_eq!(bytes[0], 255);
        assert_eq!(&bytes[1..256], name.as_bytes());
        assert_eq!(bytes.len(), 1 + 255 + 32 + 32 + 8 + 8);
        assert_eq!(decoded(&bytes), Some(card));
    }

    #[test]
    fn a_card_at_the_edges_round_trips() {
        let full = full();
        for version in [0, u64::MAX] {
            let card = Card { version, ..fixed() };
            assert_eq!(decoded(&encoded(&card)), Some(card), "version {version}");
        }
        assert_eq!(decoded(&encoded(&full)), Some(full), "32 addresses");
        let ports = Card {
            addresses: Addresses::new(vec![
                Address::Udp("0.0.0.0:0".parse().unwrap()),
                Address::Tcp("[::]:65535".parse().unwrap()),
                Address::Relay {
                    node: public(1),
                    at: "1.2.3.4:0".parse().unwrap(),
                },
            ])
            .unwrap(),
            ..fixed()
        };
        assert_eq!(decoded(&encoded(&ports)), Some(ports), "ports, own relay");
    }

    // `count` addresses of 8 bytes each.
    fn many(count: usize) -> Vec<Address> {
        vec![Address::Udp("10.0.0.1:4100".parse().unwrap()); count]
    }

    // A card with 32 addresses.
    fn full() -> Card {
        Card {
            addresses: Addresses::new(many(32)).unwrap(),
            ..fixed()
        }
    }

    #[test]
    fn decode_refuses_more_than_32_addresses() {
        // The count is at 75, after the name (11 bytes) and the two keys.
        let mut bytes = encoded(&full());
        assert_eq!(bytes[75..83], 32u64.to_le_bytes());
        let version = bytes.split_off(bytes.len() - 8);
        bytes[75..83].copy_from_slice(&33u64.to_le_bytes());
        bytes.extend([0, 4, 10, 0, 0, 1, 0x04, 0x10]);
        bytes.extend(version);
        assert_eq!(decoded(&bytes), None);
    }

    #[test]
    fn decode_refuses_a_large_count_before_it_takes_an_address() {
        // 288 is 32 in its low byte.
        for count in [288, u64::MAX] {
            let mut bytes = encoded(&full());
            bytes[75..83].copy_from_slice(&count.to_le_bytes());
            let mut rest = bytes.as_slice();
            assert_eq!(Card::decode(&mut rest), None, "count {count}");
            // The doc leaves `rest` unknown after `None`, but only its length shows
            // that decode took no address.
            assert_eq!(rest.len(), bytes.len() - 83, "count {count}");
        }
    }

    #[test]
    fn a_card_at_the_edges_of_name_and_ip_round_trips() {
        let names = [
            "a".to_string(),
            "a".repeat(255),
            format!("@{}", "b".repeat(254)),
            vec!["c"; 128].join("."),
        ];
        for name in names {
            let card = Card {
                name: name.parse().unwrap(),
                ..fixed()
            };
            assert_eq!(decoded(&encoded(&card)), Some(card), "{name}");
        }
        let ips = Card {
            addresses: Addresses::new(
                [
                    "255.255.255.255:65535",
                    "[::ffff:1.2.3.4]:1",
                    "[::]:0",
                    "[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:1",
                ]
                .into_iter()
                .map(|at| Address::Udp(at.parse().unwrap()))
                .collect(),
            )
            .unwrap(),
            ..fixed()
        };
        assert_eq!(decoded(&encoded(&ips)), Some(ips), "edge IPs");
    }
}
