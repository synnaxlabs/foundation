use std::ffi::CString;
use std::future;
use std::net::SocketAddr;

use ::sim::{Sim, node};
use connector::browse::{Device, End, Found, Query, Scope, Values};
use connector::kind::Table;
use connector::testing::Rig;
use document::{Document, Map};
use env::net::tcp;
use types::sample::Scalar;
use types::time::{Span, Stamp};

use super::*;
use crate::client::browse::Foreign;
use crate::client::tests::common::{config, env, rig};
use crate::client::tests::{RELAY, Relay, relayed};
use crate::event::Time;
use crate::ffi::UNIX_EPOCH;
use crate::kind::Kind;
use crate::sim::{Variables, during, serve, tree};

/// The sim tree of `sim::serve` from its folder `Plant`.
const PLANT: &str = "Plant";

/// What `Supervisor::browse` of the `opcua` kind gives for each of `queries` in
/// turn, with the config that `config` makes of the address of a sim server, which
/// runs on port 4840 when `served`.
fn browse_each(
    config: fn(&str) -> Document,
    served: bool,
    queries: Vec<Query>,
) -> Vec<Result<Found, Error>> {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let rig = rig(&node, tasks).await;
        let local = SocketAddr::new(node.addresses()[0], 4840);
        let config = config(&format!("opc.tcp://{local}"));
        let mut found = Vec::new();
        let browse = async {
            for query in &queries {
                found.push(rig.supervisor().browse("opcua", &config, query).await);
            }
        };
        if !served {
            browse.await;
            return found;
        }
        serving(&node, browse).await;
        found
    });
    run.expect("the run ends")
}

/// What `Supervisor::browse` of the `opcua` kind gives for `query` through a relay to
/// a sim server, on a node whose mesh time starts `delay` after the call. Also gives
/// the mesh time once the browse ends and the time of the request header of each
/// Browse request, both in 100 ns from 1601.
fn browse_relayed(delay: Span, query: Query) -> (Result<Found, Error>, i64, Vec<i64>) {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    let run = sim.run_on(&node, move |node, tasks| async move {
        let kinds = Table::new().with("opcua", Kind::default());
        let rig =
            Rig::unsynced(env(&node, tasks.clone()), node.net(), kinds, delay).await;
        let control = Relay::default();
        let config = config(&format!("opc.tcp://{}:{RELAY}", node.addresses()[0]));
        let browse = rig.supervisor().browse("opcua", &config, &query);
        let mut found = None;
        let until =
            async { found = Some(relayed(&node, &control, |_| {}, browse).await) };
        serving(&node, until).await;
        let end = ticks(rig.hub().time().await.now());
        (found.expect("the browse ends"), end, control.browses.take())
    });
    run.expect("the run ends")
}

/// `stamp` in 100 ns from 1601.
fn ticks(stamp: Stamp) -> i64 {
    stamp.nanos() / 100 + UNIX_EPOCH
}

#[test]
fn a_browse_puts_the_mesh_time_in_the_header_of_each_browse_request() {
    let (found, end, browses) = browse_relayed(Span::ZERO, level(PLANT, None));
    assert_eq!(found.expect("found").end, End::Done);
    assert!(!browses.is_empty(), "no Browse request");
    for time in browses {
        let second = end - 10_000_000..=end;
        assert!(second.contains(&time), "{time} is not in {second:?}");
    }
}

#[test]
fn a_browse_on_a_node_with_no_mesh_time_gives_a_retry() {
    let delay = Span::from_nanos(6_000_000_000);
    let (found, _, browses) = browse_relayed(delay, level(PLANT, None));
    let Err(Error::Retry(error)) = found else {
        panic!("not a retry: {found:?}");
    };
    assert_eq!(error.to_string(), "the node has no mesh time after 5s");
    assert_eq!(browses, []);
}

/// Serves the sim tree on port 4840 of `node` until `until` ends.
async fn serving(node: &node::Node, until: impl Future<Output = ()>) {
    serving_of(node, &Variables::new(&[]), until).await;
}

/// Serves `variables` at port 4840 of `node` until `until` ends.
async fn serving_of(
    node: &node::Node,
    variables: &Variables,
    until: impl Future<Output = ()>,
) {
    let listen = tcp::Listen {
        local: SocketAddr::new(node.addresses()[0], 4840),
        backlog: 4,
        options: tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("not 0"),
            delayed: false,
        },
    };
    let listener = node.net().listen(&listen).expect("the port is free");
    let wall = node.wall();
    #[expect(
        clippy::disallowed_methods,
        reason = "a simulated device reads the wall time of its node"
    )]
    let wall = move || wall.now().time;
    let mut rng = node.entropy().rng();
    let (clock, net) = (node.clock(), node.net());
    let serving = |stop| serve(clock, wall, net, listener, &mut rng, variables, stop);
    during(serving, until).await;
}

