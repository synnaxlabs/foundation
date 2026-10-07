//! The region state that the voters agree on, and the change records that move it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use types::channel;
use types::name::Name;
use types::node::{self, PublicKey};
use types::time::Stamp;

use crate::bytes::{
    put_channel, put_key, put_public_key, put_stamp, take, take_channel, take_key,
    take_public_key, take_stamp,
};
use crate::card;
use crate::member::Member;
use crate::status::Status;
use crate::ticket::{self, Options, Record};

/// The region state that this node holds: its members, its tickets, and the homes that
/// it applied.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
pub(crate) struct State {
    region: Name,
    members: BTreeMap<node::Key, Member>,
    tickets: BTreeMap<[u8; 32], Record>,
    homes: BTreeMap<channel::Key, node::Key>,
    // Each name that a member holds, its card name and each status channel name, in
    // ASCII lower case, so that two names that differ only in case collide (A3).
    names: BTreeMap<String, node::Key>,
    // Each status channel key that a member holds.
    status: BTreeSet<channel::Key>,
}

impl State {
    /// The state of the region with prefix `region`, with `members`, each under the key
    /// of its card, and no ticket or home.
    ///
    /// # Errors
    ///
    /// [`Unfit`] when the region cannot hold a member, such as when two of `members`
    /// have one key.
    pub(crate) fn new(region: Name, members: Vec<Member>) -> Result<Self, Unfit> {
        let mut state = Self {
            region,
            members: BTreeMap::new(),
            tickets: BTreeMap::new(),
            homes: BTreeMap::new(),
            names: BTreeMap::new(),
            status: BTreeSet::new(),
        };
        for member in members {
            let names = state.fits(&member.card, &member.status)?;
            state.insert(member, names);
        }
        Ok(state)
    }

    /// The member with `key`, or `None` when the region has no such member.
    pub(crate) fn member(&self, key: node::Key) -> Option<&Member> {
        self.members.get(&key)
    }

    /// The record of the ticket with `public_key`, or `None` when none is recorded.
    pub(crate) fn ticket(&self, public_key: PublicKey) -> Option<&Record> {
        self.tickets.get(&public_key.to_bytes())
    }

    /// The home of `index`, or `None` when none is set.
    pub(crate) fn home(&self, index: channel::Key) -> Option<node::Key> {
        self.homes.get(&index).copied()
    }

    /// Applies `change`. Returns the index whose home it moved, or `None` when it moved
    /// no home. Every node refuses the same changes, so all keep one state.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the change does not hold against the state. The state is then
    /// as before.
    pub(crate) fn apply(
        &mut self,
        change: Change,
    ) -> Result<Option<channel::Key>, Refused> {
        match change {
            Change::Home { index, home } => {
                Ok((self.homes.insert(index, home) != Some(home)).then_some(index))
            }
            Change::Join(join) => self.join(*join).map(|()| None),
            Change::Ticket {
                public_key,
                options,
            } => self.record(public_key, options).map(|()| None),
        }
    }

    // Admits the node of `join`. The ticket counts a use only when all checks pass.
    fn join(&mut self, join: Join) -> Result<(), Refused> {
        let card = join.card.check().map_err(Refused::Forged)?;
        let names = self.fits(&card, &join.status)?;
        let record =
            self.tickets
                .get_mut(&join.ticket.to_bytes())
                .ok_or(Refused::Unknown {
                    public_key: join.ticket,
                })?;
        record
            .admit(&card, &join.admission, join.at)
            .map_err(Refused::Ticket)?;
        let member = Member {
            card,
            admission: join.admission,
            ephemeral: record.options.ephemeral,
            status: join.status,
        };
        self.insert(member, names);
        Ok(())
    }

    // Adds `member`, whose names `fits` gave.
    fn insert(&mut self, member: Member, names: Vec<String>) {
        let key = member.card.key();
        self.names.extend(names.into_iter().map(|name| (name, key)));
        self.status.extend(member.status.as_map().values());
        self.members.insert(key, member);
    }

