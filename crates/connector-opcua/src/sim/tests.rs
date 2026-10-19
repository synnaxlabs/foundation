use std::ffi::{CStr, CString};
use std::future;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::mpsc;

use ::sim::{Sim, node};
use connector::cancel::Token;
use env::net::{self, Listener, tcp};
use types::quality::Quality;
use types::time::{Span, Stamp};

use super::{Served, Variables, during, serve, tree};
use crate::client::Client;
use crate::client::browse::{
    Base, Cause, Class, Description, Foreign, Page, Reference, Unit,
};
use crate::connection::OPTIONS;
use crate::event;
use crate::ffi::Status;
use crate::node::Node;
use crate::node::ns0::{self, HAS_COMPONENT, ORGANIZES};
use crate::value::{Fault, Type, Value};

const SPIKE: &str = "ns=3;s=SpikeData";
const DIP: &str = "ns=3;i=7";
const BAD: Quality = Quality(0x8000_0000);
const CLAMPED: Quality = Quality(0x0030_0000);
const UNCERTAIN: Quality = Quality(0x4090_0000);
const BAD_USER_ACCESS_DENIED: Status = Status(0x801F_0000);
const QUALITIES: [Quality; 4] = [Quality::GOOD, CLAMPED, UNCERTAIN, BAD];
/// The reads of a variable with each of [`QUALITIES`] in turn.
const READS: [Result<Value, Fault>; 4] = [
    Ok(Value::F64(1.0)),
    Ok(Value::F64(2.0)),
    Err(Fault::status(Status(UNCERTAIN.0))),
    Err(Fault::status(Status(BAD.0))),
];

/// Binds a listener of `node` on its port 4840.
fn listen(node: &node::Node) -> Listener {
    let local = SocketAddr::new(node.addresses()[0], 4840);
    let listen = tcp::Listen {
        local,
        backlog: 4,
        options: tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("not 0"),
            delayed: false,
        },
    };
    node.net().listen(&listen).expect("the port is free")
}

#[test]
fn a_server_answers_each_read_with_its_variable_and_records_it() {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    sim.run_on(&node, move |node, _| async move {
        let listener = listen(&node);
        let url =
            CString::new(format!("opc.tcp://{}", listener.local())).expect("no NUL");
        let mut rng = node.entropy().rng();
        let variables = Variables::new(&[SPIKE, DIP]);
        let (clock, spike) = (node.clock(), SPIKE.parse::<Node>().expect("a node"));
        let mut times = Vec::new();
        let mut reads = Vec::new();
        let until = async {
            let mut client = connect(&node, &mut rng, &url).await.expect("connect");
            for quality in QUALITIES {
                variables.change(quality);
                let before = clock.now();
                reads.push(client.read_one(&spike).await);
                times.push((before, clock.now()));
            }
            client.close().await;
        };
        let mut served = node.entropy().rng();
        let serving = |stop| {
            serve(
                node.clock(),
                event::Time::monotonic(node.clock()).wall,
                node.net(),
                listener,
                &mut served,
                &variables,
                stop,
            )
        };
        during(serving, until).await;
        assert_eq!(reads, READS);
        let record = variables.served();
        assert_eq!(record[DIP], []);
        let spike = &record[SPIKE];
        let answers: Vec<_> = (spike.iter())
            .map(|served| (served.value, served.quality))
            .collect();
        let expected: Vec<_> =
            [1.0, 2.0, 3.0, 4.0].into_iter().zip(QUALITIES).collect();
        assert_eq!(answers, expected);
        for (Served { time, .. }, (before, after)) in spike.iter().zip(times) {
            assert!(
                before <= *time && *time <= after,
                "{time:?} in {before:?}..{after:?}"
            );
        }
        let again = listen(&node);
        let serving = |stop| {
            serve(
                node.clock(),
                event::Time::monotonic(node.clock()).wall,
                node.net(),
                again,
                &mut served,
                &variables,
                stop,
            )
        };
        during(serving, async {}).await;
    })
    .expect("the run ends");
}

