//! The change records that move the region state, each with one byte form.

use std::fmt;

use types::channel;
use types::node::{self, PublicKey};
use types::time::Stamp;

use crate::bytes::{
    put_channel, put_key, put_public_key, put_stamp, take, take_channel, take_key,
    take_public_key, take_stamp,
};
use crate::card;
use crate::status::Status;
use crate::ticket::Options;

/// A change record: the data of one log entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// Makes `home` the home of `index`.
    Home {
        /// The index channel.
        index: channel::Key,
        /// The node that stores it.
        home: node::Key,
    },
    /// Admits a node with a ticket.
    Join(Box<Join>),
    /// Records a ticket.
    Ticket {
        /// The public half of the ticket's key pair.
        public_key: PublicKey,
        /// What the ticket admits.
        options: Options,
    },
}

/// A change that admits a node. Every node checks it at apply, so its card is not yet
/// checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Join {
    /// The public key of the ticket that admits the node.
    pub(crate) ticket: PublicKey,
    /// The mesh time of the join: the later edge of the stamping voter's mesh time.
    pub(crate) at: Stamp,
    /// The node's first card, signed by the node.
    pub(crate) card: card::Unchecked,
    /// The ticket's signature over the card.
    pub(crate) admission: [u8; 64],
    /// The node's status channel keys, by name under the node's name.
    pub(crate) status: Status,
}

const HOME: u8 = 1;
const JOIN: u8 = 2;
const TICKET: u8 = 3;

impl Change {
    /// Adds the one byte form of the change to `out`: a kind byte, then the body of
    /// that kind. Every number is little endian.
    ///
    /// - Home: the index, then the home.
    /// - Join: the ticket's public key, the mesh time in nanoseconds (8 bytes), the
    ///   node key, the card, its signature, the admission, and the status entries as
    ///   in [`Member::encode`](crate::Member::encode).
    /// - Ticket: the public key, the prefix behind a length byte, the reusable byte (0
    ///   or 1), the expiry in nanoseconds (8 bytes), and the ephemeral span behind a
    ///   presence byte.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Home { index, home } => {
                out.push(HOME);
                put_channel(*index, out);
                put_key(*home, out);
            }
            Self::Join(join) => {
                out.push(JOIN);
                put_public_key(join.ticket, out);
                put_stamp(join.at, out);
                join.card.encode(out);
                out.extend(join.admission);
                join.status.encode(out);
            }
            Self::Ticket {
                public_key,
                options,
            } => {
                out.push(TICKET);
                put_public_key(*public_key, out);
                options.encode(out);
            }
        }
    }

    /// Decodes the bytes that [`Change::encode`] gives, and no others.
    ///
    /// # Errors
    ///
    /// [`Malformed::Unknown`] when the bytes are empty or the kind byte is unknown, and
    /// [`Malformed::Body`] when the rest is not the byte form of a body of that kind.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Malformed> {
        let (&kind, mut rest) = bytes.split_first().ok_or(Unknown::Empty)?;
        let take_body = match kind {
            HOME => take_home,
            JOIN => take_join,
            TICKET => take_ticket,
            _ => return Err(Unknown::Kind { kind }.into()),
        };
        take_body(&mut rest)
            .filter(|_| rest.is_empty())
            .ok_or(Malformed::Body {
                kind,
                length: bytes.len(),
            })
    }
}

fn take_home(bytes: &mut &[u8]) -> Option<Change> {
    let index = take_channel(bytes)?;
    let home = take_key(bytes)?;
    Some(Change::Home { index, home })
}

fn take_join(bytes: &mut &[u8]) -> Option<Change> {
    Some(Change::Join(Box::new(Join {
        ticket: take_public_key(bytes)?,
        at: take_stamp(bytes)?,
        card: card::Unchecked::decode(bytes)?,
        admission: take(bytes)?,
        status: Status::decode(bytes)?,
    })))
}

fn take_ticket(bytes: &mut &[u8]) -> Option<Change> {
    Some(Change::Ticket {
        public_key: take_public_key(bytes)?,
        options: Options::decode(bytes)?,
    })
}

