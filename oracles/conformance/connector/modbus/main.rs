//! The conformance suite of the `modbus` kind.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::num::NonZeroUsize;
use std::sync::Mutex;

use connector::kind::Table;
use connector_modbus::Kind;
use connector_modbus::device::Device;
use connector_modbus::tcp::serve;
use document::diagnostic::{Code, Diagnostic};
use document::value::{self, Value};
use document::{Attribute, Block, Document, Label, Map};
use env::net::tcp;
use spec::channel::{Channel, Data};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::channel;
use types::name::Name;
use types::sample::{Scalar, Type};

#[path = "../scenarios.rs"]
mod scenarios;

use scenarios::{Backoff, Plant, Serve, Suite};

/// The unit of the device.
const UNIT: u8 = 1;
/// The data channels, each read from the holding register of its place.
const CHANNELS: [&str; 2] = ["plant.a", "plant.b"];

const SUITE: Suite = Suite {
    kinds,
    setup: |plant| Box::pin(std::future::ready(setup(&plant))),
    serve: |inputs| Box::pin(simulate(inputs)),
    progress: "in",
    backoff: Backoff::Restart,
    refused,
    secrets: None,
    echo: None,
};

scenarios::suite!(SUITE);

fn kinds() -> Table {
    Table::new().with("modbus", Kind::default())
}

fn name(text: &str) -> Name {
    text.parse().expect("a valid name")
}

/// Defines `plant.time` and the `u16` channels of [`CHANNELS`] on it, and gives the
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
        let sample = DataType::Sample(Type::Scalar(Scalar::U16));
        let data = Data::new(time, None, sample, None).expect("no unit");
        let kind = spec::channel::Kind::Data(data);
        let key = channel::Key::from_u128(key);
        definitions.push((name(channel), Definition::Channel(Channel { key, kind })));
    }
    plant.rig.define(definitions);
    let reads = (0..)
        .zip(CHANNELS)
        .map(|(address, channel)| (channel, address));
    config(&plant.address.to_string(), reads.collect())
}

/// Serves a device of 16 holding registers as [`UNIT`] until `until`.
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
    let listener = inputs.node.net().listen(&listen).expect("the port is free");
    let device = Mutex::new(Device {
        coils: Vec::new(),
        discrete_inputs: Vec::new(),
        holding_registers: (0..16).collect(),
        input_registers: Vec::new(),
    });
    let served = serve(listener, inputs.until, UNIT, &device).await;
    served.expect("the listener holds");
}

/// Configs with a port of 0, no `read` block, and a read address past 65535.
fn refused() -> Vec<(Document, Vec<Diagnostic>)> {
    let read = vec![("plant.a", 0)];
    vec![
        (
            config("plc:0", read.clone()),
            vec![diagnostic(
                "modbus.bad-address",
                "the address \"plc:0\" has a port that is not 1 to 65535",
                "Write a host and a port, such as \"plc:502\" or \"[fd00::1]:502\"",
            )],
        ),
        (
            config("plc:502", Vec::new()),
            vec![diagnostic(
                "modbus.no-read",
                "the connector has no `read` block",
                "Add a `read` block for each channel, such as `read \"plc.temp\" { \
                 table = \"holding\", address = 40 }`",
            )],
        ),
        (
            config("plc:502", vec![("plant.a", 70_000)]),
            vec![diagnostic(
                "modbus.bad-read-address",
                "the address 70000 is not 0 to 65535",
                "Write the address of the protocol, from 0, such as 40 for the 41st \
                 holding register",
            )],
        ),
    ]
}

fn attribute(key: &str, kind: value::Kind) -> Attribute {
    Attribute {
        key: key.into(),
        key_span: None,
        value: Value { kind, span: None },
    }
}

/// The config of a connector of `address` at 10 Hz, with a `read` block of a holding
/// register for each channel and address of `reads`.
fn config(address: &str, reads: Vec<(&str, i128)>) -> Document {
    let attributes = vec![
        attribute("address", value::Kind::String(address.into())),
        attribute("rate", value::Kind::String("10Hz".into())),
    ];
    let blocks = reads.into_iter().map(|(channel, address)| Block {
        keyword: "read".into(),
        keyword_span: None,
        labels: vec![Label {
            text: channel.into(),
            span: None,
        }],
        body: Document {
            attributes: Map::new(vec![
                attribute("table", value::Kind::String("holding".into())),
                attribute("address", value::Kind::Integer(address)),
            ])
            .expect("unique keys"),
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
