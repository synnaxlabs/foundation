//! A simulated mesh for the scenarios: nodes from `node` on `sim` seams, links that
//! can be cut, simulated devices and stores, and the operator's front ends. Each
//! method with a `todo!` waits on the issue it names.

mod influx;

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use connector_influx::sim::Store;
use env::net::udp;
use mesh::card::addresses::Addresses;
use mesh::card::{self, Card};
use mesh::region::Founding;
use mesh::status::Status;
use transport::Address;
use types::ed25519::PrivateKey;
use types::name::Prefix;
use types::node::SealKey;
use types::time::Span;

/// The port each node binds, on its host's first address.
const PORT: u16 = 7000;

/// The private key of the subject `admin`, the first admin of each mesh.
const ADMIN: PrivateKey = PrivateKey([0; 32]);

/// The address of the port of the node on `host`.
fn listen(host: &sim::node::Node) -> SocketAddr {
    SocketAddr::new(host.addresses()[0], PORT)
}

/// A whole mesh on one deterministic simulation.
#[derive(Debug)]
pub(crate) struct Lab {
    sim: sim::Sim,
    /// The link that [`Lab::heal`] puts back.
    link: sim::link::Config,
    members: Vec<Member>,
    /// The simulated InfluxDB stores, by address.
    stores: BTreeMap<String, Influx>,
    /// The samples written, by channel.
    written: BTreeMap<String, Written>,
    /// The simulation ran, or holds a task of a test, so a [`Lab::start`] would run
    /// it.
    ran: bool,
}

/// A simulated InfluxDB store and the names of the connector that writes to it.
#[derive(Debug, Default)]
struct Influx {
    store: Store,
    /// The connector name, the `connector` tag of its gap lines.
    connector: String,
    /// The data measurement of each channel, by channel.
    measurements: BTreeMap<String, String>,
}

/// The samples written to one channel on the live path. Sample `k` has the seq
/// `seqs.start + k` and the value `k as f64`.
#[derive(Debug, Clone)]
pub(crate) struct Written {
    /// The seqs that the home gave the samples.
    pub seqs: Range<u64>,
    /// The name of the channel's index, the `index` tag of its gap lines.
    pub index: String,
}

#[derive(Debug)]
struct Member {
    name: String,
    key: types::node::Key,
    private_key: PrivateKey,
    host: sim::node::Node,
    /// The region that [`Lab::mesh`] gave the node.
    region: Option<Founding>,
    /// The node, from the first [`Lab::run`] on.
    node: Option<node::Node>,
}

/// One node in a [`Lab`]: its index in `members`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Node(usize);

/// A live reader on one channel, opened with [`Lab::reader`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Reader;

/// A join ticket. It is a secret, never written to a file.
#[derive(Debug)]
pub(crate) struct Ticket;

/// The protocol of a simulated device.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Protocol {
    OpcUa,
    ModbusTcp,
    ModbusRtu,
    /// NI, through the stub `libnidaqmx.so`.
    Ni,
}

/// What a reader received on one channel, folded as it arrives, so a long run at
/// full rate needs no memory per sample.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Received {
    pub samples: u64,
    /// The seqs of the first and last samples.
    pub seqs: Option<Range<u64>>,
    /// Each sample's seq is one more than the one before it, or than the end of the
    /// gap before it.
    pub contiguous: bool,
    pub gaps: Vec<Gap>,
}

/// An explicit gap: samples the buffer no longer had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gap {
    /// Samples received before the gap.
    pub after: u64,
    pub count: u64,
}

/// One sample of a channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Sample {
    pub ns: i64,
    pub value: f64,
}

/// A command and the acknowledgment the connector wrote for it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Command {
    pub subject: String,
    pub value: f64,
    pub ack: Option<f64>,
}

impl Lab {
    /// Builds an empty mesh whose run replays from `key`.
    pub(crate) fn new(key: u64) -> Self {
        let config = sim::Config {
            seed: key,
            ..sim::Config::default()
        };
        Self {
            sim: sim::Sim::new(config),
            link: config.link,
            members: Vec::new(),
            stores: BTreeMap::new(),
            written: BTreeMap::new(),
            ran: false,
        }
    }

