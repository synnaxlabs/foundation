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
use std::rc::Rc;
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
use types::time::{Monotonic, Span};

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
    /// Each [`Lab::write`], by channel.
    writes: BTreeMap<String, Write>,
    /// The clocks of each host.
    truth: Truth,
    /// The simulation ran, or holds a task of a test, so a [`Lab::start`] would run
    /// it.
    ran: bool,
    /// What each reader got, by [`Reader`].
    readers: Vec<Arc<Mutex<Got>>>,
    /// The node of [`Lab::send`], and what a reader there got, by channel.
    sent: BTreeMap<String, (Node, Arc<Mutex<Got>>)>,
    /// The channels of [`Lab::channel`], by name.
    channels: BTreeMap<String, Keys>,
}

/// The keys of a channel of [`Lab::channel`] and of its index.
#[derive(Debug, Clone, Copy)]
struct Keys {
    index: types::channel::Key,
    data: types::channel::Key,
}

/// What one reader task got.
#[derive(Debug, Default)]
struct Got {
    opened: bool,
    samples: Vec<Sample>,
    ended: Option<hub::reader::Ended>,
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

/// A [`Lab::write`]: its node, its rate, and how far its task got.
#[derive(Debug)]
struct Write {
    node: Node,
    rate: u64,
    progress: Arc<Mutex<Progress>>,
}

/// How far the task of a write got.
#[derive(Debug, Default)]
struct Progress {
    /// The seqs of the samples that the home applied.
    seqs: Option<Range<u64>>,
    /// True time at the open of the writer, when sample 0 was due, in ns since the
    /// UNIX epoch.
    zero: i64,
}

/// The clocks of the hosts of a lab. Each host's clock and wall read those of the
/// config plus the true time since the first host was added, as the lab steps no
/// wall.
#[derive(Debug, Clone, Copy)]
struct Truth(sim::node::Config);

impl Truth {
    /// The config of a new host, where `now` reads the clock of a host added before
    /// it, if there is one.
    fn host(self, now: Option<Monotonic>) -> sim::node::Config {
        let since = now.map_or(Span::ZERO, |now| now - self.0.monotonic);
        sim::node::Config {
            monotonic: self.0.monotonic + since,
            wall: self.0.wall + since,
            ..self.0
        }
    }

