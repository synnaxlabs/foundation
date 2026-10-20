//! The conformance suite of the `influx` kind.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use connector::http::sim::Scheme;
use connector::kind::Table;
use connector_influx::Kind;
use connector_influx::sim::{Store, serve};
use document::diagnostic::{Code, Diagnostic};
use document::value::{self, Value};
use document::{Attribute, Block, Document, Map};
use env::net::{Listener, tcp};
use http::{Request, Response, StatusCode};
use hub::writer::Writer;
use spec::channel::{Channel, Data};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::channel;
use types::frame::{Form, Label, Path};
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::Span;

#[path = "../scenarios.rs"]
mod scenarios;

use scenarios::{Backoff, Plant, Serve, Suite};

const SUITE: Suite = Suite {
    kinds,
    setup: |plant| Box::pin(setup(plant)),
    serve: |inputs| Box::pin(simulate(inputs)),
    progress: "confirmed",
    backoff: Backoff::Run,
    refused,
    secrets: Some(marked),
    echo: Some(|inputs| Box::pin(echo(inputs))),
};

scenarios::suite!(SUITE);

/// The time from one frame of the writer to the next.
const PERIOD: Span = Span::from_nanos(100_000_000);

/// The secret that the marked config reads its token from.
const TOKEN: &str = "conformance.token";

fn kinds() -> Table {
    Table::new().with("influx", Kind::default())
}

fn name(text: &str) -> Name {
    text.parse().expect("a valid name")
}

/// Defines `edge.time` and the `f64` channel `edge.a` on it, writes one frame of them
/// each [`PERIOD`] until `until`, and gives the config of a connector that sends
/// `edge.*` to the database `plant`.
async fn setup(plant: Plant<'_>) -> Document {
    let time = channel::Key::from_u128(1);
    let index = spec::channel::Kind::Index {
        error: None,
        control: None,
    };
    let sample = DataType::Sample(Type::Scalar(Scalar::F64));
    let data = Data::new(time, None, sample, None).expect("no unit");
    plant.rig.define(vec![
        (
            name("edge.time"),
            Definition::Channel(Channel {
                key: time,
                kind: index,
            }),
        ),
        (
            name("edge.a"),
            Definition::Channel(Channel {
                key: channel::Key::from_u128(2),
                kind: spec::channel::Kind::Data(data),
            }),
        ),
    ]);
    let channels = vec![name("edge.time"), name("edge.a")];
    let mut writer = plant.rig.writer(name("edge.writer"), channels).await;
    let (clock, until) = (plant.node.clock(), plant.until.clone());
    plant.tasks.spawn(async move {
        while !until.cancelled() {
            write(&mut writer);
            clock.sleep(PERIOD).await;
        }
    });
    config(&format!("http://{}", plant.address), Some("plant"))
}

/// Writes one sample of `edge.time` at the time of `writer`, and `1.0` to `edge.a`.
fn write(writer: &mut Writer) {
    let entries = writer.set().entries();
    let at = |key| {
        let key = channel::Key::from_u128(key);
        let at = entries.iter().position(|entry| entry.key == key);
        at.expect("written")
    };
    let (time, a) = (at(1), at(2));
    let group = entries[time].group;
    let mut series = [(time, 8), (a, 8)];
    series.sort_unstable();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    let stamp = writer.now().nanos().to_le_bytes();
    draft
        .series_mut(time)
        .expect("the index")
        .copy_from_slice(&stamp);
    let one = 1.0_f64.to_le_bytes();
    draft.series_mut(a).expect("a series").copy_from_slice(&one);
    draft.set_count(group, 1);
    writer
        .write(Label::Path(Path::Live), draft)
        .expect("the hub takes the frame");
}

/// Serves the database `plant` with no token until `until`.
async fn simulate(inputs: Serve) {
    let listener = listen(&inputs);
    let store = Arc::new(Mutex::new(Store::default()));
    let (tasks, until) = (inputs.tasks, inputs.until);
    let database = "plant".into();
    let served = serve(listener, tasks, until, Scheme::Plain, store, database, None);
    served.await.expect("the listener holds");
}

/// `config` with its token read from [`TOKEN`], whose value is `marker`. The `ca`
/// waits on #3612: the check refuses it with an `http` address.
fn marked(config: Document, marker: &str) -> scenarios::inputs::Marked {
    let token = scenarios::inputs::secret(TOKEN);
    let secrets = vec![(name(TOKEN), marker.as_bytes().to_vec())];
    (scenarios::with(config, "token", token), secrets)
}

/// Answers each request until `until` with 400 and a text of each header of the
/// request, one `<name>: <value>` line each.
async fn echo(inputs: Serve) {
    let listener = listen(&inputs);
    let answer = |request: Request<Bytes>| {
        let headers = request.headers().iter().map(|(name, value)| {
            format!("{name}: {}\n", String::from_utf8_lossy(value.as_bytes()))
        });
        let mut response = Response::new(Bytes::from(headers.collect::<String>()));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        response
    };
    let (tasks, until) = (inputs.tasks, inputs.until);
    let served =
        connector::http::sim::serve(listener, tasks, until, Scheme::Plain, answer);
    served.await.expect("the listener holds");
}

/// Listens on the address of `inputs`.
fn listen(inputs: &Serve) -> Listener {
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
    inputs.node.net().listen(&listen).expect("the port is free")
}

/// Configs with an address with no scheme, and with no database.
fn refused() -> Vec<(Document, Vec<Diagnostic>)> {
    vec![
        (
            config("influx:8086", Some("plant")),
            vec![diagnostic(
                "connector.bad-uri",
                "the scheme of the URI is not http or https",
                "Write an `http` or `https` URI such as \"http://10.0.0.2:8086\"",
            )],
        ),
        (
            config("http://influx:8086", None),
            vec![diagnostic(
                "document.missing-attribute",
                "the connector has no `database` or `bucket`",
                "Add a `bucket` attribute and an `org` attribute, such as \"plant\", \
                 or a `database` attribute for InfluxDB 1",
            )],
        ),
    ]
}

fn diagnostic(code: &'static str, message: &str, fix: &str) -> Diagnostic {
    Diagnostic::new(Code::new(code), None, message.into(), fix.into())
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

/// The config of a connector that sends `edge.*` to `address`, into `database` when
/// given, as the measurement `telemetry`.
fn config(address: &str, database: Option<&str>) -> Document {
    let mut attributes = vec![
        attribute("address", address),
        attribute("select", "edge.*"),
        attribute("measurement", "telemetry"),
    ];
    attributes.extend(database.map(|database| attribute("database", database)));
    let reader = Block {
        keyword: "reader".into(),
        keyword_span: None,
        labels: Vec::new(),
        body: Document {
            attributes: Map::new(vec![
                attribute("mode", "complete"),
                attribute("hold", "2h"),
            ])
            .expect("unique keys"),
            blocks: Vec::new(),
        },
        span: None,
    };
    Document {
        attributes: Map::new(attributes).expect("unique keys"),
        blocks: vec![reader],
    }
}
