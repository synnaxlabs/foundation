use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString};
use std::future::{self, poll_fn};
use std::io::IoSlice;
use std::mem;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::ptr::NonNull;
use std::rc::Rc;
use std::task::{Context, Poll};

use env::clock::Clock;
use env::net::{Net, Tcp, tcp};
use env::rng::Rng;
use sim::{Sim, node};
use types::quality::Quality;
use types::time::{Span, Stamp};

use self::common::is;
use super::{
    Calls, Client, Flight, GRACE, Large, Nodes, READS, Request, SPLIT, STATE, Slots,
    UNREAD, answer, id_bytes, most,
};
use crate::client::{Credentials, Mode};
use crate::connection::CHUNKS;
use crate::connection::Manager;
use crate::event;
use crate::ffi::{self, Status};
use crate::gate::Gate;
use crate::node::{Key, Name, Node};
use crate::secure::Policy;
use crate::server::{Limits, Server};
use crate::unwind::caught;
use crate::value::{self, Fault, Value};

/// The port of the test server.
const PORT: u16 = 4840;

/// The Double variable of the test server.
const VALUE: Node = Node {
    namespace: 1,
    key: Key::Number(1000),
};

/// A `ByteString` whose answer needs 8 chunks, [`CHUNKS`], from [`read_large`].
const FITS: Node = Node {
    namespace: 1,
    key: Key::Number(1002),
};

/// A `ByteString` whose answer needs 9 chunks, one past [`CHUNKS`], from
/// [`read_large`].
const PAST: Node = Node {
    namespace: 1,
    key: Key::Number(1003),
};

/// The size of a chunk, the send buffer size of a channel.
const CHUNK: usize = 1 << 16;

/// The current time of the server, a `DateTime`.
const TIME: Node = Node {
    namespace: 0,
    key: Key::Number(ffi::TIME),
};

impl Client {
    /// Reads the Value of `node` alone.
    pub(crate) async fn read_one(&mut self, node: &Node) -> Result<Value, Fault> {
        let nodes = fits(self.nodes(std::slice::from_ref(node)));
        (self.read(&nodes).await)
            .map_err(Fault::status)
            .and_then(|values| values[0])
    }

    /// Builds one Read request of `nodes` for this client with no check of its size.
    fn unchecked(&self, nodes: &[Node]) -> Nodes {
        Nodes::new(nodes, &[nodes.len()], Rc::clone(&self.key))
    }
}

/// The side of a client in a run of [`serve`].
struct Peer<'s> {
    /// The test server.
    server: &'s Server,
    clock: Clock,
    net: Net,
    rng: Rng,
    /// The endpoint of the test server.
    url: String,
    /// The value of [`VALUE`].
    value: Value,
    /// The server does not run while it is closed.
    gate: Gate,
    /// The reads of the state of the server that it answered.
    states: Rc<Cell<u32>>,
    /// The quality of each answer of the state of the server, at first Good.
    quality: Rc<Cell<Quality>>,
}

impl Peer<'_> {
    async fn connect(&mut self, url: &str) -> Result<Client, Status> {
        let url = CString::new(url).expect("the URL holds no NUL");
        let time = event::Time::monotonic(Clock::clone(&self.clock));
        open(time, Net::clone(&self.net), &mut self.rng, &url).await
    }

    async fn connected(&mut self) -> Client {
        let url = self.url.clone();
        self.connect(&url).await.expect("the client connects")
    }

    /// Connects a client to `url` whose receive buffer is `receive` bytes and whose
    /// largest message is `message` bytes, each `None` for the default of open62541.
    async fn limited(
        &mut self,
        url: &str,
        receive: Option<u32>,
        message: Option<u32>,
    ) -> Client {
        let time = event::Time::monotonic(Clock::clone(&self.clock));
        let setup = |raw: NonNull<ffi::Client>| {
            if let Some(size) = receive {
                // SAFETY: the client lives and has not connected.
                unsafe { ffi::test::shim_client_receive(raw.as_ptr(), size) };
            }
            if let Some(size) = message {
                // SAFETY: the client lives and has not connected.
                unsafe { ffi::test::shim_client_message(raw.as_ptr(), size) };
            }
        };
        self.client(url, time, setup)
            .await
            .expect("the client connects")
    }

    /// Connects a client in `mode` on `credentials` through [`Client::connect`], on a
    /// loop whose wall time starts at `wall`.
    async fn secure(
        &mut self,
        mode: Mode,
        credentials: &Credentials<'_>,
        wall: Stamp,
    ) -> Result<Client, Status> {
        let time = event::Time::since(Clock::clone(&self.clock), wall);
        let url = CString::new(self.url.clone()).expect("the URL holds no NUL");
        let connect = Client::connect(
            time,
            tracing::Span::none(),
            self.net.clone(),
            &mut self.rng,
            &url,
            Some((credentials, mode)),
            future::pending(),
        );
        Ok(connect.await?.expect("a pending wait never ends"))
    }

    /// Connects a client to `url` on a loop that reads `time`, after `setup` sets the
    /// config of the client, and closes a client that does not connect.
    async fn client(
        &mut self,
        url: &str,
        time: event::Time,
        setup: impl FnOnce(NonNull<ffi::Client>),
    ) -> Result<Client, Status> {
        let connections =
            Manager::new(time, tracing::Span::none(), self.net.clone(), &mut self.rng);
        // SAFETY: the loop outlives the client, which the drop of the client deletes
        // first.
        let raw = unsafe { crate::client::unconnected(connections.events().raw()) };
        setup(raw);
        let url = CString::new(url).expect("the URL holds no NUL");
        // SAFETY: `raw` is a new client of the loop of `connections`.
        let mut client =
            unsafe { Client::of(Clock::clone(&self.clock), connections, raw, url) };
        match client.open().await {
            Ok(()) => Ok(client),
            Err(status) => {
                client.close().await;
                Err(status)
            }
        }
    }
}

/// Connects a new client to `url`, with no wait that ends it.
async fn open(
    time: event::Time,
    net: Net,
    rng: &mut Rng,
    url: &CStr,
) -> Result<Client, Status> {
    let connect = Client::connect(
        time,
        tracing::Span::none(),
        net,
        rng,
        url,
        None,
        future::pending(),
    );
    Ok(connect.await?.expect("a pending wait never ends"))
}

/// Runs `test` on a node of a run of `seed`, and gives its value and the digest of
/// the run.
fn run<T: Send + 'static>(
    seed: u64,
    test: impl AsyncFnOnce(node::Node) -> T + Send + 'static,
) -> (T, u64) {
    let mut sim = Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(node::Config::default());
    let value = sim
        .run_on(&node, move |node, _| test(node))
        .expect("the run ends");
    (value, sim.digest())
}

/// Starts a test server on `node`, with room for `sessions` sessions and a Double at
/// [`VALUE`] from its random values, runs `body` while the server runs, except while
/// [`Peer::gate`] is closed, then closes the server.
async fn serve<T>(
    node: &node::Node,
    sessions: u16,
    body: impl AsyncFnOnce(Peer<'_>) -> T,
) -> T {
    let time = event::Time::monotonic(node.clock());
    serve_on(node, sessions, time, None, body).await
}

/// [`serve`], on a loop that reads `time`, with an endpoint of each policy of
/// `security`, in its order, on its credentials, when it is given.
async fn serve_on<T>(
    node: &node::Node,
    sessions: u16,
    time: event::Time,
    security: Option<(&Credentials<'_>, &[Policy])>,
    body: impl AsyncFnOnce(Peer<'_>) -> T,
) -> T {
    let mut rng = node.entropy().rng();
    let local = SocketAddr::new(node.addresses()[0], PORT);
    let manager = Manager::listening(time, node.net(), local, 4, &mut rng)
        .expect("the port is free");
    let mut server = match security {
        // SAFETY: the close below deletes the server before the manager drops.
        Some((credentials, policies)) => unsafe {
            Server::start_secure(&manager, c"", sessions, credentials, policies)
        },
        // SAFETY: as above.
        None => unsafe { Server::start(&manager, c"", sessions) },
    };
    let value = Value::F64(f64::from(u32::try_from(rng.below(1 << 20)).expect("fits")));
    server.add_scalar(&VALUE, value);
    let states = Rc::new(Cell::new(0));
    let quality = Rc::new(Cell::new(Quality::GOOD));
    let (count, answer) = (Rc::clone(&states), Rc::clone(&quality));
    server.set_source(&STATE, move || {
        count.set(count.get() + 1);
        (Value::I32(0), answer.get())
    });
    let peer = Peer {
        server: &server,
        clock: node.clock(),
        net: node.net(),
        rng,
        url: format!("opc.tcp://{local}"),
        value,
        gate: Gate::default(),
        states,
        quality,
    };
    let gate = peer.gate.clone();
    let out = {
        let mut server_drive = pin!(gate.hold(manager.drive(|_| Poll::<()>::Pending)));
        let mut body = pin!(body(peer));
        poll_fn(|cx| {
            assert!(server_drive.as_mut().poll(cx).is_pending(), "it never ends");
            body.as_mut().poll(cx)
        })
        .await
    };
    server.close(&manager).await;
    out
}

/// The value of [`VALUE`] and the answer of a read of it, in a run of `seed`.
fn spike(seed: u64) -> ((Value, Result<Value, Fault>), u64) {
    run(seed, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            (peer.value, client.read_one(&VALUE).await)
        })
        .await
    })
}

#[test]
fn a_client_reads_a_double_and_a_run_replays() {
    let ((value, read), digest) = spike(7);
    assert_eq!(read, Ok(value));
    assert_eq!(spike(7), ((value, Ok(value)), digest));
    let ((other, read), _) = spike(8);
    assert_eq!(read, Ok(other));
    assert_ne!(other, value);
}

#[test]
fn a_read_of_an_unknown_node_fails_with_its_status() {
    let (read, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let unknown = Node {
                key: Key::Number(1001),
                ..VALUE
            };
            client.read_one(&unknown).await
        })
        .await
    });
    assert_eq!(read, Err(Fault::status(Status::BAD_NODE_ID_UNKNOWN)));
}

/// Calls the private `Client::new`, as no caller holds a client before its open:
/// `Client::connect` opens it at once.
#[test]
fn a_client_gives_its_url_before_and_after_its_open() {
    let (urls, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let url = CString::new(peer.url.clone()).expect("the URL holds no NUL");
            let time = event::Time::monotonic(Clock::clone(&peer.clock));
            let net = Net::clone(&peer.net);
            let mut client = Client::new(
                time,
                tracing::Span::none(),
                net,
                &mut peer.rng,
                url.clone(),
            );
            let before = client.url().to_owned();
            client.open().await.expect("the client connects");
            let after = client.url().to_owned();
            client.close().await;
            (url, before, after)
        })
        .await
    });
    let (url, before, after) = urls;
    assert_eq!((before, after), (url.clone(), url));
}

/// The name `node` must reach the server whole: it holds a `;`, an `=`, and a byte
/// past ASCII, and a node whose name is its prefix holds another value. The name in
/// namespace 0 holds no value.
#[test]
fn a_read_of_a_name_node_gives_its_value() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let named = |name: &str| Node {
                namespace: 1,
                key: Key::Name(Name::new(name).expect("a name that is not empty")),
            };
            peer.server
                .add_scalar(&named("plant.spike;x=ü"), Value::F64(2.5));
            peer.server
                .add_scalar(&named("plant.spike"), Value::F64(3.5));
            let mut client = peer.connected().await;
            let reads = (
                client.read_one(&named("plant.spike;x=ü")).await,
                client.read_one(&named("plant.spike")).await,
                client.read_one(&named("plant.dip")).await,
                client
                    .read_one(&Node {
                        namespace: 0,
                        ..named("plant.spike")
                    })
                    .await,
            );
            client.close().await;
            reads
        })
        .await
    });
    let unknown = Err(Fault::status(Status::BAD_NODE_ID_UNKNOWN));
    let expected = (Ok(Value::F64(2.5)), Ok(Value::F64(3.5)), unknown, unknown);
    assert_eq!(reads, expected);
}

#[test]
fn a_read_of_another_type_fails_with_a_type_mismatch() {
    let (read, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            client.read_one(&TIME).await
        })
        .await
    });
    assert_eq!(read, mistyped("DateTime"));
}