    /// Adds a node named `name` on a new simulated host, and writes its key there. It
    /// starts at the next [`Lab::run`].
    ///
    /// # Panics
    ///
    /// After the first [`Lab::run`] or a task of a test, as the write of the key runs
    /// the simulation to its end: a node or a task then runs before its time, or never
    /// ends.
    pub(crate) fn start(&mut self, name: &str) -> Node {
        assert!(
            !self.ran,
            "lab failure: `start` after the first run or a task"
        );
        let byte = u8::try_from(self.members.len() + 1)
            .expect("lab failure: at most 255 nodes");
        let host = self.sim.node(sim::node::Config::default());
        let (key, private_key) = (
            types::node::Key::from_u128(u128::from(byte)),
            PrivateKey([byte; 32]),
        );
        let created = self.sim.run_on(&host, {
            let private_key = private_key.clone();
            move |host, _| async move {
                node::create_key(&host.files(), key, private_key).await
            }
        });
        created
            .expect("lab failure: the run ends")
            .expect("lab failure: the key of a new host");
        self.members.push(Member {
            name: name.into(),
            key,
            private_key,
            host,
            region: None,
            node: None,
        });
        Node(self.members.len() - 1)
    }

    /// Starts each node that has not started. `run` calls it, and so does each method
    /// that needs a running node (#585 PR 4c).
    fn boot(&mut self) {
        let members = self.members.iter_mut();
        for member in members.filter(|member| member.node.is_none()) {
            let host = &member.host;
            let node = node::Node::start(node::Config {
                shards: host.shards(),
                clock: host.clock(),
                wall: host.wall(),
                budget: types::byte::Size::MEBIBYTE,
                memory: Box::new(|len| Ok(block::Heap::new(len))),
                files: {
                    let host = host.clone();
                    Box::new(move || {
                        let host = host.clone();
                        Box::new(move || host.files())
                    })
                },
                entropy: host.entropy(),
                disk: types::byte::Size::GIBIBYTE,
                net: host.net(),
                listen: listen(host),
                region: member.region.clone(),
            });
            member.node = Some(node);
        }
    }

    /// Sets the disk budget of `node` to `bytes`.
    pub(crate) fn limit(&mut self, _node: Node, _bytes: u64) {
        todo!("waits on #337")
    }

    /// The disk budget that holds `span` of one `f64` channel at `rate` samples per
    /// second, as `buffer` stores it.
    pub(crate) fn budget(&self, _rate: u64, _span: Duration) -> u64 {
        todo!("waits on #1256")
    }

    /// Makes `nodes` the members of one mesh, without a ticket: they found the root
    /// region, each as a voter, with the first admin `admin`, at the first
    /// [`Lab::run`].
    ///
    /// # Panics
    ///
    /// When `nodes` is empty, or when a node runs already, is in another mesh, or is
    /// listed twice.
    pub(crate) fn mesh(&mut self, nodes: &[Node]) {
        assert!(!nodes.is_empty(), "lab failure: `mesh` of no node");
        for (at, &node) in nodes.iter().enumerate() {
            let member = &self.members[node.0];
            let name = &member.name;
            assert!(
                member.node.is_none(),
                "lab failure: `mesh` of {name}, which runs already"
            );
            assert!(
                member.region.is_none(),
                "lab failure: {name} is in two meshes"
            );
            assert!(
                !nodes[..at].iter().any(|other| other.0 == node.0),
                "lab failure: {name} is listed twice"
            );
        }
        let members: Vec<mesh::Member> = nodes
            .iter()
            .map(|&node| {
                let member = &self.members[node.0];
                let mut seal = [member.private_key.0[0]; 32];
                seal[31] = 0;
                let card = Card {
                    name: member
                        .name
                        .parse()
                        .expect("lab failure: a node name is a card name"),
                    public_key: member.private_key.public(),
                    seal_key: SealKey::new(seal).unwrap(),
                    addresses: Addresses::new(vec![Address::Udp(listen(&member.host))])
                        .unwrap(),
                    version: 1,
                };
                mesh::Member {
                    card: card::Signed::sign(member.key, card, &member.private_key),
                    admission: [0; 64],
                    ephemeral: None,
                    status: Status::new(BTreeMap::new()).unwrap(),
                }
            })
            .collect();
        let founding = Founding {
            prefix: Prefix::ROOT,
            voters: members.iter().map(|member| member.card.key()).collect(),
            members,
            definitions: spec::founding::create(ADMIN.public()),
            homes: BTreeMap::new(),
        };
        for &node in nodes {
            self.members[node.0].region = Some(founding.clone());
        }
    }

