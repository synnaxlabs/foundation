//! Runs a test server of open62541 with clients that each have their own connection
//! manager, as the kind of OPC UA uses them.

use std::cell::RefCell;
use std::ffi::CString;
use std::future;
use std::net::{IpAddr, SocketAddr};
use std::panic;

use env::clock::Clock;
use env::net::Net;
use env::rng::Rng;
use types::quality::Quality;
use types::sample::Scalar;
use types::time::Span;

use super::PORT;
pub use crate::client::Mode;
use crate::client::browse::Base;
use crate::client::{Credentials, Nodes};
use crate::connection;
use crate::event;
use crate::node::{Key, Name, Node};
use crate::secure::{CLIENT, Policy, SERVER, START};
use crate::server::Secure;
use crate::unwind::caught;
use crate::value::Value;

/// The Double variable of a [`Server`].
const VALUE: Node = Node {
    namespace: 1,
    key: Key::Number(1000),
};
/// A `ByteString` of 7 chunks of 64 KiB, an answer of 8 chunks.
const BYTES: Node = Node {
    namespace: 1,
    key: Key::Number(1002),
};
/// The number of the key of the first counter of [`Server::counting`].
const COUNTERS: u32 = 10_000;
/// The sampling interval of [`Client::subscribe`]: the shortest publishing interval of
/// a client, so each counter changes once in each Publish answer.
const SAMPLING: Span = Span::from_nanos(100_000_000);
/// The name of the Double variable of a [`Server`] that has a name key.
const NAME: &str = "SpikeData";
/// The value of each variable.
const DOUBLE: f64 = 1.5;
/// The credentials of the server of a [`Server::secure`], which trusts [`CLIENT`].
const SERVED: Credentials<'_> = Credentials {
    certificate: SERVER.certificate,
    key: SERVER.key,
    trust: CLIENT.certificate,
};
/// The credentials of a client of a [`Server::secure`], which trusts [`SERVER`].
const CLIENT_SIDE: Credentials<'_> = Credentials {
    certificate: CLIENT.certificate,
    key: CLIENT.key,
    trust: SERVER.certificate,
};
/// The sessions of a [`Server`].
const SESSIONS: u16 = 4;
/// The `MaxNodesPerRead` of a [`Server`]: a read of more nodes takes more requests.
pub const PER_READ: u32 = 256;

/// A variable of a [`Server`].
#[derive(Clone, Copy, Debug)]
pub enum Variable {
    /// `ns=1;i=1000`.
    Number,
    /// `ns=1;s=SpikeData`.
    Name,
    /// `ns=1;i=1002`, a `ByteString` whose answer takes 8 chunks.
    Bytes,
}

/// A test server of open62541 on its own connection manager over `env::net`, with a
/// variable of each [`Variable`] and a `MaxNodesPerRead` of [`PER_READ`], for
/// the clients of [`Server::connect`].
pub struct Server {
    clock: Clock,
    net: Net,
    url: CString,
    /// Gives the next value of each client. The loop of a client sets the random
    /// numbers of open62541 on its thread, which the server also draws from.
    rng: RefCell<Rng>,
    /// The mode of each channel, or `None` for the policy `None`.
    mode: Option<Mode>,
}

/// What a [`Server`] serves.
struct Served {
    /// The value of each variable other than the counters.
    double: f64,
    /// The count of the counters of [`Server::counting`].
    counters: usize,
    /// The mode of each channel of [`Server::secure`].
    mode: Option<Mode>,
}

impl Served {
    const PLAIN: Self = Self {
        double: DOUBLE,
        counters: 0,
        mode: None,
    };
}

impl Server {
    /// Starts the server on `clock`, `net`, and `rng`, with its listener on `address`
    /// and port 4840 and room for 4 sessions, runs `body` while the server runs, then
    /// closes the server, also after a panic in `body`, which it then resumes. Each
    /// client of [`Server::connect`] also draws from `rng`.
    ///
    /// # Panics
    ///
    /// If the port is taken, if the accept of its listener fails, or if open62541
    /// refuses the server, a Double, a run of its loop, or a step of its close. A panic
    /// of its start, a run of its loop, or its close leaks the manager and the server
    /// that it made by then, and a panic of the close replaces a panic of a Double or
    /// of `body`.
    ///
    /// # Aborts
    ///
    /// When open62541 cannot allocate the loop or the connection manager: the process
    /// stops as the Rust allocator does when it has no memory.
    pub async fn scope<T>(
        clock: Clock,
        net: Net,
        address: IpAddr,
        rng: Rng,
        body: impl AsyncFnOnce(&Self) -> T,
    ) -> T {
        Self::serve(clock, net, address, rng, Served::PLAIN, body).await
    }

