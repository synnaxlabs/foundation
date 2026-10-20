use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use connector::http::Roots;
use connector::http::sim::{Certificate, Scheme};
use connector::status::{Class, Reading};
use connector::testing::{Record, Run};
use env::clock::Clock;
use env::net::tcp;
use http::{Request, Response, StatusCode};
use hub::reader::Mode;
use spec::channel::{Channel, Data};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::authority::Authority;
use types::channel;
use types::frame::{Form, Label, Path};
use types::name::{Name, Selector};
use types::sample::{Scalar, Sides, Type};
use types::time::{Span, Stamp};

use super::*;
use crate::batch::Batch;

const PORT: u16 = 8086;
const OPTIONS: tcp::Options = tcp::Options {
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
    unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
    delayed: false,
};
/// The name, key, and type of each channel of another sample type, on `edge.time`.
const TYPED: [(&str, u128, Type); 17] = [
    ("edge.bool", 10, Type::Scalar(Scalar::Bool)),
    ("edge.i8", 11, Type::Scalar(Scalar::I8)),
    ("edge.i16", 12, Type::Scalar(Scalar::I16)),
    ("edge.i32", 13, Type::Scalar(Scalar::I32)),
    ("edge.u8", 14, Type::Scalar(Scalar::U8)),
    ("edge.u16", 15, Type::Scalar(Scalar::U16)),
    ("edge.u32", 16, Type::Scalar(Scalar::U32)),
    ("edge.u64", 17, Type::Scalar(Scalar::U64)),
    ("edge.f32", 18, Type::Scalar(Scalar::F32)),
    ("edge.stamp", 19, Type::Scalar(Scalar::Stamp)),
    ("edge.span", 20, Type::Scalar(Scalar::Span)),
    ("edge.uuid", 21, Type::Scalar(Scalar::Uuid)),
    (
        "edge.xyz",
        22,
        Type::Array {
            element: Scalar::F32,
            len: 3,
        },
    ),
    ("edge.text", 23, Type::String),
    ("edge.blob", 24, Type::Bytes),
    (
        "edge.list",
        25,
        Type::List {
            element: Scalar::F32,
            max: 4,
        },
    ),
    (
        "edge.grid",
        26,
        Type::Matrix {
            element: Scalar::F32,
            sides: Sides {
                rows: 2,
                columns: 2,
            },
        },
    ),
];
/// The file of the token of `file(TOKEN_FILE)`.
const TOKEN_FILE: &str = "/run/secrets/influx";
/// The class of a retry, in [`Plant::classes`].
const RETRY: Option<Class> = Some(Class::Retry);
/// The file of the CA of `file(CA_FILE)`.
const CA_FILE: &str = "/etc/influx/ca.pem";
/// The PEM of the CA that signs each certificate of `serve` but `OtherCa`.
const CA: &[u8] = connector::http::sim::CA.as_bytes();
/// The PEM of the CA that signs `Certificate::OtherCa`.
const OTHER_CA: &[u8] = connector::http::sim::OTHER_CA.as_bytes();
/// 2026-06-01T00:00:00Z, inside the validity of each certificate of `serve` but
/// `Expired`.
const JUNE_2026: Stamp = Stamp::from_nanos(1_780_272_000_000_000_000);
/// The key of the first of the 64 `u8` channels of [`wide_names`].
const WIDE: u128 = 300;
/// The body limit of the InfluxDB of a plant, as InfluxDB 3 Core gives by default.
const BODY_LIMIT: usize = 10 << 20;

/// One request that the InfluxDB of a plant got: its path and query, and its body.
type Got = (String, String);

/// A node with a hub that defines `edge.time` (key 1), the `f64` channels `edge.a`
/// (2) and `edge.b` (3), the `i64` channel `edge.count` (4), the `f64` channel `time`
/// (5), a channel of each type of [`TYPED`], and the status channels of `influx`, and
/// an InfluxDB that answers each request with the next answer of its list. After the
/// last, it writes the body to its store, and answers 204, or 400 when the store
/// refuses a line. It answers 413 to a body over [`BODY_LIMIT`], with no answer of its
/// list. Its store holds the secret `plant.influx_token` (`s3cret`). The hub also
/// defines the channels of [`wide_names`].
struct Plant {
    rig: Rig,
    clock: Clock,
    net: env::net::Net,
    tasks: Tasks,
    address: SocketAddr,
    got: Rc<RefCell<Vec<Got>>>,
    answers: Rc<RefCell<VecDeque<(StatusCode, &'static str)>>>,
    /// The `Authorization` header of each request.
    auth: Rc<RefCell<Vec<Option<String>>>>,
    /// The bytes that each request writes to [`TOKEN_FILE`] as it arrives, in turn,
    /// while any are left.
    tokens: Rc<RefCell<VecDeque<&'static [u8]>>>,
    paths: env::paths::Paths,
    store: Rc<RefCell<crate::sim::Store>>,
    /// Each status of `influx` since the plant opened.
    status: Record,
    /// How the InfluxDB serves.
    scheme: Scheme,
}

impl Plant {
    /// A plant whose InfluxDB serves from the start.
    async fn new(node: ::sim::node::Node, tasks: Tasks) -> Self {
        let plant = Self::unserved(node, tasks).await;
        plant.serve();
        plant
    }

    /// A plant whose InfluxDB refuses each connection until [`Plant::serve`].
    async fn unserved(node: ::sim::node::Node, tasks: Tasks) -> Self {
        Self::with(node, tasks, Scheme::Plain, Roots::default()).await
    }

    /// A plant whose InfluxDB serves TLS with `certificate` from the start, at the
    /// name `server` of [`run_tls`], and whose node trusts `roots`.
    async fn tls(
        node: ::sim::node::Node,
        tasks: Tasks,
        certificate: Certificate,
        roots: Roots,
    ) -> Self {
        let plant = Self::with(node, tasks, Scheme::Tls(certificate), roots).await;
        plant.serve();
        plant
    }

    async fn with(
        node: ::sim::node::Node,
        tasks: Tasks,
        scheme: Scheme,
        roots: Roots,
    ) -> Self {
        let address = SocketAddr::new(node.addresses()[0], PORT);
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
        };
        let kinds = Table::new().with("influx", Kind);
        let rig = Rig::trusting(env, node.net(), kinds, roots).await;
        let token = "plant.influx_token".parse().expect("a name");
        testing::create_secret(&node.files(), &token, b"s3cret").await;
        define(&rig);
        let config = config_at(address, scheme);
        let names = rig.define_status("influx", &name("influx"), &config);
        let status = rig.record(&names).await;
        Self {
            rig,
            clock: node.clock(),
            net: node.net(),
            tasks,
            address,
            got: Rc::default(),
            answers: Rc::default(),
            auth: Rc::default(),
            tokens: Rc::default(),
            paths: node.paths(),
            store: Rc::default(),
            status,
            scheme,
        }
    }

    /// Starts the InfluxDB of the plant.
    fn serve(&self) {
        let listen = tcp::Listen {
            local: self.address,
            backlog: 4,
            options: OPTIONS,
        };
        let listener = self.net.listen(&listen).expect("the port is free");
        let (got, next, store) = (
            Rc::clone(&self.got),
            Rc::clone(&self.answers),
            Rc::clone(&self.store),
        );
        let (auth, tokens, paths) = (
            Rc::clone(&self.auth),
            Rc::clone(&self.tokens),
            self.paths.clone(),
        );
        let answer = move |request: Request<Bytes>| {
            let body = String::from_utf8(request.body().to_vec()).expect("UTF-8");
            got.borrow_mut().push((request.uri().to_string(), body));
            let header = request.headers().get(http::header::AUTHORIZATION);
            auth.borrow_mut()
                .push(header.map(|header| header.to_str().expect("ASCII").to_owned()));
            if let Some(token) = tokens.borrow_mut().pop_front() {
                let path = std::path::Path::new(TOKEN_FILE);
                paths.write(path, token).expect("writes the token");
            }
            let large = request.body().len() > BODY_LIMIT;
            let next = if large {
                Some((StatusCode::PAYLOAD_TOO_LARGE, "request too large"))
            } else {
                next.borrow_mut().pop_front()
            };
            let next = next.map(|(status, text)| (status, text.to_owned()));
            let (status, text) = next.unwrap_or_else(|| {
                match store.borrow_mut().write(request.body()) {
                    Ok(()) => (StatusCode::NO_CONTENT, String::new()),
                    Err(error) => (StatusCode::BAD_REQUEST, error.to_string()),
                }
            });
            let mut response = Response::new(Bytes::from(text));
            *response.status_mut() = status;
            response
        };
        let tasks = self.tasks.clone();
        let scheme = self.scheme;
        self.tasks.spawn(async move {
            let until = cancel::Token::new();
            let serve =
                connector::http::sim::serve(listener, tasks, until, scheme, answer);
            panic!("the listener failed: {:?}", serve.await);
        });
    }

    /// Starts [`crate::sim::serve`] with the database `database` in place of the
    /// InfluxDB of [`Plant::serve`], and gives its store.
    fn simulate(&self, database: &str) -> Arc<Mutex<crate::sim::Store>> {
        let listen = tcp::Listen {
            local: self.address,
            backlog: 4,
            options: OPTIONS,
        };
        let listener = self.net.listen(&listen).expect("the port is free");
        let store = Arc::default();
        let (tasks, held) = (self.tasks.clone(), Arc::clone(&store));
        let (scheme, database) = (self.scheme, database.to_owned());
        self.tasks.spawn(async move {
            let until = cancel::Token::new();
            let serve =
                crate::sim::serve(listener, tasks, until, scheme, held, database, None);
            panic!("the listener failed: {:?}", serve.await);
        });
        store
    }

    /// The config of an influx connector at this plant's InfluxDB.
    fn config(&self) -> Document {
        config_at(self.address, self.scheme)
    }

    /// Supervises the connector `influx` of `config` through the rig.
    async fn supervise(&self, config: Document) -> Run {
        self.rig.supervise("influx", name("influx"), config).await
    }

    async fn writer(&self, channels: &[&str]) -> hub::writer::Writer {
        let config = hub::writer::Config {
            subject: name("edge.writer"),
            authority: Authority::ABSOLUTE,
            lease: None,
            channels: channels.iter().map(|channel| name(channel)).collect(),
        };
        self.rig.hub().writer(config).await.expect("opens")
    }

    /// Each text of `influx.status.error`, as [`changes`] gives them.
    fn errors(&self) -> Vec<String> {
        changes(&self.status.readings(), error)
    }

    /// Each class of the error of `influx`, as [`changes`] gives them.
    fn classes(&self) -> Vec<Option<Class>> {
        changes(&self.status.readings(), |reading| {
            reading.error.as_ref().map(|(class, _)| *class)
        })
    }

    /// Each value of `influx.status.restarts`, as [`changes`] gives them.
    fn restarts(&self) -> Vec<u64> {
        changes(&self.status.readings(), |reading| reading.restarts)
    }

    /// Each value of the count `influx.status.<count>`, as [`changes`] gives them.
    fn count(&self, count: &str) -> Vec<u64> {
        let count = name(count);
        changes(&self.status.readings(), |reading| reading.counts[&count])
    }

    fn got(&self) -> Vec<Got> {
        self.got.borrow().clone()
    }
}

/// The config of an influx connector at an InfluxDB on `address` that serves with
/// `scheme`.
fn config_at(address: SocketAddr, scheme: Scheme) -> Document {
    match scheme {
        Scheme::Tls(_) => config(string(&format!("https://server:{PORT}"))),
        Scheme::Plain => config(string(&format!("http://{address}"))),
    }
}

fn name(text: &str) -> Name {
    text.parse().expect("a valid name")
}

fn define(rig: &Rig) {
    let time = channel::Key::from_u128(1);
    let on = |index, key, data_type| {
        let data_type = DataType::Sample(data_type);
        let data = Data::new(index, None, data_type, None).expect("no unit");
        let key = channel::Key::from_u128(key);
        let kind = spec::channel::Kind::Data(data);
        Definition::Channel(Channel { key, kind })
    };
    let data = |key, scalar| on(time, key, Type::Scalar(scalar));
    let site = |key| on(channel::Key::from_u128(6), key, Type::Scalar(Scalar::F64));
    let index = spec::channel::Kind::Index {
        error: None,
        control: None,
    };
    let mut definitions = vec![
        (
            name("edge.time"),
            Definition::Channel(Channel {
                key: time,
                kind: index.clone(),
            }),
        ),
        (name("edge.a"), data(2, Scalar::F64)),
        (name("edge.b"), data(3, Scalar::F64)),
        (name("edge.count"), data(4, Scalar::I64)),
        (name("time"), data(5, Scalar::F64)),
        (
            name("site.time"),
            Definition::Channel(Channel {
                key: channel::Key::from_u128(6),
                kind: index,
            }),
        ),
        (name("site.c"), site(7)),
    ];
    let typed =
        TYPED.map(|(text, key, data_type)| (name(text), on(time, key, data_type)));
    definitions.extend(typed);
    let wide = (WIDE..).zip(wide_names());
    definitions.extend(
        wide.map(|(key, text)| (name(&text), on(time, key, Type::Scalar(Scalar::U8)))),
    );
    rig.define(definitions);
}