    /// The true time when the clock of a host reads `now`, in ns since the UNIX
    /// epoch.
    fn at(self, now: Monotonic) -> i64 {
        (self.0.wall + (now - self.0.monotonic)).nanos()
    }
}

#[derive(Debug)]
struct Member {
    name: String,
    key: types::node::Key,
    private_key: PrivateKey,
    /// The public seal key that `node::create_key` made.
    seal_key: SealKey,
    host: sim::node::Node,
    /// The region that [`Lab::mesh`] gave the node.
    region: Option<Founding>,
    /// The node, from the first [`Lab::run`] on.
    node: Option<node::Node>,
}

/// One node in a [`Lab`]: its index in `members`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Node(usize);

/// A live reader on one channel, opened with [`Lab::reader`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Reader(usize);

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
            writes: BTreeMap::new(),
            truth: Truth(sim::node::Config::default()),
            ran: false,
            readers: Vec::new(),
            sent: BTreeMap::new(),
            channels: BTreeMap::new(),
        }
    }

    /// Adds a node named `name` on a new simulated host, makes the folder `/lab` of
    /// the front ends there, and writes the node's key there. It starts at the next
    /// [`Lab::run`].
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
        let now = self.members.first().map(|first| first.host.clock().now());
        let host = self.sim.node(self.truth.host(now));
        host.make_dir(Path::new("/lab"));
        let (key, private_key) = (
            types::node::Key::from_u128(u128::from(byte)),
            PrivateKey([byte; 32]),
        );
        let created = self.sim.run_on(&host, {
            let private_key = private_key.clone();
            move |host, _| async move {
                node::create_key(&host.files(), &host.entropy(), key, private_key).await
            }
        });
        let seal_key = created
            .expect("lab failure: the run ends")
            .expect("lab failure: the key of a new host");
        self.members.push(Member {
            name: name.into(),
            key,
            private_key,
            seal_key,
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
                budget: node::Budget {
                    pool: types::byte::Size::MEBIBYTE,
                    disk: types::byte::Size::GIBIBYTE,
                },
                memory: Box::new(|len| Ok(block::Heap::new(len))),
                files: {
                    let host = host.clone();
                    Box::new(move || {
                        let host = host.clone();
                        Box::new(move || host.files())
                    })
                },
                entropy: host.entropy(),
                net: host.net(),
                listen: listen(host),
                region: member
                    .region
                    .clone()
                    .map_or(node::Region::Absent, node::Region::Given),
                name: member
                    .name
                    .parse()
                    .expect("lab failure: a node name is a name"),
                settings: node::Settings::default(),
                vars: BTreeMap::new(),
                roots: os::roots::Roots::default(),
                log: node::Log {
                    writer: Box::new(std::io::sink()),
                    format: ops::Log::default(),
                },
                threads: host.threads(),
            });
            member.node = Some(node);
        }
    }

    /// Sets the disk budget of `node` to `bytes`.
    #[expect(clippy::todo, reason = "waits on #1157")]
    pub(crate) fn limit(&mut self, _node: Node, _bytes: u64) {
        todo!("waits on #1157")
    }

    /// The disk budget that holds `span` of one `f64` channel at `rate` samples per
    /// second, as `buffer` stores it.
    #[expect(clippy::todo, reason = "waits on #1256")]
    pub(crate) fn budget(&self, _rate: u64, _span: Duration) -> u64 {
        todo!("waits on #1256")
    }

    /// Makes `nodes` the members of one mesh, without a ticket: they found the root
    /// region, each as a voter, with the first admin `admin`, at the first
    /// [`Lab::run`]. Writes the key of `admin` to the file `admin.key` of each node,
    /// as a founder holds it, so the front ends reach each one.
    ///
    /// # Panics
    ///
    /// When `nodes` is empty, or when a node runs already, is in another mesh, or is
    /// listed twice. After a task of a test, as [`Lab::start`] says.
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
        assert!(
            !self.ran,
            "lab failure: `mesh` after the first run or a task"
        );
        for &node in nodes {
            let admin = sector::admin::encode(&ADMIN);
            let written = self.sim.run_on(
                &self.members[node.0].host,
                move |host, _| async move {
                    let path = Path::new(sector::admin::FILE);
                    sector::publish(&host.files(), path, &admin).await
                },
            );
            written
                .expect("lab failure: the run ends")
                .expect("lab failure: the admin key of a new host");
        }
        let members: Vec<mesh::Member> = nodes
            .iter()
            .map(|&node| {
                let member = &self.members[node.0];
                let card = Card {
                    name: member
                        .name
                        .parse()
                        .expect("lab failure: a node name is a card name"),
                    public_key: member.private_key.public(),
                    seal_key: member.seal_key,
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

    /// Creates the `f64` channel `channel`, whose home is `home`, on the index
    /// `{channel}_time`, in the founding of the region of `home`.
    ///
    /// # Panics
    ///
    /// After a node runs, when `home` is in no mesh, or when a mesh of the lab defines
    /// `channel` or `{channel}_time`.
    pub(crate) fn channel(&mut self, home: Node, channel: &str) {
        use spec::channel::{Channel, Data, Kind};
        use spec::data_type::DataType;
        use spec::definition::Definition;
        use types::sample::{Scalar, Type};
        assert!(
            self.members.iter().all(|member| member.node.is_none()),
            "lab failure: `channel` {channel} after a node runs"
        );
        let founding = self.members[home.0].region.clone().unwrap_or_else(|| {
            panic!("lab failure: the home of {channel} is in no mesh")
        });
        for name in [channel.to_string(), format!("{channel}_time")] {
            let key = name.parse().expect("lab failure: a channel name");
            let mut regions = self.members.iter().filter_map(|m| m.region.as_ref());
            assert!(
                !regions.any(|r| r.definitions.contains_key(&key)),
                "lab failure: {name} exists"
            );
        }
        let next = u128::try_from(2 * self.channels.len() + 1).unwrap();
        let (index, data) = (
            types::channel::Key::from_u128(next),
            types::channel::Key::from_u128(next + 1),
        );
        self.channels
            .insert(channel.to_string(), Keys { index, data });
        let kind = Data::new(
            index,
            None,
            DataType::Sample(Type::Scalar(Scalar::F64)),
            None,
        );
        let definitions = [
            (
                format!("{channel}_time"),
                Channel {
                    key: index,
                    kind: Kind::Index {
                        error: None,
                        control: None,
                    },
                },
            ),
            (
                channel.to_string(),
                Channel {
                    key: data,
                    kind: Kind::Data(kind.expect("lab failure: a data channel")),
                },
            ),
        ];
        let home = self.members[home.0].key;
        for member in &mut self.members {
            let Some(region) = member.region.as_mut().filter(|r| **r == founding)
            else {
                continue;
            };
            for (name, channel) in &definitions {
                let name = name.parse().expect("lab failure: a channel name");
                region
                    .definitions
                    .insert(name, Definition::Channel(channel.clone()));
            }
            region.homes.insert(index, home);
        }
    }

    /// Opens a live reader on `channel` at `node`, and runs the simulation until it
    /// opens. It gets the samples written from now on.
    ///
    /// # Panics
    ///
    /// When no [`Lab::channel`] made `channel`, or the reader does not open in 10 s.
    pub(crate) fn reader(&mut self, node: Node, channel: &str) -> Reader {
        let got = self.open(node, channel);
        self.readers.push(got);
        Reader(self.readers.len() - 1)
    }

    /// Writes `values` to `channel` at `node` as `admin`, one each millisecond from the
    /// open of the writer, as [`Lab::write`] writes. The first send on `channel` opens
    /// a reader at `node` for [`Lab::samples`].
    ///
    /// # Panics
    ///
    /// As the simulation runs, when the home does not apply a sample, as when a send
    /// starts before the one before it on `channel` ends.
    pub(crate) fn send(&mut self, node: Node, channel: &str, values: &[f64]) {
        if !self.sent.contains_key(channel) {
            let got = self.open(node, channel);
            self.sent.insert(channel.to_string(), (node, got));
        }
        let values = values.to_vec();
        let count = u64::try_from(values.len()).unwrap();
        let value = move |k| values[usize::try_from(k).unwrap()];
        self.spawn_write(node, channel, 1000, count, value);
    }

    /// Spawns a task at `node` that opens a writer on `channel` as `admin` and writes
    /// `value(k)` for each `k` below `count` once it is due, `k / rate` seconds after
    /// the open, at most [`BATCH`] samples in one frame. Each stamp is the mesh time at
    /// which its sample was due.
    ///
    /// # Panics
    ///
    /// When `rate` is 0 or over 10^7. As the simulation runs, when the writer does not
    /// open or the home does not apply a frame.
    fn spawn_write(
        &mut self,
        node: Node,
        channel: &str,
        rate: u64,
        count: u64,
        value: impl Fn(u64) -> f64 + Send + 'static,
    ) -> Arc<Mutex<Progress>> {
        use hub::home::Outcome;
        use types::frame::{Label, Path as Stream};
        assert!(
            (1..=10_000_000).contains(&rate),
            "lab failure: a rate of {rate} samples per second is not in 1 to 10^7"
        );
        self.boot();
        let progress = Arc::<Mutex<Progress>>::default();
        let out = Arc::clone(&progress);
        let member = &self.members[node.0];
        let (clock, truth) = (member.host.clock(), self.truth);
        let channel = channel.to_string();
        let config = writer_config("admin", &channel);
        member.node.as_ref().unwrap().spawn(move |hub| async move {
            let opened = hub.writer(config).await;
            let mut writer = opened.expect("lab failure: the writer opens");
            let open = clock.now();
            out.lock().unwrap().zero = truth.at(open);
            let (mut next, mut last) = (0, i64::MIN);
            loop {
                let since = clock.now() - open;
                let due = due_by(since, rate).min(count);
                while next < due {
                    let end = due.min(next + BATCH);
                    let mesh = writer.now().nanos();
                    let samples: Vec<(i64, f64)> = (next..end)
                        .map(|k| {
                            // Mesh time can hold still while it corrects back.
                            let late = since.nanos() - at_due(k, rate).nanos();
                            last = (mesh - late).max(last + 1);
                            (last, value(k))
                        })
                        .collect();
                    let draft = frame(&writer, &samples);
                    let outcomes = writer.write(Label::Path(Stream::Live), draft);
                    let Ok([Outcome::Applied { range, .. }]) = outcomes else {
                        panic!(
                            "the write of samples {next}..{end} of {channel}: \
                             {outcomes:?}"
                        );
                    };
                    let end_seq =
                        range.end().expect("lab failure: a seq below u64::MAX");
                    let mut progress = out.lock().unwrap();
                    let start =
                        progress.seqs.as_ref().map_or(range.seq, |seqs| seqs.start);
                    progress.seqs = Some(start..end_seq);
                    next = end;
                }
                if next == count {
                    return;
                }
                let at = open + at_due(next, rate);
                clock
                    .sleep_until(at.max(clock.now() + Span::MILLISECOND))
                    .await;
            }
        });
        progress
    }

    /// Starts a complete reader on `channel` at `node`, and runs the simulation a
    /// millisecond at a time until it opens: the hub gives only the frames written
    /// after the open.
    fn open(&mut self, node: Node, channel: &str) -> Arc<Mutex<Got>> {
        let keys = self.keys(channel);
        self.boot();
        let got = collect(&self.members[node.0], channel, keys);
        let step = Duration::from_millis(1);
        let opened = self.until(step, 10_000, || got.lock().unwrap().opened);
        assert!(opened, "the reader on {channel} did not open in 10 s");
        got
    }

    /// The keys of the channel `channel` of [`Lab::channel`].
    fn keys(&self, channel: &str) -> Keys {
        let keys = self.channels.get(channel);
        *keys.unwrap_or_else(|| panic!("lab failure: no `channel` made {channel}"))
    }

    /// Runs the simulation `step` at a time until `done` holds, at most `steps`
    /// times. Gives whether `done` held.
    fn until(
        &mut self,
        step: Duration,
        steps: u32,
        mut done: impl FnMut() -> bool,
    ) -> bool {
        for _ in 0..steps {
            if done() {
                return true;
            }
            self.run(step);
        }
        done()
    }

    /// Every sample that `reader` got, in the order it got them.
    ///
    /// # Panics
    ///
    /// When the reader ended.
    pub(crate) fn received(&self, reader: Reader) -> Vec<Sample> {
        took(&self.readers[reader.0])
    }

    /// Creates a single-use join ticket on `admin`.
    #[expect(clippy::todo, reason = "waits on #336")]
    pub(crate) fn ticket(&mut self, _admin: Node) -> Ticket {
        todo!("waits on #336")
    }

    /// Joins `node` to the region of the ticket's issuer.
    #[expect(clippy::todo, reason = "waits on #336")]
    pub(crate) fn join(&mut self, _node: Node, _ticket: Ticket) {
        todo!("waits on #336")
    }

    /// The members that `node` sees, by name, sorted.
    #[expect(clippy::todo, reason = "waits on #336")]
    pub(crate) fn members(&self, _node: Node) -> Vec<String> {
        todo!("waits on #336")
    }

    /// The spec hash that `node` holds.
    #[expect(clippy::todo, reason = "waits on #336")]
    pub(crate) fn spec(&self, _node: Node) -> [u8; 32] {
        todo!("waits on #336")
    }

    /// Runs `plan` of `hcl` on `node` through `ops::Node`, and returns the names of
    /// the changed definitions, sorted.
    ///
    /// # Panics
    ///
    /// With the JSON error of the plan.
    pub(crate) fn plan(&mut self, node: Node, hcl: &str) -> Vec<String> {
        let files = vec![(PathBuf::from("lab.hcl"), hcl.to_owned())];
        let output = self.operate(node, |ops| async move {
            ops.plan(files).await.map(|(_, output)| output)
        });
        changes(&output.unwrap_or_else(|error| panic!("lab failure: {error}")))
    }

    /// Runs `plan` then `apply` of `hcl` on `node` through `ops::Node`.
    ///
    /// # Panics
    ///
    /// With the JSON error of the plan or the apply.
    pub(crate) fn apply(&mut self, node: Node, hcl: &str) {
        let files = vec![(PathBuf::from("lab.hcl"), hcl.to_owned())];
        let applied = self.operate(node, |ops| async move {
            let (plan, _) = ops.plan(files).await?;
            ops.apply(Path::new("lab.plan"), &plan).await
        });
        applied.unwrap_or_else(|error| panic!("lab failure: {error}"));
    }

    /// The node that holds the home of the index `index`, as `node` sees it, or `None`
    /// when it has no home.
    ///
    /// # Panics
    ///
    /// When `index` is not a channel of the spec that `node` uses.
    pub(crate) fn home(&mut self, node: Node, index: &str) -> Option<Node> {
        let index: types::name::Name = index.parse().expect("a name");
        let home = self.operate(node, |ops| async move {
            let spec = ops.mesh().spec().await.expect("a spec");
            let Some(spec::definition::Definition::Channel(channel)) =
                spec.definitions.get(&index)
            else {
                panic!("lab failure: no channel {index}");
            };
            ops.mesh().watch(channel.key).next().await.expect("a mesh")
        });
        let mut members = self.members.iter();
        home.map(|key| {
            Node(
                members
                    .position(|member| member.key == key)
                    .expect("a member"),
            )
        })
    }

    /// Runs `task` with the `ops::Node` of `node`, and gives its result once it ends.
    ///
    /// # Panics
    ///
    /// When `task` does not end in 60 s of simulated time.
    fn operate<T, F>(
        &mut self,
        node: Node,
        task: impl FnOnce(Rc<ops::Node>) -> F + Send + 'static,
    ) -> T
    where
        T: Send + 'static,
        F: Future<Output = T> + 'static,
    {
        self.boot();
        let out = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&out);
        let running = self.members[node.0].node.as_ref().expect("a started node");
        running.operate(move |ops| async move {
            *slot.lock().unwrap() = Some(task(ops).await);
        });
        let done = self.until_set(&out, Duration::from_millis(100), 600);
        done.unwrap_or_else(|| {
            panic!(
                "lab failure: an operation on {} took 60 s",
                self.members[node.0].name
            )
        })
    }

    /// Runs the simulation `step` at a time until `out` holds a value, at most `steps`
    /// times, and takes the value.
    fn until_set<T>(
        &mut self,
        out: &Mutex<Option<T>>,
        step: Duration,
        steps: u32,
    ) -> Option<T> {
        let mut done = None;
        self.until(step, steps, || {
            done = out.lock().unwrap().take();
            done.is_some()
        });
        done
    }

    /// Writes `hcl` to `/lab/lab.hcl` on the host of `node`, runs the MCP `plan` tool
    /// there, and returns the path of the plan and the names of the changed
    /// definitions, sorted.
    ///
    /// # Panics
    ///
    /// As [`Lab::mcp`] says.
    pub(crate) fn mcp_plan(&mut self, node: Node, hcl: &str) -> (String, Vec<String>) {
        let paths = self.members[node.0].host.paths();
        let file = Path::new("/lab/lab.hcl");
        let written = paths.write(file, hcl.as_bytes());
        written.expect("lab failure: the write of /lab/lab.hcl");
        let plan = "/lab/lab.plan";
        let arguments = serde_json::json!({ "files": [file], "out": plan });
        let output = self.mcp(node, "plan", &arguments);
        (plan.to_owned(), changes(&output))
    }

    /// Runs the MCP `apply` tool on `node` with the plan at `plan` on its host, as
    /// [`Lab::mcp_plan`] gives it.
    ///
    /// # Panics
    ///
    /// As [`Lab::mcp`] says.
    pub(crate) fn mcp_apply(&mut self, node: Node, plan: &str) {
        self.mcp(node, "apply", &serde_json::json!({ "plan": plan }));
    }

    /// Calls the MCP tool `tool` with `arguments` through `foundation mcp` on the host
    /// of `node`, with the folder `/lab`, as the first admin, once the node takes
    /// sessions. Gives the structured content of the result.
    ///
    /// # Panics
    ///
    /// When the call gives an error, or does not end in 60 s of simulated time.
    fn mcp(
        &mut self,
        node: Node,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> serde_json::Value {
        self.operate(node, |_| async {});
        self.ran = true;
        let host = self.members[node.0].host.clone();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        let out = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&out);
        let config = env::shards::Config {
            name: "mcp".into(),
            core: None,
        };
        let shards = host.shards();
        let start = shards.start(config, move |tasks| async move {
            let mut lines = vec![
                ops::Line::End,
                ops::Line::Bytes(format!("{message}\n").into()),
            ];
            let (mut output, mut errors) = (Vec::new(), Vec::new());
            // The node's disk is apart from the host's paths: `reach` ignores
            // `--data`, which names an empty folder there.
            let run = ops::cli(
                ["foundation", "mcp", "--files", "/lab", "--data", "/data"]
                    .map(Into::into),
                async || Ok(lines.pop().unwrap_or(ops::Line::End)),
                &mut output,
                &mut errors,
                &host.paths(),
                &node::front_ends(),
                &node::kinds(),
                async |_: &Path| {
                    let client = node::client::reach(node::client::Config {
                        files: host.files(),
                        net: host.net(),
                        clock: host.clock(),
                        entropy: host.entropy(),
                        tasks: tasks.clone(),
                    });
                    let client = client
                        .await
                        .unwrap_or_else(|error| panic!("lab failure: {error:?}"));
                    Ok(async move |body: &[u8]| {
                        let reply = client.request(body).await;
                        Ok(reply.unwrap_or_else(|error| panic!("lab failure: {error}")))
                    })
                },
                async |_: &Path| unreachable!("`mcp` reads no public key"),
            )
            .await;
            *slot.lock().unwrap() = Some((run, output, errors));
        });
        drop(start.unwrap());
        let done = self.until_set(&out, Duration::from_millis(100), 600);
        let (run, output, errors) = done
            .unwrap_or_else(|| panic!("lab failure: the MCP tool {tool} took 60 s"));
        let errors = String::from_utf8_lossy(&errors);
        assert!(
            matches!(run, ops::Run::Exit(0)),
            "lab failure: {run:?}: {errors}"
        );
        let reply: serde_json::Value =
            serde_json::from_slice(&output).expect("lab failure: a JSON reply");
        let result = &reply["result"];
        if let Some(content) = result.get("structuredContent") {
            return content.clone();
        }
        let text = result["content"][0]["text"].as_str();
        let error = text.map_or_else(|| reply["error"].to_string(), str::to_owned);
        panic!("lab failure: the MCP tool {tool} gave {error}");
    }

    /// Attaches a simulated device to `node` at `address`.
    #[expect(clippy::todo, reason = "waits on #434, #435, #436, #3636")]
    pub(crate) fn device(&mut self, _node: Node, _protocol: Protocol, _address: &str) {
        todo!("waits on #434, #435, #436, #3636")
    }

    /// Attaches a simulated Influx store to `node` at `address`.
    #[expect(clippy::todo, reason = "waits on #341")]
    pub(crate) fn influx(&mut self, _node: Node, _address: &str) {
        todo!("waits on #341")
    }

    /// The value of `point` on the device at `address`.
    #[expect(clippy::todo, reason = "waits on #434, #435, #436, #3636")]
    pub(crate) fn point(&self, _address: &str, _point: &str) -> f64 {
        todo!("waits on #434, #435, #436, #3636")
    }

    /// Sets the value of `point` on the device at `address`.
    #[expect(clippy::todo, reason = "waits on #434, #435, #436, #3636")]
    pub(crate) fn set_point(&mut self, _address: &str, _point: &str, _value: f64) {
        todo!("waits on #434, #435, #436, #3636")
    }

    /// Writes `count` samples to `channel` at `node` as `admin`, `rate` each second, as
    /// the simulation runs. Sample `k`, from 0, has the value `k as f64`, and is
    /// stamped with the mesh time at which it was due, `k / rate` seconds after the
    /// writer opens.
    ///
    /// # Panics
    ///
    /// When `rate` is 0 or over 10^7, or `write` wrote to `channel` before. As the
    /// simulation runs, when the writer does not open, as when the spec at `node`
    /// has no `channel`, when `channel` is an index, as its frame then has no index,
    /// or when the home does not apply a frame.
    pub(crate) fn write(&mut self, node: Node, channel: &str, rate: u64, count: u64) {
        assert!(
            !self.writes.contains_key(channel),
            "lab failure: a second write to {channel}"
        );
        #[expect(clippy::cast_precision_loss, reason = "exact below 2^53")]
        let progress = self.spawn_write(node, channel, rate, count, |k| k as f64);
        let write = Write {
            node,
            rate,
            progress,
        };
        self.writes.insert(channel.to_string(), write);
    }

    /// The name of the index of the data channel `channel` in the spec at `node`. It
    /// runs the simulation until the spec read ends.
    ///
    /// # Panics
    ///
    /// When `channel` is not a data channel of the spec at `node`.
    fn index(&mut self, node: Node, channel: &str) -> String {
        use spec::channel::Kind;
        use spec::definition::Definition;
        let name: types::name::Name =
            channel.parse().expect("lab failure: a channel name");
        self.operate(node, |ops| async move {
            let spec = ops.mesh().spec().await.expect("a spec");
            let Some(Definition::Channel(channel)) = spec.definitions.get(&name) else {
                panic!("lab failure: no channel {name}");
            };
            let Kind::Data(data) = &channel.kind else {
                panic!("lab failure: {name} is an index");
            };
            let mut definitions = spec.definitions.iter();
            let index = definitions.find(|(_, definition)| {
                matches!(definition, Definition::Channel(c) if c.key == *data.index())
            });
            index
                .expect("lab failure: the index is a channel")
                .0
                .to_string()
        })
    }

    /// The seqs that the home gave the samples of the [`Lab::write`] to `channel`, so
    /// far. Sample `k` has the seq `start + k`.
    ///
    /// # Panics
    ///
    /// When no [`Lab::write`] wrote to `channel`, or the home applied none of its
    /// samples.
    pub(crate) fn written(&self, channel: &str) -> Range<u64> {
        let seqs = self.write_to(channel).progress.lock().unwrap().seqs.clone();
        seqs.unwrap_or_else(|| {
            panic!("lab failure: the home applied no sample of {channel}")
        })
    }

    /// The [`Lab::write`] to `channel`.
    ///
    /// # Panics
    ///
    /// When there is none.
    fn write_to(&self, channel: &str) -> &Write {
        let write = self.writes.get(channel);
        write.unwrap_or_else(|| {
            panic!("lab failure: no sample was written to {channel}")
        })
    }

    /// The true time, in ns since the UNIX epoch, at which each sample of the
    /// [`Lab::write`] to `channel` that the home applied was due, in order.
    ///
    /// # Panics
    ///
    /// When no [`Lab::write`] wrote to `channel`.
    pub(crate) fn truth(&self, channel: &str) -> Vec<i64> {
        let write = self.write_to(channel);
        let progress = write.progress.lock().unwrap();
        let count = progress
            .seqs
            .as_ref()
            .map_or(0, |seqs| seqs.end - seqs.start);
        let due = |k| progress.zero + at_due(k, write.rate).nanos();
        (0..count).map(due).collect()
    }

    /// Writes one sample of `value` to `channel` at `node` as `subject`, stamped with
    /// mesh time, and runs the simulation until the home applies it. Gives the text of
    /// the error when the writer does not open or the home does not apply the sample.
    ///
    /// # Panics
    ///
    /// When `subject` or `channel` is not a name, or the write does not end in 10 s.
    pub(crate) fn command(
        &mut self,
        node: Node,
        subject: &str,
        channel: &str,
        value: f64,
    ) -> Result<(), String> {
        use hub::home::Outcome;
        use types::frame::{Label, Path as Stream};
        self.boot();
        let config = writer_config(subject, channel);
        let out = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&out);
        let running = self.members[node.0].node.as_ref().unwrap();
        running.spawn(move |hub| async move {
            let written = async {
                let mut writer = hub.writer(config).await.map_err(|e| e.to_string())?;
                let draft = frame(&writer, &[(writer.now().nanos(), value)]);
                match writer.write(Label::Path(Stream::Live), draft) {
                    Ok([Outcome::Applied { .. }]) => Ok(()),
                    Ok([Outcome::Refused { refusal, .. }]) => Err(refusal.to_string()),
                    Ok(outcomes) => {
                        Err(format!("the home did not apply it: {outcomes:?}"))
                    }
                    Err(failure) => Err(failure.to_string()),
                }
            };
            *slot.lock().unwrap() = Some(written.await);
        });
        let done = self.until_set(&out, Duration::from_millis(1), 10_000);
        done.unwrap_or_else(|| {
            panic!("lab failure: the command on {channel} took 10 s")
        })
    }

    /// Reads `channel` on `node` from the oldest sample, as `subject`.
    #[expect(clippy::todo, reason = "waits on #274")]
    pub(crate) fn read(
        &mut self,
        _node: Node,
        _subject: &str,
        _channel: &str,
    ) -> Received {
        todo!("waits on #274")
    }

    /// Reads every sample of `channel` on `node`, as `subject`. For short runs only.
    /// Until a read of the home from its oldest sample exists (#274), it gives what a
    /// reader at the node of the first [`Lab::send`] on `channel` got from that send
    /// on.
    ///
    /// # Panics
    ///
    /// When no [`Lab::send`] wrote to `channel`, its reader ended, or `node` and
    /// `subject` are not the node of the first send and `admin`.
    pub(crate) fn samples(
        &mut self,
        node: Node,
        subject: &str,
        channel: &str,
    ) -> Vec<Sample> {
        let (sender, got) = self
            .sent
            .get(channel)
            .unwrap_or_else(|| panic!("lab failure: no send wrote to {channel}"));
        assert!(
            node == *sender && subject == "admin",
            "lab failure: until #274, `samples` reads only at the node of the first \
             send, as admin"
        );
        took(got)
    }

    /// The commands recorded on `channel`, with their acknowledgments.
    #[expect(clippy::todo, reason = "waits on #3634")]
    pub(crate) fn audit(&mut self, _node: Node, _channel: &str) -> Vec<Command> {
        todo!("waits on #3634")
    }

    /// What the Influx store at `address` holds for the samples of the [`Lab::write`]
    /// to `channel`, folded by [`influx::stored`]. It runs the simulation in steps of
    /// 100 ms until a read of the spec at the node of the write ends, so a live write
    /// moves ahead by at least 100 ms.
    ///
    /// # Panics
    ///
    /// When no store is at `address`, as [`Lab::written`], when the store's connector
    /// writes no measurement for `channel`, or when [`influx::stored`] panics.
    pub(crate) fn stored(&mut self, address: &str, channel: &str) -> Received {
        assert!(
            self.stores.contains_key(address),
            "lab failure: no Influx store at {address}"
        );
        let node = self.write_to(channel).node;
        let measurement = self.stores[address].measurements.get(channel);
        let measurement = measurement.unwrap_or_else(|| {
            panic!("lab failure: the connector at {address} writes no {channel}")
        });
        let measurement = measurement.clone();
        // The spec read runs the simulation, so the seqs and the store are read after
        // it, at one time.
        let index = self.index(node, channel);
        let seqs = self.written(channel);
        let influx = &self.stores[address];
        influx::stored(
            &influx.store,
            &influx.connector,
            &measurement,
            &seqs,
            &index,
        )
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

/// Spawns a complete reader on `channel` at `member`, which puts what it gets in
/// the returned value.
fn collect(member: &Member, channel: &str, keys: Keys) -> Arc<Mutex<Got>> {
    let got = Arc::<Mutex<Got>>::default();
    let out = Arc::clone(&got);
    let open = hub::reader::Config {
        select: types::name::Selector::new([channel]).expect("lab failure: a selector"),
        mode: hub::reader::Mode::Complete,
        subject: "lab".parse().expect("lab failure: a name"),
        name: None,
        hold: types::time::Span::ZERO,
    };
    let node = member.node.as_ref().expect("lab failure: the node runs");
    node.spawn(move |hub| async move {
        let opened = hub.reader(open).await;
        let mut reader = opened.expect("the reader opens");
        out.lock().unwrap().opened = true;
        loop {
            match reader.next().await {
                Ok(received) => {
                    let samples = decode(&received, keys);
                    out.lock().unwrap().samples.extend(samples);
                }
                Err(ended) => {
                    out.lock().unwrap().ended = Some(ended);
                    return;
                }
            }
        }
    });
    got
}

/// The samples of `got`.
///
/// # Panics
///
/// When its reader ended.
fn took(got: &Mutex<Got>) -> Vec<Sample> {
    let got = got.lock().unwrap();
    if let Some(ended) = &got.ended {
        panic!("the reader ended: {ended}");
    }
    got.samples.clone()
}

/// The most samples that one frame of [`Lab::write`] holds.
const BATCH: u64 = 1024;

/// The config of a writer on `channel` as `subject`.
///
/// # Panics
///
/// When `subject` or `channel` is not a name.
fn writer_config(subject: &str, channel: &str) -> hub::writer::Config {
    hub::writer::Config {
        subject: subject.parse().expect("lab failure: a subject name"),
        authority: types::authority::Authority(1),
        lease: None,
        channels: vec![channel.parse().expect("lab failure: a channel name")],
    }
}

/// The span after the open at which sample `k` is due, at `rate` samples a second.
fn at_due(k: u64, rate: u64) -> Span {
    let nanos = u128::from(k) * 1_000_000_000 / u128::from(rate);
    Span::from_nanos(
        i64::try_from(nanos).expect("lab failure: a due time fits in a span"),
    )
}

/// How many samples are due `since` the open, at `rate` samples a second.
fn due_by(since: Span, rate: u64) -> u64 {
    let since = u128::try_from(since.nanos()).expect("time does not go back");
    u64::try_from(since * u128::from(rate) / 1_000_000_000 + 1).unwrap()
}

/// A frame of `samples`, each a stamp in ns and a value, for `writer`, a writer on one
/// `f64` channel.
fn frame(writer: &hub::writer::Writer, samples: &[(i64, f64)]) -> types::frame::Draft {
    let set = writer.set().entries();
    let data = writer.entries()[0];
    let time = (0..set.len())
        .find(|&at| at != data && set[at].group == set[data].group)
        .expect("lab failure: the set holds the index");
    let bytes = 8 * samples.len();
    let mut series = [(time, bytes), (data, bytes)];
    series.sort_unstable();
    let mut draft = writer
        .draft(types::frame::Form::Raw, &series)
        .expect("lab failure: a frame");
    let fill =
        |draft: &mut types::frame::Draft, entry, part: fn(&(i64, f64)) -> [u8; 8]| {
            let series = draft
                .series_mut(entry)
                .expect("lab failure: the series is present");
            for (out, sample) in series.as_chunks_mut::<8>().0.iter_mut().zip(samples) {
                out.copy_from_slice(&part(sample));
            }
        };
    fill(&mut draft, time, |sample| sample.0.to_le_bytes());
    fill(&mut draft, data, |sample| sample.1.to_le_bytes());
    let count = u32::try_from(samples.len()).expect("lab failure: a count below 2^32");
    draft.set_count(set[time].group, count);
    draft
}

/// The samples of `received`, a frame of the channel of `keys` and its index.
///
/// # Panics
///
/// When the frame holds another channel.
fn decode(received: &hub::reader::Received<'_>, keys: Keys) -> Vec<Sample> {
    let entries = received.set().entries();
    let mut series = [Vec::new(), Vec::new()];
    for (entry, bytes) in received.view().iter() {
        let at = &entries[entry];
        let range = received
            .view()
            .range(at.group)
            .expect("the group has a range");
        let count = usize::try_from(range.count).unwrap();
        let out = match at.key {
            key if key == keys.index => &mut series[0],
            key if key == keys.data => &mut series[1],
            key => panic!("the frame holds {key:?}"),
        };
        codec::decode(at.data_type, count, bytes, out).expect("the series decodes");
    }
    let [time, data] = series;
    let (time, data) = (time.as_chunks::<8>().0, data.as_chunks::<8>().0);
    time.iter()
        .zip(data)
        .map(|(t, d)| Sample {
            ns: i64::from_le_bytes(*t),
            value: f64::from_le_bytes(*d),
        })
        .collect()
}

/// The names of the changed definitions of the JSON `output` of a plan, sorted.
fn changes(output: &serde_json::Value) -> Vec<String> {
    let changes = output["changes"].as_array().expect("a list of changes");
    let mut names: Vec<String> = changes
        .iter()
        .map(|change| change["name"].as_str().expect("a name").to_owned())
        .collect();
    names.sort();
    names
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
        let a = lab.start("a");
        lab.mesh(&[a]);
        lab.channel(a, "edge.value");
        let mut store = Store::default();
        store
            .write(
                b"foundation_gaps,connector=influx,index=edge.value_time,path=live \
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
            let progress = Progress {
                seqs: Some(7..10),
                zero: 0,
            };
            let write = Write {
                node: a,
                rate: 1,
                progress: Arc::new(Mutex::new(progress)),
            };
            lab.writes.insert(channel.into(), write);
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
    fn reads_the_seqs_after_the_spec_read() {
        let mut lab = lab();
        lab.writes.clear();
        lab.write(Node(0), "edge.value", 1000, 10_000);
        lab.run(Duration::from_secs(1));
        let before = lab.written("edge.value").count();
        let mut store = Store::default();
        let line = format!("value value={before} 2000\n");
        store.write(line.as_bytes()).unwrap();
        lab.stores.get_mut("influx").unwrap().store = store;
        let stored = lab.stored("influx", "edge.value");
        assert_eq!(stored.samples, 1, "{before} {stored:?}");
        lab.stop();
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

/// A lab of the nodes `a` and `b` in one mesh, with the channel `a.value` at `a`.
fn pair() -> (Lab, Node, Node) {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    lab.mesh(&[a, b]);
    lab.channel(a, "a.value");
    (lab, a, b)
}

fn values(samples: &[Sample]) -> Vec<f64> {
    samples.iter().map(|sample| sample.value).collect()
}

#[test]
fn a_second_send_stamps_after_the_first() {
    let (mut lab, a, b) = pair();
    let reader = lab.reader(b, "a.value");
    lab.send(a, "a.value", &[1.0, 2.0]);
    lab.run(Duration::from_secs(1));
    lab.send(a, "a.value", &[3.0]);
    lab.run(Duration::from_secs(1));
    let stored = lab.samples(a, "admin", "a.value");
    assert_eq!(values(&stored), [1.0, 2.0, 3.0]);
    assert!(stored[1].ns < stored[2].ns, "{stored:?}");
    assert_eq!(lab.received(reader), stored);
    lab.stop();
}

#[test]
#[should_panic(expected = "lab failure: no `channel` made a.value_time")]
fn a_reader_on_an_index_panics() {
    let (mut lab, _, b) = pair();
    lab.reader(b, "a.value_time");
}

#[test]
#[should_panic(
    expected = "lab failure: until #274, `samples` reads only at the node \
                           of the first send, as admin"
)]
fn samples_at_another_node_panic() {
    let (mut lab, a, b) = pair();
    lab.send(a, "a.value", &[1.0]);
    lab.samples(b, "admin", "a.value");
}

#[test]
#[should_panic(expected = "lab failure: `channel` b.value after a node runs")]
fn a_channel_after_a_node_runs_panics() {
    let (mut lab, _, b) = pair();
    lab.run(Duration::from_millis(1));
    lab.channel(b, "b.value");
}

#[test]
#[should_panic(expected = "lab failure: the home of c.value is in no mesh")]
fn a_channel_at_a_node_in_no_mesh_panics() {
    let (mut lab, ..) = pair();
    let c = lab.start("c");
    lab.channel(c, "c.value");
}

#[test]
#[should_panic(expected = "lab failure: a.value exists")]
fn a_channel_made_twice_panics() {
    let (mut lab, a, _) = pair();
    lab.channel(a, "a.value");
}

#[test]
#[should_panic(expected = "lab failure: `start` after the first run or a task")]
fn a_start_after_a_reader_panics() {
    let (mut lab, _, b) = pair();
    lab.reader(b, "a.value");
    lab.start("c");
}

#[test]
#[should_panic(
    expected = "the reader ended: the stream to the home broke: the peer stopped \
                answering"
)]
fn received_after_the_reader_ended_panics() {
    let (mut lab, a, b) = pair();
    let reader = lab.reader(b, "a.value");
    lab.cut(a, b);
    lab.run(Duration::from_secs(120));
    lab.received(reader);
}

#[test]
#[should_panic(expected = "lab failure: the writer opens: Remote { home: Key(1) }")]
fn a_send_at_a_node_that_is_not_the_home_panics() {
    let (mut lab, _, b) = pair();
    lab.send(b, "a.value", &[1.0]);
    lab.run(Duration::from_secs(1));
}

#[test]
#[should_panic(expected = "lab failure: no send wrote to a.value")]
fn samples_with_no_send_panic() {
    let (mut lab, a, _) = pair();
    lab.samples(a, "admin", "a.value");
}

#[test]
#[should_panic(expected = "lab failure: a.value_time exists")]
fn a_channel_named_as_an_index_panics() {
    let (mut lab, a, _) = pair();
    lab.channel(a, "a.value_time");
}

#[test]
#[should_panic(expected = "lab failure: b_time exists")]
fn an_index_named_as_a_channel_panics() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    lab.mesh(&[a, b]);
    lab.channel(a, "b_time");
    lab.channel(a, "b");
}