    /// As [`Server::scope`], with channels only in `mode` under `Basic256Sha256`, on
    /// test certificates of the crate. The server and each client of
    /// [`Server::connect`] read a wall time from 2001-01-01, in the window of the
    /// certificates.
    ///
    /// # Panics
    ///
    /// As [`Server::scope`].
    pub async fn secure<T>(
        clock: Clock,
        net: Net,
        address: IpAddr,
        rng: Rng,
        mode: Mode,
        body: impl AsyncFnOnce(&Self) -> T,
    ) -> T {
        let served = Served {
            mode: Some(mode),
            ..Served::PLAIN
        };
        Self::serve(clock, net, address, rng, served, body).await
    }

    /// As [`Server::scope`], with `counters` more Double variables for
    /// [`Client::subscribe`], each of which adds 1 to its value at each sample. The
    /// server grants each count of changes in a Publish answer that a client asks for.
    ///
    /// # Panics
    ///
    /// As [`Server::scope`], and if `counters` is past `u32::MAX - COUNTERS`.
    pub async fn counting<T>(
        clock: Clock,
        net: Net,
        address: IpAddr,
        rng: Rng,
        counters: usize,
        body: impl AsyncFnOnce(&Self) -> T,
    ) -> T {
        let served = Served {
            counters,
            ..Served::PLAIN
        };
        Self::serve(clock, net, address, rng, served, body).await
    }

    /// Serves `served` as [`Server::scope`] and its siblings do.
    async fn serve<T>(
        clock: Clock,
        net: Net,
        address: IpAddr,
        mut rng: Rng,
        served: Served,
        body: impl AsyncFnOnce(&Self) -> T,
    ) -> T {
        let Served {
            double,
            counters,
            mode,
        } = served;
        let local = SocketAddr::new(address, PORT);
        let manager = connection::Manager::listening(
            time(&clock, mode),
            Net::clone(&net),
            local,
            u32::from(SESSIONS),
            &mut rng,
        )
        .expect("the port is free");
        let this = Self {
            clock,
            net,
            url: CString::new(format!("opc.tcp://{local}")).expect("no NUL"),
            rng: RefCell::new(rng),
            mode,
        };
        let add = |server: &mut crate::server::Server| {
            server.add_scalar(&VALUE, Value::F64(double));
            server.add_scalar(&named(), Value::F64(double));
            server.set_per_read(PER_READ);
            server.add_bytes(&BYTES, 7 << 16);
            server.set_per_publish(u32::MAX);
            for index in 0..counters {
                let mut value = 0.0;
                server.add_source(&counter(index), move || {
                    value += 1.0;
                    (Value::F64(value), Quality::GOOD, None)
                });
            }
        };
        let body = caught(async { body(&this).await });
        let modes = mode.map(|mode| [mode]);
        let secure = modes.as_ref().map(|modes| Secure {
            credentials: &SERVED,
            policies: &[Policy::Basic256Sha256],
            modes,
        });
        let serve = crate::server::Server::serve;
        let value = serve(manager, SESSIONS, secure.as_ref(), add, body).await;
        let value = value.expect("the listener of a bench does not fail");
        value.unwrap_or_else(|panic| panic::resume_unwind(panic))
    }

