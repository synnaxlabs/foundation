//! Drives the `raft` group of one region on one shard.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::poll_fn;
use std::mem;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};

use block::Pool;
use env::clock::Clock;
use env::entropy::Entropy;
use env::files::Files;
use env::tasks::Tasks;
use raft::{Body, Data, Entry, Position, Raft, Ready, Start, Voters};
use transport::Transport;
use types::channel;
use types::name::Prefix;
use types::node::{self, PrivateKey, PublicKey};
use types::time::{Span, Stamp};

use crate::claim::{self, Known, Signer};
use crate::error::{Error, Stopped};
use crate::log::{self, Log};
use crate::member::Member;
use crate::message::Message;
use crate::region::{self, Change, Join, Malformed, Refused, Request};
use crate::status::Status;
use send::Senders;

mod send;
mod stream;

/// The time of one `raft` tick.
const TICK: Span = Span::from_nanos(100 * Span::MILLISECOND.nanos());
const ELECTION_TICKS: u32 = 10;
const HEARTBEAT_TICKS: u32 = 1;
/// The most messages that wait for one member.
const QUEUE_MAX: usize = 64;
/// The directory of the log, in the mesh's directory.
const LOG: &str = "log";

/// What a [`Mesh`] is built from.
pub(crate) struct Config {
    /// This node.
    pub(crate) key: node::Key,
    /// This node's private key. It signs the node's claims.
    pub(crate) private_key: PrivateKey,
    /// The prefix of the region's names, [`Prefix::ROOT`] for the root region.
    pub(crate) region: Prefix,
    /// Each member of the region, this node included, one record for each node. A
    /// member's peer proves the public key of its card, and that key signs the member's
    /// claims.
    pub(crate) members: Vec<Member>,
    /// The voters before the first entry of the log, the same at each open. Each is a
    /// member. A node that joins gives the founding voters from its join answer. A node
    /// with no voter takes no request.
    pub(crate) voters: BTreeSet<node::Key>,
    /// The mesh's directory.
    pub(crate) files: Files,
    /// Times the ticks of the group.
    pub(crate) clock: Clock,
    /// Gives the mesh time of each join that this node stamps.
    pub(crate) time: clock::Reader,
    /// Gives each election timeout its random part, and the random part of each status
    /// key that this node makes.
    pub(crate) entropy: Entropy,
    /// Runs the group's task and the tasks that send.
    pub(crate) tasks: Tasks,
    /// Gives the blocks of the log's reads and writes, of each message that the group
    /// sends, and of each answer to a forwarded proposal. A write that finds the pool
    /// full, or that the system refuses memory for, waits: the group takes, sends, and
    /// applies nothing until that write ends. A message that finds no block drops.
    pub(crate) pool: Rc<Pool>,
    /// The transport of this shard. The mesh dials each other member on it.
    pub(crate) transport: Rc<Transport>,
}

/// One node's part in the group of a region. Clones share it. The group runs until
/// it stops or each clone drops. It stays on the shard that opened it.
///
/// The group's task ends soon after the last clone drops. A write in progress ends
/// first, and a write that waits for a block ends at the next tick. Until then, a new
/// open of the same directory gives [`Error::Log`].
#[derive(Clone)]
pub(crate) struct Mesh {
    group: Rc<RefCell<Group>>,
    pool: Rc<Pool>,
    time: clock::Reader,
    entropy: Entropy,
}

impl Mesh {
    /// Reads the log from `config.files`, starts the group as a follower, and spawns
    /// its task on `config.tasks`. Homes are known again when this node applies the
    /// log, after it hears the leader.
    ///
    /// The group sends its messages on a session to each member. It dials a member at
    /// the addresses of its card, at the first message for it, and again after the
    /// session fails. It never closes a session.
    ///
    /// # Errors
    ///
    /// - [`Error::Member`] when the region cannot hold one of `config.members`, or two
    ///   name one node.
    /// - [`Error::NotMember`] when `config.members` lacks this node or a voter.
    /// - [`Error::WrongKey`] when `config.private_key` is not the key of this node in
    ///   `config.members`.
    /// - [`Error::Log`] when the log does not open.
    /// - [`Error::Raft`] when `raft` refuses the log.
    pub(crate) async fn open(config: Config) -> Result<Self, Error> {
        let (transport, tasks) = (Rc::clone(&config.transport), config.tasks.clone());
        let mesh = Self::start(config).await?;
        let senders = Senders {
            group: Rc::downgrade(&mesh.group),
            transport,
            pool: Rc::clone(&mesh.pool),
            tasks: tasks.clone(),
        };
        tasks.spawn(senders.run());
        Ok(mesh)
    }

