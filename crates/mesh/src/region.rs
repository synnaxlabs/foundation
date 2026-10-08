//! The region state that the voters agree on.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use spec::Pointer;
use types::channel;
use types::digest::Digest;
use types::ed25519::PublicKey;
use types::name::{Name, Prefix};
use types::node;

use raft::Voters;
use spec::definition::Definition;

use crate::card;
use crate::change::{Change, Join, Malformed};
use crate::member::Member;
use crate::status::Status;
use crate::ticket::{self, Options, Record};

/// The region before the first entry of its log: its prefix, its founding members and
/// voters, and its spec before the first change. It is the same at each member and at
/// each open. A founding node builds it from its config. A node that joins takes it
/// whole from its join answer, and is not one of its members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Founding {
    /// The prefix of the region's names, [`Prefix::ROOT`] for the root region.
    pub prefix: Prefix,
    /// Each founding member of the region, one record for each node. A member's peer
    /// proves the public key of its card, and that key signs the member's claims.
    pub members: Vec<Member>,
    /// The voters before the first entry of the log. Each is a member. A node with no
    /// voter takes no request.
    pub voters: BTreeSet<node::Key>,
    /// The definitions of the region before the first change of its spec, by tree key.
    pub definitions: BTreeMap<Name, Definition>,
    /// The home of each index of `definitions` before the first entry of the log, by
    /// channel key to the key of a member. An index with no entry has no home until a
    /// spec change gives one.
    pub homes: BTreeMap<channel::Key, node::Key>,
}

/// The region state that this node holds: its members, its tickets, the homes that it
/// applied, and its spec pointer.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
pub(crate) struct State {
    region: Prefix,
    members: BTreeMap<node::Key, Member>,
    tickets: BTreeMap<[u8; 32], Record>,
    homes: BTreeMap<channel::Key, node::Key>,
    pointer: Pointer,
    // The voters as of the last entry applied: the last `Voters` entry, or the
    // founding voters.
    voters: Voters,
    // Each name that a member holds, its card name and each status channel name, in
    // ASCII lower case, so that two names that differ only in case collide (A3).
    names: BTreeMap<String, node::Key>,
    // Each status channel key that a member holds.
    status: BTreeSet<channel::Key>,
}

impl State {
    /// The state of the region with prefix `region`, with `members`, each under the key
    /// of its card, no ticket, `homes`, the spec at version 0 with root `root`, and the
    /// founding `voters`.
    ///
    /// # Errors
    ///
    /// [`Unfit`] when the region cannot hold a member, such as when two of `members`
    /// have one key.
    pub(crate) fn new(
        region: Prefix,
        members: Vec<Member>,
        root: Digest,
        voters: BTreeSet<node::Key>,
        homes: BTreeMap<channel::Key, node::Key>,
    ) -> Result<Self, Unfit> {
        let mut state = Self {
            region,
            members: BTreeMap::new(),
            tickets: BTreeMap::new(),
            homes,
            pointer: Pointer { version: 0, root },
            voters: Voters {
                incoming: voters,
                outgoing: BTreeSet::new(),
            },
            names: BTreeMap::new(),
            status: BTreeSet::new(),
        };
        for member in members {
            let names = state.fits(&member.card, &member.status)?;
            state.insert(member, names);
        }
        Ok(state)
    }

    /// The prefix of the region.
    pub(crate) fn prefix(&self) -> &Prefix {
        &self.region
    }

    /// The member with `key`, or `None` when the region has no such member.
    pub(crate) fn member(&self, key: node::Key) -> Option<&Member> {
        self.members.get(&key)
    }