    /// Creates the `f64` channel `channel`, whose home is `home`.
    pub(crate) fn channel(&mut self, _home: Node, _channel: &str) {
        todo!("waits on #462, #585")
    }

    /// Opens a live reader on `channel` at `node`. It gets the samples written from
    /// now on.
    pub(crate) fn reader(&mut self, _node: Node, _channel: &str) -> Reader {
        todo!("waits on #340, #462, #585, #2018, #2023")
    }

    /// Writes `values` to `channel` on `node`, one each millisecond, as the
    /// simulation runs.
    pub(crate) fn send(&mut self, _node: Node, _channel: &str, _values: &[f64]) {
        todo!("waits on #462, #585")
    }

    /// Every sample that `reader` got, in the order it got them.
    pub(crate) fn received(&self, _reader: Reader) -> Vec<Sample> {
        todo!("waits on #340, #462, #585, #2018, #2023")
    }

    /// Creates a single-use join ticket on `admin`.
    pub(crate) fn ticket(&mut self, _admin: Node) -> Ticket {
        todo!("waits on #336")
    }

    /// Joins `node` to the region of the ticket's issuer.
    pub(crate) fn join(&mut self, _node: Node, _ticket: Ticket) {
        todo!("waits on #336")
    }

    /// The members that `node` sees, by name, sorted.
    pub(crate) fn members(&self, _node: Node) -> Vec<String> {
        todo!("waits on #336")
    }

    /// The spec hash that `node` holds.
    pub(crate) fn spec(&self, _node: Node) -> [u8; 32] {
        todo!("waits on #336")
    }

    /// Runs `plan` of `hcl` on `node` through the JSON CLI and returns the names of
    /// the changed definitions.
    pub(crate) fn plan(&mut self, _node: Node, _hcl: &str) -> Vec<String> {
        todo!("waits on #337")
    }

    /// Runs `plan` then `apply` of `hcl` on `node` through the JSON CLI.
    pub(crate) fn apply(&mut self, _node: Node, _hcl: &str) {
        todo!("waits on #337")
    }

    /// Runs the MCP `plan` tool on `node` and returns the plan and the names of the
    /// changed definitions.
    pub(crate) fn mcp_plan(
        &mut self,
        _node: Node,
        _hcl: &str,
    ) -> (String, Vec<String>) {
        todo!("waits on #337")
    }

    /// Runs the MCP `apply` tool on `node` with a plan from [`Lab::mcp_plan`].
    pub(crate) fn mcp_apply(&mut self, _node: Node, _plan: &str) {
        todo!("waits on #337")
    }

    /// Attaches a simulated device to `node` at `address`.
    pub(crate) fn device(&mut self, _node: Node, _protocol: Protocol, _address: &str) {
        todo!("waits on #432, #434, #435, #436")
    }

    /// Attaches a simulated Influx store to `node` at `address`.
    pub(crate) fn influx(&mut self, _node: Node, _address: &str) {
        todo!("waits on #341")
    }

    /// The value of `point` on the device at `address`.
    pub(crate) fn point(&self, _address: &str, _point: &str) -> f64 {
        todo!("waits on #432, #434, #435, #436")
    }

    /// Sets the value of `point` on the device at `address`.
    pub(crate) fn set_point(&mut self, _address: &str, _point: &str, _value: f64) {
        todo!("waits on #432, #434, #435, #436")
    }

    /// Writes `count` samples to `channel` on `node` at `rate` samples per second,
    /// as the simulation runs. Sample `k`, from 0, has the value `k as f64`.
    pub(crate) fn write(
        &mut self,
        _node: Node,
        _channel: &str,
        _rate: u64,
        _count: u64,
    ) {
        todo!("waits on #340")
    }

    /// The seqs that the home gave the samples written to `channel`.
    pub(crate) fn written(&self, _channel: &str) -> Range<u64> {
        todo!("waits on #340")
    }

    /// The true simulated time of each sample written to `channel`, in order.
    pub(crate) fn truth(&self, _channel: &str) -> Vec<i64> {
        todo!("waits on #340")
    }

    /// Sends `value` to the command channel `channel` as `subject`.
    pub(crate) fn command(
        &mut self,
        _node: Node,
        _subject: &str,
        _channel: &str,
        _value: f64,
    ) -> Result<(), String> {
        todo!("waits on #340")
    }