/// A client reads the `CurrentTime` of a server on the clock that `set_clock` puts
/// ahead of the wall time, and a later call moves it, also behind.
#[test]
fn a_server_gives_its_current_time_on_the_clock_as_set() {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    sim.run_on(&node, move |node, _| async move {
        let listener = listen(&node);
        let url =
            CString::new(format!("opc.tcp://{}", listener.local())).expect("no NUL");
        let mut rng = node.entropy().rng();
        let variables = Variables::new(&[SPIKE]);
        let wall = event::Time::monotonic(node.clock()).wall;
        let minute = Span::MINUTE.nanos();
        let aheads = [0, 5 * minute, -minute].map(Span::from_nanos);
        let mut times = Vec::new();
        let until = async {
            let mut client = connect(&node, &mut rng, &url).await.expect("connect");
            for ahead in aheads {
                if ahead != Span::ZERO {
                    variables.set_clock(ahead);
                }
                let before = wall();
                let time = client.time().await.expect("a time");
                times.push((before + ahead, time, wall() + ahead));
            }
            client.close().await;
        };
        let mut served = node.entropy().rng();
        let serving = |stop| {
            serve(
                node.clock(),
                event::Time::monotonic(node.clock()).wall,
                node.net(),
                listener,
                &mut served,
                &variables,
                stop,
            )
        };
        during(serving, until).await;
        assert_eq!(times.len(), aheads.len());
        for (before, time, after) in times {
            // The server clock has ticks of 100 ns.
            let tick = Stamp::from_nanos(before.nanos().div_euclid(100) * 100);
            assert!(tick <= time && time <= after, "{time} in {tick}..{after}");
        }
    })
    .expect("the run ends");
}

/// A client connects to a server whose clock `set_clock` puts past the last stamp,
/// as the token time is off too, and reads the last stamp in ticks of 100 ns.
#[test]
fn a_server_clock_past_the_last_stamp_stays_at_it() {
    let last = Ok(Stamp::from_nanos(i64::MAX / 100 * 100));
    reads_time(Stamp::from_nanos(1), Span::from_nanos(i64::MAX), last);
}

/// A server clock before the first stamp stays there, which a client reads as out of
/// range: the tick of 100 ns that holds the first stamp starts before it.
#[test]
fn a_server_clock_before_the_first_stamp_reads_as_out_of_range() {
    let out = Err(Status::BAD_OUT_OF_RANGE);
    reads_time(Stamp::from_nanos(-1), Span::from_nanos(i64::MIN), out);
}

/// Asserts that a client reads `time` as the `CurrentTime` of a server whose `wall`
/// stays at `wall`, and whose clock `set_clock` puts `ahead` before the connect.
fn reads_time(wall: Stamp, ahead: Span, time: Result<Stamp, Status>) {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    sim.run_on(&node, move |node, _| async move {
        let listener = listen(&node);
        let url =
            CString::new(format!("opc.tcp://{}", listener.local())).expect("no NUL");
        let mut rng = node.entropy().rng();
        let variables = Variables::new(&[SPIKE]);
        variables.set_clock(ahead);
        let until = async {
            let mut client = connect(&node, &mut rng, &url).await.expect("connect");
            assert_eq!(client.time().await, time);
            client.close().await;
        };
        let mut served = node.entropy().rng();
        let serving = |stop| {
            serve(
                node.clock(),
                move || wall,
                node.net(),
                listener,
                &mut served,
                &variables,
                stop,
            )
        };
        during(serving, until).await;
    })
    .expect("the run ends");
}

#[test]
fn a_server_answers_a_value_of_each_type_and_records_only_a_double() {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    sim.run_on(&node, move |node, _| async move {
        let listener = listen(&node);
        let url =
            CString::new(format!("opc.tcp://{}", listener.local())).expect("no NUL");
        let mut rng = node.entropy().rng();
        let values = [
            Value::Bool(true),
            Value::I8(-8),
            Value::U8(8),
            Value::I16(-16),
            Value::U16(16),
            Value::I32(-32),
            Value::U32(32),
            Value::I64(-64),
            Value::U64(64),
            Value::F32(0.5),
            Value::F64(0.25),
        ];
        let texts: Vec<String> =
            (1..=values.len()).map(|i| format!("ns=2;i={i}")).collect();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        let variables = Variables::new(&texts);
        for (text, value) in texts.iter().zip(values) {
            variables.set(text, value);
        }
        variables.change(Quality::GOOD);
        let mut reads = Vec::new();
        let until = async {
            let mut client = connect(&node, &mut rng, &url).await.expect("connect");
            for text in &texts {
                let node = text.parse::<Node>().expect("a node");
                reads.push(client.read_one(&node).await);
            }
            client.close().await;
        };
        let mut served = node.entropy().rng();
        let serving = |stop| {
            serve(
                node.clock(),
                event::Time::monotonic(node.clock()).wall,
                node.net(),
                listener,
                &mut served,
                &variables,
                stop,
            )
        };
        during(serving, until).await;
        let mut expected = values.map(Ok);
        expected[10] = Ok(Value::F64(1.25));
        assert_eq!(reads, expected);
        let record = variables.served();
        for text in &texts[..10] {
            assert_eq!(record[*text], [], "{text}");
        }
        let doubles: Vec<_> = record[texts[10]]
            .iter()
            .map(|served| served.value)
            .collect();
        assert_eq!(doubles, [1.25]);
    })
    .expect("the run ends");
}

