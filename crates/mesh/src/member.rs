//! The region's record of one node.

use std::collections::BTreeMap;

use types::channel;
use types::name::Name;
use types::node::PublicKey;
use types::time::Span;

use crate::bytes::{
    ABSENT, PRESENT, put_count, put_key, put_name, take, take_count, take_key,
    take_name, take_present,
};
use crate::card;

/// The region's record of one node. Its key is `card.key()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// What the node states about itself.
    pub card: card::Signed,
    /// The join ticket's signature over the node's first card.
    pub admission: [u8; 64],
    /// For an ephemeral node, the time offline after which the region removes it.
    pub ephemeral: Option<Span>,
    /// The node's status channel keys, by name under the node's name: `clock.offset`
    /// is `<card.name>.clock.offset` (X27). A status name keeps its meaning and data
    /// type in every release; a change takes a new name.
    pub status: BTreeMap<Name, channel::Key>,
}

impl Member {
    /// The key that the node's peer proves and that signs the node's grants.
    pub(crate) const fn public_key(&self) -> PublicKey {
        self.card.card().public_key
    }

    /// Adds the one byte form of the record to `out`: the node key, the card, its
    /// signature, the admission, a presence byte and then the expiry in nanoseconds,
    /// a count of status entries, and each entry in name order: the name behind a
    /// length byte, then the channel key. Every number is 8 or 16 little-endian bytes.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the `Join` change of #336 is the first user")
    )]
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_key(self.card.key(), out);
        self.card.card().encode(out);
        out.extend(self.card.signature());
        out.extend(self.admission);
        match self.expiry {
            None => out.push(ABSENT),
            Some(expiry) => {
                out.push(PRESENT);
                out.extend(expiry.nanos().to_le_bytes());
            }
        }
        put_count(self.status.len(), out);
        for (name, key) in &self.status {
            put_name(name, out);
            out.extend(key.as_u128().to_le_bytes());
        }
    }

    /// Takes one record from the start of `bytes`. `None` when the bytes do not start
    /// with what [`Member::encode`] gives, or when the card's signature does not hold;
    /// `bytes` is then at no known place.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the `Join` change of #336 is the first user")
    )]
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        let key = take_key(bytes)?;
        let card = card::Card::decode(bytes)?;
        let card = card::Signed::check(key, card, take(bytes)?).ok()?;
        let admission = take(bytes)?;
        let expiry = if take_present(bytes)? {
            Some(Span::from_nanos(i64::from_le_bytes(take(bytes)?)))
        } else {
            None
        };
        let mut status = BTreeMap::new();
        for _ in 0..take_count(bytes)? {
            let name = take_name(bytes)?;
            if status
                .last_key_value()
                .is_some_and(|(last, _)| *last >= name)
            {
                return None;
            }
            let key = channel::Key::from_u128(u128::from_le_bytes(take(bytes)?));
            status.insert(name, key);
        }
        Some(Self {
            card,
            admission,
            expiry,
            status,
        })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::common::{key, member};

    fn status() -> impl Strategy<Value = BTreeMap<Name, channel::Key>> {
        let name = "[a-z]{1,6}(\\.[a-z]{1,6})?".prop_map(|name| name.parse().unwrap());
        let key = any::<u128>().prop_map(channel::Key::from_u128);
        prop::collection::btree_map(name, key, 0..6)
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
            .prop_map(|(id, admission, expiry, status)| Member {
                admission,
                expiry: expiry.map(Span::from_nanos),
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
            expiry: Some(Span::from_nanos(-2)),
            status,
            ..member(3)
        }
    }

    // The node key and the card's byte form: the bytes before the signature.
    fn card_bytes(member: &Member) -> Vec<u8> {
        let mut bytes = member.card.key().as_u128().to_le_bytes().to_vec();
        member.card.card().encode(&mut bytes);
        bytes
    }

    // The bytes before the expiry's presence byte.
    fn head(member: &Member) -> Vec<u8> {
        let mut bytes = card_bytes(member);
        bytes.extend(member.card.signature());
        bytes.extend(member.admission);
        bytes
    }

    // The bytes of a member with no expiry whose status entries are `names`, in that
    // order.
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
    fn a_member_with_no_expiry_has_an_absent_byte() {
        let member = Member {
            expiry: None,
            status: BTreeMap::new(),
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
    fn decode_refuses_a_presence_byte_that_is_neither_value() {
        let mut bytes = entries(&[]);
        let at = head(&with_status(&[])).len();
        assert_eq!(bytes[at], 0);
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