    // Opens the group with no task that sends: `outgoing` gives each message.
    async fn start(config: Config) -> Result<Self, Error> {
        let state =
            region::State::new(config.region, config.members).map_err(Error::Member)?;
        let signer = Signer::new(config.key, &config.private_key);
        match state.member(config.key) {
            None => return Err(Error::NotMember(config.key)),
            Some(own) if !signer.owns(own.public_key()) => {
                return Err(Error::WrongKey);
            }
            Some(_) => {}
        }
        let mut voters = config.voters.iter();
        if let Some(&key) = voters.find(|&&key| state.member(key).is_none()) {
            return Err(Error::NotMember(key));
        }
        let pool = Rc::clone(&config.pool);
        let (log, stored) = Log::open(config.files, LOG.into(), config.pool).await?;
        let unapplied = written(&stored.entries).collect();
        let start = Start {
            hard: stored.hard,
            voters: Voters {
                incoming: config.voters,
                outgoing: BTreeSet::new(),
            },
            entries: stored.entries,
            applied: 0,
        };
        let fixed = raft::Config {
            key: config.key,
            election_ticks: ELECTION_TICKS,
            heartbeat_ticks: HEARTBEAT_TICKS,
        };
        let group = Rc::new(RefCell::new(Group {
            raft: Raft::new(fixed, start)?,
            state,
            queues: BTreeMap::new(),
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
        }));
        let weak = Rc::downgrade(&group);
        config.tasks.spawn(run(
            weak,
            log,
            signer,
            config.clock,
            config.entropy.clone(),
        ));
        Ok(Self {
            group,
            pool,
            time: config.time,
            entropy: config.entropy,
        })
    }

