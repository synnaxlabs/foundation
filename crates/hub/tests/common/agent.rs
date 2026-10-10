//! Both ends of a hub link over a real transport between two sim nodes: a program that
//! dials as an agent, and a home that serves each stream on `Link::serve`.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use hub::{Hub, Served, serve};
use spec::definition::{Definition, Kind};
use spec::subject::Subject;
use transport::stream::{Incoming, Receiver, Sender};
use transport::{Address, Class, Code, Session};
use types::connection;
use types::ed25519::{Pair, PrivateKey};
use types::hello::Hello;
use types::name::{Name, Prefix};
use types::time::Span;
use wire::Protocol;
use wire::hub::client::{Challenge, Request, Response, Signed};

use crate::net::{HOME, PORT, own_pool, public_key, transport};

/// The node key of the hub under test, which each hello names as its `via`.
pub(crate) const NODE: types::node::Key = types::node::Key::from_u128(1);
pub(crate) const SUBJECT: &str = "ops.agent";
/// Two more subjects that the spec lists with `AGENT`.
pub(crate) const SECOND: &str = "ops.second";
pub(crate) const THIRD: &str = "ops.third";
/// The key that the spec lists for [`SUBJECT`].
pub(crate) const AGENT: PrivateKey = PrivateKey([3; 32]);
pub(crate) const CONNECTION: connection::Key = connection::Key([7; 16]);
/// How long each hello lives, unless a test says otherwise.
pub(crate) const LIFE: Span = Span::from_nanos(60 * Span::SECOND.nanos());
/// How long the home holds a request before it replies.
pub(crate) const HOLD: Span = Span::from_nanos(50_000_000);

pub(crate) fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
}

/// The rules of a root tree that lists `AGENT` for [`SUBJECT`], [`SECOND`], and
/// [`THIRD`].
pub(crate) fn rules() -> access::Rules {
    let tree: BTreeMap<Name, Definition> = [SUBJECT, SECOND, THIRD]
        .into_iter()
        .map(|subject| {
            let key = Kind::Subject.key(subject).expect("a subject key");
            let listed = Subject::new(vec![public_key(&AGENT)]).expect("a subject");
            (key, Definition::Subject(listed))
        })
        .collect();
    access::Rules::new([(Prefix::ROOT, &tree)])
}

/// What the home's link gave for one stream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Got {
    Ended,
    /// A request, with the subject of its hello and its body.
    Request(Name, Vec<u8>),
}

/// Runs a client session of the hub that `open` gives on the home's node, for each of
/// `subjects`, from its own port of the program's node. The home keeps what `open`
/// gives with the hub until each session ends. Each stream is served as
/// [`serve_each`] serves it, and each agent admits a hello of its subject. Gives what
/// `serve` gave for each stream, in the order they ended.
pub(crate) fn sessions<const N: usize, T, P>(
    seed: u64,
    subjects: [&'static str; N],
    open: impl AsyncFnOnce(&sim::node::Node, &env::tasks::Tasks) -> (Hub, T)
    + Send
    + 'static,
    program: impl FnOnce([Agent; N]) -> P + Send + 'static,
) -> Vec<Result<Got, serve::Error>>
where
    P: Future<Output = ()> + 'static,
{
    let served = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&served);
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let (hub, kept_open) = open(&node, &tasks).await;
        hub.set_rules(rules());
        let transport = transport(&node, &tasks, &own_pool(), HOME, 1 << 16);
        let accepted = Rc::new(Cell::new(0));
        let mut ends = Vec::new();
        for _ in 0..N {
            let session = transport.accept().await.expect("a session");
            let link = hub.link(session.clone());
            tasks.spawn(serve_each(
                &session,
                &link,
                &tasks,
                &node.clock(),
                &kept,
                &accepted,
            ));
            ends.push(session);
        }
        for session in ends {
            session.closed().await;
        }
        recorded(&node, &kept, accepted.get()).await;
        drop((hub, kept_open));
    };
    run_program(seed, home, move |node, tasks, at| async move {
        let mut agents = Vec::new();
        for subject in subjects {
            agents.push(Agent::dial(&node, tasks.clone(), at, name(subject)).await);
        }
        let ends: Vec<_> = agents.iter().map(|agent| agent.session.clone()).collect();
        program(agents.try_into().ok().expect("an agent for each session")).await;
        for session in ends {
            session.close(Code(0));
        }
        node.clock().sleep(Span::MILLISECOND).await;
    });
    std::mem::take(&mut *served.lock().expect("not poisoned"))
}

