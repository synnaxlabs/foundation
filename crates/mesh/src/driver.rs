//! Drives the `raft` group of one region on one shard.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::poll_fn;
use std::mem;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Poll, Waker};

use block::Pool;
use env::clock::Clock;
use env::entropy::Entropy;
use env::files::Files;
use env::tasks::Tasks;
use raft::{Body, Data, Entry, Position, Raft, Ready, Start, Voters};
use types::channel;
use types::name::Name;
use types::node::{self, PrivateKey, PublicKey};
use types::time::{Span, Stamp};

use crate::error::{Error, Stopped};
use crate::grant::{self, Signer};
use crate::log::{self, Log};
use crate::member::Member;
use crate::message::Message;
use crate::region::{self, Change, Join, Malformed, Refused, Request};
use crate::status::Status;

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
    /// This node's private key. It signs the node's grants.
    pub(crate) private_key: PrivateKey,
    /// The prefix of the region's names.
    pub(crate) region: Name,
    /// Each member of the region, this node included, one record for each node. A
    /// member's peer proves the public key of its card, and that key signs the member's
    /// grants.
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
    /// Runs the group's task.
    pub(crate) tasks: Tasks,
    /// Gives the blocks of the log's reads and writes. A write that finds the pool
    /// full, or that the system refuses memory for, waits: the group sends and applies
    /// nothing until the pool gives the blocks.
    pub(crate) pool: Rc<Pool>,
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
    time: clock::Reader,
    entropy: Entropy,
}