/// A scalar of each type at its bounds, and each special value of a float.
const SCALARS: [Value; 30] = [
    Value::Bool(false),
    Value::Bool(true),
    Value::I8(i8::MIN),
    Value::I8(i8::MAX),
    Value::U8(u8::MIN),
    Value::U8(u8::MAX),
    Value::I16(i16::MIN),
    Value::I16(i16::MAX),
    Value::U16(u16::MIN),
    Value::U16(u16::MAX),
    Value::I32(i32::MIN),
    Value::I32(i32::MAX),
    Value::U32(u32::MIN),
    Value::U32(u32::MAX),
    Value::I64(i64::MIN),
    Value::I64(i64::MAX),
    Value::U64(u64::MIN),
    Value::U64(u64::MAX),
    Value::F32(f32::MIN),
    Value::F32(f32::MAX),
    Value::F32(-0.0),
    Value::F32(f32::NEG_INFINITY),
    Value::F32(f32::NAN),
    Value::F32(f32::from_bits(0xffc0_0001)),
    Value::F64(f64::MIN),
    Value::F64(f64::MAX),
    Value::F64(-0.0),
    Value::F64(f64::INFINITY),
    Value::F64(f64::NAN),
    Value::F64(f64::from_bits(0xfff8_0000_0000_0001)),
];

/// `value` as the test server sends it: open62541 sends each NaN as the quiet NaN
/// with the sign bit set.
fn sent(value: Value) -> Value {
    match value {
        Value::F32(float) if float.is_nan() => Value::F32(f32::from_bits(0xffc0_0000)),
        Value::F64(double) if double.is_nan() => {
            Value::F64(f64::from_bits(0xfff8_0000_0000_0000))
        }
        other => other,
    }
}

/// A [`Value`] that compares each float by its bits, which tell each NaN and -0.0
/// apart.
#[derive(Clone, Debug, PartialEq)]
enum Exact {
    F32(u32),
    F64(u64),
    Other(Value),
}

/// The fault of a Good value of the OPC UA type `name`, such as `array of Double`.
fn mistyped(name: &str) -> Result<Value, Fault> {
    Err(Fault::of_type(value::Type::named(name)))
}

fn exact(value: Result<Value, Fault>) -> Result<Exact, Fault> {
    value.map(|value| match value {
        Value::F32(float) => Exact::F32(float.to_bits()),
        Value::F64(double) => Exact::F64(double.to_bits()),
        other => Exact::Other(other),
    })
}

#[test]
fn a_read_gives_each_scalar_type_at_its_bounds() {
    let (values, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let nodes: Vec<Node> = (2000..)
                .zip(SCALARS)
                .map(|(number, value)| {
                    let node = Node {
                        namespace: 1,
                        key: Key::Number(number),
                    };
                    peer.server.add_scalar(&node, value);
                    node
                })
                .collect();
            let mut client = peer.connected().await;
            let nodes = fits(client.nodes(&nodes));
            let values = client.read(&nodes).await.map(<[_]>::to_vec);
            client.close().await;
            values
        })
        .await
    });
    let values = values.map(|values| values.into_iter().map(exact).collect::<Vec<_>>());
    let expected = SCALARS.map(|value| exact(Ok(sent(value)))).to_vec();
    assert_eq!(values, Ok(expected));
}

#[test]
fn the_kind_of_each_scalar_type_is_its_index_in_ua_types() {
    let kinds = [
        ffi::BOOLEAN,
        ffi::SBYTE,
        ffi::BYTE,
        ffi::INT16,
        ffi::UINT16,
        ffi::INT32,
        ffi::UINT32,
        ffi::INT64,
        ffi::UINT64,
        ffi::FLOAT,
        ffi::DOUBLE,
    ];
    let names = kinds.map(|kind| ffi::types()[usize::from(kind)].name());
    assert_eq!(
        names,
        [
            "Boolean", "SByte", "Byte", "Int16", "UInt16", "Int32", "UInt32", "Int64",
            "UInt64", "Float", "Double",
        ]
    );
}

#[test]
fn a_read_of_an_array_or_a_byte_string_fails_with_a_type_mismatch() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let array = Node {
                namespace: 1,
                key: Key::Number(1001),
            };
            let bytes = Node {
                namespace: 1,
                key: Key::Number(1002),
            };
            peer.server.add_doubles(&array);
            peer.server.add_bytes(&bytes, 1);
            let mut client = peer.connected().await;
            (client.read_one(&array).await, client.read_one(&bytes).await)
        })
        .await
    });
    assert_eq!(reads, (mistyped("array of Double"), mistyped("ByteString")));
}

#[test]
fn a_read_of_nodes_gives_the_value_of_each_in_order() {
    let ((values, value), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let unknown = Node {
                key: Key::Number(1001),
                ..VALUE
            };
            let nodes = fits(client.nodes(&[TIME, VALUE, unknown, VALUE]));
            let values = client.read(&nodes).await.map(<[_]>::to_vec);
            client.close().await;
            (values, peer.value)
        })
        .await
    });
    let expected = [
        mistyped("DateTime"),
        Ok(value),
        Err(Fault::status(Status::BAD_NODE_ID_UNKNOWN)),
        Ok(value),
    ];
    assert_eq!(values, Ok(expected.to_vec()));
}

/// The late answer of a read of two nodes that timed out, after `dropped` dropped
/// reads. It lands while the next read waits for its own answer, or, at the bound of
/// reads in flight, while that read waits to send.
fn read_after_late_answer(
    dropped: u16,
) -> (Result<Vec<Result<Value, Fault>>, Status>, Value) {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            peer.gate.close();
            drop_reads(&mut client, dropped).await;
            let late = fits(client.nodes(&[TIME, VALUE]));
            assert_eq!(client.read(&late).await, Err(Status::BAD_TIMEOUT));
            peer.gate.open();
            let nodes = fits(client.nodes(&[VALUE]));
            let values = client.read(&nodes).await.map(<[_]>::to_vec);
            client.close().await;
            (values, peer.value)
        })
        .await
    });
    out
}

#[test]
fn the_late_answer_of_a_read_does_not_change_the_next_read() {
    for dropped in [0, READS - 1] {
        let (values, value) = read_after_late_answer(dropped);
        assert_eq!(values, Ok(vec![Ok(value)]), "{dropped} dropped");
    }
}

#[test]
fn a_connect_with_no_listener_fails_with_its_status() {
    let (status, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let address = SocketAddr::new(node.addresses()[0], PORT + 1);
            peer.connect(&format!("opc.tcp://{address}")).await.err()
        })
        .await
    });
    assert_eq!(status, Some(Status::BAD_CONNECTION_REJECTED));
}

/// A port past `u32::MAX` that wraps to the port of the server fails the connect, and
/// does not reach the server.
#[test]
fn a_connect_to_a_port_that_wraps_to_the_server_fails_with_its_status() {
    let (status, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let wrapped = u64::from(PORT) + (1 << 32);
            let url = format!("opc.tcp://{}:{wrapped}", node.addresses()[0]);
            match peer.connect(&url).await {
                Ok(client) => {
                    client.close().await;
                    None
                }
                Err(status) => Some(status),
            }
        })
        .await
    });
    assert_eq!(status, Some(Status::BAD_TCP_ENDPOINT_URL_INVALID));
}

/// `UA_readNumber` stops before a digit that takes the number past `u32::MAX`.
#[test]
fn a_number_stops_before_the_digit_that_overflows_u32() {
    let read = |text: &str| {
        let mut number = 0;
        // SAFETY: `text` lives through the call, which reads only its `len` bytes.
        let progress = unsafe {
            ffi::test::UA_readNumber(text.as_ptr(), text.len(), &raw mut number)
        };
        (progress, number)
    };
    assert_eq!(read("4294967295"), (10, u32::MAX));
    assert_eq!(read("4294967296"), (9, 429_496_729));
    assert_eq!(read("42949672950"), (10, u32::MAX));
    assert_eq!(read("4840/path"), (4, 4840));
}

#[test]
fn a_connect_to_a_url_that_open62541_refuses_fails_with_its_status() {
    let (statuses, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut statuses = Vec::new();
            for url in ["http://127.0.0.1:4840", "opc.tcp://plc:99999"] {
                statuses.push(peer.connect(url).await.err());
            }
            statuses
        })
        .await
    });
    assert_eq!(
        statuses,
        [
            Some(Status::BAD_TCP_ENDPOINT_URL_INVALID),
            Some(Status::BAD_TCP_ENDPOINT_URL_INVALID)
        ]
    );
}

#[test]
fn a_read_after_the_server_closes_fails_and_open62541_refuses_the_next() {
    let (reads, _) = run(0, async |node| {
        let mut client = serve(&node, 4, async |mut peer| peer.connected().await).await;
        (client.read_one(&VALUE).await, client.read_one(&VALUE).await)
    });
    assert_eq!(
        reads,
        (
            Err(Fault::status(Status::BAD_SECURE_CHANNEL_CLOSED)),
            Err(Fault::status(Status::BAD_SERVER_NOT_CONNECTED))
        )
    );
}

/// The server answers a read whose future dropped before the next read, and the
/// callback drops that answer, so the next read gets its own answer.
#[test]
fn a_read_after_a_dropped_read_gets_its_own_answer() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            {
                let mut dropped = pin!(client.read_one(&VALUE));
                poll_fn(|cx| {
                    assert!(dropped.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
            }
            peer.clock.sleep(Span::from_nanos(10_000_000)).await;
            (
                client.read_one(&TIME).await,
                client.read_one(&VALUE).await,
                peer.value,
            )
        })
        .await
    });
    let (time, value, expected) = reads;
    assert_eq!((time, value), (mistyped("DateTime"), Ok(expected)));
}

/// Starts `count` reads of [`VALUE`] by `client`, each polled once and then dropped.
async fn drop_reads(client: &mut Client, count: u16) {
    for _ in 0..count {
        drop_read(client, &VALUE).await;
    }
}

/// Starts a read of `node` by `client`, polled once and then dropped.
async fn drop_read(client: &mut Client, node: &Node) {
    let mut read = pin!(client.read_one(node));
    poll_fn(|cx| {
        assert!(read.as_mut().poll(cx).is_pending(), "a read waits");
        Poll::Ready(())
    })
    .await;
}

/// Without the bound on the reads in flight, the server answers the 257 requests of
/// the reads and the connect in one pass, and its 257th send closes the channel.
#[test]
fn a_read_after_255_dropped_reads_gives_the_value() {
    let ((read, value), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            drop_reads(&mut client, 255).await;
            let read = client.read_one(&VALUE).await;
            client.close().await;
            (read, peer.value)
        })
        .await
    });
    assert_eq!(read, Ok(value));
}

/// While the server stops, a read after `dropped` dropped reads starts 2 s after them.
/// Gives its answer and the time it took.
fn read_after(dropped: u16) -> (Result<Value, Fault>, Span) {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            peer.gate.close();
            drop_reads(&mut client, dropped).await;
            peer.clock.sleep(Span::from_nanos(2_000_000_000)).await;
            let start = peer.clock.now();
            let read = client.read_one(&VALUE).await;
            let took = peer.clock.now() - start;
            peer.gate.open();
            client.close().await;
            (read, took)
        })
        .await
    });
    out
}

/// Below the bound, a read sends its request and times out. At the bound, the dropped
/// reads stay in flight, so it fails at the end of its wait and sends nothing.
#[test]
fn a_read_at_the_bound_fails_when_no_read_ends() {
    let (below, below_took) = read_after(READS - 1);
    let (at, at_took) = read_after(READS);
    assert_eq!(
        (below, at),
        (
            Err(Fault::status(Status::BAD_TIMEOUT)),
            Err(Fault::status(Status::BAD_TOO_MANY_OPERATIONS))
        )
    );
    let timeout = Span::from_nanos(5_000_000_000)..=Span::from_nanos(6_000_000_000);
    assert!(timeout.contains(&below_took), "{below_took:?}");
    assert!(timeout.contains(&at_took), "{at_took:?}");
}