#[test]
#[should_panic(
    expected = "the write of samples 0..1 of a.value: Ok([Refused { slot: Slot(0), \
                refusal: Waiting }])"
)]
fn an_overlapping_send_panics() {
    let (mut lab, a, _) = pair();
    lab.send(a, "a.value", &[1.0, 2.0, 3.0]);
    lab.run(Duration::from_micros(1500));
    lab.send(a, "a.value", &[4.0]);
    lab.run(Duration::from_secs(1));
}

#[test]
fn two_channels_keep_their_samples() {
    let (mut lab, a, b) = pair();
    lab.channel(b, "b.value");
    lab.send(a, "a.value", &[1.0]);
    lab.send(b, "b.value", &[2.0]);
    lab.run(Duration::from_secs(1));
    assert_eq!(values(&lab.samples(a, "admin", "a.value")), [1.0]);
    assert_eq!(values(&lab.samples(b, "admin", "b.value")), [2.0]);
    lab.stop();
}

#[test]
#[should_panic(expected = "lab failure: no channel a.value_time")]
fn a_channel_stays_in_the_mesh_of_its_home() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    lab.mesh(&[a]);
    lab.mesh(&[b]);
    lab.channel(a, "a.value");
    lab.home(b, "a.value_time");
}

#[test]
#[should_panic(expected = "until #274")]
fn samples_as_another_subject_panic() {
    let (mut lab, a, _) = pair();
    lab.send(a, "a.value", &[1.0]);
    lab.samples(a, "operator", "a.value");
}