/// The value of `field` in each of `readings`, with no value equal to the one before
/// it.
fn changes<T: PartialEq>(
    readings: &[Reading],
    field: impl Fn(&Reading) -> T,
) -> Vec<T> {
    let mut values: Vec<T> = Vec::new();
    for value in readings.iter().map(field) {
        if values.last() != Some(&value) {
            values.push(value);
        }
    }
    values
}

/// The text of `influx.status.error` in `reading`.
fn error(reading: &Reading) -> String {
    reading
        .error
        .as_ref()
        .map_or_else(String::new, |(_, text)| text.clone())
}

/// Writes one frame of `edge.a` and `edge.b` through `writer`, one sample time for
/// each of `samples`, at `first` and each nanosecond after.
fn write(writer: &mut hub::writer::Writer, first: i64, samples: &[(f64, f64)]) {
    let a: Vec<_> = samples.iter().map(|&(a, _)| a).collect();
    let b: Vec<_> = samples.iter().map(|&(_, b)| b).collect();
    write_columns(writer, first, &[(2, &a), (3, &b)]);
}

/// Writes one frame through `writer` with the samples of each channel key of
/// `columns`, which each hold the same count, at `first` and each nanosecond after.
fn write_columns(
    writer: &mut hub::writer::Writer,
    first: i64,
    columns: &[(u128, &[f64])],
) {
    let outcomes = write_draft(writer, first, columns);
    let applied = outcomes
        .iter()
        .all(|outcome| matches!(outcome, hub::home::Outcome::Applied { .. }));
    assert!(applied, "the frame applies: {outcomes:?}");
}

/// Writes one frame as [`write_columns`] does, and gives the outcome of each group.
fn write_draft(
    writer: &mut hub::writer::Writer,
    first: i64,
    columns: &[(u128, &[f64])],
) -> Vec<hub::home::Outcome> {
    let count = columns.first().map_or(0, |(_, samples)| samples.len());
    let bytes: Vec<(u128, Vec<u8>)> = (columns.iter())
        .map(|&(key, samples)| {
            (key, samples.iter().flat_map(|s| s.to_le_bytes()).collect())
        })
        .collect();
    let raw: Vec<(u128, &[u8])> = bytes
        .iter()
        .map(|(key, bytes)| (*key, bytes.as_slice()))
        .collect();
    write_raw(writer, first, count, &raw)
}

/// Writes one frame through `writer` of `count` sample times at `first` and each
/// nanosecond after, with the raw samples of each channel key of `columns`, and gives
/// the outcome of each group.
fn write_raw(
    writer: &mut hub::writer::Writer,
    first: i64,
    count: usize,
    columns: &[(u128, &[u8])],
) -> Vec<hub::home::Outcome> {
    let entries = writer.set().entries();
    let at = |key| {
        let key = channel::Key::from_u128(key);
        entries
            .iter()
            .position(|entry| entry.key == key)
            .expect("written")
    };
    let time = at(1);
    let group = entries[time].group;
    let mut series: Vec<_> = std::iter::once((time, count * 8))
        .chain(columns.iter().map(|&(key, bytes)| (at(key), bytes.len())))
        .collect();
    series.sort_unstable();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    let times = (first..).take(count).flat_map(i64::to_le_bytes);
    fill(draft.series_mut(time).expect("the index"), times);
    for &(key, bytes) in columns {
        fill(
            draft.series_mut(at(key)).expect("a series"),
            bytes.iter().copied(),
        );
    }
    draft.set_count(group, u32::try_from(count).expect("fits"));
    let outcomes = writer.write(Label::Path(Path::Live), draft);
    outcomes.expect("the hub takes the frame").to_vec()
}

/// Writes one frame of `edge.a` and `edge.b` at `first` and the 2 nanoseconds after,
/// which is lost for no room, and gives the seqs it lost. Takes drafts from the pool
/// until it has no room, frees the last, writes, and frees the rest.
fn write_lost(writer: &mut hub::writer::Writer, first: i64) -> std::ops::Range<u64> {
    let index = (writer.set().entries().iter())
        .position(|entry| entry.key == channel::Key::from_u128(1))
        .expect("written");
    let mut drafts = Vec::new();
    let mut len: usize = 1 << 23;
    while len > 0 {
        while let Ok(draft) = writer.draft(Form::Raw, &[(index, len)]) {
            drafts.push(draft);
        }
        len -= len.div_ceil(16);
    }
    drop(drafts.pop());
    let outcomes = write_draft(writer, first, &[(2, &[1.0; 3]), (3, &[2.0; 3])]);
    let [hub::home::Outcome::Lost { range, .. }] = outcomes[..] else {
        panic!("the frame is lost for no room: {outcomes:?}");
    };
    range.seq..range.end().expect("an end")
}

/// Writes one frame of only the index `edge.time`, of `count` samples at `first` and
/// each nanosecond after.
fn write_times(writer: &mut hub::writer::Writer, first: i64, count: usize) {
    let time = (writer.set().entries().iter())
        .position(|entry| entry.key == channel::Key::from_u128(1))
        .expect("written");
    let group = writer.set().entries()[time].group;
    let mut draft = writer
        .draft(Form::Raw, &[(time, count * 8)])
        .expect("a frame");
    let times = (first..).take(count).flat_map(i64::to_le_bytes);
    fill(draft.series_mut(time).expect("the index"), times);
    draft.set_count(group, u32::try_from(count).expect("fits"));
    let outcomes = writer.write(Label::Path(Path::Live), draft);
    let outcomes = outcomes.expect("the hub takes the frame");
    let applied = outcomes
        .iter()
        .all(|outcome| matches!(outcome, hub::home::Outcome::Applied { .. }));
    assert!(applied, "the frame applies: {outcomes:?}");
}

fn fill(series: &mut [u8], bytes: impl Iterator<Item = u8>) {
    for (to, from) in series.iter_mut().zip(bytes) {
        *to = from;
    }
}

/// The line of `edge.a` and `edge.b` at `time`, as the kind writes it.
fn line(a: &str, b: &str, time: i64) -> String {
    format!("telemetry edge.a={a},edge.b={b} {time}\n")
}

#[test]
fn writes_one_line_for_each_sample_time_to_its_database() {
    let got = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let run = plant
            .supervise(with(plant.config(), "database", &string("plant a/b")))
            .await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        let samples = [(1.5, 2.0), (f64::NAN, 4.0), (f64::NAN, f64::INFINITY)];
        write(&mut writer, first, &samples);
        plant.clock.sleep(Span::SECOND).await;
        run.stop().await.expect("ok after a cancel");
        (plant.got(), first)
    });
    let (got, first) = got;
    let body =
        line("1.5e0", "2e0", first) + &format!("telemetry edge.b=4e0 {}\n", first + 1);
    assert_eq!(
        got,
        vec![("/write?db=plant+a%2Fb&precision=ns".into(), body)]
    );
}

const WRITE: &str = "/write?db=plant&precision=ns";

#[test]
fn writes_to_a_bucket_in_its_org() {
    let got = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let target = [(60, "bucket", string("edge/1")), (62, "org", string("a b"))];
        let run = plant
            .supervise(plus(without(plant.config(), "database"), &target))
            .await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::SECOND).await;
        run.stop().await.expect("ok after a cancel");
        (plant.got(), first)
    });
    let (got, first) = got;
    let uri = "/api/v2/write?org=a+b&bucket=edge%2F1&precision=ns";
    assert_eq!(got, vec![(uri.into(), line("1e0", "2e0", first))]);
}

/// Each byte of `/` takes three bytes in the query, the most that one byte can take.
#[test]
fn writes_to_a_bucket_and_an_org_of_the_longest_names() {
    let longest = "/".repeat(4_096);
    let (got, first) = run(move |node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let target = [
            (60, "bucket", string(&longest)),
            (62, "org", string(&longest)),
        ];
        let run = plant
            .supervise(plus(without(plant.config(), "database"), &target))
            .await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::SECOND).await;
        run.stop().await.expect("ok after a cancel");
        (plant.got(), first)
    });
    let encoded = "%2F".repeat(4_096);
    let uri = format!("/api/v2/write?org={encoded}&bucket={encoded}&precision=ns");
    assert_eq!(got, vec![(uri, line("1e0", "2e0", first))]);
}

/// The query of the writer and the decode of [`crate::sim::serve`] agree on names
/// with a space, `&`, and `=`.
#[test]
fn writes_to_the_simulated_influxdb_by_a_name_with_a_space_an_ampersand_and_an_equals()
{
    const NAME: &str = "a b&c=d";
    let database = |plant: &Plant| with(plant.config(), "database", &string(NAME));
    let bucket = |plant: &Plant| {
        let target = [(60, "bucket", string(NAME)), (62, "org", string("o p&q=r"))];
        plus(without(plant.config(), "database"), &target)
    };
    for config in [database, bucket] {
        let (times, first) = run(move |node, tasks| async move {
            let plant = Plant::unserved(node, tasks).await;
            let store = plant.simulate(NAME);
            let run = plant.supervise(config(&plant)).await;
            let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
            plant.clock.sleep(Span::SECOND).await;
            let first = writer.now().nanos();
            write(&mut writer, first, &[(1.0, 2.0)]);
            plant.clock.sleep(Span::SECOND).await;
            run.stop().await.expect("ok after a cancel");
            let store = store.lock().expect("no panic under the lock");
            let points = store.points("telemetry", &[]);
            let times: Vec<_> = points.map(|point| point.time.nanos()).collect();
            (times, first)
        });
        assert_eq!(times, [first]);
    }
}

/// The fix in the text of a 400 or a 422.
const REFUSED: &str = "For a field type conflict, write to another `measurement` or \
                       delete the old field in InfluxDB. For points older than the \
                       retention period, raise it or write to another bucket";
/// The fix in the text of a 404.
const UNKNOWN: &str =
    "Create the database, the bucket, or the org in InfluxDB, or name one that exists";

/// The fix in the text of a 413.
const LARGE: &str = "Raise the request size limit of InfluxDB \
                     (`--max-http-request-size` of InfluxDB 3, `max-body-size` of \
                     InfluxDB 1), or write fewer channels or shorter samples at each \
                     sample time";

/// An answer of InfluxDB 1 to a batch of bad lines, of 65537 bytes or more.
fn large() -> String {
    let line = "unable to parse 'telemetry a=': missing field value\n";
    line.repeat((1 << 16) / line.len() + 1)
}

/// The status text of the ASCII error `full`, over 1024 bytes.
fn shown(full: &str) -> String {
    let head: String = full.chars().take(510).collect();
    let tail: String = full.chars().skip(full.len() - 510).collect();
    format!("{head}…{tail}")
}

#[test]
fn sends_a_batch_again_after_each_other_answer_and_shows_the_error_until_a_204() {
    let (got, errors, confirmed, points) = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let large: &'static str = large().leak();
        plant.answers.borrow_mut().extend([
            (StatusCode::INTERNAL_SERVER_ERROR, " the disk is full \n"),
            (StatusCode::SERVICE_UNAVAILABLE, "starting"),
            (StatusCode::UNAUTHORIZED, "unauthorized access"),
            (StatusCode::PAYLOAD_TOO_LARGE, "request too large"),
            (StatusCode::TOO_MANY_REQUESTS, "slow down"),
            (StatusCode::BAD_REQUEST, "field type conflict"),
            (StatusCode::UNPROCESSABLE_ENTITY, "beyond retention policy"),
            (StatusCode::NOT_FOUND, "database not found: \"plant\""),
            (StatusCode::BAD_REQUEST, large),
            (StatusCode::NOT_FOUND, large),
            (StatusCode::OK, "ok"),
        ]);
        let run = plant.supervise(plant.config()).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::from_nanos(600_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        let got = (plant.got().into_iter())
            .map(|(uri, body)| (uri, body == line("1e0", "2e0", first)))
            .collect::<Vec<_>>();
        let points = plant.store.borrow().points("telemetry", &[]).count();
        let confirmed = plant.count("confirmed");
        (got, plant.errors(), confirmed, points)
    });
    let head = large().chars().take(1 << 16).collect::<String>();
    let head = head.trim();
    assert_eq!(got, vec![(WRITE.to_owned(), true); 12]);
    let at = |answer: &str| format!("InfluxDB at 10.0.0.1:8086 answered {answer}");
    assert_eq!(
        errors,
        [
            String::new(),
            at("500 Internal Server Error: the disk is full"),
            at("503 Service Unavailable: starting"),
            at("401 Unauthorized: unauthorized access"),
            at(&format!(
                "413 Payload Too Large: request too large. {LARGE}"
            )),
            at("429 Too Many Requests: slow down"),
            at(&format!("400 Bad Request: field type conflict. {REFUSED}")),
            at(&format!(
                "422 Unprocessable Entity: beyond retention policy. {REFUSED}"
            )),
            at(&format!(
                "404 Not Found: database not found: \"plant\". {UNKNOWN}"
            )),
            shown(&at(&format!("400 Bad Request: {head}. {REFUSED}"))),
            shown(&at(&format!("404 Not Found: {head}. {UNKNOWN}"))),
            at("200 OK: ok"),
            String::new(),
        ]
    );
    assert_eq!((confirmed, points), (vec![0, 2], 1));
}