#[test]
fn a_server_has_room_for_4_sessions() {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    sim.run_on(&node, move |node, _| async move {
        let listener = listen(&node);
        let url =
            CString::new(format!("opc.tcp://{}", listener.local())).expect("no NUL");
        let mut rng = node.entropy().rng();
        let variables = Variables::new(&[SPIKE]);
        let spike = SPIKE.parse::<Node>().expect("a node");
        let mut reads = Vec::new();
        let mut fifth = None;
        let until = async {
            let mut clients = Vec::new();
            for _ in 0..4 {
                clients.push(connect(&node, &mut rng, &url).await.expect("connect"));
            }
            for client in &mut clients {
                reads.push(client.read_one(&spike).await);
            }
            fifth = connect(&node, &mut rng, &url).await.err();
            for client in clients {
                client.close().await;
            }
        };
        let mut served = node.entropy().rng();
        let serving = |stop| {
            serve(
                node.clock(),
                event::Time::monotonic(node.clock()).wall,
                node.net(),
                listener,
                &mut served,
                &variables,
                stop,
            )
        };
        during(serving, until).await;
        assert_eq!(reads, [Ok(Value::F64(0.0)); 4]);
        assert_eq!(fifth, Some(Status::BAD_CONNECTION_CLOSED));
        assert_eq!(variables.served()[SPIKE].len(), 4);
    })
    .expect("the run ends");
}

#[test]
fn a_server_panics_on_a_variable_of_the_number_0() {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    let run = sim.run_on(&node, move |node, _| async move {
        let mut rng = node.entropy().rng();
        let variables = Variables::new(&[SPIKE, "ns=3;i=0"]);
        let listener = listen(&node);
        let serving = |stop| {
            serve(
                node.clock(),
                event::Time::monotonic(node.clock()).wall,
                node.net(),
                listener,
                &mut rng,
                &variables,
                stop,
            )
        };
        during(serving, async {}).await;
    });
    let Err(::sim::Error::Panicked { message, .. }) = run else {
        panic!("the serve panics: {run:?}");
    };
    assert_eq!(
        message,
        "ns=3;i=0: open62541 adds a node of the number 0 at a free number"
    );
}

#[test]
#[should_panic(expected = r#""ns=3;x=1" is not a node id: "#)]
fn variables_panic_on_a_text_that_is_not_a_node_id() {
    drop(Variables::new(&[SPIKE, "ns=3;x=1"]));
}

#[test]
#[should_panic(expected = r#""ns=03;i=7" and "ns=3;i=7" name the same node"#)]
fn variables_panic_on_two_texts_of_one_node() {
    drop(Variables::new(&[DIP, "ns=03;i=7"]));
}

/// Serves with a `wall` that panics in a child process, and does nothing outside one.
#[test]
fn serves_with_a_wall_that_panics() {
    if !crate::child::running() {
        return;
    }
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    let run = sim.run_on(&node, move |node, _| async move {
        let mut rng = node.entropy().rng();
        let variables = Variables::new(&[SPIKE]);
        let wall = || -> types::time::Stamp { panic!("the wall fails") };
        let (clock, net) = (node.clock(), node.net());
        let serving =
            |stop| serve(clock, wall, net, listen(&node), &mut rng, &variables, stop);
        during(serving, async {}).await;
    });
    drop(run);
}

#[test]
fn a_panic_in_the_wall_of_a_server_aborts() {
    let output =
        crate::child::aborted("sim::tests::serves_with_a_wall_that_panics", &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("panic in a function that cannot unwind"),
        "{stderr}"
    );
}