#[test]
#[should_panic(expected = "the reader on a.value did not open in 10 s")]
fn a_reader_cut_from_the_home_panics() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    lab.mesh(&[a, b]);
    lab.channel(a, "a.value");
    lab.cut(a, b);
    lab.reader(b, "a.value");
}

#[test]
#[should_panic(expected = "lab failure: x exists")]
fn one_name_in_two_meshes_panics() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    lab.mesh(&[a]);
    lab.mesh(&[b]);
    lab.channel(a, "x");
    lab.channel(b, "x");
}

#[test]
#[should_panic(expected = "lab failure: x_time exists")]
fn an_index_name_in_another_mesh_panics() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    lab.mesh(&[a]);
    lab.mesh(&[b]);
    lab.channel(a, "x");
    lab.channel(b, "x_time");
}

#[test]
#[should_panic(expected = "lab failure: x_time exists")]
fn a_channel_on_an_index_name_in_another_mesh_panics() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    lab.mesh(&[a]);
    lab.mesh(&[b]);
    lab.channel(b, "x_time");
    lab.channel(a, "x");
}

#[test]
fn a_write_reads_at_another_member() {
    let (mut lab, a, b) = pair();
    let reader = lab.reader(b, "a.value");
    lab.write(a, "a.value", 1000, 5);
    lab.run(Duration::from_secs(1));
    let got = lab.received(reader);
    assert_eq!(values(&got), [0.0, 1.0, 2.0, 3.0, 4.0]);
    assert_eq!(lab.written("a.value").count(), 5);
    let truth = lab.truth("a.value");
    assert!(
        truth.array_windows().all(|[x, y]| y - x == 1_000_000),
        "{truth:?}"
    );
    // Each host's wall has no error, so each stamp is the true time of its sample.
    assert_eq!(
        got.iter().map(|sample| sample.ns).collect::<Vec<_>>(),
        truth
    );
    lab.stop();
}