    fn record(
        &mut self,
        public_key: PublicKey,
        options: Options,
    ) -> Result<(), Refused> {
        if !options.prefix.starts_with(&self.region) {
            return Err(Refused::Outside {
                prefix: options.prefix,
                region: self.region.clone(),
            });
        }
        let bytes = public_key.to_bytes();
        if self.tickets.contains_key(&bytes) {
            return Err(Refused::Recorded { public_key });
        }
        self.tickets.insert(bytes, Record::new(public_key, options));
        Ok(())
    }

    // The one check of a member against the region, at open and at each join. Gives
    // the member's names in ASCII lower case.
    fn fits(&self, card: &card::Signed, status: &Status) -> Result<Vec<String>, Unfit> {
        let status = status.as_map();
        let name = &card.card().name;
        if name.reserved() {
            return Err(Unfit::Reserved { name: name.clone() });
        }
        if !name.starts_with(&self.region) {
            return Err(Unfit::Outside {
                name: name.clone(),
                region: self.region.clone(),
            });
        }
        let mut names = vec![name.clone()];
        for status in status.keys() {
            // Two names joined by a dot fail to parse only on length.
            let Ok(full) = format!("{name}.{status}").parse::<Name>() else {
                return Err(Unfit::Long {
                    name: name.clone(),
                    status: status.clone(),
                });
            };
            if full.reserved() {
                return Err(Unfit::Reserved { name: full });
            }
            names.push(full);
        }
        let key = card.key();
        if self.members.contains_key(&key) {
            return Err(Unfit::Duplicate { key });
        }
        let mut lower = Vec::with_capacity(names.len());
        for name in names {
            let folded = name.as_str().to_ascii_lowercase();
            if let Some(&holder) = self.names.get(&folded) {
                return Err(Unfit::Taken { name, key: holder });
            }
            if lower.contains(&folded) {
                return Err(Unfit::Taken { name, key });
            }
            lower.push(folded);
        }
        let mut keys = BTreeSet::new();
        for &key in status.values() {
            if self.status.contains(&key) || !keys.insert(key) {
                return Err(Unfit::Reused { key });
            }
        }
        Ok(lower)
    }
}

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
    /// The mesh time of the join: the later edge of the proposing voter's mesh time.
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
    ///   in [`Member::encode`].
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

/// Why every node refuses a change. A refused change changes no state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The card of a `Join` is forged.
    Forged(card::Forged),
    /// The region cannot hold the member that a `Join` admits.
    Unfit(Unfit),
    /// No ticket with the public key of a `Join` is recorded.
    Unknown {
        /// The public key.
        public_key: PublicKey,
    },
    /// The ticket of a `Join` does not admit the node.
    Ticket(ticket::Refused),
    /// A ticket with the public key of a `Ticket` change is already recorded.
    Recorded {
        /// The public key.
        public_key: PublicKey,
    },
    /// The prefix of a `Ticket` change is not under the region's prefix.
    Outside {
        /// The ticket's prefix.
        prefix: Name,
        /// The region's prefix.
        region: Name,
    },
    /// The bytes of a committed entry of a known kind are not the body of that kind,
    /// as [`Malformed::Body`].
    Body {
        /// The kind byte.
        kind: u8,
        /// The length of the bytes.
        length: usize,
    },
}

impl From<Unfit> for Refused {
    fn from(unfit: Unfit) -> Self {
        Self::Unfit(unfit)
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forged(forged) => forged.fmt(f),
            Self::Unfit(unfit) => unfit.fmt(f),
            Self::Unknown { public_key } => {
                write!(f, "no ticket {public_key} is recorded")
            }
            Self::Ticket(refused) => refused.fmt(f),
            Self::Recorded { public_key } => {
                write!(f, "ticket {public_key} is already recorded")
            }
            Self::Outside { prefix, region } => {
                write!(f, "the prefix {prefix} is not under the region {region}")
            }
            Self::Body { kind, length } => Malformed::Body {
                kind: *kind,
                length: *length,
            }
            .fmt(f),
        }
    }
}