/// Reads [`VALUE`] while [`Peer::gate`] is closed, except for one pass of the server
/// `at` after the read starts. Gives its answer and the time it took.
async fn read_with_pass(
    client: &mut Client,
    peer: &Peer<'_>,
    at: Span,
) -> (Result<Value, Fault>, Span) {
    let start = peer.clock.now();
    let mut pass = pin!(peer.clock.sleep(at));
    let (mut woke, mut passed) = (false, false);
    let mut read = pin!(client.read_one(&VALUE));
    let read = poll_fn(|cx| {
        if !woke && pass.as_mut().poll(cx).is_ready() {
            woke = true;
            peer.gate.open();
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if woke && !passed {
            passed = true;
            peer.gate.close();
        }
        read.as_mut().poll(cx)
    })
    .await;
    (read, peer.clock.now() - start)
}

/// A read at the bound whose wait ends 3 s in still times out 5 s after it starts.
#[test]
fn the_timeout_of_a_read_counts_its_wait() {
    let ((read, took), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            peer.gate.close();
            drop_reads(&mut client, READS).await;
            let out =
                read_with_pass(&mut client, &peer, Span::from_nanos(3_000_000_000))
                    .await;
            peer.gate.open();
            client.close().await;
            out
        })
        .await
    });
    assert_eq!(read, Err(Fault::status(Status::BAD_TIMEOUT)));
    let timeout = Span::from_nanos(5_000_000_000)..=Span::from_nanos(6_000_000_000);
    assert!(timeout.contains(&took), "{took:?}");
}

/// The answers of [`READS`] dropped reads end them all, so a read after one more dropped
/// read sends its request at once and gets its answer in one pass of the server.
#[test]
fn the_answers_of_dropped_reads_end_them() {
    let ((read, value), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            drop_reads(&mut client, READS).await;
            peer.clock.sleep(Span::from_nanos(1_000_000_000)).await;
            peer.gate.close();
            drop_reads(&mut client, 1).await;
            let (read, _) =
                read_with_pass(&mut client, &peer, Span::from_nanos(1_000_000_000))
                    .await;
            peer.gate.open();
            client.close().await;
            (read, peer.value)
        })
        .await
    });
    assert_eq!(read, Ok(value));
}

/// A stalled server keeps the request of each read that timed out, and answers them
/// all in one pass when it runs again. Without the bound on its requests, 17 rounds of
/// 16 dropped reads send 272, and the 257th answer closes the channel.
#[test]
fn a_read_after_272_dropped_reads_on_a_stalled_server_gives_the_value() {
    let ((read, value), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            peer.gate.close();
            for _ in 0..17 {
                drop_reads(&mut client, 16).await;
                peer.clock.sleep(Span::from_nanos(6_000_000_000)).await;
            }
            peer.gate.open();
            peer.clock.sleep(Span::from_nanos(10_000_000)).await;
            let read = client.read_one(&VALUE).await;
            client.close().await;
            (read, peer.value)
        })
        .await
    });
    assert_eq!(read, Ok(value));
}

/// Reads [`FITS`] and then [`PAST`] from a server that sends at most `chunks` chunks in
/// a message.
fn read_large(chunks: u32) -> (Result<Value, Fault>, Result<Value, Fault>) {
    let (reads, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            peer.server.add_bytes(&FITS, 7 * CHUNK);
            peer.server.add_bytes(&PAST, 8 * CHUNK);
            peer.server.set_limits(Limits {
                chunks,
                ..Limits::default()
            });
            let mut client = peer.connected().await;
            let reads = (client.read_one(&FITS).await, client.read_one(&PAST).await);
            client.close().await;
            reads
        })
        .await
    });
    reads
}

/// The answer of [`FITS`] arrives and is not a Double. The answer of [`PAST`] needs
/// more than [`CHUNKS`] chunks. The test server refuses to send it, and sends an
/// error that closes the channel, whose status the read gets. A server that sends it
/// makes the client close the channel at its final chunk.
#[test]
fn an_answer_of_more_chunks_than_the_bound_fails() {
    let refused = (
        mistyped("ByteString"),
        Err(Fault::status(Status::BAD_RESPONSE_TOO_LARGE)),
    );
    assert_eq!(read_large(CHUNKS), refused);
    let closed = (
        mistyped("ByteString"),
        Err(Fault::status(Status::BAD_TCP_MESSAGE_TOO_LARGE)),
    );
    assert_eq!(read_large(0), closed);
}

/// A drop of a client with an active session ends, and calls the callback of each
/// read in flight, so the answer of a client must outlive its delete. The server then
/// closes. The test reads through C on the private client, since `read` borrows the
/// client, so no public call keeps a read in flight across a drop.
#[test]
fn a_drop_of_a_connected_client_ends_and_answers_each_read() {
    let (value, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let client = peer.connected().await;
            let outside = Calls::default();
            let nodes = Nodes::unchecked(&[VALUE]);
            let mut request = 0;
            // SAFETY: the client and `nodes` live, and `outside` outlives the client.
            let status = Status(unsafe {
                ffi::shim_client_read(
                    client.raw.as_ptr(),
                    nodes.raw(),
                    answer,
                    std::ptr::from_ref(&outside).cast_mut().cast(),
                    &raw mut request,
                )
            });
            assert_eq!(status, Status::GOOD);
            outside.pending.set(1);
            outside.reads.requests.borrow_mut().push(Flight {
                key: request,
                slots: Slots { first: 0, count: 1 },
            });
            outside.reads.values.set(vec![UNREAD]);
            drop(client);
            outside.reads.failed.get()
        })
        .await
    });
    assert_eq!(value, Some(Status::BAD_SESSION_CLOSED));
}

/// A server with room for one session takes a second client only after the first
/// closes its session.
#[test]
fn a_close_frees_the_session_on_the_server_and_a_drop_does_not() {
    let (connects, _) = run(0, async |node| {
        serve(&node, 1, async |mut peer| {
            peer.connected().await.close().await;
            drop(peer.connected().await);
            let url = peer.url.clone();
            peer.connect(&url).await.err()
        })
        .await
    });
    assert_eq!(connects, Some(Status::BAD_TOO_MANY_SESSIONS));
}

/// Connects to a listener on `node` that gives `accepted` each stream it accepts, and
/// gives the status of the failed connect and the time it took.
async fn connect_to_listener(
    node: &node::Node,
    mut accepted: impl FnMut(Tcp),
) -> (Status, Span) {
    let local = SocketAddr::new(node.addresses()[0], PORT);
    let listen = tcp::Listen {
        local,
        backlog: 4,
        options: options(),
    };
    let mut listener = node.net().listen(&listen).expect("the port is free");
    let clock = node.clock();
    let start = clock.now();
    let mut rng = node.entropy().rng();
    let url = CString::new(format!("opc.tcp://{local}")).expect("no NUL");
    let mut connect = pin!(open(
        event::Time::monotonic(node.clock()),
        node.net(),
        &mut rng,
        &url
    ));
    let status = poll_fn(|cx| {
        while let Poll::Ready(stream) = listener.poll_accept(cx) {
            accepted(stream.expect("the accept works"));
        }
        connect.as_mut().poll(cx).map(Result::err)
    })
    .await;
    (status.expect("the connect fails"), clock.now() - start)
}

/// The options of a stream of a test.
fn options() -> tcp::Options {
    tcp::Options {
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
        unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("not 0"),
        delayed: false,
    }
}

#[test]
fn a_connect_to_a_listener_that_never_answers_times_out() {
    let mut streams = Vec::new();
    let (out, _) = run(0, async move |node| {
        connect_to_listener(&node, |stream| streams.push(stream)).await
    });
    assert_eq!(out, (Status::BAD_TIMEOUT, Span::from_nanos(5_000_000_000)));
}

/// The first close of the listener ends the connect, so the client dials once.
#[test]
fn a_connect_to_a_listener_that_closes_each_stream_ends_after_one_dial() {
    let (out, _) = run(0, async |node| {
        let mut accepts = 0;
        let out = connect_to_listener(&node, |stream| {
            accepts += 1;
            drop(stream);
        })
        .await;
        (out, accepts)
    });
    let ended = (Status::BAD_CONNECTION_CLOSED, Span::from_nanos(1_000_000));
    assert_eq!(out, (ended, 1));
}

/// A close waits for the answer of the server, which a server that stops never gives,
/// for [`GRACE`].
#[test]
fn a_close_of_a_client_whose_server_stops_ends_after_the_grace() {
    let (took, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let client = peer.connected().await;
            peer.gate.close();
            let start = peer.clock.now();
            client.close().await;
            peer.gate.open();
            peer.clock.now() - start
        })
        .await
    });
    assert_eq!(took, GRACE);
}

/// A read sends its requests with no timeout of open62541, and restores the request
/// timeout of the client for each other request, such as the `CloseSession`.
#[test]
fn a_read_restores_the_request_timeout_of_the_client() {
    let (timeouts, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            // SAFETY: the client lives.
            let timeout = |client: &Client| unsafe {
                ffi::shim_client_timeout(client.raw.as_ptr())
            };
            let before = timeout(&client);
            assert_eq!(client.read_one(&VALUE).await, Ok(peer.value));
            let after = timeout(&client);
            client.close().await;
            (before, after)
        })
        .await
    });
    assert_eq!(timeouts, (5_000, 5_000));
}

/// A read that a server that stops never answers ends with `BadTimeout` at the request
/// timeout, as a close does.
#[test]
fn a_read_of_a_client_whose_server_stops_times_out() {
    let ((value, took), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            peer.gate.close();
            let start = peer.clock.now();
            let value = client.read_one(&VALUE).await;
            let took = peer.clock.now() - start;
            peer.gate.open();
            client.close().await;
            (value, took)
        })
        .await
    });
    assert_eq!(value, Err(Fault::status(Status::BAD_TIMEOUT)));
    let window = Span::from_nanos(5_000_000_000)..=Span::from_nanos(6_000_000_000);
    assert!(window.contains(&took), "{took:?}");
}

/// A connect that times out after the server makes its session closes the session, so
/// a server with room for one session takes the next client. In the run of seed 0, the
/// server makes the session 5.5 ms after the connect starts and activates it later.
#[test]
fn a_connect_that_times_out_closes_its_session() {
    let (_, closes) = times_out_and_closes(&[Span::from_nanos(6_000_000)]);
    assert_eq!(closes, 1);
}

/// A connect that times out before the server answers its `CreateSession` closes the
/// session that the server makes after the deadline. In the run of seed 0, the server
/// stops at 5.25 ms, before it gets the `CreateSession`, and answers it 100 us after
/// the deadline.
#[test]
fn a_connect_that_times_out_before_the_server_makes_its_session_closes_it() {
    let (_, closes) = times_out_and_closes(&[
        Span::from_nanos(5_250_000),
        Span::from_nanos(5_000_100_000),
    ]);
    assert_eq!(closes, 1);
}

/// A connect that times out before a server that stops answers its `CreateSession`
/// ends one grace after the deadline, sends no `CloseSession`, and leaves no session.
#[test]
fn a_connect_whose_create_session_gets_no_answer_ends() {
    let took = times_out_and_closes(&[Span::from_nanos(5_250_000)]);
    assert_eq!(took, (Span::from_nanos(5_000_000_000 + GRACE.nanos()), 0));
}

/// The close after a connect that times out waits for the answer to the
/// `CreateSession` and for the `CloseSession` for [`GRACE`] in all. The client sends
/// the `CreateSession` just before the deadline and the server answers it at 9 s, or
/// the client sends it just after the check of timeouts at 4 s and the server answers
/// it just before its timeout, and then stops. The client sends no `CloseSession`,
/// and the second client still connects: the server drops a session that is not
/// active when its channel closes.
#[test]
fn a_connect_whose_create_session_answers_late_ends_one_grace_later() {
    let before_the_deadline = times_out_and_closes(&[
        Span::from_nanos(0),
        Span::from_nanos(4_996_000_000),
        Span::from_nanos(4_999_500_000),
        Span::from_nanos(9_000_000_000),
        Span::from_nanos(9_000_100_000),
    ]);
    let after_a_check = times_out_and_closes(&[
        Span::from_nanos(0),
        Span::from_nanos(4_001_000_000),
        Span::from_nanos(4_004_500_000),
        Span::from_nanos(9_999_000_000),
        Span::from_nanos(9_999_100_000),
    ]);
    let bound = (Span::from_nanos(5_000_000_000 + GRACE.nanos()), 0);
    assert_eq!((before_the_deadline, after_a_check), (bound, bound));
}

