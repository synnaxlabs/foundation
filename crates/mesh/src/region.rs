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

use raft::{NoMajority, Voters};
use spec::definition::Definition;

use crate::bytes::{
    put_channel, put_count, put_key, put_keys, put_name, put_prefix, put_public_key,
    take, take_channel, take_count, take_key, take_keys, take_name, take_prefix,
    take_public_key, take_rising,
};
use crate::card;
use crate::change::{self, Change, Join, Undecoded};
use crate::member::Member;
use crate::status::{self, Status};
use crate::ticket::{self, Options, Record};

/// The region before the first entry of its log: its prefix, its founding members and
/// voters, its spec before the first change, and the home of each founding index that
/// it holds. It is the same at each member and at each open. A founding node builds it
/// from its config. A node that joins takes it whole from its join answer, and is not
/// one of its members.
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

impl Founding {
    /// The byte form: the prefix, the members in key order, the voters, the
    /// definitions in name order, and the homes in channel key order. Two values that
    /// differ only in the order of their members have one byte form.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_prefix(&self.prefix, &mut out);
        let mut members: Vec<&Member> = self.members.iter().collect();
        members.sort_by_key(|member| member.card.key());
        put_count(members.len(), &mut out);
        for member in members {
            member.encode(&mut out);
        }
        put_keys(&self.voters, &mut out);
        put_count(self.definitions.len(), &mut out);
        for (name, definition) in &self.definitions {
            put_name(name, &mut out);
            let definition = definition.encode();
            put_count(definition.len(), &mut out);
            out.extend(definition);
        }
        put_count(self.homes.len(), &mut out);
        for (&index, &home) in &self.homes {
            put_channel(index, &mut out);
            put_key(home, &mut out);
        }
        out
    }

    /// Reads what [`Founding::encode`] gives, with the members in key order. `None`
    /// when `bytes` are not that form, also with bytes after it.
    pub(crate) fn decode(mut bytes: &[u8]) -> Option<Self> {
        let bytes = &mut bytes;
        let prefix = take_prefix(bytes)?;
        let mut members: Vec<Member> = Vec::new();
        for _ in 0..take_count(bytes)? {
            let member = Member::decode(bytes)?;
            if members
                .last()
                .is_some_and(|last| last.card.key() >= member.card.key())
            {
                return None;
            }
            members.push(member);
        }
        let voters = take_keys(bytes)?;
        let mut definitions = BTreeMap::new();
        take_rising(bytes, take_name, |name, bytes| {
            let len = usize::try_from(take_count(bytes)?).ok()?;
            let (definition, rest) = bytes.split_at_checked(len)?;
            *bytes = rest;
            definitions.insert(name, Definition::decode(definition).ok()?);
            Some(())
        })?;
        let mut homes = BTreeMap::new();
        take_rising(bytes, take_channel, |index, bytes| {
            homes.insert(index, take_key(bytes)?);
            Some(())
        })?;
        bytes.is_empty().then_some(Self {
            prefix,
            members,
            voters,
            definitions,
            homes,
        })
    }
}