/// Serves no variables, and gives the output of `body` with a client of the server.
fn with_tree<T: Send + 'static>(
    body: impl AsyncFnOnce(&mut Client) -> T + Send + 'static,
) -> T {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    sim.run_on(&node, move |node, _| async move {
        let listener = listen(&node);
        let url =
            CString::new(format!("opc.tcp://{}", listener.local())).expect("no NUL");
        let mut rng = node.entropy().rng();
        let mut out = None;
        let until = async {
            let mut client = connect(&node, &mut rng, &url).await.expect("connect");
            out = Some(body(&mut client).await);
            client.close().await;
        };
        let mut served = node.entropy().rng();
        let variables = Variables::new(&[]);
        let wall = event::Time::monotonic(node.clock()).wall;
        let serving = |stop| {
            serve(
                node.clock(),
                wall,
                node.net(),
                listener,
                &mut served,
                &variables,
                stop,
            )
        };
        during(serving, until).await;
        out.expect("the body ends")
    })
    .expect("the run ends")
}

/// The node of the name `name` in namespace `namespace`.
fn named(namespace: u16, name: &str) -> Node {
    format!("ns={namespace};s={name}").parse().expect("a node")
}

/// A reference of the type `via` in namespace 0 to the node `name` of namespace 1,
/// with the browse and display name `text`.
fn reference(name: &str, text: &str, class: Class, via: u32) -> Reference {
    Reference {
        node: Ok(named(1, name)),
        name: text.to_owned(),
        display: text.to_owned(),
        class,
        via: Some(Node::ns0(via)),
    }
}

/// The names of the variables of `Plant/Types`, in the order of their ids.
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

/// Each page of a browse of `node`, which follows each continuation point.
async fn pages(client: &mut Client, node: &Node) -> Vec<Page> {
    let most = NonZeroU32::MAX;
    let mut pages = vec![
        client
            .browse(node, most)
            .await
            .expect("a browse")
            .expect("a node"),
    ];
    while let Some(point) = pages.last_mut().and_then(|page| page.next.take()) {
        pages.push(client.browse_next(point).await.expect("a browse next"));
    }
    pages
}

/// The references of each page of `pages`.
fn references(pages: Vec<Page>) -> Vec<Vec<Reference>> {
    pages.into_iter().map(|page| page.references).collect()
}

#[test]
fn a_client_browses_the_tree_from_objects() {
    let (objects, plant, line, types, odd) = with_tree(async |client| {
        let objects = pages(client, &Node::ns0(ns0::OBJECTS)).await;
        let plant = pages(client, &named(1, "Plant")).await;
        let line = pages(client, &named(1, "Plant.Line")).await;
        let types = pages(client, &named(1, "Plant.Types")).await;
        let odd = pages(client, &named(1, "Plant.Odd")).await;
        (objects, plant, line, types, odd)
    });
    let plant_folder = reference("Plant", "Plant", Class::Object, ORGANIZES);
    let objects: Vec<Reference> = objects
        .into_iter()
        .flat_map(|page| page.references)
        .collect();
    assert!(objects.contains(&plant_folder), "{objects:?}");
    assert_eq!(
        references(plant),
        [[
            reference("Plant.Types", "Types", Class::Object, ORGANIZES),
            reference("Plant.Odd", "Odd", Class::Object, ORGANIZES),
            reference("Plant.Line", "Line", Class::Object, HAS_COMPONENT),
        ]]
    );
    let variable = |name: &str| {
        let node = format!("Plant.Line.{name}");
        reference(&node, name, Class::Variable, HAS_COMPONENT)
    };
    assert_eq!(
        references(line),
        [vec![
            plant_folder,
            variable("Temperature"),
            variable("Speed"),
            variable("Mode"),
            variable("Profile"),
            variable("Locked"),
            variable("Recipe"),
        ]]
    );
    let types: Vec<Vec<Reference>> = references(types);
    let expected: Vec<Reference> = (TYPES.iter())
        .map(|name| {
            let node = format!("Plant.Types.{name}");
            reference(&node, name, Class::Variable, HAS_COMPONENT)
        })
        .collect();
    let mut first = expected;
    let second = first.split_off(8);
    assert_eq!(types, [first, second]);
    let level = |via| reference("Plant.Odd.Level", "Level", Class::Variable, via);
    assert_eq!(
        references(odd),
        [[
            Reference {
                node: Err(Foreign {
                    text: tree::GUID.as_bytes().to_vec(),
                    cause: Cause::Key,
                }),
                ..reference("guid", "guid", Class::Object, ORGANIZES)
            },
            level(ORGANIZES),
            level(HAS_COMPONENT),
            Reference {
                node: Ok(named(1, "Plant.Odd.Twin")),
                ..level(HAS_COMPONENT)
            },
        ]]
    );
}