/// A client that waits in [`Client::idle`] for longer than the channel lifetime of the
/// server (10 minutes) keeps its channel, and reads after it.
#[test]
fn a_client_idle_for_eleven_minutes_reads() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let first = client.read_one(&VALUE).await;
            let idle = peer
                .clock
                .sleep(Span::from_nanos(11 * Span::MINUTE.nanos()));
            client
                .idle(idle)
                .await
                .expect("the client keeps its session");
            (first, client.read_one(&VALUE).await, peer.value)
        })
        .await
    });
    let (first, second, value) = reads;
    assert_eq!((first, second), (Ok(value), Ok(value)));
}

/// A client idle for longer than the session timeout that it asks for (20 min) keeps
/// its session, as it reads the state of the server once per 10 s of the idle.
#[test]
fn a_client_idle_for_twenty_one_minutes_reads() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let first = client.read_one(&VALUE).await;
            let idle = peer
                .clock
                .sleep(Span::from_nanos(21 * Span::MINUTE.nanos()));
            client
                .idle(idle)
                .await
                .expect("the client keeps its session");
            let states = peer.states.get();
            (first, client.read_one(&VALUE).await, peer.value, states)
        })
        .await
    });
    let (first, second, value, states) = reads;
    assert_eq!((first, second, states), (Ok(value), Ok(value), 21 * 6));
}

/// A server that closes while the client waits in [`Client::idle`] ends the wait when
/// the close reaches the client, one hop (250 us) later.
#[test]
fn a_server_that_closes_ends_idle_at_once() {
    let (idle, _) = run(0, async |node| {
        let clock = node.clock();
        let mut client = serve(&node, 4, async |mut peer| peer.connected().await).await;
        let start = clock.now();
        let idle = client.idle(clock.sleep(Span::MINUTE)).await;
        (idle, clock.now() - start)
    });
    let closed = Err(Status::BAD_CONNECTION_CLOSED);
    assert_eq!(idle, (closed, Span::from_nanos(250_000)));
}

/// A client whose session the server dropped ends [`Client::idle`] at once with
/// `BadSessionClosed`: the read that finds the drop leaves the channel open.
#[test]
fn a_client_with_no_session_ends_idle_at_once() {
    let (idle, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            peer.server
                .set_session_timeout(Span::from_nanos(5_000_000_000));
            let mut client = peer.connected().await;
            peer.clock.sleep(Span::from_nanos(6_000_000_000)).await;
            let read = client.read_one(&VALUE).await;
            let start = peer.clock.now();
            let idle = client.idle(peer.clock.sleep(Span::MINUTE)).await;
            (read, idle, peer.clock.now() - start)
        })
        .await
    });
    let lost = Err(Fault::status(Status::BAD_SESSION_ID_INVALID));
    let closed = Err(Status::BAD_SESSION_CLOSED);
    assert_eq!(idle, (lost, closed, Span::ZERO));
}

/// A check passes on each status of the state of the server: Uncertain, and Bad,
/// `BadNoData` too.
#[test]
fn a_check_passes_on_each_status_of_the_state() {
    let idle = |quality: Quality| {
        run(0, async move |node| {
            serve(&node, 4, async |mut peer| {
                peer.quality.set(quality);
                let mut client = peer.connected().await;
                client
                    .idle(peer.clock.sleep(Span::from_nanos(15_000_000_000)))
                    .await
            })
            .await
        })
        .0
    };
    assert_eq!(idle(Quality(0x4000_0000)), Ok(()));
    assert_eq!(idle(Quality(Status::BAD_NO_DATA.0)), Ok(()));
    assert_eq!(idle(Quality(Status::BAD_NODE_ID_UNKNOWN.0)), Ok(()));
}

/// A connect whose wait has already ended gives no client.
#[test]
fn a_connect_whose_wait_has_ended_gives_none() {
    let (connect, _) = run(0, async |node| {
        serve(&node, 1, async |mut peer| {
            let url = CString::new(peer.url.clone()).expect("the URL holds no NUL");
            let time = event::Time::monotonic(Clock::clone(&peer.clock));
            let net = Net::clone(&peer.net);
            let ended = future::ready(());
            let connect = Client::connect(
                time,
                tracing::Span::none(),
                net,
                &mut peer.rng,
                &url,
                None,
                ended,
            )
            .await;
            connect.map(|client| client.is_none())
        })
        .await
    });
    assert_eq!(connect, Ok(true));
}

/// The close of a client idle for 15 min frees its session on a server with room for
/// one, so the next client connects.
#[test]
fn the_close_of_a_client_idle_for_fifteen_minutes_frees_its_session() {
    let (connect, _) = run(0, async |node| {
        serve(&node, 1, async |mut peer| {
            let mut client = peer.connected().await;
            let idle = peer
                .clock
                .sleep(Span::from_nanos(15 * Span::MINUTE.nanos()));
            client
                .idle(idle)
                .await
                .expect("the client keeps its session");
            client.close().await;
            let url = peer.url.clone();
            let client = peer.connect(&url).await;
            let read = match client {
                Ok(mut client) => {
                    let read = client.read_one(&VALUE).await;
                    client.close().await;
                    Ok(read)
                }
                Err(status) => Err(status),
            };
            (read, peer.value)
        })
        .await
    });
    let (read, value) = connect;
    assert_eq!(read, Ok(Ok(value)));
}

/// A connect to a server that activates the session and then stops fails with
/// `BadTimeout` within four request timeouts: the read of its `MaxNodesPerRead` times
/// out, then the close. The relay drops each message of the server from the first Read
/// response on.
#[test]
fn a_connect_to_a_server_that_stops_after_the_activation_ends_in_four_request_timeouts()
{
    let ((status, took), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            let mut stopped = false;
            let rewrite = |message: &mut Vec<u8>| {
                stopped |= handle(message).is_some();
                if stopped {
                    message.clear();
                }
            };
            let start = peer.clock.now();
            let connect =
                relayed(&node, &Relay::default(), rewrite, peer.connect(&url)).await;
            (connect.err(), peer.clock.now() - start)
        })
        .await
    });
    assert_eq!(status, Some(Status::BAD_TIMEOUT));
    assert!(took <= Span::from_nanos(20_000_000_000), "{took:?}");
}

/// Connects through [`RELAY`] to a server with room for one session that stops at the
/// first time of `flips` after the connect starts, runs again at the next, and so on,
/// and runs from the end of the connect. Asserts that the connect fails with
/// `BadTimeout` and that a second client then connects, so the first left no session.
/// Gives the time that the first connect took, and the `CloseSession` requests that
/// it sent.
fn times_out_and_closes(flips: &[Span]) -> (Span, usize) {
    let flips = flips.to_vec();
    let ((first, took, second, closes), _) = run(0, async move |node| {
        serve(&node, 1, async |mut peer| {
            let (url, gate) = (peer.url.clone(), peer.gate.clone());
            let control = Relay::default();
            let raw = CString::new(relay_url(&node)).expect("no NUL");
            let time = event::Time::monotonic(Clock::clone(&peer.clock));
            let net = Net::clone(&peer.net);
            // The relay runs until the second connect ends, so it moves each late
            // byte of the first client.
            let body = async {
                let start = peer.clock.now();
                let first = {
                    let mut connect = pin!(open(time, net, &mut peer.rng, &raw));
                    let mut flips: Vec<_> = flips
                        .iter()
                        .map(|at| Box::pin(peer.clock.sleep(*at)))
                        .collect();
                    let mut flipped = 0;
                    let connect = poll_fn(|cx| {
                        while flipped < flips.len()
                            && flips[flipped].as_mut().poll(cx).is_ready()
                        {
                            flipped += 1;
                            if flipped % 2 == 1 {
                                gate.close();
                            } else {
                                gate.open();
                            }
                            // `serve` runs the server before this body.
                            cx.waker().wake_by_ref();
                        }
                        connect.as_mut().poll(cx)
                    });
                    connect.await.err()
                };
                let took = peer.clock.now() - start;
                gate.open();
                let second = match peer.connect(&url).await {
                    Ok(client) => {
                        client.close().await;
                        None
                    }
                    Err(status) => Some(status),
                };
                (first, took, second)
            };
            let (first, took, second) = relayed(&node, &control, |_| {}, body).await;
            (first, took, second, control.closes.get())
        })
        .await
    });
    assert_eq!((first, second), (Some(Status::BAD_TIMEOUT), None));
    (took, closes)
}

/// A server adds a namespace after a client connects, and a Double in it. The client
/// reads it, then a new client reads it.
#[test]
fn a_read_of_a_namespace_that_the_server_adds_after_the_connect_gets_its_value() {
    let ((value, reads), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let first = client.read_one(&VALUE).await;
            let namespace = peer.server.add_namespace(c"urn:added");
            let added = Node {
                namespace,
                key: Key::Number(1000),
            };
            peer.server.add_scalar(&added, Value::F64(2.5));
            let second = client.read_one(&added).await;
            client.close().await;
            let mut fresh = peer.connected().await;
            let third = fresh.read_one(&added).await;
            fresh.close().await;
            (peer.value, (first, namespace, second, third))
        })
        .await
    });
    assert_eq!(
        reads,
        (Ok(value), 2, Ok(Value::F64(2.5)), Ok(Value::F64(2.5)))
    );
}

/// A node in namespace 1 named by `length` bytes.
fn long(length: usize) -> Node {
    let name = Name::new(&"x".repeat(length)).expect("a name that is not empty");
    Node {
        namespace: 1,
        key: Key::Name(name),
    }
}

/// Gives the counts of the requests of `nodes` on `client`, or the error.
fn counts(client: &Client, nodes: &[Node]) -> Result<Vec<usize>, Large> {
    let nodes = client.nodes(nodes);
    nodes
        .large()
        .first()
        .map_or_else(|| Ok(nodes.counts()), |large| Err(large.clone()))
}

/// Gives the nodes of `split`, a [`Client::nodes`] in which each node fits.
///
/// # Panics
///
/// If a node does not fit.
fn fits(nodes: Nodes) -> Nodes {
    assert_eq!(nodes.large(), [], "each node fits");
    nodes
}

/// Gives the largest of 1 to `past` (not included) for which `fits` holds, as `fits`
/// holds up to a bound and not past it.
fn largest(past: usize, fits: impl Fn(usize) -> bool) -> usize {
    let (mut fits_at, mut past) = (1, past);
    while past - fits_at > 1 {
        let probe = usize::midpoint(fits_at, past);
        if fits(probe) {
            fits_at = probe;
        } else {
            past = probe;
        }
    }
    fits_at
}

/// Gives the most bytes of the name of a [`long`] node after `nodes` that `client`
/// reads with them in one request, below `past`.
fn fitting(client: &Client, nodes: &[Node], past: usize) -> usize {
    largest(past, |length| {
        let mut list = nodes.to_vec();
        list.push(long(length));
        counts(client, &list).is_ok_and(|counts| counts.len() == 1)
    })
}

/// Gives the most [`long`] nodes of `length` bytes that `client` reads in one request.
fn most_of(client: &Client, length: usize) -> usize {
    largest(1 << 14, |count| {
        counts(client, &vec![long(length); count]).is_ok_and(|counts| counts.len() == 1)
    })
}

/// The request of a session takes 8 chunks, [`CHUNKS`], of 65536 bytes less the
/// 24 bytes of their headers, less 7 bytes at the end of each chunk but the last. The
/// client refuses to send 1 byte more, and a node after another goes in a second
/// request. A request whose one name fills each chunk to its end is 49 bytes longer
/// when open62541 refuses it.
#[test]
fn a_request_takes_the_chunks_of_one_message() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let length = fitting(&client, &[], 1 << 20);
            peer.server.add_scalar(&long(length), Value::F64(2.5));
            let fit = fits(client.nodes(&[VALUE, long(length)]));
            let split = fit.counts();
            let read = client.read(&fit).await.map(<[_]>::to_vec);
            let past = client.nodes(&[VALUE, long(length + 1)]).large().to_vec();
            let after = client.read_one(&VALUE).await;
            let unchecked = client.unchecked(&[long(length + 49)]);
            let full = client.read(&unchecked).await.map(<[_]>::to_vec);
            let unchecked = client.unchecked(&[long(length + 50)]);
            let sent = client.read(&unchecked).await.map(<[_]>::to_vec);
            client.close().await;
            (split, read, past, after, full, sent, peer.value)
        })
        .await
    });
    let (split, read, past, after, full, sent, value) = reads;
    assert_eq!(split, [1, 1]);
    assert_eq!(read, Ok(vec![Ok(value), Ok(Value::F64(2.5))]));
    assert_eq!(
        full,
        Ok(vec![Err(Fault::status(Status::BAD_NODE_ID_UNKNOWN))])
    );
    let most = 8 * (65_536 - 24) - 7 * 7;
    let large = Large {
        index: 1,
        bytes: most + 1,
        most,
    };
    assert_eq!(past, [large]);
    assert_eq!(after, Ok(value));
    assert_eq!(sent, Err(Status::BAD_RESPONSE_TOO_LARGE));
}