    /// Reads `channel` on `node` from the oldest sample, as `subject`.
    pub(crate) fn read(
        &mut self,
        _node: Node,
        _subject: &str,
        _channel: &str,
    ) -> Received {
        todo!("waits on #340")
    }

    /// Reads every sample of `channel` on `node`, as `subject`. For short runs only.
    pub(crate) fn samples(
        &mut self,
        _node: Node,
        _subject: &str,
        _channel: &str,
    ) -> Vec<Sample> {
        todo!("waits on #462, #585")
    }

    /// The commands recorded on `channel`, with their acknowledgments.
    pub(crate) fn audit(&mut self, _node: Node, _channel: &str) -> Vec<Command> {
        todo!("waits on #340")
    }

    /// What the Influx store at `address` holds for `channel`, folded by
    /// [`influx::stored`].
    ///
    /// # Panics
    ///
    /// When no store is at `address`, no sample was written to the channel, the
    /// store's connector writes no measurement for it, or [`influx::stored`] panics.
    pub(crate) fn stored(&self, address: &str, channel: &str) -> Received {
        let influx = self
            .stores
            .get(address)
            .unwrap_or_else(|| panic!("lab failure: no Influx store at {address}"));
        let written = self.written.get(channel).unwrap_or_else(|| {
            panic!("lab failure: no sample was written to {channel}")
        });
        let measurement = influx.measurements.get(channel).unwrap_or_else(|| {
            panic!("lab failure: the connector at {address} writes no {channel}")
        });
        influx::stored(&influx.store, &influx.connector, measurement, written)
    }

    /// Cuts every link between `a` and `b`. Datagrams in flight still arrive.
    pub(crate) fn cut(&mut self, a: Node, b: Node) {
        let config = sim::link::Config {
            loss: 1.0,
            ..sim::link::Config::default()
        };
        self.link(a, b, config);
    }

    /// Restores every link between `a` and `b`.
    pub(crate) fn heal(&mut self, a: Node, b: Node) {
        self.link(a, b, self.link);
    }

    /// Sets every link between `a` and `b` to `config`.
    pub(crate) fn link(&mut self, a: Node, b: Node, config: sim::link::Config) {
        let (a, b) = (&self.members[a.0].host, &self.members[b.0].host);
        self.sim.link(a, b, config);
        self.sim.link(b, a, config);
    }

    /// Starts each node that has not started, then runs the simulation for `span` of
    /// simulated time.
    ///
    /// # Panics
    ///
    /// When a task panics or the run takes too many steps.
    pub(crate) fn run(&mut self, span: Duration) {
        self.ran = true;
        self.boot();
        let nanos = i64::try_from(span.as_nanos()).expect("span fits in a Span");
        if let Err(e) = self.sim.run_for(Span::from_nanos(nanos)) {
            panic!("{e}");
        }
    }

    /// A hash of every scheduling choice and datagram so far. One key gives one
    /// digest.
    pub(crate) fn digest(&self) -> u64 {
        self.sim.digest()
    }

    /// Stops every node that started and runs the simulation until each has ended.
    ///
    /// # Panics
    ///
    /// When the run fails, or a node ends with an error.
    pub(crate) fn stop(mut self) {
        for node in self
            .members
            .iter()
            .filter_map(|member| member.node.as_ref())
        {
            node.stop();
        }
        if let Err(e) = self.sim.run() {
            panic!("{e}");
        }
        for member in self.members {
            if let Some(Err(e)) = member.node.map(node::Node::join) {
                panic!("{}: {e}", member.name);
            }
        }
    }
}

/// The names that `dir` of `node`'s data directory holds at 1 s, or the error of the
/// list. Call it after each [`Lab::start`].
fn listed(lab: &mut Lab, node: Node, dir: &'static str) -> Arc<Mutex<Option<Listed>>> {
    lab.ran = true;
    let host = lab.members[node.0].host.clone();
    let out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&out);
    let config = env::shards::Config {
        name: "listed".into(),
        core: None,
    };
    let shards = host.shards();
    let start = shards.start(config, move |_| async move {
        host.clock().sleep(Span::SECOND).await;
        *slot.lock().unwrap() = Some(host.files().list(Path::new(dir)).await);
    });
    drop(start.unwrap());
    out
}

type Listed = Result<Vec<PathBuf>, env::files::Error>;

