//! Why a voter admits no node that asks to join.

use crate::bytes::{
    put_channel, put_key, put_name, put_prefix, put_public_key, put_stamp, take,
    take_channel, take_key, take_name, take_prefix, take_public_key, take_stamp,
};
use crate::card;
use crate::region::{self, Unfit};
use crate::ticket;

const FORGED: u8 = 1;
const RESERVED: u8 = 2;
const OUTSIDE: u8 = 3;
const LONG: u8 = 4;
const DUPLICATE: u8 = 5;
const HELD: u8 = 6;
const TAKEN: u8 = 7;
const REUSED: u8 = 8;
const UNKNOWN: u8 = 9;
const TICKET_FORGED: u8 = 10;
const SCOPE: u8 = 11;
const EXPIRED: u8 = 12;
const USED: u8 = 13;
const CONTESTED: u8 = 14;
const UNSYNCED: u8 = 15;
const NO_VOTE: u8 = 16;
const STOPPED: u8 = 17;
const UNLED: u8 = 18;

/// Why a voter admits no node that asks to join. The node gets it in the answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The region refuses the join, for its card, its ticket, or its fit.
    Refused(region::Refused),
    /// A member or an earlier join of the log holds the node key with another public
    /// key. The node joins with a new key.
    Contested,
    /// The voter has no mesh time that can stamp a join. The node asks again later.
    Unsynced,
    /// The join did not commit in one try: no leader took it, or a new leader replaced
    /// its entry. The node asks again later.
    Unled,
    /// The node asked a node that is not a voter. It asks another voter.
    NoVote,
    /// The group of the voter stopped. The node asks another voter.
    Stopped,
}

impl Refusal {
    /// Adds the byte form: a kind byte, then the fields of that kind in order.
    ///
    /// # Panics
    ///
    /// When `self` is a [`region::Refused`] of a change that is not a join.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Refused(refused) => put_refused(refused, out),
            Self::Contested => out.push(CONTESTED),
            Self::Unsynced => out.push(UNSYNCED),
            Self::Unled => out.push(UNLED),
            Self::NoVote => out.push(NO_VOTE),
            Self::Stopped => out.push(STOPPED),
        }
    }

    /// Takes what [`Refusal::encode`] gives. `None` when `bytes` do not start with
    /// that form.
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        let unfit = |unfit| Some(Self::Refused(region::Refused::Unfit(unfit)));
        let ticket = |refused| Some(Self::Refused(region::Refused::Ticket(refused)));
        match u8::from_le_bytes(take(bytes)?) {
            FORGED => Some(Self::Refused(region::Refused::Forged(card::Forged {
                node: take_key(bytes)?,
            }))),
            RESERVED => unfit(Unfit::Reserved {
                name: take_name(bytes)?,
            }),
            OUTSIDE => unfit(Unfit::Outside {
                name: take_name(bytes)?,
                region: take_prefix(bytes)?,
            }),
            LONG => unfit(Unfit::Long {
                name: take_name(bytes)?,
                status: take_name(bytes)?,
            }),
            DUPLICATE => unfit(Unfit::Duplicate {
                key: take_key(bytes)?,
            }),
            HELD => unfit(Unfit::Held {
                key: take_key(bytes)?,
            }),
            TAKEN => unfit(Unfit::Taken {
                name: take_name(bytes)?,
                key: take_key(bytes)?,
            }),
            REUSED => unfit(Unfit::Reused {
                key: take_channel(bytes)?,
            }),
            UNKNOWN => Some(Self::Refused(region::Refused::Unknown {
                public_key: take_public_key(bytes)?,
            })),
            TICKET_FORGED => ticket(ticket::Refused::Forged {
                node: take_key(bytes)?,
                public_key: take_public_key(bytes)?,
            }),
            SCOPE => ticket(ticket::Refused::Scope {
                name: take_name(bytes)?,
                prefix: take_name(bytes)?,
            }),
            EXPIRED => ticket(ticket::Refused::Expired {
                public_key: take_public_key(bytes)?,
                expiry: take_stamp(bytes)?,
                at: take_stamp(bytes)?,
            }),
            USED => ticket(ticket::Refused::Used {
                public_key: take_public_key(bytes)?,
            }),
            CONTESTED => Some(Self::Contested),
            UNSYNCED => Some(Self::Unsynced),
            UNLED => Some(Self::Unled),
            NO_VOTE => Some(Self::NoVote),
            STOPPED => Some(Self::Stopped),
            _ => None,
        }
    }
}

fn put_refused(refused: &region::Refused, out: &mut Vec<u8>) {
    match refused {
        region::Refused::Forged(forged) => {
            out.push(FORGED);
            put_key(forged.node, out);
        }
        region::Refused::Unfit(unfit) => put_unfit(unfit, out),
        region::Refused::Unknown { public_key } => {
            out.push(UNKNOWN);
            put_public_key(*public_key, out);
        }
        region::Refused::Ticket(refused) => put_ticket(refused, out),
        other => panic!("invariant: a join is refused for {other:?}, not by a join"),
    }
}

fn put_unfit(unfit: &Unfit, out: &mut Vec<u8>) {
    match unfit {
        Unfit::Reserved { name } => {
            out.push(RESERVED);
            put_name(name, out);
        }
        Unfit::Outside { name, region } => {
            out.push(OUTSIDE);
            put_name(name, out);
            put_prefix(region, out);
        }
        Unfit::Long { name, status } => {
            out.push(LONG);
            put_name(name, out);
            put_name(status, out);
        }
        Unfit::Duplicate { key } => {
            out.push(DUPLICATE);
            put_key(*key, out);
        }
        Unfit::Held { key } => {
            out.push(HELD);
            put_key(*key, out);
        }
        Unfit::Taken { name, key } => {
            out.push(TAKEN);
            put_name(name, out);
            put_key(*key, out);
        }
        Unfit::Reused { key } => {
            out.push(REUSED);
            put_channel(*key, out);
        }
    }
}