/// A server with a `MaxMessageSize` of 8192 bytes takes a request of 8192 bytes, and
/// refuses 1 byte more. A good answer of 239 Doubles takes at most 36 bytes and 34 per
/// node, 8162 bytes, and of 240 nodes, 8196 bytes: the 240th node goes in a second
/// request.
#[test]
fn a_read_takes_the_max_message_size_of_the_server() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            peer.server.set_limits(Limits {
                message: 8192,
                ..Limits::default()
            });
            let mut client = peer.connected().await;
            let length = fitting(&client, &[], 9000);
            peer.server.add_scalar(&long(length), Value::F64(2.5));
            let fit = fits(client.nodes(&[long(length)]));
            let read = client.read(&fit).await.map(<[_]>::to_vec);
            let past = client.nodes(&[long(length + 1)]).large().to_vec();
            let most = fits(client.nodes(&[VALUE; 239]));
            let values = client.read(&most).await.map(<[_]>::to_vec);
            let split = fits(client.nodes(&[VALUE; 240]));
            let counts = split.counts();
            let split = client.read(&split).await.map(<[_]>::to_vec);
            let unchecked = client.unchecked(&[long(length + 1)]);
            let sent = client.read(&unchecked).await.map(<[_]>::to_vec);
            client.close().await;
            (read, past, values, counts, split, sent, peer.value)
        })
        .await
    });
    let (read, past, values, counts, split, sent, value) = reads;
    assert_eq!(read, Ok(vec![Ok(Value::F64(2.5))]));
    let large = Large {
        index: 0,
        bytes: 8193,
        most: 8192,
    };
    assert_eq!(past, [large]);
    assert_eq!(values, Ok(vec![Ok(value); 239]));
    assert_eq!((counts, split), (vec![239, 1], Ok(vec![Ok(value); 240])));
    assert_eq!(sent, Err(Status::BAD_TCP_MESSAGE_TOO_LARGE));
}

/// A server with buffers of 8192 bytes and a `MaxChunkCount` of 4 gives a message of 4
/// chunks of 8168 bytes less 21: one request of 959 nodes, and not 960.
#[test]
fn a_read_takes_the_buffers_and_chunks_of_the_server() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            peer.server.set_limits(Limits {
                buffer: 8192,
                chunks: 4,
                ..Limits::default()
            });
            let mut client = peer.connected().await;
            let nodes = fits(client.nodes(&vec![VALUE; 960]));
            let values = client.read(&nodes).await.map(<[_]>::to_vec);
            client.close().await;
            (nodes.counts(), values, peer.value)
        })
        .await
    });
    let (counts, values, value) = reads;
    assert_eq!((counts, values), (vec![959, 1], Ok(vec![Ok(value); 960])));
}

/// A client with a receive buffer of 8192 bytes sends chunks of 65536 bytes and takes
/// chunks of 8192 bytes: a request takes 8 chunks of 65512 bytes less 49, and a good
/// answer 8 chunks of 8168 bytes less 49, 1919 Doubles and not 1920.
#[test]
fn a_request_and_an_answer_each_take_the_buffer_of_their_side() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let url = peer.url.clone();
            let mut client = peer.limited(&url, Some(8192), None).await;
            let length = fitting(&client, &[], 1 << 20);
            let request = client.nodes(&[long(length + 1)]).large().to_vec();
            let nodes = fits(client.nodes(&vec![VALUE; 1920]));
            let values = client.read(&nodes).await.map(<[_]>::to_vec);
            client.close().await;
            (request, nodes.counts(), values, peer.value)
        })
        .await
    });
    let (request, counts, values, value) = reads;
    let most = 8 * (65_536 - 24) - 7 * 7;
    let large = Large {
        index: 0,
        bytes: most + 1,
        most,
    };
    assert_eq!(request, [large]);
    assert_eq!((counts, values), (vec![1919, 1], Ok(vec![Ok(value); 1920])));
}

/// The limits of the channel live only in the copy that the key gives.
#[test]
fn the_key_of_the_limits_gives_no_value_without_a_copy() {
    let (status, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let client = peer.connected().await;
            let mut value = vec![0_u64; ffi::types()[ffi::VARIANT].size().div_ceil(8)];
            let key = ffi::test::QualifiedName {
                namespace: 0,
                name: ffi::Bytes {
                    length: 13,
                    data: b"messageLimits".as_ptr().cast_mut(),
                },
            };
            // SAFETY: the client lives, `value` has room for a `UA_Variant`, and the
            // key outlives the call.
            let status = Status(unsafe {
                ffi::test::UA_Client_getConnectionAttribute(
                    client.raw.as_ptr(),
                    key,
                    value.as_mut_ptr().cast(),
                )
            });
            client.close().await;
            status
        })
        .await
    });
    assert_eq!(status, Status::BAD_NOT_SUPPORTED);
}

#[test]
fn a_limit_of_0_from_a_server_is_no_limit() {
    assert_eq!(most(8168, 0, 0), 8 * 8168 - 7 * 7);
    assert_eq!(most(8168, 0, 9), 8 * 8168 - 7 * 7);
    assert_eq!(most(8168, 0, 7), 7 * 8168 - 6 * 7);
    assert_eq!(most(8168, 50_000, 0), 50_000);
}

/// No form of a key leaves more than [`SPLIT`] bytes empty at a chunk end. A new form
/// does not build here until it gives the largest part of it that open62541 moves
/// whole, and that part does not build past `SPLIT + 1`.
#[test]
fn no_form_of_a_key_leaves_more_than_split() {
    // The match and its `const` blocks are the test: each runs when the test builds,
    // with no call.
    let _ = |key: &Key| match key {
        // The mask, the namespace, and the number of a NodeId by number.
        Key::Number(_) => const { assert!(7 - 1 <= SPLIT) },
        // The length of the string of a NodeId by name: its bytes split.
        Key::Name(_) => const { assert!(4 - 1 <= SPLIT) },
    };
}

/// The most nodes that one request takes are read, also when open62541 leaves the end
/// of a chunk empty: names of 37 and 44 bytes in chunks of 65536 bytes, and 450 to 469
/// numbers and a name that fills the request in chunks of 8192 bytes.
#[test]
fn the_most_nodes_of_one_request_are_read() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let mut reads = Vec::new();
            for length in [37, 44] {
                let count = most_of(&client, length);
                let nodes = fits(client.nodes(&vec![long(length); count]));
                reads.push((count, client.read(&nodes).await.map(<[_]>::len)));
            }
            client.close().await;
            peer.server.set_limits(Limits {
                buffer: 8192,
                chunks: 4,
                ..Limits::default()
            });
            let mut client = peer.connected().await;
            for count in 450..470 {
                let mut list = vec![VALUE; count];
                list.push(long(fitting(&client, &list, 1 << 15)));
                let nodes = fits(client.nodes(&list));
                reads.push((count + 1, client.read(&nodes).await.map(<[_]>::len)));
            }
            client.close().await;
            reads
        })
        .await
    });
    for (count, read) in reads {
        assert_eq!(read, Ok(count));
    }
}

/// A read of no nodes gives no values, and the next read its value.
#[test]
fn a_read_of_no_nodes_gives_no_values() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let none = fits(client.nodes(&[]));
            let read = client.read(&none).await.map(<[_]>::to_vec);
            let after = client.read_one(&VALUE).await;
            client.close().await;
            (read, after, peer.value)
        })
        .await
    });
    let (read, after, value) = reads;
    assert_eq!(read, Ok(vec![]));
    assert_eq!(after, Ok(value));
}

/// The node `i` of [`numbered`], whose value is `i`.
fn numbered(index: u32) -> Node {
    Node {
        namespace: 1,
        key: Key::Number(5000 + index),
    }
}

/// Reads `count` nodes of [`numbered`] and an unknown node at the index 3 on a server
/// with the `MaxNodesPerRead` `most`, and gives the counts of the requests, the values,
/// and the values that the server gives.
fn read_per(most: u32, count: u32) -> (Vec<usize>, Read, Values) {
    read_on(count, move |server| server.set_per_read(most))
}

/// As [`read_per`], on a server that `limit` gives its `MaxNodesPerRead`.
fn read_on(
    count: u32,
    limit: impl FnOnce(&Server) + Send + 'static,
) -> (Vec<usize>, Read, Values) {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            limit(peer.server);
            let mut nodes = Vec::new();
            let mut values = Vec::new();
            for index in 0..count {
                if index == 3 {
                    nodes.push(long(1));
                    values.push(Err(Fault::status(Status::BAD_NODE_ID_UNKNOWN)));
                }
                peer.server.add_scalar(&numbered(index), Value::U32(index));
                nodes.push(numbered(index));
                values.push(Ok(Value::U32(index)));
            }
            let mut client = peer.connected().await;
            let nodes = fits(client.nodes(&nodes));
            let read = client.read(&nodes).await.map(<[_]>::to_vec);
            client.close().await;
            (nodes.counts(), read, values)
        })
        .await
    })
    .0
}

/// A read takes at most the `MaxNodesPerRead` of the server in one request, and gives
/// the value of each node in order. A limit of 0 is no limit. No nodes take no request.
#[test]
fn a_request_takes_at_most_the_max_nodes_per_read_of_the_server() {
    let (counts, read, values) = read_per(10, 24);
    assert_eq!((counts, read), (vec![10, 10, 5], Ok(values)));
    let (counts, read, values) = read_per(0, 24);
    assert_eq!((counts, read), (vec![25], Ok(values)));
    assert_eq!(read_per(10, 0), (vec![], Ok(vec![]), vec![]));
}

/// The size that `nodes` gives the read of each node is the size that open62541
/// encodes, at each bound of each form of a `NodeId`.
#[test]
fn the_size_of_the_read_of_a_node_is_its_size_in_open62541() {
    let zero = Node {
        namespace: 0,
        key: Key::Number(0),
    };
    let one = |node: &Node| Request::values(&[node.into()]).bytes(0);
    let number = |namespace, key| Node {
        namespace,
        key: Key::Number(key),
    };
    let name = |namespace, text: &str| Node {
        namespace,
        key: Key::Name(Name::new(text).expect("a name")),
    };
    let nodes = [
        number(0, 0xFF),
        number(0, 0x100),
        number(1, 0),
        number(0xFF, 0xFFFF),
        number(0xFF, 0x1_0000),
        number(0x100, 0),
        number(u16::MAX, u32::MAX),
        name(0, "a"),
        name(u16::MAX, "\u{e9}"),
        name(3, &"n".repeat(300)),
    ];
    for node in &nodes {
        let open62541 = one(node) - one(&zero);
        assert_eq!(id_bytes(node) - id_bytes(&zero), open62541, "{node:?}");
    }
}

/// A `MaxNodesPerRead` that the server lacks, or that is not a `UInt32`, is no limit:
/// the server gets one Read for the connect and one for the 25 nodes.
#[test]
fn a_max_nodes_per_read_that_is_absent_or_of_another_type_is_no_limit() {
    let wrong = Node {
        namespace: 0,
        key: Key::Number(ffi::MAX_NODES_PER_READ),
    };
    for limit in [None, Some(Value::U64(1))] {
        let wrong = wrong.clone();
        let ((read, values, responses), _) = run(0, async move |node| {
            serve(&node, 4, async |mut peer| {
                if let Some(limit) = limit {
                    peer.server.add_scalar(&wrong, limit);
                }
                let nodes: Vec<_> = (0..25).map(numbered).collect();
                let values: Vec<_> =
                    (0..25).map(|index| Ok(Value::U32(index))).collect();
                for (index, node) in (0..).zip(&nodes) {
                    peer.server.add_scalar(node, Value::U32(index));
                }
                let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
                let mut responses = 0;
                let rewrite = |message: &mut Vec<u8>| {
                    if handle(message).is_some() {
                        responses += 1;
                    }
                };
                let read = relayed(&node, &Relay::default(), rewrite, async {
                    let mut client =
                        peer.connect(&url).await.expect("the client connects");
                    let nodes = fits(client.nodes(&nodes));
                    let read = client.read(&nodes).await.map(<[_]>::to_vec);
                    client.close().await;
                    read
                })
                .await;
                (read, values, responses)
            })
            .await
        });
        assert_eq!((read, responses), (Ok(values), 2));
    }
}

