//! A simulated mesh for the scenarios: nodes from `node` on `sim` seams, links that
//! can be cut, simulated devices and stores, and the operator's front ends. Each
//! method waits on the surface named in its `todo!`.

use std::ops::Range;
use std::time::Duration;

/// A whole mesh on one deterministic simulation.
#[derive(Debug)]
pub(crate) struct Lab;

/// One node in a [`Lab`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Node;

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
    pub(crate) fn new(_key: u64) -> Self {
        todo!("waits on #212")
    }

    /// Starts a node named `name` with a disk budget of `budget` bytes.
    pub(crate) fn start(&mut self, _name: &str, _budget: u64) -> Node {
        todo!("waits on #212")
    }

    /// The disk budget that holds `span` of one `f64` channel at `rate` samples per
    /// second, as `buffer` stores it.
    pub(crate) fn budget(&self, _rate: u64, _span: Duration) -> u64 {
        todo!("waits on buffer")
    }

    /// Creates a single-use join ticket on `admin`.
    pub(crate) fn ticket(&mut self, _admin: Node) -> Ticket {
        todo!("waits on mesh join")
    }

    /// Joins `node` to the region of the ticket's issuer.
    pub(crate) fn join(&mut self, _node: Node, _ticket: Ticket) {
        todo!("waits on mesh join")
    }

    /// The members that `node` sees, by name, sorted.
    pub(crate) fn members(&self, _node: Node) -> Vec<String> {
        todo!("waits on mesh membership")
    }

    /// The spec hash that `node` holds.
    pub(crate) fn spec(&self, _node: Node) -> [u8; 32] {
        todo!("waits on mesh spec")
    }

    /// Runs `plan` of `hcl` on `node` through the JSON CLI and returns the names of
    /// the changed definitions.
    pub(crate) fn plan(&mut self, _node: Node, _hcl: &str) -> Vec<String> {
        todo!("waits on ops plan")
    }

    /// Runs `plan` then `apply` of `hcl` on `node` through the JSON CLI.
    pub(crate) fn apply(&mut self, _node: Node, _hcl: &str) {
        todo!("waits on ops apply")
    }

    /// Runs the MCP `plan` tool on `node` and returns the plan and the names of the
    /// changed definitions.
    pub(crate) fn mcp_plan(
        &mut self,
        _node: Node,
        _hcl: &str,
    ) -> (String, Vec<String>) {
        todo!("waits on ops MCP")
    }

    /// Runs the MCP `apply` tool on `node` with a plan from [`Lab::mcp_plan`].
    pub(crate) fn mcp_apply(&mut self, _node: Node, _plan: &str) {
        todo!("waits on ops MCP")
    }

    /// Attaches a simulated device to `node` at `address`.
    pub(crate) fn device(&mut self, _node: Node, _protocol: Protocol, _address: &str) {
        todo!("waits on connector kinds")
    }

    /// Attaches a simulated Influx store to `node` at `address`.
    pub(crate) fn influx(&mut self, _node: Node, _address: &str) {
        todo!("waits on connector-influx")
    }

    /// The value of `point` on the device at `address`.
    pub(crate) fn point(&self, _address: &str, _point: &str) -> f64 {
        todo!("waits on connector kinds")
    }

    /// Sets the value of `point` on the device at `address`.
    pub(crate) fn set_point(&mut self, _address: &str, _point: &str, _value: f64) {
        todo!("waits on connector kinds")
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
        todo!("waits on hub writers")
    }

    /// The seqs that the home gave the samples written to `channel`.
    pub(crate) fn written(&self, _channel: &str) -> Range<u64> {
        todo!("waits on hub writers")
    }

    /// The true simulated time of each sample written to `channel`, in order.
    pub(crate) fn truth(&self, _channel: &str) -> Vec<i64> {
        todo!("waits on hub writers")
    }

    /// Sends `value` to the command channel `channel` as `subject`.
    pub(crate) fn command(
        &mut self,
        _node: Node,
        _subject: &str,
        _channel: &str,
        _value: f64,
    ) -> Result<(), String> {
        todo!("waits on hub writers and control")
    }

    /// Reads `channel` on `node` from the oldest sample, as `subject`.
    pub(crate) fn read(
        &mut self,
        _node: Node,
        _subject: &str,
        _channel: &str,
    ) -> Received {
        todo!("waits on hub readers")
    }

    /// Reads every sample of `channel` on `node`, as `subject`. For short runs only.
    pub(crate) fn samples(
        &mut self,
        _node: Node,
        _subject: &str,
        _channel: &str,
    ) -> Vec<Sample> {
        todo!("waits on hub readers")
    }

    /// The commands recorded on `channel`, with their acknowledgments.
    pub(crate) fn audit(&mut self, _node: Node, _channel: &str) -> Vec<Command> {
        todo!("waits on hub readers")
    }

    /// What the Influx store at `address` received for `measurement`.
    pub(crate) fn stored(&self, _address: &str, _measurement: &str) -> Received {
        todo!("waits on connector-influx")
    }

    /// Cuts every link between `a` and `b`.
    pub(crate) fn cut(&mut self, _a: Node, _b: Node) {
        todo!("waits on sim network #113")
    }

    /// Restores every link between `a` and `b`.
    pub(crate) fn heal(&mut self, _a: Node, _b: Node) {
        todo!("waits on sim network #113")
    }

    /// Runs the simulation for `span` of simulated time.
    pub(crate) fn run(&mut self, _span: Duration) {
        todo!("waits on #212")
    }
}
