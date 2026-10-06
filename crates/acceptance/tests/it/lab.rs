//! A simulated mesh for the scenarios: nodes from `node` on `sim` seams, links that
//! can be cut, simulated devices and stores, and the operator's front ends. Each
//! method with a `todo!` waits on the issue it names.

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use env::net::udp;
use types::time::Span;

/// A whole mesh on one deterministic simulation.
#[derive(Debug)]
pub(crate) struct Lab {
    sim: sim::Sim,
    /// The link that [`Lab::heal`] puts back.
    link: sim::link::Config,
    members: Vec<Member>,
}

#[derive(Debug)]
struct Member {
    name: String,
    host: sim::node::Node,
    node: node::Node,
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

/// What a reader received on one channel, folded as it arrives, so an hour at full
/// rate needs no memory per sample.
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
        }
    }

    /// Starts a node named `name` on a new simulated host.
    pub(crate) fn start(&mut self, name: &str) -> Node {
        let host = self.sim.node(sim::node::Config::default());
        let node = node::Node::start(node::Config {
            shards: host.shards(),
            budget: 1 << 20,
            memory: Box::new(|len| Ok(block::Heap::new(len))),
        });
        self.members.push(Member {
            name: name.into(),
            host,
            node,
        });
        Node(self.members.len() - 1)
    }

    /// Sets the disk budget of `node` to `bytes`.
    pub(crate) fn limit(&mut self, _node: Node, _bytes: u64) {
        todo!("waits on #342")
    }

    /// The disk budget that holds `span` of one `f64` channel at `rate` samples per
    /// second, as `buffer` stores it.
    pub(crate) fn budget(&self, _rate: u64, _span: Duration) -> u64 {
        todo!("waits on #342")
    }

    /// Makes `nodes` the members of one mesh, without a ticket.
    pub(crate) fn mesh(&mut self, _nodes: &[Node]) {
        todo!("waits on #462")
    }

    /// Creates the `f64` channel `channel`, whose home is `home`.
    pub(crate) fn channel(&mut self, _home: Node, _channel: &str) {
        todo!("waits on #462")
    }

    /// Opens a live reader on `channel` at `node`. It gets the samples written from
    /// now on.
    pub(crate) fn reader(&mut self, _node: Node, _channel: &str) -> Reader {
        todo!("waits on #462")
    }

    /// Writes `values` to `channel` on `node`, one each millisecond, as the
    /// simulation runs.
    pub(crate) fn send(&mut self, _node: Node, _channel: &str, _values: &[f64]) {
        todo!("waits on #462")
    }

    /// Every sample that `reader` got, in the order it got them.
    pub(crate) fn received(&self, _reader: Reader) -> Vec<Sample> {
        todo!("waits on #462")
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
    /// as the simulation runs.
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
        todo!("waits on #340")
    }

    /// The commands recorded on `channel`, with their acknowledgments.
    pub(crate) fn audit(&mut self, _node: Node, _channel: &str) -> Vec<Command> {
        todo!("waits on #340")
    }

    /// What the Influx store at `address` received for `measurement`.
    pub(crate) fn stored(&self, _address: &str, _measurement: &str) -> Received {
        todo!("waits on #341")
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

    /// Runs the simulation for `span` of simulated time.
    ///
    /// # Panics
    ///
    /// When a task panics or the run takes too many steps.
    pub(crate) fn run(&mut self, span: Duration) {
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

    /// Stops every node and runs the simulation until each has ended.
    ///
    /// # Panics
    ///
    /// When the run fails, or a node ends with an error.
    pub(crate) fn stop(mut self) {
        for member in &self.members {
            member.node.stop();
        }
        if let Err(e) = self.sim.run() {
            panic!("{e}");
        }
        for member in self.members {
            if let Err(e) = member.node.join() {
                panic!("{}: {e}", member.name);
            }
        }
    }
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
/// and counts the datagrams that reach `node`.
fn chatter(lab: &Lab, node: Node, peer: Node) -> Arc<AtomicU64> {
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
    let (at_a, at_b) = (chatter(&lab, a, b), chatter(&lab, b, a));
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