/// The config of `plant` with `token` set to a call of `function` with `argument`.
fn with_token(plant: &Plant, function: &str, argument: &str) -> Document {
    plus(plant.config(), &[(60, "token", call(function, argument))])
}

/// Runs `config` on `plant` with one sample written, until 20 s after it, and gives
/// the `Authorization` header of each request, the errors, and the end of the run.
async fn send_one(
    plant: &Plant,
    config: Document,
) -> (Vec<Option<String>>, Vec<String>, Option<Result<(), Error>>) {
    let run = plant.supervise(config).await;
    let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
    plant.clock.sleep(Span::SECOND).await;
    let first = writer.now().nanos();
    write(&mut writer, first, &[(1.0, 2.0)]);
    plant.clock.sleep(Span::from_nanos(20_000_000_000)).await;
    let end = run.end().await;
    // The record takes the last status after the home applied it.
    plant.clock.sleep(Span::SECOND).await;
    let auth = plant.auth.borrow().clone();
    (auth, plant.errors(), end)
}

#[test]
fn sends_the_token_of_a_store_or_a_file_in_each_request() {
    let got = [("secret", "plant.influx_token"), ("file", TOKEN_FILE)].map(
        |(function, argument)| {
            run(move |node, tasks| async move {
                node.paths()
                    .write(std::path::Path::new(TOKEN_FILE), b"f1le\n")
                    .expect("writes");
                let plant = Plant::new(node, tasks).await;
                let config = with_token(&plant, function, argument);
                let (auth, errors, end) = send_one(&plant, config).await;
                let points = plant.store.borrow().points("telemetry", &[]).count();
                (auth, errors, end.is_none(), points)
            })
        },
    );
    assert_eq!(
        got,
        [
            (
                vec![Some("Token s3cret".to_owned())],
                vec![String::new()],
                true,
                1
            ),
            (
                vec![Some("Token f1le".to_owned())],
                vec![String::new()],
                true,
                1
            ),
        ]
    );
}

#[test]
fn reads_the_token_again_after_a_401_or_a_403_and_writes_each_sample_once() {
    for (status, text) in [
        (StatusCode::UNAUTHORIZED, "401 Unauthorized"),
        (StatusCode::FORBIDDEN, "403 Forbidden"),
    ] {
        let got = run(move |node, tasks| async move {
            let plant = Plant::new(node.clone(), tasks).await;
            node.paths()
                .write(std::path::Path::new(TOKEN_FILE), b"old")
                .expect("writes");
            let body = r#"{"code":"unauthorized","message":"unauthorized access"}"#;
            plant.answers.borrow_mut().push_back((status, body));
            plant.tokens.borrow_mut().push_back(b"new\n");
            let config = with_token(&plant, "file", TOKEN_FILE);
            let (auth, errors, end) = send_one(&plant, config).await;
            (
                auth,
                errors,
                end.is_none(),
                plant.store.borrow().points("telemetry", &[]).count(),
            )
        });
        assert_eq!(
            got,
            (
                vec![Some("Token old".to_owned()), Some("Token new".to_owned())],
                vec![
                    String::new(),
                    format!(
                        "InfluxDB at 10.0.0.1:8086 answered {text}. Check the token \
                         file(\"{TOKEN_FILE}\"): {{\"code\":\"unauthorized\",\
                         \"message\":\"unauthorized access\"}}"
                    ),
                    String::new(),
                ],
                true,
                1,
            ),
            "{status}"
        );
    }
}

#[test]
fn the_error_of_an_answer_other_than_204_names_the_address() {
    let (address, errors, restarts) = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        (plant.answers.borrow_mut())
            .push_back((StatusCode::SERVICE_UNAVAILABLE, "starting"));
        let (_, errors, end) = send_one(&plant, plant.config()).await;
        assert!(end.is_none(), "the run goes on");
        let restarts = plant.restarts();
        (plant.address.to_string(), errors, restarts)
    });
    assert_eq!(restarts, vec![0], "the Run form: no restart");
    let shown: Vec<&String> = errors.iter().filter(|e| !e.is_empty()).collect();
    assert!(!shown.is_empty(), "{errors:?}");
    for error in shown {
        assert!(
            error.contains(&address),
            "the error names {address}: {error:?}"
        );
    }
}

#[test]
fn keeps_the_token_after_an_answer_other_than_401_or_403() {
    let got = run(|node, tasks| async move {
        let plant = Plant::new(node.clone(), tasks).await;
        node.paths()
            .write(std::path::Path::new(TOKEN_FILE), b"old")
            .expect("writes");
        (plant.answers.borrow_mut())
            .push_back((StatusCode::INTERNAL_SERVER_ERROR, "the disk is full"));
        plant.tokens.borrow_mut().push_back(b"new");
        let config = with_token(&plant, "file", TOKEN_FILE);
        let (auth, errors, end) = send_one(&plant, config).await;
        let points = plant.store.borrow().points("telemetry", &[]).count();
        (auth, errors, end.is_none(), points)
    });
    assert_eq!(
        got,
        (
            vec![Some("Token old".to_owned()); 2],
            vec![
                String::new(),
                "InfluxDB at 10.0.0.1:8086 answered 500 Internal Server Error: the \
                 disk is full"
                    .to_owned(),
                String::new(),
            ],
            true,
            1,
        )
    );
}

#[test]
fn reads_the_token_again_after_a_401_whose_body_is_over_the_cap() {
    let got = run(move |node, tasks| async move {
        let plant = Plant::new(node.clone(), tasks).await;
        (node.paths())
            .write(std::path::Path::new(TOKEN_FILE), b"old")
            .expect("writes");
        let body: &'static str = large().leak();
        (plant.answers.borrow_mut()).push_back((StatusCode::UNAUTHORIZED, body));
        plant.tokens.borrow_mut().push_back(b"new\n");
        let config = with_token(&plant, "file", TOKEN_FILE);
        let (auth, errors, end) = send_one(&plant, config).await;
        (auth, errors, end.is_none())
    });
    let head = large().chars().take(1 << 16).collect::<String>();
    let full = format!(
        "InfluxDB at 10.0.0.1:8086 answered 401 Unauthorized. Check the token \
         file({TOKEN_FILE:?}): {}",
        head.trim()
    );
    assert_eq!(
        got,
        (
            vec![Some("Token old".to_owned()), Some("Token new".to_owned())],
            vec![String::new(), shown(&full), String::new()],
            true,
        )
    );
}

#[test]
fn keeps_the_source_of_the_token_after_a_long_401_body() {
    let got = run(|node, tasks| async move {
        let plant = Plant::new(node.clone(), tasks).await;
        node.paths()
            .write(std::path::Path::new(TOKEN_FILE), b"old")
            .expect("writes");
        let body: &'static str = "x".repeat(1100).leak();
        plant
            .answers
            .borrow_mut()
            .push_back((StatusCode::UNAUTHORIZED, body));
        plant.tokens.borrow_mut().push_back(b"new");
        let config = with_token(&plant, "file", TOKEN_FILE);
        let (_, errors, _) = send_one(&plant, config).await;
        errors
    });
    let head = format!(
        "InfluxDB at 10.0.0.1:8086 answered 401 Unauthorized. Check the token \
         file(\"{TOKEN_FILE}\"): "
    );
    let shown = format!("{head}{}…{}", "x".repeat(510 - head.len()), "x".repeat(510));
    assert_eq!(got, [String::new(), shown, String::new()]);
}

#[test]
fn keeps_the_answer_of_influx_after_a_401_with_a_long_source_and_body() {
    let path: &'static str = format!("/run/secrets/{}", "\u{1}".repeat(200)).leak();
    let body: &'static str = "y".repeat(600).leak();
    let got = run(move |node, tasks| async move {
        let plant = Plant::new(node.clone(), tasks).await;
        node.paths()
            .write(std::path::Path::new(path), b"old")
            .expect("writes");
        (plant.answers.borrow_mut()).push_back((StatusCode::UNAUTHORIZED, body));
        let config = with_token(&plant, "file", path);
        let (auth, errors, _) = send_one(&plant, config).await;
        (auth, errors)
    });
    let full = format!(
        "InfluxDB at 10.0.0.1:8086 answered 401 Unauthorized. Check the token \
         file({path:?}): {body}"
    );
    let head: String = full.chars().take(510).collect();
    let tail: String = full.chars().skip(full.len() - 510).collect();
    let want = format!("{head}…{tail}");
    assert!(want.starts_with(
        "InfluxDB at 10.0.0.1:8086 answered 401 Unauthorized. Check the token \
         file(\"/run/secrets/"
    ));
    assert!(want.ends_with(&"y".repeat(510)));
    assert_eq!(
        got,
        (
            vec![Some("Token old".to_owned()); 2],
            vec![String::new(), want, String::new()]
        )
    );
}

/// Runs a plant with no `hold` whose InfluxDB answers the first request 401, at which
/// the token file holds `bad`, and `new` from `fixed` after that request. Writes a
/// sample, and a second one 3 s later. Asserts that the run stores both with no
/// restart, and gives the `Authorization` header of each request, the errors, and the
/// classes.
fn after_a_bad_token(
    bad: &'static [u8],
    fixed: Span,
) -> (Vec<Option<String>>, Vec<String>, Vec<Option<Class>>) {
    run(move |node, tasks| async move {
        let plant = Plant::new(node.clone(), tasks).await;
        let paths = node.paths();
        paths
            .write(std::path::Path::new(TOKEN_FILE), b"old")
            .expect("writes");
        plant
            .answers
            .borrow_mut()
            .push_back((StatusCode::UNAUTHORIZED, "no"));
        plant.tokens.borrow_mut().push_back(bad);
        let (clock, auth) = (plant.clock.clone(), Rc::clone(&plant.auth));
        plant.tasks.spawn(async move {
            while auth.borrow().is_empty() {
                clock.sleep(Span::from_nanos(1_000)).await;
            }
            clock.sleep(fixed).await;
            paths
                .write(std::path::Path::new(TOKEN_FILE), b"new")
                .expect("writes");
        });
        let mut config = with_token(&plant, "file", TOKEN_FILE);
        config.blocks.clear();
        let run = plant.supervise(config).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        for _ in 0..2 {
            plant.clock.sleep(Span::SECOND).await;
            let first = writer.now().nanos();
            write(&mut writer, first, &[(1.0, 2.0)]);
            plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        }
        plant.clock.sleep(Span::MINUTE).await;
        let end = run.stop().await;
        assert!(end.is_ok(), "ok after a cancel");
        let points = plant.store.borrow().points("telemetry", &[]).count();
        let restarts = plant.restarts();
        assert_eq!((points, restarts), (2, vec![0]), "the run keeps the batch");
        let classes = plant.classes();
        let auth = plant.auth.borrow().clone();
        let errors = plant.errors();
        (auth, errors, classes)
    })
}

#[test]
fn keeps_the_batch_when_the_token_is_gone_after_a_401() {
    assert_eq!(
        after_a_bad_token(b"", Span::from_nanos(4_000_000_000)),
        (
            vec![
                Some("Token old".to_owned()),
                Some("Token new".to_owned()),
                Some("Token new".to_owned()),
            ],
            vec![
                String::new(),
                format!(
                    "InfluxDB at 10.0.0.1:8086 answered 401 Unauthorized. Check the \
                     token file(\"{TOKEN_FILE}\"): no"
                ),
                format!(
                    "an attempt failed: the secret file(\"{TOKEN_FILE}\"): \
                     the file {TOKEN_FILE} is empty. Write the value to it"
                ),
                String::new(),
            ],
            vec![None, RETRY, None],
        )
    );
}

#[test]
fn reads_the_token_again_only_after_the_backoff() {
    let (_, errors, _) = after_a_bad_token(b"", Span::from_nanos(1_000_000));
    assert_eq!(
        errors,
        [
            String::new(),
            format!(
                "InfluxDB at 10.0.0.1:8086 answered 401 Unauthorized. Check the token \
                 file(\"{TOKEN_FILE}\"): no"
            ),
            String::new(),
        ],
        "the token is fixed in the backoff"
    );
}