/// What one browse of `query` gives on a sim server.
fn browse(query: Query) -> Result<Found, Error> {
    browse_each(config, true, vec![query]).remove(0)
}

fn query(start: &str, scope: Scope, values: Values) -> Query {
    Query {
        start: start.parse().expect("a path"),
        scope,
        values,
    }
}

fn level(start: &str, cursor: Option<&str>) -> Query {
    let cursor = cursor.map(|text| Cursor::new(text.to_owned()));
    query(start, Scope::Level(cursor), Values::Read)
}

fn matching(start: &str, pattern: &str) -> Query {
    let pattern = pattern.parse().expect("a pattern");
    query(start, Scope::Match(pattern), Values::Read)
}

/// The tag of the node of the name `name` in namespace 1.
fn tag(name: &str) -> String {
    format!("ns=1;s={name}")
}

/// An entry at `path` of the node of the name `name` in namespace 1.
fn entry(path: &str, name: &str, shape: Shape) -> Entry {
    Entry {
        path: path.parse().expect("a path"),
        tag: tag(name),
        shape,
    }
}

/// A point of `data_type` that a connector can read, with no unit.
fn point(data_type: Option<Scalar>) -> Shape {
    Shape::Point(Facts {
        data_type: data_type.map(Type::Scalar),
        unit: None,
        unit_code: None,
        access: Some(Access::Read),
    })
}

fn unreadable(reason: &str) -> Shape {
    Shape::Unreadable(reason.to_owned())
}

/// The one diagnostic of `result`, which is [`Error::Config`].
fn diagnostic(result: Result<Found, Error>) -> Diagnostic {
    match result {
        Err(Error::Config(mut diagnostics)) if diagnostics.len() == 1 => {
            diagnostics.remove(0)
        }
        other => panic!("not one config diagnostic: {other:?}"),
    }
}

#[test]
fn gives_the_shape_facts_and_value_of_each_entry_of_a_level() {
    let found = browse(level("Plant/Line", None)).expect("found");
    let line = |name: &str, shape| {
        entry(
            &format!("Plant/Line/{name}"),
            &format!("Plant.Line.{name}"),
            shape,
        )
    };
    let temperature = Shape::Point(Facts {
        data_type: Some(Type::Scalar(Scalar::F64)),
        unit: Some("°C".to_owned()),
        unit_code: Some("CEL".to_owned()),
        access: Some(Access::Read),
    });
    assert_eq!(
        found.entries,
        [
            entry("Plant/Line/Plant", PLANT, Shape::Folder),
            line("Temperature", temperature),
            line("Speed", point(Some(Scalar::F64))),
            line("Mode", point(Some(Scalar::I32))),
            line("Profile", unreadable("an array")),
            line(
                "Locked",
                unreadable("the server does not let this user read it")
            ),
            line("Recipe", point(None)),
        ]
    );
    assert_eq!(
        found.values,
        [
            None,
            Some(sample::Value::F64(21.5)),
            Some(sample::Value::F64(1.25)),
            Some(sample::Value::I32(0)),
            None,
            None,
            None,
        ]
    );
    assert_eq!(found.end, End::Done);
}

/// The names of `Plant/Types`, in the order of the server.
const TYPES: [&str; 15] = [
    "Boolean",
    "SByte",
    "Byte",
    "Int16",
    "UInt16",
    "Int32",
    "UInt32",
    "Int64",
    "UInt64",
    "Float",
    "Double",
    "String",
    "DateTime",
    "Guid",
    "ByteString",
];

/// The entries of `Plant/Types` from `from` to `to`.
fn types(from: usize, to: usize) -> Vec<Entry> {
    let scalars = [
        Scalar::Bool,
        Scalar::I8,
        Scalar::U8,
        Scalar::I16,
        Scalar::U16,
        Scalar::I32,
        Scalar::U32,
        Scalar::I64,
        Scalar::U64,
        Scalar::F32,
        Scalar::F64,
    ];
    (from..to)
        .map(|at| {
            let name = TYPES[at];
            let shape = scalars.get(at).map_or_else(
                || unreadable(&format!("a value of the type {name}")),
                |&scalar| point(Some(scalar)),
            );
            entry(
                &format!("Plant/Types/{name}"),
                &format!("Plant.Types.{name}"),
                shape,
            )
        })
        .collect()
}