/// The region state that this node holds: its members, its tickets, the founding homes
/// and the homes that it applied, and its spec pointer.
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
    // The member that holds each public key, by its bytes. A `Card` change keeps the
    // public key, and no change removes a member, so only `insert` writes this.
    public_keys: BTreeMap<[u8; 32], node::Key>,
    // The public key of the first `Join` of each node key, admitted or refused.
    joined: BTreeMap<node::Key, PublicKey>,
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
            public_keys: BTreeMap::new(),
            joined: BTreeMap::new(),
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

    /// The name of each member.
    pub(crate) fn names(&self) -> BTreeSet<Name> {
        self.members
            .values()
            .map(|member| member.card.card().name.clone())
            .collect()
    }

    /// The key of the member whose card holds `public_key`, or `None` when none does.
    pub(crate) fn holder(&self, public_key: PublicKey) -> Option<node::Key> {
        self.public_keys.get(&public_key.to_bytes()).copied()
    }

    /// The public key of the first applied `Join` of `key`, admitted or refused, or
    /// `None` when no join of `key` applied. A founding member has no join.
    pub(crate) fn joined(&self, key: node::Key) -> Option<PublicKey> {
        self.joined.get(&key).copied()
    }

    /// The record of the ticket with `public_key`, or `None` when none is recorded.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a later PR of #336 is the first user")
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

    /// The node of each home, in index key order.
    pub(crate) fn homes(&self) -> impl Iterator<Item = node::Key> {
        self.homes.values().copied()
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
    /// as before, but a refused `Join` still binds its node key ([`State::joined`]).
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
                self.swap_pointer(base, root, &holders, &homes)?;
                let mut moved = false;
                for (index, home) in homes {
                    if let Entry::Vacant(vacant) = self.homes.entry(index) {
                        vacant.insert(home);
                        moved = true;
                    }
                }
                Ok(moved)
            }
            Change::Card(card) => self.replace(*card).map(|()| false),
        }
    }

    /// Makes `voters` the voters of the entries after it: the configuration of a
    /// `Voters` entry, applied in log order.
    pub(crate) fn set_voters(&mut self, voters: Voters) {
        self.voters = voters;
    }

    // Compares `base` with the pointer and, when the holders of its chunks are a
    // quorum, sets the pointer that the change makes. The chunks are never read
    // here.
    fn swap_pointer(
        &mut self,
        base: Pointer,
        root: Digest,
        holders: &BTreeSet<node::Key>,
        homes: &BTreeMap<channel::Key, node::Key>,
    ) -> Result<(), Refused> {
        if base != self.pointer {
            return Err(Refused::Stale {
                base,
                pointer: self.pointer,
            });
        }
        self.voters.quorum(|key| holders.contains(&key))?;
        self.pointer = change::pointer(base, root, homes);
        Ok(())
    }

    /// Checks `join` as [`State::apply`] does, with no change to the state.
    ///
    /// # Errors
    ///
    /// The [`Refused`] that [`State::apply`] gives for `join`.
    pub(crate) fn check(&self, join: &Join) -> Result<(), Refused> {
        self.admitted(join).map(drop)
    }

    // Binds the node key of `join`, then admits its node. The ticket counts a use only
    // when all checks pass.
    fn join(&mut self, join: Join) -> Result<(), Refused> {
        let card = &join.card;
        self.joined.entry(card.key).or_insert(card.card.public_key);
        let (card, names) = self.admitted(&join)?;
        let record = self
            .tickets
            .get_mut(&join.ticket.to_bytes())
            .expect("invariant: admitted checks that the ticket is recorded");
        record.count();
        let member = Member {
            card,
            admission: join.admission,
            ephemeral: record.ephemeral(),
            status: join.status,
        };
        self.insert(member, names);
        Ok(())
    }

    // The checked card of `join` and its names from `fits`, when the region admits
    // it. The admission comes before `fits`, so a refusal names a key or a name of
    // the region only to a peer with a good admission. The use comes after it, so a
    // repeat of an applied join gets `Unfit::Duplicate`.
    fn admitted(&self, join: &Join) -> Result<(card::Signed, Vec<String>), Refused> {
        let card = join.card.clone().check().map_err(Refused::Forged)?;
        let record =
            self.tickets
                .get(&join.ticket.to_bytes())
                .ok_or(Refused::Unknown {
                    public_key: join.ticket,
                })?;
        record
            .check(&card, &join.admission, join.at)
            .map_err(Refused::Ticket)?;
        let names = self.fits(&card, &join.status)?;
        record.unused().map_err(Refused::Ticket)?;
        Ok((card, names))
    }

    // Makes `card` the card of its member. Only the addresses and the version may
    // change, so the names and the public key that `fits` checked still hold.
    fn replace(&mut self, card: card::Unchecked) -> Result<(), Refused> {
        let key = card.key;
        let member = self
            .members
            .get_mut(&key)
            .ok_or(Refused::Stranger { key })?;
        let held = member.card.card();
        card.verify(held.public_key).map_err(Refused::Forged)?;
        // Each field is named, so a new field of a card is not changeable by default.
        let card::Card {
            name,
            public_key,
            seal_key,
            addresses: _,
            version,
        } = &card.card;
        if (name, *public_key, seal_key)
            != (&held.name, held.public_key, &held.seal_key)
        {
            return Err(Refused::Changed { key });
        }
        if *version <= held.version {
            return Err(Refused::Behind {
                key,
                version: *version,
                held: held.version,
            });
        }
        // The card holds the key of its member, which signed it, so this holds.
        member.card = card.check().map_err(Refused::Forged)?;
        Ok(())
    }

    // Adds `member`, whose names `fits` gave.
    fn insert(&mut self, member: Member, names: Vec<String>) {
        let key = member.card.key();
        self.names.extend(names.into_iter().map(|name| (name, key)));
        self.status.extend(member.status.as_map().values());
        self.public_keys.insert(member.public_key().to_bytes(), key);
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
        if let Some(key) = self.holder(card.card().public_key) {
            return Err(Unfit::Held { key });
        }
        let mut lower = Vec::with_capacity(names.len());
        for name in names {
            let folded = name.as_str().to_ascii_lowercase();
            if let Some(&taker) = self.names.get(&folded) {
                return Err(Unfit::Taken { name, key: taker });
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

/// What a node that joins gives the voter that admits it.
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

impl Request {
    /// Adds the byte form: the ticket's public key, the card, the admission, then the
    /// count of status names and each name in rising order.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_public_key(self.ticket, out);
        self.card.encode(out);
        out.extend(self.admission);
        put_count(self.status.len(), out);
        for name in &self.status {
            put_name(name, out);
        }
    }

    /// Takes what [`Request::encode`] gives. `None` when the count of status names is
    /// over 64, before any name is read, or when the names are not in rising order.
    pub(crate) fn decode(bytes: &mut &[u8]) -> Option<Self> {
        let ticket = take_public_key(bytes)?;
        let card = card::Unchecked::decode(bytes)?;
        let admission = take(bytes)?;
        let mut count = *bytes;
        if take_count(&mut count)? > u64::from(status::MAX) {
            return None;
        }
        let mut names = BTreeSet::new();
        take_rising(bytes, take_name, |name, _| {
            names.insert(name);
            Some(())
        })?;
        Some(Self {
            ticket,
            card,
            admission,
            status: names,
        })
    }
}

/// Why every node refuses a change. A refused change changes no state, but a refused
/// `Join` still binds its node key (`State::joined`).
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
    /// as [`change::Malformed::Body`].
    Body(Undecoded),
    /// No member has the key of the card of a `Card` change.
    Stranger {
        /// The key of the card.
        key: node::Key,
    },
    /// The card of a `Card` change has another name, public key, or seal key than the
    /// card of its member.
    Changed {
        /// The member.
        key: node::Key,
    },
    /// The version of the card of a `Card` change is not above the version of the
    /// card of its member.
    Behind {
        /// The member.
        key: node::Key,
        /// The version of the change's card.
        version: u64,
        /// The version of the member's card.
        held: u64,
    },
}

impl From<NoMajority> for Refused {
    fn from(short: NoMajority) -> Self {
        Self::Quorum {
            held: short.counted,
            voters: short.voters,
        }
    }
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
            Self::Body(undecoded) => undecoded.fmt(f),
            Self::Stranger { key } => write!(f, "node {key} is not a member"),
            Self::Changed { key } => write!(
                f,
                "the card of node {key} changes more than its addresses and its version"
            ),
            Self::Behind { key, version, held } => write!(
                f,
                "card version {version} of node {key} is not above the held version \
                 {held}"
            ),
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
    /// Another member's card holds the public key of the member's card.
    Held {
        /// The member that holds it.
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
            Self::Held { key } => {
                write!(f, "the public key of the card is held by node {key}")
            }
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
    use transport::Address;
    use types::node::SealKey;

    use super::*;
    use crate::common::{
        EPHEMERAL, EXPIRY, create_members, digest, home, index, join, join_with,
        key as node, member, name, options, private, public, signed, spec, status,
        ticket, ticket_change, unchecked, with_status,
    };

    const FOUNDING: Digest = Digest([1; 32]);

    // The state of `members` in the region `prefix`, with voter 1, the spec
    // `FOUNDING`, and no home.
    fn create_state(prefix: Prefix, members: Vec<Member>) -> Result<State, Unfit> {
        State::new(prefix, members, FOUNDING, [node(1)].into(), BTreeMap::new())
    }

    // The card of node `id` with `name`, which holds the public key of node `holder`
    // and which `holder` signed.
    fn holding(id: u8, holder: u8, name: &str) -> card::Signed {
        let card = card::Card {
            public_key: public(holder),
            ..signed(id, name).card().clone()
        };
        card::Signed::sign(node(id), card, &private(holder))
    }

    // Members 1 and 2, and single-use ticket 7 for `plant.edge`.
    fn state() -> State {
        let mut state =
            create_state(name("plant").into(), create_members(&[1, 2])).unwrap();
        let recorded = state.apply(ticket_change(7, options("plant.edge", false)));
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
        assert_eq!(state.apply(homed(spec(1, 2, 2, &[]), &[(7, 1)])), Ok(true));
        assert_eq!(state.pointer(), moved.next(digest(2)));
    }

    #[test]
    fn a_spec_change_at_the_base_root_with_no_home_leaves_the_pointer() {
        let mut state = state();
        assert_eq!(state.apply(spec(0, 1, 2, &[3])), Ok(false));
        let moved = state.pointer();
        assert_eq!(state.apply(spec(1, 2, 2, &[])), Ok(false));
        assert_eq!(state.pointer(), moved);
        let refused = Refused::Stale {
            base: Pointer {
                version: 0,
                root: FOUNDING,
            },
            pointer: moved,
        };
        assert_eq!(state.apply(spec(0, 1, 1, &[])), Err(refused));
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
    fn a_spec_change_that_lists_a_home_moves_the_version_also_when_it_gives_none() {
        let mut state = state();
        assert_eq!(state.apply(homed(spec(0, 1, 2, &[]), &[(7, 2)])), Ok(true));
        let moved = state.pointer();
        assert_eq!(state.apply(homed(spec(1, 2, 2, &[]), &[(7, 1)])), Ok(false));
        assert_eq!(state.home(index(7)), Some(node(2)));
        assert_eq!(state.pointer(), moved.next(digest(2)));
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
            (voters(&[], &[]), keys(&[]), Ok(())),
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
            let mut state = state();
            state.set_voters(voters.clone());
            let mut change = spec(0, 1, 2, &[]);
            let Change::Spec { holders: held, .. } = &mut change else {
                unreachable!()
            };
            *held = holders.clone();
            let applied = state.apply(change).map(drop);
            assert_eq!(applied, expected, "{voters:?} {holders:?}");
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
    fn holder_gives_the_member_with_a_public_key() {
        let state = state();
        assert_eq!(state.holder(public(2)), Some(node(2)));
        assert_eq!(state.holder(public(3)), None);
    }

    #[test]
    fn new_refuses_two_members_with_one_public_key() {
        let twin = Member {
            card: holding(3, 1, "plant.node3"),
            ..member(3)
        };
        let error =
            create_state(name("plant").into(), vec![member(1), twin]).unwrap_err();
        assert_eq!(error, Unfit::Held { key: node(1) });
    }

    #[test]
    fn new_refuses_two_members_with_one_key() {
        let error =
            create_state(name("plant").into(), create_members(&[1, 2, 1])).unwrap_err();
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
            create_state(name("plant").into(), reserved),
            Err(Unfit::Reserved {
                name: name("plant.@changes")
            })
        );
        let mut outside = create_members(&[1, 2]);
        outside[1].card = signed(2, "factory.node2");
        assert_eq!(
            create_state(name("plant").into(), outside),
            Err(Unfit::Outside {
                name: name("factory.node2"),
                region: name("plant").into()
            })
        );
        let mut far = create_members(&[1, 2]);
        far[1].card = signed(2, "factory.node2");
        far[1].status = status([(long_status(256 - 14), index(1))]);
        assert_eq!(
            create_state(name("plant").into(), far),
            Err(Unfit::Outside {
                name: name("factory.node2"),
                region: name("plant").into()
            })
        );
        let mut long = create_members(&[1, 2]);
        long[1].status = status([(long_status(256 - 12), index(1))]);
        assert_eq!(
            create_state(name("plant").into(), long).unwrap_err(),
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
        let mut state = create_state(Prefix::ROOT, members).unwrap();
        assert_eq!(
            state.apply(ticket_change(8, options("site_a", false))),
            Ok(false)
        );
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
            state.apply(ticket_change(7, options("plant", true))),
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
            state.apply(ticket_change(8, options("plants.edge", false))),
            Err(Refused::Outside {
                prefix: name("plants.edge"),
                region: name("plant").into()
            })
        );
        assert_eq!(state, before);
        assert_eq!(
            state.apply(ticket_change(8, options("plant", false))),
            Ok(false)
        );
    }

    // The card of member `id`, at `version`, with the one address `10.0.0.<id>:4100`.
    fn moved(id: u8, version: u64) -> card::Card {
        let at = std::net::SocketAddr::from(([10, 0, 0, id], 4100));
        card::Card {
            addresses: card::addresses::Addresses::new(vec![Address::Udp(at)]).unwrap(),
            version,
            ..member(id).card.card().clone()
        }
    }

    // A `Card` change of `card` for node `key`, which the private key of node `signer`
    // signed, with its signature bits flipped when `forged`. The key of `signer` need
    // not be the key of the card.
    fn card_change(key: u8, card: card::Card, signer: u8, forged: bool) -> Change {
        let mut card = card::Unchecked::sign(node(key), card, &private(signer));
        if forged {
            card.signature[0] ^= 1;
        }
        Change::Card(Box::new(card))
    }

    #[test]
    fn a_card_change_replaces_the_card_of_its_member() {
        let mut state = state();
        let change = card_change(2, moved(2, 5), 2, false);
        let Change::Card(card) = change.clone() else {
            unreachable!()
        };
        let mut members = create_members(&[1, 2]);
        members[1].card = card.check().unwrap();
        let mut expected = create_state(name("plant").into(), members).unwrap();
        let recorded = expected.apply(ticket_change(7, options("plant.edge", false)));
        assert_eq!(recorded, Ok(false));
        assert_eq!(state.apply(change), Ok(false));
        assert_eq!(state, expected);
        assert_eq!(state.named(&name("plant.node2")), Some(node(2)));
        assert_eq!(state.holder(public(2)), Some(node(2)));
    }

    #[test]
    fn a_repeat_of_a_card_change_is_refused_and_changes_nothing() {
        let mut state = state();
        let change = card_change(2, moved(2, 2), 2, false);
        assert_eq!(state.apply(change.clone()), Ok(false));
        let before = state.clone();
        let behind = Refused::Behind {
            key: node(2),
            version: 2,
            held: 2,
        };
        assert_eq!(state.apply(change), Err(behind));
        assert_eq!(state, before);
    }

    #[test]
    fn a_card_change_whose_version_is_not_above_is_refused() {
        let mut state = state();
        assert_eq!(
            state.apply(card_change(2, moved(2, 5), 2, false)),
            Ok(false)
        );
        let before = state.clone();
        for version in [0, 1, 4, 5] {
            let behind = Refused::Behind {
                key: node(2),
                version,
                held: 5,
            };
            let change = card_change(2, moved(2, version), 2, false);
            assert_eq!(state.apply(change), Err(behind));
            assert_eq!(state, before);
        }
        assert_eq!(
            state.apply(card_change(2, moved(2, 6), 2, false)),
            Ok(false)
        );
    }

    #[test]
    fn a_card_change_of_a_node_that_is_not_a_member_is_refused() {
        let mut state = state();
        let before = state.clone();
        let stranger = Refused::Stranger { key: node(3) };
        let change = card_change(3, moved(3, 2), 3, false);
        assert_eq!(state.apply(change), Err(stranger));
        assert_eq!(state, before);
    }

    #[test]
    fn a_card_change_with_a_forged_card_is_refused() {
        let mut state = state();
        let before = state.clone();
        let forged = Refused::Forged(card::Forged { node: node(2) });
        let change = card_change(2, moved(2, 2), 2, true);
        assert_eq!(state.apply(change), Err(forged));
        assert_eq!(state, before);
    }

    #[test]
    fn a_card_change_of_more_than_the_addresses_is_refused() {
        let mut state = state();
        let before = state.clone();
        let renamed = card::Card {
            name: name("plant.node9"),
            ..moved(2, 2)
        };
        let rekeyed = card::Card {
            public_key: public(9),
            ..moved(2, 2)
        };
        let resealed = card::Card {
            seal_key: SealKey::new([8; 32]).unwrap(),
            ..moved(2, 2)
        };
        let changed = Refused::Changed { key: node(2) };
        for card in [renamed, rekeyed, resealed] {
            let change = card_change(2, card, 2, false);
            assert_eq!(state.apply(change), Err(changed.clone()));
            assert_eq!(state, before);
        }
    }

    /// The signature holds with the public key of the member, not with the key that
    /// the card holds.
    #[test]
    fn a_card_that_its_new_public_key_signed_is_forged() {
        let mut state = state();
        let before = state.clone();
        let rekeyed = card::Card {
            public_key: public(9),
            ..moved(2, 2)
        };
        let forged = Refused::Forged(card::Forged { node: node(2) });
        assert_eq!(state.apply(card_change(2, rekeyed, 9, false)), Err(forged));
        assert_eq!(state, before);
    }

    #[test]
    fn a_card_refusal_names_the_first_check_that_fails() {
        let mut state = state();
        let stranger = Refused::Stranger { key: node(3) };
        assert_eq!(
            state.apply(card_change(3, moved(3, 2), 3, true)),
            Err(stranger)
        );
        let renamed = card::Card {
            name: name("plant.node9"),
            ..moved(2, 2)
        };
        let forged = Refused::Forged(card::Forged { node: node(2) });
        let change = card_change(2, renamed.clone(), 2, true);
        assert_eq!(state.apply(change), Err(forged));
        let old = card::Card {
            version: 1,
            ..renamed
        };
        let changed = Refused::Changed { key: node(2) };
        assert_eq!(state.apply(card_change(2, old, 2, false)), Err(changed));
    }

    #[test]
    fn a_join_admits_the_node_with_the_ticket_options() {
        let mut state = state();
        let first = join(7, 3, "plant.edge.a");
        let admission = first.admission;
        assert_eq!(apply_join(&mut state, first), Ok(false));
        let expected = Member {
            card: signed(3, "plant.edge.a"),
            admission,
            ephemeral: Some(EPHEMERAL),
            status: status([(name("disk"), index(9))]),
        };
        assert_eq!(state.member(node(3)), Some(&expected));
        assert_eq!(
            apply_join(
                &mut state,
                with_status(join(7, 4, "plant.edge.b"), &[("disk", 10)])
            ),
            Err(Refused::Ticket(ticket::Refused::Used {
                public_key: public(7)
            }))
        );
    }

    #[test]
    fn a_join_with_a_forged_card_is_refused() {
        let mut state = state();
        let before = bound(state.clone(), 3, 3);
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
        let before = bound(state.clone(), 3, 3);
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
        let before = bound(state.clone(), 3, 3);
        assert_eq!(
            state.apply(Change::Join(Box::new(join(7, 3, "plant.edger")))),
            Err(Refused::Ticket(ticket::Refused::Scope {
                name: name("plant.edger"),
                prefix: name("plant.edge")
            }))
        );
        assert_eq!(state, before);
    }

    #[test]
    fn a_join_with_the_public_key_of_a_member_is_refused() {
        let mut state = state();
        let before = bound(state.clone(), 3, 1);
        let card = holding(3, 1, "plant.edge.a");
        let mut join = join(7, 3, "plant.edge.a");
        join.admission = ticket(7).admission(&card);
        join.card = unchecked(&card);
        let held = Unfit::Held { key: node(1) };
        assert_eq!(
            held.to_string(),
            format!("the public key of the card is held by node {}", node(1))
        );
        assert_eq!(state.apply(Change::Join(Box::new(join))), Err(held.into()));
        assert_eq!(state, before);
    }

    // A refusal after the ticket's checks would pass still counts no use.
    #[test]
    fn a_join_for_a_member_is_refused_and_counts_no_use() {
        let mut state = state();
        let before = bound(state.clone(), 1, 1);
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

    // Only a compare of the whole state shows that a refused join changes nothing but
    // its binding, and `State::new` takes no join, so this sets the private field.
    // `state` with node key `key` bound to the public key of node `public_key`.
    fn bound(mut state: State, key: u8, public_key: u8) -> State {
        state.joined.insert(node(key), public(public_key));
        state
    }

    // `state` with the node key of `join` bound as `State::apply` binds it.
    fn bind(state: &mut State, join: &Join) {
        let card = &join.card;
        state.joined.entry(card.key).or_insert(card.card.public_key);
    }

    // The first join of a node key binds it, also when apply refuses that join.
    #[test]
    fn the_first_join_of_a_node_key_binds_it() {
        let mut state = state();
        assert_eq!(state.joined(node(3)), None);
        let mut twin = join(7, 3, "plant.edge.a");
        twin.card = unchecked(&holding(3, 5, "plant.edge.a"));
        let forged = Refused::Ticket(ticket::Refused::Forged {
            node: node(3),
            public_key: public(7),
        });
        assert_eq!(apply_join(&mut state, twin), Err(forged));
        assert_eq!(state.joined(node(3)), Some(public(5)));
        assert_eq!(
            apply_join(&mut state, join(7, 3, "plant.edge.a")),
            Ok(false)
        );
        assert_eq!(state.joined(node(3)), Some(public(5)));
        assert_eq!(state.member(node(3)).unwrap().public_key(), public(3));
        assert_eq!(state.joined(node(1)), None);
    }

    #[test]
    fn a_join_with_a_reserved_name_is_refused_and_counts_no_use() {
        let mut state = state();
        assert_eq!(
            state.apply(ticket_change(8, options("plant", false))),
            Ok(false)
        );
        let before = bound(state.clone(), 3, 3);
        assert_eq!(
            apply_join(&mut state, join(8, 3, "plant.@changes")),
            Err(Unfit::Reserved {
                name: name("plant.@changes")
            }
            .into())
        );
        assert_eq!(state, before);
    }

    // `plant.@changes` is the region's changes channel.
    #[test]
    fn a_join_with_a_reserved_status_name_is_refused() {
        let mut state = state();
        assert_eq!(
            state.apply(ticket_change(8, options("plant", true))),
            Ok(false)
        );
        let before = bound(state.clone(), 3, 3);
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

    // A ticket's prefix is under the region, so a join outside the region fails the
    // ticket's scope, and only a founding member gets `Unfit::Outside`.
    #[test]
    fn a_join_outside_the_region_is_refused() {
        let mut state = state();
        let before = bound(state.clone(), 3, 3);
        assert_eq!(
            apply_join(&mut state, join(7, 3, "plants.edge")),
            Err(Refused::Ticket(ticket::Refused::Scope {
                name: name("plants.edge"),
                prefix: name("plant.edge"),
            }))
        );
        assert_eq!(state, before);
    }

    #[test]
    fn a_join_whose_status_channel_name_is_too_long_is_refused() {
        let mut state = state();
        let before = bound(state.clone(), 3, 3);
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
        assert_eq!(
            state.apply(ticket_change(8, options("plant", true))),
            Ok(false)
        );
        state
    }

    #[test]
    fn a_join_with_a_name_that_a_member_holds_is_refused() {
        let mut state = open_state();
        let before = bound(state.clone(), 3, 3);
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
    }

    #[test]
    fn a_join_refused_as_taken_counts_no_use() {
        let mut state = open_state();
        assert_eq!(
            apply_join(&mut state, join(8, 3, "plant.edge.a")),
            Ok(false)
        );
        let before = bound(state.clone(), 4, 4);
        assert_eq!(
            apply_join(&mut state, join(7, 4, "plant.edge.A")),
            Err(Unfit::Taken {
                name: name("plant.edge.A"),
                key: node(3)
            }
            .into())
        );
        assert_eq!(state, before);
        let other = with_status(join(7, 4, "plant.edge.b"), &[("disk", 10)]);
        assert_eq!(apply_join(&mut state, other), Ok(false));
        assert_eq!(
            apply_join(
                &mut state,
                with_status(join(7, 5, "plant.edge.c"), &[("disk", 11)])
            ),
            Err(Refused::Ticket(ticket::Refused::Used {
                public_key: public(7)
            }))
        );
    }

    #[test]
    fn a_join_whose_status_channel_name_a_member_holds_is_refused() {
        let mut state = open_state();
        let first = with_status(join(8, 3, "plant.a"), &[("b.c", 20)]);
        assert_eq!(apply_join(&mut state, first), Ok(false));
        let before = bound(state.clone(), 4, 4);
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
        let before = bound(state.clone(), 3, 3);
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
        let before = bound(state.clone(), 4, 4);
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
            create_state(name("plant").into(), taken),
            Err(Unfit::Taken {
                name: name("plant.NODE1"),
                key: node(1)
            })
        );
        let mut reused = create_members(&[1, 2]);
        reused[0].status = status([(name("disk"), index(20))]);
        reused[1].status = status([(name("disk"), index(20))]);
        assert_eq!(
            create_state(name("plant").into(), reused),
            Err(Unfit::Reused { key: index(20) })
        );
    }

    // Each join but the last fails more than one check, and the refusal names the
    // first. Ticket 9 is not recorded, so a peer with no good admission learns no key
    // or name of the region. After ticket 7 admits node 3, a join with it fails its use
    // last, and the last join fails only its use.
    #[test]
    fn a_join_refusal_names_the_first_check_that_fails() {
        let mut state = open_state();
        let mut before = state.clone();
        for (join, refused) in ticket_checks().into_iter().chain(member_checks()) {
            bind(&mut before, &join);
            assert_eq!(apply_join(&mut state, join), Err(refused));
            assert_eq!(state, before);
        }
        assert_eq!(
            apply_join(&mut state, join(7, 3, "plant.edge.a")),
            Ok(false)
        );
        let mut after = state.clone();
        let used = Refused::Ticket(ticket::Refused::Used {
            public_key: public(7),
        });
        let held = Unfit::Taken {
            name: name("plant.edge.a"),
            key: node(3),
        };
        let cases = [
            (
                join(7, 3, "plant.edge.a"),
                Unfit::Duplicate { key: node(3) }.into(),
            ),
            (join(7, 4, "plant.edge.a"), held.into()),
            (
                with_status(join(7, 4, "plant.edge.b"), &[("disk", 10)]),
                used,
            ),
        ];
        for (join, refused) in cases {
            bind(&mut after, &join);
            assert_eq!(apply_join(&mut state, join), Err(refused));
            assert_eq!(state, after);
        }
    }

    // Joins on `open_state` that fail the card or the ticket, and a check of the
    // member too, with the refusal of the first check that each fails.
    fn ticket_checks() -> [(Join, Refused); 6] {
        let mut forged = join(9, 3, "plant.node1");
        forged.card.signature[0] ^= 1;
        let mut unsigned = join(7, 3, "plant.node2");
        unsigned.admission[0] ^= 1;
        let mut outside = join(7, 1, "plants.edge.a");
        outside.status = status([(long_status(256 - 14), index(9))]);
        let expired = join_with(&signed(1, "plant.edge.a"), 7, EXPIRY, status([]));
        [
            (forged, Refused::Forged(card::Forged { node: node(3) })),
            (
                join(9, 1, "plant.node1"),
                Refused::Unknown {
                    public_key: public(9),
                },
            ),
            (
                join(9, 3, "plant.Node2"),
                Refused::Unknown {
                    public_key: public(9),
                },
            ),
            (
                unsigned,
                Refused::Ticket(ticket::Refused::Forged {
                    node: node(3),
                    public_key: public(7),
                }),
            ),
            (
                outside,
                Refused::Ticket(ticket::Refused::Scope {
                    name: name("plants.edge.a"),
                    prefix: name("plant.edge"),
                }),
            ),
            (
                expired,
                Refused::Ticket(ticket::Refused::Expired {
                    public_key: public(7),
                    expiry: EXPIRY,
                    at: EXPIRY,
                }),
            ),
        ]
    }

    // Joins on `open_state` with a good admission that fail checks of the member,
    // with the refusal of the first check that each fails.
    fn member_checks() -> [(Join, Refused); 5] {
        let mut reserved = join(8, 1, "plant.@a");
        reserved.status = status([(long_status(256 - 9), index(9))]);
        let mut long = join(8, 1, "plant.edge.a");
        long.status =
            status([(long_status(256 - 13), index(9)), (name("t.@a"), index(10))]);
        [
            (
                reserved,
                Unfit::Reserved {
                    name: name("plant.@a"),
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
                join(8, 1, "plant.node2"),
                Unfit::Duplicate { key: node(1) }.into(),
            ),
            (
                with_status(join(8, 3, "plant.node2"), &[("disk", 9), ("x", 9)]),
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
        ]
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
                Refused::Body(Undecoded {
                    kind: 2,
                    length: 40,
                }),
                "a change of kind 2 and 40 bytes is not in the byte form of its kind"
                    .to_owned(),
            ),
        ];
        for (refused, text) in cases {
            assert_eq!(refused.to_string(), text);
        }
    }

    #[test]
    fn a_refused_card_says_what_is_wrong() {
        let cases = [
            (
                Refused::Stranger { key: node(3) },
                format!("node {} is not a member", node(3)),
            ),
            (
                Refused::Changed { key: node(2) },
                format!(
                    "the card of node {} changes more than its addresses and its \
                     version",
                    node(2)
                ),
            ),
            (
                Refused::Behind {
                    key: node(2),
                    version: 1,
                    held: 4,
                },
                format!(
                    "card version 1 of node {} is not above the held version 4",
                    node(2)
                ),
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
            prop::option::of(1..7_u8),
        )
            .prop_map(|(ticket_id, id, name, late, forged, holder)| {
                let mut join = join(ticket_id, id, name);
                if let Some(holder) = holder {
                    let card = holding(id, holder, name);
                    join = join_with(&card, ticket_id, join.at, join.status.clone());
                }
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
            .prop_map(|(id, prefix, reusable)| {
                ticket_change(id, options(prefix, reusable))
            });
        let homes = prop::collection::btree_map(0..3_u128, 1..3_u8, 0..3);
        let specs = (0..3_u64, 1..3_u8, 1..4_u8, homes).prop_map(
            |(version, base, root, homes)| {
                let homes: Vec<_> = homes.into_iter().collect();
                homed(spec(version, base, root, &[]), &homes)
            },
        );
        let cards = (1..4_u8, 0..4_u64, any::<bool>(), any::<bool>()).prop_map(
            |(id, version, renamed, forged)| {
                let mut card = moved(id, version);
                if renamed {
                    card.name = name("plant.edge.a");
                }
                card_change(id, card, id, forged)
            },
        );
        prop_oneof![joins, tickets, specs, cards]
    }

    proptest! {
        // A refused change leaves the state as it was, but a refused join binds its
        // node key, and an applied join adds its member.
        #[test]
        fn a_refused_change_changes_nothing_but_a_binding(
            steps in prop::collection::vec(steps(), 0..16),
        ) {
            let mut state =
                create_state(name("plant").into(), create_members(&[1, 2])).unwrap();
            for step in steps {
                let before = state.clone();
                let applied = state.apply(step.clone());
                match (applied, step) {
                    (Err(_), Change::Join(join)) => {
                        let mut expected = before.clone();
                        bind(&mut expected, &join);
                        prop_assert_eq!(&state, &expected);
                    }
                    (Err(_), _) => prop_assert_eq!(&state, &before),
                    (Ok(_), Change::Join(join)) => {
                        prop_assert!(before.member(join.card.key).is_none());
                        prop_assert!(state.member(join.card.key).is_some());
                    }
                    (Ok(moved), Change::Card(card)) => {
                        prop_assert!(!moved);
                        let held = before.member(card.key).unwrap().card.card();
                        prop_assert!(card.card.version > held.version);
                        // Only a compare of the whole state shows that the change
                        // changes nothing else, and `State::new` takes no ticket
                        // and starts the pointer at version 0, so it cannot build
                        // `before`. So this sets the private field.
                        let mut expected = before.clone();
                        let member = expected.members.get_mut(&card.key).unwrap();
                        member.card = card.check().unwrap();
                        prop_assert_eq!(&state, &expected);
                    }
                    (Ok(moved), Change::Spec { base, root, homes, .. }) => {
                        prop_assert_eq!(before.pointer(), base);
                        let next = if root == base.root && homes.is_empty() {
                            base
                        } else {
                            let version = base.version.checked_add(1).unwrap();
                            Pointer { version, root }
                        };
                        prop_assert_eq!(state.pointer(), next);
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

        // A check of a join gives what its apply gives, and changes nothing.
        #[test]
        fn a_check_of_a_join_gives_the_result_of_its_apply(
            steps in prop::collection::vec(steps(), 0..16),
        ) {
            let mut state =
                create_state(name("plant").into(), create_members(&[1, 2])).unwrap();
            for step in steps {
                if let Change::Join(join) = &step {
                    let before = state.clone();
                    let checked = state.check(join);
                    prop_assert_eq!(&state, &before);
                    prop_assert_eq!(checked, state.apply(step).map(drop));
                } else {
                    let _applied = state.apply(step);
                }
            }
        }

        // After each change, `holder` gives the member that a scan finds.
        #[test]
        fn holder_gives_what_a_scan_of_the_members_gives(
            steps in prop::collection::vec(steps(), 0..16),
        ) {
            let mut state =
                create_state(name("plant").into(), create_members(&[1, 2])).unwrap();
            for step in steps {
                let _applied = state.apply(step);
                for id in 1..7 {
                    let public_key = public(id);
                    let scan = (1..6).map(node).find(|&key| {
                        state.member(key).is_some_and(|m| m.public_key() == public_key)
                    });
                    prop_assert_eq!(state.holder(public_key), scan);
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

    // A founding from a small set of prefixes, members in any order, voters,
    // definitions, and homes.
    fn foundings() -> impl Strategy<Value = Founding> {
        let ids = vec![1, 2, 3, 4, 5_u8];
        let labels = vec!["plant.a", "plant.b", "plant.c"];
        (
            prop::sample::select(vec!["", "plant", "plant.cell"]),
            prop::sample::subsequence(ids.clone(), 0..=5).prop_shuffle(),
            prop::sample::subsequence(ids, 0..=5),
            prop::sample::subsequence(labels, 0..=3),
            prop::collection::btree_map(0..4_u128, 1..=5_u128, 0..=4),
        )
            .prop_map(|(prefix, members, voters, labels, homes)| {
                let definitions = (1..).zip(labels).map(|(id, label)| {
                    let key = spec::definition::Kind::Subject.key(label).unwrap();
                    let subject = spec::subject::Subject::new(vec![public(id)]);
                    (key, Definition::Subject(subject.unwrap()))
                });
                Founding {
                    prefix: prefix.parse().unwrap(),
                    members: create_members(&members),
                    voters: voters.into_iter().map(node).collect(),
                    definitions: definitions.collect(),
                    homes: homes
                        .into_iter()
                        .map(|(i, h)| (index(i), node::Key::from_u128(h)))
                        .collect(),
                }
            })
    }

    #[test]
    fn a_founding_with_members_out_of_order_or_of_one_key_does_not_decode() {
        let members = create_members(&[1, 2]);
        let none = Founding {
            prefix: Prefix::ROOT,
            members: Vec::new(),
            voters: [node(1)].into(),
            definitions: BTreeMap::new(),
            homes: BTreeMap::new(),
        };
        let mut head = Vec::new();
        put_prefix(&Prefix::ROOT, &mut head);
        put_count(0, &mut head);
        let with = |order: [usize; 2]| {
            let mut bytes = Vec::new();
            put_prefix(&Prefix::ROOT, &mut bytes);
            put_count(2, &mut bytes);
            for i in order {
                members[i].encode(&mut bytes);
            }
            bytes.extend(none.encode().strip_prefix(head.as_slice()).unwrap());
            Founding::decode(&bytes)
        };
        let founding = Founding {
            members: members.clone(),
            ..none.clone()
        };
        assert_eq!(with([0, 1]), Some(founding));
        assert_eq!(with([1, 0]), None);
        assert_eq!(with([0, 0]), None);
    }

    // A request of node 3 by ticket 2, with `count` status names.
    fn request(count: usize) -> Request {
        let card = member(3).card;
        Request {
            ticket: public(2),
            card: unchecked(&card),
            admission: ticket(2).admission(&card),
            status: (0..count).map(|i| name(&format!("plant.c.s{i}"))).collect(),
        }
    }

    // The byte form of a request with the status names `names` in their order.
    fn request_with(names: &[&str]) -> Vec<u8> {
        let request = request(0);
        let mut bytes = Vec::new();
        put_public_key(request.ticket, &mut bytes);
        request.card.encode(&mut bytes);
        bytes.extend(request.admission);
        put_count(names.len(), &mut bytes);
        for &each in names {
            put_name(&name(each), &mut bytes);
        }
        bytes
    }

    #[test]
    fn a_request_reads_back_and_leaves_the_rest_but_no_cut_of_it_reads() {
        let request = request(64);
        let mut bytes = Vec::new();
        request.encode(&mut bytes);
        for len in 0..bytes.len() {
            assert_eq!(Request::decode(&mut &bytes[..len]), None, "{len}");
        }
        bytes.push(7);
        let mut rest = bytes.as_slice();
        assert_eq!(Request::decode(&mut rest), Some(request));
        assert_eq!(rest, [7]);
    }

    #[test]
    fn a_request_with_names_out_of_order_or_twice_does_not_decode() {
        let read =
            |names: &[&str]| Request::decode(&mut request_with(names).as_slice());
        assert!(read(&["plant.c.a", "plant.c.b"]).is_some());
        assert_eq!(read(&["plant.c.b", "plant.c.a"]), None);
        assert_eq!(read(&["plant.c.a", "plant.c.a"]), None);
    }

    #[test]
    fn a_request_of_more_than_64_names_does_not_decode() {
        let read = |request: Request| {
            let mut bytes = Vec::new();
            request.encode(&mut bytes);
            Request::decode(&mut bytes.as_slice())
        };
        assert_eq!(read(request(64)), Some(request(64)));
        assert_eq!(read(request(65)), None);
    }

    proptest! {
        // The byte form reads back as the founding with its members in key order, and
        // no cut of it, or it with a byte more, reads.
        #[test]
        fn a_founding_reads_back_from_its_byte_form(
            founding in foundings(),
            cut in any::<prop::sample::Index>(),
            extra in any::<u8>(),
        ) {
            let bytes = founding.encode();
            let mut sorted = founding.clone();
            sorted.members.sort_by_key(|member| member.card.key());
            prop_assert_eq!(sorted.encode(), bytes.clone());
            prop_assert_eq!(Founding::decode(&bytes), Some(sorted));
            let cut = &bytes[..cut.index(bytes.len())];
            prop_assert_eq!(Founding::decode(cut), None);
            let mut longer = bytes;
            longer.push(extra);
            prop_assert_eq!(Founding::decode(&longer), None);
        }
    }
}
