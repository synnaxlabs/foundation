//! Drives the `raft` group of one region on one shard.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::poll_fn;
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
use crate::log::Log;
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
    /// The public key of each member of the region, this node included. A member's
    /// peer proves the key, and the key signs the member's grants.
    pub(crate) members: BTreeMap<node::Key, PublicKey>,
    /// The voters, when the log holds no configuration. Empty for a node that joins.
    pub(crate) voters: BTreeSet<node::Key>,
    /// The mesh's directory.
    pub(crate) files: Files,
    /// Times the ticks of the group.
    pub(crate) clock: Clock,
    /// Gives each election timeout its random part.
    pub(crate) entropy: Entropy,
    /// Runs the group's task.
    pub(crate) tasks: Tasks,
    /// Gives the blocks of the log's reads and writes.
    pub(crate) pool: Rc<Pool>,
}

/// One node's part in the group of a region. The group runs until it stops or the
/// mesh and its watches drop. It stays on the shard that opened it.
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
    /// - [`Error::Log`] when the log does not open.
    /// - [`Error::Raft`] when `raft` refuses the log.
    pub(crate) async fn open(config: Config) -> Result<Self, Error> {
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
            members: config.members,
            state: region::State::default(),
            queues: BTreeMap::new(),
            stopped: None,
            task: None,
            watches: Vec::new(),
        }));
        let signer = Signer::new(config.key, &config.private_key);
        let weak = Rc::downgrade(&group);
        config
            .tasks
            .spawn(run(weak, log, signer, config.clock, config.entropy));
        Ok(Self { group })
    }

    /// A watch of the home of `index`.
    pub(crate) fn watch(&self, index: channel::Key) -> Watch {
        Watch {
            group: Rc::clone(&self.group),
            index,
            given: None,
            called: false,
        }
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
    ///   voter of this node's configuration. A node with no configuration takes a
    ///   request from each member.
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
        if group.members.get(&from) != Some(&peer) {
            return Err(Error::Spoofed { from });
        }
        let Voters { incoming, outgoing } = group.raft.voters();
        let joins = incoming.is_empty() && outgoing.is_empty();
        let voter = incoming.contains(&from) || outgoing.contains(&from);
        if request(&message.body) && !voter && !joins {
            return Err(Error::NotVoter { from });
        }
        grant::check(&message, &group.members)?;
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
    group: Rc<RefCell<Group>>,
    index: channel::Key,
    // What the last call of `next` gave.
    given: Option<node::Key>,
    // Whether `next` gave a home.
    called: bool,
}

impl Watch {
    /// Waits until the home of the index is not what the last call gave, and returns
    /// it. The first call returns at once. `None` is an index with no home.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`], at once, when the group stopped.
    pub(crate) async fn next(&mut self) -> Result<Option<node::Key>, Error> {
        poll_fn(|cx| {
            let mut group = self.group.borrow_mut();
            group.running()?;
            let home = group.state.home(self.index);
            if self.called && self.given == home {
                register(&mut group.watches, cx.waker());
                return Poll::Pending;
            }
            (self.given, self.called) = (home, true);
            Poll::Ready(Ok(home))
        })
        .await
    }
}

struct Group {
    raft: Raft,
    members: BTreeMap<node::Key, PublicKey>,
    state: region::State,
    queues: BTreeMap<node::Key, Queue>,
    stopped: Option<Stopped>,
    // The task of `run`, while it waits for an input.
    task: Option<Waker>,
    // The tasks that wait in `Watch::next`.
    watches: Vec<Waker>,
}