/// A read of more requests than the reads in flight sends each next request when one
/// ends, and gives the value of each node in order.
#[test]
fn a_read_of_more_requests_than_reads_in_flight_reads_each() {
    let count = u32::from(READS) * 2 + 3;
    let (counts, read, values) = read_per(1, count);
    assert_eq!(counts, vec![1; values.len()]);
    assert_eq!(read, Ok(values));
}

/// Reads the list of `nodes` that one client checked on another client, and gives the
/// text of the panic. The panic is caught inside `serve`, so the server closes.
fn read_on_another_client(nodes: &'static [Node]) -> Option<&'static str> {
    run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let first = peer.connected().await;
            let mut second = peer.connected().await;
            let nodes = fits(first.nodes(nodes));
            let panic = caught(second.read(&nodes))
                .await
                .expect_err("the read panics");
            panic.downcast_ref::<&str>().copied()
        })
        .await
    })
    .0
}

/// A client reads only a list that it checked, as the limits of another session can
/// be smaller.
#[test]
fn a_read_of_the_nodes_of_another_client_panics() {
    assert_eq!(
        read_on_another_client(&[VALUE]),
        Some("invariant: a client reads only the nodes that it checked")
    );
}

/// A client reads only a list that it checked, also when the list is empty.
#[test]
fn a_read_of_no_nodes_of_another_client_panics() {
    assert_eq!(
        read_on_another_client(&[]),
        Some("invariant: a client reads only the nodes that it checked")
    );
}

/// The port of the relay of [`relayed`].
pub(crate) const RELAY: u16 = 4841;

/// The binary encoding of a Read response, a `NodeId` of 4 bytes.
const READ_RESPONSE: [u8; 4] = [0x01, 0x00, 0x7A, 0x02];

/// The binary encoding of a `CloseSession` request, a `NodeId` of 4 bytes.
const CLOSE_SESSION: [u8; 4] = [0x01, 0x00, 0xD9, 0x01];

/// The binary encoding of a Browse request, a `NodeId` of 4 bytes.
const BROWSE: [u8; 4] = [0x01, 0x00, 0x0F, 0x02];

/// Runs `body` while [`relay`] runs on `node` with `control`, and gives each message
/// of the server to `rewrite`.
pub(crate) async fn relayed<T>(
    node: &node::Node,
    control: &Relay,
    rewrite: impl FnMut(&mut Vec<u8>),
    body: impl Future<Output = T>,
) -> T {
    beside(relay(node, control, |_| {}, rewrite), body).await
}

/// Runs `body` while [`relay`] runs on `node`, and gives each message of the client to
/// `up`.
pub(crate) async fn relayed_up<T>(
    node: &node::Node,
    up: impl FnMut(&mut Vec<u8>),
    body: impl Future<Output = T>,
) -> T {
    beside(relay(node, &Relay::default(), up, |_| {}), body).await
}

/// Runs `body` while `relay` runs.
async fn beside<T>(
    relay: impl Future<Output = ()>,
    body: impl Future<Output = T>,
) -> T {
    let mut relay = pin!(relay);
    let mut body = pin!(body);
    poll_fn(|cx| {
        assert!(relay.as_mut().poll(cx).is_pending(), "it never ends");
        body.as_mut().poll(cx)
    })
    .await
}

/// What a test reads and sets of a [`relay`].
#[derive(Default)]
pub(crate) struct Relay {
    /// The streams that it accepted.
    accepts: Cell<usize>,
    /// When set, it closes its listener at each accept and binds it again this long
    /// after that stream ends, so each dial in between is refused.
    pause: Option<Span>,
    /// When set, it drops both streams of the relay that runs at its next poll, and
    /// clears it.
    cut: Cell<bool>,
    /// The `CloseSession` requests that it moved to the server.
    closes: Cell<usize>,
    /// The time of the request header of each Browse request that it moved to the
    /// server, in 100 ns from 1601.
    pub(crate) browses: RefCell<Vec<i64>>,
}

/// Moves the bytes of each stream to [`RELAY`], one at a time, to the test server and
/// back. It gives each message of the client to `up` before the server gets it, and
/// each message of the server to `down` before the client gets it. A relay ends when
/// the client ends its side or `control` cuts it.
#[expect(clippy::infinite_loop, reason = "it runs until its test ends")]
async fn relay(
    node: &node::Node,
    control: &Relay,
    mut up: impl FnMut(&mut Vec<u8>),
    mut down: impl FnMut(&mut Vec<u8>),
) {
    let address = node.addresses()[0];
    let listen = tcp::Listen {
        local: SocketAddr::new(address, RELAY),
        backlog: 4,
        options: options(),
    };
    let mut listener = None;
    loop {
        let open = listener.get_or_insert_with(|| {
            node.net().listen(&listen).expect("the port is free")
        });
        let mut client = poll_fn(|cx| open.poll_accept(cx))
            .await
            .expect("the accept works");
        control.accepts.set(control.accepts.get() + 1);
        if control.pause.is_some() {
            listener = None;
        }
        let config = tcp::Config {
            remote: SocketAddr::new(address, PORT),
            options: options(),
        };
        let mut server = node
            .net()
            .connect(&config)
            .await
            .expect("the server listens");
        let (mut to_server, mut to_client) = (Pipe::default(), Pipe::default());
        poll_fn(|cx| {
            if control.cut.take() {
                return Poll::Ready(());
            }
            let ended = to_server.pump(cx, &mut client, &mut server, |message| {
                let close = is(message, CLOSE_SESSION);
                control
                    .closes
                    .set(control.closes.get() + usize::from(close));
                if is(message, BROWSE) {
                    assert_eq!(message[28], 4, "a token that is a GUID");
                    let time = message[47..55].try_into().expect("8 bytes");
                    control.browses.borrow_mut().push(i64::from_le_bytes(time));
                }
                up(message);
            });
            to_client.pump(cx, &mut server, &mut client, &mut down);
            if ended {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        if let Some(pause) = control.pause {
            node.clock().sleep(pause).await;
        }
    }
}

/// The URL of [`RELAY`] on `node`.
fn relay_url(node: &node::Node) -> String {
    format!("opc.tcp://{}:{RELAY}", node.addresses()[0])
}

/// A rewrite that puts the URL of the test server in place of each copy of the URL of
/// [`RELAY`], as a server behind a NAT or in a container gives a URL that its client
/// did not dial.
fn to_server(node: &node::Node) -> impl FnMut(&mut Vec<u8>) + use<> {
    let from = relay_url(node).into_bytes();
    let to = format!("opc.tcp://{}:{PORT}", node.addresses()[0]).into_bytes();
    assert_eq!(from.len(), to.len(), "the message keeps its length");
    move |message| {
        let mut at = 0;
        while let Some(found) =
            message[at..].windows(from.len()).position(|w| *w == from)
        {
            at += found;
            message[at..at + from.len()].copy_from_slice(&to);
            at += from.len();
        }
    }
}

/// A server that gives another URL than the one that the client dialed has the client
/// close its channel and dial the address of the given URL again, with that URL in its
/// HEL, and the connect works.
#[test]
fn a_connect_through_a_switch_of_the_url_reads() {
    let ((read, value, accepts), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay::default();
            let url = relay_url(&node);
            let read = relayed(&node, &control, to_server(&node), async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let read = client.read_one(&VALUE).await;
                client.close().await;
                read
            })
            .await;
            (read, peer.value, control.accepts.get())
        })
        .await
    });
    assert_eq!((read, accepts), (Ok(value), 2));
}

/// A failed dial after the switch of the URL takes the fallback to the given URL. Each
/// dial goes to the address of the given URL, so the fallback fails too, with its own
/// status.
#[test]
fn a_connect_whose_dial_after_a_switch_fails_takes_the_fallback() {
    let (out, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay {
                pause: Some(Span::from_nanos(10_000_000_000)),
                ..Relay::default()
            };
            let url = relay_url(&node);
            let connect =
                relayed(&node, &control, to_server(&node), peer.connect(&url));
            (connect.await.err(), control.accepts.get())
        })
        .await
    });
    assert_eq!(out, (Some(Status::BAD_CONNECTION_REJECTED), 1));
}

/// A refused dial after the switch of the URL takes the fallback to the given URL, and
/// the connect reads through it. The relay binds again one link delay after the FIN of
/// the client, with which the dial after the switch comes, and one link delay before
/// the dial of the fallback.
#[test]
fn a_connect_whose_dial_after_a_switch_is_refused_reads_through_the_fallback() {
    let ((read, value, accepts), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay {
                pause: Some(Span::from_nanos(250_000)),
                ..Relay::default()
            };
            let url = relay_url(&node);
            let mut to = to_server(&node);
            // The stream of the fallback gives the URL that the client dialed.
            let rewrite = |message: &mut Vec<u8>| {
                if control.accepts.get() == 1 {
                    to(message);
                }
            };
            let read = relayed(&node, &control, rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let read = client.read_one(&VALUE).await;
                client.close().await;
                read
            })
            .await;
            (read, peer.value, control.accepts.get())
        })
        .await
    });
    assert_eq!((read, accepts), (Ok(value), 2));
}

/// The binary encoding of a `GetEndpoints` response, a `NodeId` of 4 bytes.
const ENDPOINTS_RESPONSE: [u8; 4] = [0x01, 0x00, 0xAF, 0x01];

/// A server whose endpoint has another URL than the one that `FindServers` gave has the
/// client close its channel and dial again, and the connect works.
#[test]
fn a_connect_through_a_switch_of_the_endpoint_url_reads() {
    let ((read, value, accepts), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay::default();
            let url = relay_url(&node);
            let mut to = to_server(&node);
            let rewrite = |message: &mut Vec<u8>| {
                if is(message, ENDPOINTS_RESPONSE) && control.accepts.get() == 1 {
                    to(message);
                }
            };
            let read = relayed(&node, &control, rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let read = client.read_one(&VALUE).await;
                client.close().await;
                read
            })
            .await;
            (read, peer.value, control.accepts.get())
        })
        .await
    });
    assert_eq!((read, accepts), (Ok(value), 2));
}

/// An endpoint of mode Sign with the policy None has the client close its channel to
/// switch the security mode, and the client refuses that mode before it dials.
#[test]
fn a_switch_of_the_security_mode_ends_with_its_status() {
    let out = relayed_connect(|_| {
        let mut from = vec![1, 0, 0, 0, 47, 0, 0, 0];
        from.extend_from_slice(b"http://opcfoundation.org/UA/SecurityPolicy#None");
        move |control: &Relay, message: &mut Vec<u8>| {
            if is(message, ENDPOINTS_RESPONSE) && control.accepts.get() == 1 {
                let mut at = 0;
                while let Some(found) = message[at..]
                    .windows(from.len())
                    .position(|w| *w == from[..])
                {
                    at += found;
                    message[at] = 2;
                    at += from.len();
                }
            }
        }
    });
    assert_eq!(out, (Some(Status::BAD_SECURITY_MODE_REJECTED), 1));
}

/// A message on an open channel that the client cannot process ends the session with
/// the status of its error, and the client does not dial again, also after a switch of
/// the URL.
#[test]
fn a_message_that_the_client_cannot_process_ends_the_session_with_no_dial() {
    let (out, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay::default();
            let url = relay_url(&node);
            let clock = Clock::clone(&peer.clock);
            let bad = Cell::new(false);
            let mut to = to_server(&node);
            let rewrite = |message: &mut Vec<u8>| {
                to(message);
                if bad.get() && message.starts_with(b"MSG") {
                    // The key of another channel.
                    message[8] ^= 0xFF;
                }
            };
            let reads = relayed(&node, &control, rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                bad.set(true);
                let reads =
                    (client.read_one(&VALUE).await, client.read_one(&VALUE).await);
                clock.sleep(Span::from_nanos(10_000_000)).await;
                reads
            })
            .await;
            (reads, control.accepts.get())
        })
        .await
    });
    let reads = (
        Err(Fault::status(Status::BAD_SECURE_CHANNEL_ID_INVALID)),
        Err(Fault::status(Status::BAD_SERVER_NOT_CONNECTED)),
    );
    assert_eq!(out, (reads, 2));
}