#[test]
fn keeps_the_batch_at_a_token_that_a_header_cannot_hold_after_a_401() {
    let (auth, errors, _) =
        after_a_bad_token(b"t0\nken\n", Span::from_nanos(4_000_000_000));
    assert_eq!(
        (auth, errors.get(2)),
        (
            vec![
                Some("Token old".to_owned()),
                Some("Token new".to_owned()),
                Some("Token new".to_owned()),
            ],
            Some(&format!(
                "an attempt failed: the token file(\"{TOKEN_FILE}\") holds a byte that \
                 an HTTP header cannot hold. Write the token with no control byte"
            )),
        )
    );
}

#[test]
fn ends_the_run_at_a_token_that_a_header_cannot_hold() {
    let got = run(|node, tasks| async move {
        node.paths()
            .write(std::path::Path::new(TOKEN_FILE), b"t0\nken\n")
            .expect("writes");
        let plant = Plant::new(node, tasks).await;
        let config = with_token(&plant, "file", TOKEN_FILE);
        let (auth, errors, _) = send_one(&plant, config).await;
        let classes = plant.classes();
        (auth, errors, classes)
    });
    assert_eq!(
        got,
        (
            vec![],
            vec![
                String::new(),
                format!(
                    "the token file(\"{TOKEN_FILE}\") holds a byte that an HTTP header \
                     cannot hold. Write the token with no control byte"
                ),
            ],
            vec![None, RETRY],
        )
    );
}

/// The last error of a run with a token in a file at a path of 200 control bytes,
/// which holds `value`.
fn last_error_of_a_long_source(value: &'static [u8]) -> String {
    let path: &'static str = format!("/run/secrets/{}", "\u{1}".repeat(200)).leak();
    let errors = run(move |node, tasks| async move {
        node.paths()
            .write(std::path::Path::new(path), value)
            .expect("writes");
        let plant = Plant::new(node, tasks).await;
        let config = with_token(&plant, "file", path);
        send_one(&plant, config).await.1
    });
    errors.last().cloned().expect("an error")
}

#[test]
fn keeps_the_reason_of_a_bad_token_with_a_long_source() {
    let got = last_error_of_a_long_source(b"t0\nken\n");
    assert!(got.starts_with("the token file(\"/run/secrets/"), "{got}");
    assert!(
        got.ends_with("cannot hold. Write the token with no control byte"),
        "{got}"
    );
}

#[test]
fn keeps_the_reason_of_an_empty_token_file_with_a_long_source() {
    let got = last_error_of_a_long_source(b"");
    assert!(got.starts_with("the secret file(\"/run/secrets/"), "{got}");
    assert!(got.ends_with("is empty. Write the value to it"), "{got}");
}

#[test]
fn confirms_the_samples_of_each_batch_after_its_204_and_no_nan_or_infinity() {
    let confirmed = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let run = plant.supervise(plant.config()).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        let samples = [(1.0, 2.0), (f64::NAN, 3.0), (f64::NAN, f64::INFINITY)];
        write(&mut writer, first, &samples);
        plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        write(&mut writer, first + 3, &[(4.0, 5.0)]);
        plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        plant.count("confirmed")
    });
    assert_eq!(confirmed, [0, 3, 5]);
}

#[test]
fn keeps_the_confirmed_count_across_a_restart_of_the_run() {
    let confirmed = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let run = plant.supervise(plant.config()).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        let open = hub::reader::Config {
            select: Selector::new(["edge.a"]).expect("a selector"),
            mode: Mode::Complete,
            subject: name("influx"),
            name: Some(name("influx")),
            hold: Span::ZERO,
        };
        let replaced = plant
            .rig
            .hub()
            .reader(open)
            .await
            .expect("the reader opens");
        plant.clock.sleep(Span::from_nanos(5_000_000_000)).await;
        drop(replaced);
        write(&mut writer, first + 1, &[(3.0, 4.0)]);
        plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        plant.count("confirmed")
    });
    assert_eq!(confirmed, [0, 2, 4]);
}

#[test]
fn writes_the_gap_line_of_a_run_lost_for_no_room_and_counts_its_seqs_after_a_204() {
    let (lost, stamp, bodies, counts) = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let run = plant.supervise(plant.config()).await;
        drop(plant.writer(&["site.time", "site.c"]).await);
        let channels = ["site.time", "site.c", "edge.time", "edge.a", "edge.b"];
        let mut writer = plant.writer(&channels).await;
        let index = (writer.set().entries().iter())
            .position(|entry| entry.key == channel::Key::from_u128(1));
        assert_eq!(index, Some(2), "the group of `site.time` comes first");
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        let sent = plant.got().len();
        let failed = (StatusCode::INTERNAL_SERVER_ERROR, "");
        plant.answers.borrow_mut().push_back(failed);
        let lost = write_lost(&mut writer, first + 1);
        write(&mut writer, first + 4, &[(3.0, 4.0)]);
        plant.clock.sleep(Span::from_nanos(5_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        let bodies: Vec<_> = (plant.got().into_iter().skip(sent))
            .map(|(_, body)| body)
            .collect();
        let counts = (plant.count("lost"), plant.count("confirmed"));
        (lost, first + 4, bodies, counts)
    });
    assert_eq!(lost.end - lost.start, 3);
    let body = format!(
        "foundation_gaps,connector=influx,index=edge.time,path=live count=3i \
         {stamp}\n{}",
        line("3e0", "4e0", stamp),
    );
    assert_eq!(bodies, [body.clone(), body]);
    assert_eq!(counts, (vec![0, 3], vec![0, 2, 4]));
}

/// What InfluxDB gets after a run lost for no room, then `after`: the body of the
/// first run, the count `lost`, and the body of a second run, which gets one more
/// frame at the stamp after `after`. The stamp is that of the first sample after the
/// loss.
fn after_a_loss(
    after: fn(&mut hub::writer::Writer, i64),
) -> (String, Vec<u64>, String, i64) {
    run(move |node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        let run = plant.supervise(plant.config()).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        let sent = plant.got().len();
        write_lost(&mut writer, first + 1);
        after(&mut writer, first + 4);
        plant.clock.sleep(Span::from_nanos(5_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        let bodies = |got: Vec<Got>, skip| -> String {
            got.into_iter().skip(skip).map(|(_, body)| body).collect()
        };
        let body = bodies(std::mem::take(&mut *plant.got.borrow_mut()), sent);
        let lost = plant.count("lost");
        let run = plant.supervise(plant.config()).await;
        plant.clock.sleep(Span::SECOND).await;
        write(&mut writer, first + 10, &[(5.0, 6.0)]);
        plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        (body, lost, bodies(plant.got(), 0), first + 4)
    })
}

/// The gap line sits at the first sample time after the gap, also when the run leaves
/// its line out, and counts only the seqs of the gap. The batch acks the frame, so the
/// next run gets no gap again.
#[test]
fn writes_the_gap_line_at_a_first_sample_time_whose_line_is_left_out() {
    let (body, lost, again, stamp) = after_a_loss(|writer, at| {
        write(writer, at, &[(f64::NAN, f64::INFINITY), (3.0, 4.0)]);
    });
    let gap = "foundation_gaps,connector=influx,index=edge.time,path=live";
    let data = line("3e0", "4e0", stamp + 1);
    assert_eq!(body, format!("{gap} count=3i {stamp}\n{data}"));
    assert_eq!(lost, [0, 3]);
    assert_eq!(again, line("5e0", "6e0", stamp + 6));
}

#[test]
fn writes_the_gap_line_at_the_first_stamp_of_a_frame_with_no_data_channel() {
    let (body, lost, again, stamp) = after_a_loss(|writer, at| {
        write_times(writer, at, 2);
        write(writer, at + 2, &[(3.0, 4.0)]);
    });
    let gap = "foundation_gaps,connector=influx,index=edge.time,path=live";
    let data = line("3e0", "4e0", stamp + 2);
    assert_eq!(body, format!("{gap} count=3i {stamp}\n{data}"));
    assert_eq!(lost, [0, 3]);
    assert_eq!(again, line("5e0", "6e0", stamp + 6));
}

#[test]
fn confirms_no_sample_of_a_batch_until_its_204() {
    let (failing, confirmed) = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let failed = (StatusCode::INTERNAL_SERVER_ERROR, "");
        plant.answers.borrow_mut().extend([failed; 100]);
        let run = plant.supervise(plant.config()).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::from_nanos(20_000_000_000)).await;
        let failing = (plant.count("confirmed"), plant.got().len());
        plant.answers.borrow_mut().clear();
        plant.clock.sleep(Span::from_nanos(70_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        (failing, plant.count("confirmed"))
    });
    let (values, requests) = failing;
    assert_eq!(values, [0]);
    assert!(
        requests > 1,
        "InfluxDB got {requests} requests while it failed"
    );
    assert_eq!(confirmed, [0, 2]);
}

/// The bodies that the InfluxDB of a plant gets in each run, one run after the other,
/// and the errors that the last run shows. InfluxDB answers each request of the first
/// run with 500, or with 204 when `accepted`, and each later one with 204. Each run
/// writes one frame of one sample time, at which each channel holds its value of
/// `values`.
fn runs<const N: usize>(
    accepted: bool,
    values: [f64; N],
) -> ([Vec<String>; N], Vec<String>) {
    run(move |node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        if !accepted {
            let failed = (StatusCode::INTERNAL_SERVER_ERROR, "");
            plant.answers.borrow_mut().extend([failed; 100]);
        }
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        let (mut bodies, mut last) = (Vec::new(), 0);
        for value in values {
            last = plant.status.readings().len();
            let run = plant.supervise(plant.config()).await;
            plant.clock.sleep(Span::SECOND).await;
            let first = writer.now().nanos();
            write(&mut writer, first, &[(value, value)]);
            plant.clock.sleep(Span::from_nanos(3_000_000_000)).await;
            run.stop().await.expect("ok after a cancel");
            plant.answers.borrow_mut().clear();
            let got = std::mem::take(&mut *plant.got.borrow_mut());
            bodies.push(got.into_iter().map(|(_, body)| body).collect::<Vec<_>>());
        }
        let errors = changes(&plant.status.readings()[last..], error);
        (bodies.try_into().expect("a body list for each run"), errors)
    })
}

#[test]
fn acks_no_batch_before_a_204() {
    let ([first, second, third], errors) = runs(false, [1.0, 2.0, 3.0]);
    assert!(first.len() > 1, "{first:?}");
    let sent = first.first().expect("a request");
    assert!(first.iter().all(|body| body == sent), "{first:?}");
    assert!(
        sent.starts_with("telemetry edge.a=1e0,edge.b=1e0 "),
        "{sent}"
    );
    assert_eq!(second.len(), 2, "{second:?}");
    assert_eq!(&second[0], sent);
    assert!(second[1].starts_with("telemetry edge.a=2e0,edge.b=2e0 "));
    assert_eq!(third.len(), 1, "{third:?}");
    assert!(third[0].starts_with("telemetry edge.a=3e0,edge.b=3e0 "));
    assert_eq!(errors, [""]);
}

#[test]
fn acks_a_batch_after_its_204() {
    let ([first, second], errors) = runs(true, [1.0, 2.0]);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].starts_with("telemetry edge.a=1e0,edge.b=1e0 "));
    assert_eq!(second.len(), 1, "{second:?}");
    assert!(second[0].starts_with("telemetry edge.a=2e0,edge.b=2e0 "));
    assert_eq!(errors, [""]);
}

#[test]
fn acks_a_batch_of_no_line_with_no_request() {
    let ([first, second], errors) = runs(true, [f64::NAN, 2.0]);
    assert_eq!(first, Vec::<String>::new());
    assert_eq!(second.len(), 1, "{second:?}");
    assert!(second[0].starts_with("telemetry edge.a=2e0,edge.b=2e0 "));
    assert_eq!(errors, [""]);
}

/// The count of lines in each body that InfluxDB gets, when it answers the first
/// request with 500 and a frame of one sample time goes before frames of `times`
/// sample times each, written while that request is in flight.
fn lines_of_each_request(times: &[usize]) -> Vec<usize> {
    let times: Vec<_> = times.iter().map(|&count| (count, 1.0)).collect();
    lines_of_each_request_of(&times)
}

/// As [`lines_of_each_request`], with each `(count, value)` of `times` a frame of
/// `count` sample times at which each channel holds `value`.
fn lines_of_each_request_of(times: &[(usize, f64)]) -> Vec<usize> {
    lines_of_each_request_after(false, times)
}

/// As [`lines_of_each_request_of`], with a frame lost for no room before the frames
/// of `times` when `lost`.
fn lines_of_each_request_after(lost: bool, times: &[(usize, f64)]) -> Vec<usize> {
    let failed = (StatusCode::INTERNAL_SERVER_ERROR, "");
    requests(&[failed], lost, times).0
}

