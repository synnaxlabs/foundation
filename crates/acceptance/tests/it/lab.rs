//! A simulated mesh for the scenarios: nodes from `node` on `sim` seams, links that
//! can be cut, and the operator's front ends. Each method waits on the surface named
//! in its `todo!`; it fills in when that surface merges.

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

/// A protocol simulator for one device that a connector talks to.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Device {
    OpcUa,
    ModbusTcp,
    ModbusRtu,
    /// NI, through the stub `libnidaqmx.so`.
    Ni,
}

/// What a reader receives, in order.
#[expect(dead_code, reason = "the harness builds events once readers merge")]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    Sample(Sample),
    /// Samples the buffer no longer has.
    Gap {
        count: u64,
    },
}

/// One received sample.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sample {
    pub seq: u64,
    pub ns: i64,
    pub value: f64,
    /// The time error bound, in nanoseconds.
    pub error_ns: Option<u64>,
}

/// A command and the acknowledgment the connector wrote for it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Command {
    pub subject: String,
    pub value: f64,
    pub ack: Option<f64>,
}

impl Lab {
    /// Builds an empty mesh whose run replays from `seed`.
    pub(crate) fn new(_seed: u64) -> Self {
        todo!("waits on #212")
    }

    /// Starts a node named `name` with a disk budget of `budget` bytes.
    pub(crate) fn start(&mut self, _name: &str, _budget: u64) -> Node {
        todo!("waits on #212")
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

    /// Runs `plan` then `apply` of `hcl` on `node` through the JSON CLI, and returns
    /// the plan's JSON.
    pub(crate) fn apply(&mut self, _node: Node, _hcl: &str) -> String {
        todo!("waits on ops plan and apply")
    }

    /// Runs one CLI command with `--json` on `node` and returns its output.
    pub(crate) fn cli(&mut self, _node: Node, _args: &[&str]) -> String {
        todo!("waits on ops CLI")
    }

    /// Calls one MCP tool on `node` and returns its JSON result.
    pub(crate) fn mcp(&mut self, _node: Node, _tool: &str, _args: &str) -> String {
        todo!("waits on ops MCP")
    }

    /// Attaches a simulated device to `node` at `address`.
    pub(crate) fn device(&mut self, _node: Node, _kind: Device, _address: &str) {
        todo!("waits on connector kinds")
    }

    /// The value the device at `address` holds at `point`.
    pub(crate) fn device_value(&self, _address: &str, _point: &str) -> f64 {
        todo!("waits on connector kinds")
    }

    /// Sets the value the device at `address` reads at `point`.
    pub(crate) fn set_device_value(
        &mut self,
        _address: &str,
        _point: &str,
        _value: f64,
    ) {
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
    ) -> Vec<Event> {
        todo!("waits on hub readers")
    }

    /// The commands recorded on `channel`, with their acknowledgments.
    pub(crate) fn audit(&mut self, _node: Node, _channel: &str) -> Vec<Command> {
        todo!("waits on hub readers")
    }

    /// What the Influx simulator received for `measurement`, in arrival order.
    pub(crate) fn influx(&self, _measurement: &str) -> Vec<Event> {
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