impl std::error::Error for Refused {}

/// Why the region cannot hold a member: a founding member, or the node of a `Join`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unfit {
    /// A segment of the member's name, or of the name of one of its status channels,
    /// starts with `@`.
    Reserved {
        /// The name.
        name: Name,
    },
    /// The member's name is not under the region's prefix.
    Outside {
        /// The member's name.
        name: Name,
        /// The region's prefix.
        region: Name,
    },
    /// The name of a status channel of the member, `<name>.<status>`, is longer than
    /// [`Name::MAX_BYTES`].
    Long {
        /// The member's name.
        name: Name,
        /// The status name under it.
        status: Name,
    },
    /// The node is already a member.
    Duplicate {
        /// The node.
        key: node::Key,
    },
    /// The member's name, or the name of one of its status channels, equals a name
    /// that a member holds, ignoring ASCII case.
    Taken {
        /// The name.
        name: Name,
        /// The member that holds it, or the node itself when it holds the name twice.
        key: node::Key,
    },
    /// A status channel key of the member is held by a member, or twice by this one.
    Reused {
        /// The status channel key.
        key: channel::Key,
    },
}

impl fmt::Display for Unfit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reserved { name } => write!(
                f,
                "the name {name} has a segment that starts with `@`, which is reserved \
                 for Foundation"
            ),
            Self::Outside { name, region } => {
                write!(f, "the name {name} is not under the region {region}")
            }
            Self::Long { name, status } => write!(
                f,
                "the status channel {name}.{status} is longer than {} bytes",
                Name::MAX_BYTES
            ),
            Self::Duplicate { key } => write!(f, "node {key} is already a member"),
            Self::Taken { name, key } => {
                write!(f, "the name {name} is taken by node {key}")
            }
            Self::Reused { key } => {
                write!(f, "the status channel key {key} is already in use")
            }
        }
    }
}

impl std::error::Error for Unfit {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use types::time::Span;

    use super::*;
    use crate::common::ticket;
    use crate::common::{
        create_members, key as node, public, signed, status, status_bytes,
    };

    const EXPIRY: Stamp = Stamp::from_nanos(1_000);
    const BEFORE_EXPIRY: Stamp = Stamp::from_nanos(999);
    const EPHEMERAL: Span = Span::from_nanos(60);

    fn index(bits: u128) -> channel::Key {
        channel::Key::from_u128(bits)
    }

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    fn options(prefix: &str, reusable: bool) -> Options {
        Options {
            prefix: name(prefix),
            reusable,
            expiry: EXPIRY,
            ephemeral: Some(EPHEMERAL),
        }
    }

    fn record(id: u8, options: Options) -> Change {
        Change::Ticket {
            public_key: public(id),
            options,
        }
    }

    // The join of node `id` with `name`, which ticket `ticket` admits.
    fn join(ticket_id: u8, id: u8, name_text: &str) -> Join {
        let card = signed(id, name_text);
        Join {
            ticket: public(ticket_id),
            at: BEFORE_EXPIRY,
            card: card::Unchecked {
                key: node(id),
                card: card.card().clone(),
                signature: *card.signature(),
            },
            admission: ticket(ticket_id).admission(&card),
            status: status([(name("disk"), index(9))]),
        }
    }

    // Members 1 and 2, and single-use ticket 7 for `plant.edge`.
    fn state() -> State {
        let mut state = State::new(name("plant"), create_members(&[1, 2])).unwrap();
        let recorded = state.apply(record(7, options("plant.edge", false)));
        assert_eq!(recorded, Ok(None));
        state
    }

    fn home(i: u128, h: u128) -> Change {
        Change::Home {
            index: index(i),
            home: node::Key::from_u128(h),
        }
    }

