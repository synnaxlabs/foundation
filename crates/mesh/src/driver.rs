//! Drives the `raft` group of one region on one shard.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::future::poll_fn;
use std::mem;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};

use block::{Block, Pool};
use env::clock::{Clock, Sleep};
use env::entropy::Entropy;
use env::files::Files;
use env::tasks::Tasks;
use raft::{Body, Data, Entry, Position, Raft, Ready, Start, Voters};
use spec::Pointer;
use spec::tree::Chunks;
use transport::{Code, Session, Transport};
use types::channel;
use types::digest::Digest;
use types::ed25519::{PrivateKey, PublicKey};
use types::name::Name;
use types::node;
use types::time::{Span, Stamp};
use wire::Protocol;

use crate::applied::Applied;
use crate::bytes::block;
use crate::change::{Change, Join, Malformed};
use crate::claim::{self, Known, Signer};
use crate::error::{Error, Stopped};
use crate::log::{self, Log};
use crate::member::Member;
use crate::message::Message;
use crate::region::{self, Refused, Request};
use crate::status::{self, Status};
pub use end::Ended;
use end::Spawner;
use send::Senders;
use used::{Opening, Used};

mod apply;
mod end;
mod home;
mod propose;
mod send;
mod stream;
mod used;

/// The time of one `raft` tick.
const TICK: Span = Span::from_nanos(100 * Span::MILLISECOND.nanos());
const ELECTION_TICKS: u32 = 10;

/// The header that goes first on each stream that this node opens.
fn header(pool: &Pool) -> Result<Block, block::Error> {
    block(pool, &wire::header::encode(Protocol::Mesh))
}
const HEARTBEAT_TICKS: u32 = 1;
/// The most messages that wait for one member.
const QUEUE_MAX: usize = 64;
/// The directory of the log, in [`Config::dir`].
const LOG: &str = "log";
/// The code of a stream with a message that the mesh refused.
const REFUSED: Code = Code(16);
/// The code of a stream with a request from a node that a committed configuration
/// removed.
const REMOVED: Code = Code(17);

/// What a [`Mesh`] is built from.
#[derive(Debug)]
pub struct Config {
    /// This node.
    pub key: node::Key,
    /// This node's private key. It signs the node's claims.
    pub private_key: PrivateKey,
    /// The region before the first entry of its log, the same at each open.
    pub founding: region::Founding,
    /// The file seam. `os` or `sim` implements it.
    pub files: Files,
    /// The mesh's directory, relative to the data directory. The mesh makes it, and
    /// the log goes in `log` in it. Its parent must be there and durable.
    pub dir: PathBuf,
    /// Times the ticks of the group.
    pub clock: Clock,
    /// Gives each election timeout its random part.
    pub entropy: Entropy,
    /// Runs the group's task and the tasks that send.
    pub tasks: Tasks,
    /// Gives the blocks of the log's reads and writes, of each message that the group
    /// sends, and of each answer to a forwarded proposal. A write that finds the pool
    /// full, or that the system refuses memory for, waits: the group takes, sends, and
    /// applies nothing until that write ends. A message that finds no block drops.
    pub pool: Rc<Pool>,
    /// The transport of this shard. It proves the public half of `private_key`. The
    /// mesh dials each other member on it.
    pub transport: Rc<Transport>,
    /// This node's chunk store. The mesh puts in it the chunks of the founding tree at
    /// each open and of each spec change that this node proposes, and reads from it
    /// the tree of the base of a change.
    pub store: Rc<blob::Store>,
}

/// One node's part in the group of a region. Clones share it. The group runs until
/// it stops or each clone drops. It stays on the shard that opened it.
///
/// The group's task ends soon after the last clone drops. A write in progress ends
/// first, and a write that waits for a block ends at the next tick. Until then, a new
/// open of the same directory gives [`Error::Log`]. Once the group stops or the
/// last clone drops, each task that sends ends at once: a dial or a send in
/// progress stops, and does not wait for its timeout. [`Mesh::ended`] tells when
/// each task has ended.
#[derive(Clone)]
pub struct Mesh {
    group: Rc<RefCell<Group>>,
    spawner: Spawner,
    pool: Rc<Pool>,
    store: Rc<blob::Store>,
    clock: Clock,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the join answer of #336 is the first user")
    )]
    entropy: Entropy,
}

impl fmt::Debug for Mesh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mesh").finish_non_exhaustive()
    }
}

impl Mesh {
    /// Reads the log from `config.dir`, starts the group as a follower, and spawns
    /// its task on `config.tasks`. It puts each chunk of the founding tree in
    /// `config.store`. It reads the spec of the newest pointer that a file in
    /// `<config.dir>/spec` names, or the founding spec when there is no file, and
    /// removes the file of each older pointer. A spec that does not read or has
    /// problems is not an error: the node then uses no spec, and [`Mesh::spec`] gives
    /// the cause in `behind`.
    ///
    /// At each open, its state starts at the founding: `pointer` gives version 0, and a
    /// watch gives each home of `config.founding.homes`. The state moves on when this
    /// node applies the log, after it hears the leader. Until then, a watch can give a
    /// founding home that the log moved, as at a follower behind the leader.
    ///
    /// The group sends its messages on a session to each member. It dials a member at
    /// the addresses of its card, at the first message for it, and again after the
    /// session fails. It never closes a session.
    ///
    /// # Errors
    ///
    /// - [`Error::Member`] when the region cannot hold one of
    ///   `config.founding.members`, or two name one node.
    /// - [`Error::NotMember`] when `config.founding.members` lacks this node, a voter,
    ///   or the node of a home.
    /// - [`Error::WrongKey`] when `config.private_key` is not the key of this node in
    ///   `config.founding.members`.
    /// - [`Error::Pool`] when the pool has no block for a chunk, and [`Error::Blob`]
    ///   when a call of the store fails.
    /// - [`Error::Log`] when the log does not open.
    /// - [`Error::Raft`] when `raft` refuses the log.
    /// - [`Error::Files`] when a call on `<config.dir>/spec` or its files fails, and
    ///   [`Error::Stray`] when that directory holds a file that does not name a
    ///   pointer.
    ///
    /// # Panics
    ///
    /// When `config.transport` proves a key that is not the public half of
    /// `config.private_key`.
    pub async fn open(config: Config) -> Result<Self, Error> {
        let own = config.private_key.public();
        let proved = config.transport.public_key();
        assert!(
            proved == own,
            "invariant: the transport of a mesh proves the public half of its private \
             key: it proves {proved}, not {own}"
        );
        let transport = Rc::clone(&config.transport);
        let mesh = Self::start(config).await?;
        let senders = Senders {
            group: Rc::downgrade(&mesh.group),
            transport,
            pool: Rc::clone(&mesh.pool),
            spawner: mesh.spawner.clone(),
        };
        mesh.spawner.spawn(senders.run());
        Ok(mesh)
    }

    // Opens the group with no task that sends: `outgoing` gives each message.
    async fn start(config: Config) -> Result<Self, Error> {
        let mut chunks = Chunks::default();
        let region::Founding {
            prefix,
            members,
            voters,
            definitions,
            homes,
        } = config.founding;
        let tree = spec::region::tree(&mut chunks, &definitions);
        let state =
            region::State::new(prefix, members, tree.root, voters.clone(), homes)
                .map_err(Error::Member)?;
        check_members(&state, config.key, &config.private_key, &voters)?;
        put(&config.store, &config.pool, &chunks, &tree.chunks).await?;
        let signer = Signer::new(config.key, &config.private_key);
        let pool = Rc::clone(&config.pool);
        let files = config.files.clone();
        let (log, stored) = open_log(config.files, &config.dir, config.pool).await?;
        let used = used::open(Opening {
            files: &files,
            dir: &config.dir,
            store: &config.store,
            prefix: state.prefix(),
            root: tree.root,
            definitions,
            chunks,
        })
        .await?;
        let unapplied = written(&stored.entries).collect();
        let raft = follower(config.key, stored, voters)?;
        let group = Group::new(raft, state, unapplied, used);
        let group = Rc::new(RefCell::new(group));
        let weak = Rc::downgrade(&group);
        let spawner = Spawner::new(config.tasks);
        spawner.spawn(used::keep(
            Weak::clone(&weak),
            Rc::clone(&config.store),
            files,
            config.dir,
            config.clock.clone(),
        ));
        spawner.spawn(run(
            weak,
            log,
            signer,
            config.clock.clone(),
            config.entropy.clone(),
        ));
        Ok(Self {
            group,
            spawner,
            pool,
            store: config.store,
            clock: config.clock,
            entropy: config.entropy,
        })
    }

    /// Gives a future that resolves once each task of the mesh has ended: the group's
    /// task and each task that sends. The future holds no clone, so it does not keep
    /// the group running. Once it resolves, the mesh holds no file, and a new open of
    /// its directory can take the log.
    #[must_use]
    pub fn ended(&self) -> Ended {
        self.spawner.ended()
    }

    /// A watch of the home of `index`.
    #[must_use]
    pub fn watch(&self, index: channel::Key) -> Watch {
        let mut group = self.group.borrow_mut();
        let slot = group.slot();
        Watch {
            group: Rc::downgrade(&self.group),
            stopped: Rc::clone(&group.stopped),
            slot,
            index,
            given: None,
            called: false,
        }
    }

    /// The member with `key` in this node's view of the region, or `None` when the
    /// region has no such member. It answers also after the group stops, from the view
    /// at the stop.
    #[must_use]
    pub fn member(&self, key: node::Key) -> Option<Member> {
        self.group.borrow().state.member(key).cloned()
    }

    /// The name of each member in this node's view of the region. It answers also after
    /// the group stops, from the view at the stop.
    #[must_use]
    pub fn names(&self) -> BTreeSet<Name> {
        self.group.borrow().state.names()
    }

    /// The key of the member whose card holds `public_key` in this node's view of the
    /// region, or `None` when no member holds it. At most one member holds a key. It
    /// answers also after the group stops, from the view at the stop.
    #[must_use]
    pub fn holder(&self, public_key: PublicKey) -> Option<node::Key> {
        self.group.borrow().state.holder(public_key)
    }

    /// The spec pointer in this node's applied state. It answers also after the group
    /// stops, from the view at the stop. This is the agreed pointer. Its spec can have
    /// problems that keep the node on an earlier spec.
    #[must_use]
    pub fn pointer(&self) -> Pointer {
        self.group.borrow().state.pointer()
    }

    /// Gives the group `message`, which `peer` sent.
    ///
    /// # Errors
    ///
    /// The group does not see a message that fails a check.
    ///
    /// - [`Error::Stopped`] when the group stopped.
    /// - [`Error::Pool`] from a write of the log that finds no block, until that write
    ///   ends.
    /// - [`Error::Spoofed`] when `peer` is not the key of the member that the message
    ///   names as its sender.
    /// - [`Error::Removed`] when the message is a request and a committed
    ///   configuration removed its sender.
    /// - [`Error::NotVoter`] when the message is a request and its sender is not a
    ///   voter of this node's configuration, and no committed configuration removed
    ///   it.
    /// - [`Error::Claim`] when a claim of a known signer in the message does not
    ///   hold. A claim of a signer with no key at this node is not an error: a voter
    ///   of the proof with no key is removed, and an append is cut before the first
    ///   entry with a claim of such a signer, so the group takes the shorter run. A
    ///   claim that does not hold under the key of a join that is not applied is
    ///   such a claim: only the apply proves a key. The grant of a reply is the
    ///   exception: the sender check proved that the peer holds that key, so a
    ///   grant that fails under it is forged.
    /// - [`Error::Raft`] when `raft` refuses the message.
    ///
    /// # Panics
    ///
    /// When a claim in `message` has no signature. A decoded message gives each
    /// claim one.
    pub(crate) fn receive(
        &self,
        peer: PublicKey,
        mut message: raft::Message,
    ) -> Result<(), Error> {
        let mut group = self.group.borrow_mut();
        group.taking()?;
        let from = message.from;
        if group.public_key(from).map(Known::public_key) != Some(peer) {
            return Err(Error::Spoofed { from });
        }
        if request(&message.body) && !group.voter(from) {
            return Err(if group.raft.removed(from) {
                Error::Removed { from }
            } else {
                Error::NotVoter { from }
            });
        }
        claim::check(&group.raft, &mut message, |key| group.public_key(key))?;
        group.input(|raft| raft.step(message))?;
        group.sync();
        group.wake();
        Ok(())
    }

    /// Proposes `change` on this node, and returns once the entry is on disk here.
    /// The change is in force once a quorum holds it, and a new leader can replace
    /// it before then.
    ///
    /// # Errors
    ///
    /// - [`Error::Stopped`] when the group stopped, or stops before the write ends.
    /// - [`Error::Pool`] from an earlier write of the log that finds no block, until
    ///   that write ends.
    /// - [`Error::Raft`] with [`raft::Error::NotLeader`] when this node does not
    ///   lead, or when a new leader replaces the entry before a write holds it.
    pub(crate) async fn propose(&self, change: Change) -> Result<Position, Error> {
        let mut data = Vec::new();
        change.encode(&mut data);
        self.propose_data(data).await
    }

    /// Proposes `voters` as the next set of voters, and returns once the entry is on
    /// disk here. `raft` moves to the set through a joint configuration (RAFT
    /// VOTERS).
    ///
    /// # Errors
    ///
    /// - [`Error::Stopped`] when the group stopped, or stops before the write ends.
    /// - [`Error::NotMember`] when a node of the set is not a member in the applied
    ///   state of this node.
    /// - [`Error::Raft`] when `raft` refuses the set ([`Raft::propose_voters`]), or
    ///   with [`raft::Error::NotLeader`] when a new leader replaces the entry before
    ///   a write holds it.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the join answer of #336 is the first user")
    )]
    pub(crate) async fn propose_voters(
        &self,
        voters: BTreeSet<node::Key>,
    ) -> Result<Position, Error> {
        self.proposed(|group| {
            let mut members = voters.iter();
            if let Some(&key) = members.find(|&&key| group.state.member(key).is_none())
            {
                return Err(Error::NotMember(key));
            }
            Ok(group.raft.propose_voters(voters)?)
        })
        .await
    }

    // Proposes `data` as it is, which need not be a change.
    async fn propose_data(&self, data: Vec<u8>) -> Result<Position, Error> {
        self.proposed(|group| Ok(group.raft.propose(data)?)).await
    }

    // Waits for the write of the entry that `propose` appends.
    async fn proposed(
        &self,
        propose: impl FnOnce(&mut Group) -> Result<Position, Error>,
    ) -> Result<Position, Error> {
        let proposal = {
            let mut group = self.group.borrow_mut();
            group.taking()?;
            let at = propose(&mut group)?;
            group.sync();
            let proposal = Rc::new(Proposal {
                at,
                held: Cell::new(None),
                waker: Cell::new(None),
            });
            group.proposals.push(Rc::clone(&proposal));
            group.wake();
            proposal
        };
        poll_fn(|cx| {
            let group = self.group.borrow();
            match proposal.held.get() {
                Some(true) => return Poll::Ready(Ok(proposal.at)),
                Some(false) => {
                    let leader = group.raft.leader();
                    return Poll::Ready(Err(raft::Error::NotLeader { leader }.into()));
                }
                None => {}
            }
            group.running()?;
            proposal.waker.set(Some(cx.waker().clone()));
            Poll::Pending
        })
        .await
    }

    /// Proposes the `change` that `peer` forwarded, and gives the answer for the
    /// reply half of its stream: the position once the entry is on disk here, or
    /// "not the leader". A change that `peer` forwards again applies again.
    ///
    /// # Errors
    ///
    /// - [`Error::Stopped`] when the group stopped, or stops before the write ends.
    /// - [`Error::Pool`] from an earlier write of the log that finds no block, until
    ///   that write ends. The group does not see the change.
    /// - [`Error::PeerNotVoter`] when `peer` is the key of no voter of this node's
    ///   configuration. The group does not see the change.
    pub(crate) async fn answer(
        &self,
        peer: PublicKey,
        change: Change,
    ) -> Result<Message, Error> {
        {
            let group = self.group.borrow();
            group.taking()?;
            let holds = |voter: node::Key| {
                group.public_key(voter).map(Known::public_key) == Some(peer)
            };
            if !group.raft.voters().nodes().any(holds) {
                return Err(Error::PeerNotVoter { peer });
            }
        }
        match self.propose(change).await {
            Ok(at) => Ok(Message::Proposed { at }),
            Err(Error::Raft(raft::Error::NotLeader { leader })) => {
                Ok(Message::NotLeader { leader })
            }
            Err(error) => Err(error),
        }
    }

    /// The `Join` of `request` at the later edge of the mesh time of `time`, with a new
    /// UUIDv7 key for each status name. The caller proposes it, or forwards it to the
    /// leader. Each node checks the join when it applies it.
    ///
    /// # Errors
    ///
    /// - [`Unstamped::Unsynced`] when `time` has no mesh time, when its error is
    ///   unknown, or when the later edge is before the Unix epoch, where a UUIDv7
    ///   key has no time.
    /// - [`Unstamped::Status`] when `request` names more than 64 status channels.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the join answer of #336 is the first user")
    )]
    pub(crate) fn stamp(
        &self,
        time: &clock::Reader,
        request: Request,
    ) -> Result<Change, Unstamped> {
        let measurement = match time.status() {
            clock::Status::Synced(measurement)
            | clock::Status::Holdover(measurement, _) => measurement,
            clock::Status::Unsynced(_) => return Err(Unstamped::Unsynced),
        };
        let at = measurement.interval().latest;
        if !measurement.known() || at < Stamp::EPOCH {
            return Err(Unstamped::Unsynced);
        }
        let key = |name| {
            let mut random = [0; 16];
            self.entropy.fill(&mut random);
            (name, channel::Key::v7(at, u128::from_le_bytes(random)))
        };
        let keys = request.status.into_iter().map(key).collect();
        Ok(Change::Join(Box::new(Join {
            ticket: request.ticket,
            at,
            card: request.card,
            admission: request.admission,
            status: Status::new(keys).map_err(Unstamped::Status)?,
        })))
    }

    /// Waits for the next message for the member `to`. Each message is signed, and
    /// what it relies on is on disk. A message that 64 newer ones follow is
    /// dropped: `raft` sends again. One task at a time waits for one member.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the group stopped.
    #[cfg(test)]
    pub(crate) async fn outgoing(&self, to: node::Key) -> Result<raft::Message, Error> {
        poll_fn(|cx| {
            let mut group = self.group.borrow_mut();
            group.running()?;
            group.queues.entry(to).or_default();
            group.outgoing(to, cx).map(Ok)
        })
        .await
    }
}

/// Why this node stamps no join.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the join answer of #336 is the first user")
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unstamped {
    /// The node has no mesh time that can stamp a join: none yet, one with an unknown
    /// error, or one whose later edge is before the Unix epoch.
    Unsynced,
    /// The join request names more than 64 status channels.
    Status(status::Many),
}

impl fmt::Display for Unstamped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsynced => f.write_str(
                "this node has no mesh time with a known error at or after the Unix \
                 epoch, so it stamps no join",
            ),
            Self::Status(many) => many.fmt(f),
        }
    }
}

impl std::error::Error for Unstamped {}

/// A watch of the home of one index.
pub struct Watch {
    group: Weak<RefCell<Group>>,
    // The cause of the group's stop, which this watch gives after the group drops.
    stopped: Rc<OnceCell<Stopped>>,
    // The key of this watch's waker in the group.
    slot: u64,
    index: channel::Key,
    // What the last call of `next` gave.
    given: Option<node::Key>,
    // Whether `next` returned before.
    called: bool,
}

impl fmt::Debug for Watch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watch")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl Watch {
    /// The first call returns the home of the index at once. Each later call waits
    /// until the home differs from the one it last returned, and returns the newest:
    /// two changes between calls give one result. `None` means that the index has no
    /// home in this node's state: no founding home and no applied entry gives one. It
    /// is never `None` after a home, because no change clears a home.
    ///
    /// # Errors
    ///
    /// [`Stopped`], the cause, at once, on each call after the group stops or each
    /// [`Mesh`] of it drops. A group that stopped keeps its cause when each [`Mesh`]
    /// drops.
    pub async fn next(&mut self) -> Result<Option<node::Key>, Stopped> {
        poll_fn(|cx| {
            if let Some(stopped) = self.stopped.get() {
                return Poll::Ready(Err(stopped.clone()));
            }
            let Some(group) = self.group.upgrade() else {
                return Poll::Ready(Err(Stopped::Dropped));
            };
            let mut group = group.borrow_mut();
            let home = group.state.home(self.index);
            if self.called && self.given == home {
                group.watches.insert(self.slot, cx.waker().clone());
                return Poll::Pending;
            }
            (self.given, self.called) = (home, true);
            Poll::Ready(Ok(home))
        })
        .await
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(group) = self.group.upgrade() {
            group.borrow_mut().watches.remove(&self.slot);
        }
    }
}

struct Group {
    raft: Raft,
    state: region::State,
    queues: BTreeMap<node::Key, Queue>,
    // The session to each member that its task dialed, until a send finds that the
    // session failed or the group stops.
    sessions: BTreeMap<node::Key, Session>,
    // Why the group stopped. Each watch shares it, so the cause outlives the group.
    stopped: Rc<OnceCell<Stopped>>,
    // The task of `run`, while it waits for an input.
    task: Option<Waker>,
    // The task of each watch that waits in `Watch::next`.
    watches: BTreeMap<u64, Waker>,
    // Each proposal since the task last took a `Ready`. The next `Ready` holds the
    // entry of each, unless a new leader replaced the entry.
    proposals: Vec<Rc<Proposal>>,
    // The count of slots given, which is the slot of the next watch or try.
    slots: u64,
    // Each join and configuration entry in the log that `raft` holds and this
    // node has not applied, by index.
    unapplied: BTreeMap<u64, Written>,
    // The last entry whose join or configuration entry `sync` took.
    synced: Position,
    // Why the last try of a write of the log found no block, until that write ends.
    waits: Option<block::Error>,
    // Each member that got its queue since `Senders::run` last took this.
    fresh: Vec<node::Key>,
    // The task of `Senders::run`, while it waits for a new queue.
    starter: Option<Waker>,
    applied: Applied,
    // The task of each call that waits for the outcome of a try of its proposal, or
    // for a read of the spec.
    calls: BTreeMap<u64, Waker>,
    used: Used,
}

impl Group {
    fn new(
        raft: Raft,
        state: region::State,
        unapplied: BTreeMap<u64, Written>,
        used: Used,
    ) -> Self {
        Self {
            raft,
            state,
            queues: BTreeMap::new(),
            sessions: BTreeMap::new(),
            stopped: Rc::default(),
            task: None,
            watches: BTreeMap::new(),
            proposals: Vec::new(),
            slots: 0,
            unapplied,
            synced: Position::default(),
            waits: None,
            fresh: Vec::new(),
            starter: None,
            applied: Applied::default(),
            calls: BTreeMap::new(),
            used,
        }
    }

    // The public key of `key` in the applied state, else in the joins of the log
    // as `raft` holds it. When those name two public keys, the joins below the
    // first configuration entry whose incoming half names `key` decide: the leader
    // applied the join that made `key` a voter before it wrote that entry, so that
    // join is below it, and a later join can be a forgery. Two keys there too give
    // none until the apply decides. An outgoing half repeats an earlier incoming
    // half, so it names no node first.
    fn public_key(&self, key: node::Key) -> Option<Known> {
        if let Some(member) = self.state.member(key) {
            return Some(Known::Applied(member.public_key()));
        }
        let joins = |below| {
            self.unapplied
                .range(..below)
                .filter_map(|(_, written)| match written {
                    Written::Join(of, public) if *of == key => Some(*public),
                    _ => None,
                })
        };
        if let Some(public) = one(joins(u64::MAX)) {
            return Some(Known::Unapplied(public));
        }
        let named =
            self.unapplied
                .iter()
                .find_map(|(&index, written)| match written {
                    Written::Named(named) if named.contains(&key) => Some(index),
                    _ => None,
                })?;
        one(joins(named)).map(Known::Unapplied)
    }

    // Takes the joins and configuration entries that `raft` appended since the last
    // sync. It runs after each step and proposal; a tick adds none. When `unstable`
    // no longer holds the synced entry, a step can have replaced it, and the
    // entries from the first unstable index go.
    fn sync(&mut self) {
        let unstable = self.raft.unstable();
        let (Some(first), Some(last)) = (unstable.first(), unstable.last()) else {
            return;
        };
        let synced = self.synced;
        let new = unstable.iter().rev().take_while(|entry| entry.at != synced);
        let new = new.count();
        if new == unstable.len() {
            while let Some(entry) = self.unapplied.last_entry()
                && *entry.key() >= first.at.index
            {
                entry.remove();
            }
        }
        let (_, new) = unstable.split_at(unstable.len().saturating_sub(new));
        self.unapplied.extend(written(new));
        self.synced = last.at;
    }

    fn running(&self) -> Result<(), Error> {
        match self.stopped.get() {
            Some(stopped) => Err(Error::Stopped(stopped.clone())),
            None => Ok(()),
        }
    }

    // Whether the group takes a proposal or a message now.
    fn taking(&self) -> Result<(), Error> {
        self.running()?;
        match &self.waits {
            Some(cause) => Err(Error::Pool(cause.clone())),
            None => Ok(()),
        }
    }

    // Whether `key` votes in one half of the configuration, at least.
    fn voter(&self, key: node::Key) -> bool {
        self.raft.voters().contains(key)
    }

    // Gives `raft` an input, and wakes each call when the leader or the term changes.
    fn input<T>(&mut self, input: impl FnOnce(&mut Raft) -> T) -> T {
        let lead = (self.raft.leader(), self.raft.term());
        let output = input(&mut self.raft);
        if lead != (self.raft.leader(), self.raft.term()) {
            self.wake_calls();
        }
        output
    }

    fn slot(&mut self) -> u64 {
        let slot = self.slots;
        self.slots = slot.wrapping_add(1);
        slot
    }

    // Tells `run` that the group has an input.
    fn wake(&mut self) {
        if let Some(task) = self.task.take() {
            task.wake();
        }
    }

    // Queues each message for its member. Only this makes the queue of a member,
    // and `Senders::run` then starts the one task that reads it.
    fn send(&mut self, messages: Vec<raft::Message>) {
        for message in messages {
            let queue = self.queues.entry(message.to).or_insert_with(|| {
                self.fresh.push(message.to);
                self.starter.take().into_iter().for_each(Waker::wake);
                Queue::default()
            });
            queue.push(message);
        }
    }

    // The queue of the member `to`, for the task that reads it.
    fn queue(&mut self, to: node::Key) -> &mut Queue {
        let queue = self.queues.get_mut(&to);
        queue.expect("invariant: a task reads the queue that started it")
    }

    // The next message for the member `to`.
    fn outgoing(&mut self, to: node::Key, cx: &Context<'_>) -> Poll<raft::Message> {
        let queue = self.queue(to);
        let Some(message) = queue.messages.pop_front() else {
            queue.waker = Some(cx.waker().clone());
            return Poll::Pending;
        };
        Poll::Ready(message)
    }

    // Wakes each task that sends, so that it ends, and drops each session.
    fn end_senders(&mut self) {
        self.sessions.clear();
        let queues = self.queues.values_mut();
        let waiting = queues.filter_map(|queue| queue.waker.take());
        waiting.chain(self.starter.take()).for_each(Waker::wake);
    }

    // Applies each change in `committed`, and wakes the watches when a home moves.
    fn apply(&mut self, committed: Vec<Entry>) -> Result<(), Stopped> {
        if let Some(last) = committed.last() {
            let last = last.at.index;
            self.unapplied.retain(|&index, _| index > last);
        }
        for Entry { at, data } in committed {
            let applied = match data {
                Data::Bytes(bytes) => match Change::decode(&bytes) {
                    Ok(change) => self.apply_change(change),
                    // Every node of this build judges a body the same way.
                    Err(Malformed::Body { kind, length }) => {
                        Err(Refused::Body { kind, length })
                    }
                    Err(Malformed::Unknown(cause)) => {
                        return Err(Stopped::Change { at, cause });
                    }
                },
                Data::Voters(change) => {
                    self.state.set_voters(change.voters);
                    Ok(false)
                }
                Data::Empty => Ok(false),
            };
            // A refused change is a no-op on every node.
            if let Ok(true) = applied {
                self.wake_watches();
            }
            self.applied.push(at, applied.map(|_| ()));
            self.wake_calls();
        }
        Ok(())
    }

    // Applies `change`, and gives the task of the spec in use each pointer that moves.
    fn apply_change(&mut self, change: Change) -> Result<bool, Refused> {
        let listed = match &change {
            Change::Spec { chunks, .. } => Some(chunks.clone()),
            Change::Home { .. } | Change::Join(_) | Change::Ticket { .. } => None,
        };
        let applied = self.state.apply(change)?;
        if let Some(listed) = listed {
            self.used.committed(self.state.pointer(), listed);
        }
        Ok(applied)
    }

    fn stop(&mut self, stopped: Stopped) {
        self.stopped.get_or_init(|| stopped);
        self.wake();
        self.used.wake();
        self.end_senders();
        self.wake_watches();
        self.wake_calls();
        for proposal in &self.proposals {
            proposal.wake();
        }
    }

    fn wake_watches(&mut self) {
        mem::take(&mut self.watches)
            .into_values()
            .for_each(Waker::wake);
    }

