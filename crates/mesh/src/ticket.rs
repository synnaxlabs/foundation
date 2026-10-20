//! Join tickets: the Ed25519 key pairs that admit nodes to a region.

use std::fmt;
use std::str::FromStr;

use base64ct::{Base64UrlUnpadded, Encoding as _};
use types::ed25519::{BadSignature, Pair, PrivateKey, PublicKey};
use types::name::{Name, Prefix};
use types::node;
use types::time::{Span, Stamp};

use crate::bytes::{
    put_bool, put_count, put_key, put_name, put_optional_span, put_prefix,
    put_public_key, put_stamp, take, take_bool, take_count, take_key, take_name,
    take_optional_span, take_prefix, take_public_key, take_stamp,
};
use crate::card;

const TAG: &[u8] = b"foundation/admission/1";

const TEXT: &str = "foundation-ticket-1-";

/// What a join ticket admits. The region records it with the ticket's public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The name of each node that the ticket admits is under this prefix.
    pub prefix: Name,
    /// The ticket admits any number of nodes. Else it admits one.
    pub reusable: bool,
    /// The mesh time from which the ticket admits no node.
    pub expiry: Stamp,
    /// For an ephemeral node, the time offline after which the region removes it. The
    /// `Member` that the ticket admits takes this value.
    pub ephemeral: Option<Span>,
}

impl Options {
    /// Adds the one byte form of the options to `out`: the prefix behind a length byte,
    /// the reusable byte (0 or 1), the expiry in nanoseconds as 8 little-endian bytes,
    /// and the ephemeral span behind a presence byte.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_name(&self.prefix, out);
        put_bool(self.reusable, out);
        put_stamp(self.expiry, out);
        put_optional_span(self.ephemeral, out);
    }

    /// Takes what [`Options::encode`] gives from the start of `bytes`. `None` when the
    /// bytes do not start with that form; `bytes` is then at no known place.
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        Some(Self {
            prefix: take_name(bytes)?,
            reusable: take_bool(bytes)?,
            expiry: take_stamp(bytes)?,
            ephemeral: take_optional_span(bytes)?,
        })
    }
}

/// A voter that a joining node dials first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Voter {
    /// The voter's node key.
    pub key: node::Key,
    /// The key that `transport` pins for the voter.
    pub public_key: PublicKey,
    /// Where to dial the voter.
    pub addresses: card::addresses::Addresses,
}

/// The secret part of a join ticket, which an operator carries to the joining node. It
/// is never in region state, and Foundation never writes it to a file. Its `Debug`
/// writes the region and the public key only, and it has no `Display`, no `Clone`, and
/// no equality.
pub struct Ticket {
    private_key: PrivateKey,
    region: Prefix,
    voters: Vec<Voter>,
}

impl Ticket {
    /// A ticket for the region with prefix `region`, whose key pair is `private_key`,
    /// and whose joining node dials `voters` first.
    ///
    /// # Panics
    ///
    /// When `voters` is empty: a region always has a voter.
    #[must_use]
    pub fn new(private_key: PrivateKey, region: Prefix, voters: Vec<Voter>) -> Self {
        assert!(!voters.is_empty(), "a ticket names at least one voter");
        Self {
            private_key,
            region,
            voters,
        }
    }

    /// The public half of the key pair: the key of the ticket's record.
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.private_key.public()
    }

    /// The prefix of the ticket's region.
    #[must_use]
    pub const fn region(&self) -> &Prefix {
        &self.region
    }

    /// The voters that the joining node dials first.
    #[must_use]
    pub fn voters(&self) -> &[Voter] {
        &self.voters
    }

    /// The admission of `card`: the ticket's signature over `foundation/admission/1`,
    /// the card's node key, and the card's byte form.
    #[must_use]
    pub fn admission(&self, card: &card::Signed) -> [u8; 64] {
        Pair::new(&self.private_key).sign(&statement(card))
    }

    /// The text that an operator carries: `foundation-ticket-1-` and the URL-safe
    /// Base64 of the byte form, with no padding. The byte form is the private key, the
    /// region prefix behind a length byte, and each voter as its node key, its public
    /// key, and its addresses, behind a count of 8 little-endian bytes. The text is a
    /// secret: the private key is in it.
    #[must_use]
    pub fn text(&self) -> String {
        let mut bytes = self.private_key.0.to_vec();
        put_prefix(&self.region, &mut bytes);
        put_count(self.voters.len(), &mut bytes);
        for voter in &self.voters {
            put_key(voter.key, &mut bytes);
            put_public_key(voter.public_key, &mut bytes);
            voter.addresses.encode(&mut bytes);
        }
        format!("{TEXT}{}", Base64UrlUnpadded::encode_string(&bytes))
    }
}

