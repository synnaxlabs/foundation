//! The conformance suite of the `opcua` kind.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::net::SocketAddr;
use std::num::NonZeroUsize;

use connector::kind::Table;
use connector::status::{Class, Reading, State};
use connector_opcua::Kind;
use connector_opcua::sim::{Variables, serve};
use document::diagnostic::{Code, Diagnostic};
use document::value::{self, Value};
use document::{Attribute, Block, Document, Label, Map};
use env::net::tcp;
use spec::channel::{Channel, Data};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::channel;
use types::name::Name;
use types::quality::Quality;
use types::sample::{Scalar, Type};
use types::time::Span;

#[path = "../scenarios.rs"]
mod scenarios;

use scenarios::{Backoff, Plant, Serve, Suite};

/// The variables of the server, each read into the channel of the same place in
/// [`CHANNELS`].
const NODES: [&str; 3] = ["ns=3;s=SpikeData", "ns=3;i=7", "ns=2;s=Dip"];
/// The data channels, on `plant.time`.
const CHANNELS: [&str; 3] = ["plant.a", "plant.b", "plant.c"];
/// The note of a run with no `read` block.
const NO_READ: &str = "the connector has no `read` block, so it reads nothing. \
                       `browse` lists the points of the server, and `import` adds a \
                       `read` block for each.";
/// The time from one change of the variables to the next.
const PERIOD: Span = Span::from_nanos(100_000_000);

const SUITE: Suite = Suite {
    kinds,
    setup: |plant| Box::pin(std::future::ready(setup(&plant))),
    serve: |inputs| Box::pin(simulate(inputs)),
    progress: "in",
    backoff: Backoff::Restart,
    refused,
    secrets: &["certificate", "key", "trust"],
    echo: None,
};

scenarios::suite!(SUITE);

fn kinds() -> Table {
    Table::new().with("opcua", Kind::default())
}

fn name(text: &str) -> Name {
    text.parse().expect("a valid name")
}

/// Defines `plant.time` and the `f64` channels of [`CHANNELS`] on it, and gives the
/// config of a connector that reads each of them at 10 Hz.
fn setup(plant: &Plant<'_>) -> Document {
    let time = channel::Key::from_u128(1);
    let index = spec::channel::Kind::Index {
        error: None,
        control: None,
    };
    let mut definitions = vec![(
        name("plant.time"),
        Definition::Channel(Channel {
            key: time,
            kind: index,
        }),
    )];
    for (key, channel) in (2..).zip(CHANNELS) {
        let sample = DataType::Sample(Type::Scalar(Scalar::F64));
        let data = Data::new(time, None, sample, None).expect("no unit");
        let kind = spec::channel::Kind::Data(data);
        let key = channel::Key::from_u128(key);
        definitions.push((name(channel), Definition::Channel(Channel { key, kind })));
    }
    plant.rig.define(definitions);
    let address = format!("opc.tcp://{}", plant.address);
    let reads = CHANNELS
        .iter()
        .zip(NODES)
        .map(|(&channel, node)| (channel, node));
    config(&address, "10Hz", reads.collect())
}

/// Serves the variables of [`NODES`] until `until`, and changes each one every
/// [`PERIOD`], so a subscription gets new values as from a real plant.
async fn simulate(inputs: Serve) {
    let listen = tcp::Listen {
        local: inputs.address,
        backlog: 4,
        options: tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("not 0"),
            delayed: false,
        },
    };
    let net = inputs.node.net();
    let listener = net.listen(&listen).expect("the port is free");
    let wall = inputs.node.wall();
    #[expect(
        clippy::disallowed_methods,
        reason = "a simulated device reads the wall time of its node"
    )]
    let now = move || wall.now().time;
    let mut rng = inputs.node.entropy().rng();
    let (clock, variables) = (inputs.node.clock(), Variables::new(&NODES));
    let (ticks, changed) = (clock.clone(), variables.clone());
    inputs.tasks.spawn(async move {
        loop {
            ticks.sleep(PERIOD).await;
            changed.change(Quality::GOOD);
        }
    });
    let served = serve(
        clock,
        now,
        net,
        listener,
        &mut rng,
        &variables,
        inputs.until,
    );
    served.await.expect("the listener holds");
}