impl Mesh {
    /// Reads the log from `config.files`, starts the group as a follower, and spawns
    /// its task on `config.tasks`. Homes are known again when this node applies the
    /// log, after it hears the leader.
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
        let (log, stored) = Log::open(config.files, LOG.into(), config.pool).await?;
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
    /// - [`Error::Spoofed`] when `peer` is not the key of the member that the message
    ///   names as its sender.
    /// - [`Error::NotVoter`] when the message is a request and its sender is not a
    ///   voter of this node's configuration.
    /// - [`Error::Grant`] when a grant in the message does not hold.
    /// - [`Error::Raft`] when `raft` refuses the message.
    ///
    /// # Panics
    ///
    /// When a grant in `message` has no signature. A decoded message gives each
    /// grant one.
    pub(crate) fn receive(
        &self,
        peer: PublicKey,
        message: raft::Message,
    ) -> Result<(), Error> {
        let mut group = self.group.borrow_mut();
        group.running()?;
        let from = message.from;
        let public_key = |key| group.state.member(key).map(Member::public_key);
        if public_key(from) != Some(peer) {
            return Err(Error::Spoofed { from });
        }
        let Voters { incoming, outgoing } = group.raft.voters();
        let voter = incoming.contains(&from) || outgoing.contains(&from);
        if request(&message.body) && !voter {
            return Err(Error::NotVoter { from });
        }
        grant::check(&message, public_key)?;
        group.raft.step(message)?;
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
    /// - [`Error::Raft`] with [`raft::Error::NotLeader`] when this node does not
    ///   lead, or when a new leader replaces the entry before a write holds it.
    pub(crate) async fn propose(&self, change: Change) -> Result<Position, Error> {
        let mut data = Vec::new();
        change.encode(&mut data);
        self.propose_data(data).await
    }

    // Proposes `data` as it is, which need not be a change.
    async fn propose_data(&self, data: Vec<u8>) -> Result<Position, Error> {
        let proposal = {
            let mut group = self.group.borrow_mut();
            group.running()?;
            let proposal = Rc::new(Proposal {
                at: group.raft.propose(data)?,
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
    /// - [`Error::PeerNotVoter`] when `peer` is the key of no voter of this node's
    ///   configuration. The group does not see the change.
    pub(crate) async fn answer(
        &self,
        peer: PublicKey,
        change: Change,
    ) -> Result<Message, Error> {
        {
            let group = self.group.borrow();
            group.running()?;
            let Voters { incoming, outgoing } = group.raft.voters();
            let holds = |voter: &node::Key| {
                group.state.member(*voter).map(Member::public_key) == Some(peer)
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
    /// - [`Error::Unsynced`] when this node has no mesh time, or when the later edge
    ///   is before the Unix epoch, where a UUIDv7 key has no time.
    /// - [`Error::Status`] when `request` names more than 64 status channels.
    pub(crate) fn stamp(&self, request: Request) -> Result<Change, Error> {
        let mesh = self.time.now().mesh.ok_or(Error::Unsynced)?;
        let at = mesh.latest;
        if at < Stamp::EPOCH {
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
    pub(crate) async fn outgoing(&self, to: node::Key) -> Result<raft::Message, Error> {
        poll_fn(|cx| {
            let mut group = self.group.borrow_mut();
            group.running()?;
            let queue = group.queues.entry(to).or_default();
            let Some(message) = queue.messages.pop_front() else {
                queue.waker = Some(cx.waker().clone());
                return Poll::Pending;
            };
            Poll::Ready(Ok(message))
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
}

impl Group {
    fn running(&self) -> Result<(), Error> {
        match self.stopped.get() {
            Some(stopped) => Err(Error::Stopped(stopped.clone())),
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

    // Queues each message for its member.
    fn send(&mut self, messages: Vec<raft::Message>) {
        for message in messages {
            self.queues.entry(message.to).or_default().push(message);
        }
    }

    // Applies each change in `committed`, and wakes the watches when a home moves.
    fn apply(&mut self, committed: Vec<Entry>) -> Result<(), Stopped> {
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
        let queues = self.queues.values_mut();
        let waiting = queues.filter_map(|queue| queue.waker.take());
        waiting.for_each(Waker::wake);
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
    // The task must end and free the log, and a watch that waits must learn that
    // each mesh dropped.
    fn drop(&mut self) {
        self.wake();
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
    // The task that waits in `Mesh::outgoing`.
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
            match log.write(ready.hard.clone(), &ready.entries).await {
                Err(log::Error::Pool(
                    block::Error::Exhausted { .. } | block::Error::Refused { .. },
                )) => {}
                written => break written,
            }
            (&mut tick).await;
            tick = clock.sleep(TICK);
            if group.strong_count() == 0 {
                return;
            }
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
    use std::io::IoSliceMut;
    use std::iter;
    use std::mem::ManuallyDrop;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::Path;
    use std::pin::pin;
    use std::sync::{Arc, Mutex};

    use block::testing::Scarce;
    use env::files::{self, Operation};
    use env::net::udp::{self, Meta, Transmit};
    use raft::{Answer, Grant, Hard, Proof, Term};
    use sim::{Crash, Sim, link};
    use transport::Address;
    use types::node::SealKey;

    use super::*;
    use crate::card;
    use crate::common::{self, create_pool, key, message, private, proven, public};
    use crate::region::{Unfit, Unknown};
    use crate::status::Many;
    use crate::ticket::Options;

    const IDS: [u8; 3] = [1, 2, 3];
    const PORT: u16 = 7000;
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
            pool: create_pool(),
        }
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

    async fn open(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        members: &[u8],
        voters: &[u8],
    ) -> Result<Mesh, Error> {
        Mesh::open(config(node, tasks, id, members, voters)).await
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

    /// Sends each message for `to` as one datagram.
    async fn send(mesh: Mesh, mut sender: udp::Sender, to: u8) -> ! {
        loop {
            let message = mesh.outgoing(key(to)).await.unwrap();
            let contents = Message::Raft(message).encode();
            let transmit = Transmit {
                destination: address(to),
                source: None,
                ecn: None,
                contents: &contents,
                segment: None,
            };
            poll_fn(|cx| sender.poll_send(cx, &transmit)).await.unwrap();
        }
    }

    /// Gives the mesh each datagram, with the key of the node at its source address.
    async fn receive(mesh: Mesh, mut receiver: udp::Receiver) -> ! {
        let mut bytes = vec![0; 1 << 16];
        loop {
            let mut meta = [Meta::default()];
            poll_fn(|cx| {
                let mut buffers = [IoSliceMut::new(&mut bytes)];
                receiver.poll_recv(cx, &mut buffers, &mut meta)
            })
            .await
            .unwrap();
            let [meta] = meta;
            let IpAddr::V4(source) = meta.source.ip() else {
                panic!("{} is not a node of the cluster", meta.source);
            };
            let peer = public(source.octets()[3]);
            for datagram in bytes[..meta.len].chunks(meta.stride.max(1)) {
                let Some(Message::Raft(message)) = Message::decode(datagram) else {
                    panic!("{datagram:?} is not a raft message");
                };
                mesh.receive(peer, message).unwrap();
            }
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

    /// Three voters, each on its own node, that send their messages as datagrams.
    struct Cluster {
        sim: Sim,
        nodes: Vec<sim::node::Node>,
        board: Arc<Mutex<Board>>,
    }

    /// `config` with an MTU that holds each raft message in one datagram, as the
    /// stream of a mesh does.
    fn wide(config: link::Config) -> link::Config {
        link::Config {
            mtu: 1 << 16,
            ..config
        }
    }

    impl Cluster {
        fn new(seed: u64) -> Self {
            let mut sim = Sim::new(sim::Config {
                seed,
                link: wide(link::Config::default()),
                ..sim::Config::default()
            });
            let node = |_| sim.node(sim::node::Config::default());
            Self {
                nodes: IDS.map(node).into(),
                sim,
                board: Arc::default(),
            }
        }

        /// Starts each voter. It runs until its node crashes.
        fn start(&self) {
            for (node, id) in self.nodes.iter().zip(IDS) {
                let (own, board) = (node.clone(), Arc::clone(&self.board));
                let config = env::shards::Config {
                    name: format!("voter-{id}"),
                    core: None,
                };
                let main = move |tasks| async move {
                    voter(own, tasks, id, board).await;
                };
                drop(node.shards().start(config, main).unwrap());
            }
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
            let node = |id| &self.nodes[IDS.iter().position(|&own| own == id).unwrap()];
            let config = wide(link::Config {
                loss,
                ..link::Config::default()
            });
            self.sim.link(node(a), node(b), config);
            self.sim.link(node(b), node(a), config);
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
        let mesh = open(&node, &tasks, id, &IDS, &IDS).await.unwrap();
        let config = udp::Config {
            local: address(id),
            send_buffer_bytes: 1 << 20,
            recv_buffer_bytes: 1 << 20,
        };
        let (sender, receiver) = node.net().udp(&config).unwrap();
        for to in IDS.into_iter().filter(|&to| to != id) {
            let (mesh, sender) = (mesh.clone(), sender.clone());
            tasks.spawn(async move {
                send(mesh, sender, to).await;
            });
        }
        let (receiving, proposing) = (mesh.clone(), mesh.clone());
        tasks.spawn(async move {
            receive(receiving, receiver).await;
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

    /// Gives the output of `future` when it does not wait.
    async fn now<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
        poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
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
            let mesh = Mesh::open(config).await.unwrap();
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

    // The second proposal comes while the write of the first one waits for a block.
    #[test]
    fn a_proposal_gives_the_cause_when_the_write_before_its_own_fails() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &[1], &[1])
            };
            let mesh = Mesh::open(config).await.unwrap();
            lead(&mesh, &node.clock(), home(1)).await;
            let held = fill(&pool);
            let results = Rc::new(RefCell::new(Vec::new()));
            for id in [2, 3] {
                let (other, results) = (mesh.clone(), Rc::clone(&results));
                tasks.spawn(async move {
                    let proposed = other.propose(home(id)).await;
                    results.borrow_mut().push(proposed);
                });
                node.clock().sleep(TICK).await;
            }
            let stopped = fail_sync(&node);
            drop(held);
            node.clock().sleep(Span::from_nanos(TICK.nanos() * 2)).await;
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
                    entries: vec![Entry {
                        at,
                        data: Data::Voters(joint),
                    }],
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

        /// The positions that the node gave for `home(3)`.
        fn positions(answers: &Answers) -> Vec<Position> {
            let position = |answer| match answer {
                Ok(Message::Proposed { at }) => Some(at),
                Ok(Message::NotLeader { leader: None }) => None,
                other => panic!("the node answered {other:?}"),
            };
            answers.take().into_iter().filter_map(position).collect()
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
        // block. After a power cut the node leads the same term again, and the
        // position that it would give for `home(3)` holds `home(1)`.
        #[test]
        fn a_position_in_an_answer_names_no_other_change_when_the_pool_is_full() {
            let mut sim = Sim::new(sim::Config::default());
            let node = sim.node(sim::node::Config::default());
            let answered = sim
                .run_on(&node, |node, tasks| async move {
                    let pool = small_pool();
                    let config = Config {
                        pool: Rc::clone(&pool),
                        ..config(&node, &tasks, 1, &[1], &[1])
                    };
                    let mesh = Mesh::open(config).await.unwrap();
                    // No write of the log ends from here on.
                    let _held = ManuallyDrop::new(fill(&pool));
                    let answers = Answers::default();
                    // Longer than each election timeout.
                    for _ in 0..30 {
                        node.clock().sleep(TICK).await;
                        forward(&mesh, &tasks, &answers);
                    }
                    node.clock().sleep(TICK).await;
                    let answered = answers.borrow().len();
                    assert!(answered < 30, "no change waits for the write");
                    positions(&answers)
                })
                .unwrap();
            sim.crash(&node, Crash::Power);
            let taken = lead_again(&mut sim, &node);
            assert!(
                !answered.contains(&taken),
                "the node answered that {taken:?} holds the home {}, and after the \
                 power cut that position holds the home {}; it gave {answered:?}",
                key(3),
                key(1),
            );
        }

        // The same with no fault but the power cut: the change comes while the node
        // writes the term that it now leads. A disk call takes 100 us at most, so the
        // voter gives the change each 10 us for 500 us before and after each tick.
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
                let mesh = Mesh::open(config).await.unwrap();
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
                let forged = Error::Grant(grant::Error::Forged { voter: key(3) });
                assert_eq!(mesh.receive(public(2), heartbeat), Err(forged.clone()));
                assert_eq!(
                    forged.to_string(),
                    format!("the grant of voter {} is forged", key(3))
                );
                node.clock().sleep(TICK).await;
                assert!(quiet(&mesh, 2).await);
                assert_eq!(term(&mesh), Term(0));
            });
        }

        #[test]
        fn refuses_a_grant_of_a_voter_that_is_not_a_member() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2], &[1, 2]).await.unwrap();
                let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                let refused = Error::Grant(grant::Error::NotMember { voter: key(3) });
                assert_eq!(mesh.receive(public(2), heartbeat), Err(refused));
            });
        }

        #[test]
        fn checks_the_peer_then_the_voter_then_the_grants() {
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
                let grant = Error::Grant(grant::Error::Forged { voter: key(1) });
                assert_eq!(mesh.receive(public(2), forged(2)), Err(grant));
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
                    entries: vec![Entry {
                        at,
                        data: Data::Voters(joint),
                    }],
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
            let mesh = Mesh::open(config).await.unwrap();
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
    fn a_full_pool_holds_a_message_until_a_block_is_free() {
        solo(|node, tasks| async move {
            let pool = small_pool();
            let config = Config {
                pool: Rc::clone(&pool),
                ..config(&node, &tasks, 1, &IDS, &IDS)
            };
            let mesh = Mesh::open(config).await.unwrap();
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
            let mesh = Mesh::open(config).await.unwrap();
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
            assert_eq!(Mesh::open(config).await.err(), Some(error));
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
            let mesh = Mesh::open(config).await.unwrap();
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
            let mesh = Mesh::open(config).await.unwrap();
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
        let given: Vec<_> = join.status.as_map().keys().map(Name::as_str).collect();
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
            assert_eq!(records.get(&2), Some(&common::member(2)), "node {id}");
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

    #[test]
    fn a_node_with_no_mesh_time_at_or_after_the_epoch_stamps_no_join() {
        for synced_before in [false, true] {
            solo(move |node, tasks| async move {
                let time = if synced_before {
                    node.step_wall(Span::from_nanos(-2 * NOW.nanos()));
                    synced(&node)
                } else {
                    clock::Clock::new(node.clock()).1
                };
                let config = Config {
                    time,
                    ..config(&node, &tasks, 1, &[1], &[1])
                };
                let mesh = Mesh::open(config).await.unwrap();
                let stamped = mesh.stamp(request(4, 8, &[]));
                assert_eq!(stamped, Err(Error::Unsynced), "{synced_before}");
            });
        }
        let text = "this node has no mesh time at or after the Unix epoch, so it stamps no join";
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
                    let opened = Mesh::open(config).await.err();
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
            assert_eq!(Mesh::open(config).await.err(), Some(Error::WrongKey));
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
            let data = Data::Voters(Voters::default());
            let entries = [Entry { at, data }];
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
}