#[test]
fn pages_a_level_with_a_cursor_that_a_new_browse_takes() {
    let found = browse_each(
        config,
        true,
        vec![level("Plant/Types", None), level("Plant/Types", Some("8"))],
    );
    let [first, second] = <[_; 2]>::try_from(found).expect("two browses");
    let (first, second) = (first.expect("found"), second.expect("found"));
    assert_eq!(first.entries, types(0, 8));
    assert_eq!(first.end, End::Next(Cursor::new("8".to_owned())));
    assert_eq!(second.entries, types(8, 15));
    assert_eq!(second.end, End::Done);
    assert_eq!(
        second.values,
        [
            Some(sample::Value::U64(64)),
            Some(sample::Value::F32(0.5)),
            Some(sample::Value::F64(0.25)),
            None,
            None,
            None,
            None,
        ]
    );
}

#[test]
fn matches_each_point_of_a_folder_of_two_pages_in_one_browse() {
    let found = browse(matching("Plant/Types", "*")).expect("found");
    assert_eq!(found.entries, types(0, 15));
    let mut values = vec![
        Some(sample::Value::Bool(true)),
        Some(sample::Value::I8(-8)),
        Some(sample::Value::U8(8)),
        Some(sample::Value::I16(-16)),
        Some(sample::Value::U16(16)),
        Some(sample::Value::I32(-32)),
        Some(sample::Value::U32(32)),
        Some(sample::Value::I64(-64)),
        Some(sample::Value::U64(64)),
        Some(sample::Value::F32(0.5)),
        Some(sample::Value::F64(0.25)),
    ];
    values.resize(15, None);
    assert_eq!(found.values, values);
    assert_eq!(found.end, End::Done);
}

#[test]
fn matches_each_point_under_a_folder_once_past_a_loop() {
    let pattern = "**".parse().expect("a pattern");
    let found = browse(query(PLANT, Scope::Match(pattern), Values::Skipped));
    let found = found.expect("found");
    let paths: Vec<String> = (found.entries.iter())
        .map(|entry| entry.path.to_string())
        .collect();
    let odd = ["guid", "Level", "Level (ns=1;s=Plant.Odd.Twin)"];
    let line = [
        "Temperature",
        "Speed",
        "Mode",
        "Profile",
        "Locked",
        "Recipe",
    ];
    let expected: Vec<String> = (TYPES.iter().map(|name| format!("Types/{name}")))
        .chain(odd.iter().map(|name| format!("Odd/{name}")))
        .chain(line.iter().map(|name| format!("Line/{name}")))
        .map(|path| format!("Plant/{path}"))
        .collect();
    assert_eq!(paths, expected);
    assert_eq!(found.values, []);
    assert_eq!(found.end, End::Done);
}

/// The entry `Twin` of `Plant/Odd`, named apart from `Level`.
fn twin() -> Entry {
    let name = "Level (ns=1;s=Plant.Odd.Twin)";
    entry(
        &format!("Plant/Odd/{name}"),
        "Plant.Odd.Twin",
        point(Some(Scalar::F64)),
    )
}

#[test]
fn drops_a_repeated_node_and_names_a_repeated_name_apart() {
    let found = browse(level("Plant/Odd", None)).expect("found");
    let guid = Entry {
        path: "Plant/Odd/guid".parse().expect("a path"),
        tag: tree::GUID.to_owned(),
        shape: unreadable(
            "its node ID is a GUID or opaque, which the kind cannot read",
        ),
    };
    assert_eq!(
        found.entries,
        [
            guid,
            entry(
                "Plant/Odd/Level",
                "Plant.Odd.Level",
                point(Some(Scalar::F64))
            ),
            twin(),
        ]
    );
    assert_eq!(
        found.values,
        [
            None,
            Some(sample::Value::F64(2.5)),
            Some(sample::Value::F64(3.5))
        ]
    );
}

#[test]
fn a_cursor_of_a_new_browse_drops_and_names_as_the_pages_before_it() {
    let found = browse(level("Plant/Odd", Some("2"))).expect("found");
    assert_eq!(found.entries, [twin()]);
    assert_eq!(found.end, End::Done);
}

#[test]
fn refuses_a_cursor_that_is_not_a_count() {
    let error = diagnostic(browse(level(PLANT, Some("next"))));
    assert_eq!(error.code, BAD_CURSOR);
    assert_eq!(
        error.to_string(),
        "the cursor \"next\" is not a cursor of the opcua kind. Browse from the first \
         page, with no cursor"
    );
}