    #[test]
    fn a_home_is_none_until_a_change_sets_it() {
        let mut state = state();
        assert_eq!(state.home(index(7)), None);
        assert_eq!(state.apply(home(7, 1)), Ok(Some(index(7))));
        assert_eq!(state.home(index(7)), Some(node(1)));
        assert_eq!(state.home(index(8)), None);
    }

    #[test]
    fn a_change_to_the_same_home_moves_nothing() {
        let mut state = state();
        assert_eq!(state.apply(home(7, 1)), Ok(Some(index(7))));
        assert_eq!(state.apply(home(7, 1)), Ok(None));
        assert_eq!(state.apply(home(7, 2)), Ok(Some(index(7))));
        assert_eq!(state.home(index(7)), Some(node(2)));
    }

    #[test]
    fn new_refuses_two_members_with_one_key() {
        let error = State::new(name("plant"), create_members(&[1, 2, 1])).unwrap_err();
        assert_eq!(error, Unfit::Duplicate { key: node(1) });
        assert_eq!(
            error.to_string(),
            format!("node {} is already a member", node(1))
        );
    }

    #[test]
    fn new_refuses_a_member_that_the_region_cannot_hold() {
        let mut reserved = create_members(&[1, 2]);
        reserved[1].card = signed(2, "plant.@changes");
        assert_eq!(
            State::new(name("plant"), reserved),
            Err(Unfit::Reserved {
                name: name("plant.@changes")
            })
        );
        let mut outside = create_members(&[1, 2]);
        outside[1].card = signed(2, "factory.node2");
        assert_eq!(
            State::new(name("plant"), outside),
            Err(Unfit::Outside {
                name: name("factory.node2"),
                region: name("plant")
            })
        );
        let mut long = create_members(&[1, 2]);
        long[1].status = status([(long_status(256 - 12), index(1))]);
        assert_eq!(
            State::new(name("plant"), long).unwrap_err(),
            Unfit::Long {
                name: name("plant.node2"),
                status: long_status(256 - 12),
            }
        );
    }

    #[test]
    fn a_ticket_change_records_the_ticket() {
        let state = state();
        let options = options("plant.edge", false);
        let expected = ticket::Record::new(public(7), options);
        assert_eq!(state.ticket(public(7)), Some(&expected));
        assert_eq!(state.ticket(public(8)), None);
    }

    #[test]
    fn a_second_record_of_a_ticket_is_refused() {
        let mut state = state();
        let before = state.clone();
        assert_eq!(
            state.apply(record(7, options("plant", true))),
            Err(Refused::Recorded {
                public_key: public(7)
            })
        );
        assert_eq!(state, before);
    }

    #[test]
    fn a_ticket_outside_the_region_is_refused() {
        let mut state = state();
        let before = state.clone();
        assert_eq!(
            state.apply(record(8, options("plants.edge", false))),
            Err(Refused::Outside {
                prefix: name("plants.edge"),
                region: name("plant")
            })
        );
        assert_eq!(state, before);
        assert_eq!(state.apply(record(8, options("plant", false))), Ok(None));
    }

    #[test]
    fn a_join_admits_the_node_with_the_ticket_options() {
        let mut state = state();
        let join = join(7, 3, "plant.edge.a");
        let admission = join.admission;
        assert_eq!(state.apply(Change::Join(Box::new(join))), Ok(None));
        let expected = Member {
            card: signed(3, "plant.edge.a"),
            admission,
            ephemeral: Some(EPHEMERAL),
            status: status([(name("disk"), index(9))]),
        };
        assert_eq!(state.member(node(3)), Some(&expected));
        assert_eq!(state.ticket(public(7)).map(|record| record.uses), Some(1));
    }

    #[test]
    fn a_join_with_a_forged_card_is_refused() {
        let mut state = state();
        let before = state.clone();
        let mut join = join(7, 3, "plant.edge.a");
        join.card.signature[0] ^= 1;
        assert_eq!(
            state.apply(Change::Join(Box::new(join))),
            Err(Refused::Forged(card::Forged { node: node(3) }))
        );
        assert_eq!(state, before);
    }