#[test]
fn a_mesh_founds_one_region_of_its_nodes_at_the_first_run() {
    let mut lab = Lab::new(1);
    let (a, b, c) = (lab.start("a"), lab.start("b"), lab.start("c"));
    lab.mesh(&[a, b]);
    let meshes = [a, b, c].map(|node| listed(&mut lab, node, "mesh"));
    lab.run(Duration::from_secs(2));
    // A node gives no read of its region, so the test reads what `mesh` stored.
    let founding = lab.members[a.0].region.clone().unwrap();
    assert_eq!(lab.members[b.0].region, Some(founding.clone()));
    assert_eq!(lab.members[c.0].region, None);
    let cards: Vec<_> = founding
        .members
        .iter()
        .map(|member| {
            let card = member.card.card();
            (
                member.card.key(),
                card.name.to_string(),
                card.addresses.clone(),
            )
        })
        .collect();
    let address = |node: Node| {
        Addresses::new(vec![Address::Udp(listen(&lab.members[node.0].host))]).unwrap()
    };
    let key = |n| types::node::Key::from_u128(n);
    assert_eq!(
        cards,
        [
            (key(1), "a".into(), address(a)),
            (key(2), "b".into(), address(b)),
        ]
    );
    assert_eq!(founding.voters, [key(1), key(2)].into());
    assert_eq!(founding.prefix, Prefix::ROOT);
    assert_eq!(founding.definitions, spec::founding::create(ADMIN.public()));
    let meshes = meshes.map(|mesh| mesh.lock().unwrap().take().unwrap().map(drop));
    let none = Err(env::files::Error::NotFound {
        path: PathBuf::from("mesh"),
    });
    assert_eq!(meshes, [Ok(()), Ok(()), none]);
    lab.stop();
}

#[test]
#[should_panic(expected = "lab failure: `mesh` of a, which runs already")]
fn a_mesh_after_the_first_run_panics() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    lab.run(Duration::from_millis(1));
    lab.mesh(&[a, b]);
}

#[test]
#[should_panic(expected = "lab failure: `start` after the first run or a task")]
fn a_start_after_the_first_run_panics() {
    let mut lab = Lab::new(1);
    lab.start("a");
    lab.run(Duration::from_millis(1));
    lab.start("b");
}

#[test]
#[should_panic(expected = "lab failure: `start` after the first run or a task")]
fn a_start_after_a_run_of_no_node_panics() {
    let mut lab = Lab::new(1);
    lab.run(Duration::from_millis(1));
    lab.start("a");
}

#[test]
#[should_panic(expected = "lab failure: `start` after the first run or a task")]
fn a_start_after_a_listed_task_panics() {
    let mut lab = Lab::new(1);
    let a = lab.start("a");
    listed(&mut lab, a, "mesh");
    lab.start("b");
}

#[test]
#[should_panic(expected = "lab failure: `start` after the first run or a task")]
fn a_start_after_a_chatter_task_panics() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    chatter(&mut lab, a, b);
    lab.start("c");
}

#[test]
#[should_panic(expected = "lab failure: b is in two meshes")]
fn a_node_in_two_meshes_panics() {
    let mut lab = Lab::new(1);
    let (a, b, c) = (lab.start("a"), lab.start("b"), lab.start("c"));
    lab.mesh(&[a, b]);
    lab.mesh(&[b, c]);
}

#[test]
#[should_panic(expected = "lab failure: a is listed twice")]
fn a_node_listed_twice_in_a_mesh_panics() {
    let mut lab = Lab::new(1);
    let a = lab.start("a");
    lab.mesh(&[a, a]);
}

#[test]
#[should_panic(expected = "lab failure: `mesh` of no node")]
fn a_mesh_of_no_node_panics() {
    Lab::new(1).mesh(&[]);
}

/// A lab with 255 nodes, none started.
fn full() -> Lab {
    let mut lab = Lab::new(1);
    for n in 0..255 {
        lab.start(&format!("n{n}"));
    }
    lab
}

#[test]
fn a_lab_takes_255_nodes() {
    full();
}

#[test]
#[should_panic(expected = "lab failure: at most 255 nodes")]
fn a_lab_panics_at_the_256th_node() {
    full().start("n255");
}