/// Bytes that are not a change record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Malformed {
    /// The bytes are not a change of a kind that this build knows.
    Unknown(Unknown),
    /// The rest of the bytes is not the body of a change of that kind.
    Body {
        /// The kind byte.
        kind: u8,
        /// The length of the bytes.
        length: usize,
    },
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(unknown) => unknown.fmt(f),
            Self::Body { kind, length } => write!(
                f,
                "a change of kind {kind} and {length} bytes \
                 is not in the byte form of its kind"
            ),
        }
    }
}

impl std::error::Error for Malformed {}

impl From<Unknown> for Malformed {
    fn from(unknown: Unknown) -> Self {
        Self::Unknown(unknown)
    }
}

/// Bytes that are not a change of a kind that this build knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unknown {
    /// The bytes are empty.
    Empty,
    /// The kind byte names no change.
    Kind {
        /// The kind byte.
        kind: u8,
    },
}

impl fmt::Display for Unknown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a change of 0 bytes has no kind"),
            Self::Kind { kind } => write!(f, "change kind {kind} is unknown"),
        }
    }
}

impl std::error::Error for Unknown {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use types::time::Span;

    use super::*;
    use crate::common::{
        home, index, join, name, options, public, record, status_bytes, with_status,
    };

    fn encoded(change: &Change) -> Vec<u8> {
        let mut out = Vec::new();
        change.encode(&mut out);
        out
    }

    #[test]
    fn a_home_change_has_a_fixed_byte_form() {
        let mut expected = vec![1, 0x02, 0x01];
        expected.extend([0; 14]);
        expected.extend([0x0b, 0x0a]);
        expected.extend([0; 14]);
        assert_eq!(encoded(&home(0x0102, 0x0a0b)), expected);
    }

    #[test]
    fn a_join_change_has_a_fixed_byte_form() {
        let join = join(7, 3, "plant.edge.a");
        let mut expected = vec![2];
        expected.extend(public(7).to_bytes());
        expected.extend(999_i64.to_le_bytes());
        expected.extend(3_u128.to_le_bytes());
        join.card.card.encode(&mut expected);
        expected.extend(join.card.signature);
        expected.extend(join.admission);
        expected.extend(1_u64.to_le_bytes());
        expected.push(4);
        expected.extend(b"disk");
        expected.extend(9_u128.to_le_bytes());
        assert_eq!(encoded(&Change::Join(Box::new(join))), expected);
    }