    #[test]
    fn a_join_with_an_unknown_ticket_is_refused() {
        let mut state = state();
        let before = state.clone();
        assert_eq!(
            state.apply(Change::Join(Box::new(join(8, 3, "plant.edge.a")))),
            Err(Refused::Unknown {
                public_key: public(8)
            })
        );
        assert_eq!(state, before);
    }

    #[test]
    fn a_join_that_the_ticket_refuses_is_refused() {
        let mut state = state();
        let before = state.clone();
        assert_eq!(
            state.apply(Change::Join(Box::new(join(7, 3, "plant.edger")))),
            Err(Refused::Ticket(ticket::Refused::Scope {
                name: name("plant.edger"),
                prefix: name("plant.edge")
            }))
        );
        assert_eq!(state, before);
    }

    // A refusal after the ticket's checks would pass still counts no use.
    #[test]
    fn a_join_for_a_member_is_refused_and_counts_no_use() {
        let mut state = state();
        let before = state.clone();
        assert_eq!(
            state.apply(Change::Join(Box::new(join(7, 1, "plant.edge.a")))),
            Err(Unfit::Duplicate { key: node(1) }.into())
        );
        assert_eq!(state, before);
        let admitted = state.apply(Change::Join(Box::new(join(7, 3, "plant.edge.a"))));
        assert_eq!(admitted, Ok(None));
    }

    // A status name of `len` bytes.
    fn long_status(len: usize) -> Name {
        name(&"s".repeat(len))
    }

    fn apply_join(
        state: &mut State,
        join: Join,
    ) -> Result<Option<channel::Key>, Refused> {
        state.apply(Change::Join(Box::new(join)))
    }

    #[test]
    fn a_join_with_a_reserved_name_is_refused_and_counts_no_use() {
        let mut state = state();
        assert_eq!(state.apply(record(8, options("plant", false))), Ok(None));
        let before = state.clone();
        assert_eq!(
            apply_join(&mut state, join(8, 3, "plant.@changes")),
            Err(Unfit::Reserved {
                name: name("plant.@changes")
            }
            .into())
        );
        assert_eq!(state, before);
        assert_eq!(state.ticket(public(8)).map(|record| record.uses), Some(0));
    }

    // `plant.@changes` is the region's changes channel.
    #[test]
    fn a_join_with_a_reserved_status_name_is_refused() {
        let mut state = state();
        assert_eq!(state.apply(record(8, options("plant", true))), Ok(None));
        let before = state.clone();
        let mut reserved = join(8, 3, "plant");
        reserved.status = status([(name("@changes"), index(9))]);
        assert_eq!(
            apply_join(&mut state, reserved),
            Err(Unfit::Reserved {
                name: name("plant.@changes")
            }
            .into())
        );
        assert_eq!(state, before);
        let mut deep = join(8, 3, "plant");
        deep.status = status([(name("disk.@a"), index(9))]);
        assert_eq!(
            apply_join(&mut state, deep),
            Err(Unfit::Reserved {
                name: name("plant.disk.@a")
            }
            .into())
        );
        assert_eq!(apply_join(&mut state, join(8, 3, "plant")), Ok(None));
    }

    // A ticket's prefix is under the region, so only a founding member can be outside
    // it; a join outside the region fails here before the ticket's scope.
    #[test]
    fn a_join_outside_the_region_is_refused() {
        let mut state = state();
        let before = state.clone();
        assert_eq!(
            apply_join(&mut state, join(7, 3, "plants.edge")),
            Err(Unfit::Outside {
                name: name("plants.edge"),
                region: name("plant")
            }
            .into())
        );
        assert_eq!(state, before);
    }