    /// A watch of the home of `index`.
    pub(crate) fn watch(&self, index: channel::Key) -> Watch {
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
    pub(crate) fn member(&self, key: node::Key) -> Option<Member> {
        self.group.borrow().state.member(key).cloned()
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
    /// - [`Error::NotVoter`] when the message is a request and its sender is not a
    ///   voter of this node's configuration.
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
        let Voters { incoming, outgoing } = group.raft.voters();
        let voter = incoming.contains(&from) || outgoing.contains(&from);
        if request(&message.body) && !voter {
            return Err(Error::NotVoter { from });
        }
        claim::check(&group.raft, &mut message, |key| group.public_key(key))?;
        group.raft.step(message)?;
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
            let Voters { incoming, outgoing } = group.raft.voters();
            let holds = |voter: &node::Key| {
                group.public_key(*voter).map(Known::public_key) == Some(peer)
            };
            if !incoming.iter().chain(outgoing).any(holds) {
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

    /// The `Join` of `request` at the later edge of this node's mesh time, with a new
    /// UUIDv7 key for each status name. The caller proposes it, or forwards it to the
    /// leader. Each node checks the join when it applies it.
    ///
    /// # Errors
    ///
    /// - [`Error::Unsynced`] when this node has no mesh time, when its error is
    ///   unknown, or when the later edge is before the Unix epoch, where a UUIDv7
    ///   key has no time.
    /// - [`Error::Status`] when `request` names more than 64 status channels.
    pub(crate) fn stamp(&self, request: Request) -> Result<Change, Error> {
        let measurement = match self.time.status() {
            clock::Status::Synced(measurement)
            | clock::Status::Holdover(measurement, _) => measurement,
            clock::Status::Unsynced(_) => return Err(Error::Unsynced),
        };
        let at = measurement.interval().latest;
        if !measurement.known() || at < Stamp::EPOCH {
            return Err(Error::Unsynced);
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
            status: Status::new(keys).map_err(Error::Status)?,
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

/// A watch of the home of one index.
pub(crate) struct Watch {
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

impl Watch {
    /// The first call returns the home of the index at once. Each later call waits
    /// until the home differs from the one it last returned, and returns the newest:
    /// two changes between calls give one result. `None` means that no applied entry
    /// set a home for the index. It is never `None` after a home, because no change
    /// clears a home.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] with the cause, at once, on each call after the group stops
    /// or each [`Mesh`] of it drops. A group that stopped keeps its cause when each
    /// [`Mesh`] drops.
    pub(crate) async fn next(&mut self) -> Result<Option<node::Key>, Error> {
        poll_fn(|cx| {
            if let Some(stopped) = self.stopped.get() {
                return Poll::Ready(Err(Error::Stopped(stopped.clone())));
            }
            let Some(group) = self.group.upgrade() else {
                return Poll::Ready(Err(Error::Stopped(Stopped::Dropped)));
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
    // Why the group stopped. Each watch shares it, so the cause outlives the group.
    stopped: Rc<OnceCell<Stopped>>,
    // The task of `run`, while it waits for an input.
    task: Option<Waker>,
    // The task of each watch that waits in `Watch::next`.
    watches: BTreeMap<u64, Waker>,
    // Each proposal since the task last took a `Ready`. The next `Ready` holds the
    // entry of each, unless a new leader replaced the entry.
    proposals: Vec<Rc<Proposal>>,
    // The count of slots given, which is the slot of the next watch.
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
}

impl Group {
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

    // Wakes each task that sends, so that it ends.
    fn wake_senders(&mut self) {
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
            let bytes = match data {
                Data::Bytes(bytes) => bytes,
                Data::Empty | Data::Voters(_) => continue,
            };
            let applied = match Change::decode(&bytes) {
                Ok(change) => self.state.apply(change),
                // Every node of this build judges a body the same way.
                Err(Malformed::Body { kind, length }) => {
                    Err(Refused::Body { kind, length })
                }
                Err(Malformed::Unknown(cause)) => {
                    return Err(Stopped::Change { at, cause });
                }
            };
            // A refused change is a no-op on every node.
            if let Ok(Some(_)) = applied {
                self.wake_watches();
            }
        }
        Ok(())
    }

    fn stop(&mut self, stopped: Stopped) {
        self.stopped.get_or_init(|| stopped);
        self.wake_senders();
        self.wake_watches();
        for proposal in &self.proposals {
            proposal.wake();
        }
    }

    fn wake_watches(&mut self) {
        mem::take(&mut self.watches)
            .into_values()
            .for_each(Waker::wake);
    }
}

impl Drop for Group {
    // Each task must end, which frees the log, and a watch that waits must learn
    // that each mesh dropped.
    fn drop(&mut self) {
        self.wake();
        self.wake_senders();
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
            // A tick that a slow write hides is lost, so the group's time only
            // slows.
            while Pin::new(&mut tick).poll(cx).is_ready() {
                group.raft.tick(rng.next_u64());
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
        // The pool may give the blocks later, so the write runs again at each tick.
        let written = loop {
            let cause = match log.write(ready.hard.clone(), &ready.entries).await {
                Err(log::Error::Pool(
                    cause @ (block::Error::Exhausted { .. }
                    | block::Error::Refused { .. }),
                )) => cause,
                written => break written,
            };
            if let Some(group) = group.upgrade() {
                group.borrow_mut().waits = Some(cause);
            }
            (&mut tick).await;
            tick = clock.sleep(TICK);
            if group.strong_count() == 0 {
                return;
            }
        };
        let Some(group) = group.upgrade() else { return };
        let mut group = group.borrow_mut();
        group.waits = None;
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
        for proposal in &proposals {
            proposal.wake();
        }
        if let Err(stopped) = applied {
            group.stop(stopped);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::mem::ManuallyDrop;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::num::{NonZeroU32, NonZeroUsize};
    use std::path::Path;
    use std::pin::pin;
    use std::sync::{Arc, Mutex};

    use block::testing::Scarce;
    use env::files::{self, Operation};
    use raft::{Answer, Grant, Hard, Proof, Term};
    use sim::{Crash, Sim, link};
    use transport::{Address, Peer, Port};
    use types::node::SealKey;
    use wire::Protocol;

    use super::*;
    use crate::card;
    use crate::common::{self, create_pool, key, message, private, proven, public};
    use crate::region::{Unfit, Unknown};
    use crate::status::Many;
    use crate::ticket::Options;

    const IDS: [u8; 3] = [1, 2, 3];
    const PORT: u16 = 7000;
    /// The idle time of each transport.
    const IDLE: Span = Span::from_nanos(60 * Span::SECOND.nanos());
    const INDEX: channel::Key = channel::Key::from_u128(7);

    /// What each node's watch gave, in order.
    type Homes = BTreeMap<u8, Vec<Option<node::Key>>>;

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
        /// The region state of each node, at the same time.
        states: BTreeMap<u8, region::State>,
        /// The join request that each node stamps.
        requests: BTreeMap<u8, Request>,
        /// The join that each node stamped.
        stamped: BTreeMap<u8, Change>,
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

    fn config(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        members: &[u8],
        voters: &[u8],
    ) -> Config {
        config_at(node, tasks, id, 0, members, voters)
    }

    /// As [`config`], with the transport at `port` of `node`.
    fn config_at(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        port: u16,
        members: &[u8],
        voters: &[u8],
    ) -> Config {
        let pool = create_pool();
        Config {
            key: key(id),
            private_key: private(id),
            region: "plant".parse().unwrap(),
            members: common::create_members(members),
            voters: voters.iter().map(|&id| key(id)).collect(),
            files: node.files(),
            clock: node.clock(),
            time: synced(node),
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
        }
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
            message_bytes_max: NonZeroUsize::new(pool.largest().min(1 << 16)).unwrap(),
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
        Mesh::start(config(node, tasks, id, members, voters)).await
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
    async fn stamp(mesh: Mesh, clock: Clock, id: u8, board: Arc<Mutex<Board>>) -> ! {
        loop {
            clock.sleep(TICK).await;
            let request = board.lock().unwrap().requests.remove(&id);
            let Some(request) = request else {
                continue;
            };
            let join = mesh.stamp(request).unwrap();
            board.lock().unwrap().stamped.insert(id, join);
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

    async fn voter(
        node: sim::node::Node,
        tasks: Tasks,
        id: u8,
        board: Arc<Mutex<Board>>,
    ) -> ! {
        let base = config_at(&node, &tasks, id, PORT, &IDS, &IDS);
        let transport = Rc::clone(&base.transport);
        let hidden = board.lock().unwrap().hidden;
        let member = |of| {
            if hidden == Some(of) {
                common::member(of)
            } else {
                create_voter(of)
            }
        };
        let config = Config {
            members: IDS.map(member).into(),
            ..base
        };
        let mesh = Mesh::open(config).await.unwrap();
        let (serving, proposing, streams) = (mesh.clone(), mesh.clone(), tasks.clone());
        tasks.spawn(async move {
            accept(serving, transport, streams).await;
        });
        let (clock, script) = (node.clock(), Arc::clone(&board));
        tasks.spawn(async move {
            propose(proposing, clock, id, script).await;
        });
        let (answering, clock, forwards) =
            (mesh.clone(), node.clock(), Arc::clone(&board));
        tasks.spawn(async move {
            answer(answering, clock, id, forwards).await;
        });
        let (stamping, clock, requests) =
            (mesh.clone(), node.clock(), Arc::clone(&board));
        tasks.spawn(async move {
            stamp(stamping, clock, id, requests).await;
        });
        let mut watch = mesh.watch(INDEX);
        loop {
            let home = watch.next().await.unwrap();
            let records: BTreeMap<_, _> = (1..10)
                .filter_map(|of| Some((of, mesh.member(key(of))?)))
                .collect();
            // No call of `Mesh` gives the use count of a ticket.
            let state = mesh.group.borrow().state.clone();
            let mut board = board.lock().unwrap();
            board.states.insert(id, state);
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

    #[test]
    fn the_other_voters_agree_when_the_card_of_a_voter_has_no_address() {
        let mut cluster = Cluster::new(5);
        cluster.board.lock().unwrap().hidden = Some(3);
        cluster.script(home);
        cluster.start();
        cluster.run(seconds(10));
        let (led, homes) = cluster.take();
        let &[leader] = led.as_slice() else {
            panic!("the group took a proposal from each of {led:?}");
        };
        let home = |id| match id {
            3 => (id, vec![None]),
            _ => (id, vec![None, Some(key(leader))]),
        };
        assert_eq!(homes, IDS.map(home).into());
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
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        sim.run_on(&node, body).unwrap();
    }

    /// Proposes `change` once per tick until this node leads.
    async fn lead(mesh: &Mesh, clock: &Clock, change: Change) -> Position {
        let follower = Error::Raft(raft::Error::NotLeader { leader: None });
        loop {
            match mesh.propose(change.clone()).await {
                Ok(at) => return at,
                Err(error) => assert_eq!(error, follower),
            }
            clock.sleep(TICK).await;
        }
    }

    /// Makes each sync of the log fail, and gives why the group then stops.
    fn fail_sync(node: &sim::node::Node) -> Error {
        let path = Path::new(LOG).join("log-0");
        node.fail_file(&path, Operation::Sync);
        let cause = files::Error::Io {
            path,
            operation: Operation::Sync,
            code: 5,
        };
        Error::Stopped(Stopped::Write(log::Error::Files(cause)))
    }

    fn term(mesh: &Mesh) -> Term {
        mesh.group.borrow().raft.term()
    }

    /// What `future` gives, or `None` when it waits for longer than `limit`.
    async fn within<F: Future>(
        clock: &Clock,
        limit: Span,
        mut future: Pin<&mut F>,
    ) -> Option<F::Output> {
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

    #[test]
    fn each_proposal_returns_after_the_write_of_its_entry() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &[1], &[1])
            };
            let mesh = Mesh::start(config).await.unwrap();
            let first = lead(&mesh, &node.clock(), home(1)).await;
            let held = fill(&pool);
            let results = Rc::new(RefCell::new(Vec::new()));
            for id in [2, 3] {
                let (other, results) = (mesh.clone(), Rc::clone(&results));
                tasks.spawn(async move {
                    let proposed = other.propose(home(id)).await;
                    results.borrow_mut().push(proposed.unwrap());
                });
            }
            node.clock().sleep(Span::from_nanos(TICK.nanos() * 3)).await;
            assert_eq!(*results.borrow(), []);
            drop(held);
            node.clock().sleep(Span::from_nanos(TICK.nanos() * 2)).await;
            let mut positions = results.take();
            positions.sort_by_key(|at| at.index);
            assert_eq!(positions, [after(first, 1), after(first, 2)]);
        });
    }

    // The second proposal comes while the write of the first one is in a disk call.
    #[test]
    fn a_proposal_gives_the_cause_when_the_write_before_its_own_fails() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let stopped = fail_sync(&node);
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
            let stopped = Error::Stopped(Stopped::Change { at: bad, cause });
            assert_eq!(mesh.watch(INDEX).next().await, Err(stopped));
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
            let stopped = Error::Stopped(Stopped::Change { at: bad, cause });
            assert_eq!(mesh.watch(INDEX).next().await, Err(stopped));
        });
    }

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
                let stopped = fail_sync(&node);
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
                    ..config(&node, &tasks, 1, &[1], &[1])
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
                assert_eq!(term(&mesh), Term(0));
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

        // Members 2 and 4 share one public key. Node 2 is a voter, and node 4 is
        // not.
        #[test]
        fn not_voter_names_no_key_that_a_voter_holds() {
            solo(|node, tasks| async move {
                let mut config = config(&node, &tasks, 1, &IDS, &IDS);
                let mut card = common::member(2).card.card().clone();
                card.name = "plant.node4".parse().unwrap();
                let card = card::Signed::sign(key(4), card, &private(2));
                config.members.push(Member {
                    card,
                    ..common::member(4)
                });
                let mesh = Mesh::start(config).await.unwrap();
                let heartbeat = message(4, 1, Body::Heartbeat { commit: 0 });
                let refused = mesh.receive(public(2), heartbeat);
                assert_eq!(refused, Err(Error::NotVoter { from: key(4) }));
                let answer = mesh.answer(public(2), home(2)).await;
                assert_eq!(answer, Ok(Message::NotLeader { leader: None }));
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
                assert_eq!(term(&mesh), Term(0));
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
                assert_eq!(term(&mesh), Term(0));
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
                let link = |index, outgoing: &[u8]| {
                    let at = Position {
                        term: Term(common::TERM.0 - 1),
                        index,
                    };
                    let voters = Voters {
                        incoming: [1, 2, 3].map(key).into(),
                        outgoing: outgoing.iter().map(|&id| key(id)).collect(),
                    };
                    let Data::Voters(change) = common::change(2, at, voters).data
                    else {
                        unreachable!("a change is a voters entry");
                    };
                    raft::Link { at, change }
                };
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
                assert_eq!(term(&mesh), Term(0));
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
                assert_eq!(term(&mesh), common::TERM);
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
                assert_eq!(term(&mesh), Term(0));
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

        // The heartbeat of leader 2 in the term after `later`, which 2 and 4
        // elected, with the chain of leader 3 of `later` that makes 4 a voter from
        // index 2.
        fn elected_through_chain() -> raft::Message {
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
            let mut elected = heartbeat(2, next, &[(2, 2), (4, 4)]);
            elected.chain = [(2, set(&[2, 3])), (3, set(&[]))].map(link).into();
            elected
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
                let unproven = Error::Raft(raft::Error::Unproven {
                    term: Term(later().0 + 1),
                    from: key(2),
                });
                let elected = elected_through_chain();
                assert_eq!(mesh.receive(public(2), elected), Err(unproven));
                assert_eq!(term(&mesh), common::TERM);
            });
        }

        // With the real join alone, the same heartbeat is proven.
        #[test]
        fn takes_a_leader_whose_chain_only_names_a_node_with_one_join() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[2, 3]).await.unwrap();
                write(&mesh, changes(&[join(4)])).await;
                let elected = elected_through_chain();
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
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let clock = node.clock();
            let opened = clock.now();
            lead(&mesh, &clock, home(1)).await;
            let waited = clock.now() - opened;
            // The timeout is 10 to 19 ticks, and `lead` proposes once per tick.
            let timeout = seconds(1)..=seconds(2);
            assert!(timeout.contains(&waited), "it led after {waited}");
        });
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
            let stopped = fail_sync(&node);
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
                ..config(&node, &tasks, 1, &[1], &[1])
            };
            let mesh = Mesh::start(config).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let held = fill(&pool);
            assert!(started(&mesh, home(2)).await.is_pending());
            node.clock().sleep(Span::from_nanos(TICK.nanos() * 3)).await;
            assert_eq!(mesh.group.borrow().state.home(INDEX), Some(key(1)));
            assert_eq!(mesh.group.borrow().running(), Ok(()));
            drop(held);
            assert_eq!(watch.next().await, Ok(Some(key(2))));
        });
    }

    #[test]
    fn a_burst_of_changes_that_the_pool_cannot_hold_commits() {
        solo(|node, tasks| async move {
            let config = Config {
                pool: small_pool(),
                ..config(&node, &tasks, 1, &[1], &[1])
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
                ..config(&node, &tasks, 1, &IDS, &IDS)
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
                ..config(&node, &tasks, 1, &IDS, &IDS)
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
                ..config(&node, &tasks, 1, &[1], &[1])
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
                ..config(&node, &tasks, 1, &[1], &[1])
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
                ..config(&node, &tasks, 1, &[1], &[1])
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
                ..config(&node, &tasks, 1, &IDS, &IDS)
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
                ..config(&node, &tasks, 1, &[1, 2, 3, 4], &IDS)
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
                ..config(&node, &tasks, 1, &IDS, &IDS)
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
                ..config(&node, &tasks, 1, &IDS, &IDS)
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
                ..config(&node, &tasks, 1, &IDS, &IDS)
            };
            let mesh = Mesh::start(config).await.unwrap();
            switch.refuse();
            let refused = block::Error::Refused { requested: 1 };
            assert_eq!(pool.alloc(1).err(), Some(refused));
            let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
            assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
            node.clock().sleep(Span::from_nanos(TICK.nanos() * 3)).await;
            assert!(quiet(&mesh, 2).await);
            assert_eq!(mesh.group.borrow().running(), Ok(()));
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
                ..config(&node, &tasks, 1, &[1], &[1])
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
            let stopped = fail_sync(&node);
            assert_eq!(mesh.propose(home(2)).await, Err(stopped.clone()));
            node.clock().sleep(TICK).await;
            assert_eq!(seen.take(), [Ok(Some(key(1))), Err(stopped)]);
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
            let stopped = fail_sync(&node);
            assert_eq!(mesh.propose(home(2)).await, Err(stopped.clone()));
            assert_eq!(watch.next().await, Err(stopped.clone()));
            let Error::Stopped(Stopped::Write(cause)) = &stopped else {
                unreachable!()
            };
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
                let stopped = fail_sync(&node);
                assert_eq!(mesh.propose(home(2)).await, Err(stopped.clone()));
                assert_eq!(watch.next().await, Err(stopped));
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
            let cause = Unknown::Kind { kind: 9 };
            let stopped = Error::Stopped(Stopped::Change { at, cause });
            assert_eq!(watch.next().await, Err(stopped.clone()));
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
            let cause = Unknown::Empty;
            let stopped = Error::Stopped(Stopped::Change { at, cause });
            assert_eq!(watch.next().await, Err(stopped.clone()));
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
            let config = Config {
                time: time.clone(),
                ..config(&node, &tasks, 1, &[1], &[1])
            };
            let mesh = Mesh::open(config).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let interval = time.now().mesh.unwrap();
            let names = ["clock.error", "clock.offset"];
            let late = mesh.stamp(request(4, 8, &names)).unwrap();
            let early = mesh.stamp(request(5, 9, &[])).unwrap();
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
            let config = Config {
                time: time.clone(),
                ..config(&node, &tasks, 1, &[1], &[1])
            };
            let mesh = Mesh::open(config).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let stamped = mesh.stamp(request(4, 8, &[])).unwrap();
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
                let config = Config {
                    time,
                    ..config(&node, &tasks, 1, &[1], &[1])
                };
                let mesh = Mesh::open(config).await.unwrap();
                let stamped = mesh.stamp(request(4, 8, &[]));
                assert_eq!(stamped, Err(Error::Unsynced), "{case}");
            });
        }
        let text = "this node has no mesh time with a known error at or after the Unix \
                    epoch, so it stamps no join";
        assert_eq!(Error::Unsynced.to_string(), text);
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "feeds the mesh clock of a test")]
    fn a_node_at_the_unix_epoch_stamps_a_join() {
        solo(|node, tasks| async move {
            let (mut clock, time) = clock::Clock::new(node.clock());
            let config = Config {
                time,
                ..config(&node, &tasks, 1, &[1], &[1])
            };
            let mesh = Mesh::open(config).await.unwrap();
            node.step_wall(Span::from_nanos(-node.wall().now().time.nanos()));
            node.set_wall_error(Some(Span::from_nanos(0)));
            let source = clock.add();
            let wall = clock::source::Wall::new(node.wall(), node.clock());
            clock.push(source, wall.measure());
            let stamped = mesh.stamp(request(4, 8, &[])).unwrap();
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
            let config = Config {
                time: time.clone(),
                ..config(&node, &tasks, 1, &[1], &[1])
            };
            let mesh = Mesh::open(config).await.unwrap();
            let source = clock.add();
            let wall = clock::source::Wall::new(node.wall(), node.clock());
            clock.push(source, wall.measure());
            clock.remove(source);
            assert!(matches!(time.status(), clock::Status::Holdover(..)));
            let stamped = mesh.stamp(request(4, 8, &[])).unwrap();
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
            let names: Vec<_> = (0..65).map(|i| format!("s{i:02}")).collect();
            let names: Vec<_> = names.iter().map(String::as_str).collect();
            let refused = mesh.stamp(request(4, 8, &names));
            let many = Error::Status(Many { count: 65 });
            assert_eq!(refused, Err(many.clone()));
            assert_eq!(many.to_string(), "65 status entries, more than 64");
            let names = &names[..64];
            mesh.stamp(request(4, 8, names)).unwrap();
        });
    }

    // The root region holds each name: a ticket and a join under any prefix.
    #[test]
    fn the_root_region_takes_a_join_of_a_node_with_any_name() {
        solo(|node, tasks| async move {
            let config = Config {
                region: Prefix::ROOT,
                ..config(&node, &tasks, 1, &[1], &[1])
            };
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
            assert_eq!(node.files().list(Path::new("")).await, Ok(Vec::new()));
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
            let stopped = fail_sync(&node);
            assert_eq!(mesh.propose(home(2)).await, Err(stopped.clone()));
            assert_eq!(watch.next().await, Err(stopped));
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
                    let mut config = config(&node, &tasks, 1, &[1, 2, 3], &[1]);
                    config.members.insert(at, second);
                    let opened = Mesh::start(config).await.err();
                    let duplicate =
                        Some(Error::Member(Unfit::Duplicate { key: key(2) }));
                    assert_eq!(opened, duplicate, "{case} at {at}");
                    assert_eq!(node.files().list(Path::new("")).await, Ok(Vec::new()));
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
                private_key: private(2),
                ..config(&node, &tasks, 1, &IDS, &[])
            };
            assert_eq!(Mesh::start(config).await.err(), Some(Error::WrongKey));
            assert_eq!(node.files().list(Path::new("")).await, Ok(Vec::new()));
            let text = "the private key of this node is not the key of its member";
            assert_eq!(Error::WrongKey.to_string(), text);
        });
    }

    #[test]
    fn open_gives_the_error_of_the_log() {
        solo(|node, tasks| async move {
            let _first = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let busy = open(&node, &tasks, 1, &[1], &[1]).await.err().unwrap();
            let path = Path::new(LOG).join("log-0");
            let cause = log::Error::Files(files::Error::Busy { path });
            assert_eq!(busy.to_string(), cause.to_string());
            assert_eq!(busy, Error::Log(cause));
        });
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

    #[test]
    fn a_dropped_watch_leaves_no_waker() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
            for _ in 0..3 {
                let mut watch = mesh.watch(INDEX);
                tasks.spawn(async move {
                    assert_eq!(watch.next().await, Ok(None));
                    let mut next = pin!(watch.next());
                    let waits =
                        poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx).is_pending()));
                    assert!(waits.await);
                });
            }
            let mut kept = mesh.watch(INDEX);
            assert_eq!(kept.next().await, Ok(None));
            let mut next = pin!(kept.next());
            assert!(
                poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx).is_pending())).await
            );
            node.clock().sleep(TICK).await;
            let slots: Vec<_> = mesh.group.borrow().watches.keys().copied().collect();
            assert_eq!(slots, [3]);
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
            let dropped = Error::Stopped(Stopped::Dropped);
            assert_eq!(
                given.take(),
                Some((Err(dropped.clone()), Err(dropped.clone())))
            );
            let text = "the group stopped: each mesh of the group dropped";
            assert_eq!(dropped.to_string(), text);
        });
    }

    #[test]
    fn a_watch_keeps_the_cause_of_a_stop_after_each_mesh_drops() {
        solo(|node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let mut watch = mesh.watch(INDEX);
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let stopped = fail_sync(&node);
            assert_eq!(mesh.propose(home(2)).await, Err(stopped.clone()));
            assert_eq!(watch.next().await, Err(stopped.clone()));
            drop(mesh);
            assert_eq!(watch.next().await, Err(stopped));
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
            let stopped = fail_sync(&node);
            assert_eq!(mesh.propose(home(2)).await, Err(stopped.clone()));
            assert_eq!(mesh.outgoing(key(2)).await, Err(stopped.clone()));
            drop(mesh);
            node.clock().sleep(TICK).await;
            assert_eq!(given.take(), Some(Err(stopped)));
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

    #[test]
    fn a_full_queue_drops_its_oldest_message() {
        let mut queue = Queue::default();
        let heartbeat = |commit| message(1, 2, Body::Heartbeat { commit });
        (0..=64)
            .map(heartbeat)
            .for_each(|message| queue.push(message));
        let expected: Vec<_> = (1..=64).map(heartbeat).collect();
        assert_eq!(Vec::from(queue.messages), expected);
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
            let config = Config {
                members: IDS.map(create_voter).into(),
                ..config_at(&node, &tasks, id, PORT, &IDS, &IDS)
            };
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