/// As [`lines_of_each_request_after`], with InfluxDB giving `answers` in turn, then
/// 204, and with the values of `confirmed` after the lines.
fn requests(
    answers: &[(StatusCode, &'static str)],
    lost: bool,
    times: &[(usize, f64)],
) -> (Vec<usize>, Vec<u64>) {
    let (answers, times) = (answers.to_vec(), times.to_vec());
    run(move |node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        plant.answers.borrow_mut().extend(answers);
        let run = plant.supervise(plant.config()).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let mut first = writer.now().nanos();
        write(&mut writer, first, &[(0.0, 0.0)]);
        plant.clock.sleep(Span::from_nanos(100_000_000)).await;
        assert_eq!(
            plant.got.borrow().len(),
            1,
            "the first request is in flight"
        );
        if lost {
            first += 10_000;
            write_lost(&mut writer, first);
        }
        for (count, value) in times {
            first += 10_000;
            write(&mut writer, first, &vec![(value, value); count]);
        }
        plant.clock.sleep(Span::from_nanos(5_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        let got = plant.got();
        let lines = (got.iter()).map(|(_, body)| body.lines().count()).collect();
        (lines, plant.count("confirmed"))
    })
}

#[test]
fn reads_each_frame_into_the_next_batch_while_a_request_is_in_flight() {
    assert_eq!(lines_of_each_request(&[1, 2, 3]), [1, 1, 6]);
}

#[test]
fn reads_no_frame_into_a_batch_of_5000_lines_or_more() {
    assert_eq!(
        lines_of_each_request(&[2_500, 2_499, 1, 3_000, 2_000]),
        [1, 1, 5_000, 5_000]
    );
}

/// A body of 5,000 lines closes in the middle of a frame, and the rest of the frame
/// goes on in the next body.
#[test]
fn splits_a_frame_at_5000_lines() {
    assert_eq!(lines_of_each_request(&[2_500, 2_499, 2]), [1, 1, 5_000, 1]);
}

/// A body that closes at 5,000 lines in the middle of a frame acks the frames that end
/// in it, so its 204 counts their samples while the rest of the frame waits.
#[test]
fn acks_the_frames_of_a_body_that_closes_at_5000_lines() {
    let (taken, failed) = (
        (StatusCode::NO_CONTENT, ""),
        (StatusCode::INTERNAL_SERVER_ERROR, ""),
    );
    let mut answers = vec![failed, taken, taken];
    answers.extend([failed; 100]);
    let times = [(2_500, 1.0), (2_499, 1.0), (2, 1.0)];
    let (lines, confirmed) = requests(&answers, false, &times);
    assert_eq!(lines[..4], [1, 1, 5_000, 1]);
    assert_eq!(confirmed, [0, 10_000]);
}

/// A gap line counts toward the 5,000 lines of a full batch.
#[test]
fn reads_no_frame_into_a_batch_of_4999_lines_and_a_gap_line() {
    assert_eq!(
        lines_of_each_request_after(true, &[(2_499, 1.0), (2_500, 1.0), (1, 1.0)]),
        [1, 1, 5_000, 1]
    );
}

/// A sample time at which each value is NaN gives no line, so it does not count
/// toward the 5,000 lines of a full batch.
#[test]
fn counts_no_line_toward_a_full_batch_for_a_sample_time_of_only_nan() {
    assert_eq!(
        lines_of_each_request_of(&[
            (2_500, f64::NAN),
            (2_500, f64::NAN),
            (1, 1.0),
            (1, 1.0)
        ]),
        [1, 1, 2]
    );
}

/// A batch of 63 lines of 64 KiB takes one more frame, and a batch of 64 lines, 4 MiB,
/// takes none, so it stays under the body limit of InfluxDB 3.
#[test]
fn reads_no_frame_into_a_batch_of_4_mib_or_more() {
    let bytes = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let failed = (StatusCode::INTERNAL_SERVER_ERROR, "");
        plant.answers.borrow_mut().push_back(failed);
        let run = plant
            .supervise(with(plant.config(), "select", &string("edge.*")))
            .await;
        let mut writer = plant.writer(&["edge.time", "edge.text"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        let stamp = first.to_string().len();
        let text = "x".repeat((1 << 16) - "telemetry edge.text=\"\" \n".len() - stamp);
        let column = variable(Type::String, &[text.as_bytes()]);
        for n in 0..=100 {
            let outcomes =
                write_raw(&mut writer, first + n * 10_000, 1, &[(23, &column)]);
            let applied = (outcomes.iter())
                .all(|outcome| matches!(outcome, hub::home::Outcome::Applied { .. }));
            assert!(applied, "the frame applies: {outcomes:?}");
            if n == 0 {
                plant.clock.sleep(Span::from_nanos(100_000_000)).await;
                let sent = plant.got.borrow().len();
                assert_eq!(sent, 1, "the first request is in flight");
            }
        }
        plant.clock.sleep(Span::from_nanos(5_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        let got = plant.got();
        (got.iter()).map(|(_, body)| body.len()).collect::<Vec<_>>()
    });
    assert_eq!(bytes, [1 << 16, 1 << 16, 64 << 16, 36 << 16]);
}

/// The names of 64 `u8` channels on `edge.time`, of 240 bytes each, so the line of one
/// sample time at which each holds 255 is 15,774 bytes, and a ring record of the test
/// hub holds a frame of over 10 MiB of lines.
fn wide_names() -> Vec<String> {
    (0..64)
        .map(|at| format!("edge.w{at:02}{}", "x".repeat(232)))
        .collect()
}

/// Writes one frame through `writer` of `count` sample times at `first` and each
/// nanosecond after, at which each channel of [`wide_names`] holds 255.
fn write_wide(writer: &mut hub::writer::Writer, first: i64, count: usize) {
    let samples = vec![255; count];
    let columns: Vec<_> = (WIDE..WIDE + 64)
        .map(|key| (key, samples.as_slice()))
        .collect();
    let outcomes = write_raw(writer, first, count, &columns);
    let applied = (outcomes.iter())
        .all(|outcome| matches!(outcome, hub::home::Outcome::Applied { .. }));
    assert!(applied, "the frame applies: {outcomes:?}");
}

/// What two runs gave with frames of the channels of [`wide_names`].
struct Wide {
    /// The count of lines of each request of each run.
    lines: [Vec<usize>; 2],
    /// The errors of both runs.
    errors: Vec<String>,
    /// The values of `confirmed`, which each run starts at 0.
    confirmed: Vec<u64>,
    /// The points that the store holds at the end.
    points: usize,
}

/// Runs the connector twice. InfluxDB answers the first requests of the first run with
/// `answers`, and each later one from its store. The first run writes a frame of the
/// channels of [`wide_names`] for each count of sample times of `frames`: the first,
/// then the rest while its request is in flight. The second run writes none.
fn wide(answers: &[(StatusCode, &'static str)], frames: &'static [usize]) -> Wide {
    let answers = answers.to_vec();
    run(move |node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        plant.answers.borrow_mut().extend(answers);
        let names = wide_names();
        let names =
            std::iter::once("edge.time").chain(names.iter().map(String::as_str));
        let mut writer = plant.writer(&names.collect::<Vec<_>>()).await;
        let mut lines = Vec::new();
        for frames in [frames, &[]] {
            let run = plant.supervise(plant.config()).await;
            plant.clock.sleep(Span::SECOND).await;
            let mut first = writer.now().nanos();
            for (n, &count) in frames.iter().enumerate() {
                write_wide(&mut writer, first, count);
                first += 10_000;
                if n == 0 {
                    plant.clock.sleep(Span::from_nanos(100_000_000)).await;
                }
            }
            plant.clock.sleep(Span::from_nanos(10_000_000_000)).await;
            run.stop().await.expect("ok after a cancel");
            plant.answers.borrow_mut().clear();
            let got = std::mem::take(&mut *plant.got.borrow_mut());
            lines.push(got.iter().map(|(_, body)| body.lines().count()).collect());
        }
        Wide {
            lines: lines.try_into().expect("two runs"),
            errors: plant.errors(),
            confirmed: plant.count("confirmed"),
            points: plant.store.borrow().points("telemetry", &[]).count(),
        }
    })
}

/// A frame of over 6 MiB of lines after a batch of just under 4 MiB goes on in the
/// next requests, so no request passes the 10 MiB of InfluxDB 3 Core. The run acks
/// each frame and counts its samples once, with the request of its last line.
#[test]
fn sends_a_frame_of_over_6_mib_after_a_batch_of_4_mib_in_more_requests() {
    let failed = (StatusCode::INTERNAL_SERVER_ERROR, "");
    let wide = wide(&[failed], &[1, 260, 450]);
    assert_eq!(wide.lines, [vec![1, 1, 265, 265, 180], vec![]]);
    assert_eq!(
        wide.errors,
        [
            "",
            "InfluxDB at 10.0.0.1:8086 answered 500 Internal Server Error: ",
            ""
        ]
    );
    assert_eq!(wide.confirmed, [0, 64, 711 * 64, 0]);
    assert_eq!(wide.points, 711);
}

/// A frame whose lines pass the body limit alone lands in more than one request.
#[test]
fn sends_a_frame_over_the_body_limit_in_more_requests() {
    let wide = wide(&[], &[700]);
    assert_eq!(wide.lines, [vec![265, 265, 170], vec![]]);
    assert_eq!(wide.errors, [""]);
    assert_eq!(wide.confirmed, [0, 700 * 64, 0]);
    assert_eq!(wide.points, 700);
}

/// A batch is full, so the run reads no frame, while a body of a frame's lines waits
/// for its request, or the open body holds 5,000 lines. A line that takes a body past
/// 4 MiB moves to the next body.
#[test]
fn is_full_until_each_closed_body_is_taken() {
    let (full, lines) = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let names = wide_names();
        let names =
            std::iter::once("edge.time").chain(names.iter().map(String::as_str));
        let mut writer = plant.writer(&names.collect::<Vec<_>>()).await;
        let open = hub::reader::Config {
            select: Selector::new(["edge.**"]).expect("a selector"),
            mode: Mode::Complete,
            subject: name("test"),
            name: None,
            hold: Span::ZERO,
        };
        let mut reader = plant
            .rig
            .hub()
            .reader(open)
            .await
            .expect("the reader opens");
        let first = writer.now().nanos();
        write_wide(&mut writer, first, 700);
        write_wide(&mut writer, first + 10_000, 266);
        let narrow = [1; 5_000];
        let outcomes =
            write_raw(&mut writer, first + 20_000, 5_000, &[(WIDE, &narrow)]);
        let applied = (outcomes.iter())
            .all(|outcome| matches!(outcome, hub::home::Outcome::Applied { .. }));
        assert!(applied, "the frame applies: {outcomes:?}");
        plant.clock.sleep(Span::SECOND).await;
        let measurement = Measurement::new("telemetry", &[]).expect("a measurement");
        let target = Target::Bucket {
            name: "plant".into(),
            org: "o".into(),
        };
        let mut batch = Batch::new(measurement, &target, &name("influx"));
        let (mut full, mut lines) = (Vec::new(), Vec::new());
        for _ in 0..3 {
            batch
                .add(&reader.next().await.expect("a frame"))
                .expect("adds");
            full.push(batch.full());
            while let Some(taken) = batch.take() {
                let acks = taken.ack.is_some();
                let body = String::from_utf8(taken.body).expect("UTF-8");
                lines.push((body.lines().count(), acks));
                full.push(batch.full());
            }
        }
        (full, lines)
    });
    assert_eq!(
        full,
        [true, true, false, false, true, false, false, true, false]
    );
    assert_eq!(
        lines,
        [
            (265, false),
            (265, false),
            (170, true),
            (265, false),
            (1, true),
            (5_000, true)
        ]
    );
}

/// A frame in more requests is acked and counted only after the last of them lands, so
/// a restart sends each of its lines again.
#[test]
fn acks_and_counts_a_frame_in_more_requests_only_after_its_last_request() {
    let (taken, failed) = (
        (StatusCode::NO_CONTENT, ""),
        (StatusCode::INTERNAL_SERVER_ERROR, ""),
    );
    let mut answers = vec![taken, taken];
    answers.extend([failed; 100]);
    let wide = wide(&answers, &[700]);
    let [first, second] = wide.lines;
    assert_eq!(first[..3], [265, 265, 170]);
    assert!(first[3..].iter().all(|&lines| lines == 170), "{first:?}");
    assert_eq!(second, [265, 265, 170]);
    assert_eq!(
        wide.errors,
        [
            "",
            "InfluxDB at 10.0.0.1:8086 answered 500 Internal Server Error: ",
            ""
        ]
    );
    assert_eq!(wide.confirmed, [0, 700 * 64]);
    assert_eq!(wide.points, 700);
}

/// A request that holds the ends of more frames acks the last of them.
#[test]
fn acks_the_last_frame_that_ends_in_a_request() {
    let failed = (StatusCode::INTERNAL_SERVER_ERROR, "");
    let wide = wide(&[failed], &[1, 1, 1]);
    assert_eq!(wide.lines, [vec![1, 1, 2], vec![]]);
}

/// A frame that ends in a body that the next frame closes is acked and counted with
/// the request of that body.
#[test]
fn acks_a_frame_with_the_body_that_the_next_frame_closes() {
    let (taken, failed) = (
        (StatusCode::NO_CONTENT, ""),
        (StatusCode::INTERNAL_SERVER_ERROR, ""),
    );
    let mut answers = vec![failed, taken, taken, taken];
    answers.extend([failed; 100]);
    let wide = wide(&answers, &[1, 260, 450]);
    let [first, second] = wide.lines;
    assert_eq!(first[..5], [1, 1, 265, 265, 180]);
    assert_eq!(second, [265, 185]);
    assert_eq!(wide.confirmed, [0, 64, 261 * 64, 0, 450 * 64]);
}

/// The lost seqs and the oversized samples of a frame in more requests count once,
/// after the request of its last line: a run whose last request fails counts none,
/// and the next run counts each once.
#[test]
fn counts_the_lost_and_oversized_of_a_frame_in_more_requests_after_its_last() {
    let (lines, counts) = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let names = wide_names();
        let channels = ["edge.time", "edge.a", "edge.b", "edge.text"];
        let channels = channels.into_iter().chain(names.iter().map(String::as_str));
        let mut writer = plant.writer(&channels.collect::<Vec<_>>()).await;
        let run = plant.supervise(plant.config()).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        let sent = plant.got().len();
        let (taken, failed) = (
            (StatusCode::NO_CONTENT, ""),
            (StatusCode::INTERNAL_SERVER_ERROR, ""),
        );
        plant.answers.borrow_mut().push_back(taken);
        plant.answers.borrow_mut().extend([failed; 100]);
        write_lost(&mut writer, first + 1);
        let long = "\\".repeat(32_769);
        let mut text = vec![long.as_bytes()];
        text.extend([b"a".as_slice(); 265]);
        let text = variable(Type::String, &text);
        let wide = [255; 266];
        let mut columns: Vec<_> =
            (WIDE..WIDE + 64).map(|key| (key, &wide[..])).collect();
        columns.push((23, &text));
        write_raw(&mut writer, first + 4, 266, &columns);
        plant.clock.sleep(Span::from_nanos(10_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        plant.answers.borrow_mut().clear();
        let lines = |got: &[Got]| -> Vec<usize> {
            got.iter().map(|(_, body)| body.lines().count()).collect()
        };
        let mut runs = vec![lines(&plant.got()[sent..])];
        plant.got.borrow_mut().clear();
        let run = plant.supervise(plant.config()).await;
        plant.clock.sleep(Span::from_nanos(10_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        runs.push(lines(&plant.got()));
        let counts = (
            plant.count("lost"),
            plant.count("oversized"),
            plant.count("confirmed"),
        );
        (runs, counts)
    });
    assert_eq!(lines[0][..2], [266, 1]);
    assert!(
        lines[0][2..].iter().all(|&lines| lines == 1),
        "{:?}",
        lines[0]
    );
    assert_eq!(lines[1], [266, 1]);
    assert_eq!(
        counts,
        (vec![0, 3], vec![0, 1], vec![0, 2, 0, 266 * 64 + 265])
    );
}

/// What one frame gave: the end of the run once it returned, each request, the stamp
/// of the frame's first sample, and the fields of each point of the store.
struct Frame {
    end: Option<Result<(), Error>>,
    got: Vec<Got>,
    first: i64,
    points: Vec<BTreeMap<String, crate::sim::Field>>,
    /// Each value of `influx.status.oversized`.
    oversized: Vec<u64>,
}

/// Writes one frame of `count` sample times, with the raw samples of each channel of
/// `columns` (its name, key, and bytes), to a connector that selects `select`. It
/// writes to the database `plant`, or to the bucket `edge` when `bucket`, and then the
/// store takes a `u` integer, as InfluxDB 2 and 3 do.
fn one_frame(
    select: &'static str,
    bucket: bool,
    count: usize,
    columns: Vec<(&'static str, u128, Vec<u8>)>,
) -> Frame {
    run(move |node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let mut config = with(plant.config(), "select", &string(select));
        if bucket {
            *plant.store.borrow_mut() = crate::sim::Store::unsigned();
            let target = [(60, "bucket", string("edge")), (62, "org", string("o"))];
            config = plus(without(config, "database"), &target);
        }
        let run = plant.supervise(config).await;
        let names = columns.iter().map(|&(name, _, _)| name);
        let names: Vec<_> = std::iter::once("edge.time").chain(names).collect();
        let mut writer = plant.writer(&names).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        let raw: Vec<_> = (columns.iter())
            .map(|(_, key, bytes)| (*key, bytes.as_slice()))
            .collect();
        let outcomes = write_raw(&mut writer, first, count, &raw);
        let applied = outcomes
            .iter()
            .all(|outcome| matches!(outcome, hub::home::Outcome::Applied { .. }));
        assert!(applied, "the frame applies: {outcomes:?}");
        plant.clock.sleep(Span::SECOND).await;
        let end = run.end().await;
        let points = (plant.store.borrow().points("telemetry", &[]))
            .map(|point| {
                let fields = point.fields.iter();
                fields.map(|(key, field)| (key.to_owned(), field)).collect()
            })
            .collect();
        Frame {
            end,
            got: plant.got(),
            first,
            points,
            oversized: plant.count("oversized"),
        }
    })
}

/// The little-endian bytes of each sample of `samples`.
macro_rules! bytes {
    ($($sample:expr),* $(,)?) => {
        [$($sample.to_le_bytes().to_vec()),*].concat()
    };
}

#[test]
fn writes_each_scalar_type_other_than_u64_and_uuid_as_its_field() {
    let columns = vec![
        ("edge.bool", 10, vec![1, 0]),
        ("edge.i8", 11, bytes![i8::MIN, i8::MAX]),
        ("edge.i16", 12, bytes![i16::MIN, i16::MAX]),
        ("edge.i32", 13, bytes![i32::MIN, i32::MAX]),
        ("edge.count", 4, bytes![i64::MIN, i64::MAX]),
        ("edge.u8", 14, bytes![0_u8, u8::MAX]),
        ("edge.u16", 15, bytes![0_u16, u16::MAX]),
        ("edge.u32", 16, bytes![0_u32, u32::MAX]),
        ("edge.f32", 18, bytes![0.1_f32, f32::NAN]),
        (
            "edge.stamp",
            19,
            bytes![-1_i64, 1_700_000_000_123_456_789_i64],
        ),
        ("edge.span", 20, bytes![-5_i64, i64::MAX]),
    ];
    let frame = one_frame("edge.*", false, 2, columns);
    assert!(frame.end.is_none(), "the run goes on: {:?}", frame.end);
    let first = frame.first;
    let body = format!(
        "telemetry edge.bool=t,edge.i8=-128i,edge.i16=-32768i,edge.i32=-2147483648i,\
         edge.count=-9223372036854775808i,edge.u8=0i,edge.u16=0i,edge.u32=0i,\
         edge.f32=1.0000000149011612e-1,edge.stamp=-1i,edge.span=-5i {first}\n\
         telemetry edge.bool=f,edge.i8=127i,edge.i16=32767i,edge.i32=2147483647i,\
         edge.count=9223372036854775807i,edge.u8=255i,edge.u16=65535i,\
         edge.u32=4294967295i,edge.stamp=1700000000123456789i,\
         edge.span=9223372036854775807i {}\n",
        first + 1
    );
    assert_eq!(frame.got, vec![(WRITE.into(), body)]);
    assert_eq!(frame.points.len(), 2);
}

#[test]
fn writes_an_f32_that_is_infinite_as_no_field() {
    let columns = vec![(
        "edge.f32",
        18,
        bytes![f32::INFINITY, f32::NEG_INFINITY, -2.5_f32],
    )];
    let frame = one_frame("edge.*", false, 3, columns);
    let body = format!("telemetry edge.f32=-2.5e0 {}\n", frame.first + 2);
    assert_eq!(frame.got, vec![(WRITE.into(), body)]);
}

#[test]
fn writes_a_u64_to_a_bucket_as_an_unsigned_integer() {
    let columns = vec![("edge.u64", 17, bytes![0_u64, u64::MAX])];
    let frame = one_frame("edge.*", true, 2, columns);
    assert!(frame.end.is_none(), "the run goes on: {:?}", frame.end);
    let first = frame.first;
    let body = format!(
        "telemetry edge.u64=0u {first}\n\
         telemetry edge.u64=18446744073709551615u {}\n",
        first + 1
    );
    let uri = "/api/v2/write?org=o&bucket=edge&precision=ns";
    assert_eq!(frame.got, vec![(uri.into(), body)]);
    let field = |value| BTreeMap::from([("edge.u64".into(), value)]);
    assert_eq!(
        frame.points,
        [
            field(crate::sim::Field::Unsigned(0)),
            field(crate::sim::Field::Unsigned(u64::MAX)),
        ]
    );
}

#[test]
fn stops_with_a_config_error_at_a_u64_channel_to_a_database() {
    let frame = one_frame("edge.*", false, 1, vec![("edge.u64", 17, bytes![1_u64])]);
    assert_eq!(frame.got, []);
    assert!(frame.points.is_empty());
    let Some(Err(Error::Config(diagnostics))) = frame.end else {
        panic!("a config error, not {:?}", frame.end);
    };
    assert_eq!(
        diagnostics,
        [Diagnostic::new(
            Code::new("influx.unsigned"),
            None,
            "the channel \"edge.u64\" holds u64 samples, which InfluxDB 1 does not \
             store"
                .into(),
            "Write to a `bucket` of InfluxDB 2 or 3, or leave the channel out of \
             `select`"
                .into(),
        )]
    );
}

/// The raw series of `samples` of the variable `data_type`.
fn variable(data_type: Type, samples: &[&[u8]]) -> Vec<u8> {
    let form = codec::Variable::of(data_type).expect("a variable type");
    let mut bytes = vec![0; form.len(samples).expect("fits")];
    form.write(samples, &mut bytes);
    bytes
}

#[test]
fn writes_a_string_with_each_quote_and_backslash_escaped_and_each_newline_kept() {
    let samples: [&[u8]; 3] = [b"a\"b\\c", b"two\nlines", b""];
    let columns = vec![("edge.text", 23, variable(Type::String, &samples))];
    let frame = one_frame("edge.*", false, 3, columns);
    assert!(frame.end.is_none(), "the run goes on: {:?}", frame.end);
    let first = frame.first;
    let body = format!(
        "telemetry edge.text=\"a\\\"b\\\\c\" {first}\n\
         telemetry edge.text=\"two\nlines\" {}\n\
         telemetry edge.text=\"\" {}\n",
        first + 1,
        first + 2
    );
    assert_eq!(frame.got, vec![(WRITE.into(), body)]);
    let field = |text: &str| {
        BTreeMap::from([("edge.text".into(), crate::sim::Field::String(text.into()))])
    };
    assert_eq!(
        frame.points,
        [field("a\"b\\c"), field("two\nlines"), field("")]
    );
}

#[test]
fn leaves_out_and_counts_a_string_over_65536_bytes_after_its_escapes() {
    let kept = ["é".repeat(32_768), format!("\"{}", "x".repeat(65_534))];
    let long = ["x".repeat(65_537), format!("\"{}", "x".repeat(65_535))];
    let samples = [&kept[0], &long[0], &kept[1], &long[1]].map(String::as_bytes);
    let columns = vec![
        ("edge.text", 23, variable(Type::String, &samples)),
        ("edge.count", 4, bytes![1_i64, 2_i64, 3_i64, 4_i64]),
    ];
    let frame = one_frame("edge.*", false, 4, columns);
    assert!(frame.end.is_none(), "the run goes on: {:?}", frame.end);
    let first = frame.first;
    let body = format!(
        "telemetry edge.text=\"{}\",edge.count=1i {first}\n\
         telemetry edge.count=2i {}\n\
         telemetry edge.text=\"\\{}\",edge.count=3i {}\n\
         telemetry edge.count=4i {}\n",
        kept[0],
        first + 1,
        kept[1],
        first + 2,
        first + 3
    );
    assert_eq!(frame.got, vec![(WRITE.into(), body)]);
    assert_eq!(frame.points.len(), 4);
    assert_eq!(frame.oversized, [0, 2]);
}

#[test]
fn writes_a_uuid_as_its_lowercase_text() {
    let uuid = 0x0011_2233_4455_6677_8899_aabb_ccdd_eeff_u128.to_be_bytes();
    let columns = vec![("edge.uuid", 21, uuid.to_vec())];
    let frame = one_frame("edge.*", false, 1, columns);
    assert!(frame.end.is_none(), "the run goes on: {:?}", frame.end);
    let body = format!(
        "telemetry edge.uuid=\"00112233-4455-6677-8899-aabbccddeeff\" {}\n",
        frame.first
    );
    assert_eq!(frame.got, vec![(WRITE.into(), body)]);
}

#[test]
fn writes_bytes_as_base64_with_padding_and_leaves_out_and_counts_over_49152() {
    let (kept, long) = (vec![0xfb_u8; 49_152], vec![0_u8; 49_153]);
    let samples: [&[u8]; 4] = [&[0, 255, 1], &[0xfb], &kept, &long];
    let columns = vec![
        ("edge.blob", 24, variable(Type::Bytes, &samples)),
        ("edge.count", 4, bytes![1_i64, 2_i64, 3_i64, 4_i64]),
    ];
    let frame = one_frame("edge.*", false, 4, columns);
    assert!(frame.end.is_none(), "the run goes on: {:?}", frame.end);
    let first = frame.first;
    let body = format!(
        "telemetry edge.blob=\"AP8B\",edge.count=1i {first}\n\
         telemetry edge.blob=\"+w==\",edge.count=2i {}\n\
         telemetry edge.blob=\"{}\",edge.count=3i {}\n\
         telemetry edge.count=4i {}\n",
        first + 1,
        "+/v7".repeat(16_384),
        first + 2,
        first + 3
    );
    assert_eq!(frame.got, vec![(WRITE.into(), body)]);
    assert_eq!(frame.points.len(), 4);
    assert_eq!(frame.oversized, [0, 1]);
}

#[test]
fn writes_empty_bytes_as_an_empty_string() {
    let columns = vec![("edge.blob", 24, variable(Type::Bytes, &[&[]]))];
    let frame = one_frame("edge.*", false, 1, columns);
    let body = format!("telemetry edge.blob=\"\" {}\n", frame.first);
    assert_eq!(frame.got, vec![(WRITE.into(), body)]);
}

#[test]
fn counts_an_oversized_string_whose_line_is_left_out() {
    let long = "x".repeat(65_537);
    let columns = vec![("edge.text", 23, variable(Type::String, &[long.as_bytes()]))];
    let frame = one_frame("edge.*", false, 1, columns);
    assert!(frame.end.is_none(), "the run goes on: {:?}", frame.end);
    assert_eq!(frame.got, []);
    assert_eq!(frame.oversized.last(), Some(&1), "{:?}", frame.oversized);
}

#[test]
fn counts_oversized_bytes_whose_line_is_left_out() {
    let long = vec![0_u8; 49_153];
    let columns = vec![("edge.blob", 24, variable(Type::Bytes, &[&long]))];
    let frame = one_frame("edge.*", false, 1, columns);
    assert!(frame.end.is_none(), "the run goes on: {:?}", frame.end);
    assert_eq!(frame.got, []);
    assert_eq!(frame.oversized.last(), Some(&1), "{:?}", frame.oversized);
}

/// A batch adds only the samples that it left out, so a later batch with none leaves
/// the count as it is.
#[test]
fn counts_each_oversized_sample_in_the_batch_that_left_it_out() {
    let oversized = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let run = plant
            .supervise(with(plant.config(), "select", &string("edge.*")))
            .await;
        let mut writer = plant.writer(&["edge.time", "edge.text"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        let long = "x".repeat(65_537);
        let frames: [[&[u8]; 2]; 2] =
            [[long.as_bytes(), long.as_bytes()], [b"a", b"b"]];
        for (at, samples) in [first, first + 10].into_iter().zip(frames) {
            let column = variable(Type::String, &samples);
            write_raw(&mut writer, at, 2, &[(23, &column)]);
            plant.clock.sleep(Span::SECOND).await;
        }
        run.stop().await.expect("ok after a cancel");
        assert_eq!(plant.got().len(), 1, "the second batch is sent");
        plant.count("oversized")
    });
    assert_eq!(oversized, [0, 2]);
}

#[test]
fn stops_with_a_config_error_at_a_channel_of_a_type_it_does_not_write() {
    let xyz = ("edge.xyz", 22, bytes![1.0_f32, 2.0_f32, 3.0_f32], "f32[3]");
    let list = variable(
        Type::List {
            element: Scalar::F32,
            max: 4,
        },
        &[&bytes![1.0_f32]],
    );
    let list = ("edge.list", 25, list, "list<f32, 4>");
    let grid = bytes![1.0_f32, 2.0_f32, 3.0_f32, 4.0_f32];
    let grid = ("edge.grid", 26, grid, "f32[2][2]");
    for (channel, key, bytes, data_type) in [xyz, list, grid] {
        let frame = one_frame("edge.*", true, 1, vec![(channel, key, bytes)]);
        assert_eq!(frame.got, []);
        let Some(Err(Error::Config(diagnostics))) = frame.end else {
            panic!("a config error, not {:?}", frame.end);
        };
        assert_eq!(
            diagnostics,
            [Diagnostic::new(
                Code::new("influx.sample-type"),
                None,
                format!(
                    "the channel {channel:?} holds {data_type} samples, which the \
                     influx kind does not write"
                ),
                "Leave the channel out of `select`".into(),
            )]
        );
    }
}

#[test]
fn stops_with_a_config_error_at_a_channel_whose_name_is_no_field_key() {
    let frame = one_frame("time", false, 1, vec![("time", 5, bytes![7.0_f64])]);
    assert_eq!(frame.got, []);
    let Some(Err(Error::Config(diagnostics))) = frame.end else {
        panic!("a config error, not {:?}", frame.end);
    };
    assert_eq!(
        diagnostics,
        [Diagnostic::new(
            Code::new("influx.bad-channel"),
            None,
            "a channel name is not a field key: InfluxDB keeps the name \"time\" for \
             itself"
                .into(),
            "Rename the channel, or leave it out of `select`".into(),
        )]
    );
}

#[test]
fn writes_the_fields_of_the_channels_of_each_frame() {
    let (got, first) = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let run = plant.supervise(plant.config()).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        write_columns(&mut writer, first + 1, &[(3, &[3.0])]);
        write_columns(&mut writer, first + 2, &[(2, &[4.0])]);
        write_columns(&mut writer, first + 3, &[]);
        write(&mut writer, first + 4, &[(5.0, 6.0)]);
        plant.clock.sleep(Span::SECOND).await;
        run.stop().await.expect("ok after a cancel");
        (plant.got(), first)
    });
    let body = [
        line("1e0", "2e0", first),
        format!("telemetry edge.b=3e0 {}\n", first + 1),
        format!("telemetry edge.a=4e0 {}\n", first + 2),
        line("5e0", "6e0", first + 4),
    ];
    let got: Vec<_> = got.into_iter().map(|(_, body)| body).collect();
    assert_eq!(got.concat(), body.concat());
}

/// The second key set holds `edge.b` at the entry where the first holds `edge.a`.
#[test]
fn names_each_field_by_the_key_set_of_its_frame() {
    let (got, first) = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let run = plant.supervise(plant.config()).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        drop(writer);
        let mut writer = plant.writer(&["edge.time", "edge.b"]).await;
        write_columns(&mut writer, first + 1, &[(3, &[3.0])]);
        plant.clock.sleep(Span::SECOND).await;
        run.stop().await.expect("ok after a cancel");
        (plant.got(), first)
    });
    let body = [
        line("1e0", "2e0", first),
        format!("telemetry edge.b=3e0 {}\n", first + 1),
    ];
    let got: Vec<_> = got.into_iter().map(|(_, body)| body).collect();
    assert_eq!(got.concat(), body.concat());
}

#[test]
fn shows_a_write_that_fails_until_the_run_ends() {
    let errors = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let address = format!("http://{}:{}", plant.address.ip(), PORT + 1);
        let run = plant
            .supervise(with(plant.config(), "address", &string(&address)))
            .await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::from_nanos(3_000_000_000)).await;
        run.stop().await.expect("ok after a cancel");
        // The record takes the last status after the home applied it.
        plant.clock.sleep(Span::SECOND).await;
        plant.errors()
    });
    let refused = "the write to InfluxDB failed: the connect failed: 10.0.0.1:8087 \
                   refused the connection";
    assert_eq!(errors, ["", refused, ""]);
}