#[test]
fn nodes_start_run_and_stop() {
    let mut lab = Lab::new(1);
    lab.start("cloud");
    lab.start("edge");
    lab.run(Duration::from_secs(1));
    lab.stop();
}

/// Sends one datagram every 100 ms, from 50 ms to 2,950 ms, from `node` to `peer`,
/// and counts the datagrams that reach `node`. Call it after each [`Lab::start`].
fn chatter(lab: &mut Lab, node: Node, peer: Node) -> Arc<AtomicU64> {
    lab.ran = true;
    let at = |n: Node| SocketAddr::new(lab.members[n.0].host.addresses()[0], 9000);
    let host = &lab.members[node.0].host;
    let (mut sender, mut receiver) = host
        .net()
        .udp(&udp::Config {
            local: at(node),
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
        })
        .unwrap();
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let millis = |n: i64| Span::from_nanos(n * Span::MILLISECOND.nanos());
    let (clock, to) = (host.clock(), at(peer));
    let send = host.shards().start(shard("send"), move |_| async move {
        clock.sleep(millis(50)).await;
        for _ in 0..30 {
            let transmit = udp::Transmit {
                destination: to,
                source: None,
                ecn: None,
                contents: b"ping",
                segment: None,
            };
            poll_fn(|cx| sender.poll_send(cx, &transmit)).await.unwrap();
            clock.sleep(millis(100)).await;
        }
    });
    let count = Arc::new(AtomicU64::new(0));
    let tally = Arc::clone(&count);
    let receive = host.shards().start(shard("receive"), move |_| async move {
        let mut bytes = [0; 64];
        let mut meta = [udp::Meta::default()];
        loop {
            poll_fn(|cx| {
                let mut buffers = [IoSliceMut::new(&mut bytes)];
                receiver.poll_recv(cx, &mut buffers, &mut meta)
            })
            .await
            .unwrap();
            let datagrams = meta[0].len / meta[0].stride.max(1);
            tally.fetch_add(datagrams as u64, Ordering::Relaxed);
        }
    });
    drop((send.unwrap(), receive.unwrap()));
    count
}

#[test]
fn a_cut_drops_datagrams_both_ways_until_heal() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    let (at_a, at_b) = (chatter(&mut lab, a, b), chatter(&mut lab, b, a));
    let counts = || [&at_a, &at_b].map(|c| c.load(Ordering::Relaxed));
    lab.run(Duration::from_secs(1));
    assert_eq!(counts(), [10, 10], "before the cut");
    lab.cut(a, b);
    lab.run(Duration::from_secs(1));
    assert_eq!(counts(), [10, 10], "during the cut");
    lab.heal(a, b);
    lab.run(Duration::from_secs(1));
    assert_eq!(counts(), [20, 20], "after the heal");
}

mod stored {
    use super::*;

    fn lab() -> Lab {
        let mut lab = Lab::new(1);
        let mut store = Store::default();
        store
            .write(
                b"foundation_gaps,connector=influx,index=edge.time,path=live \
                  count=2i 1020\nvalue value=2 1020\n",
            )
            .unwrap();
        let influx = Influx {
            store,
            connector: "influx".into(),
            measurements: [("edge.value".into(), "value".into())].into(),
        };
        lab.stores.insert("influx".into(), influx);
        for channel in ["edge.value", "edge.other"] {
            let written = Written {
                seqs: 7..10,
                index: "edge.time".into(),
            };
            lab.written.insert(channel.into(), written);
        }
        lab
    }

    #[test]
    fn folds_the_store_at_the_address_with_the_measurement_of_the_channel() {
        assert_eq!(
            lab().stored("influx", "edge.value"),
            Received {
                samples: 1,
                seqs: Some(9..10),
                contiguous: true,
                gaps: vec![Gap { after: 0, count: 2 }],
            }
        );
    }

    #[test]
    #[should_panic(expected = "lab failure: no Influx store at other")]
    fn panics_with_no_store_at_the_address() {
        lab().stored("other", "edge.value");
    }

    #[test]
    #[should_panic(expected = "lab failure: no sample was written to edge.none")]
    fn panics_with_no_sample_written_to_the_channel() {
        lab().stored("influx", "edge.none");
    }

    #[test]
    #[should_panic(
        expected = "lab failure: the connector at influx writes no edge.other"
    )]
    fn panics_when_the_connector_writes_no_measurement_for_the_channel() {
        lab().stored("influx", "edge.other");
    }
}