    /// Connects a client to the server, on its own connection manager, whose reads
    /// each read `count` copies of a [`Variable`], in one request while they number at
    /// most [`PER_READ`] and fit one message of the session, in the mode of a
    /// [`Server::secure`].
    ///
    /// # Panics
    ///
    /// If the connect fails, with its status.
    ///
    /// # Aborts
    ///
    /// When open62541 cannot allocate the loop, the connection manager, the client, or
    /// a Read request: the process stops as the Rust allocator does when it has no
    /// memory.
    pub async fn connect(&self, count: usize) -> Client {
        let rng = &mut Rng::from_seed(self.rng.borrow_mut().next_u64());
        let time = time(&self.clock, self.mode);
        let net = Net::clone(&self.net);
        let connect = crate::client::Client::connect(
            time,
            tracing::Span::none(),
            net,
            rng,
            &self.url,
            self.mode.map(|mode| (&CLIENT_SIDE, mode)),
            future::pending(),
        );
        let session = match connect.await {
            Ok(session) => session.expect("a pending wait never ends"),
            Err(crate::client::Error::Failed(status)) => {
                panic!("a connect failed: {status:?}")
            }
            Err(crate::client::Error::Refused(refusal)) => {
                panic!("invariant: the test credentials are valid: {refusal}")
            }
        };
        let nodes = |node: Node| {
            let nodes: Vec<Node> = std::iter::repeat_n(node, count).collect();
            let nodes = session.nodes(&nodes);
            assert!(nodes.large().is_empty(), "one copy fits one message");
            nodes
        };
        let (number, name) = (nodes(VALUE), nodes(named()));
        let bytes = session.nodes(&[BYTES]);
        Client {
            session,
            number,
            name,
            bytes,
        }
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// The time of a loop of a [`Server`] of `mode`: a wall time from [`START`], in the
/// window of the certificates, when it is secure.
fn time(clock: &Clock, mode: Option<Mode>) -> event::Time {
    let clock = Clock::clone(clock);
    match mode {
        Some(_) => event::Time::since(clock, START),
        None => event::Time::monotonic(clock),
    }
}

/// The node of counter `index` of [`Server::counting`].
fn counter(index: usize) -> Node {
    let index = u32::try_from(index).ok();
    let key = index.and_then(|index| index.checked_add(COUNTERS));
    Node {
        namespace: 1,
        key: Key::Number(key.expect("a counter has a key of a u32")),
    }
}

/// The node of [`Variable::Name`].
fn named() -> Node {
    Node {
        namespace: 1,
        key: Key::Name(Name::new(NAME).expect("SpikeData is a name")),
    }
}

/// A client of a [`Server`].
pub struct Client {
    session: crate::client::Client,
    number: Nodes,
    name: Nodes,
    bytes: Nodes,
}

impl Client {
    /// Reads the Double of each copy of `variable`, or the `ByteString` of
    /// [`Variable::Bytes`], which gives the fault of a value that is not a sample.
    ///
    /// # Panics
    ///
    /// If the read fails, with its status, or gives another value.
    pub async fn read(&mut self, variable: Variable) {
        let nodes = match variable {
            Variable::Number => &self.number,
            Variable::Name => &self.name,
            Variable::Bytes => {
                let values = self.session.read(&self.bytes).await;
                assert!(matches!(values, Ok([Err(_)])), "read {values:?}");
                return;
            }
        };
        let values = match self.session.read(nodes).await {
            Ok(values) => values,
            Err(status) => panic!("a read failed: {status:?}"),
        };
        for value in values {
            match value {
                Ok(Value::F64(value)) => {
                    assert!(value.to_bits() == DOUBLE.to_bits(), "read {value}");
                }
                Ok(value) => panic!("read {value:?}"),
                Err(status) => panic!("a read failed: {status:?}"),
            }
        }
    }

    /// Describes the variable of [`Variable::Number`], and gives the sample type of
    /// its base type, or `None` when its base is not a type from Boolean to Double.
    ///
    /// # Panics
    ///
    /// If the describe fails, with its status.
    pub async fn describe(&mut self) -> Option<Scalar> {
        match self.session.describe(&[VALUE]).await {
            Ok(descriptions) => match descriptions.first().and_then(|found| found.base)
            {
                Some(Base::Scalar(scalar)) => Some(scalar),
                _ => None,
            },
            Err(status) => panic!("a describe failed: {status:?}"),
        }
    }

    /// Subscribes to the first `count` counters of [`Server::counting`], each sampled
    /// at the publishing interval of 100 ms, so each changes once in each Publish
    /// answer.
    ///
    /// # Panics
    ///
    /// If the subscription or an item fails, with its status, or on a second call.
    pub async fn subscribe(&mut self, count: usize) {
        let nodes: Vec<Node> = (0..count).map(counter).collect();
        match self.session.subscribe(&nodes, SAMPLING, Span::ZERO).await {
            Ok(revised) => {
                for item in revised.items {
                    item.unwrap_or_else(|status| panic!("an item failed: {status:?}"));
                }
            }
            Err(status) => panic!("a subscribe failed: {status:?}"),
        }
    }

    /// Drives the client until the changes of a Publish answer come, or the changes of
    /// each answer that it took while it was not driven, and gives their count.
    ///
    /// # Panics
    ///
    /// If the subscription ends, with its status, or a change is not the Double of a
    /// counter, which is at least 1.
    pub async fn changes(&mut self) -> usize {
        match self.session.changes(future::pending()).await {
            Ok(changes) => {
                for change in changes {
                    let counted =
                        matches!(change.value, Ok(Value::F64(value)) if value >= 1.0);
                    assert!(counted, "{change:?}");
                }
                changes.len()
            }
            Err(status) => panic!("the subscription ended: {status:?}"),
        }
    }

    /// Closes the session and the connection of the client. A client that drops with no
    /// close holds one of the 4 sessions of its server until the session times out.
    pub async fn close(self) {
        self.session.close().await;
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::num::NonZeroU64;

    use env::rng::Rng;
    use sim::node::Node;
    use sim::{Sim, node};
    use types::time::Span;

    use super::{Mode, PORT, Server, Variable};
    use crate::peer::session;
    use crate::unwind::caught;

    /// Runs `body` on a node of a sim, and gives the error of the run as text.
    fn run<T: Send + 'static>(
        body: impl AsyncFnOnce(Node) -> T + Send + 'static,
    ) -> Result<T, String> {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, _| body(node))
            .map_err(|error| error.to_string())
    }

    /// Runs `body` in the scope of a server on `node`, with a new `Rng` from the
    /// entropy of `node`.
    async fn scope<T>(node: &Node, body: impl AsyncFnOnce(&Server) -> T) -> T {
        let (clock, net, address) = (node.clock(), node.net(), node.addresses()[0]);
        Server::scope(clock, net, address, node.entropy().rng(), body).await
    }

    /// The error of a run of seed 0 whose task panics with `message`.
    fn panicked(message: &str) -> Result<(), String> {
        Err(format!(
            "thread run_on panicked: {message}; replay with seed 0x0"
        ))
    }

    /// `count` minutes.
    fn minutes(count: i64) -> Span {
        Span::from_nanos(count * 60 * 1_000_000_000)
    }

    /// Connects a client, reads, and closes it.
    async fn read(server: &Server) {
        let mut client = server.connect(1).await;
        client.read(Variable::Number).await;
        client.close().await;
    }

    /// Runs a scope whose `body` panics with "the body panics", which the scope
    /// resumes, then a scope at its address that connects, reads, and closes.
    fn frees_its_port(body: impl AsyncFnOnce(&Server) + Send + 'static) {
        let result = run(async |node| {
            let panic = caught(scope(&node, body)).await.expect_err("panics");
            assert_eq!(panic.downcast_ref(), Some(&"the body panics"));
            scope(&node, read).await;
        });
        assert_eq!(result, Ok(()));
    }

    /// A scope closes after a panic in its body, also with a client that drops with the
    /// body before the close, so the next scope takes its port.
    #[test]
    fn a_scope_whose_body_panics_frees_its_port() {
        frees_its_port(async |server: &Server| {
            let _client = server.connect(1).await;
            panic!("the body panics");
        });
    }

    /// A body that panics when it is called, before it gives its future.
    #[test]
    fn a_scope_whose_body_panics_at_its_call_frees_its_port() {
        frees_its_port(|_: &Server| -> std::future::Ready<()> {
            panic!("the body panics");
        });
    }

    /// The server has room for 4 sessions.
    #[test]
    fn a_fifth_client_of_a_scope_panics_with_its_status() {
        let result = run(async |node| {
            scope(&node, async |server: &Server| {
                let mut clients = Vec::new();
                for _ in 0..5 {
                    clients.push(server.connect(1).await);
                }
                for client in clients {
                    client.close().await;
                }
            })
            .await;
        });
        assert_eq!(result, panicked("a connect failed: BadConnectionClosed"));
    }

    /// A client that drops with no close holds its session until the session times out,
    /// after 20 minutes.
    #[test]
    fn a_dropped_client_holds_its_session_until_it_times_out() {
        let fifth = |wait: Span| {
            run(async move |node| {
                let clock = node.clock();
                scope(&node, async |server: &Server| {
                    for _ in 0..4 {
                        drop(server.connect(1).await);
                    }
                    clock.sleep(wait).await;
                    server.connect(1).await.close().await;
                })
                .await;
            })
        };
        let held = panicked("a connect failed: BadTooManySessions");
        assert_eq!(fifth(minutes(19)), held);
        assert_eq!(fifth(minutes(21)), Ok(()));
    }

    /// The answer of [`Variable::Bytes`] carries 7 chunks of 64 KiB and less than 8,
    /// so its read takes that much time on a link of 1 MB/s.
    #[test]
    fn a_read_of_the_bytes_carries_seven_chunks_of_64_kib() {
        let link = sim::link::Config {
            rate: NonZeroU64::new(1_000_000),
            ..sim::link::Config::default()
        };
        let mut sim = Sim::new(sim::Config {
            link,
            ..sim::Config::default()
        });
        let node = sim.node(node::Config::default());
        let took = sim
            .run_on(&node, move |node, _| async move {
                let clock = node.clock();
                scope(&node, async |server: &Server| {
                    let mut client = server.connect(1).await;
                    let start = clock.now();
                    client.read(Variable::Bytes).await;
                    let took = clock.now() - start;
                    client.close().await;
                    took
                })
                .await
            })
            .expect("the run ends");
        let at_rate = |chunks: i64| Span::from_nanos(chunks * 65_536 * 1_000);
        assert!(at_rate(7) <= took && took < at_rate(8), "took {took:?}");
    }

    /// The server closes a channel idle for 10 minutes.
    #[test]
    fn a_read_of_a_client_that_lost_its_channel_panics_with_its_status() {
        let result = run(async |node| {
            let clock = node.clock();
            scope(&node, async |server: &Server| {
                let mut client = server.connect(1).await;
                client.read(Variable::Number).await;
                clock.sleep(minutes(11)).await;
                client.read(Variable::Number).await;
                client.close().await;
            })
            .await;
        });
        assert_eq!(result, panicked("a read failed: BadSecureChannelClosed"));
    }

    #[test]
    fn a_second_scope_at_the_address_of_a_scope_panics() {
        let result = run(async |node| {
            scope(&node, async |_: &Server| {
                scope(&node, async |_: &Server| ()).await;
            })
            .await;
        });
        let free = "the port is free: AddressInUse { local: 10.0.0.1:4840 }";
        assert_eq!(result, panicked(free));
    }

    /// No public call makes a server serve another value, so this test serves 2.5
    /// through the private `Server::serve`. While a server serves `DOUBLE`, no caller
    /// of `Client::read` can see the removal of its check of the value, so only this
    /// test fails then.
    #[test]
    fn a_read_of_another_value_panics_with_it() {
        let result = run(async |node| {
            let (clock, net, address) = (node.clock(), node.net(), node.addresses()[0]);
            let served = super::Served {
                double: 2.5,
                ..super::Served::PLAIN
            };
            Server::serve(clock, net, address, node.entropy().rng(), served, read)
                .await;
        });
        assert_eq!(result, panicked("read 2.5"));
    }

    /// The server offers one endpoint, in its mode, so a client in another mode or
    /// with no security does not connect.
    #[test]
    fn a_client_reads_over_each_mode_of_a_secure_server() {
        for mode in [Mode::Sign, Mode::SignAndEncrypt] {
            let result = run(async move |node| {
                let (clock, net, address) =
                    (node.clock(), node.net(), node.addresses()[0]);
                let rng = node.entropy().rng();
                Server::secure(clock, net, address, rng, mode, read).await;
            });
            assert_eq!(result, Ok(()), "{mode:?}");
        }
    }

    #[test]
    fn a_client_reads_each_double_of_its_server() {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, _| async move {
            let address = node.addresses()[0];
            let body = async |server: &Server| {
                let mut client = server.connect(3).await;
                client.read(Variable::Number).await;
                client.read(Variable::Name).await;
                let debug = format!("{server:?} {client:?}");
                client.close().await;
                debug
            };
            let rng = node.entropy().rng();
            let debug =
                Server::scope(node.clock(), node.net(), address, rng, body).await;
            let url = format!("opc.tcp://{address}:4840");
            assert_eq!(
                debug,
                format!(r#"Server {{ url: "{url}", .. }} Client {{ .. }}"#)
            );
        })
        .expect("the run ends");
    }

    /// Gives the ids of a session of a peer in a scope of each start value: before
    /// the first connect, and after each of two connects. The server draws them from
    /// the random numbers that the last loop on this thread set.
    fn sessions(starts: [u64; 2]) -> Vec<[[[u8; 16]; 2]; 3]> {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, _| async move {
            let address = node.addresses()[0];
            let (net, remote) = (node.net(), SocketAddr::new(address, PORT));
            let mut ids = Vec::new();
            for start in starts {
                let body = async |server: &Server| {
                    let mut ids = [session(&net, remote).await; 3];
                    for id in &mut ids[1..] {
                        let client = server.connect(1).await;
                        *id = session(&net, remote).await;
                        client.close().await;
                    }
                    ids
                };
                let rng = Rng::from_seed(start);
                ids.push(
                    Server::scope(node.clock(), node.net(), address, rng, body).await,
                );
            }
            ids
        })
        .expect("the run ends")
    }

    #[test]
    fn each_scope_and_each_connect_take_another_value() {
        let ids = sessions([1, 2]);
        assert_ne!(ids[0][0], ids[1][0], "two scopes took the same value");
        assert_ne!(ids[0][1], ids[0][2], "two connects took the same value");
        let message = "the clients of two scopes took the same value";
        assert_ne!(ids[0][1], ids[1][1], "{message}");
    }

    #[test]
    fn scopes_of_one_start_value_give_a_peer_the_same_sessions() {
        let ids = sessions([1, 1]);
        assert_eq!(ids[0], ids[1], "a start value gave other sessions");
    }
}
