//! The region's record of one node.

use types::ed25519::PublicKey;
use types::time::Span;

use crate::bytes::{put_optional_span, take, take_optional_span};
use crate::card;
use crate::status::Status;

/// The region's record of one node. Its key is `card.key()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// What the node states about itself.
    pub card: card::Signed,
    /// The join ticket's signature over the node's first card. A founding member, which
    /// no ticket admitted, has 64 zero bytes.
    pub admission: [u8; 64],
    /// For an ephemeral node, the time offline after which the region removes it.
    pub ephemeral: Option<Span>,
    /// The node's status channel keys, by name under the node's name: `clock.offset`
    /// is `<card.name>.clock.offset` (X27). A status name keeps its meaning and data
    /// type in every release; a change takes a new name.
    pub status: Status,
}

impl Member {
    /// The key that the node's peer proves and that signs the node's claims.
    pub(crate) const fn public_key(&self) -> PublicKey {
        self.card.card().public_key
    }

    /// Adds the one byte form of the record to `out`: the signed card, the admission,
    /// a presence byte and then `ephemeral` as 8 little-endian bytes, signed, a count
    /// of status entries as 8 little-endian bytes, and each entry in the byte order of
    /// its name: the name behind a length byte, then the channel key.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        self.card.encode(out);
        out.extend(self.admission);
        put_optional_span(self.ephemeral, out);
        self.status.encode(out);
    }

    /// Takes one record from the start of `bytes`. `None` when the bytes do not start
    /// with what [`Member::encode`] gives, or when the card's signature does not hold;
    /// `bytes` is then at no known place.
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        let card = card::Signed::decode(bytes)?;
        let admission = take(bytes)?;
        let ephemeral = take_optional_span(bytes)?;
        let status = Status::decode(bytes)?;
        Some(Self {
            card,
            admission,
            ephemeral,
            status,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use types::channel;

    use super::*;
    use crate::bytes::{put_count, put_name};
    use crate::common::{key, member};

    fn status() -> impl Strategy<Value = Status> {
        let name = "[a-z]{1,6}(\\.[a-z]{1,6})?".prop_map(|name| name.parse().unwrap());
        let key = any::<u128>().prop_map(channel::Key::from_u128);
        prop::collection::btree_map(name, key, 0..6)
            .prop_map(|map| Status::new(map).unwrap())
    }

    fn admission() -> impl Strategy<Value = [u8; 64]> {
        (any::<[u8; 32]>(), any::<[u8; 32]>()).prop_map(|(head, tail)| {
            let mut admission = [0; 64];
            admission[..32].copy_from_slice(&head);
            admission[32..].copy_from_slice(&tail);
            admission
        })
    }

    fn records() -> impl Strategy<Value = Member> {
        (
            1..=u8::MAX,
            admission(),
            prop::option::of(any::<i64>()),
            status(),
        )
            .prop_map(|(id, admission, ephemeral, status)| Member {
                admission,
                ephemeral: ephemeral.map(Span::from_nanos),
                status,
                ..member(id)
            })
    }

    fn encoded(member: &Member) -> Vec<u8> {
        let mut bytes = Vec::new();
        member.encode(&mut bytes);
        bytes
    }

    fn decoded(bytes: &[u8]) -> Option<Member> {
        let mut rest = bytes;
        let member = Member::decode(&mut rest)?;
        rest.is_empty().then_some(member)
    }

    fn with_status(names: &[&str]) -> Member {
        let status = names
            .iter()
            .zip(1..)
            .map(|(name, key)| (name.parse().unwrap(), channel::Key::from_u128(key)))
            .collect();
        Member {
            admission: [5; 64],
            ephemeral: Some(Span::from_nanos(-2)),
            status: Status::new(status).unwrap(),
            ..member(3)
        }
    }

    // The node key and the card's byte form: the bytes before the signature.
    fn card_bytes(member: &Member) -> Vec<u8> {
        let mut bytes = member.card.key().as_u128().to_le_bytes().to_vec();
        member.card.card().encode(&mut bytes);
        bytes
    }

    // The bytes before the presence byte of `ephemeral`.
    fn head(member: &Member) -> Vec<u8> {
        let mut bytes = card_bytes(member);
        bytes.extend(member.card.signature());
        bytes.extend(member.admission);
        bytes
    }

    // The bytes of a member that is not ephemeral, whose status entries are `names`, in
    // that order.
    fn entries(names: &[&str]) -> Vec<u8> {
        let mut bytes = head(&with_status(&[]));
        bytes.push(0);
        put_count(names.len(), &mut bytes);
        for name in names {
            put_name(&name.parse().unwrap(), &mut bytes);
            bytes.extend(1_u128.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn a_member_has_a_fixed_byte_form() {
        let member = with_status(&["clock.offset", "disk"]);
        let mut expected = card_bytes(&member);
        assert_eq!(expected[..16], key(3).as_u128().to_le_bytes());
        expected.extend(member.card.signature());
        expected.extend([5; 64]);
        expected.push(1);
        expected.extend((-2_i64).to_le_bytes());
        expected.extend(2_u64.to_le_bytes());
        expected.push(12);
        expected.extend(b"clock.offset");
        expected.extend(1_u128.to_le_bytes());
        expected.push(4);
        expected.extend(b"disk");
        expected.extend(2_u128.to_le_bytes());
        assert_eq!(encoded(&member), expected);
    }

    #[test]
    fn a_member_that_is_not_ephemeral_has_an_absent_byte() {
        let member = Member {
            ephemeral: None,
            status: Status::new(BTreeMap::new()).unwrap(),
            ..with_status(&[])
        };
        assert_eq!(encoded(&member), entries(&[]));
        assert_eq!(decoded(&entries(&[])), Some(member));
    }

    #[test]
    fn decode_refuses_status_names_out_of_order() {
        assert!(decoded(&entries(&["clock.offset", "disk"])).is_some());
        assert_eq!(decoded(&entries(&["disk", "clock.offset"])), None);
        assert_eq!(decoded(&entries(&["disk", "disk"])), None);
    }

    #[test]
    fn decode_refuses_more_than_64_status_entries() {
        let names: Vec<String> = (0..65).map(|i| format!("s{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(
            decoded(&entries(&names[..64])).map(|m| m.status.as_map().len()),
            Some(64)
        );
        assert_eq!(decoded(&entries(&names)), None);
    }

    #[test]
    fn decode_refuses_a_presence_byte_that_is_neither_value() {
        let mut bytes = entries(&[]);
        let at = head(&with_status(&[])).len();
        assert_eq!(bytes[at], 0);
        bytes[at] = 2;
        assert_eq!(decoded(&bytes), None);
        let mut bytes = encoded(&with_status(&[]));
        assert_eq!(bytes[at], 1);
        bytes[at] = 2;
        assert_eq!(decoded(&bytes), None);
    }

    #[test]
    fn decode_refuses_a_card_whose_signature_does_not_hold() {
        let member = with_status(&[]);
        let mut bytes = encoded(&member);
        let signature = card_bytes(&member).len();
        bytes[signature] ^= 1;
        assert_eq!(decoded(&bytes), None);
        bytes[signature] ^= 1;
        assert_eq!(decoded(&bytes), Some(member));
        bytes[0] ^= 1;
        assert_eq!(decoded(&bytes), None);
    }

    proptest! {
        // Each case checks one card signature for each byte, so it is slow.
        #![proptest_config(ProptestConfig::with_cases(32))]

        // A decode that takes two forms of one value fails here.
        #[test]
        fn a_changed_byte_that_decodes_encodes_back(
            member in records(),
            flip in 1..=u8::MAX,
        ) {
            let bytes = encoded(&member);
            for at in 0..bytes.len() {
                let mut bytes = bytes.clone();
                bytes[at] ^= flip;
                if let Some(changed) = decoded(&bytes) {
                    prop_assert_eq!(encoded(&changed), bytes);
                }
            }
        }
    }

    proptest! {
        #[test]
        fn a_member_round_trips(member in records()) {
            prop_assert_eq!(decoded(&encoded(&member)), Some(member));
        }

        #[test]
        fn decode_takes_no_byte_after_the_member(
            member in records(),
            tail in prop::collection::vec(any::<u8>(), 0..8),
        ) {
            let mut bytes = encoded(&member);
            let end = bytes.len();
            bytes.extend(&tail);
            let mut rest = &bytes[..];
            prop_assert_eq!(Member::decode(&mut rest), Some(member));
            prop_assert_eq!(rest, &bytes[end..]);
        }

        #[test]
        fn decode_refuses_each_shorter_prefix(member in records()) {
            let bytes = encoded(&member);
            for end in 0..bytes.len() {
                prop_assert_eq!(decoded(&bytes[..end]), None);
            }
        }
    }
}