/// Configs with a missing scheme, two `read` blocks of one channel, and a node id that
/// does not parse.
fn refused() -> Vec<(Document, Vec<Diagnostic>)> {
    let device = SocketAddr::from(([10, 0, 0, 2], 9000));
    let read = vec![("plant.a", NODES[0])];
    vec![
        (
            config(&device.to_string(), "10Hz", read),
            vec![diagnostic(
                "opcua.bad-address",
                "the address \"10.0.0.2:9000\" does not start with opc.tcp://",
                "Write an address such as \"opc.tcp://plc:4840\"",
            )],
        ),
        (
            config(
                "opc.tcp://plc:4840",
                "10Hz",
                vec![("plant.a", NODES[0]), ("plant.a", NODES[1])],
            ),
            vec![diagnostic(
                "opcua.repeated-channel",
                "the connector reads `plant.a` in another `read` block",
                "Remove one of the two `read` blocks",
            )],
        ),
        (
            config("opc.tcp://plc:4840", "10Hz", vec![("plant.a", "nope")]),
            vec![diagnostic(
                "opcua.bad-node-id",
                "a node ID is ns=<index>;i=<number> or ns=<index>;s=<name>, with an \
                 index to 65535, a number to 4294967295, and a name that is not empty",
                "Write a node ID such as \"ns=3;s=SpikeData\" or \"ns=3;i=1001\"",
            )],
        ),
    ]
}

/// A config with no `read` block checks with no diagnostic and no write.
#[test]
fn checks_a_config_with_no_read() {
    let config = config("opc.tcp://plc:4840", "10Hz", Vec::new());
    let checked = kinds().check("opcua", None, &config);
    assert_eq!(checked.map(|channels| channels.writes), Ok(Vec::new()));
}

/// A config with no `read` block runs against the device: within 10 s Running with
/// no error, no restart, no sample in, and the note that it reads nothing. Once the
/// device stops, within 30 s an error of class Device that names the device.
#[test]
fn runs_a_config_with_no_read() {
    scenarios::run(&SUITE, scenarios::direct, |scene| async move {
        let mut config = scene.setup().await;
        config.blocks.clear();
        let served = scene.serve();
        let mut run = scene.supervise(config).await;
        let noted = |reading: &Reading| reading.note.as_deref() == Some(NO_READ);
        let reading = run.until(scenarios::START, noted).await;
        let shown = (reading.state, reading.restarts, &reading.error);
        assert_eq!(shown, (State::Running, 0, &None), "{reading:?}");
        assert_eq!(reading.counts.get(&name("in")), Some(&0), "{reading:?}");
        scene.stop(served).await;
        let lost = |reading: &Reading| reading.error.is_some();
        let reading = run.until(scenarios::LOST, lost).await;
        let class = reading.error.as_ref().map(|(class, _)| *class);
        assert_eq!(class, Some(Class::Device), "{reading:?}");
        scene.named(&reading);
        run.stop().await.expect("the run ends with no error");
    });
}

fn attribute(key: &str, text: &str) -> Attribute {
    Attribute {
        key: key.into(),
        key_span: None,
        value: Value {
            kind: value::Kind::String(text.into()),
            span: None,
        },
    }
}

/// The config of a connector of `address` at `rate`, with a `read` block for each
/// channel and node of `reads`.
fn config(address: &str, rate: &str, reads: Vec<(&str, &str)>) -> Document {
    let attributes = vec![attribute("address", address), attribute("rate", rate)];
    let blocks = reads.into_iter().map(|(channel, node)| Block {
        keyword: "read".into(),
        keyword_span: None,
        labels: vec![Label {
            text: channel.into(),
            span: None,
        }],
        body: Document {
            attributes: Map::new(vec![attribute("node_id", node)]).expect("one key"),
            blocks: Vec::new(),
        },
        span: None,
    });
    Document {
        attributes: Map::new(attributes).expect("unique keys"),
        blocks: blocks.collect(),
    }
}

fn diagnostic(code: &'static str, message: &str, fix: &str) -> Diagnostic {
    Diagnostic::new(Code::new(code), None, message.into(), fix.into())
}