#[test]
fn a_client_describes_each_variable_of_the_tree() {
    let line = [
        "Temperature",
        "Speed",
        "Mode",
        "Profile",
        "Locked",
        "Recipe",
    ];
    let nodes: Vec<Node> = (TYPES.iter().map(|name| format!("Plant.Types.{name}")))
        .chain(line.iter().map(|name| format!("Plant.Line.{name}")))
        .chain([tree::LEVEL, tree::TWIN].map(str::to_owned))
        .map(|name| named(1, &name))
        .collect();
    let described = with_tree(async move |client| client.describe(&nodes).await);
    let description = |base: Option<u32>, rank: i32, access: u8| Description {
        base: base.map(Base::of),
        rank: Some(rank),
        access: Some(access),
        unit: None,
    };
    let mut expected: Vec<Description> = (1..=15)
        .map(|base| description(Some(base), -1, 1))
        .collect();
    expected.extend([
        Description {
            unit: Some(Unit {
                text: Some("°C".to_owned()),
                code: Some("CEL".to_owned()),
            }),
            ..description(Some(11), -1, 1)
        },
        description(Some(11), -1, 1),
        description(Some(6), -1, 1),
        description(Some(11), 1, 1),
        description(Some(11), -1, 0),
        description(None, -1, 1),
        description(Some(11), -1, 1),
        description(Some(11), -1, 1),
    ]);
    assert_eq!(described, Ok(expected));
}

#[test]
fn a_read_of_each_variable_of_the_tree_gives_its_value() {
    let names: Vec<String> = (TYPES.iter().map(|name| format!("Plant.Types.{name}")))
        .chain(
            [
                "Temperature",
                "Speed",
                "Mode",
                "Profile",
                "Locked",
                "Recipe",
            ]
            .map(|name| format!("Plant.Line.{name}")),
        )
        .chain(["Plant.Odd.Level".to_owned(), "Plant.Odd.Twin".to_owned()])
        .collect();
    let reads = with_tree(async move |client| {
        let mut reads = Vec::new();
        for name in &names {
            reads.push(client.read_one(&named(1, name)).await);
        }
        reads
    });
    let mistyped = |name: &str| Err(Fault::of_type(Type::named(name)));
    assert_eq!(
        reads,
        [
            Ok(Value::Bool(true)),
            Ok(Value::I8(-8)),
            Ok(Value::U8(8)),
            Ok(Value::I16(-16)),
            Ok(Value::U16(16)),
            Ok(Value::I32(-32)),
            Ok(Value::U32(32)),
            Ok(Value::I64(-64)),
            Ok(Value::U64(64)),
            Ok(Value::F32(0.5)),
            Ok(Value::F64(0.25)),
            mistyped("String"),
            mistyped("DateTime"),
            mistyped("Guid"),
            mistyped("ByteString"),
            Ok(Value::F64(21.5)),
            Ok(Value::F64(1.25)),
            Ok(Value::I32(0)),
            mistyped("array of Double"),
            Err(Fault::status(BAD_USER_ACCESS_DENIED)),
            mistyped("ExtensionObject"),
            Ok(Value::F64(2.5)),
            Ok(Value::F64(3.5)),
        ]
    );
}

#[test]
fn a_server_panics_on_a_variable_of_a_node_of_the_tree() {
    let reserved = tree::NS0_TYPES.map(|number| format!("ns=0;i={number}"));
    for text in reserved.iter().chain([&format!("ns=1;s={}", tree::SPEED)]) {
        a_server_panics_on_a_variable_of(text);
    }
}

/// Asserts that a serve panics on the variable `text`.
fn a_server_panics_on_a_variable_of(text: &str) {
    let text = text.to_owned();
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    let run = sim.run_on(&node, move |node, _| async move {
        let mut rng = node.entropy().rng();
        let variables = Variables::new(&[&text]);
        let listener = listen(&node);
        let serving = |stop| {
            serve(
                node.clock(),
                event::Time::monotonic(node.clock()).wall,
                node.net(),
                listener,
                &mut rng,
                &variables,
                stop,
            )
        };
        during(serving, async {}).await;
    });
    let Err(::sim::Error::Panicked { message, .. }) = run else {
        panic!("the serve panics: {run:?}");
    };
    assert!(
        message.starts_with(
            "assertion `left == right` failed: open62541 refused the node"
        ),
        "{message}"
    );
}