#[test]
fn refuses_a_cursor_with_no_point_past_the_skip_bound() {
    let error = diagnostic(browse(level(PLANT, Some("100001"))));
    assert_eq!(error.code, BAD_CURSOR);
    assert_eq!(
        error.to_string(),
        "the cursor \"100001\" is not a cursor of the opcua kind. Browse from the \
         first page, with no cursor"
    );
    let found = browse(level(PLANT, Some("100000"))).expect("found");
    assert_eq!((found.entries, found.end), (Vec::new(), End::Done));
}

#[test]
fn refuses_a_config_with_no_address() {
    let none = |_: &str| Document {
        attributes: Map::new(Vec::new()).expect("no keys"),
        blocks: Vec::new(),
    };
    let found = browse_each(none, true, vec![level(PLANT, None)]).remove(0);
    let error = diagnostic(found);
    assert_eq!(error.code, Code::new("document.missing-attribute"));
    assert_eq!(
        error.to_string(),
        "the connector has no `address`. Add an `address` attribute with the endpoint \
         of the server, such as \"opc.tcp://plc:4840\""
    );
}

#[test]
fn fails_when_the_server_does_not_answer() {
    let found = browse_each(config, false, vec![level(PLANT, None)]).remove(0);
    let Err(Error::Device(source)) = found else {
        panic!("not a device error: {found:?}");
    };
    assert_eq!(
        source.to_string(),
        "the connect to opc.tcp://10.0.0.1:4840 failed with BadConnectionRejected"
    );
}

/// Gives the output of `body` with a browse of a client of a sim server.
fn on_server<T: Send + 'static>(
    body: impl AsyncFnOnce(&mut Server<'_>) -> T + Send + 'static,
) -> T {
    on_server_of(Variables::new(&[]), body)
}

/// As [`on_server`], with a sim server of `variables`.
fn on_server_of<T: Send + 'static>(
    variables: Variables,
    body: impl AsyncFnOnce(&mut Server<'_>) -> T + Send + 'static,
) -> T {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    let run = sim.run_on(&node, move |node, _| async move {
        let url = format!("opc.tcp://{}:4840", node.addresses()[0]);
        let url = CString::new(url).expect("no NUL");
        let mut out = None;
        let until = async {
            let (time, mut rng) = (Time::monotonic(node.clock()), node.entropy().rng());
            let connect = Client::connect(
                time,
                tracing::Span::none(),
                node.net(),
                &mut rng,
                &url,
                None,
                future::pending(),
            );
            let mut client = connect.await.expect("a connect").expect("a client");
            out = Some(body(&mut Server::new(&mut client)).await);
            client.close().await;
        };
        serving_of(&node, &variables, until).await;
        out.expect("the body ends")
    });
    run.expect("the run ends")
}

#[test]
fn gives_no_value_for_each_node_whose_read_alone_does_not_fit_a_message() {
    let values = on_server(async |server| {
        let long = |name: &str| format!("ns=1;s={}", name.repeat(600_000));
        let (x, y) = (long("x"), long("y"));
        server.values(&[&x, "ns=1;s=Plant.Odd.Level", &y]).await
    });
    let values = values.expect("values");
    assert_eq!(values, [None, Some(sample::Value::F64(2.5)), None]);
}

#[test]
fn names_the_endpoint_when_a_read_of_values_fails() {
    let variables = Variables::new(&[]);
    variables.refuse_reads_past(1);
    let values = on_server_of(variables, async |server| {
        let level = "ns=1;s=Plant.Odd.Level";
        server.values(&[level, level]).await
    });
    let Err(Error::Device(source)) = values else {
        panic!("not a device error: {values:?}");
    };
    assert_eq!(
        source.to_string(),
        "the read from opc.tcp://10.0.0.1:4840 failed with BadTooManyOperations"
    );
}

/// The open62541 server frees the oldest of the 32 points that a session holds when a
/// Browse needs one more.
#[test]
fn browses_a_folder_again_when_the_server_freed_the_point_of_a_cursor() {
    let pages = on_server(async |server| {
        let folder = entry("Plant/Types", "Plant.Types", Shape::Folder);
        let first = server.level(Some(&folder), None).await.expect("a page");
        let first = first.expect("a folder");
        let node = tag("Plant.Types").parse().expect("a node");
        for _ in 0..32 {
            let page = server.client.browse(&node, NonZeroU32::MIN).await;
            assert!(page.expect("a browse").expect("a node").next.is_some());
        }
        let cursor = first.next.clone().expect("a cursor");
        let second = server.level(Some(&folder), Some(&cursor)).await;
        (first, second.expect("a page").expect("a folder"))
    });
    let (first, second) = pages;
    let names = |page: &Page| -> Vec<String> {
        page.entries
            .iter()
            .map(|child| child.name.clone())
            .collect()
    };
    assert_eq!(names(&first), TYPES[..8]);
    assert_eq!(first.next, Some(Cursor::new("8".to_owned())));
    assert_eq!(names(&second), TYPES[8..]);
    assert_eq!(second.next, None);
}