impl Group {
    fn running(&self) -> Result<(), Error> {
        match &self.stopped {
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

    // The last steps of a `Ready`, after its write: queues its messages and applies
    // what it committed.
    fn settle(
        &mut self,
        messages: Vec<raft::Message>,
        committed: Vec<Entry>,
    ) -> Result<(), Stopped> {
        for message in messages {
            self.queues.entry(message.to).or_default().push(message);
        }
        for Entry { at, data } in committed {
            let bytes = match data {
                Data::Bytes(bytes) => bytes,
                Data::Empty | Data::Voters(_) => continue,
            };
            let change = Change::decode(&bytes)
                .map_err(|cause| Stopped::Change { at, cause })?;
            if self.state.apply(change).is_some() {
                self.watches.drain(..).for_each(Waker::wake);
            }
        }
        Ok(())
    }

    fn stop(&mut self, stopped: Stopped) {
        self.stopped = Some(stopped);
        let queues = self.queues.values_mut();
        let waiting = queues.filter_map(|queue| queue.waker.take());
        waiting.chain(self.watches.drain(..)).for_each(Waker::wake);
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

// Adds `waker` to `wakers` unless one there wakes the same task.
fn register(wakers: &mut Vec<Waker>, waker: &Waker) {
    if !wakers.iter().any(|w| w.will_wake(waker)) {
        wakers.push(waker.clone());
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
        let written = log.write(ready.hard.take(), &ready.entries).await;
        let Some(group) = group.upgrade() else { return };
        let mut group = group.borrow_mut();
        let Ready {
            messages,
            committed,
            ..
        } = ready;
        let settled = match written {
            Ok(()) => group.settle(messages, committed),
            Err(error) => Err(Stopped::Write(error)),
        };
        if let Err(stopped) = settled {
            group.stop(stopped);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::IoSliceMut;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::Path;
    use std::pin::pin;
    use std::sync::{Arc, Mutex};

    use env::files::{self, Operation};
    use env::net::udp::{self, Meta, Transmit};
    use raft::{Answer, Hard, Term};
    use sim::{Crash, Sim, link};

    use super::*;
    use crate::log;
    use crate::message::Message;
    use crate::region::Malformed;
    use crate::testing::{self, key, message, pool, private, proven, public};

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

    async fn open(
        node: &sim::node::Node,
        tasks: &Tasks,
        id: u8,
        members: &[u8],
        voters: &[u8],
    ) -> Result<Mesh, Error> {
        let config = Config {
            key: key(id),
            private_key: private(id),
            members: testing::members(members),
            voters: voters.iter().map(|&id| key(id)).collect(),
            files: node.files(),
            clock: node.clock(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
            pool: pool(),
        };
        Mesh::open(config).await
    }

    /// Sends each message for `to` as one datagram.
    async fn send(mesh: Rc<Mesh>, mut sender: udp::Sender, to: u8) -> ! {
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
    async fn receive(mesh: Rc<Mesh>, mut receiver: udp::Receiver) -> ! {
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
    async fn propose(
        mesh: Rc<Mesh>,
        clock: Clock,
        id: u8,
        board: Arc<Mutex<Board>>,
    ) -> ! {
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
        let mesh = Rc::new(open(&node, &tasks, id, &IDS, &IDS).await.unwrap());
        let config = udp::Config {
            local: address(id),
            send_buffer_bytes: 1 << 20,
            recv_buffer_bytes: 1 << 20,
        };
        let (sender, receiver) = node.net().udp(&config).unwrap();
        for to in IDS.into_iter().filter(|&to| to != id) {
            let (mesh, sender) = (Rc::clone(&mesh), sender.clone());
            tasks.spawn(async move {
                send(mesh, sender, to).await;
            });
        }
        let (receiving, proposing) = (Rc::clone(&mesh), Rc::clone(&mesh));
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

    /// Whether `mesh` has no message for node `to` now.
    async fn quiet(mesh: &Mesh, to: u8) -> bool {
        let mut outgoing = pin!(mesh.outgoing(key(to)));
        poll_fn(|cx| Poll::Ready(outgoing.as_mut().poll(cx).is_pending())).await
    }

    mod receive {
        use super::*;

        #[test]
        fn refuses_a_request_from_a_member_that_is_not_a_voter() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2, 3, 4], &IDS).await.unwrap();
                let last = Position::default();
                let entries = Vec::new();
                let requests = [
                    Body::PreVote { last },
                    Body::Vote { last },
                    Body::Heartbeat { commit: 0 },
                    Body::Append {
                        prev: last,
                        entries,
                        commit: 0,
                    },
                ];
                let refused = Error::NotVoter { from: key(4) };
                for body in requests {
                    let received = mesh.receive(public(4), message(4, 1, body, None));
                    assert_eq!(received, Err(refused.clone()));
                }
                assert_eq!(
                    refused.to_string(),
                    format!("node {} sent a request, but it is not a voter", key(4))
                );
                node.clock().sleep(TICK).await;
                assert!(quiet(&mesh, 4).await);
            });
        }

        #[test]
        fn takes_a_reply_from_a_member_that_is_not_a_voter() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1, 2, 3, 4], &IDS).await.unwrap();
                let answer = Answer::Refused;
                let replies = [
                    Body::PreVoteReply { answer },
                    Body::VoteReply { answer },
                    Body::HeartbeatReply,
                    Body::AppendReply { last: 0 },
                    Body::AppendReject { hint: 0 },
                ];
                for body in replies {
                    let received = mesh.receive(public(4), message(4, 1, body, None));
                    assert_eq!(received, Ok(()));
                }
            });
        }

        #[test]
        fn takes_a_request_from_a_member_when_the_node_has_no_voters() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &[]).await.unwrap();
                let heartbeat = proven(2, 1, Body::Heartbeat { commit: 0 });
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply, message(1, 2, Body::HeartbeatReply, None));
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
                let stranger = message(9, 1, Body::HeartbeatReply, None);
                let received = mesh.receive(public(9), stranger);
                assert_eq!(received, Err(Error::Spoofed { from: key(9) }));
                node.clock().sleep(TICK).await;
                assert!(quiet(&mesh, 2).await);
                assert_eq!(mesh.receive(public(2), heartbeat), Ok(()));
                let reply = mesh.outgoing(key(2)).await.unwrap();
                assert_eq!(reply, message(1, 2, Body::HeartbeatReply, None));
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
            });
        }

        #[test]
        fn gives_the_error_of_raft() {
            solo(|node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
                let misrouted = Error::Raft(raft::Error::Misrouted { to: key(3) });
                let received =
                    mesh.receive(public(2), message(2, 3, Body::HeartbeatReply, None));
                assert_eq!(received, Err(misrouted));
            });
        }
    }

    #[test]
    fn a_log_that_cannot_write_stops_the_group() {
        solo(|node, tasks| async move {
            let mesh = Rc::new(open(&node, &tasks, 1, &[1], &[1]).await.unwrap());
            let mut watch = mesh.watch(INDEX);
            assert_eq!(watch.next().await, Ok(None));
            lead(&mesh, &node.clock(), home(1)).await;
            assert_eq!(watch.next().await, Ok(Some(key(1))));
            let waiting = Rc::new(RefCell::new(None));
            let (other, slot) = (Rc::clone(&mesh), Rc::clone(&waiting));
            tasks.spawn(async move {
                let message = other.outgoing(key(2)).await;
                *slot.borrow_mut() = Some(message);
            });
            let path = Path::new(LOG).join("log-0");
            node.fail_file(&path, Operation::Sync);
            let at = Position {
                term: Term(1),
                index: 3,
            };
            assert_eq!(mesh.propose(home(2)), Ok(at));
            let cause = files::Error::Io {
                path,
                operation: Operation::Sync,
                code: 5,
            };
            let cause = log::Error::Files(cause);
            let stopped = Error::Stopped(Stopped::Write(cause.clone()));
            assert_eq!(watch.next().await, Err(stopped.clone()));
            assert_eq!(stopped.to_string(), format!("the group stopped: {cause}"));
            node.clock().sleep(TICK).await;
            assert_eq!(waiting.take(), Some(Err(stopped.clone())));
            assert_eq!(mesh.propose(home(2)), Err(stopped.clone()));
            let reply = message(2, 1, Body::HeartbeatReply, None);
            assert_eq!(mesh.receive(public(2), reply), Err(stopped));
        });
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
        let heartbeat = |commit| message(1, 2, Body::Heartbeat { commit }, None);
        (0..=64)
            .map(heartbeat)
            .for_each(|message| queue.push(message));
        let expected: Vec<_> = (1..=64).map(heartbeat).collect();
        assert_eq!(Vec::from(queue.messages), expected);
    }
}