impl FromStr for Ticket {
    type Err = Unreadable;

    /// The ticket of `text`, which [`Ticket::text`] gives, with any ASCII whitespace
    /// at its start and its end, such as the line end of a file.
    ///
    /// # Errors
    ///
    /// [`Unreadable`] when `text` is not that form, also for a count of 0 voters.
    fn from_str(text: &str) -> Result<Self, Unreadable> {
        let text = text.trim_ascii().strip_prefix(TEXT).ok_or(Unreadable)?;
        let bytes = Base64UrlUnpadded::decode_vec(text).ok().ok_or(Unreadable)?;
        let mut bytes = bytes.as_slice();
        let private_key = PrivateKey(take(&mut bytes).ok_or(Unreadable)?);
        let region = take_prefix(&mut bytes).ok_or(Unreadable)?;
        let count = take_count(&mut bytes).ok_or(Unreadable)?;
        let mut voters = Vec::new();
        for _ in 0..count {
            voters.push(take_voter(&mut bytes).ok_or(Unreadable)?);
        }
        if voters.is_empty() || !bytes.is_empty() {
            return Err(Unreadable);
        }
        Ok(Self::new(private_key, region, voters))
    }
}

// Takes one voter of the byte form of [`Ticket::text`].
fn take_voter(bytes: &mut &[u8]) -> Option<Voter> {
    Some(Voter {
        key: take_key(bytes)?,
        public_key: take_public_key(bytes)?,
        addresses: card::addresses::Addresses::decode(bytes)?,
    })
}

/// A text that is no ticket. It names no part of the text: the text is a secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unreadable;

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the text is not a join ticket")
    }
}

impl std::error::Error for Unreadable {}

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ticket")
            .field("region", &self.region)
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// The region's record of a ticket. Its key is `public_key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    public_key: PublicKey,
    options: Options,
    uses: u64,
}

impl Record {
    /// The record of a new ticket with `public_key` and `options`.
    pub(crate) const fn new(public_key: PublicKey, options: Options) -> Self {
        Self {
            public_key,
            options,
            uses: 0,
        }
    }

    /// For a member that the ticket admits, the time offline after which the region
    /// removes it, or `None` when it is not ephemeral.
    pub(crate) const fn ephemeral(&self) -> Option<Span> {
        self.options.ephemeral
    }

    /// How many nodes the ticket admitted.
    #[cfg(test)]
    pub(crate) const fn uses(&self) -> u64 {
        self.uses
    }

    /// Counts one node that the ticket admitted, after [`Record::check`] and
    /// [`Record::unused`] pass. The count saturates at [`u64::MAX`].
    pub(crate) const fn count(&mut self) {
        self.uses = self.uses.saturating_add(1);
    }

    /// Checks that the ticket admits `card` with `admission` at mesh time `at`.
    ///
    /// # Errors
    ///
    /// The first refusal that holds, in this order: [`Refused::Forged`],
    /// [`Refused::Scope`], and [`Refused::Expired`].
    pub(crate) fn check(
        &self,
        card: &card::Signed,
        admission: &[u8; 64],
        at: Stamp,
    ) -> Result<(), Refused> {
        let public_key = self.public_key;
        public_key.verify(&statement(card), admission).map_err(
            |_bad: BadSignature| Refused::Forged {
                node: card.key(),
                public_key,
            },
        )?;
        let name = &card.card().name;
        if !name.starts_with(&self.options.prefix) {
            return Err(Refused::Scope {
                name: name.clone(),
                prefix: self.options.prefix.clone(),
            });
        }
        if at >= self.options.expiry {
            return Err(Refused::Expired {
                public_key,
                expiry: self.options.expiry,
                at,
            });
        }
        Ok(())
    }