    #[test]
    fn a_join_whose_status_channel_name_is_too_long_is_refused() {
        let mut state = state();
        let before = state.clone();
        // `plant.edge.a.` is 13 bytes.
        let mut long = join(7, 3, "plant.edge.a");
        long.status = status([(long_status(256 - 13), index(9))]);
        assert_eq!(
            apply_join(&mut state, long),
            Err(Unfit::Long {
                name: name("plant.edge.a"),
                status: long_status(256 - 13),
            }
            .into())
        );
        assert_eq!(state, before);
        let mut longest = join(7, 3, "plant.edge.a");
        longest.status = status([(long_status(255 - 13), index(9))]);
        assert_eq!(apply_join(&mut state, longest), Ok(None));
    }

    // Member 1 is `plant.node1`. Ticket 8 admits any node under `plant`.
    fn open_state() -> State {
        let mut state = state();
        assert_eq!(state.apply(record(8, options("plant", true))), Ok(None));
        state
    }

    fn with_status(mut join: Join, status: &[(&str, u128)]) -> Join {
        let map = status.iter().map(|&(text, key)| (name(text), index(key)));
        join.status = Status::new(map.collect()).unwrap();
        join
    }

    #[test]
    fn a_join_with_a_name_that_a_member_holds_is_refused() {
        let mut state = open_state();
        let before = state.clone();
        let cases = [
            (join(8, 3, "plant.node1"), "plant.node1", 1),
            (join(8, 3, "plant.Node1"), "plant.Node1", 1),
            (join(8, 3, "plant.NODE2"), "plant.NODE2", 2),
        ];
        for (join, taken, holder) in cases {
            assert_eq!(
                apply_join(&mut state, join),
                Err(Unfit::Taken {
                    name: name(taken),
                    key: node(holder)
                }
                .into())
            );
            assert_eq!(state, before);
        }
        assert_eq!(state.ticket(public(8)).map(|record| record.uses), Some(0));
    }