/// A message of the connect on an open channel that the client cannot process ends the
/// connect with the status of its error, and the client does not dial again. The first
/// `MSG` comes before the switch of the URL, so it ends after one dial.
#[test]
fn a_connect_message_that_the_client_cannot_process_ends_the_connect_with_its_status() {
    let ends: Vec<_> = (0..5).map(bad_connect_message).collect();
    let mut want = vec![(Some(Status::BAD_SECURE_CHANNEL_ID_INVALID), 2); 5];
    want[0].1 = 1;
    assert_eq!(ends, want);
}

/// Connects through a relay that gives the client the `MSG` of the server of index
/// `bad` with the key of another channel. Gives the status of the connect, and the
/// accepts 10 ms after it ends.
fn bad_connect_message(bad: usize) -> (Option<Status>, usize) {
    relayed_connect(move |node| {
        let mut given = 0;
        let mut to = to_server(node);
        move |_: &Relay, message: &mut Vec<u8>| {
            to(message);
            if message.starts_with(b"MSG") {
                if given == bad {
                    message[8] ^= 0xFF;
                }
                given += 1;
            }
        }
    })
}

/// A read whose answer is an `ERR` with more bytes behind it in one buffer gives the
/// status of the `ERR`, not that of the bytes that the closed channel cannot process.
#[test]
fn a_read_whose_answer_is_an_err_and_more_bytes_gives_the_status_of_the_err() {
    let (read, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay::default();
            let url = relay_url(&node);
            let bad = Cell::new(false);
            let mut to = to_server(&node);
            let rewrite = |message: &mut Vec<u8>| {
                to(message);
                if bad.get() && is(message, READ_RESPONSE) {
                    let mut both = err(Status::BAD_RESPONSE_TOO_LARGE);
                    both.append(message);
                    *message = both;
                }
            };
            relayed(&node, &control, rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                bad.set(true);
                client.read_one(&VALUE).await
            })
            .await
        })
        .await
    });
    assert_eq!(read, Err(Fault::status(Status::BAD_RESPONSE_TOO_LARGE)));
}

/// An `ERR` in place of the answer to `ActivateSession` ends the connect with the
/// status of the `ERR`, not that of the new session that the client then asks for.
#[test]
fn an_err_in_place_of_the_activate_answer_ends_the_connect_with_its_status() {
    let out = relayed_connect(|node| {
        let mut to = to_server(node);
        move |_: &Relay, message: &mut Vec<u8>| {
            to(message);
            if is(message, ACTIVATE_RESPONSE) {
                *message = err(Status::BAD_SESSION_CLOSED);
            }
        }
    });
    assert_eq!(out, (Some(Status::BAD_SESSION_CLOSED), 2));
}

/// An `ERR` always closes the channel, so one with a code that is not Bad, in place of
/// a read answer, ends the read with `BadDecodingError`.
#[test]
fn a_read_whose_answer_is_an_err_of_a_code_that_is_not_bad_gives_bad_decoding_error() {
    let reads: Vec<_> = NOT_BAD.into_iter().map(read_after_err).collect();
    let want = vec![Err(Fault::status(Status::BAD_DECODING_ERROR)); NOT_BAD.len()];
    assert_eq!(reads, want);
}

/// An `ERR` with a code that is not Bad, in place of the answer to `ActivateSession`,
/// ends the connect with `BadDecodingError`.
#[test]
fn an_err_of_a_code_that_is_not_bad_ends_the_connect_with_bad_decoding_error() {
    let ends: Vec<_> = NOT_BAD.into_iter().map(activate_err).collect();
    let want = vec![(Some(Status::BAD_DECODING_ERROR), 2); NOT_BAD.len()];
    assert_eq!(ends, want);
}

/// Good, a Good code with a subcode (`GoodCallAgain`), and Uncertain.
const NOT_BAD: [Status; 3] = [Status(0), Status(0x00A9_0000), Status(0x4000_0000)];

/// Gives the read of `VALUE` whose answer is an `ERR` of `status`.
fn read_after_err(status: Status) -> Result<Value, Fault> {
    let (read, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay::default();
            let url = relay_url(&node);
            let bad = Cell::new(false);
            let mut to = to_server(&node);
            let rewrite = |message: &mut Vec<u8>| {
                to(message);
                if bad.get() && is(message, READ_RESPONSE) {
                    *message = err(status);
                }
            };
            relayed(&node, &control, rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                bad.set(true);
                client.read_one(&VALUE).await
            })
            .await
        })
        .await
    });
    read
}

/// Gives the end of a connect whose answer to `ActivateSession` is an `ERR` of
/// `status`, as `relayed_connect` does.
fn activate_err(status: Status) -> (Option<Status>, usize) {
    relayed_connect(move |node| {
        let mut to = to_server(node);
        move |_: &Relay, message: &mut Vec<u8>| {
            to(message);
            if is(message, ACTIVATE_RESPONSE) {
                *message = err(status);
            }
        }
    })
}

/// A session answer whose service result is not Good and not Bad ends the connect with
/// `BadDecodingError`.
#[test]
fn a_session_answer_of_a_code_that_is_not_bad_ends_the_connect_with_bad_decoding_error()
{
    let ends: Vec<_> = [CREATE_RESPONSE, ACTIVATE_RESPONSE]
        .into_iter()
        .flat_map(|kind| NOT_GOOD.map(|status| session_result(kind, status)))
        .collect();
    assert_eq!(ends, vec![(Some(Status::BAD_DECODING_ERROR), 1); 4]);
}

/// A new session whose answer has a service result that is not Good and not Bad ends
/// the read in flight with `BadDecodingError`.
#[test]
fn a_read_at_a_new_session_of_a_code_that_is_not_bad_gives_bad_decoding_error() {
    let reads: Vec<_> = NOT_GOOD.into_iter().map(read_at_new_session).collect();
    let want = (
        Some(Status::BAD_SESSION_CLOSED),
        Some(Status::BAD_DECODING_ERROR),
    );
    assert_eq!(reads, vec![want; NOT_GOOD.len()]);
}

/// A Read answer whose service result is Good with a subcode or Uncertain gives its
/// values.
#[test]
fn a_read_answer_of_a_code_that_is_not_bad_gives_its_values() {
    for status in NOT_GOOD {
        let (value, read) = read_of_result(status, false);
        assert_eq!(read, Ok(value), "{status:?}");
    }
}

/// The answer to the read of `MaxNodesPerRead` whose service result is Good with a
/// subcode or Uncertain gives its value, so the connect connects.
#[test]
fn a_max_nodes_per_read_answer_of_a_code_that_is_not_bad_connects() {
    for status in NOT_GOOD {
        let (value, read) = read_of_result(status, true);
        assert_eq!(read, Ok(value), "{status:?}");
    }
}

/// Gives the value of `VALUE` at the server, and its read whose answer has the service
/// result `status`, as each Read answer of the connect has too when `connect`.
fn read_of_result(status: Status, connect: bool) -> (Value, Result<Value, Fault>) {
    let (read, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay::default();
            let url = relay_url(&node);
            let armed = Cell::new(connect);
            let mut to = to_server(&node);
            let rewrite = |message: &mut Vec<u8>| {
                to(message);
                if armed.get() && is(message, READ_RESPONSE) {
                    set_result(message, status);
                }
            };
            relayed(&node, &control, rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                armed.set(true);
                (peer.value, client.read_one(&VALUE).await)
            })
            .await
        })
        .await
    });
    read
}

/// The codes of `NOT_BAD` that are not Good.
const NOT_GOOD: [Status; 2] = [Status(0x00A9_0000), Status(0x4000_0000)];

/// The type of a `CreateSession` answer.
const CREATE_RESPONSE: [u8; 4] = [0x01, 0x00, 0xD0, 0x01];

/// Sets the service result of the answer `message` to `status`.
fn set_result(message: &mut [u8], status: Status) {
    // The type of the answer, then its time and request handle.
    message[40..44].copy_from_slice(&status.0.to_le_bytes());
}

/// Gives the end of a connect whose answers of type `kind` have the service result
/// `status`, as `relayed_connect` does.
fn session_result(kind: [u8; 4], status: Status) -> (Option<Status>, usize) {
    relayed_connect(move |_| {
        move |_: &Relay, message: &mut Vec<u8>| {
            if is(message, kind) {
                set_result(message, status);
            }
        }
    })
}

/// Gives the errors of two reads of `VALUE`. The answer of the first has the service
/// result `BadSessionClosed`, so the client asks for a new session, whose answer has
/// the service result `status`.
fn read_at_new_session(status: Status) -> (Option<Status>, Option<Status>) {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay::default();
            let url = relay_url(&node);
            let armed = Cell::new(false);
            let creates = Cell::new(0);
            let mut to = to_server(&node);
            let rewrite = |message: &mut Vec<u8>| {
                to(message);
                if armed.get() && is(message, READ_RESPONSE) {
                    set_result(message, Status::BAD_SESSION_CLOSED);
                    armed.set(false);
                }
                if is(message, CREATE_RESPONSE) {
                    creates.set(creates.get() + 1);
                    if creates.get() == 2 {
                        set_result(message, status);
                    }
                }
            };
            relayed(&node, &control, rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let nodes = fits(client.nodes(std::slice::from_ref(&VALUE)));
                armed.set(true);
                let first = client.read(&nodes).await.err();
                (first, client.read(&nodes).await.err())
            })
            .await
        })
        .await
    });
    out
}

/// The type of an `ActivateSession` answer.
const ACTIVATE_RESPONSE: [u8; 4] = [0x01, 0x00, 0xD6, 0x01];

/// Gives an `ERR` of `status` with no reason.
fn err(status: Status) -> Vec<u8> {
    let mut body = status.0.to_le_bytes().to_vec();
    body.extend((-1_i32).to_le_bytes());
    crate::peer::frame(*b"ERRF", &body)
}

/// Connects through a relay that gives the client the first `pass` messages of the
/// server, and cuts both streams at the next one. Gives the status of the connect, and
/// the accepts 10 ms after it ends.
fn cut_after(pass: usize) -> (Option<Status>, usize) {
    relayed_connect(move |_| {
        let mut given = 0;
        move |control: &Relay, message: &mut Vec<u8>| {
            if given == pass {
                message.clear();
                control.cut.set(true);
            } else {
                given += 1;
            }
        }
    })
}

/// Connects through a relay that gives each message of the server to the rewrite that
/// `make` makes from the node. Gives the status of the connect, and the accepts 10 ms
/// after it ends.
fn relayed_connect<R: FnMut(&Relay, &mut Vec<u8>)>(
    make: impl FnOnce(&node::Node) -> R + Send + 'static,
) -> (Option<Status>, usize) {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay::default();
            let url = relay_url(&node);
            let clock = Clock::clone(&peer.clock);
            let mut rewrite = make(&node);
            let rewrite = |message: &mut Vec<u8>| rewrite(&control, message);
            let status = relayed(&node, &control, rewrite, async {
                let status = peer.connect(&url).await.err();
                clock.sleep(Span::from_nanos(10_000_000)).await;
                status
            })
            .await;
            (status, control.accepts.get())
        })
        .await
    });
    out
}

/// A reset while the request that creates or activates the session waits ends the
/// connect with `BadConnectionClosed` after one dial, though the end of that request
/// gives `BadSecureChannelClosed` first.
#[test]
fn a_reset_while_a_session_request_waits_ends_the_connect_with_no_dial() {
    let closed = (Some(Status::BAD_CONNECTION_CLOSED), 1);
    assert_eq!((cut_after(4), cut_after(5)), (closed, closed));
}

/// A reset of the streams of an active session ends it: each later read fails, and
/// the client does not dial again, also after a switch of the URL.
#[test]
fn a_read_after_a_reset_of_an_active_session_fails_with_no_dial() {
    let reads = (
        Err(Fault::status(Status::BAD_SECURE_CHANNEL_CLOSED)),
        Err(Fault::status(Status::BAD_SERVER_NOT_CONNECTED)),
    );
    let ends = [false, true].map(reset_of_an_active_session);
    assert_eq!(ends, [(reads, 1), (reads, 2)]);
}