#[test]
fn holds_each_sample_once_in_time_order_after_writes_that_fail() {
    stores_each_sample_once_after_writes_that_fail(false);
}

#[test]
fn holds_each_sample_after_writes_that_fail_with_no_reader_block() {
    stores_each_sample_once_after_writes_that_fail(true);
}

/// Checks that InfluxDB stores each sample once, in time order, when it refuses each
/// connection for 6 s, then answers 500 once. The config has no `reader` block when
/// `blockless`, else that of [`config`].
fn stores_each_sample_once_after_writes_that_fail(blockless: bool) {
    let (points, accepted, errors, first) = run(move |node, tasks| async move {
        let plant = Plant::unserved(node, tasks).await;
        let failed = (StatusCode::INTERNAL_SERVER_ERROR, "the disk is full");
        plant.answers.borrow_mut().push_back(failed);
        let mut config = plant.config();
        if blockless {
            config.blocks.clear();
        }
        let run = plant.supervise(config).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        for at in 0..6 {
            if at == 3 {
                plant.serve();
            }
            let value = f64::from(u8::try_from(at).expect("fits"));
            write(&mut writer, first + at, &[(value, -value)]);
            plant.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        }
        plant.clock.sleep(Span::MINUTE).await;
        run.stop().await.expect("ok after a cancel");
        let store = plant.store.borrow();
        let points: Vec<_> = (store.points("telemetry", &[]))
            .map(|point| (point.time.nanos(), format!("{:?}", point.fields)))
            .collect();
        let got = plant.got();
        let accepted = (got.iter().skip(1))
            .map(|(_, body)| body.lines().count())
            .sum::<usize>();
        (points, accepted, plant.errors(), first)
    });
    let expected: Vec<_> = (0..6)
        .map(|at| {
            let fields =
                format!("{{\"edge.a\": Float({at}.0), \"edge.b\": Float(-{at}.0)}}");
            (first + at, fields)
        })
        .collect();
    assert_eq!(points, expected);
    assert_eq!(accepted, expected.len(), "the lines of each 204");
    let refused = "the write to InfluxDB failed: the connect failed: 10.0.0.1:8086 \
                   refused the connection";
    let full = "InfluxDB at 10.0.0.1:8086 answered 500 Internal Server Error: the disk \
                is full";
    assert_eq!(errors, ["", refused, full, ""]);
}