#[test]
fn a_write_at_the_last_host_started_stamps_true_time() {
    let mut lab = Lab::new(1);
    let (a, b, c) = (lab.start("a"), lab.start("b"), lab.start("c"));
    lab.mesh(&[a, b, c]);
    lab.channel(c, "c.value");
    let reader = lab.reader(a, "c.value");
    lab.write(c, "c.value", 1000, 5);
    lab.run(Duration::from_secs(1));
    let got = lab.received(reader);
    let stamps: Vec<i64> = got.iter().map(|sample| sample.ns).collect();
    assert_eq!(stamps, lab.truth("c.value"));
    lab.stop();
}

/// Reads the clocks through the private `members`. With both offsets of `Truth::host`
/// removed, `truth` of a write at a later host lags true time by the time the adds
/// took, under 1 ms for three hosts. The open of a writer varies by more than that
/// (-850 µs to +677 µs over seeds 0 to 15, box1), so no lab call shows the lag.
#[test]
fn each_host_clock_reads_the_same_time() {
    let mut lab = Lab::new(1);
    for name in ["a", "b", "c"] {
        lab.start(name);
    }
    let nows: Vec<_> = lab.members.iter().map(|m| m.host.clock().now()).collect();
    assert!(nows.iter().all(|now| *now == nows[0]), "{nows:?}");
}