    /// Checks that the ticket admits one more node.
    ///
    /// # Errors
    ///
    /// [`Refused::Used`] when the ticket is single use and admitted a node.
    pub(crate) fn unused(&self) -> Result<(), Refused> {
        if !self.options.reusable && self.uses > 0 {
            return Err(Refused::Used {
                public_key: self.public_key,
            });
        }
        Ok(())
    }
}

/// Why a ticket does not admit a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The admission is not the ticket's signature over the node's card.
    Forged {
        /// The node of the card.
        node: node::Key,
        /// The ticket's public key.
        public_key: PublicKey,
    },
    /// The node's name is not under the ticket's prefix.
    Scope {
        /// The node's name.
        name: Name,
        /// The ticket's prefix.
        prefix: Name,
    },
    /// The join is at or after the ticket's expiry.
    Expired {
        /// The ticket's public key.
        public_key: PublicKey,
        /// The ticket's expiry.
        expiry: Stamp,
        /// The mesh time of the join.
        at: Stamp,
    },
    /// A single-use ticket already admitted a node.
    Used {
        /// The ticket's public key.
        public_key: PublicKey,
    },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forged { node, public_key } => write!(
                f,
                "the admission of node {node} does not hold for ticket {public_key}"
            ),
            Self::Scope { name, prefix } => {
                write!(
                    f,
                    "the name {name} is not under the ticket's prefix {prefix}"
                )
            }
            Self::Expired {
                public_key,
                expiry,
                at,
            } => write!(
                f,
                "ticket {public_key} expired at {expiry}, and the join is at {at}"
            ),
            Self::Used { public_key } => {
                write!(
                    f,
                    "ticket {public_key} admits one node, and it admitted one"
                )
            }
        }
    }
}

impl std::error::Error for Refused {}