fn put_ticket(refused: &ticket::Refused, out: &mut Vec<u8>) {
    match refused {
        ticket::Refused::Forged { node, public_key } => {
            out.push(TICKET_FORGED);
            put_key(*node, out);
            put_public_key(*public_key, out);
        }
        ticket::Refused::Scope { name, prefix } => {
            out.push(SCOPE);
            put_name(name, out);
            put_name(prefix, out);
        }
        ticket::Refused::Expired {
            public_key,
            expiry,
            at,
        } => {
            out.push(EXPIRED);
            put_public_key(*public_key, out);
            put_stamp(*expiry, out);
            put_stamp(*at, out);
        }
        ticket::Refused::Used { public_key } => {
            out.push(USED);
            put_public_key(*public_key, out);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use proptest::prelude::*;
    use types::name::Prefix;
    use types::time::Stamp;

    use super::*;
    use crate::common::{index, key, name, public};

    /// A refusal of each kind, with small fields.
    pub(crate) fn refusals() -> impl Strategy<Value = Refusal> {
        let name =
            || prop::sample::select(vec!["plant", "plant.a", "@b"]).prop_map(name);
        let id = || 1..=9_u8;
        let unfit = prop_oneof![
            name().prop_map(|name| Unfit::Reserved { name }),
            name().prop_map(|name| Unfit::Outside {
                name,
                region: Prefix::ROOT,
            }),
            (name(), name()).prop_map(|(name, status)| Unfit::Long { name, status }),
            id().prop_map(|id| Unfit::Duplicate { key: key(id) }),
            id().prop_map(|id| Unfit::Held { key: key(id) }),
            (name(), id()).prop_map(|(name, id)| Unfit::Taken { name, key: key(id) }),
            any::<u128>().prop_map(|bits| Unfit::Reused { key: index(bits) }),
        ];
        let ticket = prop_oneof![
            (id(), id()).prop_map(|(node, holder)| ticket::Refused::Forged {
                node: key(node),
                public_key: public(holder),
            }),
            (name(), name())
                .prop_map(|(name, prefix)| ticket::Refused::Scope { name, prefix }),
            (id(), any::<i64>(), any::<i64>()).prop_map(|(id, expiry, at)| {
                ticket::Refused::Expired {
                    public_key: public(id),
                    expiry: Stamp::from_nanos(expiry),
                    at: Stamp::from_nanos(at),
                }
            }),
            id().prop_map(|id| ticket::Refused::Used {
                public_key: public(id)
            }),
        ];
        let refused = prop_oneof![
            id().prop_map(|id| region::Refused::Forged(card::Forged { node: key(id) })),
            unfit.prop_map(region::Refused::Unfit),
            id().prop_map(|id| region::Refused::Unknown {
                public_key: public(id)
            }),
            ticket.prop_map(region::Refused::Ticket),
        ];
        prop_oneof![
            4 => refused.prop_map(Refusal::Refused),
            1 => Just(Refusal::Contested),
            1 => Just(Refusal::Unsynced),
            1 => Just(Refusal::Unled),
            1 => Just(Refusal::NoVote),
            1 => Just(Refusal::Stopped),
        ]
    }

    fn encoded(refusal: &Refusal) -> Vec<u8> {
        let mut out = Vec::new();
        refusal.encode(&mut out);
        out
    }

    #[test]
    fn a_refusal_has_a_fixed_byte_form() {
        let unknown = Refusal::Refused(region::Refused::Unknown {
            public_key: public(2),
        });
        let taken = Refusal::Refused(region::Refused::Unfit(Unfit::Taken {
            name: name("plant"),
            key: key(3),
        }));
        let mut key_3 = vec![3];
        key_3.extend([0; 15]);
        let cases: [(Refusal, Vec<u8>); 7] = [
            (unknown, [&[9][..], &public(2).to_bytes()].concat()),
            (taken, [&[7, 5][..], b"plant", &key_3].concat()),
            (Refusal::Contested, vec![14]),
            (Refusal::Unsynced, vec![15]),
            (Refusal::NoVote, vec![16]),
            (Refusal::Stopped, vec![17]),
            (Refusal::Unled, vec![18]),
        ];
        for (refusal, expected) in cases {
            assert_eq!(encoded(&refusal), expected, "{refusal:?}");
            assert_eq!(Refusal::decode(&mut expected.as_slice()), Some(refusal));
        }
        for kind in [0, 19, 255] {
            assert_eq!(Refusal::decode(&mut [kind].as_slice()), None, "{kind}");
        }
    }

    #[test]
    #[should_panic(expected = "invariant: a join is refused for Recorded")]
    fn a_refusal_of_a_change_that_is_not_a_join_has_no_byte_form() {
        let recorded = region::Refused::Recorded {
            public_key: public(1),
        };
        encoded(&Refusal::Refused(recorded));
    }

    proptest! {
        // A refusal reads back and leaves what follows it, and no cut of it reads.
        #[test]
        fn a_refusal_reads_back_from_its_byte_form(
            refusal in refusals(),
            cut in any::<prop::sample::Index>(),
            extra in any::<u8>(),
        ) {
            let bytes = encoded(&refusal);
            let cut = &bytes[..cut.index(bytes.len())];
            prop_assert_eq!(Refusal::decode(&mut &cut[..]), None);
            let mut longer = bytes;
            longer.push(extra);
            let mut rest = longer.as_slice();
            prop_assert_eq!(Refusal::decode(&mut rest), Some(refusal));
            prop_assert_eq!(rest, [extra]);
        }
    }
}