#[test]
#[should_panic(expected = "lab failure: the home applied no sample of a.value")]
fn written_before_the_first_sample_applies_panics() {
    let (mut lab, a, _) = pair();
    lab.write(a, "a.value", 1000, 1);
    lab.written("a.value");
}

#[test]
#[should_panic(expected = "lab failure: the writer opens: Unknown(Name(\"a.none\"))")]
fn a_write_to_no_channel_panics() {
    let (mut lab, a, _) = pair();
    lab.write(a, "a.none", 1000, 1);
    lab.run(Duration::from_secs(1));
}

#[test]
#[should_panic(expected = "lab failure: the set holds the index")]
fn a_write_to_an_index_panics() {
    let (mut lab, a, _) = pair();
    lab.write(a, "a.value_time", 1000, 1);
    lab.run(Duration::from_secs(1));
}

#[test]
fn a_fast_write_batches_its_samples() {
    let (mut lab, a, b) = pair();
    let reader = lab.reader(b, "a.value");
    lab.write(a, "a.value", 1_000_000, 10_000);
    lab.run(Duration::from_secs(1));
    let got = lab.received(reader);
    let expected: Vec<f64> = (0..10_000).map(f64::from).collect();
    assert_eq!(values(&got), expected);
    assert!(got.array_windows().all(|[x, y]| x.ns < y.ns));
    let truth = lab.truth("a.value");
    assert_eq!(truth.len(), 10_000);
    assert!(truth.array_windows().all(|[x, y]| y - x == 1000));
    lab.stop();
}