    #[test]
    fn a_ticket_change_has_a_fixed_byte_form() {
        let mut expected = vec![3];
        expected.extend(public(7).to_bytes());
        expected.push(10);
        expected.extend(b"plant.edge");
        expected.push(1);
        expected.extend(1_000_i64.to_le_bytes());
        expected.push(1);
        expected.extend(60_i64.to_le_bytes());
        assert_eq!(encoded(&record(7, options("plant.edge", true))), expected);
        let mut options = options("plant.edge", false);
        options.ephemeral = None;
        let bytes = encoded(&record(7, options));
        assert_eq!(bytes[44..], [0, 0xe8, 0x03, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn decode_refuses_empty_bytes() {
        let error = Change::decode(&[]).unwrap_err();
        assert_eq!(error, Malformed::Unknown(Unknown::Empty));
        assert_eq!(error.to_string(), "a change of 0 bytes has no kind");
    }

    #[test]
    fn decode_refuses_an_unknown_kind() {
        let mut bytes = encoded(&home(1, 2));
        for kind in [0, 4] {
            bytes[0] = kind;
            let error = Change::decode(&bytes).unwrap_err();
            assert_eq!(error, Malformed::Unknown(Unknown::Kind { kind }));
            assert_eq!(error.to_string(), format!("change kind {kind} is unknown"));
        }
    }

    #[test]
    fn decode_refuses_a_body_of_another_length() {
        let bytes = encoded(&home(1, 2));
        for length in [1, 17, 32, 34] {
            let mut cut = bytes.clone();
            cut.resize(length, 0);
            let error = Change::decode(&cut).unwrap_err();
            assert_eq!(error, Malformed::Body { kind: 1, length });
            assert_eq!(
                error.to_string(),
                format!(
                    "a change of kind 1 and {length} bytes is not in the byte form of \
                     its kind"
                )
            );
        }
    }

    #[test]
    fn decode_refuses_a_reusable_byte_that_is_neither_value() {
        let mut bytes = encoded(&record(7, options("plant.edge", false)));
        assert_eq!(bytes[44], 0);
        bytes[44] = 2;
        let length = bytes.len();
        let error = Change::decode(&bytes).unwrap_err();
        assert_eq!(error, Malformed::Body { kind: 3, length });
    }

    #[test]
    fn a_join_with_more_than_64_status_entries_does_not_decode() {
        let none = with_status(join(7, 3, "plant.edge.a"), &[]);
        let mut bytes = encoded(&Change::Join(Box::new(none)));
        let empty = bytes.len() - 8;
        bytes.truncate(empty);
        bytes.extend(status_bytes(64));
        let most = Change::decode(&bytes);
        let entries = most.map(|change| match change {
            Change::Join(join) => join.status.as_map().len(),
            _ => unreachable!(),
        });
        assert_eq!(entries, Ok(64));
        bytes.truncate(empty);
        bytes.extend(status_bytes(65));
        let length = bytes.len();
        assert_eq!(
            Change::decode(&bytes),
            Err(Malformed::Body { kind: 2, length })
        );
    }

    // Apply checks the signatures, so decode keeps a forged card.
    #[test]
    fn decode_keeps_a_join_whose_signatures_do_not_hold() {
        let mut join = join(7, 3, "plant.edge.a");
        join.card.signature[0] ^= 1;
        join.admission[0] ^= 1;
        let change = Change::Join(Box::new(join));
        assert_eq!(Change::decode(&encoded(&change)), Ok(change));
    }

    fn statuses() -> impl Strategy<Value = Status> {
        let name = "[a-z]{1,6}(\\.[a-z]{1,6})?".prop_map(|text| name(&text));
        prop::collection::btree_map(name, any::<u128>().prop_map(index), 0..4)
            .prop_map(|map| Status::new(map).unwrap())
    }

    fn changes() -> impl Strategy<Value = Change> {
        let homes = (any::<u128>(), any::<u128>()).prop_map(|(i, h)| home(i, h));
        let joins = (any::<u8>(), any::<u8>(), any::<i64>(), statuses()).prop_map(
            |(ticket_id, id, at, status)| {
                Change::Join(Box::new(Join {
                    at: Stamp::from_nanos(at),
                    status,
                    ..join(ticket_id, id, "plant.edge.a")
                }))
            },
        );
        let tickets = (
            any::<u8>(),
            "[a-z]{1,6}(\\.[a-z]{1,6})?",
            any::<bool>(),
            any::<i64>(),
            prop::option::of(any::<i64>()),
        )
            .prop_map(|(id, prefix, reusable, expiry, ephemeral)| {
                record(
                    id,
                    Options {
                        prefix: name(&prefix),
                        reusable,
                        expiry: Stamp::from_nanos(expiry),
                        ephemeral: ephemeral.map(Span::from_nanos),
                    },
                )
            });
        prop_oneof![homes, joins, tickets]
    }

    proptest! {
        #[test]
        fn a_change_round_trips(change in changes()) {
            prop_assert_eq!(Change::decode(&encoded(&change)), Ok(change));
        }

        #[test]
        fn decode_refuses_each_shorter_prefix_and_a_longer_form(
            change in changes(),
            extra in any::<u8>(),
        ) {
            let mut bytes = encoded(&change);
            for length in 1..bytes.len() {
                let error = Change::decode(&bytes[..length]).unwrap_err();
                prop_assert_eq!(error, Malformed::Body { kind: bytes[0], length });
            }
            bytes.push(extra);
            let length = bytes.len();
            let error = Change::decode(&bytes).unwrap_err();
            prop_assert_eq!(error, Malformed::Body { kind: bytes[0], length });
        }

        // Every byte form that decodes is the one that its change encodes to.
        #[test]
        fn a_change_has_one_byte_form(
            bytes in prop::collection::vec(any::<u8>(), 0..40),
        ) {
            if let Ok(change) = Change::decode(&bytes) {
                prop_assert_eq!(encoded(&change), bytes);
            }
        }
    }
}