// The bytes a ticket signs to admit `card`.
fn statement(card: &card::Signed) -> Vec<u8> {
    card::statement(TAG, card.key(), card.card())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use proptest::prelude::*;
    use transport::Address;

    use super::*;
    use crate::common::{
        BEFORE_EXPIRY, EXPIRY, key, options, private, public, signed, ticket, voter,
    };

    // The text of `bytes` with the first part of a ticket.
    fn text(bytes: &[u8]) -> String {
        format!(
            "foundation-ticket-1-{}",
            Base64UrlUnpadded::encode_string(bytes)
        )
    }

    // The byte form of `ticket(7)`, written out.
    fn bytes_of_ticket_7() -> Vec<u8> {
        let mut bytes = vec![7; 32];
        bytes.extend(b"\x05plant");
        bytes.extend(1u64.to_le_bytes());
        bytes.extend(key(1).as_u128().to_le_bytes());
        bytes.extend(public(1).to_bytes());
        bytes.extend(1u64.to_le_bytes());
        bytes.extend([0, 4, 10, 0, 0, 1]);
        bytes.extend(4000u16.to_le_bytes());
        bytes
    }

    fn voters() -> impl Strategy<Value = Vec<Voter>> {
        let address = (any::<bool>(), any::<[u8; 4]>(), any::<u16>()).prop_map(
            |(tcp, ip, port)| {
                let at = SocketAddr::from((ip, port));
                if tcp {
                    Address::Tcp(at)
                } else {
                    Address::Udp(at)
                }
            },
        );
        let voter = (any::<u8>(), prop::collection::vec(address, 0..4)).prop_map(
            |(id, list)| Voter {
                key: key(id),
                public_key: public(id),
                addresses: card::addresses::Addresses::new(list).unwrap(),
            },
        );
        prop::collection::vec(voter, 1..5)
    }

    fn regions() -> impl Strategy<Value = Prefix> {
        prop_oneof![
            Just(Prefix::ROOT),
            Just("plant".parse().unwrap()),
            Just("plant.edge.a".parse().unwrap()),
        ]
    }

    #[test]
    fn the_text_is_the_first_part_and_the_base64_of_the_byte_form() {
        assert_eq!(ticket(7).text(), text(&bytes_of_ticket_7()));
    }

    #[test]
    fn the_text_of_the_root_region_has_an_empty_prefix() {
        let ticket = Ticket::new(private(7), Prefix::ROOT, vec![voter()]);
        let mut bytes = bytes_of_ticket_7();
        bytes.splice(32..38, [0]);
        assert_eq!(ticket.text(), text(&bytes));
        let decoded = Ticket::from_str(&ticket.text()).unwrap();
        assert_eq!(decoded.region(), &Prefix::ROOT);
    }

    #[test]
    fn each_cut_of_the_text_is_unreadable() {
        let text = ticket(7).text();
        for end in 0..text.len() {
            assert_eq!(
                Ticket::from_str(text.get(..end).unwrap()).err(),
                Some(Unreadable),
                "{end}"
            );
        }
    }

    #[test]
    fn whitespace_at_the_ends_of_the_text_is_no_part_of_it() {
        let text = ticket(7).text();
        for (start, end) in [("", "\n"), ("", "\r\n"), (" \t", " \n"), ("\n", "")] {
            let read = Ticket::from_str(&format!("{start}{text}{end}")).unwrap();
            assert_eq!(read.text(), text, "{start:?} {end:?}");
        }
        let (left, right) = text.split_at(40);
        let inside = format!("{left}\n{right}");
        let unicode = format!("{text}\u{a0}");
        for text in [inside, unicode] {
            assert_eq!(Ticket::from_str(&text).err(), Some(Unreadable), "{text:?}");
        }
    }

    #[test]
    fn a_byte_past_the_end_is_unreadable() {
        let mut bytes = bytes_of_ticket_7();
        bytes.push(0);
        assert_eq!(Ticket::from_str(&text(&bytes)).err(), Some(Unreadable));
    }

    #[test]
    fn a_ticket_with_no_voter_is_unreadable() {
        let mut bytes = vec![7; 32];
        bytes.extend(b"\x05plant");
        bytes.extend(0u64.to_le_bytes());
        assert_eq!(Ticket::from_str(&text(&bytes)).err(), Some(Unreadable));
    }

    #[test]
    fn a_wrong_first_part_is_unreadable() {
        let base64 = Base64UrlUnpadded::encode_string(&bytes_of_ticket_7());
        for first in [
            "",
            "foundation-ticket-",
            "foundation-ticket-2-",
            "Foundation-ticket-1-",
            "foundation_ticket_1_",
            "\u{a0}foundation-ticket-1-",
        ] {
            let text = format!("{first}{base64}");
            assert_eq!(Ticket::from_str(&text).err(), Some(Unreadable), "{first}");
        }
    }

    #[test]
    fn padded_or_standard_base64_is_unreadable() {
        let mut padded = text(&bytes_of_ticket_7());
        padded.push_str("==");
        let standard = format!(
            "foundation-ticket-1-{}",
            base64ct::Base64Unpadded::encode_string(&[0xfb; 33])
        );
        for text in [padded, standard] {
            assert_eq!(Ticket::from_str(&text).err(), Some(Unreadable), "{text}");
        }
    }

    #[test]
    fn a_voter_key_of_small_order_is_unreadable() {
        let mut bytes = bytes_of_ticket_7();
        bytes[62..94].copy_from_slice(&[0; 32]);
        assert_eq!(Ticket::from_str(&text(&bytes)).err(), Some(Unreadable));
    }

    #[test]
    fn unreadable_names_no_part_of_the_text() {
        assert_eq!(Unreadable.to_string(), "the text is not a join ticket");
    }

    fn record(id: u8, reusable: bool) -> Record {
        Record::new(ticket(id).public_key(), options("plant.edge", reusable))
    }

    #[test]
    fn the_public_key_is_the_half_of_the_private_key() {
        assert_eq!(ticket(7).public_key(), public(7));
    }

    #[test]
    fn a_ticket_keeps_its_region_and_voters() {
        let ticket = ticket(7);
        assert_eq!(ticket.region(), &"plant".parse().unwrap());
        assert_eq!(ticket.voters(), [voter()]);
    }

    #[test]
    fn a_ticket_for_the_root_region_keeps_the_root() {
        let ticket = Ticket::new(private(7), Prefix::ROOT, vec![voter()]);
        assert_eq!(ticket.region(), &Prefix::ROOT);
    }

    #[test]
    #[should_panic(expected = "a ticket names at least one voter")]
    fn a_ticket_with_no_voter_panics() {
        drop(Ticket::new(
            private(7),
            "plant".parse().unwrap(),
            Vec::new(),
        ));
    }

    #[test]
    fn debug_writes_the_region_and_the_public_key_only() {
        let region: Prefix = "plant".parse().unwrap();
        assert_eq!(
            format!("{:?}", ticket(7)),
            format!(
                "Ticket {{ region: {region:?}, public_key: {:?}, .. }}",
                public(7)
            )
        );
    }

    #[test]
    fn the_admission_signs_the_tag_the_node_key_and_the_card() {
        let card = signed(3, "plant.edge.a");
        let mut statement = b"foundation/admission/1".to_vec();
        statement.extend(key(3).as_u128().to_le_bytes());
        card.card().encode(&mut statement);
        let admission = ticket(7).admission(&card);
        assert_eq!(public(7).verify(&statement, &admission), Ok(()));
    }

    #[test]
    fn a_single_use_ticket_admits_one_node() {
        let mut record = record(7, false);
        let card = signed(3, "plant.edge.a");
        let admission = ticket(7).admission(&card);
        for _ in 0..2 {
            assert_eq!(record.check(&card, &admission, BEFORE_EXPIRY), Ok(()));
            assert_eq!(record.unused(), Ok(()));
        }
        record.count();
        assert_eq!(record.check(&card, &admission, BEFORE_EXPIRY), Ok(()));
        assert_eq!(
            record.unused(),
            Err(Refused::Used {
                public_key: public(7)
            })
        );
    }

    #[test]
    fn a_reusable_ticket_admits_each_node_after_any_use() {
        let mut record = record(7, true);
        record.uses = u64::MAX;
        let card = signed(3, "plant.edge.a");
        let admitted = record.check(&card, &ticket(7).admission(&card), BEFORE_EXPIRY);
        assert_eq!(admitted, Ok(()));
        assert_eq!(record.unused(), Ok(()));
    }

    /// `unused` reads the count only for a single-use ticket, which apply counts at
    /// most once, so no caller sees a wrap.
    #[test]
    fn the_use_count_saturates() {
        let mut record = record(7, true);
        record.count();
        assert_eq!(record.uses(), 1);
        record.uses = u64::MAX;
        record.count();
        assert_eq!(record.uses(), u64::MAX);
    }

    #[test]
    fn an_admission_of_another_ticket_is_forged() {
        let record = record(7, false);
        let card = signed(3, "plant.edge.a");
        assert_eq!(
            record.check(&card, &ticket(8).admission(&card), BEFORE_EXPIRY),
            Err(Refused::Forged {
                node: key(3),
                public_key: public(7)
            })
        );
    }

    #[test]
    fn an_admission_of_another_card_is_forged() {
        let record = record(7, false);
        let admission = ticket(7).admission(&signed(3, "plant.edge.a"));
        let other = signed(4, "plant.edge.a");
        assert_eq!(
            record.check(&other, &admission, BEFORE_EXPIRY),
            Err(Refused::Forged {
                node: key(4),
                public_key: public(7)
            })
        );
    }

    #[test]
    fn a_name_outside_the_prefix_is_out_of_scope() {
        let record = record(7, false);
        let card = signed(3, "plant.edger");
        assert_eq!(
            record.check(&card, &ticket(7).admission(&card), BEFORE_EXPIRY),
            Err(Refused::Scope {
                name: "plant.edger".parse().unwrap(),
                prefix: "plant.edge".parse().unwrap()
            })
        );
    }

    #[test]
    fn a_refusal_names_the_first_check_that_fails() {
        let record = record(7, false);
        let outside = signed(3, "plant.edger");
        assert_eq!(
            record.check(&outside, &ticket(8).admission(&outside), EXPIRY),
            Err(Refused::Forged {
                node: key(3),
                public_key: public(7)
            })
        );
        assert_eq!(
            record.check(&outside, &ticket(7).admission(&outside), EXPIRY),
            Err(Refused::Scope {
                name: "plant.edger".parse().unwrap(),
                prefix: "plant.edge".parse().unwrap()
            })
        );
        let inside = signed(4, "plant.edge.a");
        let admission = ticket(7).admission(&inside);
        assert_eq!(record.check(&inside, &admission, BEFORE_EXPIRY), Ok(()));
        assert_eq!(
            record.check(&inside, &admission, EXPIRY),
            Err(Refused::Expired {
                public_key: public(7),
                expiry: EXPIRY,
                at: EXPIRY
            })
        );
    }

    #[test]
    fn a_ticket_admits_no_node_from_its_expiry() {
        let record = record(7, false);
        let card = signed(3, "plant.edge.a");
        assert_eq!(
            record.check(&card, &ticket(7).admission(&card), EXPIRY),
            Err(Refused::Expired {
                public_key: public(7),
                expiry: EXPIRY,
                at: EXPIRY
            })
        );
        let after = Stamp::from_nanos(EXPIRY.nanos() + 1);
        assert_eq!(
            record.check(&card, &ticket(7).admission(&card), after),
            Err(Refused::Expired {
                public_key: public(7),
                expiry: EXPIRY,
                at: after
            })
        );
        assert_eq!(
            record.check(&card, &ticket(7).admission(&card), BEFORE_EXPIRY),
            Ok(())
        );
    }

    #[test]
    fn refused_says_what_is_wrong() {
        let expiry = Stamp::from_nanos(0);
        let at = Stamp::from_nanos(1);
        let key7 = public(7);
        let cases = [
            (
                Refused::Forged {
                    node: key(3),
                    public_key: key7,
                },
                format!(
                    "the admission of node {} does not hold for ticket {key7}",
                    key(3)
                ),
            ),
            (
                Refused::Scope {
                    name: "plant.edger".parse().unwrap(),
                    prefix: "plant.edge".parse().unwrap(),
                },
                "the name plant.edger is not under the ticket's prefix plant.edge"
                    .to_owned(),
            ),
            (
                Refused::Expired {
                    public_key: key7,
                    expiry,
                    at,
                },
                format!(
                    "ticket {key7} expired at 1970-01-01T00:00:00.000000000Z, and the \
                     join is at 1970-01-01T00:00:00.000000001Z"
                ),
            ),
            (
                Refused::Used { public_key: key7 },
                format!("ticket {key7} admits one node, and it admitted one"),
            ),
        ];
        for (refused, text) in cases {
            assert_eq!(refused.to_string(), text);
        }
    }

    proptest! {
        #[test]
        fn the_text_keeps_the_key_pair_the_region_and_the_voters(
            private_key in any::<[u8; 32]>(),
            region in regions(),
            voters in voters(),
        ) {
            let ticket = Ticket::new(PrivateKey(private_key), region, voters);
            let text = ticket.text();
            let decoded = Ticket::from_str(&text).unwrap();
            prop_assert_eq!(decoded.public_key(), ticket.public_key());
            prop_assert_eq!(decoded.region(), ticket.region());
            prop_assert_eq!(decoded.voters(), ticket.voters());
            prop_assert_eq!(decoded.text(), text);
        }

        #[test]
        fn a_read_of_any_text_after_the_first_part_never_panics(
            bytes in prop::collection::vec(any::<u8>(), 0..160),
        ) {
            if let Ok(ticket) = Ticket::from_str(&text(&bytes)) {
                prop_assert_eq!(ticket.text(), text(&bytes));
            }
        }

        #[test]
        fn a_ticket_admits_each_card_that_it_signs(
            ticket_id in any::<u8>(),
            node_id in any::<u8>(),
            version in any::<u64>(),
        ) {
            let mut card = signed(node_id, "plant.edge.a").card().clone();
            card.version = version;
            let card = card::Signed::sign(key(node_id), card, &private(node_id));
            let record = record(ticket_id, false);
            let admission = ticket(ticket_id).admission(&card);
            prop_assert_eq!(record.check(&card, &admission, BEFORE_EXPIRY), Ok(()));
        }
    }
}
