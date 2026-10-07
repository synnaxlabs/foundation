//! Drives the `raft` group of one region on one shard.

use std::cell::{OnceCell, RefCell};
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
use types::node::{self, PrivateKey, PublicKey};
use types::time::Span;

use crate::error::{Error, Stopped};
use crate::grant::{self, Signer};
use crate::log::{self, Log};
use crate::member::Member;
use crate::region::{self, Change};

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
    /// Each member of the region, this node included. A member's peer proves the
    /// public key of its card, and that key signs the member's grants. Each card must
    /// be signed for its key here: `open` does not check it (#1259).
    pub(crate) members: BTreeMap<node::Key, Member>,
    /// The voters before the first entry of the log, the same at each open. Each is a
    /// member. A node that joins gives the founding voters from its join answer. A node
    /// with no voter takes no request.
    pub(crate) voters: BTreeSet<node::Key>,
    /// The mesh's directory.
    pub(crate) files: Files,
    /// Times the ticks of the group.
    pub(crate) clock: Clock,
    /// Gives each election timeout its random part.
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
}

impl Mesh {
    /// Reads the log from `config.files`, starts the group as a follower, and spawns
    /// its task on `config.tasks`. Homes are known again when this node applies the
    /// log, after it hears the leader.
    ///
    /// # Errors
    ///
    /// - [`Error::NotMember`] when `config.members` lacks this node or a voter.
    /// - [`Error::WrongKey`] when `config.private_key` is not the key of this node in
    ///   `config.members`.
    /// - [`Error::Log`] when the log does not open.
    /// - [`Error::Raft`] when `raft` refuses the log.
    pub(crate) async fn open(config: Config) -> Result<Self, Error> {
        let signer = Signer::new(config.key, &config.private_key);
        match config.members.get(&config.key) {
            None => return Err(Error::NotMember(config.key)),
            Some(own) if !signer.owns(own.public_key()) => {
                return Err(Error::WrongKey);
            }
            Some(_) => {}
        }
        let mut voters = config.voters.iter();
        if let Some(&key) = voters.find(|key| !config.members.contains_key(key)) {
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
            state: region::State::new(config.members),
            queues: BTreeMap::new(),
            stopped: Rc::default(),
            task: None,
            watches: BTreeMap::new(),
            watched: 0,
        }));
        let weak = Rc::downgrade(&group);
        config
            .tasks
            .spawn(run(weak, log, signer, config.clock, config.entropy));
        Ok(Self { group })
    }

    /// A watch of the home of `index`.
    pub(crate) fn watch(&self, index: channel::Key) -> Watch {
        let mut group = self.group.borrow_mut();
        let slot = group.watched;
        group.watched = slot.wrapping_add(1);
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

    /// Proposes `change` on this node. The change is in force once a quorum holds
    /// it, and a new leader can replace it before then.
    ///
    /// # Errors
    ///
    /// - [`Error::Stopped`] when the group stopped.
    /// - [`Error::Raft`] with [`raft::Error::NotLeader`] when this node does not
    ///   lead.
    pub(crate) fn propose(&self, change: Change) -> Result<Position, Error> {
        let mut group = self.group.borrow_mut();
        group.running()?;
        let mut data = Vec::new();
        change.encode(&mut data);
        let at = group.raft.propose(data)?;
        group.wake();
        Ok(at)
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
    // The count of watches made, which is the slot of the next one.
    watched: u64,
}

impl Group {
    fn running(&self) -> Result<(), Error> {
        match self.stopped.get() {
            Some(stopped) => Err(Error::Stopped(stopped.clone())),
            None => Ok(()),
        }
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
            let change = Change::decode(&bytes)
                .map_err(|cause| Stopped::Change { at, cause })?;
            if self.state.apply(change).is_some() {
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
            Poll::Ready(Some(ready))
        });
        let Some(mut ready) = next.await else { return };
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
            messages,
            committed,
            ..
        } = ready;
        let applied = written.map_err(Stopped::Write).and_then(|()| {
            group.send(messages);
            group.apply(committed)
        });
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
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::Path;
    use std::pin::pin;
    use std::sync::{Arc, Mutex};

    use block::testing::Scarce;
    use env::files::{self, Operation};
    use env::net::udp::{self, Meta, Transmit};
    use raft::{Answer, Hard, Term};
    use sim::{Crash, Sim, link};

    use super::*;
    use crate::card;
    use crate::common::{self, key, message, pool, private, proven, public};
    use crate::message::Message;
    use crate::region::Malformed;

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
        /// The change that each node proposes until the group takes it.
        script: BTreeMap<u8, Change>,
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
            members: common::members(members),
            voters: voters.iter().map(|&id| key(id)).collect(),
            files: node.files(),
            clock: node.clock(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
            pool: pool(),
        }
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

    /// Proposes the change of node `id` in the script, once per tick, until the
    /// group takes it.
    async fn propose(mesh: Mesh, clock: Clock, id: u8, board: Arc<Mutex<Board>>) -> ! {
        loop {
            clock.sleep(TICK).await;
            let mut board = board.lock().unwrap();
            let Some(&change) = board.script.get(&id) else {
                continue;
            };
            match mesh.propose(change) {
                Ok(_) => {
                    board.script.remove(&id);
                    board.led.push(id);
                }
                Err(Error::Raft(raft::Error::NotLeader { .. })) => {}
                Err(error) => panic!("node {id} cannot propose: {error}"),
            }
        }
    }

    /// Three voters, each on its own node, that send their messages as datagrams.
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
            let script = IDS.map(|id| (id, change(id))).into();
            self.board.lock().unwrap().script = script;
        }

        fn run(&mut self, span: Span) {
            self.sim.run_for(span).unwrap();
        }

        /// Sets the chance that a datagram between `a` and `b` is lost, each way.
        fn link(&mut self, a: u8, b: u8, loss: f64) {
            let node = |id| &self.nodes[IDS.iter().position(|&own| own == id).unwrap()];
            let config = link::Config {
                loss,
                ..link::Config::default()
            };
            self.sim.link(node(a), node(b), config);
            self.sim.link(node(b), node(a), config);
        }

        /// Takes what the voters did so far.
        fn take(&self) -> (Vec<u8>, Homes) {
            let board = std::mem::take(&mut *self.board.lock().unwrap());
            (board.led, board.homes)
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
        let mut watch = mesh.watch(INDEX);
        loop {
            let home = watch.next().await.unwrap();
            board
                .lock()
                .unwrap()
                .homes
                .entry(id)
                .or_default()
                .push(home);
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
            match mesh.propose(change) {
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

    /// Whether `mesh` has no message for node `to` now.
    async fn quiet(mesh: &Mesh, to: u8) -> bool {
        let mut outgoing = pin!(mesh.outgoing(key(to)));
        poll_fn(|cx| Poll::Ready(outgoing.as_mut().poll(cx).is_pending())).await
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
            assert_eq!(watch.next().await, Ok(None));
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            // The group now waits for a tick.
            clock.sleep(TICK).await;
            clock.sleep(Span::MILLISECOND).await;
            let proposed = clock.now();
            mesh.propose(home(2)).unwrap();
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
            assert_eq!(watch.next().await, Ok(None));
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let held = fill(&pool);
            mesh.propose(home(2)).unwrap();
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
            mesh.propose(home(2)).unwrap();
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
            mesh.propose(home(2)).unwrap();
            node.clock().sleep(TICK).await;
            assert_eq!(seen.take(), [Ok(None), Ok(Some(key(1))), Err(stopped)]);
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
            let at = Position {
                term: Term(1),
                index: 3,
            };
            assert_eq!(mesh.propose(home(2)), Ok(at));
            assert_eq!(watch.next().await, Err(stopped.clone()));
            let Error::Stopped(Stopped::Write(cause)) = &stopped else {
                unreachable!()
            };
            assert_eq!(stopped.to_string(), format!("the group stopped: {cause}"));
            node.clock().sleep(TICK).await;
            assert_eq!(waiting.take(), Some(Err(stopped.clone())));
            assert_eq!(mesh.propose(home(2)), Err(stopped.clone()));
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
                assert_eq!(watch.next().await, Ok(None));
                assert_eq!(watch.next().await, Ok(Some(key(1))));
                let stopped = fail_sync(&node);
                mesh.propose(home(2)).unwrap();
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
                        Log::open(files, LOG.into(), pool()).await.unwrap();
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
            let end = changes.last().copied().flatten();
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
            let cause = Malformed::Kind { kind: 9 };
            let stopped = Error::Stopped(Stopped::Change { at, cause });
            assert_eq!(watch.next().await, Err(stopped.clone()));
            let text = "the group stopped: the committed entry at index 1 of term 5 is \
                        not a change: change kind 9 is unknown";
            assert_eq!(stopped.to_string(), text);
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
            assert_eq!(watch.next().await, Ok(None));
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let stopped = fail_sync(&node);
            mesh.propose(home(2)).unwrap();
            assert_eq!(watch.next().await, Err(stopped));
            assert_eq!(mesh.member(key(1)), Some(common::member(1)));
            assert_eq!(mesh.member(key(2)), None);
        });
    }

    // `open` does not check that a card is signed for its key in `members` (#1259).
    #[test]
    fn open_takes_a_card_that_is_signed_for_another_key() {
        solo(|node, tasks| async move {
            let mut config = config(&node, &tasks, 1, &[1, 2], &[1]);
            config.members.insert(key(2), common::member(3));
            let mesh = Mesh::open(config).await.unwrap();
            let given = mesh.member(key(2)).unwrap();
            assert_eq!(given, common::member(3));
            let card = given.card.card().clone();
            assert_eq!(
                card::Signed::check(key(2), card, *given.card.signature()).err(),
                Some(card::Forged { node: key(2) }),
            );
        });
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
            assert_eq!(watch.next().await, Ok(None));
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let stopped = fail_sync(&node);
            mesh.propose(home(2)).unwrap();
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
            mesh.propose(home(2)).unwrap();
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
            let (_, stored) =
                Log::open(node.files(), LOG.into(), pool()).await.unwrap();
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