/// `asserted`, then the close of each stream still open when [`sessions`] closed its
/// `count` sessions: `stalled` requests and the hello stream of each session.
pub(crate) fn closed_after(
    mut asserted: Vec<Result<Got, serve::Error>>,
    count: usize,
    stalled: usize,
) -> Vec<Result<Got, serve::Error>> {
    let open = count + stalled;
    asserted.extend((0..open).map(|_| Err(serve::Error::Stream(closed_with(0)))));
    asserted
}

/// What `serve` gave for each stream, in the order they ended.
pub(crate) type Kept = Arc<Mutex<Vec<Result<Got, serve::Error>>>>;

/// Serves each stream of `session` on `link` in its own task through [`answer`], and
/// pushes what it gives to `kept`. Counts each stream in `accepted`. Ends when the
/// session ends.
pub(crate) fn serve_each(
    session: &Session,
    link: &hub::Link,
    tasks: &env::tasks::Tasks,
    clock: &env::clock::Clock,
    kept: &Kept,
    accepted: &Rc<Cell<usize>>,
) -> impl Future<Output = ()> + 'static {
    let (session, link, tasks, clock, kept, accepted) = (
        session.clone(),
        link.clone(),
        tasks.clone(),
        clock.clone(),
        Arc::clone(kept),
        Rc::clone(accepted),
    );
    async move {
        while let Ok(mut incoming) = session.accept().await {
            accepted.set(accepted.get() + 1);
            let (link, kept, clock) = (link.clone(), Arc::clone(&kept), clock.clone());
            tasks.spawn(async move {
                header(&mut incoming).await;
                let got = answer(link.serve(incoming), &clock).await;
                kept.lock().expect("not poisoned").push(got);
            });
        }
    }
}

/// Waits until `kept` holds what `serve` gave for each of `accepted` streams. The end
/// of the home's future drops each serve task that has not yet recorded.
///
/// # Panics
///
/// When a second passes first.
pub(crate) async fn recorded(node: &sim::node::Node, kept: &Kept, accepted: usize) {
    let deadline = node.clock().now() + Span::SECOND;
    loop {
        let recorded = kept.lock().expect("not poisoned").len();
        if recorded == accepted {
            break;
        }
        let now = node.clock().now();
        assert!(
            now < deadline,
            "{recorded} of {accepted} serve tasks recorded"
        );
        node.clock().sleep(Span::MICROSECOND).await;
    }
}

/// Sets `rules` on `hub` unless `None`, and gives the session of the first program
/// that dials the node, with its link on `hub`.
pub(crate) async fn accept_on(
    hub: &hub::Hub,
    node: &sim::node::Node,
    tasks: &env::tasks::Tasks,
    rules: Option<access::Rules>,
) -> (Session, hub::Link) {
    if let Some(rules) = rules {
        hub.set_rules(rules);
    }
    let transport = transport(node, tasks, &own_pool(), HOME, 1 << 16);
    let session = transport.accept().await.expect("a session");
    let link = hub.link(session.clone());
    (session, link)
}

/// Runs `home` on one simulated node, and `program` on another, with the home's
/// address.
pub(crate) fn run_program<F, H, G, P>(seed: u64, home: F, program: G)
where
    F: FnOnce(sim::node::Node, env::tasks::Tasks) -> H + Send + 'static,
    H: Future<Output = ()> + 'static,
    G: FnOnce(sim::node::Node, env::tasks::Tasks, Address) -> P + Send + 'static,
    P: Future<Output = ()> + 'static,
{
    run_program_on(seed, sim::link::Config::default(), home, program);
}