    /// The record of the ticket with `public_key`, or `None` when none is recorded.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the join answer of #336 is the first user")
    )]
    pub(crate) fn ticket(&self, public_key: PublicKey) -> Option<&Record> {
        self.tickets.get(&public_key.to_bytes())
    }

    /// The key of the member named `name`, or `None` when no member has that name.
    pub(crate) fn named(&self, name: &Name) -> Option<node::Key> {
        let mut members = self.members.iter();
        let (&key, _) = members.find(|(_, member)| member.card.card().name == *name)?;
        Some(key)
    }

    /// The home of `index`, or `None` when none is set.
    pub(crate) fn home(&self, index: channel::Key) -> Option<node::Key> {
        self.homes.get(&index).copied()
    }

    /// The spec pointer.
    pub(crate) fn pointer(&self) -> Pointer {
        self.pointer
    }

    /// Applies `change`. Returns whether it moved a home. Every node refuses the same
    /// changes, so all keep one state.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the change does not hold against the state. The state is then
    /// as before.
    pub(crate) fn apply(&mut self, change: Change) -> Result<bool, Refused> {
        match change {
            Change::Home { index, home } => {
                Ok(self.homes.insert(index, home) != Some(home))
            }
            Change::Join(join) => self.join(*join).map(|()| false),
            Change::Ticket {
                public_key,
                options,
            } => self.record(public_key, options).map(|()| false),
            Change::Spec {
                base,
                root,
                holders,
                homes,
                ..
            } => {
                self.move_pointer(base, root, &holders)?;
                let mut moved = false;
                for (index, home) in homes {
                    if let Entry::Vacant(vacant) = self.homes.entry(index) {
                        vacant.insert(home);
                        moved = true;
                    }
                }
                Ok(moved)
            }
        }
    }

    /// Makes `voters` the voters of the entries after it: the configuration of a
    /// `Voters` entry, applied in log order.
    pub(crate) fn set_voters(&mut self, voters: Voters) {
        self.voters = voters;
    }

    // Moves the pointer by compare-and-swap on `base`, when the holders of its chunks
    // are a quorum. The chunks are never read here.
    fn move_pointer(
        &mut self,
        base: Pointer,
        root: Digest,
        holders: &BTreeSet<node::Key>,
    ) -> Result<(), Refused> {
        if base != self.pointer {
            return Err(Refused::Stale {
                base,
                pointer: self.pointer,
            });
        }
        quorum(&self.voters, holders)?;
        self.pointer = base.next(root);
        Ok(())
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
        if !self.region.contains(&options.prefix) {
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
        if !self.region.contains(name) {
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

/// Whether `holders` are a majority of each half of `voters`.
///
/// # Errors
///
/// [`Refused::Quorum`] for the first half that lacks a majority, the incoming half
/// first.
pub(crate) fn quorum(
    voters: &Voters,
    holders: &BTreeSet<node::Key>,
) -> Result<(), Refused> {
    let outgoing = (!voters.outgoing.is_empty()).then_some(&voters.outgoing);
    for half in std::iter::once(&voters.incoming).chain(outgoing) {
        let held = half.intersection(holders).count();
        // `held` is at most `half.len()`, so neither side overflows.
        if held.saturating_mul(2) <= half.len() {
            let voters = half.len();
            return Err(Refused::Quorum { held, voters });
        }
    }
    Ok(())
}

/// What a node that joins gives the voter that admits it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the join answer of #336 is the first user")
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// The public key of the ticket that admits the node.
    pub(crate) ticket: PublicKey,
    /// The node's first card, signed by the node.
    pub(crate) card: card::Unchecked,
    /// The ticket's signature over the card.
    pub(crate) admission: [u8; 64],
    /// The names of the node's status channels, under the node's name.
    pub(crate) status: BTreeSet<Name>,
}

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
        region: Prefix,
    },
    /// The base of a `Spec` change is not the spec pointer.
    Stale {
        /// The base of the change.
        base: Pointer,
        /// The spec pointer.
        pointer: Pointer,
    },
    /// The holders of the chunks of a `Spec` change are not a majority of a half of
    /// the voters as of the entry.
    Quorum {
        /// The voters of that half that hold the chunks.
        held: usize,
        /// The voters of that half.
        voters: usize,
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
            Self::Stale { base, pointer } => write!(
                f,
                "the base {base} of a spec change is not the pointer {pointer}"
            ),
            Self::Quorum { held, voters } => write!(
                f,
                "{held} of {voters} voters hold the chunks of a spec change, not a \
                 majority"
            ),
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
pub enum Unfit {
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
        region: Prefix,
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

    use super::*;
    use crate::common::{
        EPHEMERAL, EXPIRY, create_members, digest, home, index, join, key as node,
        name, options, public, record, signed, spec, status, with_status,
    };

    const FOUNDING: Digest = Digest([1; 32]);

    // Members 1 and 2, and single-use ticket 7 for `plant.edge`.
    fn state() -> State {
        let mut state = State::new(
            name("plant").into(),
            create_members(&[1, 2]),
            FOUNDING,
            [node(1)].into(),
            BTreeMap::new(),
        )
        .unwrap();
        let recorded = state.apply(record(7, options("plant.edge", false)));
        assert_eq!(recorded, Ok(false));
        state
    }

    #[test]
    fn a_home_is_none_until_a_change_sets_it() {
        let mut state = state();
        assert_eq!(state.home(index(7)), None);
        assert_eq!(state.apply(home(7, 1)), Ok(true));
        assert_eq!(state.home(index(7)), Some(node(1)));
        assert_eq!(state.home(index(8)), None);
    }

    #[test]
    fn a_change_to_the_same_home_moves_nothing() {
        let mut state = state();
        assert_eq!(state.apply(home(7, 1)), Ok(true));
        assert_eq!(state.apply(home(7, 1)), Ok(false));
        assert_eq!(state.apply(home(7, 2)), Ok(true));
        assert_eq!(state.home(index(7)), Some(node(2)));
    }

    #[test]
    fn a_spec_change_on_the_pointer_moves_it_to_the_next_version() {
        let mut state = state();
        let founding = Pointer {
            version: 0,
            root: FOUNDING,
        };
        assert_eq!(state.pointer(), founding);
        assert_eq!(state.apply(spec(0, 1, 2, &[3])), Ok(false));
        let moved = Pointer {
            version: 1,
            root: digest(2),
        };
        assert_eq!(state.pointer(), moved);
        assert_eq!(state.apply(spec(1, 2, 2, &[])), Ok(false));
        assert_eq!(state.pointer().version, 2);
    }

    /// `spec` with the home `[(index, home)]` of each pair of `homes`.
    fn homed(change: Change, homes: &[(u128, u8)]) -> Change {
        let Change::Spec {
            base,
            root,
            chunks,
            holders,
            ..
        } = change
        else {
            unreachable!()
        };
        let homes = homes.iter().map(|&(i, h)| (index(i), node(h))).collect();
        Change::Spec {
            base,
            root,
            chunks,
            holders,
            homes,
        }
    }

    #[test]
    fn a_spec_change_gives_a_home_only_to_an_index_that_has_none() {
        let mut state = state();
        assert_eq!(state.apply(home(7, 1)), Ok(true));
        let first = homed(spec(0, 1, 2, &[]), &[(7, 2), (8, 2)]);
        assert_eq!(state.apply(first), Ok(true));
        assert_eq!(state.home(index(7)), Some(node(1)));
        assert_eq!(state.home(index(8)), Some(node(2)));
        let second = homed(spec(1, 2, 3, &[]), &[(7, 2), (8, 1)]);
        assert_eq!(state.apply(second), Ok(false));
        assert_eq!(state.pointer().version, 2);
        assert_eq!(state.home(index(7)), Some(node(1)));
        assert_eq!(state.home(index(8)), Some(node(2)));
    }

    #[test]
    fn named_gives_the_key_of_the_member_with_the_name() {
        let state = state();
        assert_eq!(state.named(&name("plant.node2")), Some(node(2)));
        assert_eq!(state.named(&name("plant.node3")), None);
        assert_eq!(state.named(&name("plant")), None);
    }

    fn keys<'a>(ids: impl IntoIterator<Item = &'a u8>) -> BTreeSet<node::Key> {
        ids.into_iter().map(|&id| node(id)).collect()
    }

    fn voters(incoming: &[u8], outgoing: &[u8]) -> Voters {
        Voters {
            incoming: keys(incoming),
            outgoing: keys(outgoing),
        }
    }

    #[test]
    fn holders_are_a_quorum_only_as_a_majority_of_each_half() {
        let short = |held, voters| Err(Refused::Quorum { held, voters });
        let cases = [
            (voters(&[1], &[]), keys(&[1]), Ok(())),
            (voters(&[1], &[]), keys(&[]), short(0, 1)),
            (voters(&[1, 2], &[]), keys(&[1]), short(1, 2)),
            (voters(&[1, 2], &[]), keys(&[1, 2]), Ok(())),
            (voters(&[1, 2, 3], &[]), keys(&[2, 3]), Ok(())),
            (voters(&[1, 2, 3], &[]), keys(&[3, 4]), short(1, 3)),
            (voters(&[1, 2, 3], &[1]), keys(&[2, 3]), short(0, 1)),
            (voters(&[1, 2], &[1, 2, 3]), keys(&[1]), short(1, 2)),
            (voters(&[1, 2, 3], &[1, 2]), keys(&[1, 2]), Ok(())),
        ];
        for (voters, holders, expected) in cases {
            let quorum = quorum(&voters, &holders);
            assert_eq!(quorum, expected, "{voters:?} {holders:?}");
        }
        assert_eq!(
            Refused::Quorum { held: 1, voters: 2 }.to_string(),
            "1 of 2 voters hold the chunks of a spec change, not a majority"
        );
    }

    // The voters of the last `Voters` entry count for each change after it.
    #[test]
    fn a_spec_change_is_refused_when_its_holders_are_not_a_quorum_of_the_voters() {
        let mut state = state();
        state.set_voters(voters(&[1, 2], &[1]));
        let short = Refused::Quorum { held: 1, voters: 2 };
        let before = state.clone();
        assert_eq!(
            state.apply(homed(spec(0, 1, 2, &[]), &[(7, 1)])),
            Err(short)
        );
        assert_eq!(state, before);
        let mut held = spec(0, 1, 2, &[]);
        let Change::Spec { holders, .. } = &mut held else {
            unreachable!()
        };
        *holders = keys(&[1, 2]);
        assert_eq!(state.apply(held), Ok(false));
        assert_eq!(state.pointer().version, 1);
    }

    // Of two changes from one base, the first applies and the second is refused.
    #[test]
    fn a_spec_change_on_a_stale_base_is_refused() {
        let mut state = state();
        assert_eq!(state.apply(spec(0, 1, 2, &[])), Ok(false));
        let before = state.clone();
        let moved = state.pointer();
        let cases = [
            (homed(spec(0, 1, 3, &[]), &[(7, 1)]), 0, digest(1)),
            (spec(1, 1, 3, &[]), 1, digest(1)),
            (spec(0, 2, 3, &[]), 0, digest(2)),
            (spec(2, 2, 3, &[]), 2, digest(2)),
        ];
        for (change, version, root) in cases {
            assert_eq!(
                state.apply(change),
                Err(Refused::Stale {
                    base: Pointer { version, root },
                    pointer: moved,
                })
            );
            assert_eq!(state, before);
        }
    }

    #[test]
    fn new_refuses_two_members_with_one_key() {
        let error = State::new(
            name("plant").into(),
            create_members(&[1, 2, 1]),
            FOUNDING,
            [node(1)].into(),
            BTreeMap::new(),
        )
        .unwrap_err();
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
            State::new(
                name("plant").into(),
                reserved,
                FOUNDING,
                [node(1)].into(),
                BTreeMap::new()
            ),
            Err(Unfit::Reserved {
                name: name("plant.@changes")
            })
        );
        let mut outside = create_members(&[1, 2]);
        outside[1].card = signed(2, "factory.node2");
        assert_eq!(
            State::new(
                name("plant").into(),
                outside,
                FOUNDING,
                [node(1)].into(),
                BTreeMap::new()
            ),
            Err(Unfit::Outside {
                name: name("factory.node2"),
                region: name("plant").into()
            })
        );
        let mut long = create_members(&[1, 2]);
        long[1].status = status([(long_status(256 - 12), index(1))]);
        assert_eq!(
            State::new(
                name("plant").into(),
                long,
                FOUNDING,
                [node(1)].into(),
                BTreeMap::new()
            )
            .unwrap_err(),
            Unfit::Long {
                name: name("plant.node2"),
                status: long_status(256 - 12),
            }
        );
    }

    #[test]
    fn the_root_region_holds_a_member_and_a_ticket_under_any_prefix() {
        let mut members = create_members(&[1, 2]);
        members[1].card = signed(2, "factory.node2");
        let mut state = State::new(
            Prefix::ROOT,
            members,
            FOUNDING,
            [node(1)].into(),
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(state.apply(record(8, options("site_a", false))), Ok(false));
        let join = join(8, 3, "site_a.pt_1");
        assert_eq!(state.apply(Change::Join(Box::new(join))), Ok(false));
        let names = [1, 2, 3]
            .map(|id| state.member(node(id)).map(|m| m.card.card().name.clone()));
        assert_eq!(
            names,
            [
                Some(name("plant.node1")),
                Some(name("factory.node2")),
                Some(name("site_a.pt_1"))
            ]
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
                region: name("plant").into()
            })
        );
        assert_eq!(state, before);
        assert_eq!(state.apply(record(8, options("plant", false))), Ok(false));
    }

    #[test]
    fn a_join_admits_the_node_with_the_ticket_options() {
        let mut state = state();
        let join = join(7, 3, "plant.edge.a");
        let admission = join.admission;
        assert_eq!(state.apply(Change::Join(Box::new(join))), Ok(false));
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
        assert_eq!(admitted, Ok(false));
    }

    // A status name of `len` bytes.
    fn long_status(len: usize) -> Name {
        name(&"s".repeat(len))
    }

    fn apply_join(state: &mut State, join: Join) -> Result<bool, Refused> {
        state.apply(Change::Join(Box::new(join)))
    }

    #[test]
    fn a_join_with_a_reserved_name_is_refused_and_counts_no_use() {
        let mut state = state();
        assert_eq!(state.apply(record(8, options("plant", false))), Ok(false));
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
        assert_eq!(state.apply(record(8, options("plant", true))), Ok(false));
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
        assert_eq!(apply_join(&mut state, join(8, 3, "plant")), Ok(false));
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
                region: name("plant").into()
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
        assert_eq!(apply_join(&mut state, longest), Ok(false));
    }

    // Member 1 is `plant.node1`. Ticket 8 admits any node under `plant`.
    fn open_state() -> State {
        let mut state = state();
        assert_eq!(state.apply(record(8, options("plant", true))), Ok(false));
        state
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
        assert_eq!(
            apply_join(&mut state, join(8, 3, "plant.edge.a")),
            Ok(false)
        );
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
        assert_eq!(apply_join(&mut state, other), Ok(false));
        assert_eq!(state.ticket(public(7)).map(|record| record.uses), Some(1));
    }

    #[test]
    fn a_join_whose_status_channel_name_a_member_holds_is_refused() {
        let mut state = open_state();
        let first = with_status(join(8, 3, "plant.a"), &[("b.c", 20)]);
        assert_eq!(apply_join(&mut state, first), Ok(false));
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
        assert_eq!(apply_join(&mut state, other), Ok(false));
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
        assert_eq!(apply_join(&mut state, first), Ok(false));
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
            State::new(
                name("plant").into(),
                taken,
                FOUNDING,
                [node(1)].into(),
                BTreeMap::new()
            ),
            Err(Unfit::Taken {
                name: name("plant.NODE1"),
                key: node(1)
            })
        );
        let mut reused = create_members(&[1, 2]);
        reused[0].status = status([(name("disk"), index(20))]);
        reused[1].status = status([(name("disk"), index(20))]);
        assert_eq!(
            State::new(
                name("plant").into(),
                reused,
                FOUNDING,
                [node(1)].into(),
                BTreeMap::new()
            ),
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
                    region: name("plant").into(),
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
                    region: name("plant").into(),
                },
                "the prefix plants.edge is not under the region plant".to_owned(),
            ),
            (
                Refused::Stale {
                    base: Pointer {
                        version: 0,
                        root: digest(0xab),
                    },
                    pointer: Pointer {
                        version: 1,
                        root: digest(0xcd),
                    },
                },
                format!(
                    "the base version 0, root {} of a spec change is not the pointer \
                     version 1, root {}",
                    "ab".repeat(32),
                    "cd".repeat(32)
                ),
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
                    region: name("plant").into(),
                },
                "the name plants.edge is not under the region plant".to_owned(),
            ),
        ];
        for (unfit, text) in cases {
            assert_eq!(unfit.to_string(), text);
            assert_eq!(Refused::Unfit(unfit).to_string(), text);
        }
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
        let homes = prop::collection::btree_map(0..3_u128, 1..3_u8, 0..3);
        let specs = (0..3_u64, 1..3_u8, 1..4_u8, homes).prop_map(
            |(version, base, root, homes)| {
                let homes: Vec<_> = homes.into_iter().collect();
                homed(spec(version, base, root, &[]), &homes)
            },
        );
        prop_oneof![joins, tickets, specs]
    }

    proptest! {
        // A refused change leaves the state as it was, and an applied join adds its
        // member.
        #[test]
        fn a_refused_change_changes_nothing(
            steps in prop::collection::vec(steps(), 0..16),
        ) {
            let members = create_members(&[1, 2]);
            let voters = keys(&[1]);
            let mut state =
                State::new(name("plant").into(), members, FOUNDING, voters, BTreeMap::new()).unwrap();
            for step in steps {
                let before = state.clone();
                let applied = state.apply(step.clone());
                match (applied, step) {
                    (Err(_), _) => prop_assert_eq!(&state, &before),
                    (Ok(_), Change::Join(join)) => {
                        prop_assert!(before.member(join.card.key).is_none());
                        prop_assert!(state.member(join.card.key).is_some());
                    }
                    (Ok(moved), Change::Spec { base, root, homes, .. }) => {
                        prop_assert_eq!(before.pointer(), base);
                        let next = base.version.checked_add(1).unwrap();
                        prop_assert_eq!(state.pointer(), Pointer { version: next, root });
                        for (&index, &home) in &homes {
                            let kept = before.home(index).unwrap_or(home);
                            prop_assert_eq!(state.home(index), Some(kept));
                        }
                        let given = homes.keys().any(|&i| before.home(i).is_none());
                        prop_assert_eq!(moved, given);
                    }
                    (Ok(_), _) => {}
                }
            }
        }

        // More holders never lose a quorum, and every voter is one.
        #[test]
        fn more_holders_keep_a_quorum(
            incoming in prop::collection::btree_set(0..6_u8, 1..5),
            outgoing in prop::collection::btree_set(0..6_u8, 0..5),
            holders in prop::collection::btree_set(0..6_u8, 0..6),
            more in prop::collection::btree_set(0..6_u8, 0..6),
        ) {
            let voters = Voters {
                incoming: keys(&incoming),
                outgoing: keys(&outgoing),
            };
            let all = keys(holders.union(&more));
            if quorum(&voters, &keys(&holders)).is_ok() {
                prop_assert_eq!(quorum(&voters, &all), Ok(()));
            }
            prop_assert_eq!(quorum(&voters, &keys(incoming.union(&outgoing))), Ok(()));
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
                prop_assert_eq!(moved, Ok(before != Some(h)));
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