/// The page of a cursor of `Plant/Types` at the count `before`, after the server
/// frees its point, or after this browse releases it when `released`.
fn moved_cursor(before: usize, released: bool) -> Result<Option<Page>, Error> {
    on_server(async move |server| {
        let folder = entry("Plant/Types", "Plant.Types", Shape::Folder);
        let first = server.level(Some(&folder), None).await.expect("a page");
        assert!(first.expect("a folder").next.is_some());
        let node: Node = tag("Plant.Types").parse().expect("a node");
        let held = server.points.remove(&(node.clone(), 8)).expect("a point");
        server.points.insert((node.clone(), before), held);
        let run = server.given.remove(&(node.clone(), 8)).expect("a cursor");
        server.given.insert((node.clone(), before), run);
        if released {
            server.release(1).await.expect("a release");
        } else {
            for _ in 0..32 {
                let page = server.client.browse(&node, NonZeroU32::MIN).await;
                assert!(page.expect("a browse").expect("a node").next.is_some());
            }
        }
        let cursor = Cursor::new(before.to_string());
        server.level(Some(&folder), Some(&cursor)).await
    })
}

/// A cursor at `SKIP` whose point the server freed, or this browse released, browses
/// the folder again from its start.
#[test]
fn a_cursor_at_the_skip_bound_with_no_point_browses_again() {
    for released in [false, true] {
        let page = moved_cursor(100_000, released).expect("a page");
        let page = page.expect("a folder");
        assert_eq!((page.entries, page.next), (Vec::new(), None), "{released}");
    }
}

/// The browse does not release a point past `SKIP`, as its cursor cannot browse the
/// folder again: the cursor goes on from its point.
#[test]
fn releases_no_point_past_the_skip_bound() {
    let page = on_server(async |server| {
        let folder = entry("Plant/Types", "Plant.Types", Shape::Folder);
        let first = server.level(Some(&folder), None).await.expect("a page");
        assert!(first.expect("a folder").next.is_some());
        let node: Node = tag("Plant.Types").parse().expect("a node");
        let held = server.points.remove(&(node.clone(), 8)).expect("a point");
        server.points.insert((node.clone(), 100_001), held);
        let run = server.given.remove(&(node.clone(), 8)).expect("a cursor");
        server.given.insert((node, 100_001), run);
        server.release(usize::MAX).await.expect("a release");
        let cursor = Cursor::new("100001".to_owned());
        server.level(Some(&folder), Some(&cursor)).await
    });
    let page = page.expect("a page").expect("a folder");
    let names: Vec<String> = page.entries.into_iter().map(|child| child.name).collect();
    assert_eq!(names, TYPES[8..]);
    assert_eq!(page.next, None);
}

#[test]
fn gives_none_for_a_folder_that_the_server_does_not_have() {
    let page = on_server(async |server| {
        let folder = entry("Plant/Gone", "Plant.Gone", Shape::Folder);
        server.level(Some(&folder), None).await
    });
    assert!(matches!(page, Ok(None)), "{page:?}");
}

fn foreign(text: &[u8], cause: Cause) -> Reference {
    Reference {
        node: Err(Foreign {
            text: text.to_vec(),
            cause,
        }),
        name: String::new(),
        display: String::new(),
        class: Class::Variable,
        via: None,
    }
}

#[test]
fn the_tag_of_a_foreign_node_is_its_escaped_text_that_no_node_reads() {
    let tags = [
        (&b"s=a%\xFF"[..], Cause::Name, "s%=a%25%FF"),
        (b"ns=2;s=", Cause::Name, "ns=2;s%="),
        (
            b"ns=1;g=09087e75-8e5e-499b-954f-f2a9603db28a",
            Cause::Key,
            "",
        ),
        (b"svr=1;ns=1;s=a", Cause::Remote, "svr=1;ns=1;s=a"),
    ];
    for (text, cause, expected) in tags {
        let reference = foreign(text, cause);
        let expected = if expected.is_empty() {
            str::from_utf8(text).expect("UTF-8")
        } else {
            expected
        };
        assert_eq!(super::tag(&reference), expected);
        assert!(
            expected.parse::<Node>().is_err(),
            "{expected} reads as a node"
        );
    }
}