#[test]
fn a_write_continues_the_seqs_of_a_send() {
    let (mut lab, a, _) = pair();
    lab.send(a, "a.value", &[1.0, 2.0, 3.0]);
    lab.run(Duration::from_secs(1));
    lab.write(a, "a.value", 1000, 2);
    lab.run(Duration::from_secs(1));
    assert_eq!(lab.written("a.value"), 3..5);
    let stored = lab.samples(a, "admin", "a.value");
    assert_eq!(values(&stored), [1.0, 2.0, 3.0, 0.0, 1.0]);
    lab.stop();
}

#[test]
fn a_spec_change_in_a_cut_mesh_does_not_commit() {
    let (mut lab, a, b) = pair();
    lab.cut(a, b);
    let hcl = r#"channel "x.time" { kind = "index" }
placement "x" {
  select = "x.*"
  home   = "a"
}
"#;
    let files = vec![(PathBuf::from("lab.hcl"), hcl.to_owned())];
    let (applied, held) = lab.operate(a, |ops| async move {
        let (plan, _) = ops.plan(files).await.expect("a plan");
        let applied = ops.apply(Path::new("lab.plan"), &plan).await;
        let spec = ops.mesh().spec().await.expect("a spec");
        let name: types::name::Name = "x.time".parse().unwrap();
        (applied, spec.definitions.contains_key(&name))
    });
    let error = serde_json::json!({"errors": [{
        "code": "ops.apply",
        "fix": "Fix the cause in the message, then plan and apply again",
        "message": "1 of 2 voters hold the chunks of the spec change, not a majority; \
                    b: a step of its put passed the deadline of 10s",
        "notes": [],
    }]});
    assert_eq!(applied.unwrap_err(), error);
    assert!(!held, "the spec at a holds x.time");
}

