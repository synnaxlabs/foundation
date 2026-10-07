//! Join tickets: the Ed25519 key pairs that admit nodes to a region.

use std::fmt;

use types::name::Name;
use types::node::{self, PrivateKey, PublicKey};
use types::time::{Span, Stamp};

use crate::card;
use crate::ed25519;

const TAG: &[u8] = b"foundation/admission/1";

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
/// is never in region state or in a file. Its `Debug` writes the region and the public
/// key only, and it has no `Display`, no `Clone`, and no equality.
pub struct Ticket {
    private_key: PrivateKey,
    region: Name,
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
    pub fn new(private_key: PrivateKey, region: Name, voters: Vec<Voter>) -> Self {
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
        ed25519::public(&ed25519::pair(&self.private_key))
    }

    /// The prefix of the ticket's region.
    #[must_use]
    pub const fn region(&self) -> &Name {
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
        ed25519::sign(&ed25519::pair(&self.private_key), &statement(card))
    }
}

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ticket")
            .field("region", &self.region)
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// The region's record of a ticket. Its key is `public_key`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    /// The public half of the ticket's key pair.
    pub(crate) public_key: PublicKey,
    /// What the ticket admits.
    pub(crate) options: Options,
    /// How many nodes the ticket admitted.
    pub(crate) uses: u64,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
impl Record {
    /// The record of a new ticket with `public_key` and `options`.
    pub(crate) const fn new(public_key: PublicKey, options: Options) -> Self {
        Self {
            public_key,
            options,
            uses: 0,
        }
    }

    /// Admits `card` with `admission` at mesh time `at`, and counts one use.
    ///
    /// # Errors
    ///
    /// The first refusal that holds, in this order: [`Refused::Forged`],
    /// [`Refused::Scope`], [`Refused::Expired`], and [`Refused::Used`]. A refusal
    /// counts no use.
    pub(crate) fn admit(
        &mut self,
        card: &card::Signed,
        admission: &[u8; 64],
        at: Stamp,
    ) -> Result<(), Refused> {
        let public_key = self.public_key;
        if !ed25519::holds(public_key, &statement(card), admission) {
            return Err(Refused::Forged {
                node: card.key(),
                public_key,
            });
        }
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
        if !self.options.reusable && self.uses > 0 {
            return Err(Refused::Used { public_key });
        }
        self.uses = self.uses.saturating_add(1);
        Ok(())
    }
}

/// Why a ticket does not admit a node.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
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
    use proptest::prelude::*;

    use super::*;
    use crate::common::{key, private, public, signed, ticket, voter};

    const EXPIRY: Stamp = Stamp::from_nanos(1_000);
    const BEFORE_EXPIRY: Stamp = Stamp::from_nanos(999);

    fn options(prefix: &str, reusable: bool) -> Options {
        Options {
            prefix: prefix.parse().unwrap(),
            reusable,
            expiry: EXPIRY,
            ephemeral: None,
        }
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
        assert_eq!(ticket.region().as_str(), "plant");
        assert_eq!(ticket.voters(), [voter()]);
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
        let region: Name = "plant".parse().unwrap();
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
        assert!(ed25519::holds(public(7), &statement, &admission));
    }

    #[test]
    fn a_single_use_ticket_admits_one_node() {
        let mut record = record(7, false);
        let (first, second) = (signed(3, "plant.edge.a"), signed(4, "plant.edge.b"));
        let at = BEFORE_EXPIRY;
        assert_eq!(
            record.admit(&first, &ticket(7).admission(&first), at),
            Ok(())
        );
        assert_eq!(
            record.admit(&second, &ticket(7).admission(&second), at),
            Err(Refused::Used {
                public_key: public(7)
            })
        );
        // The count is region state, which every node must agree on.
        assert_eq!(record.uses, 1);
    }

    #[test]
    fn a_reusable_ticket_admits_each_node() {
        let mut record = record(7, true);
        for (id, name) in [(3, "plant.edge.a"), (4, "plant.edge.b")] {
            let card = signed(id, name);
            let admitted =
                record.admit(&card, &ticket(7).admission(&card), BEFORE_EXPIRY);
            assert_eq!(admitted, Ok(()));
        }
        // The count is region state, which every node must agree on.
        assert_eq!(record.uses, 2);
    }

    #[test]
    fn an_admission_of_another_ticket_is_forged() {
        let mut record = record(7, false);
        let card = signed(3, "plant.edge.a");
        assert_eq!(
            record.admit(&card, &ticket(8).admission(&card), BEFORE_EXPIRY),
            Err(Refused::Forged {
                node: key(3),
                public_key: public(7)
            })
        );
    }

    #[test]
    fn an_admission_of_another_card_is_forged() {
        let mut record = record(7, false);
        let admission = ticket(7).admission(&signed(3, "plant.edge.a"));
        let other = signed(4, "plant.edge.a");
        assert_eq!(
            record.admit(&other, &admission, BEFORE_EXPIRY),
            Err(Refused::Forged {
                node: key(4),
                public_key: public(7)
            })
        );
    }

    #[test]
    fn a_name_outside_the_prefix_is_out_of_scope() {
        let mut record = record(7, false);
        let card = signed(3, "plant.edger");
        assert_eq!(
            record.admit(&card, &ticket(7).admission(&card), BEFORE_EXPIRY),
            Err(Refused::Scope {
                name: "plant.edger".parse().unwrap(),
                prefix: "plant.edge".parse().unwrap()
            })
        );
    }

    #[test]
    fn a_refusal_names_the_first_check_that_fails_and_counts_no_use() {
        let mut record = record(7, false);
        let outside = signed(3, "plant.edger");
        assert_eq!(
            record.admit(&outside, &ticket(8).admission(&outside), EXPIRY),
            Err(Refused::Forged {
                node: key(3),
                public_key: public(7)
            })
        );
        assert_eq!(
            record.admit(&outside, &ticket(7).admission(&outside), EXPIRY),
            Err(Refused::Scope {
                name: "plant.edger".parse().unwrap(),
                prefix: "plant.edge".parse().unwrap()
            })
        );
        let inside = signed(4, "plant.edge.a");
        let admission = ticket(7).admission(&inside);
        assert_eq!(record.admit(&inside, &admission, BEFORE_EXPIRY), Ok(()));
        assert_eq!(
            record.admit(&inside, &admission, EXPIRY),
            Err(Refused::Expired {
                public_key: public(7),
                expiry: EXPIRY,
                at: EXPIRY
            })
        );
    }

    #[test]
    fn a_ticket_admits_no_node_from_its_expiry() {
        let mut record = record(7, false);
        let card = signed(3, "plant.edge.a");
        assert_eq!(
            record.admit(&card, &ticket(7).admission(&card), EXPIRY),
            Err(Refused::Expired {
                public_key: public(7),
                expiry: EXPIRY,
                at: EXPIRY
            })
        );
        assert_eq!(
            record.admit(&card, &ticket(7).admission(&card), BEFORE_EXPIRY),
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
        fn a_ticket_admits_each_card_that_it_signs(
            ticket_id in any::<u8>(),
            node_id in any::<u8>(),
            version in any::<u64>(),
        ) {
            let mut card = signed(node_id, "plant.edge.a").card().clone();
            card.version = version;
            let card = card::Signed::sign(key(node_id), card, &private(node_id));
            let mut record = record(ticket_id, false);
            let admission = ticket(ticket_id).admission(&card);
            prop_assert_eq!(record.admit(&card, &admission, BEFORE_EXPIRY), Ok(()));
        }
    }
}