#[test]
fn adds_a_field_before_the_fields_of_the_layout() {
    let (got, first) = run(|node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let run = plant.supervise(plant.config()).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write_columns(&mut writer, first, &[(3, &[3.0])]);
        write(&mut writer, first + 1, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::SECOND).await;
        run.stop().await.expect("ok after a cancel");
        (plant.got(), first)
    });
    let body = [
        format!("telemetry edge.b=3e0 {first}\n"),
        line("1e0", "2e0", first + 1),
    ];
    let got: Vec<_> = got.into_iter().map(|(_, body)| body).collect();
    assert_eq!(got.concat(), body.concat());
}

/// With no `hold`, a restart loses the batch in flight, as INFLUX KIND states until
/// #274.
#[test]
fn loses_the_batch_in_flight_at_a_restart_with_no_hold() {
    let (first, second) = run(move |node, tasks| async move {
        let plant = Plant::new(node, tasks).await;
        let failed = (StatusCode::INTERNAL_SERVER_ERROR, "");
        plant.answers.borrow_mut().extend([failed; 100]);
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        let mut bodies = Vec::new();
        for value in [1.0, 2.0] {
            let mut config = plant.config();
            config.blocks.clear();
            let run = plant.supervise(config).await;
            plant.clock.sleep(Span::SECOND).await;
            let first = writer.now().nanos();
            write(&mut writer, first, &[(value, value)]);
            plant.clock.sleep(Span::from_nanos(3_000_000_000)).await;
            run.stop().await.expect("ok after a cancel");
            plant.answers.borrow_mut().clear();
            let got = std::mem::take(&mut *plant.got.borrow_mut());
            bodies.push(got.into_iter().map(|(_, body)| body).collect::<Vec<_>>());
        }
        let second = bodies.pop().expect("two runs");
        (bodies.pop().expect("two runs"), second)
    });
    assert!(first.len() > 1, "{first:?}");
    assert_eq!(second.len(), 1, "{second:?}");
    assert!(
        second[0].starts_with("telemetry edge.a=2e0,edge.b=2e0 "),
        "{second:?}"
    );
    assert_eq!(second[0].lines().count(), 1, "{second:?}");
}

/// As [`run`], on a node at the name `server` whose wall starts at [`JUNE_2026`], with
/// a host directory `/etc/influx` too.
fn run_tls<T, F>(main: impl FnOnce(::sim::node::Node, Tasks) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let mut sim = ::sim::Sim::new(::sim::Config::default());
    let node = sim.node(::sim::node::Config {
        wall: JUNE_2026,
        ..::sim::node::Config::default()
    });
    let answer = ::sim::name::Answer::Addresses(vec![node.addresses()[0]]);
    let name = ::sim::name::Config {
        answer,
        ..::sim::name::Config::default()
    };
    sim.name("server", name);
    node.make_dir(std::path::Path::new("/run/secrets"));
    node.make_dir(std::path::Path::new("/etc/influx"));
    sim.run_on(&node, main).expect("the run ends")
}