#[test]
#[should_panic(expected = "lab failure: a second write to a.value")]
fn a_second_write_panics() {
    let (mut lab, a, _) = pair();
    lab.write(a, "a.value", 1000, 1);
    lab.write(a, "a.value", 1000, 1);
}

#[test]
#[should_panic(
    expected = "lab failure: a rate of 0 samples per second is not in 1 to 10^7"
)]
fn a_write_of_rate_zero_panics() {
    let (mut lab, a, _) = pair();
    lab.write(a, "a.value", 0, 1);
}

#[test]
#[should_panic(
    expected = "lab failure: a rate of 10000001 samples per second is not in 1 to 10^7"
)]
fn a_write_over_the_top_rate_panics() {
    let (mut lab, a, _) = pair();
    lab.write(a, "a.value", 10_000_001, 1);
}

#[test]
fn a_write_at_the_top_rate_applies_each_sample() {
    let (mut lab, a, _) = pair();
    lab.write(a, "a.value", 10_000_000, 1_000_000);
    lab.run(Duration::from_secs(1));
    assert_eq!(lab.written("a.value").count(), 1_000_000);
    lab.stop();
}

#[test]
#[should_panic(expected = "lab failure: no sample was written to a.value")]
fn written_with_no_write_panics() {
    let (lab, ..) = pair();
    lab.written("a.value");
}

#[test]
fn a_command_reads_at_another_member() {
    let (mut lab, a, b) = pair();
    let reader = lab.reader(b, "a.value");
    assert_eq!(lab.command(a, "operator", "a.value", 7.0), Ok(()));
    lab.run(Duration::from_secs(1));
    assert_eq!(values(&lab.received(reader)), [7.0]);
    lab.stop();
}

#[test]
fn a_command_at_a_node_that_is_not_the_home_fails() {
    let (mut lab, a, b) = pair();
    let home = lab.members[a.0].key;
    assert_eq!(
        lab.command(b, "operator", "a.value", 7.0),
        Err(format!(
            "the home of an index of the writer is node {home}, and a writer writes \
             only at this node"
        ))
    );
}

#[test]
fn a_command_during_a_send_is_refused() {
    let (mut lab, a, _) = pair();
    lab.send(a, "a.value", &[1.0, 2.0, 3.0]);
    lab.run(Duration::from_micros(1500));
    assert_eq!(
        lab.command(a, "operator", "a.value", 7.0),
        Err("not in control: another writer holds the gate".into())
    );
}

#[test]
fn a_command_on_no_channel_fails() {
    let (mut lab, a, _) = pair();
    assert_eq!(
        lab.command(a, "operator", "x", 7.0),
        Err("no channel is named x".into())
    );
}

/// An index whose home is `a`, and a channel on it.
const SITE: &str = r#"channel "site.time" { kind = "index" }
channel "site.temp" {
  data_type = "f64"
  index = "site.time"
}
placement "site" {
  select = "site.*"
  home   = "a"
}
"#;

#[test]
fn an_mcp_apply_at_one_member_changes_the_spec_of_the_mesh() {
    let mut lab = Lab::new(1);
    let (a, b) = (lab.start("a"), lab.start("b"));
    lab.mesh(&[a, b]);
    let (plan, changes) = lab.mcp_plan(b, SITE);
    assert_eq!(plan, "/lab/lab.plan");
    assert_eq!(changes, ["site", "site.temp", "site.time"]);
    lab.mcp_apply(b, &plan);
    assert_eq!(lab.mcp_plan(a, SITE).1, Vec::<String>::new(), "a");
    assert_eq!(lab.mcp_plan(b, SITE).1, Vec::<String>::new(), "b");
    lab.stop();
}

#[test]
#[should_panic(
    expected = "lab failure: the MCP tool apply gave {\"errors\":[{\"code\":\
                           \"ops.read\",\"fix\":\"Fix the cause in the message, then \
                           run the command again\",\"message\":\"no file is at \
                           /lab/none.plan\",\"notes\":[]}]}"
)]
fn an_mcp_tool_error_panics() {
    let mut lab = Lab::new(1);
    let a = lab.start("a");
    lab.mesh(&[a]);
    lab.mcp_apply(a, "/lab/none.plan");
}

#[test]
#[should_panic(expected = "lab failure: `mesh` after the first run or a task")]
fn a_mesh_after_a_listed_task_panics() {
    let mut lab = Lab::new(1);
    let a = lab.start("a");
    listed(&mut lab, a, "mesh");
    lab.mesh(&[a]);
}