/// Connects through a relay, with a switch of the URL when `switched`, and resets the
/// streams of the active session. Gives the two reads after the reset, and the accepts
/// 10 ms after them.
fn reset_of_an_active_session(switched: bool) -> (Reads, usize) {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let control = Relay::default();
            let url = relay_url(&node);
            let clock = Clock::clone(&peer.clock);
            let mut to = to_server(&node);
            let rewrite = |message: &mut Vec<u8>| {
                if switched {
                    to(message);
                }
            };
            let reads = relayed(&node, &control, rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                control.cut.set(true);
                let reads =
                    (client.read_one(&VALUE).await, client.read_one(&VALUE).await);
                // open62541 would dial again at once, within this wait.
                clock.sleep(Span::from_nanos(10_000_000)).await;
                reads
            })
            .await;
            (reads, control.accepts.get())
        })
        .await
    });
    out
}

/// The results of two reads of `VALUE`.
type Reads = (Result<Value, Fault>, Result<Value, Fault>);

/// One direction of a relay.
#[derive(Default)]
struct Pipe {
    /// The bytes read after the last whole message.
    read: Vec<u8>,
    /// The bytes not yet written.
    write: Vec<u8>,
}

impl Pipe {
    /// Moves the bytes that `from` has to `to`, gives each whole message to `rewrite`
    /// first, and gives whether `from` ended its side.
    fn pump(
        &mut self,
        cx: &mut Context<'_>,
        from: &mut Tcp,
        to: &mut Tcp,
        mut rewrite: impl FnMut(&mut Vec<u8>),
    ) -> bool {
        let mut ended = false;
        let mut buffer = [0; 1 << 12];
        while let Poll::Ready(read) = from.poll_read(cx, &mut buffer) {
            let read = read.expect("the read works");
            if read == 0 {
                ended = true;
                break;
            }
            self.read.extend_from_slice(&buffer[..read]);
        }
        while let Some(size) = self.read.get(4..8) {
            let size = u32::from_le_bytes(size.try_into().expect("4 bytes"));
            let size = usize::try_from(size).expect("a usize holds a u32");
            if self.read.len() < size {
                break;
            }
            let mut message = self.read.drain(..size).collect();
            rewrite(&mut message);
            self.write.extend(message);
        }
        while !self.write.is_empty() {
            let Poll::Ready(written) = to.poll_write(cx, &[IoSlice::new(&self.write)])
            else {
                break;
            };
            self.write.drain(..written.expect("the write works"));
        }
        ended
    }
}

/// Gives the request handle of `message` when it is a Read response.
fn handle(message: &[u8]) -> Option<u32> {
    is(message, READ_RESPONSE)
        .then(|| u32::from_le_bytes(message[36..40].try_into().expect("4 bytes")))
}

/// Makes the Read response `message`, with no diagnostics, string table, or header
/// extension, give `count` empty results.
fn recount_of(message: &mut Vec<u8>, count: i32) {
    assert_eq!(message[44..52], [0, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0]);
    let diagnostics = message.len() - 4;
    assert_eq!(message[diagnostics..], [0xFF; 4]);
    let results =
        count.to_le_bytes().into_iter().chain(vec![
            0;
            usize::try_from(count)
                .expect("a count of 0 or more")
        ]);
    message.splice(52..diagnostics, results);
    let size = u32::try_from(message.len()).expect("a message of a u32 of bytes");
    message[4..8].copy_from_slice(&size.to_le_bytes());
}

/// The value of each node of a read.
type Values = Vec<Result<Value, Fault>>;

/// The values of a read, or its error.
type Read = Result<Values, Status>;

/// Gives two reads of [`VALUE`] by one client through a relay that gives the Read
/// response of the first `recount` empty results when it is set, the request handle of
/// each Read response, the value of [`VALUE`], and the values that the client holds
/// after the first answer. The first Read response is that of the connect.
fn read_twice(recount: Option<i32>) -> ([Read; 2], Vec<u32>, Value, Values) {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            let mut handles = Vec::new();
            let rewrite = |message: &mut Vec<u8>| {
                if let Some(handle) = handle(message) {
                    if let (1, Some(count)) = (handles.len(), recount) {
                        recount_of(message, count);
                    }
                    handles.push(handle);
                }
            };
            let (reads, copied) = relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let nodes = fits(client.nodes(&[VALUE]));
                let first = client.read(&nodes).await.map(<[_]>::to_vec);
                let copied = client.calls.reads.values.take();
                client.calls.reads.values.set(copied.clone());
                let second = client.read(&nodes).await.map(<[_]>::to_vec);
                client.close().await;
                ([first, second], copied)
            })
            .await;
            (reads, handles, peer.value, copied)
        })
        .await
    })
    .0
}

/// Each read of one request has its own request handle on the wire.
#[test]
fn each_read_of_one_request_has_its_own_handle() {
    let (reads, handles, value, _) = read_twice(None);
    assert_eq!(reads, [Ok(vec![Ok(value)]), Ok(vec![Ok(value)])]);
    let [_, one, two] = handles[..] else {
        panic!("three Read responses, not {}", handles.len());
    };
    assert_ne!(one, two);
}

/// An answer with a value for more or fewer nodes than its read fails, copies none of
/// them, and the next read gives the value of each node. The test reads the buffer of
/// the client: no call gives it, so no caller sees a copied value.
#[test]
fn an_answer_of_another_count_of_values_fails() {
    for count in [0, 2] {
        let (reads, _, value, copied) = read_twice(Some(count));
        assert_eq!(
            (reads, copied),
            (
                [Err(Status::BAD_UNEXPECTED_ERROR), Ok(vec![Ok(value)])],
                vec![UNREAD]
            ),
            "{count} results"
        );
    }
}

/// Gives two reads by one client through a relay, on a server with a `MaxNodesPerRead`
/// of 1: of the nodes 1 and 2 of [`numbered`] in two requests, then of the nodes 3 and
/// 4.
/// `rewrite` gets the index of each Read response after that of the connect, from 1,
/// and the response, and gives the responses to send in its place. The relay gives
/// them the next sequence numbers of the channel, which open62541 checks.
fn read_split(
    mut rewrite: impl FnMut(usize, Vec<u8>) -> Vec<Vec<u8>> + Send + 'static,
) -> [Read; 2] {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            peer.server.set_per_read(1);
            for index in 1..=4 {
                peer.server.add_scalar(&numbered(index), Value::U32(index));
            }
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            let (mut responses, mut sequence) = (0, None);
            let rewrite = |message: &mut Vec<u8>| {
                if handle(message).is_none() {
                    return;
                }
                responses += 1;
                if responses == 1 {
                    return;
                }
                let first = u32::from_le_bytes(message[16..20].try_into().expect("4"));
                let next: &mut u32 = sequence.get_or_insert(first);
                for mut response in rewrite(responses - 1, mem::take(message)) {
                    response[16..20].copy_from_slice(&next.to_le_bytes());
                    *next += 1;
                    message.append(&mut response);
                }
            };
            relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let first = fits(client.nodes(&[numbered(1), numbered(2)]));
                let second = fits(client.nodes(&[numbered(3), numbered(4)]));
                let first = client.read(&first).await.map(<[_]>::to_vec);
                let second = client.read(&second).await.map(<[_]>::to_vec);
                client.close().await;
                [first, second]
            })
            .await
        })
        .await
    })
    .0
}

/// The answers of the requests of a read in another order give the values in the
/// order of the nodes.
#[test]
fn a_read_gives_its_values_in_node_order_whatever_the_order_of_its_answers() {
    let mut held = Vec::new();
    let reads = read_split(move |index, message| match index {
        1 => {
            held = message;
            vec![]
        }
        2 => vec![message, mem::take(&mut held)],
        _ => vec![message],
    });
    let value = |index| Ok(Value::U32(index));
    assert_eq!(
        reads,
        [Ok(vec![value(1), value(2)]), Ok(vec![value(3), value(4)])]
    );
}

/// An answer of a request of a read that failed does not go into the next read: it
/// comes after the answer of the node 3, and would put the value 1 in its place.
#[test]
fn an_answer_of_a_failed_read_goes_into_no_later_read() {
    let mut held = Vec::new();
    let reads = read_split(move |index, mut message| match index {
        1 => {
            held = message;
            vec![]
        }
        2 => {
            recount_of(&mut message, 0);
            vec![message]
        }
        3 => vec![message, mem::take(&mut held)],
        _ => vec![message],
    });
    assert_eq!(
        reads,
        [
            Err(Status::BAD_UNEXPECTED_ERROR),
            Ok(vec![Ok(Value::U32(3)), Ok(Value::U32(4))])
        ]
    );
}

/// A read with two failed answers in one message ends with the status of the first:
/// the answer of the node 2 has no values, then that of the node 1 has a bad service
/// result.
#[test]
fn a_read_with_two_failed_answers_ends_with_the_status_of_the_first() {
    let mut held = Vec::new();
    let reads = read_split(move |index, mut message| match index {
        1 => {
            message[40..44]
                .copy_from_slice(&Status::BAD_INTERNAL_ERROR.0.to_le_bytes());
            held = message;
            vec![]
        }
        2 => {
            recount_of(&mut message, 0);
            vec![message, mem::take(&mut held)]
        }
        _ => vec![message],
    });
    assert_eq!(
        reads,
        [
            Err(Status::BAD_UNEXPECTED_ERROR),
            Ok(vec![Ok(Value::U32(3)), Ok(Value::U32(4))])
        ]
    );
}

/// A read sends no request after an answer of it fails. With a `MaxNodesPerRead` of 1,
/// the first answer fails while [`READS`] requests are in flight, and the server gets
/// no more: it answers the connect and those.
#[test]
fn a_read_sends_no_request_after_an_answer_of_it_fails() {
    let ((read, responses), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            peer.server.set_per_read(1);
            let nodes: Vec<_> = (0..u32::from(READS) + 4).map(numbered).collect();
            for (index, node) in (0..).zip(&nodes) {
                peer.server.add_scalar(node, Value::U32(index));
            }
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            let mut responses = 0;
            let rewrite = |message: &mut Vec<u8>| {
                if handle(message).is_some() {
                    responses += 1;
                    if responses == 2 {
                        recount_of(message, 0);
                    }
                }
            };
            let read = relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let nodes = fits(client.nodes(&nodes));
                let read = client.read(&nodes).await.map(<[_]>::to_vec);
                client.close().await;
                read
            })
            .await;
            (read, responses)
        })
        .await
    });
    assert_eq!(
        (read, responses),
        (Err(Status::BAD_UNEXPECTED_ERROR), 1 + usize::from(READS))
    );
}

/// A connect whose read of the `MaxNodesPerRead` of the server fails gives the status
/// of the read, and closes its session with one `CloseSession`: a server with room
/// for one session takes the next client.
#[test]
fn a_connect_whose_read_of_the_max_nodes_per_read_fails_closes_its_session() {
    let (connect, _) = run(0, async |node| {
        serve(&node, 1, async |mut peer| {
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            let rewrite = |message: &mut Vec<u8>| {
                if handle(message).is_some() {
                    recount_of(message, 0);
                }
            };
            let control = Relay::default();
            let connect = relayed(&node, &control, rewrite, peer.connect(&url)).await;
            peer.connected().await.close().await;
            (connect.err(), control.closes.get())
        })
        .await
    });
    assert_eq!(connect, (Some(Status::BAD_UNEXPECTED_ERROR), 1));
}

/// A read of the time whose answer holds no result fails with `BadUnexpectedError`.
#[test]
fn a_time_whose_answer_has_no_result_fails_with_unexpected_error() {
    let (time, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            let connected = Cell::new(false);
            let rewrite = |message: &mut Vec<u8>| {
                if connected.get() && handle(message).is_some() {
                    recount_of(message, 0);
                }
            };
            relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                connected.set(true);
                let time = client.time().await;
                client.close().await;
                time
            })
            .await
        })
        .await
    });
    assert_eq!(time, Err(Status::BAD_UNEXPECTED_ERROR));
}

mod abort;
mod browse;
mod chunks;
pub(crate) mod common;
mod flood;
mod secure;
mod walk;