    #[test]
    fn a_join_refused_as_taken_counts_no_use() {
        let mut state = open_state();
        assert_eq!(apply_join(&mut state, join(8, 3, "plant.edge.a")), Ok(None));
        let before = state.clone();
        assert_eq!(
            apply_join(&mut state, join(7, 4, "plant.edge.A")),
            Err(Unfit::Taken {
                name: name("plant.edge.A"),
                key: node(3)
            }
            .into())
        );
        assert_eq!(state, before);
        assert_eq!(state.ticket(public(7)).map(|record| record.uses), Some(0));
        let other = with_status(join(7, 4, "plant.edge.b"), &[("disk", 10)]);
        assert_eq!(apply_join(&mut state, other), Ok(None));
        assert_eq!(state.ticket(public(7)).map(|record| record.uses), Some(1));
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

    #[test]
    fn a_join_whose_status_channel_name_a_member_holds_is_refused() {
        let mut state = open_state();
        let first = with_status(join(8, 3, "plant.a"), &[("b.c", 20)]);
        assert_eq!(apply_join(&mut state, first), Ok(None));
        let before = state.clone();
        let cases = [
            (
                with_status(join(8, 4, "plant.a.b"), &[("c", 21)]),
                "plant.a.b.c",
            ),
            (
                with_status(join(8, 4, "plant.a.B"), &[("C", 21)]),
                "plant.a.B.C",
            ),
            (join(8, 4, "plant.a.b.C"), "plant.a.b.C"),
        ];
        for (join, taken) in cases {
            assert_eq!(
                apply_join(&mut state, join),
                Err(Unfit::Taken {
                    name: name(taken),
                    key: node(3)
                }
                .into())
            );
            assert_eq!(state, before);
        }
        let other = with_status(join(8, 4, "plant.a.b"), &[("d", 21)]);
        assert_eq!(apply_join(&mut state, other), Ok(None));
    }

    #[test]
    fn a_join_with_two_status_names_that_differ_in_case_is_refused() {
        let mut state = open_state();
        let before = state.clone();
        let join = with_status(join(8, 3, "plant.a"), &[("Disk", 20), ("disk", 21)]);
        assert_eq!(
            apply_join(&mut state, join),
            Err(Unfit::Taken {
                name: name("plant.a.disk"),
                key: node(3)
            }
            .into())
        );
        assert_eq!(state, before);
    }

    #[test]
    fn a_join_with_a_status_key_in_use_is_refused() {
        let mut state = open_state();
        let first = with_status(join(8, 3, "plant.a"), &[("disk", 20)]);
        assert_eq!(apply_join(&mut state, first), Ok(None));
        let before = state.clone();
        let held = with_status(join(8, 4, "plant.b"), &[("disk", 20)]);
        let twice = with_status(join(8, 4, "plant.b"), &[("cpu", 21), ("disk", 21)]);
        for (join, key) in [(held, 20), (twice, 21)] {
            assert_eq!(
                apply_join(&mut state, join),
                Err(Unfit::Reused { key: index(key) }.into())
            );
            assert_eq!(state, before);
        }
    }

    #[test]
    fn new_refuses_two_members_with_one_name_or_one_status_key() {
        let mut taken = create_members(&[1, 2]);
        taken[1].card = signed(2, "plant.NODE1");
        assert_eq!(
            State::new(name("plant"), taken),
            Err(Unfit::Taken {
                name: name("plant.NODE1"),
                key: node(1)
            })
        );
        let mut reused = create_members(&[1, 2]);
        reused[0].status = status([(name("disk"), index(20))]);
        reused[1].status = status([(name("disk"), index(20))]);
        assert_eq!(
            State::new(name("plant"), reused),
            Err(Unfit::Reused { key: index(20) })
        );
    }

    // Each join fails more than one check, and the refusal names the first.
    #[test]
    fn a_join_refusal_names_the_first_check_that_fails() {
        let mut state = state();
        let mut forged = join(7, 1, "plant.@a");
        forged.card.signature[0] ^= 1;
        let mut outside = join(7, 1, "plants.edge.a");
        outside.status = status([(long_status(256 - 14), index(9))]);
        let mut long = join(7, 1, "plant.edge.a");
        long.status =
            status([(long_status(256 - 13), index(9)), (name("t.@a"), index(10))]);
        let cases = [
            (forged, Refused::Forged(card::Forged { node: node(1) })),
            (
                join(7, 1, "plant.@a"),
                Unfit::Reserved {
                    name: name("plant.@a"),
                }
                .into(),
            ),
            (
                outside,
                Unfit::Outside {
                    name: name("plants.edge.a"),
                    region: name("plant"),
                }
                .into(),
            ),
            (
                long,
                Unfit::Long {
                    name: name("plant.edge.a"),
                    status: long_status(256 - 13),
                }
                .into(),
            ),
            (
                join(8, 1, "plant.edge.a"),
                Unfit::Duplicate { key: node(1) }.into(),
            ),
            (
                with_status(join(7, 3, "plant.node2"), &[("disk", 9), ("x", 9)]),
                Unfit::Taken {
                    name: name("plant.node2"),
                    key: node(2),
                }
                .into(),
            ),
            (
                with_status(join(8, 3, "plant.edge.a"), &[("disk", 9), ("x", 9)]),
                Unfit::Reused { key: index(9) }.into(),
            ),
        ];
        let before = state.clone();
        for (join, refused) in cases {
            assert_eq!(apply_join(&mut state, join), Err(refused));
            assert_eq!(state, before);
        }
    }

    #[test]
    fn refused_says_what_is_wrong() {
        let key7 = public(7);
        let cases = [
            (
                Refused::Forged(card::Forged { node: node(3) }),
                format!("the card of node {} is forged", node(3)),
            ),
            (
                Refused::Unknown { public_key: key7 },
                format!("no ticket {key7} is recorded"),
            ),
            (
                Refused::Ticket(ticket::Refused::Used { public_key: key7 }),
                format!("ticket {key7} admits one node, and it admitted one"),
            ),
            (
                Refused::Recorded { public_key: key7 },
                format!("ticket {key7} is already recorded"),
            ),
            (
                Refused::Outside {
                    prefix: name("plants.edge"),
                    region: name("plant"),
                },
                "the prefix plants.edge is not under the region plant".to_owned(),
            ),
            (
                Refused::Body {
                    kind: 2,
                    length: 40,
                },
                "a change of kind 2 and 40 bytes is not in the byte form of its kind"
                    .to_owned(),
            ),
        ];
        for (refused, text) in cases {
            assert_eq!(refused.to_string(), text);
        }
    }

    #[test]
    fn unfit_says_what_is_wrong() {
        let cases = [
            (
                Unfit::Reserved {
                    name: name("plant.@changes"),
                },
                "the name plant.@changes has a segment that starts with `@`, which is \
                 reserved for Foundation"
                    .to_owned(),
            ),
            (
                Unfit::Long {
                    name: name("plant.a"),
                    status: name("disk"),
                },
                "the status channel plant.a.disk is longer than 255 bytes".to_owned(),
            ),
            (
                Unfit::Duplicate { key: node(1) },
                format!("node {} is already a member", node(1)),
            ),
            (
                Unfit::Taken {
                    name: name("plant.a"),
                    key: node(1),
                },
                format!("the name plant.a is taken by node {}", node(1)),
            ),
            (
                Unfit::Reused { key: index(9) },
                format!("the status channel key {} is already in use", index(9)),
            ),
            (
                Unfit::Outside {
                    name: name("plants.edge"),
                    region: name("plant"),
                },
                "the name plants.edge is not under the region plant".to_owned(),
            ),
        ];
        for (unfit, text) in cases {
            assert_eq!(unfit.to_string(), text);
            assert_eq!(Refused::Unfit(unfit).to_string(), text);
        }
    }

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

    // A change from a small set, so that changes meet the same keys and tickets.
    fn steps() -> impl Strategy<Value = Change> {
        let joins = (
            7..10_u8,
            1..6_u8,
            prop::sample::select(vec!["plant.edge.a", "plant.edge.b", "plant.edger"]),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(ticket_id, id, name, late, forged)| {
                let mut join = join(ticket_id, id, name);
                if late {
                    join.at = EXPIRY;
                }
                if forged {
                    join.admission[0] ^= 1;
                }
                Change::Join(Box::new(join))
            });
        let tickets = (
            7..10_u8,
            prop::sample::select(vec!["plant.edge", "plant", "factory"]),
            any::<bool>(),
        )
            .prop_map(|(id, prefix, reusable)| record(id, options(prefix, reusable)));
        prop_oneof![joins, tickets]
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

        // A refused change leaves the state as it was, and an applied join adds its
        // member.
        #[test]
        fn a_refused_change_changes_nothing(
            steps in prop::collection::vec(steps(), 0..16),
        ) {
            let mut state = State::new(name("plant"), create_members(&[1, 2])).unwrap();
            for step in steps {
                let before = state.clone();
                let applied = state.apply(step.clone());
                match (applied, step) {
                    (Err(_), _) => prop_assert_eq!(&state, &before),
                    (Ok(_), Change::Join(join)) => {
                        prop_assert!(before.member(join.card.key).is_none());
                        prop_assert!(state.member(join.card.key).is_some());
                    }
                    (Ok(_), _) => {}
                }
            }
        }

        // The applied state is the last home that each index was given.
        #[test]
        fn the_state_keeps_the_last_home_of_each_index(
            changes in prop::collection::vec((0..4u128, 0..3u128), 0..32),
        ) {
            let mut state = state();
            let mut last = BTreeMap::new();
            for (i, h) in changes {
                let before = last.insert(i, h);
                let moved = state.apply(home(i, h));
                prop_assert_eq!(moved, Ok((before != Some(h)).then_some(index(i))));
            }
            for i in 0..4 {
                prop_assert_eq!(
                    state.home(index(i)),
                    last.get(&i).map(|&h| node::Key::from_u128(h))
                );
            }
        }
    }
}