pub(crate) fn run_program_on<F, H, G, P>(
    seed: u64,
    wire: sim::link::Config,
    home: F,
    program: G,
) where
    F: FnOnce(sim::node::Node, env::tasks::Tasks) -> H + Send + 'static,
    H: Future<Output = ()> + 'static,
    G: FnOnce(sim::node::Node, env::tasks::Tasks, Address) -> P + Send + 'static,
    P: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        link: wire,
        ..sim::Config::default()
    });
    let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
    let at = Address::Udp(SocketAddr::new(nodes[0].addresses()[0], PORT));
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let node = nodes[0].clone();
    let main = move |tasks: env::tasks::Tasks| home(node, tasks);
    drop(
        nodes[0]
            .shards()
            .start(shard("home"), main)
            .expect("starts"),
    );
    let node = nodes[1].clone();
    let main = move |tasks: env::tasks::Tasks| program(node, tasks, at);
    drop(
        nodes[1]
            .shards()
            .start(shard("program"), main)
            .expect("starts"),
    );
    sim.run().expect("the run ends");
}

/// Reads the header of `incoming`, and checks that it names the hub.
pub(crate) async fn header(incoming: &mut Incoming) {
    let header = incoming.receiver.recv().await.expect("a header");
    let header = header.expect("the header comes before the finish");
    assert_eq!(
        wire::header::decode(&header),
        Ok((Protocol::Hub, &[][..])),
        "the stream is of the hub"
    );
}

/// What `serve` gave for one stream, once it replied to a request with its body
/// reversed after [`HOLD`], or the error of that reply.
pub(crate) async fn answer(
    serve: impl Future<Output = Result<Served, serve::Error>>,
    clock: &env::clock::Clock,
) -> Result<Got, serve::Error> {
    match serve.await? {
        Served::Ended => Ok(Got::Ended),
        Served::Request(request) => {
            let subject = request.admitted.hello.subject.clone();
            let reply: Vec<u8> = request.body.iter().rev().copied().collect();
            clock.sleep(HOLD).await;
            request.reply.send(&reply).await?;
            Ok(Got::Request(subject, request.body))
        }
    }
}

/// The program's end of one stream.
pub(crate) struct Stream {
    pool: std::rc::Rc<block::Pool>,
    pub(crate) sender: Sender,
    pub(crate) receiver: Receiver,
}

impl Stream {
    pub(crate) async fn send(&mut self, bytes: &[u8]) {
        let mut block = self.pool.alloc(bytes.len()).expect("the pool has room");
        block.copy_from_slice(bytes);
        self.sender.send(block.freeze()).await.expect("sends");
    }

    /// Sends a request of `length` bytes, signed over `body`, and `body` in messages
    /// of at most 64 KiB.
    pub(crate) async fn request(&mut self, length: u64, body: &[u8]) {
        let signed = access::proof::request(CONNECTION, body);
        let request = Request {
            length,
            signature: Pair::new(&AGENT).sign(&signed),
        };
        let mut out = [0; Request::LEN];
        request.encode(&mut out);
        self.send(&out).await;
        for chunk in body.chunks(1 << 16) {
            self.send(chunk).await;
        }
    }

    /// The next message from the node, `None` once it finished.
    pub(crate) async fn recv(&mut self) -> Result<Option<Vec<u8>>, transport::Error> {
        Ok(self.receiver.recv().await?.map(|block| block.to_vec()))
    }

    /// The next challenge from the node.
    pub(crate) async fn challenge(&mut self) -> Challenge {
        let message = self.receiver.recv().await.expect("a message");
        let message = message.expect("a challenge before the finish");
        Challenge::decode(&message).expect("a challenge")
    }

    /// The response and its body, once the node finished the stream.
    pub(crate) async fn response(&mut self) -> Vec<u8> {
        let message = self.receiver.recv().await.expect("a message");
        let message = message.expect("a response before the finish");
        let mut rest = Response::decode(&message).expect("a response").body();
        let mut body = Vec::new();
        while let Some(message) = self.receiver.recv().await.expect("a message") {
            body.extend_from_slice(rest.take(&message).expect("a body message"));
        }
        rest.end().expect("the whole body");
        body
    }
}

/// The program: its session, and its hello stream.
pub(crate) struct Agent {
    pub(crate) node: sim::node::Node,
    /// The subject of each hello that [`Agent::admit`] sends.
    subject: Name,
    pool: std::rc::Rc<block::Pool>,
    pub(crate) session: Session,
    pub(crate) hello: Stream,
}