/// The config of `plant` with `ca` set to a call of `function` with `argument`.
fn with_ca(plant: &Plant, function: &str, argument: &str) -> Document {
    plus(plant.config(), &[(60, "ca", call(function, argument))])
}

/// Runs [`send_one`] on a plant whose InfluxDB serves `certificate`, whose node
/// trusts `roots`, and whose config reads `ca` from `file(CA_FILE)` when it holds
/// bytes. Gives the errors, whether the run went on, the lines stored, and each value
/// of `class` and of `restarts`.
fn send_over_tls(
    certificate: Certificate,
    roots: Roots,
    ca: &'static [u8],
) -> (Vec<String>, bool, usize, Vec<Option<Class>>, Vec<u64>) {
    run_tls(move |node, tasks| async move {
        node.paths()
            .write(std::path::Path::new(CA_FILE), ca)
            .expect("writes");
        let plant = Plant::tls(node, tasks, certificate, roots).await;
        let config = if ca.is_empty() {
            plant.config()
        } else {
            with_ca(&plant, "file", CA_FILE)
        };
        let (_, errors, end) = send_one(&plant, config).await;
        let points = plant.store.borrow().points("telemetry", &[]).count();
        let classes = plant.classes();
        let restarts = plant.restarts();
        (errors, end.is_none(), points, classes, restarts)
    })
}

#[test]
fn writes_over_https_with_the_ca_of_a_file_or_a_store() {
    let got = [("file", CA_FILE), ("secret", "plant.influx_ca")].map(
        |(function, argument)| {
            run_tls(move |node, tasks| async move {
                node.paths()
                    .write(std::path::Path::new(CA_FILE), CA)
                    .expect("writes");
                let ca = "plant.influx_ca".parse().expect("a name");
                testing::create_secret(&node.files(), &ca, CA).await;
                let plant =
                    Plant::tls(node, tasks, Certificate::Valid, Roots::default()).await;
                let config = with_ca(&plant, function, argument);
                let (_, errors, end) = send_one(&plant, config).await;
                let points = plant.store.borrow().points("telemetry", &[]).count();
                (errors, end.is_none(), points)
            })
        },
    );
    assert_eq!(
        got,
        [
            (vec![String::new()], true, 1),
            (vec![String::new()], true, 1)
        ]
    );
}

#[test]
fn writes_over_https_with_the_roots_of_the_node_and_no_ca() {
    assert_eq!(
        send_over_tls(Certificate::Valid, connector::http::sim::ca(), b""),
        (vec![String::new()], true, 1, vec![None], vec![0])
    );
}

#[test]
fn shows_why_tls_fails_and_retries_in_the_run_with_no_ca() {
    let failed = |reasons: Vec<String>| {
        let errors = reasons.into_iter().map(|reason| {
            format!(
                "the write to InfluxDB failed: TLS with server:8086 failed: {reason}"
            )
        });
        let mut errors: Vec<_> = errors.collect();
        errors.insert(0, String::new());
        errors.push(String::new());
        errors
    };
    // Each attempt checks the certificate at its own mesh time.
    let expired = [1, 2, 3, 6, 9].map(|after: u64| {
        format!(
            "invalid peer certificate: certificate expired: verification time {} \
             (UNIX), but certificate is not valid after 1769904000 ({} seconds ago)",
            1_780_272_000 + after,
            10_368_000 + after
        )
    });
    for (certificate, reasons) in [
        (
            Certificate::OtherHost,
            vec![
                "invalid peer certificate: certificate not valid for name \"server\"; \
                 certificate is only valid for DnsName(\"other\")"
                    .to_owned(),
            ],
        ),
        (Certificate::Expired, expired.to_vec()),
        (
            Certificate::OtherCa,
            vec!["invalid peer certificate: UnknownIssuer".to_owned()],
        ),
    ] {
        let (errors, going, points, _, restarts) =
            send_over_tls(certificate, connector::http::sim::ca(), b"");
        assert_eq!(
            (errors, going, points, restarts),
            (failed(reasons), true, 0, vec![0]),
            "{certificate:?}"
        );
    }
}

#[test]
fn retries_a_tls_failure_with_a_ca_in_the_run() {
    let (errors, going, points, _, restarts) =
        send_over_tls(Certificate::OtherCa, Roots::default(), CA);
    let error = format!(
        "the write to InfluxDB failed: TLS with server:8086 failed: invalid peer \
         certificate: UnknownIssuer. Check the ca file(\"{CA_FILE}\")"
    );
    assert_eq!(
        (errors, going, points, restarts),
        (vec![String::new(), error, String::new()], true, 0, vec![0])
    );
}

/// Runs a plant with no `hold` whose InfluxDB serves [`Certificate::OtherCa`], with
/// `ca` read from `file(CA_FILE)`, which holds [`CA`] at the start and each text of
/// `texts` from its second after the first sample. Writes a second sample 8 s after
/// the first. Gives the points stored, the errors, and each value of `class` and of
/// `restarts`.
fn after_a_new_ca(
    texts: &'static [(i64, &'static [u8])],
) -> (usize, Vec<String>, Vec<Option<Class>>, Vec<u64>) {
    run_tls(move |node, tasks| async move {
        let paths = node.paths();
        paths
            .write(std::path::Path::new(CA_FILE), CA)
            .expect("writes");
        let plant =
            Plant::tls(node, tasks, Certificate::OtherCa, Roots::default()).await;
        let mut config = with_ca(&plant, "file", CA_FILE);
        config.blocks.clear();
        let run = plant.supervise(config).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        let clock = plant.clock.clone();
        plant.tasks.spawn(async move {
            let mut slept = 0;
            for &(second, text) in texts {
                let wait = (second - slept) * 1_000_000_000;
                clock.sleep(Span::from_nanos(wait)).await;
                slept = second;
                paths
                    .write(std::path::Path::new(CA_FILE), text)
                    .expect("writes");
            }
        });
        plant.clock.sleep(Span::from_nanos(8_000_000_000)).await;
        let second = writer.now().nanos();
        write(&mut writer, second, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::from_nanos(80_000_000_000)).await;
        let end = run.stop().await;
        assert!(end.is_ok(), "ok after a cancel");
        let points = plant.store.borrow().points("telemetry", &[]).count();
        let errors = plant.errors();
        let classes = plant.classes();
        let restarts = plant.restarts();
        (points, errors, classes, restarts)
    })
}

#[test]
fn reads_again_a_ca_fixed_while_the_node_runs() {
    let (points, _, _, restarts) = after_a_new_ca(&[(5, OTHER_CA)]);
    assert_eq!(
        (points, restarts),
        (2, vec![0]),
        "the run reads the fixed ca"
    );
}

#[test]
fn keeps_the_batch_at_a_ca_that_gives_no_root_when_read_again() {
    let (points, errors, classes, restarts) =
        after_a_new_ca(&[(5, b"not a certificate\n"), (20, OTHER_CA)]);
    let failed = format!(
        "the write to InfluxDB failed: TLS with server:8086 failed: invalid peer \
         certificate: UnknownIssuer. Check the ca file(\"{CA_FILE}\")"
    );
    let empty = format!(
        "an attempt failed: the ca file(\"{CA_FILE}\") holds no PEM \
         certificate. Write the CA certificate of the server in PEM"
    );
    assert_eq!(
        (points, errors, classes, restarts),
        (
            2,
            vec![String::new(), failed, empty, String::new()],
            vec![None, RETRY, None],
            vec![0]
        )
    );
}

/// With no `hold`, InfluxDB closes the first connection in the TLS handshake, then
/// serves. Gives the points stored and each value of `restarts`.
fn after_a_close_in_the_handshake(ca: bool) -> (usize, Vec<u64>) {
    run_tls(move |node, tasks| async move {
        node.paths()
            .write(std::path::Path::new(CA_FILE), CA)
            .expect("writes");
        let roots = if ca {
            Roots::default()
        } else {
            connector::http::sim::ca()
        };
        let plant =
            Plant::with(node, tasks, Scheme::Tls(Certificate::Valid), roots).await;
        let listen = tcp::Listen {
            local: plant.address,
            backlog: 4,
            options: OPTIONS,
        };
        let mut listener = plant.net.listen(&listen).expect("the port is free");
        let mut config = if ca {
            with_ca(&plant, "file", CA_FILE)
        } else {
            plant.config()
        };
        config.blocks.clear();
        let run = plant.supervise(config).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        let tcp = std::future::poll_fn(|cx| listener.poll_accept(cx))
            .await
            .expect("accepts");
        drop((tcp, listener));
        plant.clock.sleep(Span::from_nanos(10_000_000)).await;
        plant.serve();
        plant.clock.sleep(Span::MINUTE).await;
        let end = run.stop().await;
        assert!(end.is_ok(), "ok after a cancel");
        let points = plant.store.borrow().points("telemetry", &[]).count();
        let restarts = plant.restarts();
        (points, restarts)
    })
}

#[test]
fn keeps_the_sample_of_a_close_in_the_handshake_with_no_hold() {
    let got = [false, true].map(after_a_close_in_the_handshake);
    assert_eq!(got, [(1, vec![0]), (1, vec![0])]);
}

#[test]
fn keeps_the_roots_of_the_node_with_a_ca() {
    assert_eq!(
        send_over_tls(Certificate::Valid, connector::http::sim::ca(), OTHER_CA),
        (vec![String::new()], true, 1, vec![None], vec![0])
    );
}

#[test]
fn reads_a_ca_that_starts_with_a_byte_order_mark() {
    const MARKED: &[u8] = &{
        let mut marked = [0; 3 + CA.len()];
        let (mark, rest) = marked.split_at_mut(3);
        mark.copy_from_slice(b"\xEF\xBB\xBF");
        rest.copy_from_slice(CA);
        marked
    };
    assert_eq!(
        send_over_tls(Certificate::Valid, Roots::default(), MARKED),
        (vec![String::new()], true, 1, vec![None], vec![0])
    );
}

#[test]
fn shows_that_no_ca_is_trusted_with_no_roots_and_no_ca() {
    let (errors, going, points, ..) =
        send_over_tls(Certificate::Valid, Roots::default(), b"");
    assert_eq!(
        (errors, going, points),
        (
            vec![
                String::new(),
                "the write to InfluxDB failed: no CA is trusted: install the CA \
                 certificates of the OS, or give `ca`"
                    .to_owned(),
                String::new(),
            ],
            true,
            0
        )
    );
}

#[test]
fn ends_the_run_at_a_ca_that_gives_no_root() {
    for (ca, held) in [
        (&b"not a certificate\n"[..], "no PEM certificate"),
        (
            b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
            "a PEM certificate that does not parse",
        ),
    ] {
        let (errors, _, points, classes, _) =
            send_over_tls(Certificate::Valid, connector::http::sim::ca(), ca);
        let error = format!(
            "the ca file(\"{CA_FILE}\") holds {held}. Write the CA certificate of the \
             server in PEM"
        );
        assert_eq!(
            (errors, points, classes),
            (vec![String::new(), error], 0, vec![None, RETRY]),
            "{held}"
        );
    }
}

#[test]
fn retries_a_refused_connect_in_the_run_with_a_ca() {
    let got = run_tls(|node, tasks| async move {
        let paths = node.paths();
        paths
            .write(std::path::Path::new(CA_FILE), CA)
            .expect("writes");
        let plant = Plant::with(
            node,
            tasks,
            Scheme::Tls(Certificate::Valid),
            Roots::default(),
        )
        .await;
        let run = plant.supervise(with_ca(&plant, "file", CA_FILE)).await;
        let mut writer = plant.writer(&["edge.time", "edge.a", "edge.b"]).await;
        plant.clock.sleep(Span::SECOND).await;
        let first = writer.now().nanos();
        write(&mut writer, first, &[(1.0, 2.0)]);
        plant.clock.sleep(Span::from_nanos(500_000_000)).await;
        paths
            .write(std::path::Path::new(CA_FILE), b"not a certificate\n")
            .expect("writes");
        plant.clock.sleep(Span::from_nanos(5_500_000_000)).await;
        plant.serve();
        plant.clock.sleep(Span::MINUTE).await;
        let end = run.stop().await;
        assert!(end.is_ok(), "ok after a cancel");
        let points = plant.store.borrow().points("telemetry", &[]).count();
        let errors = plant.errors();
        let restarts = plant.restarts();
        (points, errors, restarts)
    });
    let refused = "the write to InfluxDB failed: the connect failed: 10.0.0.1:8086 \
                   refused the connection";
    let errors = vec![String::new(), refused.to_owned(), String::new()];
    assert_eq!(
        got,
        (1, errors, vec![0]),
        "a refused connect keeps the run and does not read the ca again"
    );
}