    fn wake_calls(&mut self) {
        mem::take(&mut self.calls)
            .into_values()
            .for_each(Waker::wake);
    }
}

impl Drop for Group {
    // Each task must end, which frees the log, and a watch that waits must learn
    // that each mesh dropped.
    fn drop(&mut self) {
        self.wake();
        self.used.wake();
        self.end_senders();
        self.wake_watches();
    }
}

// A call of `Mesh::propose` that waits for the write of its entry.
struct Proposal {
    at: Position,
    // Whether the write of the first `Ready` after the call held the entry. `None`
    // until that write ends.
    held: Cell<Option<bool>>,
    // The task of the call, while it waits.
    waker: Cell<Option<Waker>>,
}

impl Proposal {
    fn wake(&self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

// The messages that wait for one member.
#[derive(Default)]
struct Queue {
    messages: VecDeque<raft::Message>,
    // The one task that reads this queue, while it waits for a message or in a send
    // of one.
    waker: Option<Waker>,
}

impl Queue {
    // Adds `message`. A full queue drops its oldest message.
    fn push(&mut self, message: raft::Message) {
        if self.messages.len() == QUEUE_MAX {
            self.messages.pop_front();
        }
        self.messages.push_back(message);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

// Whether `entries`, which is the run of entries of one `Ready`, holds the entry
// at `at`.
fn holds(entries: &[Entry], at: Position) -> bool {
    let first = entries.first().map(|entry| entry.at.index);
    let offset = first.and_then(|first| at.index.checked_sub(first));
    let offset = offset.and_then(|offset| usize::try_from(offset).ok());
    let entry = offset.and_then(|offset| entries.get(offset));
    entry.is_some_and(|entry| entry.at == at)
}

// A join or a configuration entry of the log.
#[derive(Debug)]
enum Written {
    // The node key and public key of a join.
    Join(node::Key, PublicKey),
    // The nodes in the incoming half of a configuration entry.
    Named(BTreeSet<node::Key>),
}

// Starts the group as a follower on the log that `stored` holds, with `voters`.
fn follower(
    key: node::Key,
    stored: log::Stored,
    voters: BTreeSet<node::Key>,
) -> Result<Raft, Error> {
    let start = Start {
        hard: stored.hard,
        voters: Voters {
            incoming: voters,
            outgoing: BTreeSet::new(),
        },
        entries: stored.entries,
        applied: 0,
    };
    let fixed = raft::Config {
        key,
        election_ticks: ELECTION_TICKS,
        heartbeat_ticks: HEARTBEAT_TICKS,
    };
    Ok(Raft::new(fixed, start)?)
}

// Each join and configuration entry of `entries`, with its index.
fn written(entries: &[Entry]) -> impl Iterator<Item = (u64, Written)> {
    entries.iter().filter_map(|entry| {
        let written = match &entry.data {
            Data::Voters(change) => Written::Named(change.voters.incoming.clone()),
            Data::Bytes(bytes) => {
                let Ok(Change::Join(join)) = Change::decode(bytes) else {
                    return None;
                };
                let card = &join.card;
                Written::Join(card.key, card.card.public_key)
            }
            Data::Empty => return None,
        };
        Some((entry.at.index, written))
    })
}

// The one key of `keys`, when they are all the same.
fn one(mut keys: impl Iterator<Item = PublicKey>) -> Option<PublicKey> {
    let first = keys.next()?;
    keys.all(|key| key == first).then_some(first)
}

// Whether `body` asks its receiver to act. The other bodies answer a request.
fn request(body: &Body) -> bool {
    match body {
        Body::PreVote { .. }
        | Body::Vote { .. }
        | Body::Heartbeat { .. }
        | Body::Append { .. } => true,
        Body::PreVoteReply { .. }
        | Body::VoteReply { .. }
        | Body::HeartbeatReply
        | Body::AppendReply { .. }
        | Body::AppendReject { .. } => false,
    }
}

// Checks that `own`, each of `voters`, and the node of each home are members of
// `state`, and that `private_key` is the key of `own`.
fn check_members(
    state: &region::State,
    own: node::Key,
    private_key: &PrivateKey,
    voters: &BTreeSet<node::Key>,
) -> Result<(), Error> {
    match state.member(own) {
        None => return Err(Error::NotMember(own)),
        Some(record) if record.public_key() != private_key.public() => {
            return Err(Error::WrongKey);
        }
        Some(_) => {}
    }
    let mut others = voters.iter().copied().chain(state.homes());
    if let Some(key) = others.find(|&key| state.member(key).is_none()) {
        return Err(Error::NotMember(key));
    }
    Ok(())
}

// Puts each chunk of `digests`, which `chunks` holds, in `store`.
async fn put(
    store: &blob::Store,
    pool: &Pool,
    chunks: &Chunks,
    digests: &[Digest],
) -> Result<(), Error> {
    for &digest in digests {
        let bytes = chunks
            .get(digest)
            .expect("invariant: the chunks of a tree hold each chunk it lists");
        let block = pool.copy(bytes).map_err(Error::Pool)?;
        store.put(digest, &block).await.map_err(Error::Blob)?;
    }
    Ok(())
}

// Makes `dir`, durable in its parent, and opens the log in `LOG` in it.
async fn open_log(
    files: Files,
    dir: &Path,
    pool: Rc<Pool>,
) -> Result<(Log, log::Stored), log::Error> {
    files.create_dir(dir).await?;
    files
        .sync_dir(dir.parent().unwrap_or(Path::new("")))
        .await?;
    Log::open(files, dir.join(LOG), pool).await
}

// Ticks the group and does what each `Ready` says, in the order that `raft` needs:
// sign, write, send, apply. It ends when the group stops or its last handle drops.
// It is the only caller of `Raft::ready`, so the writes keep their order.
async fn run(
    group: Weak<RefCell<Group>>,
    mut log: Log,
    signer: Signer,
    clock: Clock,
    entropy: Entropy,
) {
    let mut rng = entropy.rng();
    let mut tick = clock.sleep(TICK);
    loop {
        let next = poll_fn(|cx| {
            let Some(group) = group.upgrade() else {
                return Poll::Ready(None);
            };
            let mut group = group.borrow_mut();
            if group.running().is_err() {
                return Poll::Ready(None);
            }
            // A tick that a slow write hides is lost, so the group's time only
            // slows.
            while Pin::new(&mut tick).poll(cx).is_ready() {
                group.input(|raft| raft.tick(rng.next_u64()));
                tick = clock.sleep(TICK);
            }
            let ready = group.raft.ready();
            if ready == Ready::default() {
                group.task = Some(cx.waker().clone());
                return Poll::Pending;
            }
            Poll::Ready(Some((ready, mem::take(&mut group.proposals))))
        });
        let Some((mut ready, proposals)) = next.await else {
            return;
        };
        signer.sign(&mut ready);
        let Some(written) = write(&group, &mut log, &clock, &mut tick, &ready).await
        else {
            wake_each(&proposals);
            return;
        };
        let Some(group) = group.upgrade() else { return };
        let mut group = group.borrow_mut();
        let Ready {
            entries,
            messages,
            committed,
            ..
        } = ready;
        let applied = written.map_err(Stopped::Write).and_then(|()| {
            for proposal in &proposals {
                proposal.held.set(Some(holds(&entries, proposal.at)));
            }
            group.send(messages);
            group.apply(committed)
        });
        wake_each(&proposals);
        if let Err(stopped) = applied {
            group.stop(stopped);
            return;
        }
    }
}

// Writes the hard state and the entries of `ready`, and tries again at each tick
// while the pool gives no block. `None` when the last handle of the group drops, or a
// sender stops the group, before the write ends: `run` then sends and applies none
// of them.
async fn write(
    group: &Weak<RefCell<Group>>,
    log: &mut Log,
    clock: &Clock,
    tick: &mut Sleep,
    ready: &Ready,
) -> Option<Result<(), log::Error>> {
    loop {
        let cause = match log.write(ready.hard.clone(), &ready.entries).await {
            Err(log::Error::Pool(
                cause @ (block::Error::Exhausted { .. } | block::Error::Refused { .. }),
            )) => cause,
            written => {
                let group = group.upgrade()?;
                let mut group = group.borrow_mut();
                group.waits = None;
                return group.running().is_ok().then_some(written);
            }
        };
        group.upgrade()?.borrow_mut().waits = Some(cause);
        (&mut *tick).await;
        *tick = clock.sleep(TICK);
        if group.upgrade()?.borrow().running().is_err() {
            return None;
        }
    }
}

fn wake_each(proposals: &[Rc<Proposal>]) {
    for proposal in proposals {
        proposal.wake();
    }
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::mem::ManuallyDrop;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::num::{NonZeroU32, NonZeroUsize};
    use std::pin::pin;
    use std::sync::{Arc, Mutex};
    use std::task::Wake;

    use block::testing::Scarce;
    use env::files::{self, Operation};
    use raft::{Answer, Grant, Hard, Proof, Term};
    use sim::{Crash, Sim, link};
    use spec::definition::Definition;
    use spec::tree;
    use transport::{Address, Peer, Port};
    use types::name::{Name, Prefix};
    use types::node::SealKey;
    use types::time::Monotonic;
    use wire::Protocol;

    use super::*;
    use crate::card;
    use crate::change::{CHUNKS_MAX, Unknown};
    use crate::common::{self, create_pool, key, message, private, proven, public};
    use crate::region::Unfit;
    use crate::status::Many;
    use crate::ticket::Options;
    use crate::used::{Behind, Spec};

    const IDS: [u8; 3] = [1, 2, 3];
    const PORT: u16 = 7000;
    /// The directory of each test node's chunk store.
    const BLOB: &str = "blob";
    /// The idle time of each transport.
    const IDLE: Span = Span::from_nanos(60 * Span::SECOND.nanos());
    const INDEX: channel::Key = channel::Key::from_u128(7);
    /// An index that a spec change can add after `INDEX`.
    const SECOND: channel::Key = channel::Key::from_u128(8);

    /// What each node's watch gave, in order.
    type Homes = BTreeMap<u8, Vec<Option<node::Key>>>;

    /// The parts of a spec change that a node of a cluster proposes.
    struct Proposal {
        base: Pointer,
        root: Digest,
        chunks: BTreeSet<Digest>,
        holders: BTreeSet<node::Key>,
        homes: BTreeMap<channel::Key, node::Key>,
    }

    /// What the voters of a cluster did and what they do next.
    #[derive(Default)]
    struct Board {
        homes: Homes,
        /// The node of each proposal that the group took, in order.
        led: Vec<u8>,
        /// The position of each of those proposals.
        at: Vec<Position>,
        /// The byte forms of the changes that each node proposes in order, each until
        /// the group takes it.
        script: BTreeMap<u8, VecDeque<Vec<u8>>>,
        /// The voter that forwards a change to each node, with the change.
        forwards: BTreeMap<u8, (u8, Change)>,
        /// The answer of each node to the change that it got.
        answers: BTreeMap<u8, Message>,
        /// The members from 1 to 9 on each node, when its watch last gave a home.
        members: BTreeMap<u8, BTreeSet<u8>>,
        /// The voter whose card has no address on each node.
        hidden: Option<u8>,
        /// The records of those members on each node, at the same time.
        records: BTreeMap<u8, BTreeMap<u8, Member>>,
        /// The home of `SECOND` on each node, at the same time.
        seconds: BTreeMap<u8, Option<node::Key>>,
        /// The region state of each node, at the same time.
        states: BTreeMap<u8, region::State>,
        /// The spec pointer of each node, at the same time.
        pointers: BTreeMap<u8, Pointer>,
        /// The founding definitions of each voter.
        founding: BTreeMap<Name, Definition>,
        /// The join request that each node stamps.
        requests: BTreeMap<u8, Request>,
        /// The join that each node stamped.
        stamped: BTreeMap<u8, Change>,
        /// The members that are not founding voters.
        learners: BTreeSet<u8>,
        /// The home that each node sets next.
        sets: BTreeMap<u8, u8>,
        /// Each call of `set_home` that returned, in order: its node, the home on
        /// that node at the return, and what the call gave.
        set: Vec<(u8, Option<node::Key>, Result<(), Error>)>,
        /// The voters that each node proposes next.
        configurations: BTreeMap<u8, BTreeSet<u8>>,
        /// The spec change that each node proposes next.
        applies: BTreeMap<u8, Proposal>,
        /// Each spec change of `applies` that returned, in order: its node, the
        /// pointer on that node at the return, and what the change gave.
        applied: Vec<(u8, Pointer, Result<Pointer, Error>)>,
        /// The definitions whose tree each node puts in its store next.
        puts: BTreeMap<u8, BTreeMap<Name, Definition>>,
        /// What the spec in use of each node was at its last read.
        specs: BTreeMap<u8, Seen>,
    }

    /// The spec in use of a node at a read, and what the task of the spec held.
    #[derive(Debug, PartialEq)]
    struct Seen {
        pointer: Option<Pointer>,
        definitions: BTreeMap<Name, Definition>,
        behind: Option<Behind>,
        /// The newest committed pointer that the task did not use. No public call
        /// shows it.
        newest: Option<Pointer>,
        /// The chunks of the tree in use, as `Debug` gives them: `Chunks` has no `Eq`.
        chunks: String,
    }

    impl Seen {
        fn new(mesh: &Mesh, spec: Spec) -> Self {
            let group = mesh.group.borrow();
            Self {
                pointer: spec.pointer,
                definitions: (*spec.definitions).clone(),
                behind: spec.behind,
                newest: group.used.newest.as_ref().map(|newest| newest.pointer),
                chunks: format!("{:?}", group.used.chunks),
            }
        }
    }

    fn seconds(count: i64) -> Span {
        Span::from_nanos(count.checked_mul(Span::SECOND.nanos()).unwrap())
    }

    fn home(id: u8) -> Change {
        Change::Home {
            index: INDEX,
            home: key(id),
        }
    }

    /// Node `id` of a cluster is the node of that number, from 1.
    fn address(id: u8) -> SocketAddr {
        SocketAddr::new(Ipv4Addr::new(10, 0, 0, id).into(), PORT)
    }

    async fn config(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        members: &[u8],
        voters: &[u8],
    ) -> Config {
        config_at(node, tasks, id, 0, members, voters).await
    }

    /// As [`config`], with the transport at `port` of `node`.
    async fn config_at(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        port: u16,
        members: &[u8],
        voters: &[u8],
    ) -> Config {
        let pool = create_pool();
        let store = blob::Store::open(blob::Config {
            files: node.files(),
            dir: BLOB.into(),
            pool: Rc::clone(&pool),
        })
        .await
        .unwrap();
        Config {
            key: key(id),
            private_key: private(id),
            founding: region::Founding {
                prefix: "plant".parse().unwrap(),
                members: common::create_members(members),
                voters: voters.iter().map(|&id| key(id)).collect(),
                definitions: BTreeMap::new(),
                homes: BTreeMap::new(),
            },
            files: node.files(),
            dir: PathBuf::new(),
            clock: node.clock(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
            transport: Rc::new(create_transport(
                node,
                tasks,
                id,
                port,
                Rc::clone(&pool),
            )),
            pool,
            store: Rc::new(store),
        }
    }

    /// As [`config_at`], with the record of [`create_voter`] for each of `members`.
    async fn dialed_at(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        port: u16,
        members: &[u8],
        voters: &[u8],
    ) -> Config {
        let mut config = config_at(node, tasks, id, port, members, voters).await;
        config.founding.members = members.iter().map(|&of| create_voter(of)).collect();
        config
    }

    /// A transport of node `id` at `port` of `node`, with `pool`. Port 0 is a free
    /// port.
    fn create_transport(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        port: u16,
        pool: Rc<Pool>,
    ) -> Transport {
        bind(node, port, transport_config(node, tasks, id, pool))
    }

    fn transport_config(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        pool: Rc<Pool>,
    ) -> transport::Config {
        transport::Config {
            private_key: private(id),
            message_bytes_max: NonZeroUsize::new(1 << 16).unwrap(),
            window_bytes: 1 << 20,
            streams_max: NonZeroU32::new(16).unwrap(),
            idle: IDLE,
            clock: node.clock(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
            pool,
        }
    }

    /// A transport with `config` at `port` of `node`.
    fn bind(node: &sim::node::Node, port: u16, config: transport::Config) -> Transport {
        let at = SocketAddr::new(node.addresses()[0], port);
        let mut parts = Port::bind(&node.net(), at)
            .unwrap()
            .split(NonZeroUsize::MIN);
        Transport::new(config, parts.pop().unwrap()).unwrap()
    }

    /// The wall time at the start of each run: 2026-01-01T00:00:00Z.
    const NOW: Stamp = Stamp::from_nanos(1_767_225_600 * 1_000_000_000);

    /// A reader of mesh time on `node` that follows the node's wall clock.
    #[expect(clippy::disallowed_methods, reason = "feeds the mesh clock of a test")]
    fn synced(node: &sim::node::Node) -> clock::Reader {
        let (mut clock, reader) = clock::Clock::new(node.clock());
        let source = clock.add();
        let wall = clock::source::Wall::new(node.wall(), node.clock());
        clock.push(source, wall.measure());
        reader
    }

    /// A mesh of node `id` with no task that sends: the test reads each message with
    /// `outgoing`.
    async fn open(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        members: &[u8],
        voters: &[u8],
    ) -> Result<Mesh, Error> {
        Mesh::start(config(node, tasks, id, members, voters).await).await
    }

    /// The record of node `id` with the card of node `signer` at `version`, which
    /// `signer` signed.
    fn record(id: u8, signer: u8, version: u64) -> Member {
        let mut card = common::member(signer).card.card().clone();
        card.version = version;
        Member {
            card: card::Signed::sign(key(id), card, &private(signer)),
            ..common::member(id)
        }
    }

    /// The configuration entries that `leader` wrote at `TERM` from index 1: one for
    /// each of `sets`, as its incoming and outgoing voters.
    fn changes(leader: u8, sets: &[(&[u8], &[u8])]) -> Vec<Entry> {
        let keys = |ids: &[u8]| ids.iter().map(|&id| key(id)).collect();
        let change = |(index, &(incoming, outgoing))| {
            let at = Position {
                term: common::TERM,
                index,
            };
            let voters = Voters {
                incoming: keys(incoming),
                outgoing: keys(outgoing),
            };
            common::change(leader, at, voters)
        };
        (1..).zip(sets).map(change).collect()
    }

    /// A pool of one page.
    fn small_pool() -> Rc<Pool> {
        let budget = block::Config { budget: 4096 };
        let memory = block::Heap::new(budget.reservation());
        Rc::new(Pool::new(budget, memory))
    }

    /// Takes each block that `pool` can give.
    fn fill(pool: &Pool) -> Vec<block::Unique> {
        let lens = [pool.largest(), 1];
        let blocks = lens.map(|len| iter::from_fn(move || pool.alloc(len).ok()));
        blocks.into_iter().flatten().collect()
    }

    /// What a group gives while its write of `requested` bytes waits for a block of a
    /// pool that `fill` took.
    fn exhausted(requested: usize) -> Error {
        Error::Pool(block::Error::Exhausted {
            requested,
            available: 64,
        })
    }

    /// The record of voter `id`, with the address of its transport in its card.
    fn create_voter(id: u8) -> Member {
        let member = common::member(id);
        let mut card = member.card.card().clone();
        let addresses = vec![Address::Udp(address(id))];
        card.addresses = card::addresses::Addresses::new(addresses).unwrap();
        Member {
            card: card::Signed::sign(key(id), card, &private(id)),
            ..member
        }
    }

    /// Serves each stream of each session that a peer opens to `transport`, as `node`
    /// does. The group refuses no message of a cluster.
    async fn accept(mesh: Mesh, transport: Rc<Transport>, tasks: Tasks) -> ! {
        loop {
            let session = transport.accept().await.unwrap();
            let Peer::Node(peer) = session.peer() else {
                panic!("a peer with no node key opened a session");
            };
            let (mesh, streams) = (mesh.clone(), tasks.clone());
            tasks.spawn(async move {
                while let Ok(mut incoming) = session.accept().await {
                    let mesh = mesh.clone();
                    streams.spawn(async move {
                        let Ok(Some(header)) = incoming.receiver.recv().await else {
                            return;
                        };
                        let protocol = wire::header::decode(&header).unwrap();
                        assert_eq!(protocol, (Protocol::Mesh, &[][..]));
                        match mesh.serve(peer, incoming).await {
                            Ok(()) | Err(Error::Stream(_)) => {}
                            Err(error) => panic!("a voter refused a message: {error}"),
                        }
                    });
                }
            });
        }
    }

    /// Proposes the next change of node `id` in the script, once per tick, until the
    /// group takes it.
    async fn propose(mesh: Mesh, clock: Clock, id: u8, board: Arc<Mutex<Board>>) -> ! {
        loop {
            clock.sleep(TICK).await;
            let data = board
                .lock()
                .unwrap()
                .script
                .get(&id)
                .and_then(|script| script.front().cloned());
            let Some(data) = data else {
                continue;
            };
            match mesh.propose_data(data).await {
                Ok(at) => {
                    let mut board = board.lock().unwrap();
                    board.script.get_mut(&id).map(VecDeque::pop_front);
                    board.led.push(id);
                    board.at.push(at);
                }
                Err(Error::Raft(raft::Error::NotLeader { .. })) => {}
                Err(error) => panic!("node {id} cannot propose: {error}"),
            }
        }
    }

    /// Gives node `id` the change that a voter forwards to it, and puts the answer
    /// on the board.
    async fn answer(mesh: Mesh, clock: Clock, id: u8, board: Arc<Mutex<Board>>) -> ! {
        loop {
            clock.sleep(TICK).await;
            let forward = board.lock().unwrap().forwards.remove(&id);
            let Some((from, change)) = forward else {
                continue;
            };
            let answer = mesh.answer(public(from), change).await.unwrap();
            board.lock().unwrap().answers.insert(id, answer);
        }
    }

    /// Stamps the join request of node `id`, and puts the join on the board.
    async fn stamp(
        mesh: Mesh,
        clock: Clock,
        time: clock::Reader,
        id: u8,
        board: Arc<Mutex<Board>>,
    ) -> ! {
        loop {
            clock.sleep(TICK).await;
            let request = board.lock().unwrap().requests.remove(&id);
            let Some(request) = request else {
                continue;
            };
            let join = mesh.stamp(&time, request).unwrap();
            board.lock().unwrap().stamped.insert(id, join);
        }
    }

    /// Sets the home that the board gives node `id`, and puts the result on the
    /// board.
    async fn set(mesh: Mesh, clock: Clock, id: u8, board: Arc<Mutex<Board>>) -> ! {
        loop {
            clock.sleep(TICK).await;
            let home = board.lock().unwrap().sets.remove(&id);
            let Some(home) = home else {
                continue;
            };
            let result = mesh.set_home(INDEX, key(home)).await;
            let home = mesh.group.borrow().state.home(INDEX);
            board.lock().unwrap().set.push((id, home, result));
        }
    }

    /// Proposes the voters that the board gives node `id`, and waits for the write.
    async fn configure(
        mesh: Mesh,
        clock: Clock,
        id: u8,
        board: Arc<Mutex<Board>>,
    ) -> ! {
        loop {
            clock.sleep(TICK).await;
            let voters = board.lock().unwrap().configurations.remove(&id);
            let Some(voters) = voters else {
                continue;
            };
            let voters = voters.into_iter().map(key).collect();
            mesh.propose_voters(voters).await.unwrap();
        }
    }

    /// Proposes the spec change that the board gives node `id`, as `Mesh::apply`
    /// does after its count of the holders, and puts the result on the board.
    async fn apply(mesh: Mesh, clock: Clock, id: u8, board: Arc<Mutex<Board>>) -> ! {
        loop {
            clock.sleep(TICK).await;
            let Some(spec) = board.lock().unwrap().applies.remove(&id) else {
                continue;
            };
            let Proposal {
                base,
                root,
                chunks,
                holders,
                homes,
            } = spec;
            let result = mesh.settle_spec(base, root, chunks, holders, homes).await;
            let pointer = mesh.pointer();
            board.lock().unwrap().applied.push((id, pointer, result));
        }
    }

    /// Puts in the store of node `id` each chunk of the tree of the definitions that
    /// the board gives it.
    async fn store(mesh: Mesh, clock: Clock, id: u8, board: Arc<Mutex<Board>>) -> ! {
        loop {
            clock.sleep(TICK).await;
            let Some(definitions) = board.lock().unwrap().puts.remove(&id) else {
                continue;
            };
            let mut chunks = Chunks::default();
            let update = spec::region::tree(&mut chunks, &definitions);
            let digests = &update.chunks;
            put(&mesh.store, &mesh.pool, &chunks, digests)
                .await
                .unwrap();
        }
    }

    /// Reads the spec in use of node `id` once per tick, and puts it on the board.
    async fn read(mesh: Mesh, clock: Clock, id: u8, board: Arc<Mutex<Board>>) -> ! {
        loop {
            clock.sleep(TICK).await;
            let seen = Seen::new(&mesh, mesh.spec().await.unwrap());
            board.lock().unwrap().specs.insert(id, seen);
        }
    }

    /// Three voters, each on its own node with its own transport.
    struct Cluster {
        sim: Sim,
        nodes: Vec<sim::node::Node>,
        board: Arc<Mutex<Board>>,
    }

    impl Cluster {
        fn new(seed: u64) -> Self {
            let mut sim = Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let node = |_| sim.node(sim::node::Config::default());
            Self {
                nodes: IDS.map(node).into(),
                sim,
                board: Arc::default(),
            }
        }

        /// Starts each voter.
        fn start(&self) {
            for id in IDS {
                self.start_voter(id);
            }
        }

        /// Starts voter `id`. It runs until its node crashes.
        fn start_voter(&self, id: u8) {
            let (own, board) = (self.node(id).clone(), Arc::clone(&self.board));
            let config = env::shards::Config {
                name: format!("voter-{id}"),
                core: None,
            };
            let main = move |tasks| async move {
                voter(own, tasks, id, board).await;
            };
            drop(self.node(id).shards().start(config, main).unwrap());
        }

        fn node(&self, id: u8) -> &sim::node::Node {
            &self.nodes[IDS.iter().position(|&own| own == id).unwrap()]
        }

        /// Sets what each node proposes.
        fn script(&self, change: impl Fn(u8) -> Change) {
            let script = IDS.map(|id| (id, [encoded(&change(id))].into())).into();
            self.board.lock().unwrap().script = script;
        }

        /// Each node proposes the byte forms `changes`, in order.
        fn script_each(&self, changes: &[Vec<u8>]) {
            let script = IDS.map(|id| (id, changes.iter().cloned().collect())).into();
            self.board.lock().unwrap().script = script;
        }

        fn run(&mut self, span: Span) {
            self.sim.run_for(span).unwrap();
        }

        /// Sets the chance that a datagram between `a` and `b` is lost, each way.
        fn link(&mut self, a: u8, b: u8, loss: f64) {
            let config = link::Config {
                loss,
                ..link::Config::default()
            };
            let (a, b) = (self.node(a).clone(), self.node(b).clone());
            self.sim.link(&a, &b, config);
            self.sim.link(&b, &a, config);
        }

        /// Takes what the voters did so far.
        fn take(&self) -> (Vec<u8>, Homes) {
            let board = self.board();
            (board.led, board.homes)
        }

        /// Takes the board, and leaves an empty one.
        fn board(&self) -> Board {
            mem::take(&mut *self.board.lock().unwrap())
        }
    }

    /// Spawns on `tasks` each task of node `id` that runs what `board` scripts.
    fn script(
        mesh: &Mesh,
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        board: &Arc<Mutex<Board>>,
    ) {
        let (proposing, clock, proposals) =
            (mesh.clone(), node.clock(), Arc::clone(board));
        tasks.spawn(async move {
            propose(proposing, clock, id, proposals).await;
        });
        let (answering, clock, forwards) =
            (mesh.clone(), node.clock(), Arc::clone(board));
        tasks.spawn(async move {
            answer(answering, clock, id, forwards).await;
        });
        let (stamping, clock, requests) =
            (mesh.clone(), node.clock(), Arc::clone(board));
        let time = synced(node);
        tasks.spawn(async move {
            stamp(stamping, clock, time, id, requests).await;
        });
        let (setting, clock, sets) = (mesh.clone(), node.clock(), Arc::clone(board));
        tasks.spawn(async move {
            set(setting, clock, id, sets).await;
        });
        let (applying, clock, applies) =
            (mesh.clone(), node.clock(), Arc::clone(board));
        tasks.spawn(async move {
            apply(applying, clock, id, applies).await;
        });
        let (configuring, clock, configurations) =
            (mesh.clone(), node.clock(), Arc::clone(board));
        tasks.spawn(async move {
            configure(configuring, clock, id, configurations).await;
        });
        let (storing, clock, puts) = (mesh.clone(), node.clock(), Arc::clone(board));
        tasks.spawn(async move {
            store(storing, clock, id, puts).await;
        });
        let (reading, clock, specs) = (mesh.clone(), node.clock(), Arc::clone(board));
        tasks.spawn(async move {
            read(reading, clock, id, specs).await;
        });
    }

    async fn voter(
        node: sim::node::Node,
        tasks: Tasks,
        id: u8,
        board: Arc<Mutex<Board>>,
    ) -> ! {
        let (hidden, learners, founding) = {
            let board = board.lock().unwrap();
            (board.hidden, board.learners.clone(), board.founding.clone())
        };
        let voters: Vec<u8> = IDS
            .into_iter()
            .filter(|of| !learners.contains(of))
            .collect();
        let base = config_at(&node, &tasks, id, PORT, &IDS, &voters).await;
        let transport = Rc::clone(&base.transport);
        let member = |of| {
            if hidden == Some(of) {
                common::member(of)
            } else {
                create_voter(of)
            }
        };
        let mut config = base;
        config.founding.members = IDS.map(member).into();
        config.founding.definitions = founding;
        let mesh = Mesh::open(config).await.unwrap();
        let (serving, streams) = (mesh.clone(), tasks.clone());
        tasks.spawn(async move {
            accept(serving, transport, streams).await;
        });
        script(&mesh, &node, &tasks, id, &board);
        let mut watch = mesh.watch(INDEX);
        loop {
            let home = watch.next().await.unwrap();
            let records: BTreeMap<_, _> = (1..10)
                .filter_map(|of| Some((of, mesh.member(key(of))?)))
                .collect();
            // No call of `Mesh` gives the use count of a ticket.
            let state = mesh.group.borrow().state.clone();
            let second = mesh.watch(SECOND).next().await.unwrap();
            let mut board = board.lock().unwrap();
            board.seconds.insert(id, second);
            board.states.insert(id, state);
            board.pointers.insert(id, mesh.pointer());
            board.homes.entry(id).or_default().push(home);
            board.members.insert(id, records.keys().copied().collect());
            board.records.insert(id, records);
        }
    }

    /// Runs a cluster in which each node proposes itself as the home, and gives the
    /// digest of the run with what the voters did.
    fn agree(seed: u64) -> (u64, Vec<u8>, Homes) {
        let mut cluster = Cluster::new(seed);
        cluster.script(home);
        cluster.start();
        cluster.run(seconds(5));
        let (led, homes) = cluster.take();
        (cluster.sim.digest(), led, homes)
    }

    fn each(homes: &[Option<node::Key>]) -> Homes {
        IDS.map(|id| (id, homes.to_vec())).into()
    }

    #[test]
    fn three_voters_agree_on_the_home_that_the_leader_proposes() {
        for seed in 0..8 {
            let (digest, led, homes) = agree(seed);
            let &[leader] = led.as_slice() else {
                panic!("run {seed}: the group took a proposal from each of {led:?}");
            };
            assert_eq!(homes, each(&[None, Some(key(leader))]), "run {seed}");
            assert_eq!(agree(seed), (digest, led, homes), "run {seed}");
        }
    }

    #[test]
    fn three_voters_agree_on_the_home_that_a_voter_forwards() {
        let mut cluster = Cluster::new(3);
        cluster.script(home);
        cluster.start();
        cluster.run(seconds(5));
        let board = cluster.board();
        let (&[leader], &[at]) = (board.led.as_slice(), board.at.as_slice()) else {
            panic!("the group took a proposal from each of {:?}", board.led);
        };
        let from = IDS.into_iter().find(|&id| id != leader).unwrap();
        let forward = |id| (id, (from, home(from)));
        cluster.board.lock().unwrap().forwards = IDS.map(forward).into();
        cluster.run(seconds(5));
        let answer = |id| {
            let follows = Message::NotLeader {
                leader: Some(key(leader)),
            };
            let leads = Message::Proposed { at: after(at, 1) };
            (id, if id == leader { leads } else { follows })
        };
        let board = cluster.board();
        assert_eq!(board.answers, IDS.map(answer).into());
        assert_eq!(board.homes, each(&[Some(key(from))]));
    }

    #[test]
    fn the_home_is_known_again_after_a_power_cut_of_each_node() {
        let mut cluster = Cluster::new(1);
        cluster.script(home);
        cluster.start();
        cluster.run(seconds(5));
        let (led, before) = cluster.take();
        assert_eq!(before, each(&[None, Some(key(led[0]))]));
        for node in &cluster.nodes {
            cluster.sim.crash(node, Crash::Power);
        }
        cluster.start();
        cluster.run(seconds(5));
        assert_eq!(cluster.take(), (Vec::new(), before));
    }

    // The subject `plant.app` with the key of node 1.
    fn create_founding() -> BTreeMap<Name, Definition> {
        let subject = spec::subject::Subject::new(vec![public(1)]).unwrap();
        let key = spec::definition::Kind::Subject.key("plant.app").unwrap();
        [(key, Definition::Subject(subject))].into()
    }

    #[test]
    fn the_pointer_before_the_first_change_holds_the_root_of_the_founding_tree() {
        solo(|node, tasks| async move {
            let empty = Mesh::start(config(&node, &tasks, 1, &[1], &[1]).await).await;
            let empty = empty.unwrap().pointer();
            let none = Pointer {
                version: 0,
                root: tree::empty(),
            };
            assert_eq!(empty, none);
            node.clock().sleep(TICK).await;
            let founding = create_founding();
            let sets = founding
                .iter()
                .map(|(name, value)| tree::Change::Set(name.clone(), value.encode()));
            let update =
                tree::apply(&mut Chunks::default(), tree::empty(), sets).unwrap();
            let mut config = Config {
                dir: PathBuf::from("other"),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            config.founding.definitions = founding;
            let mesh = Mesh::start(config).await.unwrap();
            let expected = Pointer {
                version: 0,
                root: update.root,
            };
            assert_eq!(mesh.pointer(), expected);
            assert_ne!(update.root, tree::empty());
        });
    }

    // Two spec changes from the founding pointer, then a home: the first applies, and
    // the second is refused on each voter.
    #[test]
    fn three_voters_agree_on_the_spec_pointer_also_after_a_power_cut() {
        let mut cluster = Cluster::new(2);
        let founding = create_founding();
        let base = Pointer {
            version: 0,
            root: spec::region::tree(&mut Chunks::default(), &founding).root,
        };
        cluster.board.lock().unwrap().founding = founding;
        let change = |byte| Change::Spec {
            base,
            root: common::digest(byte),
            chunks: [common::digest(byte)].into(),
            holders: IDS.map(key).into(),
            homes: BTreeMap::new(),
        };
        let changes = [change(1), change(2), home(1)];
        cluster.script_each(&changes.map(|change| encoded(&change)));
        cluster.start();
        cluster.run(seconds(5));
        let moved = Pointer {
            version: 1,
            root: common::digest(1),
        };
        let pointers: BTreeMap<_, _> = IDS.map(|id| (id, moved)).into();
        let board = cluster.board();
        assert_eq!(board.led.len(), 3);
        assert_eq!(board.pointers, pointers);
        cluster.board.lock().unwrap().founding = create_founding();
        for node in &cluster.nodes {
            cluster.sim.crash(node, Crash::Power);
        }
        cluster.start();
        cluster.run(seconds(5));
        assert_eq!(cluster.board().pointers, pointers);
    }

    // Voter 3 starts late, so the leader sends it the record in a catch-up `Append`.
    #[test]
    fn a_late_voter_gets_a_spec_change_of_the_most_chunks() {
        let mut cluster = Cluster::new(3);
        let founding = create_founding();
        let base = Pointer {
            version: 0,
            root: spec::region::tree(&mut Chunks::default(), &founding).root,
        };
        cluster.board.lock().unwrap().founding = founding;
        let chunks = (0..CHUNKS_MAX)
            .map(|at| Digest::of(&at.to_le_bytes()))
            .collect();
        let change = Change::Spec {
            base,
            root: common::digest(1),
            chunks,
            holders: IDS.map(key).into(),
            homes: BTreeMap::new(),
        };
        cluster.script_each(&[encoded(&change), encoded(&home(1))]);
        cluster.start_voter(1);
        cluster.start_voter(2);
        cluster.run(seconds(5));
        cluster.start_voter(3);
        cluster.run(seconds(5));
        let moved = Pointer {
            version: 1,
            root: common::digest(1),
        };
        let pointers: BTreeMap<_, _> = IDS.map(|id| (id, moved)).into();
        assert_eq!(cluster.board().pointers, pointers);
    }

    #[test]
    fn a_voter_gets_the_home_again_after_its_power_cut() {
        let mut cluster = Cluster::new(4);
        cluster.script(home);
        cluster.start();
        cluster.run(seconds(5));
        let (led, _) = cluster.take();
        let leader = led[0];
        let cut = IDS.into_iter().find(|&id| id != leader).unwrap();
        let node = cluster.node(cut).clone();
        cluster.sim.crash(&node, Crash::Power);
        cluster.start_voter(cut);
        cluster.run(seconds(5));
        let homes = [(cut, vec![None, Some(key(leader))])].into();
        assert_eq!(cluster.take(), (Vec::new(), homes));
    }

    // The others reach node 3 on the sessions that it dialed.
    #[test]
    fn each_voter_agrees_when_the_card_of_a_voter_has_no_address() {
        let mut cluster = Cluster::new(5);
        cluster.board.lock().unwrap().hidden = Some(3);
        cluster.script(home);
        cluster.start();
        cluster.run(seconds(10));
        let (led, homes) = cluster.take();
        let &[leader] = led.as_slice() else {
            panic!("the group took a proposal from each of {led:?}");
        };
        let homes_of = |id| (id, vec![None, Some(key(leader))]);
        assert_eq!(homes, IDS.map(homes_of).into());
    }

    #[test]
    fn a_leader_with_no_quorum_commits_nothing_and_takes_the_home_of_the_next() {
        let mut cluster = Cluster::new(2);
        cluster.script(home);
        cluster.start();
        cluster.run(seconds(5));
        let (led, _) = cluster.take();
        let old = led[0];
        for other in IDS.into_iter().filter(|&id| id != old) {
            cluster.link(old, other, 1.0);
        }
        cluster.script(|id| home(10 + id));
        cluster.run(seconds(5));
        let (led, homes) = cluster.take();
        let &[first, new] = led.as_slice() else {
            panic!("the group took a proposal from each of {led:?}");
        };
        assert_eq!(first, old);
        assert_ne!(new, old);
        let mut expected = each(&[Some(key(10 + new))]);
        expected.remove(&old);
        assert_eq!(homes, expected);
        for other in IDS.into_iter().filter(|&id| id != old) {
            cluster.link(old, other, 0.0);
        }
        cluster.run(seconds(5));
        let healed = BTreeMap::from([(old, vec![Some(key(10 + new))])]);
        assert_eq!(cluster.take(), (Vec::new(), healed));
    }

    /// Cuts one follower off for `cut` seconds while the leader commits home 9, heals
    /// the links, and gives the milliseconds until the watch of the follower gives
    /// that home, in steps of 250 ms.
    fn follower_heal_ms(run: u64, cut: i64) -> i64 {
        const STEP: Span = Span::from_nanos(250 * Span::MILLISECOND.nanos());
        let mut cluster = Cluster::new(run);
        cluster.script(home);
        cluster.start();
        cluster.run(seconds(5));
        let (led, _) = cluster.take();
        let leader = led[0];
        let follower = IDS.into_iter().find(|&id| id != leader).unwrap();
        for other in IDS.into_iter().filter(|&id| id != follower) {
            cluster.link(follower, other, 1.0);
        }
        cluster.script(|_| home(9));
        cluster.run(seconds(cut));
        let (led, homes) = cluster.take();
        assert_eq!(led, [leader], "run {run}");
        assert_eq!(homes.get(&follower), None, "run {run}");
        for other in IDS.into_iter().filter(|&id| id != follower) {
            cluster.link(follower, other, 0.0);
        }
        let healed = (250..=100_000).step_by(250).find(|_| {
            cluster.run(STEP);
            let (_, homes) = cluster.take();
            homes.get(&follower).and_then(|homes| homes.last()) == Some(&Some(key(9)))
        });
        healed.expect("the follower has no home 100 s after the heal")
    }

    // The bound is the 5 s that the leader of the test above gets after its heal. The
    // measured wait is at most 1 s, and a longer cut can give a longer wait (#1415).
    #[test]
    fn a_follower_cut_off_for_5_s_has_the_home_5_s_after_the_links_heal() {
        for run in 0..4 {
            let waited = follower_heal_ms(run, 5);
            assert!(waited <= 5000, "run {run}: {waited} ms after the heal");
        }
    }

    // The cut is longer than the idle time of a session, 60 s, so each session of
    // the follower timed out, and the dial that follows is 2 s old at the heal. The
    // measured wait is at most 1.5 s, and an older dial can wait longer (#1415).
    #[test]
    fn a_follower_cut_off_for_62_s_has_the_home_5_s_after_the_links_heal() {
        for run in 0..4 {
            let waited = follower_heal_ms(run, 62);
            assert!(waited <= 5000, "run {run}: {waited} ms after the heal");
        }
    }

    /// Runs `body` on the one node of a run.
    fn solo<F: Future<Output = ()> + 'static>(
        body: impl FnOnce(sim::node::Node, Tasks) -> F + Send + 'static,
    ) {
        solo_at(0, body);
    }

    /// As [`solo`], with the run at `seed`.
    fn solo_at<F: Future<Output = ()> + 'static>(
        seed: u64,
        body: impl FnOnce(sim::node::Node, Tasks) -> F + Send + 'static,
    ) {
        let mut sim = Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        sim.run_on(&node, body).unwrap();
    }

    /// Proposes `change` once per tick until this node leads.
    async fn lead(mesh: &Mesh, clock: &Clock, change: Change) -> Position {
        lead_at(mesh, clock, change).await.1
    }

    /// As [`lead`], and also gives the time at which it asked the proposal that
    /// succeeded.
    async fn lead_at(
        mesh: &Mesh,
        clock: &Clock,
        change: Change,
    ) -> (Monotonic, Position) {
        let follower = Error::Raft(raft::Error::NotLeader { leader: None });
        loop {
            let asked = clock.now();
            match mesh.propose(change.clone()).await {
                Ok(at) => return (asked, at),
                Err(error) => assert_eq!(error, follower),
            }
            clock.sleep(TICK).await;
        }
    }

    /// Makes each sync of the log fail, and gives why the group then stops.
    fn fail_sync(node: &sim::node::Node) -> Stopped {
        let path = Path::new(LOG).join("log-0");
        node.fail_file(&path, Operation::Sync);
        let cause = files::Error::Io {
            path,
            operation: Operation::Sync,
            code: 5,
        };
        Stopped::Write(log::Error::Files(cause))
    }

    fn term(mesh: &Mesh) -> Term {
        mesh.group.borrow().raft.term()
    }

    /// The term for which `mesh` asks node `to` for a pre-vote: the term after its
    /// own. A voter that follows no leader asks within two election timeouts.
    async fn pre_vote_term(mesh: &Mesh, to: u8) -> Term {
        let asked = mesh.outgoing(key(to)).await.unwrap();
        let last = Position::default();
        assert_eq!(asked.body, Body::PreVote { last });
        asked.term
    }

    /// What `future` gives, or `None` when it waits for longer than `limit`.
    async fn within<F: Future>(
        clock: &Clock,
        limit: Span,
        future: F,
    ) -> Option<F::Output> {
        let mut future = pin!(future);
        let mut end = clock.sleep(limit);
        poll_fn(|cx| {
            if let Poll::Ready(output) = future.as_mut().poll(cx) {
                return Poll::Ready(Some(output));
            }
            Pin::new(&mut end).poll(cx).map(|()| None)
        })
        .await
    }

    /// Gives the output of `future` when it does not wait.
    async fn now<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
        poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
    }

    /// Polls each call one time.
    async fn poll_each<F: Future>(calls: &mut [Pin<Box<F>>]) -> Vec<Poll<F::Output>> {
        poll_fn(|cx| {
            let polls = calls.iter_mut().map(|call| call.as_mut().poll(cx));
            Poll::Ready(polls.collect())
        })
        .await
    }

    /// Proposes `change`, and gives the result when the call does not wait.
    async fn started(mesh: &Mesh, change: Change) -> Poll<Result<Position, Error>> {
        now(pin!(mesh.propose(change))).await
    }

    /// Whether `mesh` has no message for node `to` now.
    async fn quiet(mesh: &Mesh, to: u8) -> bool {
        now(pin!(mesh.outgoing(key(to)))).await.is_pending()
    }

    /// The position after `at` in its term.
    fn after(at: Position, count: u64) -> Position {
        Position {
            index: at.index.checked_add(count).unwrap(),
            ..at
        }
    }

    // Both proposals enter before the group's write, which then waits for a block.
    // The group refuses a proposal that comes while it waits. Each call then runs on
    // its own task, so each needs its own wake.
    #[test]
    fn each_proposal_returns_after_the_write_of_its_entry() {
        for seed in 0..16 {
            solo_at(seed, move |node, tasks| async move {
                let pool = small_pool();
                let config = Config {
                    pool: Rc::clone(&pool),
                    ..config(&node, &tasks, 1, &[1], &[1]).await
                };
                let mesh = Mesh::start(config).await.unwrap();
                let first = lead(&mesh, &node.clock(), home(1)).await;
                let held = fill(&pool);
                let mut calls = [2, 3].map(|id| {
                    let other = mesh.clone();
                    Box::pin(async move { other.propose(home(id)).await })
                });
                let polled = poll_each(&mut calls).await;
                assert_eq!(polled, [Poll::Pending, Poll::Pending], "run {seed}");
                let results = Rc::new(RefCell::new([None, None]));
                for (slot, call) in calls.into_iter().enumerate() {
                    let given = Rc::clone(&results);
                    tasks.spawn(async move {
                        let result = call.await;
                        given.borrow_mut()[slot] = Some(result);
                    });
                }
                node.clock().sleep(Span::from_nanos(TICK.nanos() * 3)).await;
                assert_eq!(*results.borrow(), [None, None], "run {seed}");
                drop(held);
                node.clock().sleep(Span::from_nanos(TICK.nanos() * 2)).await;
                let positions = [after(first, 1), after(first, 2)].map(Ok).map(Some);
                assert_eq!(*results.borrow(), positions, "run {seed}");
            });
        }
    }

    // The second proposal comes while the write of the first one is in a disk call.
    #[test]
    fn a_proposal_gives_the_cause_when_the_write_before_its_own_fails() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let stopped = Error::Stopped(fail_sync(&node));
            let results = Rc::new(RefCell::new(Vec::new()));
            for id in [2, 3] {
                let (other, given) = (mesh.clone(), Rc::clone(&results));
                tasks.spawn(async move {
                    let proposed = other.propose(home(id)).await;
                    given.borrow_mut().push(proposed);
                });
                node.clock().sleep(Span::NANOSECOND).await;
                assert_eq!(*results.borrow(), []);
            }
            node.clock().sleep(TICK).await;
            assert_eq!(results.take(), [Err(stopped.clone()), Err(stopped)]);
        });
    }

    /// Node 2 grants each campaign of node 1 until node 1 sends its first append,
    /// and gives the position of the entry that starts the term of node 1.
    async fn elect(mesh: &Mesh) -> Position {
        loop {
            let sent = mesh.outgoing(key(2)).await.unwrap();
            let answer = Answer::Granted(None);
            let body = match sent.body {
                Body::PreVote { .. } => Body::PreVoteReply { answer },
                Body::Vote { .. } => Body::VoteReply { answer },
                Body::Append { entries, .. } => return entries.last().unwrap().at,
                _ => continue,
            };
            let mut ready = Ready {
                messages: vec![raft::Message {
                    term: sent.term,
                    ..message(2, 1, body)
                }],
                ..Ready::default()
            };
            common::signer(2).sign(&mut ready);
            let granted = ready.messages.remove(0);
            assert_eq!(mesh.receive(public(2), granted), Ok(()));
        }
    }

    // One `Ready` holds the entry of the proposal and commits an entry that is not
    // a change.
    #[test]
    fn a_proposal_gives_its_position_when_the_ready_of_its_write_stops_the_group() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let bad = Position {
                term: common::TERM,
                index: 1,
            };
            let append = Body::Append {
                prev: Position::default(),
                entries: vec![Entry {
                    at: bad,
                    data: Data::Bytes(vec![9]),
                }],
                commit: 0,
            };
            assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
            let first = elect(&mesh).await;
            let mut proposal = pin!(mesh.propose(home(1)));
            assert!(now(proposal.as_mut()).await.is_pending());
            let reply = raft::Message {
                term: first.term,
                ..message(2, 1, Body::AppendReply { last: first.index })
            };
            assert_eq!(mesh.receive(public(2), reply), Ok(()));
            assert_eq!(proposal.await, Ok(after(first, 1)));
            let cause = Unknown::Kind { kind: 9 };
            let stopped = Stopped::Change { at: bad, cause };
            assert_eq!(mesh.watch(INDEX).next().await, Err(stopped));
            assert_eq!(mesh.holder(public(1)), Some(key(1)));
        });
    }

    // One `Ready` holds the entry of the second proposal and commits the first home.
    // Its write is in a disk call when voter 2 stops the group, as a sender task does
    // on a code 17. The write ends, and the group sends and applies none of it.
    #[test]
    fn a_proposal_whose_write_is_in_a_disk_call_gives_a_removed_stop() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            let first = elect(&mesh).await;
            assert_eq!(mesh.propose(home(1)).await, Ok(after(first, 1)));
            let mut proposal = pin!(mesh.propose(home(2)));
            assert!(now(proposal.as_mut()).await.is_pending());
            let last = first.index + 1;
            let reply = raft::Message {
                term: first.term,
                ..message(2, 1, Body::AppendReply { last })
            };
            assert_eq!(mesh.receive(public(2), reply), Ok(()));
            node.clock().sleep(Span::NANOSECOND).await;
            let removed = Stopped::Removed { by: key(2) };
            mesh.group.borrow_mut().stop(removed.clone());
            node.clock().sleep(TICK).await;
            assert_eq!(proposal.await, Err(Error::Stopped(removed.clone())));
            assert_eq!(watch.next().await, Err(removed));
            // No call shows the home after the stop.
            assert_eq!(mesh.group.borrow().state.home(INDEX), None);
        });
    }

    // The pool stays full, so only the end of the group's task frees the log for a
    // new open, which takes its own pool.
    #[test]
    fn a_stop_while_a_write_waits_for_a_block_frees_the_log_at_the_next_tick() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let small = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            let mesh = Mesh::start(small).await.unwrap();
            lead(&mesh, &node.clock(), home(1)).await;
            let _held = fill(&pool);
            let mut proposal = pin!(mesh.propose(home(2)));
            assert!(now(proposal.as_mut()).await.is_pending());
            node.clock().sleep(TICK).await;
            let removed = Stopped::Removed { by: key(2) };
            mesh.group.borrow_mut().stop(removed.clone());
            assert_eq!(proposal.await, Err(Error::Stopped(removed)));
            node.clock().sleep(TICK).await;
            let again = Mesh::start(config(&node, &tasks, 1, &[1], &[1]).await).await;
            assert_eq!(again.map(drop), Ok(()));
        });
    }

    // One `Ready` replaces the entry of the proposal and commits an entry that is
    // not a change.
    #[test]
    fn a_replaced_proposal_gives_not_the_leader_when_its_ready_stops_the_group() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let first = elect(&mesh).await;
            let mut proposal = pin!(mesh.propose(home(1)));
            assert!(now(proposal.as_mut()).await.is_pending());
            let bad = Position {
                term: common::TERM,
                index: 2,
            };
            let append = Body::Append {
                prev: first,
                entries: vec![Entry {
                    at: bad,
                    data: Data::Bytes(vec![9]),
                }],
                commit: 2,
            };
            assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
            let leader = Some(key(2));
            let replaced = Error::Raft(raft::Error::NotLeader { leader });
            assert_eq!(proposal.await, Err(replaced));
            let cause = Unknown::Kind { kind: 9 };
            let stopped = Stopped::Change { at: bad, cause };
            assert_eq!(mesh.watch(INDEX).next().await, Err(stopped));
        });
    }

    // The `Debug` text is the behavior under test: no other call shows it.
    #[test]
    fn the_debug_text_of_a_mesh_and_of_a_watch_holds_only_the_index() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            assert_eq!(format!("{mesh:?}"), "Mesh { .. }");
            let watch = mesh.watch(INDEX);
            assert_eq!(format!("{watch:?}"), "Watch { index: Key(7), .. }");
        });
    }

    #[test]
    fn the_debug_text_of_ended_holds_nothing() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let mut ended = mesh.ended();
            let polled = poll_fn(|cx| Poll::Ready(Pin::new(&mut ended).poll(cx))).await;
            assert_eq!(polled, Poll::Pending);
            assert_eq!(format!("{ended:?}"), "Ended { .. }");
        });
    }

    mod apply;
    mod home;
    mod in_use;
    mod send;
    mod serve;

    /// The entries in the log of `node`, which had a power cut.
    fn stored(sim: &mut Sim, node: &sim::node::Node) -> Vec<Entry> {
        sim.run_on(node, |node, _| async move {
            let (_, stored) = Log::open(node.files(), LOG.into(), create_pool())
                .await
                .unwrap();
            stored.entries
        })
        .unwrap()
    }

    /// Runs `body` on node 1, and gives the entries its log then holds.
    fn solo_stored<F: Future<Output = ()> + 'static>(
        body: impl FnOnce(sim::node::Node, Tasks) -> F + Send + 'static,
    ) -> Vec<Entry> {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        sim.run_on(&node, body).unwrap();
        sim.crash(&node, Crash::Power);
        stored(&mut sim, &node)
    }

    mod answer {
        use super::*;

        #[test]
        fn a_leader_gives_the_position_of_each_change_that_a_voter_forwards() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
                let mut watch = mesh.watch(INDEX);
                let first = lead(&mesh, &node.clock(), home(1)).await;
                assert_eq!(watch.next().await, Ok(Some(key(1))));
                // Node 9 is not a member: the leader does not check the home.
                for (count, id) in [(1, 9), (2, 9), (3, 2)] {
                    let at = after(first, count);
                    let answer = mesh.answer(public(1), home(id)).await;
                    assert_eq!(answer, Ok(Message::Proposed { at }), "change {count}");
                }
                assert_eq!(watch.next().await, Ok(Some(key(2))));
            });
        }

        #[test]
        fn a_node_that_does_not_lead_names_the_leader_that_it_knows() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let answer = mesh.answer(public(3), home(3)).await;
                assert_eq!(answer, Ok(Message::NotLeader { leader: None }));
                let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
                let answer = mesh.answer(public(3), home(3)).await;
                let leader = Some(key(2));
                assert_eq!(answer, Ok(Message::NotLeader { leader }));
            });
        }

        #[test]
        fn refuses_a_peer_whose_key_no_voter_holds() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
                let first = lead(&mesh, &node.clock(), home(1)).await;
                for (case, peer) in [("a member", 2), ("no member", 9)] {
                    let answer = mesh.answer(public(peer), home(2)).await;
                    let refused = Error::PeerNotVoter { peer: public(peer) };
                    let text = format!(
                        "the peer with the public key {} forwarded a change, but no \
                         voter holds that key",
                        public(peer),
                    );
                    assert_eq!(refused.to_string(), text, "{case}");
                    assert_eq!(answer, Err(refused), "{case}");
                }
                // The group saw no change between the two.
                assert_eq!(mesh.propose(home(1)).await, Ok(after(first, 1)));
                assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
            });
        }

        // Node 3 leaves and node 4 joins: each is a voter of one half of the
        // configuration.
        #[test]
        fn takes_a_change_from_a_voter_of_each_half_of_a_joint_configuration() {
            solo(|node, tasks| async move {
                let members = [1, 2, 3, 4, 5];
                let mesh = open(&node, &tasks, 1, &members, &IDS).await.unwrap();
                let joint = Voters {
                    incoming: [key(1), key(2), key(4)].into(),
                    outgoing: [key(1), key(2), key(3)].into(),
                };
                let at = Position {
                    term: common::TERM,
                    index: 1,
                };
                let append = Body::Append {
                    prev: Position::default(),
                    entries: vec![common::change(2, at, joint)],
                    commit: 0,
                };
                assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
                let leader = Some(key(2));
                for peer in [3, 4] {
                    let answer = mesh.answer(public(peer), home(peer)).await;
                    assert_eq!(answer, Ok(Message::NotLeader { leader }), "{peer}");
                }
                let answer = mesh.answer(public(5), home(5)).await;
                assert_eq!(answer, Err(Error::PeerNotVoter { peer: public(5) }));
            });
        }

        #[test]
        fn gives_the_cause_when_the_write_of_the_entry_fails() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
                lead(&mesh, &node.clock(), home(1)).await;
                let stopped = Error::Stopped(fail_sync(&node));
                let answer = mesh.answer(public(1), home(2)).await;
                assert_eq!(answer, Err(stopped.clone()));
                for (case, peer) in [("the voter", 1), ("no voter", 2)] {
                    let answer = mesh.answer(public(peer), home(2)).await;
                    assert_eq!(answer, Err(stopped.clone()), "{case}");
                }
            });
        }

        // The entry of the change is at index 2 of the leader's term. The append of
        // the next leader puts its own entry there, or ends the log before it.
        #[test]
        fn gives_no_position_for_an_entry_that_the_next_leader_replaces_first() {
            for (case, index) in [("replaced", 2), ("removed", 1)] {
                solo(move |node, tasks| async move {
                    let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                    let first = elect(&mesh).await;
                    assert_eq!(first.index, 1);
                    let mut answer = pin!(mesh.answer(public(3), home(3)));
                    assert!(now(answer.as_mut()).await.is_pending(), "{case}");
                    let at = Position {
                        term: common::TERM,
                        index,
                    };
                    let append = Body::Append {
                        prev: if index == 2 {
                            first
                        } else {
                            Position::default()
                        },
                        entries: vec![Entry {
                            at,
                            data: Data::Empty,
                        }],
                        commit: 0,
                    };
                    assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
                    let leader = Some(key(2));
                    assert_eq!(
                        answer.await,
                        Ok(Message::NotLeader { leader }),
                        "{case}"
                    );
                });
            }
        }

        type Answers = Rc<RefCell<Vec<Result<Message, Error>>>>;

        /// Starts a task that gives `home(3)` from the voter to `mesh`, and puts the
        /// answer in `answers`.
        fn forward(mesh: &Mesh, tasks: &Tasks, answers: &Answers) {
            let (mesh, answers) = (mesh.clone(), Rc::clone(answers));
            tasks.spawn(async move {
                let answer = mesh.answer(public(1), home(3)).await;
                answers.borrow_mut().push(answer);
            });
        }

        /// Opens node 1 again after the power cut, and gives the position of the
        /// entry that sets the home to node 1, once the node has that home.
        fn lead_again(sim: &mut Sim, node: &sim::node::Node) -> Position {
            sim.run_on(node, |node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
                let mut watch = mesh.watch(INDEX);
                let at = lead(&mesh, &node.clock(), home(1)).await;
                while watch.next().await.unwrap() != Some(key(1)) {}
                at
            })
            .unwrap()
        }

        // The lone voter leads in memory while the write of its term waits for a
        // block. After a power cut the node leads the same term again, so a position
        // that it gave now would then hold another change.
        #[test]
        fn a_node_gives_no_position_while_the_write_of_its_term_waits() {
            solo(|node, tasks| async move {
                let pool = small_pool();
                let config = Config {
                    pool: Rc::clone(&pool),
                    ..config(&node, &tasks, 1, &[1], &[1]).await
                };
                let mesh = Mesh::start(config).await.unwrap();
                // No write of the log ends from here on.
                let _held = ManuallyDrop::new(fill(&pool));
                let answers = Answers::default();
                // Longer than each election timeout.
                for _ in 0..30 {
                    node.clock().sleep(TICK).await;
                    forward(&mesh, &tasks, &answers);
                }
                node.clock().sleep(TICK).await;
                let answers = answers.take();
                let full = Err(exhausted(200));
                let waits = answers.iter().position(|answer| *answer == full);
                let (before, from) = answers.split_at(waits.unwrap());
                let follows = Ok(Message::NotLeader { leader: None });
                assert_eq!(before, vec![follows; before.len()]);
                assert_eq!(from, vec![full; from.len()]);
                assert_eq!(answers.len(), 30);
            });
        }

        // The change comes while the node writes the term that it now leads, and a
        // power cut follows. A disk call takes 100 us at most, so the voter gives the
        // change each 10 us for 500 us before and after each tick.
        #[test]
        fn a_position_in_an_answer_names_no_other_change_after_a_power_cut() {
            let mut reused = Vec::new();
            for seed in 0..16 {
                let mut sim = Sim::new(sim::Config {
                    seed,
                    ..sim::Config::default()
                });
                let node = sim.node(sim::node::Config::default());
                let answered = sim
                    .run_on(&node, |node, tasks| async move {
                        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
                        let clock = node.clock();
                        let steps = |count| Span::from_nanos(count * 10_000);
                        clock.sleep(steps(TICK.nanos() / 10_000 - 50)).await;
                        loop {
                            for _ in 0..100 {
                                let mut answer = pin!(mesh.answer(public(1), home(3)));
                                match now(answer.as_mut()).await {
                                    Poll::Ready(Ok(Message::NotLeader {
                                        leader: None,
                                    })) => {}
                                    Poll::Ready(first) => return first,
                                    Poll::Pending => return answer.await,
                                }
                                clock.sleep(steps(1)).await;
                            }
                            clock.sleep(steps(TICK.nanos() / 10_000 - 100)).await;
                        }
                    })
                    .unwrap();
                let Ok(Message::Proposed { at: answered }) = answered else {
                    panic!("run {seed}: the node answered {answered:?}");
                };
                sim.crash(&node, Crash::Power);
                let taken = lead_again(&mut sim, &node);
                if answered == taken {
                    reused.push((seed, taken));
                }
            }
            assert_eq!(
                reused,
                Vec::new(),
                "in each run (seed, position), the node answered that the position \
                 holds the home {}, and after the power cut it holds the home {}",
                key(3),
                key(1),
            );
        }
    }

    mod receive {
        use super::*;

        // The link at `index` of the term before `TERM`, by leader 2: the voters
        // move from `outgoing` to 1, 2 and 3.
        fn link(index: u64, outgoing: &[u8]) -> raft::Link {
            let at = Position {
                term: Term(common::TERM.0 - 1),
                index,
            };
            let voters = Voters {
                incoming: [1, 2, 3].map(key).into(),
                outgoing: outgoing.iter().map(|&id| key(id)).collect(),
            };
            let Data::Voters(change) = common::change(2, at, voters).data else {
                unreachable!("a change is a voters entry");
            };
            raft::Link { at, change }
        }

        fn requests() -> [Body; 4] {
            let last = Position::default();
            [
                Body::PreVote { last },
                Body::Vote { last },
                Body::Heartbeat { commit: 0 },
                Body::Append {
                    prev: last,
                    entries: Vec::new(),
                    commit: 0,
                },
            ]
        }

        fn replies() -> [Body; 5] {
            let answer = Answer::Refused;
            [
                Body::PreVoteReply { answer },
                Body::VoteReply { answer },
                Body::HeartbeatReply,
                Body::AppendReply { last: 0 },
                Body::AppendReject { hint: 0 },
            ]
        }

        #[test]
        fn refuses_a_request_from_a_member_that_is_not_a_voter() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2, 3, 4], &IDS).await.unwrap();
                let refused = Error::NotVoter { from: key(4) };
                for body in requests() {
                    let received = mesh.receive(public(4), message(4, 1, body));
                    assert_eq!(received, Err(refused.clone()));
                }
                assert_eq!(
                    refused.to_string(),
                    format!("node {} sent a request, but it is not a voter", key(4))
                );
                node.clock().sleep(TICK).await;
                assert!(quiet(&mesh, 4).await);
                assert_eq!(pre_vote_term(&mesh, 2).await, Term(1));
            });
        }

        #[test]
        fn takes_a_reply_from_a_member_that_is_not_a_voter() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2, 3, 4], &IDS).await.unwrap();
                for body in replies() {
                    let received = mesh.receive(public(4), message(4, 1, body));
                    assert_eq!(received, Ok(()));
                }
            });
        }

        /// Takes `entries` on `mesh` in one append from leader 2 that commits up to
        /// `commit`, and waits for the group to write them.
        async fn take(
            node: &sim::node::Node,
            mesh: &Mesh,
            entries: Vec<Entry>,
            commit: u64,
        ) {
            let append = Body::Append {
                prev: Position::default(),
                entries,
                commit,
            };
            assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
            node.clock().sleep(TICK).await;
        }

        /// Commits up to `commit` on `mesh` with a heartbeat from leader 2, and waits
        /// for the group to apply.
        async fn commit(node: &sim::node::Node, mesh: &Mesh, commit: u64) {
            let heartbeat = proven(2, 1, Body::Heartbeat { commit });
            assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
            node.clock().sleep(TICK).await;
        }

        /// What `mesh` answers to each request from node `from`, when each answer is
        /// the same.
        fn answered(mesh: &Mesh, from: u8) -> Result<(), Error> {
            let mut answers = requests()
                .map(|body| mesh.receive(public(from), message(from, 1, body)))
                .into_iter();
            let first = answers.next().unwrap();
            assert!(answers.all(|answer| answer == first));
            first
        }

        // Node 3 is a voter at the start, and the committed leave lacks it.
        #[test]
        fn answers_removed_to_a_node_that_a_committed_configuration_removed() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let sets = [(&[1, 2][..], &[1, 2, 3][..]), (&[1, 2], &[])];
                take(&node, &mesh, changes(2, &sets), 2).await;
                let removed = Error::Removed { from: key(3) };
                assert_eq!(answered(&mesh, 3), Err(removed.clone()));
                assert_eq!(
                    removed.to_string(),
                    format!(
                        "node {} sent a request, but a committed configuration \
                         removed it",
                        key(3)
                    )
                );
                for body in replies() {
                    let received = mesh.receive(public(3), message(3, 1, body));
                    assert_eq!(received, Ok(()));
                }
                assert!(quiet(&mesh, 3).await);
            });
        }

        // Only the outgoing set of the committed joint entry held node 3, as for a
        // wiped voter whose first entry is a joint entry.
        #[test]
        fn answers_removed_to_a_node_that_only_an_outgoing_set_held() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[1, 2]).await.unwrap();
                let sets = [(&[1, 2][..], &[1, 2, 3][..]), (&[1, 2], &[])];
                take(&node, &mesh, changes(2, &sets), 2).await;
                assert_eq!(answered(&mesh, 3), Err(Error::Removed { from: key(3) }));
            });
        }

        // Node 4 is a member that no configuration in the log held.
        #[test]
        fn answers_not_voter_to_a_request_from_a_node_that_no_configuration_held() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2, 3, 4], &IDS).await.unwrap();
                let sets = [(&[1, 2][..], &[1, 2, 3][..]), (&[1, 2], &[])];
                take(&node, &mesh, changes(2, &sets), 2).await;
                assert_eq!(answered(&mesh, 4), Err(Error::NotVoter { from: key(4) }));
                assert_eq!(answered(&mesh, 3), Err(Error::Removed { from: key(3) }));
            });
        }

        // The joint entry still holds node 3 in its outgoing set.
        #[test]
        fn answers_not_voter_until_a_configuration_without_the_sender_commits() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let sets = [(&[1, 2][..], &[1, 2, 3][..]), (&[1, 2], &[])];
                take(&node, &mesh, changes(2, &sets), 0).await;
                let not_voter = Error::NotVoter { from: key(3) };
                assert_eq!(answered(&mesh, 3), Err(not_voter.clone()));
                commit(&node, &mesh, 1).await;
                assert_eq!(answered(&mesh, 3), Err(not_voter));
                commit(&node, &mesh, 2).await;
                assert_eq!(answered(&mesh, 3), Err(Error::Removed { from: key(3) }));
            });
        }

        // Only committed entries hold node 3: it joins and leaves after the start.
        #[test]
        fn answers_removed_to_a_node_that_only_a_committed_entry_held() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[1, 2]).await.unwrap();
                let sets = [
                    (&[1, 2, 3][..], &[1, 2][..]),
                    (&[1, 2, 3], &[]),
                    (&[1, 2], &[1, 2, 3]),
                    (&[1, 2], &[]),
                ];
                take(&node, &mesh, changes(2, &sets), 4).await;
                assert_eq!(answered(&mesh, 3), Err(Error::Removed { from: key(3) }));
            });
        }

        // The commit lags at the first entry, as after a restart. The entries that
        // add node 4 and remove it again can still be truncated, so node 4 is not
        // removed until they commit.
        #[test]
        fn answers_not_voter_to_a_node_that_only_an_entry_past_the_commit_held() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2, 3, 4], &IDS).await.unwrap();
                let sets = [
                    (&[1, 2, 3][..], &[][..]),
                    (&[1, 2, 3, 4], &[1, 2, 3]),
                    (&[1, 2, 3, 4], &[]),
                    (&[1, 2, 3], &[1, 2, 3, 4]),
                    (&[1, 2, 3], &[]),
                ];
                take(&node, &mesh, changes(2, &sets), 1).await;
                assert_eq!(answered(&mesh, 4), Err(Error::NotVoter { from: key(4) }));
                commit(&node, &mesh, 5).await;
                assert_eq!(answered(&mesh, 4), Err(Error::Removed { from: key(4) }));
            });
        }

        // After a new open, the log on disk holds node 3, and no configuration is
        // committed until the leader says so.
        #[test]
        fn answers_removed_to_a_node_that_the_log_on_disk_held_once_it_commits() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[1, 2]).await.unwrap();
                let sets = [
                    (&[1, 2, 3][..], &[1, 2][..]),
                    (&[1, 2, 3], &[]),
                    (&[1, 2], &[1, 2, 3]),
                    (&[1, 2], &[]),
                ];
                take(&node, &mesh, changes(2, &sets), 0).await;
                drop(mesh);
                node.clock().sleep(TICK).await;
                let mesh = open(&node, &tasks, 1, &IDS, &[1, 2]).await.unwrap();
                assert_eq!(answered(&mesh, 3), Err(Error::NotVoter { from: key(3) }));
                commit(&node, &mesh, 4).await;
                assert_eq!(answered(&mesh, 3), Err(Error::Removed { from: key(3) }));
            });
        }

        // Members 2 and 4 would share one public key, so that a message of node 4
        // could pass the sender check under the key of voter 2.
        #[test]
        fn a_member_with_the_public_key_of_a_voter_does_not_start() {
            solo(|node, tasks| async move {
                let mut config = config(&node, &tasks, 1, &IDS, &IDS).await;
                let mut card = common::member(2).card.card().clone();
                card.name = "plant.node4".parse().unwrap();
                let card = card::Signed::sign(key(4), card, &private(2));
                config.founding.members.push(Member {
                    card,
                    ..common::member(4)
                });
                let held = Unfit::Held { key: key(2) };
                assert_eq!(Mesh::start(config).await.err(), Some(Error::Member(held)));
            });
        }

        #[test]
        fn refuses_a_request_when_the_node_has_no_voters() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[]).await.unwrap();
                let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                let refused = Error::NotVoter { from: key(2) };
                assert_eq!(mesh.receive(public(2), heartbeat), Err(refused));
                node.clock().sleep(TICK).await;
                assert!(quiet(&mesh, 2).await);
                // No public call shows the term of a node with no voters.
                assert_eq!(term(&mesh), Term(0));
            });
        }

        #[test]
        fn refuses_a_message_whose_peer_is_not_its_sender() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                let spoofed = Error::Spoofed { from: key(2) };
                let received = mesh.receive(public(3), heartbeat.clone());
                assert_eq!(received, Err(spoofed.clone()));
                let text =
                    "as its sender, but its peer does not hold the key of that member";
                assert_eq!(
                    spoofed.to_string(),
                    format!("a message names node {} {text}", key(2))
                );
                for body in requests().into_iter().chain(replies()) {
                    let stranger = message(9, 1, body.clone());
                    let received = mesh.receive(public(9), stranger);
                    assert_eq!(received, Err(Error::Spoofed { from: key(9) }));
                    let received = mesh.receive(public(3), message(2, 1, body));
                    assert_eq!(received, Err(spoofed.clone()));
                }
                node.clock().sleep(TICK).await;
                assert!(quiet(&mesh, 2).await);
                assert_eq!(pre_vote_term(&mesh, 2).await, Term(1));
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            });
        }

        #[test]
        fn refuses_a_message_with_a_forged_grant() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let mut heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                let proof = heartbeat.proof.as_mut().unwrap();
                proof.voters.get_mut(&key(3)).unwrap().as_mut().unwrap().0[63] ^= 1;
                let forged = Error::Claim(claim::Error::Forged { signer: key(3) });
                assert_eq!(mesh.receive(public(2), heartbeat), Err(forged.clone()));
                assert_eq!(
                    forged.to_string(),
                    format!("the claim of node {} is forged", key(3))
                );
                node.clock().sleep(TICK).await;
                assert!(quiet(&mesh, 2).await);
                assert_eq!(pre_vote_term(&mesh, 2).await, Term(1));
            });
        }

        #[test]
        fn takes_a_proof_with_a_bad_claim_of_a_node_with_no_key() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2], &[1, 2]).await.unwrap();
                let mut heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                let proof = heartbeat.proof.as_mut().unwrap();
                proof.voters.get_mut(&key(3)).unwrap().as_mut().unwrap().0[63] ^= 1;
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
            });
        }

        #[test]
        fn checks_the_peer_then_the_voter_then_the_claims() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[1, 2]).await.unwrap();
                let forged = |leader| {
                    let mut heartbeat =
                        proven(leader, 1, Body::Heartbeat { commit: 0 });
                    let proof = heartbeat.proof.as_mut().unwrap();
                    proof.voters.get_mut(&key(1)).unwrap().as_mut().unwrap().0[63] ^= 1;
                    heartbeat
                };
                let spoofed = Error::Spoofed { from: key(2) };
                assert_eq!(mesh.receive(public(3), forged(2)), Err(spoofed));
                let not_voter = Error::NotVoter { from: key(3) };
                assert_eq!(mesh.receive(public(3), forged(3)), Err(not_voter));
                let claim = Error::Claim(claim::Error::Forged { signer: key(1) });
                assert_eq!(mesh.receive(public(2), forged(2)), Err(claim));
            });
        }

        #[test]
        fn takes_a_request_from_a_voter_that_leaves() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let at = Position {
                    term: common::TERM,
                    index: 1,
                };
                let joint = Voters {
                    incoming: [key(1), key(2)].into(),
                    outgoing: [key(1), key(2), key(3)].into(),
                };
                let append = Body::Append {
                    prev: Position::default(),
                    entries: vec![common::change(2, at, joint)],
                    commit: 0,
                };
                assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply, message(1, 2, Body::AppendReply { last: 1 }));
                let mut pre_vote = message(3, 1, Body::PreVote { last: at });
                pre_vote.term = Term(common::TERM.0 + 1);
                assert_eq!(mesh.receive(public(3), pre_vote), Ok(()));
            });
        }

        // Node 1 was down while leader 2 moved the voters from 1, 2, 3 and 4 to 1, 2
        // and 3 in the term before `TERM`, and nodes 2 and 3 then elected node 2 in
        // `TERM`. The chain of the two entries proves the leader.
        #[test]
        fn takes_a_leader_that_the_chain_proves() {
            solo(|node, tasks| async move {
                let all = [1, 2, 3, 4];
                let mesh = open(&node, &tasks, 1, &all, &all).await.unwrap();
                let mut heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                heartbeat.proof.as_mut().unwrap().voters.remove(&key(1));
                let short = heartbeat.clone();
                heartbeat.chain = vec![link(1, &all), link(2, &[])];
                let mut forged = heartbeat.clone();
                forged.chain[1].change.signature.as_mut().unwrap().0[63] ^= 1;
                let unproven = raft::Error::Unproven {
                    term: common::TERM,
                    from: key(2),
                };
                assert_eq!(mesh.receive(public(2), short), Err(Error::Raft(unproven)));
                let claim = Error::Claim(claim::Error::Forged { signer: key(2) });
                assert_eq!(mesh.receive(public(2), forged), Err(claim));
                assert_eq!(pre_vote_term(&mesh, 2).await, Term(1));
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            });
        }

        // The message of `takes_a_leader_that_the_chain_proves`, for node 3 in place
        // of node 1. `step` refuses a message for another node before it reads a
        // link, so `receive` checks no link of it.
        #[test]
        fn checks_no_link_of_a_message_for_another_node() {
            solo(|node, tasks| async move {
                let all = [1, 2, 3, 4];
                let mesh = open(&node, &tasks, 1, &all, &all).await.unwrap();
                let mut heartbeat = proven(2, 3, Body::Heartbeat { commit: 0 });
                heartbeat.proof.as_mut().unwrap().voters.remove(&key(1));
                heartbeat.chain = vec![link(1, &all), link(2, &[])];
                let misrouted = Error::Raft(raft::Error::Misrouted { to: key(3) });
                let honest = mesh.receive(public(2), heartbeat.clone());
                assert_eq!(honest, Err(misrouted.clone()));
                let mut forged = heartbeat;
                forged.chain[1].change.signature.as_mut().unwrap().0[63] ^= 1;
                assert_eq!(mesh.receive(public(2), forged), Err(misrouted));
                assert_eq!(pre_vote_term(&mesh, 2).await, Term(1));
            });
        }

        // `step` refuses a second leader of its term before it reads a claim, so
        // `receive` checks no grant of its proof.
        #[test]
        fn checks_no_grant_of_a_second_leader() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let led = proven(2, 1, Body::Heartbeat { commit: 0 });
                assert_eq!(mesh.receive(public(2), led), Ok(()));
                let second = proven(3, 1, Body::Heartbeat { commit: 0 });
                let refused = Error::Raft(raft::Error::SecondLeader {
                    term: common::TERM,
                    from: key(3),
                });
                assert_eq!(
                    mesh.receive(public(3), second.clone()),
                    Err(refused.clone())
                );
                let mut forged = second;
                let proof = forged.proof.as_mut().unwrap();
                proof.voters.get_mut(&key(1)).unwrap().as_mut().unwrap().0[63] ^= 1;
                assert_eq!(mesh.receive(public(3), forged), Err(refused));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            });
        }

        #[test]
        fn refuses_an_append_whose_change_is_forged_before_it_steps() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let at = Position {
                    term: common::TERM,
                    index: 1,
                };
                let joint = Voters {
                    incoming: [key(1), key(2)].into(),
                    outgoing: [key(1), key(2), key(3)].into(),
                };
                let mut entry = common::change(2, at, joint);
                let Data::Voters(change) = &mut entry.data else {
                    unreachable!("a change is a voters entry");
                };
                change.signature.as_mut().unwrap().0[63] ^= 1;
                let append = Body::Append {
                    prev: Position::default(),
                    entries: vec![entry],
                    commit: 0,
                };
                let forged = Error::Claim(claim::Error::Forged { signer: key(2) });
                assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Err(forged));
                node.clock().sleep(TICK).await;
                assert!(quiet(&mesh, 2).await);
                assert_eq!(pre_vote_term(&mesh, 2).await, Term(1));
                let probe = Body::Append {
                    prev: at,
                    entries: Vec::new(),
                    commit: 0,
                };
                assert_eq!(mesh.receive(public(2), proven(2, 1, probe)), Ok(()));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply, message(1, 2, Body::AppendReject { hint: 0 }));
            });
        }

        #[test]
        fn a_member_does_not_move_a_node_with_no_voters_to_its_term() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2, 3, 4], &[]).await.unwrap();
                let proof = raft::Proof {
                    grant: raft::Grant::Vote,
                    candidate: key(4),
                    voters: [(key(4), None)].into(),
                };
                let mut lie = message(4, 1, Body::Heartbeat { commit: 0 });
                (lie.term, lie.proof) = (Term(u64::MAX), Some(proof));
                let mut ready = Ready {
                    messages: vec![lie],
                    ..Ready::default()
                };
                common::signer(4).sign(&mut ready);
                let lie = ready.messages.remove(0);
                let refused = Error::NotVoter { from: key(4) };
                assert_eq!(mesh.receive(public(4), lie), Err(refused));
                node.clock().sleep(TICK).await;
                assert!(quiet(&mesh, 4).await);
                // No public call shows the term of a node with no voters.
                assert_eq!(term(&mesh), Term(0));
            });
        }

        #[test]
        fn gives_the_error_of_raft() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let misrouted = Error::Raft(raft::Error::Misrouted { to: key(3) });
                let received =
                    mesh.receive(public(2), message(2, 3, Body::HeartbeatReply));
                assert_eq!(received, Err(misrouted));
            });
        }
    }

    mod unapplied {
        use super::*;
        use crate::common::{TERM, proven_at};

        pub(super) fn later() -> Term {
            Term(common::TERM.0 + 1)
        }

        /// An append of `data` from index 1 in `term`, which commits nothing. Node 2
        /// signs each change in it as its leader.
        fn append(term: Term, data: Vec<Data>) -> Body {
            let entries = iter::zip(1.., data)
                .map(|(index, data)| Entry {
                    at: Position { term, index },
                    data,
                })
                .collect();
            let mut ready = Ready {
                entries,
                ..Ready::default()
            };
            common::signer(2).sign(&mut ready);
            Body::Append {
                prev: Position::default(),
                entries: ready.entries,
                commit: 0,
            }
        }

        /// Until the apply refuses a join of node 4 whose card holds the public key
        /// of voter 2, a request of node 4 under that key passes the sender check and
        /// names node 4. After it, node 4 has no key, and the message is spoofed.
        #[test]
        fn a_join_with_the_public_key_of_a_voter_names_its_node_until_it_applies() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let card = card::Card {
                    public_key: public(2),
                    ..common::member(4).card.card().clone()
                };
                let twin = card::Signed::sign(key(4), card, &common::private(2));
                let joins = [ticket(), join_with(&twin, 7, Stamp::EPOCH), join(5)];
                write(&mesh, changes(&joins)).await;
                let request = message(4, 1, Body::Heartbeat { commit: 0 });
                let refused = Err(Error::NotVoter { from: key(4) });
                assert_eq!(mesh.receive(public(2), request), refused);
                let commit = proven(2, 1, Body::Heartbeat { commit: 3 });
                assert_eq!(mesh.receive(public(2), commit), Ok(()));
                node.clock().sleep(Span::MILLISECOND).await;
                assert_eq!(mesh.member(key(4)), None);
                assert_eq!(mesh.holder(public(2)), Some(key(2)));
                assert_eq!(mesh.holder(public(5)), Some(key(5)));
                let reply = message(4, 1, Body::HeartbeatReply);
                let spoofed = Err(Error::Spoofed { from: key(4) });
                assert_eq!(mesh.receive(public(2), reply), spoofed);
            });
        }

        pub(super) fn changes(changes: &[Change]) -> Vec<Data> {
            let bytes = changes.iter().map(|change| Data::Bytes(encoded(change)));
            bytes.collect()
        }

        /// A change to `incoming` and `outgoing` by leader 2 in `term`, with the
        /// votes of 1, 2 and 3, and no signature yet.
        fn voters(term: Term, incoming: &[u8], outgoing: &[u8]) -> Data {
            let keys = |ids: &[u8]| ids.iter().map(|&id| key(id)).collect();
            let votes = [1, 2, 3].map(|voter| (voter, voter));
            let proof = proven_at(2, 1, term, &votes, Body::HeartbeatReply).proof;
            Data::Voters(raft::Change {
                voters: Voters {
                    incoming: keys(incoming),
                    outgoing: keys(outgoing),
                },
                votes: proof.expect("a proven message holds a proof"),
                signature: None,
            })
        }

        /// A heartbeat from `leader` to node 1 in `term`, with the votes of each
        /// `(voter, signer)`.
        pub(super) fn heartbeat(
            leader: u8,
            term: Term,
            votes: &[(u8, u8)],
        ) -> raft::Message {
            proven_at(leader, 1, term, votes, Body::Heartbeat { commit: 0 })
        }

        /// Gives node 1 the append of `data` from leader 2, and waits for its write.
        pub(super) async fn write(mesh: &Mesh, data: Vec<Data>) {
            let append = proven(2, 1, append(common::TERM, data));
            assert_eq!(mesh.receive(public(2), append), Ok(()));
            mesh.outgoing(key(2)).await.unwrap();
        }

        #[test]
        fn takes_a_vote_of_a_node_whose_join_is_written_and_not_applied() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let mut data = changes(&[join(4)]);
                data.extend([
                    voters(TERM, &[2, 4], &[2, 3]),
                    voters(TERM, &[2, 4], &[]),
                ]);
                write(&mesh, data).await;
                let proven = heartbeat(4, later(), &[(2, 2), (4, 4)]);
                assert_eq!(mesh.receive(public(4), proven), Ok(()));
                assert_eq!(term(&mesh), later());
            });
        }

        #[test]
        fn takes_a_change_from_a_voter_whose_join_is_written_and_not_applied() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let mut data = changes(&[join(4)]);
                data.push(voters(TERM, &[2, 4], &[2, 3]));
                write(&mesh, data).await;
                let answer = mesh.answer(public(4), home(1)).await;
                let leader = Some(key(2));
                assert_eq!(answer, Ok(Message::NotLeader { leader }));
            });
        }

        #[test]
        fn a_vote_of_a_node_with_no_key_proves_nothing() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let unproven = heartbeat(2, later(), &[(2, 2), (4, 4)]);
                let error = raft::Error::Unproven {
                    term: later(),
                    from: key(2),
                };
                assert_eq!(mesh.receive(public(2), unproven), Err(Error::Raft(error)));
                let proven = heartbeat(2, later(), &[(2, 2), (3, 3), (4, 4)]);
                assert_eq!(mesh.receive(public(2), proven), Ok(()));
                assert_eq!(term(&mesh), later());
            });
        }

        // Node 1 holds a stale join of 4 with the key of 5. The true vote of 4
        // fails under it, so the node removes the vote, and 2 and 3 prove the term.
        #[test]
        fn a_vote_that_fails_under_a_written_join_is_removed() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[stale_join()])).await;
                let proven = heartbeat(2, later(), &[(2, 2), (3, 3), (4, 4)]);
                assert_eq!(mesh.receive(public(2), proven), Ok(()));
                assert_eq!(term(&mesh), later());
                let stale = message(2, 1, Body::Heartbeat { commit: 0 });
                assert_eq!(mesh.receive(public(2), stale), Ok(()));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!((reply.body, reply.term), (Body::HeartbeatReply, later()));
                let answer = mesh.outgoing(key(2)).await.unwrap();
                let voters = answer.proof.map(|proof| proof.voters.into_keys());
                let voters: Option<Vec<_>> = voters.map(Iterator::collect);
                assert_eq!(voters, Some(vec![key(2), key(3)]));
            });
        }

        #[test]
        fn a_vote_of_a_node_with_no_key_is_in_no_proof_that_the_node_keeps() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2], &[1, 2]).await.unwrap();
                let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
                assert_eq!(term(&mesh), common::TERM);
                let mut stale = message(2, 1, Body::Heartbeat { commit: 0 });
                stale.term = Term(common::TERM.0 - 1);
                assert_eq!(mesh.receive(public(2), stale), Ok(()));
                let voters = |proof: Option<Proof>| {
                    proof.map(|proof| proof.voters.into_keys().collect::<Vec<_>>())
                };
                let reply = message(1, 2, Body::HeartbeatReply);
                assert_eq!(mesh.outgoing(key(2)).await, Ok(reply));
                let answer = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(answer.body, Body::HeartbeatReply);
                assert_eq!(voters(answer.proof), Some(vec![key(1), key(2)]));
                drop(mesh);
                node.clock().sleep(Span::MILLISECOND).await;
                let (_, stored) = Log::open(node.files(), LOG.into(), create_pool())
                    .await
                    .unwrap();
                assert_eq!(voters(stored.hard.proof), Some(vec![key(1), key(2)]));
            });
        }

        #[test]
        fn a_node_whose_written_joins_name_two_keys_has_none_until_the_apply() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let Change::Join(mut forged) = join(4) else {
                    unreachable!()
                };
                forged.card.card.public_key = public(5);
                write(&mesh, changes(&[ticket(), Change::Join(forged), join(4)])).await;
                let vote = |signer, commit| {
                    let votes = [(2, 2), (3, 3), (4, signer)];
                    proven_at(2, 1, common::TERM, &votes, Body::Heartbeat { commit })
                };
                assert_eq!(mesh.receive(public(2), vote(4, 0)), Ok(()));
                assert_eq!(mesh.receive(public(2), vote(5, 0)), Ok(()));
                assert_eq!(mesh.receive(public(2), vote(4, 3)), Ok(()));
                node.clock().sleep(Span::MILLISECOND).await;
                let admitted = mesh.member(key(4)).map(|member| member.card);
                assert_eq!(admitted, Some(common::member(4).card));
                let forged = Error::Claim(claim::Error::Forged { signer: key(4) });
                assert_eq!(mesh.receive(public(2), vote(5, 3)), Err(forged));
                assert_eq!(mesh.receive(public(2), vote(4, 3)), Ok(()));
            });
        }

        // With no ticket 7 the apply refuses both joins, so nothing else removes
        // their keys.
        #[test]
        fn a_refused_join_gives_its_node_no_key_once_applied() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[join(4), join(6)])).await;
                let commit = proven(2, 1, Body::Heartbeat { commit: 2 });
                assert_eq!(mesh.receive(public(2), commit), Ok(()));
                node.clock().sleep(Span::from_nanos(1_000_000)).await;
                assert_eq!(mesh.member(key(4)), None);
                for id in [4, 6] {
                    let reply = message(id, 1, Body::HeartbeatReply);
                    let spoofed = Error::Spoofed { from: key(id) };
                    assert_eq!(mesh.receive(public(id), reply), Err(spoofed));
                }
            });
        }

        // `raft` reads its configuration from the log that it holds, before the
        // write, so the keys follow that log too.
        #[test]
        fn a_forged_join_gives_no_key_once_a_step_appends_the_real_join() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let Change::Join(mut forged) = join(4) else {
                    unreachable!()
                };
                forged.card.card.public_key = public(5);
                write(&mesh, changes(&[Change::Join(forged)])).await;
                let mut data = changes(&[join(4)]);
                data.extend([
                    voters(TERM, &[2, 4], &[2, 3]),
                    voters(TERM, &[2, 4], &[]),
                ]);
                let entries = iter::zip(2.., data)
                    .map(|(index, data)| Entry {
                        at: Position {
                            term: common::TERM,
                            index,
                        },
                        data,
                    })
                    .collect();
                let append = Body::Append {
                    prev: Position {
                        term: common::TERM,
                        index: 1,
                    },
                    entries,
                    commit: 0,
                };
                assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
                let forged = heartbeat(2, later(), &[(2, 2), (4, 5)]);
                let unproven = raft::Error::Unproven {
                    term: later(),
                    from: key(2),
                };
                assert_eq!(mesh.receive(public(2), forged), Err(Error::Raft(unproven)));
                assert_eq!(term(&mesh), common::TERM);
            });
        }

        // As above, but the forged vote comes in an append whose `prev` is the
        // forged join, below the real join.
        #[test]
        fn an_append_from_between_two_written_joins_takes_no_forged_vote() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[stale_join()])).await;
                let mut data = changes(&[join(4)]);
                data.extend([
                    voters(TERM, &[2, 4], &[2, 3]),
                    voters(TERM, &[2, 4], &[]),
                ]);
                let at = |index| Position {
                    term: common::TERM,
                    index,
                };
                let entries = std::iter::zip(2.., data)
                    .map(|(index, data)| Entry {
                        at: at(index),
                        data,
                    })
                    .collect();
                let append = Body::Append {
                    prev: at(1),
                    entries,
                    commit: 0,
                };
                assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
                let probe = Body::Append {
                    prev: at(1),
                    entries: Vec::new(),
                    commit: 0,
                };
                let forged = proven_at(2, 1, later(), &[(2, 2), (4, 5)], probe);
                let unproven = raft::Error::Unproven {
                    term: later(),
                    from: key(2),
                };
                assert_eq!(mesh.receive(public(2), forged), Err(Error::Raft(unproven)));
                assert_eq!(term(&mesh), common::TERM);
            });
        }

        /// The append of leader 3 that replaces a written join of 4 with the real
        /// one, and makes 4 a voter.
        pub(super) fn replacing_forged() -> raft::Message {
            let mut data = changes(&[join(4)]);
            data.extend([
                voters(later(), &[2, 3, 4], &[2, 3]),
                voters(later(), &[2, 3, 4], &[]),
            ]);
            let replace = append(later(), data);
            proven_at(3, 1, later(), &[(2, 2), (3, 3)], replace)
        }

        /// The refusal of a heartbeat of 3 in the term after [`later`] with a vote
        /// of 4 under the key of 5.
        pub(super) fn forged_unproven() -> Error {
            Error::Raft(raft::Error::Unproven {
                term: Term(common::TERM.0 + 2),
                from: key(3),
            })
        }

        #[test]
        fn a_step_that_replaces_a_forged_join_removes_its_key_before_the_write() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[stale_join()])).await;
                assert_eq!(mesh.receive(public(3), replacing_forged()), Ok(()));
                let next = Term(later().0.checked_add(1).unwrap());
                let forged = heartbeat(3, next, &[(3, 3), (4, 5)]);
                assert_eq!(mesh.receive(public(3), forged), Err(forged_unproven()));
                assert_eq!(term(&mesh), later());
                let reply = message(4, 1, Body::HeartbeatReply);
                let spoofed = Error::Spoofed { from: key(4) };
                assert_eq!(mesh.receive(public(5), reply), Err(spoofed));
            });
        }

        /// The join of node 4 with the key of node 5: a forgery that no leader
        /// committed.
        pub(super) fn stale_join() -> Change {
            let Change::Join(mut forged) = join(4) else {
                unreachable!()
            };
            forged.card.card.public_key = public(5);
            Change::Join(forged)
        }

        /// The log of leader 3 of `later`, which 2 and 3 elected: the join of node
        /// 4 and the change that makes it a voter. Then the change of leader 2 of
        /// the term after, which 2 and 4 elected.
        pub(super) fn replacing() -> Vec<Entry> {
            let next = Term(common::TERM.0 + 2);
            let at = |term, index| Position { term, index };
            let set = |incoming: &[u8], outgoing: &[u8]| Voters {
                incoming: incoming.iter().map(|&id| key(id)).collect(),
                outgoing: outgoing.iter().map(|&id| key(id)).collect(),
            };
            vec![
                Entry {
                    at: at(later(), 1),
                    data: changes(&[join(4)]).remove(0),
                },
                common::change_voted(
                    3,
                    at(later(), 2),
                    set(&[2, 3, 4], &[2, 3]),
                    &[2, 3],
                ),
                common::change_voted(3, at(later(), 3), set(&[2, 3, 4], &[]), &[2, 3]),
                common::change_voted(2, at(next, 4), set(&[2, 3, 4], &[]), &[2, 4]),
            ]
        }

        /// The append of `entries` after `prev` from leader 3 of the term after the
        /// one of `replacing`, to node 1.
        pub(super) fn replace(prev: Position, entries: Vec<Entry>) -> raft::Message {
            let body = Body::Append {
                prev,
                entries,
                commit: 0,
            };
            let last = Term(common::TERM.0 + 3);
            proven_at(3, 1, last, &[(2, 2), (3, 3)], body)
        }

        // Node 1 holds a stale join of node 4 at index 1. The log of leader 3
        // replaces it with the real join, adds 4 to the voters, and holds a change
        // that 4 voted in. The vote fails under the stale key, so the node cuts the
        // run before it, and takes the rest from there.
        #[test]
        fn takes_a_change_voted_by_a_node_whose_stale_join_the_append_replaces() {
            let entries = solo_stored(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[stale_join()])).await;
                let first = replace(Position::default(), replacing());
                assert_eq!(mesh.receive(public(3), first), Ok(()));
                let reply = mesh.outgoing(key(3)).await.unwrap();
                assert_eq!(reply.body, Body::AppendReply { last: 3 });
                let mut entries = replacing();
                let rest = entries.split_off(3);
                let prev = entries[2].at;
                assert_eq!(mesh.receive(public(3), replace(prev, rest)), Ok(()));
                let reply = mesh.outgoing(key(3)).await.unwrap();
                assert_eq!(reply.body, Body::AppendReply { last: 4 });
            });
            assert_eq!(entries, replacing());
        }

        // As above, but the leader probes from index 1, as a leader that backs up
        // one index at a time does. The cut run does not match the stale join, so
        // the node rejects it, and the leader backs up.
        #[test]
        fn rejects_a_probe_above_a_stale_join_that_the_probe_replaces() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[stale_join()])).await;
                let mut entries = replacing();
                let rest = entries.split_off(1);
                let probe = replace(entries[0].at, rest);
                assert_eq!(mesh.receive(public(3), probe), Ok(()));
                let reply = mesh.outgoing(key(3)).await.unwrap();
                assert_eq!(reply.body, Body::AppendReject { hint: 0 });
            });
        }

        /// The log of leader 2 of `TERM`: the real join of 4, the change to the
        /// voters 2 and 4, then a stale join of 4 with the key of 5.
        pub(super) fn stale_second() -> Vec<Data> {
            let mut data = changes(&[join(4)]);
            data.extend([voters(TERM, &[2, 4], &[2, 3]), voters(TERM, &[2, 4], &[])]);
            data.extend(changes(&[stale_join()]));
            data
        }

        /// The append of `leader` of `later`, which 2 and 4 elected, that replaces
        /// the stale join of [`stale_second`] from `prev` below it, with its
        /// chain. And the log that a node holds after it.
        pub(super) fn replace_second(leader: u8) -> (raft::Message, Vec<Entry>) {
            let Body::Append {
                entries: mut log, ..
            } = append(TERM, stale_second())
            else {
                unreachable!()
            };
            let chain = log[1..3]
                .iter()
                .map(|entry| {
                    let Data::Voters(change) = &entry.data else {
                        unreachable!()
                    };
                    raft::Link {
                        at: entry.at,
                        change: change.clone(),
                    }
                })
                .collect();
            let empty = Entry {
                at: Position {
                    term: later(),
                    index: 4,
                },
                data: Data::Empty,
            };
            let body = Body::Append {
                prev: log[2].at,
                entries: vec![empty.clone()],
                commit: 0,
            };
            let mut replace = proven_at(leader, 1, later(), &[(2, 2), (4, 4)], body);
            replace.chain = chain;
            log.truncate(3);
            log.push(empty);
            (replace, log)
        }

        // The real join of 4 is below the change that names it, and the stale join
        // above, so the real key proves the vote of 4, and the node takes the
        // whole log of the leader.
        #[test]
        fn takes_an_append_that_replaces_a_stale_second_join_above_prev() {
            let entries = solo_stored(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, stale_second()).await;
                let (replace, _) = replace_second(2);
                assert_eq!(mesh.receive(public(2), replace), Ok(()));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply.body, Body::AppendReply { last: 4 });
            });
            assert_eq!(entries, replace_second(2).1);
        }

        // The forged join of 4 is below the change that names it too, so the node
        // has no key for 4, and a leader that 2 and 4 elected is unproven here. A
        // defect until #336 builds the voter that checks a join before it stamps it.
        #[test]
        fn a_leader_elected_under_a_change_above_two_written_joins_is_unproven() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let mut data = changes(&[stale_join(), join(4)]);
                data.extend([
                    voters(TERM, &[2, 4], &[2, 3]),
                    voters(TERM, &[2, 4], &[]),
                ]);
                write(&mesh, data).await;
                let elected = heartbeat(2, later(), &[(2, 2), (4, 4)]);
                let unproven = Error::Raft(raft::Error::Unproven {
                    term: later(),
                    from: key(2),
                });
                assert_eq!(mesh.receive(public(2), elected), Err(unproven));
                assert_eq!(term(&mesh), common::TERM);
            });
        }

        // The heartbeat of leader 2 in the term after `later`, with the `votes` of
        // its election and the chain of leader 3 of `later` that makes 4 a voter
        // from index 2.
        fn elected_through_chain(votes: &[(u8, u8)]) -> raft::Message {
            let at = |index| Position {
                term: later(),
                index,
            };
            let set = |outgoing: &[u8]| Voters {
                incoming: [2, 3, 4].map(key).into(),
                outgoing: outgoing.iter().map(|&id| key(id)).collect(),
            };
            let link = |(index, voters)| {
                let entry = common::change_voted(3, at(index), voters, &[2, 3]);
                let Data::Voters(change) = entry.data else {
                    unreachable!()
                };
                raft::Link {
                    at: entry.at,
                    change,
                }
            };
            let next = Term(later().0.checked_add(1).unwrap());
            let mut elected = heartbeat(2, next, votes);
            elected.chain = [(2, set(&[2, 3])), (3, set(&[]))].map(link).into();
            elected
        }

        // The `Unproven` of `elected_through_chain`.
        fn chain_unproven() -> Error {
            Error::Raft(raft::Error::Unproven {
                term: Term(later().0.checked_add(1).unwrap()),
                from: key(2),
            })
        }

        // Node 1 holds the real join of 4 at 1 and a stale join at 2, and the
        // change that names 4 in the chain only. A link proves the entry in the
        // log of its sender, not the joins below it here, so 4 has no key and the
        // leader is unproven. A limit of liveness, until #1623.
        #[test]
        fn a_leader_whose_chain_only_names_a_node_with_two_joins_is_unproven() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[join(4), stale_join()])).await;
                let elected = elected_through_chain(&[(2, 2), (4, 4)]);
                assert_eq!(mesh.receive(public(2), elected), Err(chain_unproven()));
                assert_eq!(term(&mesh), common::TERM);
            });
        }

        // The same log, with the vote of 4 under its stale key: with two joins of 4
        // in the log, the vote is not counted.
        #[test]
        fn a_vote_under_the_stale_key_of_a_node_only_a_chain_names_is_not_counted() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[join(4), stale_join()])).await;
                let elected = elected_through_chain(&[(2, 2), (4, 5)]);
                assert_eq!(mesh.receive(public(2), elected), Err(chain_unproven()));
                assert_eq!(term(&mesh), common::TERM);
            });
        }

        // Only a leader writes a join, so a forged join alone in the log comes from
        // a voter that lies, which `raft` trusts until #882: its key is the key of
        // 4, and a vote under it counts.
        #[test]
        fn a_vote_under_the_key_of_the_one_forged_join_of_a_chain_only_node_counts() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[stale_join()])).await;
                let elected = elected_through_chain(&[(2, 2), (4, 5)]);
                assert_eq!(mesh.receive(public(2), elected), Ok(()));
                assert_eq!(term(&mesh), Term(later().0 + 1));
            });
        }

        // With the real join alone, the same heartbeat is proven.
        #[test]
        fn takes_a_leader_whose_chain_only_names_a_node_with_one_join() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[join(4)])).await;
                let elected = elected_through_chain(&[(2, 2), (4, 4)]);
                assert_eq!(mesh.receive(public(2), elected), Ok(()));
                assert_eq!(term(&mesh), Term(later().0 + 1));
            });
        }

        // The first change of the log names 2 and 3 only, so it is not the line for
        // 4: both joins of 4 are below the change that names 4, and a vote that the
        // stale key signs does not count.
        #[test]
        fn a_change_that_does_not_name_a_node_is_not_its_line() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let mut data = changes(&[stale_join()]);
                data.push(voters(TERM, &[2, 3], &[2, 3]));
                data.extend(changes(&[join(4)]));
                data.extend([
                    voters(TERM, &[2, 4], &[2, 3]),
                    voters(TERM, &[2, 4], &[]),
                ]);
                write(&mesh, data).await;
                let forged = heartbeat(2, later(), &[(2, 2), (4, 5)]);
                let unproven = Error::Raft(raft::Error::Unproven {
                    term: later(),
                    from: key(2),
                });
                assert_eq!(mesh.receive(public(2), forged), Err(unproven));
                assert_eq!(term(&mesh), common::TERM);
            });
        }

        // A later change that names 4 again does not move the line: the joins below
        // the first one decide.
        #[test]
        fn the_first_change_that_names_a_node_decides() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let mut data = stale_second();
                data.push(voters(TERM, &[2, 3, 4], &[2, 4]));
                write(&mesh, data).await;
                let (replace, _) = replace_second(2);
                assert_eq!(mesh.receive(public(2), replace), Ok(()));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply.body, Body::AppendReply { last: 4 });
            });
        }

        // Node 1 campaigns under the change to 1, 2 and 4, and 4 answers on the
        // stream of the key of its stale join with a vote that its real key signs.
        #[test]
        fn a_vote_reply_that_fails_under_the_written_join_of_its_sender_is_forged() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let mut data = changes(&[stale_join()]);
                data.push(voters(TERM, &[1, 2, 4], &[1, 2, 3]));
                write(&mesh, data).await;
                let sent = mesh.outgoing(key(2)).await.unwrap();
                assert!(matches!(sent.body, Body::PreVote { .. }));
                let answer = Answer::Granted(None);
                let reply = raft::Message {
                    term: sent.term,
                    ..message(2, 1, Body::PreVoteReply { answer })
                };
                assert_eq!(mesh.receive(public(2), sign(2, reply)), Ok(()));
                let vote = loop {
                    let sent = mesh.outgoing(key(4)).await.unwrap();
                    if matches!(sent.body, Body::Vote { .. }) {
                        break sent;
                    }
                };
                let reply = raft::Message {
                    term: vote.term,
                    ..message(4, 1, Body::VoteReply { answer })
                };
                let forged = Error::Claim(claim::Error::Forged { signer: key(4) });
                assert_eq!(mesh.receive(public(5), sign(4, reply)), Err(forged));
            });
        }

        /// `message` with each claim signed by the key of `signer`.
        fn sign(signer: u8, message: raft::Message) -> raft::Message {
            let mut ready = Ready {
                messages: vec![message],
                ..Ready::default()
            };
            common::signer(signer).sign(&mut ready);
            ready.messages.remove(0)
        }

        // As above, with node 4 as the leader: its key is the real one.
        #[test]
        fn a_leader_with_a_stale_second_join_above_the_change_is_not_spoofed() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, stale_second()).await;
                let (replace, _) = replace_second(4);
                assert_eq!(mesh.receive(public(4), replace), Ok(()));
                let reply = mesh.outgoing(key(4)).await.unwrap();
                assert_eq!(reply.body, Body::AppendReply { last: 4 });
            });
        }

        #[test]
        fn a_join_keeps_its_key_when_a_step_follows_before_the_write() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let joined = proven(2, 1, append(common::TERM, changes(&[join(4)])));
                assert_eq!(mesh.receive(public(2), joined), Ok(()));
                let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
                let reply = message(4, 1, Body::HeartbeatReply);
                assert_eq!(mesh.receive(public(4), reply), Ok(()));
            });
        }

        #[test]
        fn a_step_that_replaces_from_below_a_written_join_removes_its_key() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[home(1), join(4)])).await;
                let replace = append(later(), changes(&[home(1)]));
                let replace = proven_at(3, 1, later(), &[(2, 2), (3, 3)], replace);
                assert_eq!(mesh.receive(public(3), replace), Ok(()));
                let reply = message(4, 1, Body::HeartbeatReply);
                let spoofed = Error::Spoofed { from: key(4) };
                assert_eq!(mesh.receive(public(4), reply), Err(spoofed));
            });
        }

        #[test]
        fn a_step_that_replaces_from_below_an_unwritten_join_removes_its_key() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let joined = append(common::TERM, changes(&[home(1), join(4)]));
                assert_eq!(mesh.receive(public(2), proven(2, 1, joined)), Ok(()));
                let replace = append(later(), changes(&[home(1)]));
                let replace = proven_at(3, 1, later(), &[(2, 2), (3, 3)], replace);
                assert_eq!(mesh.receive(public(3), replace), Ok(()));
                let reply = message(4, 1, Body::HeartbeatReply);
                let spoofed = Error::Spoofed { from: key(4) };
                assert_eq!(mesh.receive(public(4), reply), Err(spoofed));
            });
        }

        #[test]
        fn a_step_that_replaces_from_between_two_joins_keeps_only_the_first() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[join(4), join(6)])).await;
                let at = |index| Position {
                    term: later(),
                    index,
                };
                let replace = Body::Append {
                    prev: Position {
                        term: common::TERM,
                        index: 1,
                    },
                    entries: vec![Entry {
                        at: at(2),
                        data: changes(&[home(1)]).remove(0),
                    }],
                    commit: 0,
                };
                let replace = proven_at(3, 1, later(), &[(2, 2), (3, 3)], replace);
                assert_eq!(mesh.receive(public(3), replace), Ok(()));
                let reply = |id| message(id, 1, Body::HeartbeatReply);
                assert_eq!(mesh.receive(public(4), reply(4)), Ok(()));
                let spoofed = Error::Spoofed { from: key(6) };
                assert_eq!(mesh.receive(public(6), reply(6)), Err(spoofed));
            });
        }

        #[test]
        fn a_replace_from_below_two_written_joins_drops_both() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[join(4), join(5)])).await;
                let forged =
                    |leader, term| heartbeat(leader, term, &[(2, 2), (3, 3), (4, 5)]);
                let votes = [(2, 2), (3, 3)];
                let replace = append(later(), changes(&[home(1)]));
                let replace = proven_at(3, 1, later(), &votes, replace);
                assert_eq!(mesh.receive(public(3), replace), Ok(()));
                mesh.outgoing(key(3)).await.unwrap();
                assert_eq!(mesh.receive(public(3), forged(3, later())), Ok(()));
            });
        }

        #[test]
        fn a_reopen_keeps_a_written_join_until_a_replace() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[join(4)])).await;
                drop(mesh);
                node.clock().sleep(Span::MILLISECOND).await;
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let reply = || message(4, 1, Body::HeartbeatReply);
                assert_eq!(mesh.receive(public(4), reply()), Ok(()));
                let votes = [(2, 2), (3, 3)];
                let replace = append(later(), changes(&[home(1)]));
                let replace = proven_at(3, 1, later(), &votes, replace);
                assert_eq!(mesh.receive(public(3), replace), Ok(()));
                let spoofed = Error::Spoofed { from: key(4) };
                assert_eq!(mesh.receive(public(4), reply()), Err(spoofed));
            });
        }

        #[test]
        fn a_written_join_does_not_move_the_key_of_an_applied_member() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[ticket(), join(4)])).await;
                let commit = proven(2, 1, Body::Heartbeat { commit: 2 });
                assert_eq!(mesh.receive(public(2), commit), Ok(()));
                node.clock().sleep(Span::MILLISECOND).await;
                let admitted = mesh.member(key(4)).map(|member| member.card);
                assert_eq!(admitted, Some(common::member(4).card));
                let Change::Join(mut forged) = join(4) else {
                    unreachable!()
                };
                forged.card.card.public_key = public(5);
                let at = |index| Position {
                    term: common::TERM,
                    index,
                };
                let next = Body::Append {
                    prev: at(2),
                    entries: vec![Entry {
                        at: at(3),
                        data: changes(&[Change::Join(forged)]).remove(0),
                    }],
                    commit: 2,
                };
                assert_eq!(mesh.receive(public(2), proven(2, 1, next)), Ok(()));
                let reply = || message(4, 1, Body::HeartbeatReply);
                assert_eq!(mesh.receive(public(4), reply()), Ok(()));
                let spoofed = Error::Spoofed { from: key(4) };
                assert_eq!(mesh.receive(public(5), reply()), Err(spoofed));
            });
        }

        #[test]
        fn a_reopen_keeps_each_written_join() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[join(4), join(6)])).await;
                drop(mesh);
                node.clock().sleep(Span::MILLISECOND).await;
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                for id in [4, 6] {
                    let reply = message(id, 1, Body::HeartbeatReply);
                    assert_eq!(mesh.receive(public(id), reply), Ok(()), "{id}");
                }
            });
        }

        #[test]
        fn a_join_after_the_synced_entry_gives_its_key_before_the_write() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let homes = append(common::TERM, changes(&[home(1), home(1)]));
                assert_eq!(mesh.receive(public(2), proven(2, 1, homes)), Ok(()));
                let at = |index| Position {
                    term: common::TERM,
                    index,
                };
                let mut data = changes(&[join(4), home(1)]).into_iter();
                let entries = [3, 4].map(|index| Entry {
                    at: at(index),
                    data: data.next().unwrap(),
                });
                let next = Body::Append {
                    prev: at(2),
                    entries: entries.into(),
                    commit: 0,
                };
                assert_eq!(mesh.receive(public(2), proven(2, 1, next)), Ok(()));
                let reply = message(4, 1, Body::HeartbeatReply);
                assert_eq!(mesh.receive(public(4), reply), Ok(()));
            });
        }

        // The test clears the joins that `sync` took. A later sync that decodes an
        // entry again gives the key back, so the key shows each second decode.
        #[test]
        fn a_sync_decodes_no_entry_that_an_earlier_sync_took() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let joined = append(common::TERM, changes(&[home(1), join(4)]));
                assert_eq!(mesh.receive(public(2), proven(2, 1, joined)), Ok(()));
                mesh.group.borrow_mut().unapplied.clear();
                let at = |index| Position {
                    term: common::TERM,
                    index,
                };
                let next = Body::Append {
                    prev: at(2),
                    entries: vec![Entry {
                        at: at(3),
                        data: changes(&[home(1)]).remove(0),
                    }],
                    commit: 0,
                };
                assert_eq!(mesh.receive(public(2), proven(2, 1, next)), Ok(()));
                let reply = message(4, 1, Body::HeartbeatReply);
                let spoofed = Error::Spoofed { from: key(4) };
                assert_eq!(mesh.receive(public(4), reply), Err(spoofed));
            });
        }

        #[test]
        fn a_step_that_replaces_an_unwritten_join_above_an_unwritten_entry_removes_its_key()
         {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                let joined = append(common::TERM, changes(&[home(1), join(4)]));
                assert_eq!(mesh.receive(public(2), proven(2, 1, joined)), Ok(()));
                let replace = Body::Append {
                    prev: Position {
                        term: common::TERM,
                        index: 1,
                    },
                    entries: vec![Entry {
                        at: Position {
                            term: later(),
                            index: 2,
                        },
                        data: changes(&[home(1)]).remove(0),
                    }],
                    commit: 0,
                };
                let replace = proven_at(3, 1, later(), &[(2, 2), (3, 3)], replace);
                assert_eq!(mesh.receive(public(3), replace), Ok(()));
                let reply = message(4, 1, Body::HeartbeatReply);
                let spoofed = Error::Spoofed { from: key(4) };
                assert_eq!(mesh.receive(public(4), reply), Err(spoofed));
            });
        }

        #[test]
        fn a_join_that_this_leader_proposes_gives_a_key_before_the_write() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[1]).await.unwrap();
                lead(&mesh, &node.clock(), home(1)).await;
                assert!(started(&mesh, join(4)).await.is_pending());
                let reply = message(4, 1, Body::HeartbeatReply);
                assert_eq!(mesh.receive(public(4), reply), Ok(()));
            });
        }

        #[test]
        fn propose_voters_refuses_a_node_that_is_not_a_member() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
                let at = lead(&mesh, &node.clock(), home(1)).await;
                let refused = mesh.propose_voters([key(1), key(3)].into()).await;
                assert_eq!(refused, Err(Error::NotMember(key(3))));
                let text = format!("node {} is not a member of the region", key(3));
                assert_eq!(refused.unwrap_err().to_string(), text);
                let proposed = mesh.propose_voters([key(1), key(2)].into()).await;
                assert_eq!(proposed, Ok(after(at, 1)));
            });
        }

        #[test]
        fn propose_voters_refuses_a_node_whose_join_is_not_applied() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2], &[2]).await.unwrap();
                write(&mesh, changes(&[join(3)])).await;
                let refused = mesh.propose_voters([key(2), key(3)].into()).await;
                assert_eq!(refused, Err(Error::NotMember(key(3))));
                let follower = raft::Error::NotLeader {
                    leader: Some(key(2)),
                };
                let proposed = mesh.propose_voters([key(2)].into()).await;
                assert_eq!(proposed, Err(Error::Raft(follower)));
            });
        }
    }

    #[test]
    fn a_lone_voter_leads_after_one_election_timeout() {
        for seed in 0..16 {
            solo_at(seed, move |node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
                let clock = node.clock();
                let opened = clock.now();
                let (asked, _) = lead_at(&mesh, &clock, home(1)).await;
                let asked = asked - opened;
                // The timeout is 10 to 19 ticks, and `lead_at` asks once per tick.
                let timeout = seconds(1)..=seconds(2);
                assert!(timeout.contains(&asked), "run {seed}: it led at {asked}");
            });
        }
    }

    #[test]
    fn an_input_does_not_wait_for_the_next_tick() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let clock = node.clock();
            let opened = clock.now();
            // The group now waits for its first tick.
            clock.sleep(Span::MILLISECOND).await;
            let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
            assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
            let reply = mesh.outgoing(key(2)).await.unwrap();
            assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            let waited = clock.now() - opened;
            assert!(waited < TICK, "the reply came after {waited}");
        });
    }

    #[test]
    fn a_proposal_does_not_wait_for_the_next_tick() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let clock = node.clock();
            let mut watch = mesh.watch(INDEX);
            lead(&mesh, &clock, home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            // The group now waits for a tick.
            clock.sleep(TICK).await;
            clock.sleep(Span::MILLISECOND).await;
            let proposed = clock.now();
            mesh.propose(home(2)).await.unwrap();
            assert_eq!(watch.next().await, Ok(Some(key(2))));
            let waited = clock.now() - proposed;
            let half = Span::from_nanos(TICK.nanos() / 2);
            assert!(waited < half, "the home came after {waited}");
        });
    }

    #[test]
    fn a_message_waits_for_the_write_of_its_ready() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let waiting = Rc::new(RefCell::new(None));
            let (other, slot) = (mesh.clone(), Rc::clone(&waiting));
            tasks.spawn(async move {
                let message = other.outgoing(key(2)).await;
                *slot.borrow_mut() = Some(message);
            });
            node.clock().sleep(Span::MILLISECOND).await;
            let stopped = Error::Stopped(fail_sync(&node));
            let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
            assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
            node.clock().sleep(TICK).await;
            assert_eq!(waiting.take(), Some(Err(stopped)));
        });
    }

    #[test]
    fn a_full_pool_holds_a_change_until_a_block_is_free() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let held = fill(&pool);
            assert!(started(&mesh, home(2)).await.is_pending());
            node.clock().sleep(Span::from_nanos(TICK.nanos() * 3)).await;
            assert!(now(pin!(watch.next())).await.is_pending());
            drop(held);
            assert_eq!(watch.next().await, Ok(Some(key(2))));
        });
    }

    #[test]
    fn a_burst_of_changes_that_the_pool_cannot_hold_commits() {
        solo(|node, tasks| async move {
            let config = Config {
                pool: small_pool(),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            let first = lead(&mesh, &node.clock(), home(1)).await;
            // One poll starts each proposal, so one `Ready` holds the 99 entries.
            let mut calls: Vec<_> = (2..=100)
                .map(|id| Box::pin(mesh.propose(home(id))))
                .collect();
            let waits = poll_each(&mut calls).await;
            assert!(waits.iter().all(Poll::is_pending));
            node.clock().sleep(Span::from_nanos(TICK.nanos() * 3)).await;
            let positions = poll_each(&mut calls).await;
            let expected: Vec<_> = (1..=99)
                .map(|count| Poll::Ready(Ok(after(first, count))))
                .collect();
            assert_eq!(positions, expected);
            assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(100))));
        });
    }

    #[test]
    fn a_full_pool_holds_a_message_until_a_block_is_free() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &IDS, &IDS).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            let held = fill(&pool);
            let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
            assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
            node.clock().sleep(Span::from_nanos(TICK.nanos() * 3)).await;
            assert!(quiet(&mesh, 2).await);
            drop(held);
            let reply = mesh.outgoing(key(2)).await.unwrap();
            assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
        });
    }

    #[test]
    fn a_group_gets_no_tick_while_its_write_waits_for_a_block() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &IDS, &IDS).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            let held = fill(&pool);
            let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
            assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
            // Longer than each election timeout.
            node.clock()
                .sleep(Span::from_nanos(TICK.nanos() * 30))
                .await;
            drop(held);
            let reply = mesh.outgoing(key(2)).await.unwrap();
            assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            node.clock().sleep(TICK).await;
            assert!(quiet(&mesh, 2).await);
            assert!(quiet(&mesh, 3).await);
        });
    }

    #[test]
    fn a_group_that_waits_for_a_block_takes_no_change() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            let first = lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let held = fill(&pool);
            let mut waits = pin!(mesh.propose(home(2)));
            assert!(now(waits.as_mut()).await.is_pending());
            // The write runs again at each tick, and the group refuses after each.
            for _ in 0..2 {
                node.clock().sleep(TICK).await;
                let refused = mesh.propose(home(3)).await.unwrap_err();
                assert_eq!(refused, exhausted(93));
                assert_eq!(
                    refused.to_string(),
                    "the pool has no block for the mesh now: pool is full: asked for \
                     93 bytes, 64 bytes free"
                );
            }
            drop(held);
            assert_eq!(waits.await, Ok(after(first, 1)));
            // The change that the group refused took no position.
            assert_eq!(mesh.propose(home(4)).await, Ok(after(first, 2)));
            assert_eq!(watch.next().await, Ok(Some(key(4))));
        });
    }

    // The write that gets its block is in disk calls for a time.
    #[test]
    fn a_group_that_waited_for_a_block_takes_no_change_until_its_write_ends() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            let first = lead(&mesh, &node.clock(), home(1)).await;
            let held = fill(&pool);
            let mut waits = pin!(mesh.propose(home(2)));
            assert!(now(waits.as_mut()).await.is_pending());
            node.clock().sleep(TICK).await;
            drop(held);
            let ended = loop {
                if let Poll::Ready(ended) = now(waits.as_mut()).await {
                    break ended;
                }
                let refused = started(&mesh, home(3)).await;
                assert_eq!(refused, Poll::Ready(Err(exhausted(93))));
                node.clock().sleep(Span::from_nanos(10_000)).await;
            };
            assert_eq!(ended, Ok(after(first, 1)));
            assert_eq!(mesh.propose(home(4)).await, Ok(after(first, 2)));
        });
    }

    #[test]
    fn a_group_that_waits_gives_the_cause_of_its_last_try() {
        solo(|node, tasks| async move {
            let budget = block::Config { budget: 4096 };
            let (memory, switch) = Scarce::new(budget.reservation());
            let pool = Rc::new(Pool::new(budget, memory));
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            let first = lead(&mesh, &node.clock(), home(1)).await;
            let held = fill(&pool);
            let mut waits = pin!(mesh.propose(home(2)));
            assert!(now(waits.as_mut()).await.is_pending());
            node.clock().sleep(TICK).await;
            let refused = started(&mesh, home(3)).await;
            assert_eq!(refused, Poll::Ready(Err(exhausted(93))));
            switch.refuse();
            drop(held);
            node.clock().sleep(TICK).await;
            let cause = block::Error::Refused { requested: 93 };
            let refused = started(&mesh, home(3)).await;
            assert_eq!(refused, Poll::Ready(Err(Error::Pool(cause))));
            switch.allow();
            assert_eq!(waits.await, Ok(after(first, 1)));
        });
    }

    #[test]
    fn a_group_that_waits_for_a_block_takes_no_message() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &IDS, &IDS).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            let held = fill(&pool);
            let heartbeat = || proven(2, 1, Body::Heartbeat { commit: 0 });
            assert_eq!(mesh.receive(public(2), heartbeat()), Ok(()));
            for _ in 0..2 {
                node.clock().sleep(TICK).await;
                let refused = mesh.receive(public(2), heartbeat());
                assert_eq!(refused, Err(exhausted(327)));
            }
            drop(held);
            let reply = mesh.outgoing(key(2)).await.unwrap();
            assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            // The heartbeats that the group refused get no reply.
            node.clock().sleep(TICK).await;
            assert!(quiet(&mesh, 2).await);
            assert_eq!(mesh.receive(public(2), heartbeat()), Ok(()));
        });
    }

    // The group drops each message and each forwarded change in a wait, so it pays
    // for no check of one.
    #[test]
    fn a_group_that_waits_for_a_block_checks_the_wait_first() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &[1, 2, 3, 4], &IDS).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            let held = fill(&pool);
            let heartbeat = || proven(2, 1, Body::Heartbeat { commit: 0 });
            let mut forged = heartbeat();
            let proof = forged.proof.as_mut().unwrap();
            proof.voters.get_mut(&key(3)).unwrap().as_mut().unwrap().0[63] ^= 1;
            let stranger = message(4, 1, Body::Heartbeat { commit: 0 });
            let messages = [
                (public(3), heartbeat(), Error::Spoofed { from: key(2) }),
                (public(4), stranger, Error::NotVoter { from: key(4) }),
                (
                    public(2),
                    forged,
                    Error::Claim(claim::Error::Forged { signer: key(3) }),
                ),
            ];
            assert_eq!(mesh.receive(public(2), heartbeat()), Ok(()));
            node.clock().sleep(TICK).await;
            for (peer, message, _) in messages.clone() {
                assert_eq!(mesh.receive(peer, message), Err(exhausted(327)));
            }
            let answer = mesh.answer(public(4), home(4)).await;
            assert_eq!(answer, Err(exhausted(327)));
            drop(held);
            let reply = mesh.outgoing(key(2)).await.unwrap();
            assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
            for (peer, message, refused) in messages {
                assert_eq!(mesh.receive(peer, message), Err(refused));
            }
            let answer = mesh.answer(public(4), home(4)).await;
            assert_eq!(answer, Err(Error::PeerNotVoter { peer: public(4) }));
        });
    }

    #[test]
    fn a_group_that_waits_for_refused_memory_gives_that_cause() {
        solo(|node, tasks| async move {
            let budget = block::Config { budget: 4 << 20 };
            let (memory, switch) = Scarce::new(budget.reservation());
            let config = Config {
                pool: Rc::new(Pool::new(budget, memory)),
                ..config(&node, &tasks, 1, &IDS, &IDS).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            switch.refuse();
            let heartbeat = || proven(2, 1, Body::Heartbeat { commit: 0 });
            assert_eq!(mesh.receive(public(2), heartbeat()), Ok(()));
            node.clock().sleep(TICK).await;
            let refused = mesh.receive(public(2), heartbeat()).unwrap_err();
            let cause = block::Error::Refused { requested: 327 };
            assert_eq!(refused, Error::Pool(cause));
            assert_eq!(
                refused.to_string(),
                "the pool has no block for the mesh now: the system refused memory \
                 for a block of 327 bytes"
            );
        });
    }

    #[test]
    fn a_pool_with_no_block_of_one_sector_does_not_open() {
        solo(|node, tasks| async move {
            let budget = block::Config { budget: 0 };
            let memory = block::Heap::new(budget.reservation());
            let config = Config {
                pool: Rc::new(Pool::new(budget, memory)),
                ..config(&node, &tasks, 1, &IDS, &IDS).await
            };
            let cause = block::Error::TooLarge {
                requested: 512,
                largest: 0,
            };
            let error = Error::Log(log::Error::Pool(cause));
            assert_eq!(Mesh::start(config).await.err(), Some(error));
        });
    }

    #[test]
    fn memory_that_the_system_refuses_holds_a_message_until_it_commits() {
        solo(|node, tasks| async move {
            let budget = block::Config { budget: 4 << 20 };
            let (memory, switch) = Scarce::new(budget.reservation());
            let pool = Rc::new(Pool::new(budget, memory));
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &IDS, &IDS).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            switch.refuse();
            let refused = block::Error::Refused { requested: 1 };
            assert_eq!(pool.alloc(1).err(), Some(refused));
            let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
            assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
            node.clock().sleep(Span::from_nanos(TICK.nanos() * 3)).await;
            assert!(quiet(&mesh, 2).await);
            switch.allow();
            let reply = mesh.outgoing(key(2)).await.unwrap();
            assert_eq!(reply, message(1, 2, Body::HeartbeatReply));
        });
    }

    #[test]
    fn a_dropped_mesh_frees_its_log_when_the_pool_is_full() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            let mesh = Mesh::start(config).await.unwrap();
            lead(&mesh, &node.clock(), home(1)).await;
            node.clock().sleep(TICK).await;
            let _held = fill(&pool);
            assert!(started(&mesh, home(2)).await.is_pending());
            node.clock().sleep(TICK).await;
            drop(mesh);
            node.clock().sleep(TICK).await;
            let again = open(&node, &tasks, 1, &[1], &[1]).await;
            assert_eq!(again.err(), None);
        });
    }

    #[test]
    fn a_change_waits_for_the_write_of_its_ready() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            lead(&mesh, &node.clock(), home(1)).await;
            let seen = Rc::new(RefCell::new(Vec::new()));
            let (mut watch, slot) = (mesh.watch(INDEX), Rc::clone(&seen));
            // Its own task, so that it sees each home before the write ends.
            tasks.spawn(async move {
                loop {
                    let next = watch.next().await;
                    let stopped = next.is_err();
                    slot.borrow_mut().push(next);
                    if stopped {
                        return;
                    }
                }
            });
            node.clock().sleep(TICK).await;
            let cause = fail_sync(&node);
            let stopped = Error::Stopped(cause.clone());
            assert_eq!(mesh.propose(home(2)).await, Err(stopped));
            node.clock().sleep(TICK).await;
            assert_eq!(seen.take(), [Ok(Some(key(1))), Err(cause)]);
        });
    }

    #[test]
    fn a_log_that_cannot_write_stops_the_group() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let waiting = Rc::new(RefCell::new(None));
            let (other, slot) = (mesh.clone(), Rc::clone(&waiting));
            tasks.spawn(async move {
                let message = other.outgoing(key(2)).await;
                *slot.borrow_mut() = Some(message);
            });
            let cause = fail_sync(&node);
            let stopped = Error::Stopped(cause.clone());
            assert_eq!(mesh.propose(home(2)).await, Err(stopped.clone()));
            assert_eq!(watch.next().await, Err(cause.clone()));
            assert_eq!(stopped.to_string(), format!("the group stopped: {cause}"));
            node.clock().sleep(TICK).await;
            assert_eq!(waiting.take(), Some(Err(stopped.clone())));
            assert_eq!(mesh.propose(home(2)).await, Err(stopped.clone()));
            let reply = message(2, 1, Body::HeartbeatReply);
            assert_eq!(mesh.receive(public(2), reply), Err(stopped));
        });
    }

    #[test]
    fn a_power_cut_keeps_a_home_after_a_failed_sync_and_a_new_open() {
        let mut lost = Vec::new();
        let mut gave = 0_usize;
        for run in 0..64 {
            let mut sim = Sim::new(sim::Config {
                seed: run,
                ..sim::Config::default()
            });
            let node = sim.node(sim::node::Config::default());
            sim.run_on(&node, |node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
                let mut watch = mesh.watch(INDEX);
                lead(&mesh, &node.clock(), home(1)).await;
                assert_eq!(watch.next().await, Ok(Some(key(1))));
                let cause = fail_sync(&node);
                let stopped = Error::Stopped(cause.clone());
                assert_eq!(mesh.propose(home(2)).await, Err(stopped));
                assert_eq!(watch.next().await, Err(cause));
                drop(mesh);
                let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
                let mut watch = mesh.watch(INDEX);
                lead(&mesh, &node.clock(), home(3)).await;
                while watch.next().await.unwrap() != Some(key(3)) {}
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            let changes = sim
                .run_on(&node, |node, _| async move {
                    let files = node.files();
                    let (_, stored) =
                        Log::open(files, LOG.into(), create_pool()).await.unwrap();
                    let changes = stored.entries.into_iter().map(|entry| {
                        let Data::Bytes(bytes) = entry.data else {
                            return None;
                        };
                        Change::decode(&bytes).ok()
                    });
                    changes.collect::<Vec<_>>()
                })
                .unwrap();
            if changes.contains(&Some(home(2))) {
                gave = gave.saturating_add(1);
            }
            let end = changes.last().cloned().flatten();
            if end != Some(home(3)) {
                lost.push((run, end));
            }
        }
        assert_eq!(lost, [], "(run, the last change after the power cut)");
        assert!(
            gave > 16,
            "runs in which the open gave the failed change: {gave}"
        );
    }

    #[test]
    fn a_committed_entry_that_is_not_a_change_stops_the_group() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            let at = Position {
                term: Term(5),
                index: 1,
            };
            let data = Data::Bytes(vec![9]);
            let append = Body::Append {
                prev: Position::default(),
                entries: vec![Entry { at, data }],
                commit: 1,
            };
            assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
            let cause = Stopped::Change {
                at,
                cause: Unknown::Kind { kind: 9 },
            };
            assert_eq!(watch.next().await, Err(cause.clone()));
            let stopped = Error::Stopped(cause);
            let text = "the group stopped: the committed entry at index 1 of term 5 is \
                        not a change: change kind 9 is unknown";
            assert_eq!(stopped.to_string(), text);
            // The node does not lead, and the stop comes first.
            assert_eq!(mesh.propose(home(1)).await, Err(stopped.clone()));
            assert_eq!(mesh.answer(public(2), home(2)).await, Err(stopped));
        });
    }

    #[test]
    fn a_committed_change_of_zero_bytes_stops_the_group() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            let at = Position {
                term: Term(5),
                index: 1,
            };
            let data = Data::Bytes(Vec::new());
            let append = Body::Append {
                prev: Position::default(),
                entries: vec![Entry { at, data }],
                commit: 1,
            };
            assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
            let cause = Stopped::Change {
                at,
                cause: Unknown::Empty,
            };
            assert_eq!(watch.next().await, Err(cause.clone()));
            let stopped = Error::Stopped(cause);
            let text = "the group stopped: the committed entry at index 1 of term 5 is \
                        not a change: a change of 0 bytes has no kind";
            assert_eq!(stopped.to_string(), text);
        });
    }

    /// Ticket `id`, which admits `plant.*` until `expiry`, any number of times when
    /// `reusable`.
    fn ticket_of(id: u8, prefix: &str, reusable: bool, expiry: Stamp) -> Change {
        let options = Options {
            prefix: prefix.parse().unwrap(),
            reusable,
            expiry,
            ephemeral: None,
        };
        Change::Ticket {
            public_key: public(id),
            options,
        }
    }

    /// Ticket 7, which admits `plant.*` any number of times.
    fn ticket() -> Change {
        ticket_of(7, "plant", true, Stamp::from_nanos(1))
    }

    fn unchecked(card: &card::Signed) -> card::Unchecked {
        card::Unchecked {
            key: card.key(),
            card: card.card().clone(),
            signature: *card.signature(),
        }
    }

    /// The join at `at` of the node of `card`, which ticket `ticket` admits.
    fn join_with(card: &card::Signed, ticket: u8, at: Stamp) -> Change {
        Change::Join(Box::new(Join {
            ticket: public(ticket),
            at,
            card: unchecked(card),
            admission: common::ticket(ticket).admission(card),
            status: common::status([]),
        }))
    }

    /// The join of node `id` as `plant.node<id>`, which ticket 7 admits.
    fn join(id: u8) -> Change {
        join_with(&common::member(id).card, 7, Stamp::EPOCH)
    }

    /// The request of node `id` as `plant.node<id>`, which ticket `ticket` admits, with
    /// the status channels `status`.
    fn request(id: u8, ticket: u8, status: &[&str]) -> Request {
        let card = common::member(id).card;
        Request {
            ticket: public(ticket),
            card: unchecked(&card),
            admission: common::ticket(ticket).admission(&card),
            status: status.iter().map(|name| name.parse().unwrap()).collect(),
        }
    }

    fn encoded(change: &Change) -> Vec<u8> {
        let mut data = Vec::new();
        change.encode(&mut data);
        data
    }

    // A body that does not decode is a refusal on every node of this build, not a
    // stop, so one voter that proposes bad bytes cannot halt the region.
    #[test]
    fn a_committed_join_that_does_not_decode_changes_nothing() {
        let mut over = encoded(&join(5));
        over.truncate(over.len() - 8);
        over.extend(common::status_bytes(65));
        let changes = [
            encoded(&ticket()),
            over,
            encoded(&join(4)),
            encoded(&home(1)),
        ];
        let mut cluster = Cluster::new(1);
        cluster.script_each(&changes);
        cluster.start();
        cluster.run(seconds(5));
        let board = std::mem::take(&mut *cluster.board.lock().unwrap());
        assert_eq!(board.led, vec![board.led[0]; 4]);
        assert_eq!(board.homes, each(&[None, Some(key(1))]));
        let members = IDS.map(|id| (id, [1, 2, 3, 4].into())).into();
        assert_eq!(board.members, members);
    }

    #[test]
    fn a_committed_join_with_a_forged_card_changes_nothing() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            lead(&mesh, &node.clock(), ticket()).await;
            let Change::Join(mut forged) = join(3) else {
                unreachable!()
            };
            forged.card.signature[0] ^= 1;
            mesh.propose(Change::Join(forged)).await.unwrap();
            mesh.propose(join(4)).await.unwrap();
            mesh.propose(home(1)).await.unwrap();
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            assert_eq!(mesh.member(key(3)), None);
            let admitted = mesh.member(key(4)).map(|member| member.card);
            assert_eq!(admitted, Some(common::member(4).card));
        });
    }

    /// The leader of the cluster after its first change commits, and a follower.
    fn roles(cluster: &Cluster) -> (u8, u8) {
        let led = cluster.board.lock().unwrap().led.clone();
        let &[leader] = led.as_slice() else {
            panic!("the group took a proposal from each of {led:?}");
        };
        (leader, IDS.into_iter().find(|&id| id != leader).unwrap())
    }

    // A follower that gets no answer forwards the join again, so the leader applies it
    // twice: the second is refused before the ticket counts a use.
    #[test]
    fn a_join_that_a_follower_stamps_admits_the_node_on_every_member() {
        let hour = NOW + Span::HOUR;
        let mut cluster = Cluster::new(2);
        cluster.script_each(&[encoded(&ticket_of(8, "plant", true, hour))]);
        cluster.start();
        cluster.run(seconds(5));
        let (leader, follower) = roles(&cluster);
        let names = ["clock.error", "clock.offset"];
        let request = request(4, 8, &names);
        cluster
            .board
            .lock()
            .unwrap()
            .requests
            .insert(follower, request);
        cluster.run(seconds(1));
        let join = cluster
            .board
            .lock()
            .unwrap()
            .stamped
            .remove(&follower)
            .unwrap();
        for _ in 0..2 {
            let forward = (follower, join.clone());
            cluster
                .board
                .lock()
                .unwrap()
                .forwards
                .insert(leader, forward);
            cluster.run(seconds(1));
            let answer = cluster.board.lock().unwrap().answers.remove(&leader);
            assert!(
                matches!(answer, Some(Message::Proposed { .. })),
                "{answer:?}"
            );
        }
        cluster.script(home);
        cluster.run(seconds(2));
        let repeat = join.clone();
        let Change::Join(join) = join else {
            panic!("{join:?} is not a join")
        };
        let given: Vec<_> = join
            .status
            .as_map()
            .keys()
            .map(types::name::Name::as_str)
            .collect();
        assert_eq!(given, names);
        let admitted = Member {
            status: join.status,
            admission: join.admission,
            ..common::member(4)
        };
        let board = cluster.board();
        for (id, records) in board.records {
            assert_eq!(records.get(&4), Some(&admitted), "node {id}");
        }
        for (id, mut state) in board.states {
            let uses = state.ticket(public(8)).map(|record| record.uses);
            assert_eq!(uses, Some(1), "node {id}");
            let again = state.apply(repeat.clone()).unwrap_err();
            let duplicate = Refused::from(Unfit::Duplicate { key: key(4) });
            assert_eq!(again, duplicate, "node {id}");
            assert_eq!(
                again.to_string(),
                format!("node {} is already a member", key(4))
            );
        }
    }

    #[test]
    fn each_refused_join_changes_nothing_on_every_member() {
        let hour = NOW + Span::HOUR;
        let card = |id| common::member(id).card;
        let mut forged = join_with(&card(4), 7, NOW);
        if let Change::Join(join) = &mut forged {
            join.admission = common::ticket(6).admission(&card(4));
        }
        let changes = [
            ticket_of(7, "plant", true, hour),
            ticket_of(8, "plant", false, hour),
            ticket_of(9, "plant", true, NOW),
            ticket_of(10, "plant.line", true, hour),
            join_with(&card(4), 6, NOW),
            forged,
            join_with(&card(4), 9, NOW),
            join_with(&card(4), 10, NOW),
            join_with(&card(5), 8, NOW),
            join_with(&card(6), 8, NOW),
            home(1),
        ];
        let mut cluster = Cluster::new(4);
        cluster.script_each(&changes.each_ref().map(encoded));
        cluster.start();
        cluster.run(seconds(5));
        let board = cluster.board();
        assert_eq!(board.led, vec![board.led[0]; changes.len()]);
        assert_eq!(board.homes, each(&[None, Some(key(1))]));
        let members = IDS.map(|id| (id, [1, 2, 3, 5].into())).into();
        assert_eq!(board.members, members);
    }

    #[test]
    fn a_join_for_a_voter_changes_neither_its_record_nor_the_votes() {
        let other = common::signed(2, "plant.other");
        let changes = [
            encoded(&ticket()),
            encoded(&join_with(&other, 7, Stamp::EPOCH)),
            encoded(&home(1)),
        ];
        let mut cluster = Cluster::new(5);
        cluster.script_each(&changes);
        cluster.start();
        cluster.run(seconds(5));
        let board = cluster.board();
        assert_eq!(board.homes, each(&[None, Some(key(1))]));
        for (id, records) in board.records {
            assert_eq!(records.get(&2), Some(&create_voter(2)), "node {id}");
        }
        cluster.script(|_| home(2));
        cluster.run(seconds(5));
        let (led, homes) = cluster.take();
        assert_eq!(led.len(), 1, "the group took {led:?}");
        assert_eq!(homes, each(&[Some(key(2))]));
    }

    // A stamp at the midpoint would admit a join with a ticket whose expiry is inside
    // the interval.
    #[test]
    fn a_join_is_stamped_at_the_later_edge_of_mesh_time() {
        solo(|node, tasks| async move {
            let time = synced(&node);
            let config = config(&node, &tasks, 1, &[1], &[1]).await;
            let mesh = Mesh::open(config).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let interval = time.now().mesh.unwrap();
            let names = ["clock.error", "clock.offset"];
            let late = mesh.stamp(&time, request(4, 8, &names)).unwrap();
            let early = mesh.stamp(&time, request(5, 9, &[])).unwrap();
            let Change::Join(join) = &late else {
                panic!("{late:?} is not a join")
            };
            let at = join.at;
            assert_eq!(at, interval.latest);
            assert!(interval.earliest < at - Span::from_nanos(1));
            let millis = u128::try_from(at.nanos() / 1_000_000).unwrap();
            for key in join.status.as_map().values() {
                assert_eq!(key.as_u128() >> 80, millis, "{key:?}");
            }
            let keys: BTreeSet<_> = join.status.as_map().values().collect();
            assert_eq!(keys.len(), names.len());
            let inside = at - Span::from_nanos(1);
            let after = at + Span::from_nanos(1);
            mesh.propose(ticket_of(8, "plant", true, inside))
                .await
                .unwrap();
            mesh.propose(ticket_of(9, "plant", true, after))
                .await
                .unwrap();
            mesh.propose(late).await.unwrap();
            mesh.propose(early).await.unwrap();
            mesh.propose(home(2)).await.unwrap();
            assert_eq!(watch.next().await, Ok(Some(key(2))));
            assert_eq!(mesh.member(key(4)), None);
            assert!(mesh.member(key(5)).is_some());
        });
    }

    // The stamp is the time at which the voter admitted the request, so a commit
    // after the expiry does not refuse it.
    #[test]
    fn a_join_that_commits_after_the_expiry_admits_its_node() {
        solo(|node, tasks| async move {
            let time = synced(&node);
            let config = config(&node, &tasks, 1, &[1], &[1]).await;
            let mesh = Mesh::open(config).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let stamped = mesh.stamp(&time, request(4, 8, &[])).unwrap();
            let Change::Join(join) = &stamped else {
                panic!("{stamped:?} is not a join")
            };
            let expiry = join.at + seconds(1);
            mesh.propose(ticket_of(8, "plant", false, expiry))
                .await
                .unwrap();
            node.clock().sleep(seconds(3600)).await;
            assert!(time.now().mesh.unwrap().earliest > expiry);
            mesh.propose(stamped).await.unwrap();
            mesh.propose(home(2)).await.unwrap();
            assert_eq!(watch.next().await, Ok(Some(key(2))));
            let admitted = mesh.member(key(4)).map(|member| member.card);
            assert_eq!(admitted, Some(common::member(4).card));
        });
    }

    #[test]
    fn a_node_with_no_mesh_time_of_known_error_at_or_after_the_epoch_stamps_no_join() {
        for case in ["unsynced", "before the epoch", "unknown error"] {
            solo(move |node, tasks| async move {
                let time = match case {
                    "unsynced" => clock::Clock::new(node.clock()).1,
                    "before the epoch" => {
                        node.step_wall(Span::from_nanos(-2 * NOW.nanos()));
                        synced(&node)
                    }
                    _ => {
                        node.set_wall_error(None);
                        synced(&node)
                    }
                };
                let config = config(&node, &tasks, 1, &[1], &[1]).await;
                let mesh = Mesh::open(config).await.unwrap();
                let stamped = mesh.stamp(&time, request(4, 8, &[]));
                assert_eq!(stamped, Err(Unstamped::Unsynced), "{case}");
            });
        }
        let text = "this node has no mesh time with a known error at or after the Unix \
                    epoch, so it stamps no join";
        assert_eq!(Unstamped::Unsynced.to_string(), text);
        let _: &dyn std::error::Error = &Unstamped::Unsynced;
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "feeds the mesh clock of a test")]
    fn a_node_at_the_unix_epoch_stamps_a_join() {
        solo(|node, tasks| async move {
            let (mut clock, time) = clock::Clock::new(node.clock());
            let config = config(&node, &tasks, 1, &[1], &[1]).await;
            let mesh = Mesh::open(config).await.unwrap();
            node.step_wall(Span::from_nanos(-node.wall().now().time.nanos()));
            node.set_wall_error(Some(Span::from_nanos(0)));
            let source = clock.add();
            let wall = clock::source::Wall::new(node.wall(), node.clock());
            clock.push(source, wall.measure());
            let stamped = mesh.stamp(&time, request(4, 8, &[])).unwrap();
            let Change::Join(join) = stamped else {
                panic!("{stamped:?} is not a join")
            };
            assert_eq!(join.at, Stamp::EPOCH);
        });
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "feeds the mesh clock of a test")]
    fn a_node_in_holdover_stamps_a_join_at_the_later_edge() {
        solo(|node, tasks| async move {
            let (mut clock, time) = clock::Clock::new(node.clock());
            let config = config(&node, &tasks, 1, &[1], &[1]).await;
            let mesh = Mesh::open(config).await.unwrap();
            let source = clock.add();
            let wall = clock::source::Wall::new(node.wall(), node.clock());
            clock.push(source, wall.measure());
            clock.remove(source);
            assert!(matches!(time.status(), clock::Status::Holdover(..)));
            let stamped = mesh.stamp(&time, request(4, 8, &[])).unwrap();
            let Change::Join(join) = stamped else {
                panic!("{stamped:?} is not a join")
            };
            assert_eq!(join.at, time.now().mesh.unwrap().latest);
        });
    }

    #[test]
    fn a_join_request_with_65_status_names_is_refused() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let time = synced(&node);
            let names: Vec<_> = (0..65).map(|i| format!("s{i:02}")).collect();
            let names: Vec<_> = names.iter().map(String::as_str).collect();
            let refused = mesh.stamp(&time, request(4, 8, &names));
            let many = Unstamped::Status(Many { count: 65 });
            assert_eq!(refused, Err(many.clone()));
            assert_eq!(many.to_string(), "65 status entries, more than 64");
            let names = &names[..64];
            mesh.stamp(&time, request(4, 8, names)).unwrap();
        });
    }

    // The root region holds each name: a ticket and a join under any prefix.
    #[test]
    fn the_root_region_takes_a_join_of_a_node_with_any_name() {
        solo(|node, tasks| async move {
            let mut config = config(&node, &tasks, 1, &[1], &[1]).await;
            config.founding.prefix = Prefix::ROOT;
            let mesh = Mesh::open(config).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            let Change::Ticket {
                public_key,
                mut options,
            } = ticket()
            else {
                unreachable!()
            };
            options.prefix = "site_a".parse().unwrap();
            lead(
                &mesh,
                &node.clock(),
                Change::Ticket {
                    public_key,
                    options,
                },
            )
            .await;
            let card = common::signed(4, "site_a.pt_1");
            let Change::Join(mut join) = join(4) else {
                unreachable!()
            };
            join.card.card = card.card().clone();
            join.card.signature = *card.signature();
            join.admission = common::ticket(7).admission(&card);
            mesh.propose(Change::Join(join)).await.unwrap();
            mesh.propose(home(1)).await.unwrap();
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let admitted = mesh.member(key(4)).map(|member| member.card);
            assert_eq!(admitted, Some(card));
        });
    }

    #[test]
    fn open_refuses_a_node_or_a_voter_that_is_not_a_member() {
        solo(|node, tasks| async move {
            let own = open(&node, &tasks, 1, &[2, 3], &[2, 3]).await;
            assert_eq!(own.err(), Some(Error::NotMember(key(1))));
            let voter = open(&node, &tasks, 1, &[1, 2], &IDS).await;
            let refused = Error::NotMember(key(3));
            assert_eq!(voter.err(), Some(refused.clone()));
            let text = format!("node {} is not a member of the region", key(3));
            assert_eq!(refused.to_string(), text);
            assert_eq!(
                node.files().list(Path::new("")).await,
                Ok(vec![BLOB.into()])
            );
        });
    }

    #[test]
    fn open_refuses_the_first_founding_home_at_a_node_that_is_not_a_member() {
        solo(|node, tasks| async move {
            let mut config = config(&node, &tasks, 1, &IDS, &IDS).await;
            config.founding.homes = BTreeMap::from([
                (channel::Key::from_u128(4), key(2)),
                (channel::Key::from_u128(5), key(9)),
                (channel::Key::from_u128(6), key(8)),
            ]);
            let refused = Mesh::open(config).await.err();
            assert_eq!(refused, Some(Error::NotMember(key(9))));
        });
    }

    #[test]
    fn open_refuses_a_voter_that_is_not_a_member_before_a_founding_home() {
        solo(|node, tasks| async move {
            let mut config = config(&node, &tasks, 1, &[1, 2], &IDS).await;
            config.founding.homes =
                BTreeMap::from([(channel::Key::from_u128(4), key(9))]);
            let refused = Mesh::open(config).await.err();
            assert_eq!(refused, Some(Error::NotMember(key(3))));
        });
    }

    #[test]
    fn open_refuses_this_node_that_is_not_a_member_before_a_founding_home() {
        solo(|node, tasks| async move {
            let mut config = config(&node, &tasks, 1, &[2, 3], &[2, 3]).await;
            config.founding.homes =
                BTreeMap::from([(channel::Key::from_u128(4), key(9))]);
            let refused = Mesh::open(config).await.err();
            assert_eq!(refused, Some(Error::NotMember(key(1))));
        });
    }

    #[test]
    fn open_refuses_a_wrong_private_key_before_a_founding_home() {
        solo(|node, tasks| async move {
            let mut config = Config {
                key: key(1),
                ..config(&node, &tasks, 2, &IDS, &[]).await
            };
            config.founding.homes =
                BTreeMap::from([(channel::Key::from_u128(4), key(9))]);
            assert_eq!(Mesh::open(config).await.err(), Some(Error::WrongKey));
        });
    }

    #[test]
    fn member_gives_the_record_of_a_member_and_none_for_another_node() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            assert_eq!(mesh.member(key(2)), Some(common::member(2)));
            assert_eq!(mesh.member(key(1)), Some(common::member(1)));
            assert_eq!(mesh.member(key(9)), None);
        });
    }

    #[test]
    fn member_gives_the_record_after_the_group_stops() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let cause = fail_sync(&node);
            let stopped = Error::Stopped(cause.clone());
            assert_eq!(mesh.propose(home(2)).await, Err(stopped));
            assert_eq!(watch.next().await, Err(cause));
            assert_eq!(mesh.member(key(1)), Some(common::member(1)));
            assert_eq!(mesh.member(key(2)), None);
        });
    }

    #[test]
    fn member_gives_the_record_that_its_card_names_for_each_order_of_the_records() {
        let orders = [
            [1, 2, 3],
            [1, 3, 2],
            [2, 1, 3],
            [2, 3, 1],
            [3, 1, 2],
            [3, 2, 1],
        ];
        for order in orders {
            solo(move |node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &order, &[1]).await.unwrap();
                for id in IDS {
                    let member = Some(common::member(id));
                    assert_eq!(mesh.member(key(id)), member, "{order:?}");
                }
            });
        }
    }

    #[test]
    fn open_refuses_two_records_of_one_node() {
        let admitted = Member {
            admission: [1; 64],
            ..common::member(2)
        };
        let mut card = record(2, 3, 2).card.card().clone();
        card.seal_key = SealKey::new([8; 32]).unwrap();
        let address = Address::Udp(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 4100));
        card.addresses = card::addresses::Addresses::new(vec![address]).unwrap();
        let other = Member {
            card: card::Signed::sign(key(2), card, &private(3)),
            admission: [1; 64],
            ephemeral: Some(Span::MILLISECOND),
            status: common::status([("clock.offset".parse().unwrap(), INDEX)]),
        };
        let cases = [
            ("an equal record", record(2, 2, 1)),
            ("another version", record(2, 2, 2)),
            ("another signer", record(2, 3, 1)),
            ("another admission", admitted),
            ("another record in each field", other),
        ];
        for (case, second) in cases {
            for at in [0, 3] {
                let second = second.clone();
                solo(move |node, tasks| async move {
                    let mut config = config(&node, &tasks, 1, &[1, 2, 3], &[1]).await;
                    config.founding.members.insert(at, second);
                    let opened = Mesh::start(config).await.err();
                    let duplicate =
                        Some(Error::Member(Unfit::Duplicate { key: key(2) }));
                    assert_eq!(opened, duplicate, "{case} at {at}");
                    assert_eq!(
                        node.files().list(Path::new("")).await,
                        Ok(vec![BLOB.into()])
                    );
                });
            }
        }
        let text = format!("node {} is already a member", key(2));
        assert_eq!(
            Error::Member(Unfit::Duplicate { key: key(2) }).to_string(),
            text
        );
    }

    #[test]
    fn open_refuses_a_private_key_that_is_not_the_key_of_the_member() {
        solo(|node, tasks| async move {
            let config = Config {
                key: key(1),
                ..config(&node, &tasks, 2, &IDS, &[]).await
            };
            assert_eq!(Mesh::open(config).await.err(), Some(Error::WrongKey));
            assert_eq!(
                node.files().list(Path::new("")).await,
                Ok(vec![BLOB.into()])
            );
            let text = "the private key of this node is not the key of its member";
            assert_eq!(Error::WrongKey.to_string(), text);
        });
    }

    #[test]
    fn open_gives_the_error_of_the_log() {
        solo(|node, tasks| async move {
            let _first = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let busy = open(&node, &tasks, 1, &[1], &[1]).await.err().unwrap();
            assert_eq!(busy.to_string(), locked().to_string());
            assert_eq!(busy, locked());
        });
    }

    /// What an open gives while a group holds the directory of the log.
    fn locked() -> Error {
        let path = Path::new(LOG).join("lock");
        Error::Log(log::Error::Files(files::Error::Busy { path }))
    }

    fn create_sim(run: u64) -> (Sim, sim::node::Node) {
        let mut sim = Sim::new(sim::Config {
            seed: run,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        (sim, node)
    }

    /// Writes one record that fills `log-0`, so the next write starts `log-1`.
    async fn fill_first_file(node: &sim::node::Node) {
        let opened = Log::open(node.files(), LOG.into(), create_pool()).await;
        let (mut log, _) = opened.unwrap();
        let entries: Vec<Entry> = (1..=20_000)
            .map(|index| Entry {
                at: Position {
                    term: Term(1),
                    index,
                },
                data: Data::Bytes(encoded(&home(1))),
            })
            .collect();
        log.write(None, &entries).await.unwrap();
    }

    /// Opens node 1 again and again for 1 ms while its first mesh lives, in each of
    /// 64 runs. Gives each run in which an open did not give `locked`, with what
    /// each such open gave. With `full`, the first write of the group starts `log-1`.
    fn opens_of_a_held_log(full: bool) -> Vec<(u64, Vec<String>)> {
        let mut runs = Vec::new();
        for run in 0..64 {
            let (mut sim, node) = create_sim(run);
            let opens = sim.run_on(&node, move |node, tasks| async move {
                if full {
                    fill_first_file(&node).await;
                }
                let first = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
                let clock = node.clock();
                let end = clock.now().checked_add(Span::MILLISECOND).unwrap();
                let mut opens = Vec::new();
                while clock.now() < end {
                    match open(&node, &tasks, 1, &[1], &[1]).await {
                        Err(error) if error == locked() => {}
                        Err(error) => opens.push(error.to_string()),
                        Ok(second) => {
                            let names = node.files().list(Path::new(LOG)).await;
                            clock.sleep(TICK).await;
                            let proposed = second.propose(home(3)).await;
                            opens.push(format!("Ok with {names:?}, then {proposed:?}"));
                        }
                    }
                }
                drop(first);
                opens
            });
            let opens = opens.unwrap();
            if !opens.is_empty() {
                runs.push((run, opens));
            }
        }
        runs
    }

    #[test]
    fn open_gives_busy_on_the_lock_while_a_mesh_lives() {
        assert_eq!(opens_of_a_held_log(false), []);
    }

    // The first write of the group starts `log-1` and frees `log-0`.
    #[test]
    fn open_gives_busy_on_the_lock_while_a_write_starts_a_file() {
        assert_eq!(opens_of_a_held_log(true), []);
    }

    // The record of the changes does not fit in `log-0`, so its write makes `log-1`.
    // The mesh drops while that write is in progress, and a new open starts at once.
    #[test]
    fn an_open_after_a_drop_does_not_share_the_log_with_a_write_that_makes_a_file() {
        let mut wrong = Vec::new();
        let mut busy = 0_usize;
        for run in 0..48 {
            let (mut sim, node) = create_sim(run);
            let first = sim.run_on(&node, |node, tasks| async move {
                let clock = node.clock();
                let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
                lead(&mesh, &clock, home(1)).await;
                clock.sleep(TICK).await;
                for _ in 0..20_000 {
                    assert!(started(&mesh, home(2)).await.is_pending());
                }
                clock.sleep(Span::from_nanos(1)).await;
                drop(mesh);
                let again = open(&node, &tasks, 1, &[1], &[1]).await;
                let first = again.as_ref().err().cloned();
                let mesh = if let Ok(mesh) = again {
                    mesh
                } else {
                    clock.sleep(TICK).await;
                    open(&node, &tasks, 1, &[1], &[1]).await.unwrap()
                };
                let mut watch = mesh.watch(INDEX);
                lead(&mesh, &clock, home(3)).await;
                while watch.next().await.unwrap() != Some(key(3)) {}
                first
            });
            let first = first.unwrap();
            if let Some(error) = &first {
                assert_eq!(*error, locked(), "run {run}");
                busy = busy.saturating_add(1);
            }
            sim.crash(&node, Crash::Process);
            let again = sim.run_on(&node, |node, tasks| async move {
                open(&node, &tasks, 1, &[1], &[1]).await.err()
            });
            if let Some(error) = again.unwrap() {
                wrong.push((run, first, error.to_string()));
            }
        }
        assert_eq!(
            wrong,
            [],
            "(run, the open after the drop, the open after a crash)"
        );
        assert_ne!(busy, 0, "no open after the drop met the old task");
    }

    #[test]
    fn open_gives_the_error_of_raft() {
        solo(|node, tasks| async move {
            let (mut log, _) = Log::open(node.files(), LOG.into(), create_pool())
                .await
                .unwrap();
            let at = Position {
                term: Term(1),
                index: 1,
            };
            let entries = [common::change(1, at, Voters::default())];
            let proof = Proof {
                grant: Grant::Vote,
                candidate: key(1),
                voters: [(key(1), Some(common::signature(1, Grant::Vote, 1)))].into(),
            };
            let hard = Hard {
                term: Term(1),
                vote: Some(key(1)),
                leader: Some(key(1)),
                proof: Some(proof),
            };
            log.write(Some(hard.clone()), &entries).await.unwrap();
            drop(log);
            let refused = open(&node, &tasks, 1, &[1], &[1]).await.err().unwrap();
            assert_eq!(refused, Error::Raft(raft::Error::NoVoters));
            let text = "a configuration has an empty incoming voter set";
            assert_eq!(refused.to_string(), text);
            let again = open(&node, &tasks, 1, &[1], &[1]).await.err().unwrap();
            assert_eq!(again, Error::Raft(raft::Error::NoVoters));
            let (_, stored) = Log::open(node.files(), LOG.into(), create_pool())
                .await
                .unwrap();
            let wrote = (hard, entries.to_vec());
            assert_eq!((stored.hard, stored.entries), wrote);
        });
    }

    #[test]
    fn a_dropped_mesh_frees_its_log_before_the_next_tick() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            node.clock().sleep(Span::MILLISECOND).await;
            drop(mesh);
            node.clock().sleep(Span::MILLISECOND).await;
            let again = open(&node, &tasks, 1, &[1], &[1]).await;
            assert_eq!(again.err(), None);
        });
    }

    /// The config of node 1 of `IDS`, through `Mesh::open`. No node serves the
    /// address of another member, so each dial waits for [`IDLE`].
    async fn dialing(node: &sim::node::Node, tasks: &Tasks) -> Config {
        dialed_at(node, tasks, 1, 0, &IDS, &IDS).await
    }

    #[test]
    fn ended_waits_for_the_last_mesh_and_the_log() {
        solo(|node, tasks| async move {
            let mesh = Mesh::open(dialing(&node, &tasks).await).await.unwrap();
            let (mut first, second) = (mesh.ended(), mesh.ended());
            node.clock().sleep(Span::SECOND).await;
            let polled = poll_fn(|cx| Poll::Ready(Pin::new(&mut first).poll(cx))).await;
            assert_eq!(polled, Poll::Pending);
            drop(mesh);
            first.await;
            second.await;
            assert_eq!(Mesh::open(dialing(&node, &tasks).await).await.err(), None);
        });
    }

    #[test]
    fn ended_wakes_each_task_that_waits() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let woken = Rc::new(Cell::new(0));
            for _ in 0..2 {
                let (ended, woken) = (mesh.ended(), Rc::clone(&woken));
                tasks.spawn(async move {
                    ended.await;
                    woken.set(woken.get() + 1);
                });
            }
            node.clock().sleep(Span::MILLISECOND).await;
            drop(mesh);
            node.clock().sleep(Span::MILLISECOND).await;
            assert_eq!(woken.get(), 2);
        });
    }

    /// The drop of the last mesh stops each wait for a dial, and `ended` does not wait
    /// for the dial.
    #[test]
    fn ended_waits_for_each_task_that_sends_but_not_for_its_dial() {
        for seed in 0..32 {
            solo_at(seed, move |node, tasks| async move {
                let config = dialing(&node, &tasks).await;
                let transport = Rc::clone(&config.transport);
                let mesh = Mesh::open(config).await.unwrap();
                let clock = node.clock();
                clock.sleep(seconds(5)).await;
                assert!(Rc::strong_count(&transport) > 2);
                let ended = mesh.ended();
                let dropped = clock.now();
                drop(mesh);
                ended.await;
                let waited = clock.now() - dropped;
                assert!(waited <= TICK, "it ended after {waited}");
                assert_eq!(Rc::strong_count(&transport), 1, "seed {seed}");
            });
        }
    }

    #[test]
    fn ended_resolves_when_the_group_stops() {
        solo(|node, tasks| async move {
            let mut config = dialing(&node, &tasks).await;
            config.founding.voters = [key(1)].into_iter().collect();
            let mesh = Mesh::open(config).await.unwrap();
            let cause = fail_sync(&node);
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            assert_eq!(watch.next().await, Err(cause));
            mesh.ended().await;
        });
    }

    #[test]
    fn a_power_cut_right_after_the_open_keeps_the_directory_and_its_log() {
        let names = |names: &[&str]| Ok(names.iter().map(PathBuf::from).collect());
        for seed in 0..32 {
            let mut sim = Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let node = sim.node(sim::node::Config::default());
            sim.run_on(&node, |node, tasks| async move {
                let config = Config {
                    dir: "region".into(),
                    ..config(&node, &tasks, 1, &[1], &[1]).await
                };
                Mesh::start(config).await.unwrap();
            })
            .unwrap();
            sim.crash(&node, Crash::Power);
            let listed = sim
                .run_on(&node, |node, _| async move {
                    let files = node.files();
                    (
                        files.list(Path::new("")).await,
                        files.list(Path::new("region")).await,
                    )
                })
                .unwrap();
            let kept = (names(&[BLOB, "region"]), names(&[LOG, used::SPEC]));
            assert_eq!(listed, kept, "seed {seed}");
        }
    }

    #[test]
    fn an_open_gives_a_failed_sync_of_the_parent_of_its_directory() {
        solo(|node, tasks| async move {
            let config = Config {
                dir: "region".into(),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            node.fail_file(Path::new(""), Operation::SyncDir);
            let cause = files::Error::Io {
                path: "".into(),
                operation: Operation::SyncDir,
                code: 5,
            };
            let failed = Error::Log(log::Error::Files(cause));
            assert_eq!(Mesh::start(config).await.err(), Some(failed));
        });
    }

    #[test]
    fn an_open_fails_when_the_parent_of_its_directory_is_not_there() {
        solo(|node, tasks| async move {
            let config = Config {
                dir: "gone/region".into(),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            let cause = files::Error::NotFound {
                path: "gone/region".into(),
            };
            let failed = Error::Log(log::Error::Files(cause));
            assert_eq!(Mesh::start(config).await.err(), Some(failed));
        });
    }

    #[test]
    fn the_log_is_in_the_directory_of_the_config() {
        solo(|node, tasks| async move {
            let dir = Path::new("region");
            let at = async || Config {
                dir: dir.into(),
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            let _mesh = Mesh::start(at().await).await.unwrap();
            let busy = Mesh::start(at().await).await.err();
            let path = dir.join(LOG).join("lock");
            let cause = log::Error::Files(files::Error::Busy { path });
            assert_eq!(busy, Some(Error::Log(cause)));
            assert_eq!(open(&node, &tasks, 1, &[1], &[1]).await.err(), None);
        });
    }

    #[test]
    fn open_puts_each_chunk_of_the_founding_tree_in_the_store() {
        solo(|node, tasks| async move {
            let founding = apply::create_large(200);
            let mut tree = Chunks::default();
            let update = spec::region::tree(&mut tree, &founding);
            assert!(update.chunks.len() > 1, "{} chunks", update.chunks.len());
            let mut config = config(&node, &tasks, 1, &[1], &[1]).await;
            config.founding.definitions = founding;
            let store = Rc::clone(&config.store);
            Mesh::start(config).await.unwrap();
            for digest in update.chunks {
                let chunk = store.get(digest).await.unwrap().unwrap();
                assert_eq!(Some(&*chunk), tree.get(digest), "{digest}");
            }
        });
    }

    #[test]
    fn open_gives_a_failed_put_of_a_founding_chunk() {
        solo(|node, tasks| async move {
            let founding = apply::create_subjects(&["plant.a"], 1);
            let mut config = config(&node, &tasks, 1, &[1], &[1]).await;
            config.founding.definitions = founding;
            node.fail_file(Path::new(BLOB), Operation::SyncDir);
            let cause = files::Error::Io {
                path: BLOB.into(),
                operation: Operation::SyncDir,
                code: 5,
            };
            let failed = Error::Blob(blob::Error::Files(cause));
            assert_eq!(Mesh::start(config).await.err(), Some(failed));
        });
    }

    /// A waker that a test counts the clones of, which `Waker::noop` does not allow.
    struct Idle;

    #[expect(clippy::manual_noop_waker, reason = "a test counts its clones")]
    impl Wake for Idle {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn an_ended_polled_twice_holds_one_waker() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let held = Arc::new(Idle);
            let waker = Waker::from(Arc::clone(&held));
            let mut cx = Context::from_waker(&waker);
            let mut ended = mesh.ended();
            for _ in 0..2 {
                assert!(Pin::new(&mut ended).poll(&mut cx).is_pending());
            }
            drop(waker);
            assert_eq!(Arc::strong_count(&held), 2);
            drop(mesh);
        });
    }

    #[test]
    fn a_dropped_ended_leaves_no_waker() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let held = Arc::new(Idle);
            let waker = Waker::from(Arc::clone(&held));
            let mut cx = Context::from_waker(&waker);
            let mut ended = mesh.ended();
            assert!(Pin::new(&mut ended).poll(&mut cx).is_pending());
            drop(ended);
            drop(waker);
            assert_eq!(Arc::strong_count(&held), 1);
            drop(mesh);
        });
    }

    #[test]
    fn a_dropped_watch_leaves_no_waker() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            let (kept, dropped) = (Arc::new(Idle), Arc::new(Idle));
            let count = || (Arc::strong_count(&kept), Arc::strong_count(&dropped));
            let mut watches = Vec::new();
            for held in [&dropped, &dropped, &kept, &dropped] {
                let waker = Waker::from(Arc::clone(held));
                let mut cx = Context::from_waker(&waker);
                let mut watch = mesh.watch(INDEX);
                assert_eq!(watch.next().await, Ok(None));
                assert!(pin!(watch.next()).poll(&mut cx).is_pending());
                watches.push(watch);
            }
            // Each `Arc` here, and the waker that the group holds for each watch.
            assert_eq!(count(), (2, 4));
            let stays = watches.remove(2);
            drop(watches);
            assert_eq!(count(), (2, 1));
            drop(stays);
            assert_eq!(count(), (1, 1));
        });
    }

    #[test]
    fn a_watch_gives_the_cause_when_each_mesh_drops() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            let given = Rc::new(RefCell::new(None));
            let slot = Rc::clone(&given);
            tasks.spawn(async move {
                let waited = watch.next().await;
                *slot.borrow_mut() = Some((waited, watch.next().await));
            });
            node.clock().sleep(TICK).await;
            assert_eq!(*given.borrow(), None);
            drop(mesh);
            node.clock().sleep(TICK).await;
            let dropped = Stopped::Dropped;
            assert_eq!(
                given.take(),
                Some((Err(dropped.clone()), Err(dropped.clone())))
            );
            assert_eq!(dropped.to_string(), "each mesh of the group dropped");
        });
    }

    #[test]
    fn a_watch_keeps_the_cause_of_a_stop_after_each_mesh_drops() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let cause = fail_sync(&node);
            let stopped = Error::Stopped(cause.clone());
            assert_eq!(mesh.propose(home(2)).await, Err(stopped));
            assert_eq!(watch.next().await, Err(cause.clone()));
            drop(mesh);
            assert_eq!(watch.next().await, Err(cause));
        });
    }

    #[test]
    fn a_watch_that_waits_gets_the_cause_of_a_stop_when_its_mesh_drops_first() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            lead(&mesh, &node.clock(), home(1)).await;
            node.clock().sleep(TICK).await;
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let given = Rc::new(RefCell::new(None));
            let slot = Rc::clone(&given);
            tasks.spawn(async move {
                *slot.borrow_mut() = Some(watch.next().await);
            });
            node.clock().sleep(TICK).await;
            let cause = fail_sync(&node);
            let stopped = Error::Stopped(cause.clone());
            assert_eq!(mesh.propose(home(2)).await, Err(stopped.clone()));
            assert_eq!(mesh.outgoing(key(2)).await, Err(stopped));
            drop(mesh);
            node.clock().sleep(TICK).await;
            assert_eq!(given.take(), Some(Err(cause)));
        });
    }

    #[test]
    fn the_group_ends_when_its_mesh_drops() {
        solo(|node, tasks| async move {
            drop(open(&node, &tasks, 1, &[1], &[1]).await.unwrap());
            node.clock().sleep(seconds(5)).await;
            let (_, stored) = Log::open(node.files(), LOG.into(), create_pool())
                .await
                .unwrap();
            assert_eq!((stored.hard, stored.entries), (Hard::default(), Vec::new()));
        });
    }

    mod queue {
        use std::ops::RangeInclusive;

        use super::*;

        /// Gives node 1 one heartbeat of leader 2 in each term of `terms`, with no
        /// read of a reply between them. Gives the term of each reply that the queue
        /// for node 2 then holds, in the order of the queue.
        async fn replies(
            mesh: &Mesh,
            clock: &Clock,
            terms: RangeInclusive<u64>,
        ) -> Vec<Term> {
            for term in terms.map(Term) {
                let heartbeat = Body::Heartbeat { commit: 0 };
                let heartbeat = common::proven_in(term, 2, 1, heartbeat);
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
            }
            clock.sleep(TICK).await;
            let mut replies = Vec::new();
            while let Poll::Ready(reply) = now(pin!(mesh.outgoing(key(2)))).await {
                let reply = reply.unwrap();
                let expected = raft::Message {
                    term: reply.term,
                    ..message(1, 2, Body::HeartbeatReply)
                };
                assert_eq!(reply, expected);
                replies.push(reply.term);
            }
            replies
        }

        #[test]
        fn holds_64_messages() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let replies = replies(&mesh, &node.clock(), 1..=64).await;
                assert_eq!(replies, (1..=64).map(Term).collect::<Vec<_>>());
            });
        }

        #[test]
        fn drops_its_oldest_message_when_full() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let replies = replies(&mesh, &node.clock(), 1..=65).await;
                assert_eq!(replies, (2..=65).map(Term).collect::<Vec<_>>());
            });
        }
    }

    /// The `removed` answer between nodes. Node 1 led at `TERM` and never starts:
    /// each node that starts takes one append from it before it serves.
    mod removal {
        use std::future::pending;
        use std::sync::MutexGuard;

        use super::*;

        const MEMBERS: [u8; 4] = [1, 2, 3, 4];

        /// How a node starts: its voters, and the entries it takes from node 1 in one
        /// append with `commit`.
        struct Opening {
            id: u8,
            voters: Vec<u8>,
            entries: Vec<Entry>,
            commit: u64,
        }

        /// What each node did, by node.
        #[derive(Default)]
        struct Record {
            /// Each message a node refused, by the node of the peer that sent it.
            refused: BTreeMap<u8, BTreeMap<u8, Vec<Error>>>,
            /// The count of streams a node served, by the node of their peer.
            served: BTreeMap<u8, BTreeMap<u8, usize>>,
            /// Why the group of a node stopped.
            stopped: BTreeMap<u8, Stopped>,
            /// What a new open of the log of a node gave at its stop, while the
            /// mesh that stopped lived.
            reopened: BTreeMap<u8, Result<(), Error>>,
            /// The leader a node knew at its last tick.
            leaders: BTreeMap<u8, Option<node::Key>>,
            /// The homes the watch of a node gave.
            homes: BTreeMap<u8, Vec<Option<node::Key>>>,
        }

        struct Run {
            sim: Sim,
            nodes: Vec<sim::node::Node>,
            record: Arc<Mutex<Record>>,
        }

        impl Run {
            fn new() -> Self {
                let mut sim = Sim::new(sim::Config::default());
                let node = |_| sim.node(sim::node::Config::default());
                Self {
                    nodes: MEMBERS.map(node).into(),
                    sim,
                    record: Arc::default(),
                }
            }

            /// Starts a node as `opening` says. With `proposes`, it proposes itself
            /// as the home once per tick until the group takes it.
            fn start(&self, opening: Opening, proposes: bool) {
                let at = MEMBERS.iter().position(|&id| id == opening.id).unwrap();
                let own = self.nodes[at].clone();
                let (node, record) = (own.clone(), Arc::clone(&self.record));
                let config = env::shards::Config {
                    name: format!("node-{}", opening.id),
                    core: None,
                };
                let main = move |tasks| async move {
                    serve(own, tasks, opening, proposes, record).await;
                };
                drop(node.shards().start(config, main).unwrap());
            }

            fn run(&mut self, span: Span) {
                self.sim.run_for(span).unwrap();
            }

            fn record(&self) -> MutexGuard<'_, Record> {
                self.record.lock().unwrap()
            }

            /// What node `on` refused from node `from`.
            fn refused(&self, on: u8, from: u8) -> Vec<Error> {
                let record = self.record();
                let refused = record.refused.get(&on).and_then(|by| by.get(&from));
                refused.cloned().unwrap_or_default()
            }
        }

        /// Opens the mesh of a node as `opening` says and serves it, as `voter`
        /// does, and records what it does.
        async fn serve(
            own: sim::node::Node,
            tasks: Tasks,
            opening: Opening,
            proposes: bool,
            record: Arc<Mutex<Record>>,
        ) {
            let Opening {
                id,
                voters,
                entries,
                commit,
            } = opening;
            let config = async |tasks: &Tasks, port| {
                dialed_at(&own, tasks, id, port, &MEMBERS, &voters).await
            };
            let first = config(&tasks, PORT).await;
            let transport = Rc::clone(&first.transport);
            let mesh = Mesh::open(first).await.unwrap();
            if !entries.is_empty() {
                let append = Body::Append {
                    prev: Position::default(),
                    entries,
                    commit,
                };
                mesh.receive(public(1), proven(1, id, append)).unwrap();
            }
            let (serving, streams, refused) =
                (mesh.clone(), tasks.clone(), Arc::clone(&record));
            tasks.spawn(async move {
                accept(serving, transport, streams, id, refused).await;
            });
            if proposes {
                let (proposing, clock) = (mesh.clone(), own.clock());
                tasks.spawn(async move {
                    // Unlike `lead`, the node can know the failed leader, node 1.
                    loop {
                        match proposing.propose(home(id)).await {
                            Ok(_) => return,
                            Err(Error::Raft(raft::Error::NotLeader { .. })) => {}
                            Err(error) => panic!("the proposal failed: {error}"),
                        }
                        clock.sleep(TICK).await;
                    }
                });
            }
            let (ticking, clock, leaders) =
                (mesh.clone(), own.clock(), Arc::clone(&record));
            tasks.spawn(async move {
                loop {
                    clock.sleep(TICK).await;
                    // No call of `Mesh` gives the leader.
                    let leader = ticking.group.borrow().raft.leader();
                    leaders.lock().unwrap().leaders.insert(id, leader);
                }
            });
            let mut watch = mesh.watch(INDEX);
            loop {
                match watch.next().await {
                    Ok(home) => record
                        .lock()
                        .unwrap()
                        .homes
                        .entry(id)
                        .or_default()
                        .push(home),
                    Err(stopped) => {
                        record.lock().unwrap().stopped.insert(id, stopped);
                        // The stop ends the group's task at once, which frees the
                        // log before any tick.
                        let again =
                            Mesh::start(config(&tasks, 0).await).await.map(drop);
                        record.lock().unwrap().reopened.insert(id, again);
                        return pending().await;
                    }
                }
            }
        }

        /// Serves each stream of each session that a peer opens to `transport`, as
        /// node `id`, and records each message that the group refuses.
        async fn accept(
            mesh: Mesh,
            transport: Rc<Transport>,
            tasks: Tasks,
            id: u8,
            record: Arc<Mutex<Record>>,
        ) -> ! {
            loop {
                let session = transport.accept().await.unwrap();
                let Peer::Node(peer) = session.peer() else {
                    panic!("a peer with no node key opened a session");
                };
                let from = MEMBERS.into_iter().find(|&of| public(of) == peer).unwrap();
                let (mesh, streams, record) =
                    (mesh.clone(), tasks.clone(), Arc::clone(&record));
                tasks.spawn(async move {
                    while let Ok(mut incoming) = session.accept().await {
                        let (mesh, record) = (mesh.clone(), Arc::clone(&record));
                        streams.spawn(async move {
                            let Ok(Some(header)) = incoming.receiver.recv().await
                            else {
                                return;
                            };
                            let protocol = wire::header::decode(&header).unwrap();
                            assert_eq!(protocol, (Protocol::Mesh, &[][..]));
                            {
                                let served = &mut record.lock().unwrap().served;
                                let count: &mut usize = served
                                    .entry(id)
                                    .or_default()
                                    .entry(from)
                                    .or_default();
                                *count = count.saturating_add(1);
                            }
                            match mesh.serve(peer, incoming).await {
                                Ok(()) | Err(Error::Stream(_)) => {}
                                Err(error) => {
                                    let refused = &mut record.lock().unwrap().refused;
                                    refused
                                        .entry(id)
                                        .or_default()
                                        .entry(from)
                                        .or_default()
                                        .push(error);
                                }
                            }
                        });
                    }
                });
            }
        }

        /// A node that starts with voters 1, 2, and 3, and takes the first `count`
        /// of `entries` with `commit`.
        fn opening(id: u8, entries: &[Entry], count: usize, commit: u64) -> Opening {
            Opening {
                id,
                voters: vec![1, 2, 3],
                entries: entries[..count].to_vec(),
                commit,
            }
        }

        // The five steps of #1054. Node 1 led with voters 1, 2, and 3, wrote the joint
        // entry that adds node 4, and failed. Node 3 missed that entry, and node 4
        // campaigns before node 2 is up. Node 3 refuses node 4, which goes on, and
        // node 2 wins with 2, 3, and 4: its proposal commits on each, so node 3
        // catches up.
        #[test]
        fn a_voter_that_missed_a_change_does_not_stop_the_voter_it_added() {
            let mut run = Run::new();
            let sets = [(&[1, 2, 3][..], &[][..]), (&[1, 2, 3, 4], &[1, 2, 3])];
            let entries = changes(1, &sets);
            run.start(opening(3, &entries, 1, 1), false);
            run.start(opening(4, &entries, 2, 1), false);
            run.run(seconds(10));
            assert!(!run.refused(3, 4).is_empty(), "node 3 refused nothing");
            run.start(opening(2, &entries, 2, 1), true);
            run.run(seconds(10));
            let refused = run.refused(3, 4);
            let not_voter = Error::NotVoter { from: key(4) };
            assert!(
                refused.iter().all(|error| *error == not_voter),
                "{refused:?}"
            );
            let record = run.record();
            assert_eq!(record.stopped, BTreeMap::new());
            let leader = Some(key(2));
            assert_eq!(record.leaders, [2, 3, 4].map(|id| (id, leader)).into());
            let homes = [2, 3, 4].map(|id| (id, vec![None, leader]));
            assert_eq!(record.homes, homes.into());
        }

        // Node 2 holds the committed leave that removes node 3, which missed it.
        #[test]
        fn a_removed_node_that_missed_its_release_stops_at_its_campaign() {
            let mut run = Run::new();
            let sets = [(&[1, 2][..], &[1, 2, 3][..]), (&[1, 2], &[])];
            let entries = changes(1, &sets);
            run.start(opening(2, &entries, 2, 2), false);
            run.start(opening(3, &entries, 0, 0), false);
            run.run(seconds(10));
            let stopped = Stopped::Removed { by: key(2) };
            assert_eq!(run.record().stopped, [(3, stopped.clone())].into());
            assert_eq!(run.record().reopened, [(3, Ok(()))].into());
            assert_eq!(
                stopped.to_string(),
                format!(
                    "voter {} answered removed: a committed configuration lacks this \
                     node",
                    key(2)
                )
            );
            let refused = run.refused(2, 3);
            let removed = Error::Removed { from: key(3) };
            assert!(!refused.is_empty());
            assert!(refused.iter().all(|error| *error == removed), "{refused:?}");
            let served = run.record().served[&2][&3];
            run.run(seconds(10));
            assert_eq!(run.record().served[&2][&3], served);
            assert_eq!(run.refused(2, 3).len(), refused.len());
        }

        // Node 4 is a member that no configuration in the log of node 2 held, and
        // node 2 has a committed configuration that lacks it.
        #[test]
        fn a_node_that_no_configuration_held_keeps_its_group() {
            let mut run = Run::new();
            let entries = changes(1, &[(&[1, 2, 3][..], &[][..])]);
            run.start(opening(2, &entries, 1, 1), false);
            let stranger = Opening {
                voters: vec![1, 2, 3, 4],
                ..opening(4, &entries, 0, 0)
            };
            run.start(stranger, false);
            run.run(seconds(10));
            let refused = run.refused(2, 4);
            let not_voter = Error::NotVoter { from: key(4) };
            assert!(!refused.is_empty());
            assert!(
                refused.iter().all(|error| *error == not_voter),
                "{refused:?}"
            );
            run.run(seconds(10));
            assert!(run.refused(2, 4).len() > refused.len());
            assert_eq!(run.record().stopped, BTreeMap::new());
        }

        // Node 2 holds the entries that add node 4 and remove it again, past its
        // commit, which lags at the first entry. Node 4 joined with the voters of
        // the entry that added it.
        #[test]
        fn a_node_that_only_an_entry_past_the_commit_held_keeps_its_group() {
            let mut run = Run::new();
            let sets = [
                (&[1, 2, 3][..], &[][..]),
                (&[1, 2, 3, 4], &[1, 2, 3]),
                (&[1, 2, 3, 4], &[]),
                (&[1, 2, 3], &[1, 2, 3, 4]),
                (&[1, 2, 3], &[]),
            ];
            let entries = changes(1, &sets);
            run.start(opening(2, &entries, 5, 1), false);
            let added = Opening {
                voters: vec![1, 2, 3, 4],
                ..opening(4, &entries, 0, 0)
            };
            run.start(added, false);
            run.run(seconds(10));
            let refused = run.refused(2, 4);
            let not_voter = Error::NotVoter { from: key(4) };
            assert!(!refused.is_empty());
            assert!(
                refused.iter().all(|error| *error == not_voter),
                "{refused:?}"
            );
            run.run(seconds(10));
            assert!(run.refused(2, 4).len() > refused.len());
            assert_eq!(run.record().stopped, BTreeMap::new());
        }

        // Node 2 holds the joint entry and the leave that remove node 4, neither
        // committed, so node 4 is a peer and not a voter. Node 4 holds a committed
        // configuration that lacks node 2. Node 2 wins with node 3.
        #[test]
        fn a_removed_answer_from_a_node_that_is_not_a_voter_changes_nothing() {
            let mut run = Run::new();
            let all = [1, 2, 3, 4];
            let leaving = changes(1, &[(&[1, 2, 3][..], &all[..]), (&[1, 2, 3], &[])]);
            let leader = Opening {
                voters: all.to_vec(),
                ..opening(2, &leaving, 2, 0)
            };
            run.start(leader, false);
            let voter = Opening {
                voters: all.to_vec(),
                ..opening(3, &leaving, 0, 0)
            };
            run.start(voter, false);
            let alone = changes(1, &[(&[1, 4][..], &all[..]), (&[1, 4], &[])]);
            let liar = Opening {
                voters: all.to_vec(),
                ..opening(4, &alone, 2, 2)
            };
            run.start(liar, false);
            run.run(seconds(10));
            let refused = run.refused(4, 2);
            let removed = Error::Removed { from: key(2) };
            assert!(!refused.is_empty());
            assert!(refused.iter().all(|error| *error == removed), "{refused:?}");
            let record = run.record();
            assert_eq!(record.stopped, BTreeMap::new());
            let leader = Some(key(2));
            assert_eq!(record.leaders[&2], leader);
            assert_eq!(record.leaders[&3], leader);
        }
    }

    /// A chain with a vote or a leader of a node that has no key at this node.
    mod chain {
        use super::*;
        use crate::common::proven_at;

        pub(super) const FOUNDERS: [u8; 3] = [1, 2, 3];
        pub(super) const ALL: [u8; 4] = [1, 2, 3, 4];

        fn voters(incoming: &[u8], outgoing: &[u8]) -> Voters {
            Voters {
                incoming: incoming.iter().map(|&id| key(id)).collect(),
                outgoing: outgoing.iter().map(|&id| key(id)).collect(),
            }
        }

        // The configuration entry `voters` that `leader` wrote at `index` of `term`
        // with the votes of `voted`, as a link.
        fn link(
            leader: u8,
            term: u64,
            index: u64,
            voters: Voters,
            voted: &[u8],
        ) -> raft::Link {
            let at = Position {
                term: Term(term),
                index,
            };
            let entry = common::change_voted(leader, at, voters, voted);
            let Data::Voters(change) = entry.data else {
                unreachable!("a change is a voters entry");
            };
            raft::Link { at, change }
        }

        // The chain of a region whose founders are 1, 2 and 3. Leader 2 of term 4,
        // which 2 and 3 elected, made node 4 a voter. `leader` of term 5, which
        // `voted` elected, then moved the voters to 3 alone.
        pub(super) fn links(leader: u8, voted: &[u8]) -> Vec<raft::Link> {
            vec![
                link(2, 4, 1, voters(&ALL, &FOUNDERS), &[2, 3]),
                link(2, 4, 2, voters(&ALL, &[]), &[2, 3]),
                link(leader, 5, 3, voters(&[3], &ALL), voted),
                link(leader, 5, 4, voters(&[3], &[]), voted),
            ]
        }

        // A heartbeat of term 6 from leader 3, which elected itself alone, to node
        // 1, with `chain`.
        pub(super) fn heartbeat(chain: Vec<raft::Link>) -> raft::Message {
            let body = Body::Heartbeat { commit: 0 };
            let mut heartbeat = proven_at(3, 1, Term(6), &[(3, 3)], body);
            heartbeat.chain = chain;
            heartbeat
        }

        pub(super) fn unproven() -> Error {
            Error::Raft(raft::Error::Unproven {
                term: Term(6),
                from: key(3),
            })
        }

        // Node 1, a founder in term 0 with an empty log and the keys of `members`,
        // takes `heartbeat` with `expected`. On `Ok` it is in term 6 and replies.
        // On an error it stays in term 0 and sends nothing.
        fn takes(
            members: &'static [u8],
            heartbeat: raft::Message,
            expected: Result<(), Error>,
        ) {
            solo(move |node, tasks| async move {
                let mesh = open(&node, &tasks, 1, members, &FOUNDERS).await.unwrap();
                assert_eq!(mesh.receive(public(3), heartbeat), expected);
                if expected.is_ok() {
                    assert_eq!(term(&mesh), Term(6));
                    let reply = mesh.outgoing(key(3)).await.unwrap();
                    let mut expected = message(1, 3, Body::HeartbeatReply);
                    expected.term = Term(6);
                    assert_eq!(reply, expected);
                } else {
                    assert_eq!(term(&mesh), Term(0));
                    assert!(quiet(&mesh, 3).await);
                }
            });
        }

        #[test]
        fn a_link_with_a_vote_of_no_key_and_a_quorum_of_known_votes_proves() {
            takes(&FOUNDERS, heartbeat(links(2, &ALL)), Ok(()));
        }

        #[test]
        fn a_link_with_no_quorum_of_known_votes_proves_nothing() {
            takes(&ALL, heartbeat(links(2, &[2, 3, 4])), Ok(()));
            takes(&FOUNDERS, heartbeat(links(2, &[2, 3, 4])), Err(unproven()));
        }

        // The founders' votes of term 6 for 3 prove nothing against the voters
        // after the two links of 4, which a cut leaves out.
        #[test]
        fn a_chain_cut_leaves_out_each_link_after_the_cut() {
            let mut chain = links(4, &ALL);
            chain.push(link(3, 6, 5, voters(&[3], &[]), &FOUNDERS));
            let body = Body::Heartbeat { commit: 0 };
            let mut heartbeat = proven_at(3, 1, Term(7), &[(3, 3)], body);
            heartbeat.chain = chain;
            let unproven = raft::Error::Unproven {
                term: Term(7),
                from: key(3),
            };
            takes(&FOUNDERS, heartbeat, Err(Error::Raft(unproven)));
        }

        #[test]
        fn a_chain_is_cut_before_a_link_whose_leader_has_no_key() {
            takes(&ALL, heartbeat(links(4, &ALL)), Ok(()));
            let mut cut = heartbeat(links(4, &ALL));
            cut.chain[2].change.signature = Some(raft::Signature([0; 64]));
            takes(&FOUNDERS, cut, Err(unproven()));
        }

        #[test]
        fn a_forged_link_vote_of_a_known_node_refuses_the_message() {
            let mut forged = heartbeat(links(2, &ALL));
            let votes = &mut forged.chain[2].change.votes.voters;
            votes.get_mut(&key(3)).unwrap().as_mut().unwrap().0[63] ^= 1;
            let claim = Error::Claim(claim::Error::Forged { signer: key(3) });
            takes(&FOUNDERS, forged, Err(claim));
        }
    }

    /// An append that the check cuts before an entry with a claim of a node whose
    /// join is in the same run.
    mod cut {
        use transport::stream::Incoming;

        use super::*;
        use crate::common::proven_at;

        fn at(term: u64, index: u64) -> Position {
            Position {
                term: Term(term),
                index,
            }
        }

        fn voters(incoming: &[u8], outgoing: &[u8]) -> Voters {
            Voters {
                incoming: incoming.iter().map(|&id| key(id)).collect(),
                outgoing: outgoing.iter().map(|&id| key(id)).collect(),
            }
        }

        fn bytes(at: Position, change: &Change) -> Entry {
            Entry {
                at,
                data: Data::Bytes(encoded(change)),
            }
        }

        fn empty(at: Position) -> Entry {
            Entry {
                at,
                data: Data::Empty,
            }
        }

        /// The log of a region whose founders are 1, 2 and 3. Leader 2 of term 4
        /// admits node 4 and makes it a voter. Leader 2 of term 5, which 2, 3 and 4
        /// elected, removes it as a voter. Leader 3 of term 6 starts its term.
        fn history() -> Vec<Entry> {
            vec![
                empty(at(4, 1)),
                bytes(at(4, 2), &ticket()),
                bytes(at(4, 3), &join(4)),
                common::change_voted(
                    2,
                    at(4, 4),
                    voters(&[1, 2, 3, 4], &[1, 2, 3]),
                    &[2, 3],
                ),
                common::change_voted(2, at(4, 5), voters(&[1, 2, 3, 4], &[]), &[2, 3]),
                empty(at(5, 6)),
                common::change_voted(
                    2,
                    at(5, 7),
                    voters(&[1, 2, 3], &[1, 2, 3, 4]),
                    &[2, 3, 4],
                ),
                common::change_voted(2, at(5, 8), voters(&[1, 2, 3], &[]), &[2, 3, 4]),
                empty(at(6, 9)),
            ]
        }

        /// The append of `entries` after `prev` from leader 3 of term 6, which 2
        /// and 3 elected, to node `to`.
        fn append(
            to: u8,
            prev: Position,
            entries: Vec<Entry>,
            commit: u64,
        ) -> raft::Message {
            let body = Body::Append {
                prev,
                entries,
                commit,
            };
            proven_at(3, to, Term(6), &[(2, 2), (3, 3)], body)
        }

        /// Gives `mesh` the append of `entries` after `prev` from leader 3, and the
        /// body of the reply.
        async fn take(
            mesh: &Mesh,
            prev: Position,
            entries: Vec<Entry>,
            commit: u64,
        ) -> Body {
            let append = append(1, prev, entries, commit);
            assert_eq!(mesh.receive(public(3), append), Ok(()));
            let reply = mesh.outgoing(key(3)).await.unwrap();
            assert_eq!(
                (reply.from, reply.to, reply.term),
                (key(1), key(3), Term(6))
            );
            reply.body
        }

        // Node 1 was down from the start. It holds no entry, so its members are the
        // founders. The leader sends it the log in one append, as `raft` does for a
        // log of at most 64 entries. The node cuts the run before the first change
        // with the vote of node 4, and takes the rest from there.
        #[test]
        fn a_founder_that_was_down_takes_the_log_of_a_leader_in_a_cut_and_the_rest() {
            let entries = solo_stored(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let reply = take(&mesh, Position::default(), history(), 9).await;
                assert_eq!(reply, Body::AppendReply { last: 6 });
                assert!(mesh.member(key(4)).is_some());
                let rest = history().split_off(6);
                let reply = take(&mesh, at(5, 6), rest, 9).await;
                assert_eq!(reply, Body::AppendReply { last: 9 });
            });
            assert_eq!(entries, history());
        }

        // The same log in two appends: the join of node 4 commits and applies before
        // the append that holds the vote of node 4.
        #[test]
        fn a_founder_that_was_down_takes_the_same_log_in_two_appends() {
            let entries = solo_stored(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let mut first = history();
                let second = first.split_off(3);
                let reply = take(&mesh, Position::default(), first, 3).await;
                assert_eq!(reply, Body::AppendReply { last: 3 });
                node.clock().sleep(TICK).await;
                assert!(mesh.member(key(4)).is_some());
                let reply = take(&mesh, at(4, 3), second, 9).await;
                assert_eq!(reply, Body::AppendReply { last: 9 });
            });
            assert_eq!(entries, history());
        }

        #[test]
        fn a_forged_change_of_a_known_leader_before_the_cut_refuses_the_append() {
            let entries = solo_stored(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let mut forged = history();
                let Data::Voters(change) = &mut forged[3].data else {
                    unreachable!()
                };
                change.signature.as_mut().unwrap().0[0] ^= 1;
                let append = append(1, Position::default(), forged, 9);
                let refused = Error::Claim(claim::Error::Forged { signer: key(2) });
                assert_eq!(mesh.receive(public(3), append), Err(refused));
                assert!(quiet(&mesh, 3).await);
                assert_eq!(term(&mesh), Term(0));
                assert_eq!(mesh.member(key(4)), None);
                let reply = take(&mesh, at(4, 3), Vec::new(), 0).await;
                assert_eq!(reply, Body::AppendReject { hint: 0 });
            });
            assert_eq!(entries, []);
        }

        #[test]
        fn a_cut_run_after_a_prev_the_node_does_not_hold_is_rejected() {
            let entries = solo_stored(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let rest = history().split_off(3);
                let reply = take(&mesh, at(4, 3), rest, 9).await;
                assert_eq!(reply, Body::AppendReject { hint: 0 });
                assert_eq!(mesh.member(key(4)), None);
                let reply = take(&mesh, Position::default(), history(), 9).await;
                assert_eq!(reply, Body::AppendReply { last: 6 });
                let rest = history().split_off(6);
                let reply = take(&mesh, at(5, 6), rest, 9).await;
                assert_eq!(reply, Body::AppendReply { last: 9 });
            });
            assert_eq!(entries, history());
        }

        #[test]
        fn the_commit_after_a_cut_is_not_above_the_last_entry_kept() {
            let mut whole = history();
            whole.push(bytes(at(6, 10), &home(1)));
            let expected = whole.clone();
            let entries = solo_stored(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let mut watch = mesh.watch(INDEX);
                assert_eq!(watch.next().await, Ok(None));
                let reply = take(&mesh, Position::default(), whole.clone(), 10).await;
                assert_eq!(reply, Body::AppendReply { last: 6 });
                node.clock().sleep(TICK).await;
                assert!(mesh.member(key(4)).is_some());
                assert!(now(pin!(watch.next())).await.is_pending());
                let rest = whole.split_off(6);
                let reply = take(&mesh, at(5, 6), rest, 10).await;
                assert_eq!(reply, Body::AppendReply { last: 10 });
                assert_eq!(watch.next().await, Ok(Some(key(1))));
            });
            assert_eq!(entries, expected);
        }

        /// Serves each stream that node 1 opens to `transport` as `accept` does, and
        /// records the body of each message before the mesh takes it.
        async fn accept_recorded(
            mesh: Mesh,
            transport: Rc<Transport>,
            tasks: Tasks,
            bodies: Arc<Mutex<Vec<Body>>>,
        ) -> ! {
            loop {
                let session = transport.accept().await.unwrap();
                let (mesh, streams) = (mesh.clone(), tasks.clone());
                let bodies = Arc::clone(&bodies);
                tasks.spawn(async move {
                    while let Ok(incoming) = session.accept().await {
                        let (mesh, bodies) = (mesh.clone(), Arc::clone(&bodies));
                        streams.spawn(async move {
                            let Incoming { mut receiver, .. } = incoming;
                            let Ok(Some(header)) = receiver.recv().await else {
                                return;
                            };
                            let protocol = wire::header::decode(&header).unwrap();
                            assert_eq!(protocol, (Protocol::Mesh, &[][..]));
                            while let Ok(Some(bytes)) = receiver.recv().await {
                                let Some(Message::Raft(message)) =
                                    Message::decode(&bytes)
                                else {
                                    panic!("node 1 sent what is not a raft message");
                                };
                                bodies.lock().unwrap().push(message.body.clone());
                                assert_eq!(mesh.receive(public(1), message), Ok(()));
                            }
                        });
                    }
                });
            }
        }

        /// Node `id` of a pair. Node 2 first takes the history from leader 3, in two
        /// appends, and records the body of each message node 1 sends it.
        async fn peer(
            node: sim::node::Node,
            tasks: Tasks,
            id: u8,
            bodies: Arc<Mutex<Vec<Body>>>,
        ) -> ! {
            let config = dialed_at(&node, &tasks, id, PORT, &IDS, &IDS).await;
            let transport = Rc::clone(&config.transport);
            let mesh = Mesh::open(config).await.unwrap();
            if id == 1 {
                accept(mesh, transport, tasks).await;
            }
            let mut first = history();
            let second = first.split_off(3);
            let head = append(2, Position::default(), first, 3);
            assert_eq!(mesh.receive(public(3), head), Ok(()));
            let tail = append(2, at(4, 3), second, 9);
            assert_eq!(mesh.receive(public(3), tail), Ok(()));
            accept_recorded(mesh, transport, tasks, bodies).await
        }

        // Node 2 holds the history and leads term 7 with the vote of node 1, which
        // holds no entry. Its first append to node 1 is cut before index 7. With no
        // proposal, node 1 holds the whole log of node 2 after the next heartbeat.
        #[test]
        fn a_follower_that_cut_a_run_holds_the_whole_log_after_the_next_heartbeat() {
            let mut sim = Sim::new(sim::Config::default());
            let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
            let bodies = Arc::new(Mutex::new(Vec::new()));
            for (node, id) in nodes.iter().zip([1, 2]) {
                let (own, bodies) = (node.clone(), Arc::clone(&bodies));
                let config = env::shards::Config {
                    name: format!("peer-{id}"),
                    core: None,
                };
                let main =
                    move |tasks| async move { peer(own, tasks, id, bodies).await };
                drop(node.shards().start(config, main).unwrap());
            }
            sim.run_for(seconds(10)).unwrap();
            let answers: Vec<_> = mem::take(&mut *bodies.lock().unwrap())
                .into_iter()
                .filter(|body| {
                    matches!(body, Body::AppendReply { .. } | Body::AppendReject { .. })
                })
                .collect();
            let expected = [
                Body::AppendReject { hint: 0 },
                Body::AppendReply { last: 6 },
                Body::AppendReply { last: 10 },
            ];
            assert_eq!(answers[..3], expected, "{answers:?}");
            assert_eq!(answers.last(), Some(&Body::AppendReply { last: 10 }));
            for node in &nodes {
                sim.crash(node, Crash::Power);
            }
            let [one, two] = nodes;
            let (one, two) = (stored(&mut sim, &one), stored(&mut sim, &two));
            assert_eq!(two[..9], history());
            assert_eq!(two.len(), 10, "{two:?}");
            assert_eq!(two[9].at.term, Term(7));
            assert_eq!(one, two);
        }
    }
}