impl Agent {
    /// Dials the home at `at` from `node` as a program of `subject`, and opens the
    /// hello stream.
    pub(crate) async fn dial(
        node: &sim::node::Node,
        tasks: env::tasks::Tasks,
        at: Address,
        subject: Name,
    ) -> Self {
        let pool = own_pool();
        let config = transport::client::Config {
            net: node.net(),
            clock: node.clock(),
            entropy: node.entropy(),
            tasks,
            pool: std::rc::Rc::clone(&pool),
        };
        let client = transport::Client::new(config).expect("a client");
        let session = client.dial(public_key(&HOME), &[at]).await.expect("dials");
        let (sender, receiver) = session.open(Class::Complete).await.expect("opens");
        let mut hello = Stream {
            pool: std::rc::Rc::clone(&pool),
            sender,
            receiver,
        };
        hello.send(&wire::header::encode(Protocol::Hub)).await;
        Self {
            node: node.clone(),
            subject,
            pool,
            session,
            hello,
        }
    }

    /// A hello of [`SUBJECT`] through [`NODE`] that echoes `challenge` and lives
    /// [`LIFE`] from the challenge's mesh time.
    pub(crate) fn hello(challenge: Challenge) -> Hello {
        Hello {
            subject: name(SUBJECT),
            key: public_key(&AGENT),
            via: NODE,
            connection: CONNECTION,
            nonce: challenge.nonce,
            expires: challenge.now.latest + LIFE,
        }
    }

    /// Signs `hello` with `key` and sends it on the hello stream.
    pub(crate) async fn send_hello(&mut self, hello: Hello, key: &PrivateKey) {
        self.hello.send(&signed(hello, key)).await;
    }

    /// Takes the challenge, answers it with a valid hello of the agent's subject, and
    /// gives the next challenge, which the node sends once it admitted the hello.
    pub(crate) async fn admit(&mut self) -> Challenge {
        let challenge = self.hello.challenge().await;
        let hello = Hello {
            subject: self.subject.clone(),
            ..Self::hello(challenge)
        };
        self.send_hello(hello, &AGENT).await;
        self.hello.challenge().await
    }

    /// Opens a request stream and sends its header.
    pub(crate) async fn open(&self) -> Stream {
        let mut stream = self.silent().await;
        stream.send(&wire::header::encode(Protocol::Hub)).await;
        stream
    }

    /// Opens a stream and sends nothing, so the node does not see it yet.
    pub(crate) async fn silent(&self) -> Stream {
        let (sender, receiver) =
            self.session.open(Class::Complete).await.expect("opens");
        Stream {
            pool: std::rc::Rc::clone(&self.pool),
            sender,
            receiver,
        }
    }

    /// Sends a request of `length` bytes, signed over `body`, and `body` in messages
    /// of at most 64 KiB, on a new stream. Finishes the stream.
    pub(crate) async fn request(&self, length: u64, body: &[u8]) -> Stream {
        let mut stream = self.unfinished(length, body).await;
        stream.sender.finish().expect("finishes");
        stream
    }

    /// As [`Agent::request`], but leaves the stream open.
    pub(crate) async fn unfinished(&self, length: u64, body: &[u8]) -> Stream {
        let mut stream = self.open().await;
        stream.request(length, body).await;
        stream
    }

    /// How the node closed the session.
    pub(crate) async fn closed(&self) -> transport::Error {
        self.session.closed().await
    }

    pub(crate) async fn sleep(&self, span: Span) {
        self.node.clock().sleep(span).await;
    }
}

/// `hello`, signed with `key` and encoded.
pub(crate) fn signed(hello: Hello, key: &PrivateKey) -> Vec<u8> {
    let signature = Pair::new(key).sign(&access::proof::hello(&hello));
    let signed = Signed { hello, signature };
    let mut out = vec![0; signed.encoded_len()];
    signed.encode(&mut out);
    out
}

pub(crate) fn closed_with(code: u32) -> transport::Error {
    transport::Error::PeerClosed { code: Code(code) }
}

pub(crate) fn reset_with(code: u32) -> Result<Option<Vec<u8>>, transport::Error> {
    Err(transport::Error::Reset { code: Code(code) })
}