/// Connects a client of `node` to `url`.
async fn connect(
    node: &node::Node,
    rng: &mut env::rng::Rng,
    url: &CStr,
) -> Result<Client, Status> {
    let time = event::Time::monotonic(node.clock());
    let connect = Client::connect(
        time,
        tracing::Span::none(),
        node.net(),
        rng,
        url,
        None,
        future::pending(),
    );
    Ok(connect.await?.expect("a pending wait never ends"))
}

/// What a peer of a [`serve`] saw: the output of the serve, the first read of a stream
/// that the peer opened before the stop, a connect after it, and the address of the
/// server.
type Stopped = (
    Result<(), net::Error>,
    Result<usize, net::Error>,
    Result<(), net::Error>,
    SocketAddr,
);

/// Serves on a device node, opens a stream from a peer node, then, once the server
/// accepted it, calls `stop` with the device, the address of the server, and the token
/// of the serve.
fn stopped(
    stop: impl FnOnce(&node::Node, SocketAddr, &Token) + Send + 'static,
) -> Stopped {
    let mut sim = Sim::new(::sim::Config::default());
    let (device, peer) = (
        sim.node(node::Config::default()),
        sim.node(node::Config::default()),
    );
    let local = SocketAddr::new(device.addresses()[0], 4840);
    let (until, (send, served)) = (Token::new(), mpsc::channel());
    let (node, token) = (device.clone(), until.clone());
    let started = device.shards().start(
        env::shards::Config {
            name: "serve".into(),
            core: None,
        },
        move |_| async move {
            let (listener, mut rng) = (listen(&node), node.entropy().rng());
            let variables = Variables::new(&[SPIKE]);
            let wall = event::Time::monotonic(node.clock()).wall;
            let out = serve(
                node.clock(),
                wall,
                node.net(),
                listener,
                &mut rng,
                &variables,
                token,
            );
            let out = out.await;
            drop(listen(&node));
            send.send(out).expect("the test waits");
        },
    );
    drop(started.expect("the shard starts"));
    let (net, clock, (send, read)) = (peer.net(), peer.clock(), mpsc::channel());
    let config = tcp::Config {
        remote: local,
        options: OPTIONS,
    };
    let started = peer.shards().start(
        env::shards::Config {
            name: "dial".into(),
            core: None,
        },
        move |_| async move {
            let mut stream = net.connect(&config).await.expect("the connect works");
            while device.streams() == 0 {
                clock.sleep(Span::from_nanos(1_000_000)).await;
            }
            stop(&device, local, &until);
            let mut buffer = [0; 64];
            let first = poll_fn(|cx| stream.poll_read(cx, &mut buffer)).await;
            let again = net.connect(&config).await.map(drop);
            send.send((first, again)).expect("the test waits");
        },
    );
    drop(started.expect("the shard starts"));
    sim.run_for(Span::SECOND).expect("the run goes on");
    let out = served.try_recv().expect("the serve ends");
    let (first, again) = read.try_recv().expect("the peer reads the end");
    (out, first, again, local)
}

/// A cancel closes an open stream with a FIN, and the serve gives `Ok`.
#[test]
fn a_cancel_closes_each_stream_and_ends_the_serve() {
    let (out, first, again, remote) = stopped(|_, _, until| until.cancel());
    assert_eq!(out, Ok(()));
    assert_eq!(first, Ok(0));
    assert_eq!(again, Err(net::Error::Refused { remote }));
}

/// A failed accept closes an open stream with a FIN, and the serve gives the error.
#[test]
fn a_failed_accept_closes_each_stream_and_gives_its_error() {
    let (out, first, again, remote) =
        stopped(|device, local, _| device.fail_listener(local));
    assert_eq!(out, Err(net::Error::Io { code: 5 }));
    assert_eq!(first, Ok(0));
    assert_eq!(again, Err(net::Error::Refused { remote }));
}

/// A failed accept wins over a cancel in the same step.
#[test]
fn a_failed_accept_wins_over_a_cancel_in_the_same_step() {
    let (out, ..) = stopped(|device, local, until| {
        device.fail_listener(local);
        until.cancel();
    });
    assert_eq!(out, Err(net::Error::Io { code: 5 }));
}
