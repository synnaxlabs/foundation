//! The client sessions that `Link::serve` serves, over a real transport from a program
//! to a simulated node: the hello stream, and the request streams.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use access::proof::{Error as Refusal, Field};
use hub::{Served, serve};
use spec::definition::{Definition, Kind};
use spec::subject::Subject;
use transport::stream::{Incoming, Receiver, Sender};
use transport::{Address, Class, Code, Port, Session};
use types::connection;
use types::ed25519::{Pair, PrivateKey};
use types::hello::Hello;
use types::name::{Name, Prefix};
use types::node;
use types::time::Span;
use wire::Protocol;
use wire::header::MALFORMED;
use wire::hub::client::{
    CHANGED, Challenge, EXPIRED, REFUSED, Request, Response, STALE, Signed, UNSYNCED,
    VIA,
};

use super::serve::{HOME, PORT, own_pool, public_key, transport};
use super::{AREA, BODY_MAX, NODE, POOL, Test};

pub(super) const SUBJECT: &str = "ops.agent";
/// The key that the spec lists for [`SUBJECT`].
pub(super) const AGENT: PrivateKey = PrivateKey([3; 32]);
/// A key that the spec does not list.
pub(super) const OTHER: PrivateKey = PrivateKey([4; 32]);
const CONNECTION: connection::Key = connection::Key([7; 16]);
/// How long each hello lives, unless a test says otherwise.
const LIFE: Span = Span::from_nanos(60 * Span::SECOND.nanos());
/// How long the home holds a request before it replies.
const HOLD: Span = Span::from_nanos(50_000_000);
/// How long the program waits for what must not come.
pub(super) const QUIET: Span = Span::from_nanos(200_000_000);

pub(super) fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
}

/// The rules of a root tree that lists `AGENT` for [`SUBJECT`].
pub(super) fn rules() -> access::Rules {
    let key = Kind::Subject.key(SUBJECT).expect("a subject key");
    let subject = Subject::new(vec![public_key(&AGENT)]).expect("a subject");
    let tree: BTreeMap<Name, Definition> = [(key, Definition::Subject(subject))].into();
    access::Rules::new([(Prefix::ROOT, &tree)])
}

/// What the home's link gave for one stream.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Got {
    Ended,
    /// A request, with the subject of its hello and its body.
    Request(Name, Vec<u8>),
}

/// What the home saw: what `serve` gave for each stream, in the order they ended, and
/// how the session ended.
#[derive(Debug)]
pub(super) struct Home {
    pub(super) served: Vec<Result<Got, serve::Error>>,
    pub(super) closed: transport::Error,
}

/// Runs one client session: the home's node makes a [`Test`] hub, with mesh time when
/// `synced`, and serves each hub stream of the session on one `hub::Link`, once it
/// reads the stream's header in the stream's own future, as `node` does. It replies
/// to each request with its body reversed, after [`HOLD`]. The program's node gives
/// its end to `program`.
fn session<P>(
    seed: u64,
    synced: bool,
    program: impl FnOnce(Agent) -> P + Send + 'static,
) -> Home
where
    P: Future<Output = ()> + 'static,
{
    session_with(seed, synced, POOL, Some(rules()), program)
}

/// As [`session`], with a home pool of `pool` bytes, and `rules` given to
/// `Hub::set_rules` unless `None`.
fn session_with<P>(
    seed: u64,
    synced: bool,
    pool: usize,
    rules: Option<access::Rules>,
    program: impl FnOnce(Agent) -> P + Send + 'static,
) -> Home
where
    P: Future<Output = ()> + 'static,
{
    serve_session(seed, synced, pool, rules, move |node, tasks, at| {
        as_agent(node, tasks, at, program)
    })
}

/// As [`session_with`], where `program` runs on the program's node with the home's
/// address, and dials it itself.
pub(super) fn serve_session<G, P>(
    seed: u64,
    synced: bool,
    pool: usize,
    rules: Option<access::Rules>,
    program: G,
) -> Home
where
    G: FnOnce(sim::node::Node, env::tasks::Tasks, Address) -> P + Send + 'static,
    P: Future<Output = ()> + 'static,
{
    serve_session_on(
        seed,
        sim::link::Config::default(),
        synced,
        pool,
        rules,
        program,
    )
}

/// As [`serve_session`], on `wire`.
pub(super) fn serve_session_on<G, P>(
    seed: u64,
    wire: sim::link::Config,
    synced: bool,
    pool: usize,
    rules: Option<access::Rules>,
    program: G,
) -> Home
where
    G: FnOnce(sim::node::Node, env::tasks::Tasks, Address) -> P + Send + 'static,
    P: Future<Output = ()> + 'static,
{
    let served = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(Mutex::new(None));
    let (kept, ended) = (Arc::clone(&served), Arc::clone(&closed));
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let (test, session, link) = accept(&node, &tasks, pool, synced, rules).await;
        serve_streams(&node, &tasks, &session, &link, &kept).await;
        *ended.lock().expect("not poisoned") = Some(session.closed().await);
        drop((link, test));
    };
    run_program_on(seed, wire, home, program);
    let served = std::mem::take(&mut *served.lock().expect("not poisoned"));
    let closed = closed.lock().expect("not poisoned").take();
    Home {
        served,
        closed: closed.expect("the session closed"),
    }
}

/// A [`Test`] hub with a home pool of `pool` bytes, with mesh time when `synced`.
/// A [`Test`] hub as [`home`] gives it, with `rules` set unless `None`, and the
/// session of the first program that dials it, with its link.
/// Serves each hub stream of `session` on `link`, once it reads the stream's header,
/// and keeps each result in `kept`, until the session closes.
pub(super) async fn serve_streams(
    node: &sim::node::Node,
    tasks: &env::tasks::Tasks,
    session: &Session,
    link: &hub::Link,
    kept: &Arc<Mutex<Vec<Result<Got, serve::Error>>>>,
) {
    while let Ok(mut incoming) = session.accept().await {
        let (link, kept, clock) = (link.clone(), Arc::clone(kept), node.clock());
        tasks.spawn(async move {
            header(&mut incoming).await;
            let got = answer(link.serve(incoming), &clock).await;
            kept.lock().expect("not poisoned").push(got);
        });
    }
}

/// Makes the error of the wall clock of `node` unknown, and 10 ms one minute later.
pub(super) fn shrink_wall_error(node: &sim::node::Node, tasks: &env::tasks::Tasks) {
    node.set_wall_error(None);
    let shrink = node.clone();
    tasks.spawn(async move {
        shrink.clock().sleep(Span::MINUTE).await;
        shrink.set_wall_error(Some(Span::from_nanos(10_000_000)));
    });
}

pub(super) async fn accept(
    node: &sim::node::Node,
    tasks: &env::tasks::Tasks,
    pool: usize,
    synced: bool,
    rules: Option<access::Rules>,
) -> (Test, Session, hub::Link) {
    let test = home(node, tasks, pool, synced).await;
    if let Some(rules) = rules {
        test.hub.set_rules(rules);
    }
    let transport = transport(node, tasks, &own_pool(), HOME, 1 << 16);
    let session = transport.accept().await.expect("a session");
    let link = test.hub.link(session.clone());
    (test, session, link)
}

pub(super) async fn home(
    node: &sim::node::Node,
    tasks: &env::tasks::Tasks,
    pool: usize,
    synced: bool,
) -> Test {
    let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
    let mut test = Test::new(node.clone(), tasks.clone(), layout, pool, None).await;
    if synced {
        test.sync().await;
    }
    test
}

/// Runs `home` on one simulated node, and `program` on another, which dials the
/// first as a program.
fn run<H, P>(
    seed: u64,
    home: impl FnOnce(sim::node::Node, env::tasks::Tasks) -> H + Send + 'static,
    program: impl FnOnce(Agent) -> P + Send + 'static,
) where
    H: Future<Output = ()> + 'static,
    P: Future<Output = ()> + 'static,
{
    run_program(seed, home, move |node, tasks, at| {
        as_agent(node, tasks, at, program)
    });
}

/// Dials the home at `at` as an [`Agent`], runs `program`, and closes the session.
async fn as_agent<P>(
    node: sim::node::Node,
    tasks: env::tasks::Tasks,
    at: Address,
    program: impl FnOnce(Agent) -> P,
) where
    P: Future<Output = ()>,
{
    let agent = Agent::dial(&node, tasks, at).await;
    let session = agent.session.clone();
    program(agent).await;
    session.close(Code(0));
    node.clock().sleep(Span::MILLISECOND).await;
}

/// Runs `home` on one simulated node, and `program` on another, with the home's
/// address.
pub(super) fn run_program<F, H, G, P>(seed: u64, home: F, program: G)
where
    F: FnOnce(sim::node::Node, env::tasks::Tasks) -> H + Send + 'static,
    H: Future<Output = ()> + 'static,
    G: FnOnce(sim::node::Node, env::tasks::Tasks, Address) -> P + Send + 'static,
    P: Future<Output = ()> + 'static,
{
    run_program_on(seed, sim::link::Config::default(), home, program);
}

pub(super) fn run_program_on<F, H, G, P>(
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
pub(super) async fn header(incoming: &mut Incoming) {
    let header = incoming.receiver.recv().await.expect("a header");
    let header = header.expect("the header comes before the finish");
    assert_eq!(wire::header::decode(&header), Ok((Protocol::Hub, &[][..])));
}

/// What `serve` gave for one stream, once it replied to a request with its body
/// reversed after [`HOLD`].
pub(super) async fn answer(
    serve: impl Future<Output = Result<Served, serve::Error>>,
    clock: &env::clock::Clock,
) -> Result<Got, serve::Error> {
    match serve.await? {
        Served::Ended => Ok(Got::Ended),
        Served::Request(request) => {
            let subject = request.admitted.hello.subject.clone();
            let reply: Vec<u8> = request.body.iter().rev().copied().collect();
            clock.sleep(HOLD).await;
            request.reply.send(&reply).await.expect("sends the reply");
            Ok(Got::Request(subject, request.body))
        }
    }
}

/// The program's end of one stream.
struct Stream {
    pool: std::rc::Rc<block::Pool>,
    sender: Sender,
    receiver: Receiver,
}

impl Stream {
    async fn send(&mut self, bytes: &[u8]) {
        let mut block = self.pool.alloc(bytes.len()).expect("the pool has room");
        block.copy_from_slice(bytes);
        self.sender.send(block.freeze()).await.expect("sends");
    }

    /// Sends a request of `length` bytes, signed over `body`, and `body` in messages
    /// of at most 64 KiB.
    async fn request(&mut self, length: u64, body: &[u8]) {
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
    async fn recv(&mut self) -> Result<Option<Vec<u8>>, transport::Error> {
        Ok(self.receiver.recv().await?.map(|block| block.to_vec()))
    }

    /// The next challenge from the node.
    async fn challenge(&mut self) -> Challenge {
        let message = self.receiver.recv().await.expect("a message");
        let message = message.expect("a challenge before the finish");
        Challenge::decode(&message).expect("a challenge")
    }

    /// The response and its body, once the node finished the stream.
    async fn response(&mut self) -> Vec<u8> {
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
struct Agent {
    node: sim::node::Node,
    pool: std::rc::Rc<block::Pool>,
    session: Session,
    hello: Stream,
}

impl Agent {
    /// Dials the home at `at` from `node` as a program, and opens the hello stream.
    async fn dial(
        node: &sim::node::Node,
        tasks: env::tasks::Tasks,
        at: Address,
    ) -> Self {
        let pool = own_pool();
        let own = SocketAddr::new(node.addresses()[0], PORT);
        let mut parts = Port::bind(&node.net(), own)
            .expect("binds")
            .split(NonZeroUsize::MIN);
        let config = transport::client::Config {
            clock: node.clock(),
            entropy: node.entropy(),
            tasks,
            pool: std::rc::Rc::clone(&pool),
        };
        let client = transport::Client::new(config, parts.pop().expect("one part"))
            .expect("a client");
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
            pool,
            session,
            hello,
        }
    }

    /// A hello of [`SUBJECT`] through [`NODE`] that echoes `challenge` and lives
    /// [`LIFE`] from the challenge's mesh time.
    fn hello(challenge: Challenge) -> Hello {
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
    async fn send_hello(&mut self, hello: Hello, key: &PrivateKey) {
        self.hello.send(&signed(hello, key)).await;
    }

    /// Takes the challenge, answers it with a valid hello, and gives the next
    /// challenge, which the node sends once it admitted the hello.
    async fn admit(&mut self) -> Challenge {
        let challenge = self.hello.challenge().await;
        self.send_hello(Self::hello(challenge), &AGENT).await;
        self.hello.challenge().await
    }

    /// Opens a request stream and sends its header.
    async fn open(&self) -> Stream {
        let mut stream = self.silent().await;
        stream.send(&wire::header::encode(Protocol::Hub)).await;
        stream
    }

    /// Opens a stream and sends nothing, so the node does not see it yet.
    async fn silent(&self) -> Stream {
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
    async fn request(&self, length: u64, body: &[u8]) -> Stream {
        let mut stream = self.unfinished(length, body).await;
        stream.sender.finish().expect("finishes");
        stream
    }

    /// As [`Agent::request`], but leaves the stream open.
    async fn unfinished(&self, length: u64, body: &[u8]) -> Stream {
        let mut stream = self.open().await;
        stream.request(length, body).await;
        stream
    }

    /// How the node closed the session.
    async fn closed(&self) -> transport::Error {
        self.session.closed().await
    }

    async fn sleep(&self, span: Span) {
        self.node.clock().sleep(span).await;
    }
}

/// `hello`, signed with `key` and encoded.
fn signed(hello: Hello, key: &PrivateKey) -> Vec<u8> {
    let signature = Pair::new(key).sign(&access::proof::hello(&hello));
    let signed = Signed { hello, signature };
    let mut out = vec![0; signed.encoded_len()];
    signed.encode(&mut out);
    out
}

fn closed_with(code: u32) -> transport::Error {
    transport::Error::PeerClosed { code: Code(code) }
}

fn reset_with(code: u32) -> Result<Option<Vec<u8>>, transport::Error> {
    Err(transport::Error::Reset { code: Code(code) })
}

#[test]
fn answers_a_request_of_an_admitted_hello() {
    let home = session(80, true, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.request(4, b"ping").await;
        assert_eq!(stream.response().await, b"gnip");
        agent.hello.sender.finish().expect("finishes");
        agent.sleep(QUIET).await;
    });
    assert_eq!(
        home.served,
        [
            Ok(Got::Request(name(SUBJECT), b"ping".to_vec())),
            Ok(Got::Ended)
        ]
    );
    assert_eq!(home.closed, transport::Error::Closed { code: Code(0) });
}

/// The node takes a request once its body is whole, before the program finishes the
/// stream.
#[test]
fn answers_a_request_whose_stream_stays_open() {
    let home = session(101, true, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.unfinished(2, b"ab").await;
        assert_eq!(stream.response().await, b"ba");
    });
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"ab".to_vec()))
    );
}

#[test]
fn answers_a_request_with_no_body() {
    let home = session(81, true, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.request(0, b"").await;
        assert_eq!(stream.response().await, b"");
    });
    assert_eq!(home.served[0], Ok(Got::Request(name(SUBJECT), Vec::new())));
}

/// A body and a response over the 64 KiB message limit go in parts.
#[test]
fn takes_a_request_and_sends_its_response_in_parts() {
    let body: Vec<u8> = (0..200_000_u32).map(|i| (i % 251) as u8).collect();
    let expected: Vec<u8> = body.iter().rev().copied().collect();
    let sent = body.clone();
    let home = session(82, true, move |mut agent| async move {
        agent.admit().await;
        let length = u64::try_from(sent.len()).expect("fits");
        let mut stream = agent.request(length, &sent).await;
        assert_eq!(stream.response().await, expected);
    });
    assert_eq!(home.served[0], Ok(Got::Request(name(SUBJECT), body)));
}

#[test]
fn refuses_a_hello_of_a_key_the_spec_does_not_list_and_closes_the_session() {
    let home = session(83, true, |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut hello = Agent::hello(challenge);
        hello.key = public_key(&OTHER);
        agent.send_hello(hello, &OTHER).await;
        assert_eq!(agent.closed().await, closed_with(REFUSED));
    });
    let refusal = Refusal::Unlisted {
        subject: name(SUBJECT),
        key: public_key(&OTHER),
    };
    assert_eq!(home.served, [Err(serve::Error::Access(refusal))]);
    assert_eq!(
        home.closed,
        transport::Error::Closed {
            code: Code(REFUSED)
        }
    );
    assert_eq!(
        serve::Error::Access(Refusal::Signature).to_string(),
        "access refused the program: the signature is not of the message by the key"
    );
}

#[test]
fn refuses_a_hello_that_does_not_echo_the_nonce() {
    let home = session(84, true, |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut hello = Agent::hello(challenge);
        hello.nonce[0] ^= 1;
        agent.send_hello(hello, &AGENT).await;
        assert_eq!(agent.hello.recv().await, Err(closed_with(STALE)));
        assert_eq!(agent.closed().await, closed_with(STALE));
    });
    assert_eq!(home.served, [Err(serve::Error::Stale)]);
    assert_eq!(
        serve::Error::Stale.to_string(),
        "the hello does not echo the nonce of the node's last challenge"
    );
}

#[test]
fn refuses_each_hello_before_the_first_rules() {
    let home = session_with(106, true, POOL, None, |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        agent.send_hello(Agent::hello(challenge), &AGENT).await;
        assert_eq!(agent.closed().await, closed_with(REFUSED));
    });
    let refusal = Refusal::Unknown {
        subject: name(SUBJECT),
    };
    assert_eq!(home.served, [Err(serve::Error::Access(refusal))]);
    assert_eq!(
        home.closed,
        transport::Error::Closed {
            code: Code(REFUSED)
        }
    );
}

#[test]
fn refuses_a_hello_through_another_node() {
    let other = node::Key::from_u128(2);
    let home = session(85, true, move |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut hello = Agent::hello(challenge);
        hello.via = other;
        agent.send_hello(hello, &AGENT).await;
        assert_eq!(agent.closed().await, closed_with(VIA));
    });
    let refusal = Refusal::Via {
        via: other,
        peer: NODE,
    };
    assert_eq!(home.served, [Err(serve::Error::Access(refusal))]);
}

/// A node with no mesh time sends no challenge, and closes the session.
#[test]
fn closes_the_session_as_unsynced_when_the_node_has_no_mesh_time() {
    let home = session(86, false, |agent| async move {
        assert_eq!(agent.closed().await, closed_with(UNSYNCED));
    });
    assert_eq!(home.served, [Err(serve::Error::Access(Refusal::Unsynced))]);
}

#[test]
fn closes_the_session_when_the_hello_expires() {
    let life = Span::from_nanos(2 * Span::SECOND.nanos());
    let expires = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&expires);
    let home = session(87, true, move |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut hello = Agent::hello(challenge);
        hello.expires = challenge.now.latest + life;
        *kept.lock().expect("not poisoned") = Some(hello.expires);
        agent.send_hello(hello, &AGENT).await;
        assert_eq!(agent.closed().await, closed_with(EXPIRED));
    });
    let expires = expires.lock().expect("not poisoned").expect("a hello");
    let [Err(serve::Error::Access(Refusal::Expired { expires: at, now }))] =
        home.served.as_slice()
    else {
        panic!("one expiry, not {:?}", home.served);
    };
    assert_eq!(*at, expires);
    assert!(
        *now >= expires && *now < expires + Span::from_nanos(10_000_000),
        "the node closed the session at the expiry: {now:?}, {expires:?}"
    );
}

/// A renewal before the expiry moves it, so a request after the first expiry gets its
/// response.
#[test]
fn keeps_the_session_past_the_first_expiry_after_a_renewal() {
    let life = Span::from_nanos(2 * Span::SECOND.nanos());
    let home = session(88, true, move |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut hello = Agent::hello(challenge);
        hello.expires = challenge.now.latest + life;
        agent.send_hello(hello, &AGENT).await;
        let challenge = agent.hello.challenge().await;
        agent.sleep(Span::SECOND).await;
        agent.send_hello(Agent::hello(challenge), &AGENT).await;
        agent
            .sleep(Span::from_nanos(2 * Span::SECOND.nanos()))
            .await;
        let mut stream = agent.request(2, b"ab").await;
        assert_eq!(stream.response().await, b"ba");
    });
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"ab".to_vec()))
    );
}

/// A renewal that moves the expiry earlier closes the session at the new expiry.
#[test]
fn closes_the_session_at_the_expiry_of_a_renewal_that_moves_it_earlier() {
    let short = Span::from_nanos(2 * Span::SECOND.nanos());
    let expires = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&expires);
    let home = session(102, true, move |mut agent| async move {
        let challenge = agent.admit().await;
        let mut hello = Agent::hello(challenge);
        hello.expires = challenge.now.latest + short;
        *kept.lock().expect("not poisoned") = Some(hello.expires);
        agent.send_hello(hello, &AGENT).await;
        assert_eq!(agent.closed().await, closed_with(EXPIRED));
    });
    let expires = expires.lock().expect("not poisoned").expect("a hello");
    let [Err(serve::Error::Access(Refusal::Expired { expires: at, now }))] =
        home.served.as_slice()
    else {
        panic!("one expiry, not {:?}", home.served);
    };
    assert_eq!(*at, expires);
    assert!(
        *now >= expires && *now < expires + Span::from_nanos(10_000_000),
        "the node closed the session at the renewed expiry: {now:?}, {expires:?}"
    );
}

#[test]
fn refuses_a_renewal_on_another_connection() {
    let home = session(89, true, |mut agent| async move {
        let challenge = agent.admit().await;
        let mut hello = Agent::hello(challenge);
        hello.connection = connection::Key([8; 16]);
        agent.send_hello(hello, &AGENT).await;
        assert_eq!(agent.closed().await, closed_with(CHANGED));
    });
    let refusal = Refusal::Changed {
        field: Field::Connection,
    };
    assert_eq!(home.served, [Err(serve::Error::Access(refusal))]);
}

/// A request stream before the hello is admitted stops as `MALFORMED`, and the
/// session stays open.
#[test]
fn stops_a_request_before_the_hello_is_admitted() {
    let home = session(90, true, |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut early = agent.request(2, b"ab").await;
        assert_eq!(early.recv().await, reset_with(MALFORMED));
        agent.send_hello(Agent::hello(challenge), &AGENT).await;
        agent.hello.challenge().await;
        let mut stream = agent.request(2, b"cd").await;
        assert_eq!(stream.response().await, b"dc");
    });
    assert_eq!(home.served[0], Err(serve::Error::Unadmitted));
    assert_eq!(
        home.served[1],
        Ok(Got::Request(name(SUBJECT), b"cd".to_vec()))
    );
    assert_eq!(
        serve::Error::Unadmitted.to_string(),
        "the program sent a request before the node admitted a hello"
    );
}

/// The link takes one open request: a second, while the first waits for its reply,
/// stops as `MALFORMED`.
#[test]
fn stops_a_second_request_while_one_is_open() {
    let home = session(91, true, |mut agent| async move {
        agent.admit().await;
        let mut first = agent.request(2, b"ab").await;
        agent.sleep(Span::from_nanos(HOLD.nanos() / 5)).await;
        let mut second = agent.request(2, b"cd").await;
        assert_eq!(second.recv().await, reset_with(MALFORMED));
        assert_eq!(first.response().await, b"ba");
        let mut third = agent.request(2, b"ef").await;
        assert_eq!(third.response().await, b"fe");
    });
    assert_eq!(
        home.served,
        [
            Err(serve::Error::Pending),
            Ok(Got::Request(name(SUBJECT), b"ab".to_vec())),
            Ok(Got::Request(name(SUBJECT), b"ef".to_vec())),
            Err(serve::Error::Stream(closed_with(0))),
        ]
    );
    assert_eq!(
        serve::Error::Pending.to_string(),
        "the program sent a request while another request waits for its reply"
    );
}

/// The link frees a request before the first byte of its reply, so a request sent once
/// the last reply ends is never refused as pending.
#[test]
fn takes_each_request_sent_once_the_last_reply_ends() {
    const COUNT: u8 = 20;
    let home = session(98, true, |mut agent| async move {
        agent.admit().await;
        for i in 0..COUNT {
            let mut stream = agent.request(2, &[i, 0]).await;
            assert_eq!(stream.response().await, [0, i]);
        }
    });
    let expected: Vec<_> = (0..COUNT)
        .map(|i| Ok(Got::Request(name(SUBJECT), vec![i, 0])))
        .chain([Err(serve::Error::Stream(closed_with(0)))])
        .collect();
    assert_eq!(home.served, expected);
}

/// A hello on a stream after the first stops as `MALFORMED`, and the session stays
/// open.
#[test]
fn stops_a_hello_on_a_request_stream() {
    let home = session(99, true, |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        agent.send_hello(Agent::hello(challenge), &AGENT).await;
        let challenge = agent.hello.challenge().await;
        let mut other = agent.open().await;
        other.send(&signed(Agent::hello(challenge), &AGENT)).await;
        assert_eq!(other.recv().await, reset_with(MALFORMED));
        let mut stream = agent.request(2, b"ab").await;
        assert_eq!(stream.response().await, b"ba");
    });
    assert_eq!(
        home.served,
        [
            Err(serve::Error::Message(wire::hub::Error::Kind { kind: 4 })),
            Ok(Got::Request(name(SUBJECT), b"ab".to_vec())),
            Err(serve::Error::Stream(closed_with(0))),
        ]
    );
}

#[test]
fn stops_a_request_whose_body_did_not_come() {
    let home = session(92, true, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.open().await;
        let request = Request {
            length: 10,
            signature: [0; 64],
        };
        let mut out = [0; Request::LEN];
        request.encode(&mut out);
        stream.send(&out).await;
        stream.send(b"abcd").await;
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.recv().await, reset_with(MALFORMED));
    });
    let unfinished = wire::hub::Error::Unfinished { remain: 6 };
    assert_eq!(
        home.served,
        [
            Err(serve::Error::Message(unfinished)),
            Err(serve::Error::Stream(closed_with(0))),
        ]
    );
    assert_eq!(
        serve::Error::Message(unfinished).to_string(),
        "a hub message is not valid: the stream ended with 6 bytes of its body to come"
    );
}

#[test]
fn refuses_a_request_whose_signature_does_not_verify() {
    let home = session(93, true, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.open().await;
        let request = Request {
            length: 2,
            signature: Pair::new(&AGENT)
                .sign(&access::proof::request(CONNECTION, b"xy")),
        };
        let mut out = [0; Request::LEN];
        request.encode(&mut out);
        stream.send(&out).await;
        stream.send(b"ab").await;
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.recv().await, reset_with(REFUSED));
        let mut next = agent.request(2, b"cd").await;
        assert_eq!(next.response().await, b"dc");
    });
    assert_eq!(
        home.served[0],
        Err(serve::Error::Access(Refusal::Signature))
    );
}

/// A hello stream that the program finishes before its first hello gives `Ended`, and
/// closes the session with code 0.
#[test]
fn ends_a_hello_stream_that_finished_before_its_hello() {
    let home = session(104, true, |mut agent| async move {
        agent.hello.challenge().await;
        agent.hello.sender.finish().expect("finishes");
        agent.sleep(QUIET).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended)]);
    assert_eq!(home.closed, transport::Error::Closed { code: Code(0) });
}

/// A request stream that the program finishes before its request gives `Ended`.
#[test]
fn ends_a_request_stream_that_finished_before_its_request() {
    let home = session(94, true, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.open().await;
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.recv().await, Ok(None));
    });
    assert_eq!(home.served[0], Ok(Got::Ended));
}

/// A request on the hello stream after a hello breaks the order of the stream, so the
/// session closes.
#[test]
fn closes_the_session_on_a_request_on_the_hello_stream() {
    let home = session(95, true, |mut agent| async move {
        agent.admit().await;
        agent.hello.send(&empty_request()).await;
        assert_eq!(agent.closed().await, closed_with(MALFORMED));
    });
    assert_eq!(
        home.served,
        [Err(serve::Error::Message(wire::hub::Error::Kind {
            kind: 5
        }))]
    );
}

/// A request as the first message of the session asks before a hello, so the session
/// closes.
#[test]
fn closes_the_session_on_a_request_before_the_first_hello() {
    let home = session(100, true, |mut agent| async move {
        agent.hello.challenge().await;
        agent.hello.send(&empty_request()).await;
        assert_eq!(agent.closed().await, closed_with(MALFORMED));
    });
    assert_eq!(
        home.served,
        [Err(serve::Error::Message(wire::hub::Error::Kind {
            kind: 5
        }))]
    );
}

/// The link frees a request before the first byte of its response.
#[test]
fn takes_a_request_sent_once_the_last_response_header_comes() {
    let body: Vec<u8> = (0..3_000_000_u32).map(|i| (i % 251) as u8).collect();
    let home = session(103, true, move |mut agent| async move {
        agent.admit().await;
        let length = u64::try_from(body.len()).expect("fits");
        let mut first = agent.open().await;
        let request = Request {
            length,
            signature: Pair::new(&AGENT)
                .sign(&access::proof::request(CONNECTION, &body)),
        };
        let mut out = [0; Request::LEN];
        request.encode(&mut out);
        first.send(&out).await;
        for chunk in body.chunks(1 << 16) {
            first.send(chunk).await;
            agent.sleep(Span::MILLISECOND).await;
        }
        first.sender.finish().expect("finishes");
        let header = first.recv().await.expect("a message").expect("a header");
        Response::decode(&header).expect("a response");
        let mut second = agent.request(2, b"ab").await;
        while first.recv().await.expect("a message").is_some() {}
        assert_eq!(second.response().await, b"ba");
    });
    assert!(
        home.served
            .iter()
            .all(|got| got != &Err(serve::Error::Pending)),
        "{:?}",
        home.served
    );
}

/// A response chunk fits the largest block of the home's pool, also when a message of
/// the transport is larger.
#[test]
fn cuts_a_response_to_the_largest_block_of_the_pool() {
    let body: Vec<u8> = (0..60_000_u32).map(|i| (i % 251) as u8).collect();
    let reversed: Vec<u8> = body.iter().rev().copied().collect();
    let length = u64::try_from(body.len()).expect("fits");
    let home = session_with(
        105,
        true,
        1 << 16,
        Some(rules()),
        move |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.open().await;
            let request = Request {
                length,
                signature: Pair::new(&AGENT)
                    .sign(&access::proof::request(CONNECTION, &body)),
            };
            let mut out = [0; Request::LEN];
            request.encode(&mut out);
            stream.send(&out).await;
            for chunk in body.chunks(1 << 16) {
                stream.send(chunk).await;
                agent.sleep(Span::MILLISECOND).await;
            }
            stream.sender.finish().expect("finishes");
            assert_eq!(stream.response().await, reversed);
        },
    );
    assert!(
        matches!(home.served[0], Ok(Got::Request(_, _))),
        "{:?}",
        home.served
    );
}

/// A request with no body and a signature that does not verify.
fn empty_request() -> [u8; Request::LEN] {
    let request = Request {
        length: 0,
        signature: [0; 64],
    };
    let mut out = [0; Request::LEN];
    request.encode(&mut out);
    out
}

/// A hello whose expiry is past the cap lives only until the cap, so a renewal cannot
/// hold a session for longer than the cap.
#[test]
fn closes_the_session_at_the_cap_of_a_hello_past_it() {
    let cap = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&cap);
    let home = session(96, true, move |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut hello = Agent::hello(challenge);
        hello.expires = challenge.now.latest + access::proof::CAP + Span::SECOND;
        *kept.lock().expect("not poisoned") =
            Some(challenge.now.latest + access::proof::CAP);
        agent.send_hello(hello, &AGENT).await;
        assert_eq!(agent.closed().await, closed_with(EXPIRED));
    });
    let cap = cap.lock().expect("not poisoned").expect("a hello");
    let [Err(serve::Error::Access(Refusal::Expired { expires: at, now }))] =
        home.served.as_slice()
    else {
        panic!("one expiry, not {:?}", home.served);
    };
    let soon = Span::from_nanos(10_000_000);
    assert!(
        *at >= cap && *at < cap + soon,
        "the hello ends at the cap: {at:?}, {cap:?}"
    );
    assert!(
        *now >= *at && *now < *at + soon,
        "the node closed the session at the cap: {now:?}, {at:?}"
    );
}

/// A node with an unknown error admits a hello, its error shrinks to 10 ms, and the
/// program renews once from the old challenge, then stops. The renewal ends at the
/// cap past the latest edge at its admission, and the session closes there.
#[test]
fn closes_the_session_at_the_cap_of_a_renewal_after_a_drop() {
    let ten = Span::from_nanos(10 * Span::MINUTE.nanos());
    let five = Span::from_nanos(5 * Span::MINUTE.nanos());
    let third = Arc::new(Mutex::new(None));
    let kept_third = Arc::clone(&third);
    let served = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&served);
    run(
        173,
        move |node, tasks| async move {
            shrink_wall_error(&node, &tasks);
            let (test, session, link) =
                accept(&node, &tasks, POOL, true, Some(rules())).await;
            serve_streams(&node, &tasks, &session, &link, &kept).await;
            drop((link, test));
        },
        move |mut agent| async move {
            let first = agent.hello.challenge().await;
            let mut hello = Agent::hello(first);
            hello.expires = first.now.latest + ten;
            agent.send_hello(hello, &AGENT).await;
            let second = agent.hello.challenge().await;
            agent.sleep(five).await;
            let mut hello = Agent::hello(second);
            hello.expires = second.now.latest + five + ten;
            agent.send_hello(hello, &AGENT).await;
            let next = agent.hello.challenge().await;
            *kept_third.lock().expect("not poisoned") = Some(next.now);
            let start = agent.node.clock().now();
            assert_eq!(agent.closed().await, closed_with(EXPIRED));
            let lived = agent.node.clock().now() - start;
            assert!(
                lived.nanos() <= access::proof::CAP.nanos() + Span::SECOND.nanos(),
                "the session lived {lived:?} past the renewal"
            );
        },
    );
    let next = third
        .lock()
        .expect("not poisoned")
        .expect("a third challenge");
    let served = std::mem::take(&mut *served.lock().expect("not poisoned"));
    let [Err(serve::Error::Access(Refusal::Expired { expires: at, now }))] =
        served.as_slice()
    else {
        panic!("one expiry, not {served:?}");
    };
    let cap = next.latest + access::proof::CAP;
    assert!(
        *at <= cap && *at > cap - Span::SECOND,
        "the renewal ends at the cap: {at:?}, {cap:?}"
    );
    assert!(*now >= *at && *now < *at + Span::from_nanos(10_000_000));
}

/// The first expiry that a link reads is the one the node checks: a hello that has
/// already expired at the node is refused at admit.
#[test]
fn refuses_a_hello_that_expired_before_it_came() {
    let home = session(97, true, |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut hello = Agent::hello(challenge);
        hello.expires = challenge.now.earliest - Span::SECOND;
        agent.send_hello(hello, &AGENT).await;
        assert_eq!(agent.closed().await, closed_with(EXPIRED));
    });
    assert!(
        matches!(
            home.served.as_slice(),
            [Err(serve::Error::Access(Refusal::Expired { .. }))]
        ),
        "{:?}",
        home.served
    );
}

/// `serve` takes the role of a stream at the call: the second stream given to it is a
/// request stream, also when its future runs first. This program breaks the client
/// wire rule, with a request stream before its hello.
#[test]
fn takes_the_role_of_a_stream_at_the_call() {
    let got = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&got);
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let test = home(&node, &tasks, POOL, true).await;
        let transport = transport(&node, &tasks, &own_pool(), HOME, 1 << 16);
        let session = transport.accept().await.expect("a session");
        let link = test.hub.link(session.clone());
        let mut first = session.accept().await.expect("a stream");
        let mut second = session.accept().await.expect("a stream");
        header(&mut first).await;
        header(&mut second).await;
        let hello = link.serve(first);
        let request = link.serve(second);
        let served = answer(request, &node.clock()).await;
        *kept.lock().expect("not poisoned") = Some(served);
        drop((hello, link, test));
    };
    run(107, home, |agent| async move {
        let _request = agent.open().await;
        agent.sleep(QUIET).await;
    });
    let got = got.lock().expect("not poisoned").take();
    assert_eq!(got, Some(Err(serve::Error::Unadmitted)));
}

/// The node sees a stream at its header, so a request stream that the program opens
/// before its hello, and whose header it sends after the challenge that follows the
/// hello, is a request stream.
#[test]
fn answers_a_request_on_a_stream_opened_before_the_hello() {
    let home = session(108, true, |mut agent| async move {
        let mut stream = agent.silent().await;
        agent.sleep(QUIET).await;
        agent.admit().await;
        stream.send(&wire::header::encode(Protocol::Hub)).await;
        stream.request(4, b"ping").await;
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.response().await, b"gnip");
        agent.hello.sender.finish().expect("finishes");
        agent.sleep(QUIET).await;
    });
    assert_eq!(
        home.served,
        [
            Ok(Got::Request(name(SUBJECT), b"ping".to_vec())),
            Ok(Got::Ended)
        ]
    );
}

/// The node does not check the client wire rule: a request stream whose header the
/// node reads before it admits the hello is served once the hello is admitted.
#[test]
fn serves_a_request_whose_header_came_before_the_admission() {
    let got = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&got);
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let test = home(&node, &tasks, POOL, true).await;
        test.hub.set_rules(rules());
        let transport = transport(&node, &tasks, &own_pool(), HOME, 1 << 16);
        let session = transport.accept().await.expect("a session");
        let link = test.hub.link(session.clone());
        let mut first = session.accept().await.expect("a stream");
        header(&mut first).await;
        let hello = link.serve(first);
        let mut second = session.accept().await.expect("a stream");
        header(&mut second).await;
        let request = link.serve(second);
        tasks.spawn(async move {
            drop(hello.await);
        });
        node.clock().sleep(QUIET).await;
        let served = answer(request, &node.clock()).await;
        *kept.lock().expect("not poisoned") = Some(served);
        node.clock().sleep(QUIET).await;
        drop((link, test));
    };
    run(109, home, |mut agent| async move {
        let mut stream = agent.open().await;
        agent.admit().await;
        stream.request(4, b"ping").await;
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.response().await, b"gnip");
    });
    let got = got.lock().expect("not poisoned").take();
    assert_eq!(got, Some(Ok(Got::Request(name(SUBJECT), b"ping".to_vec()))));
}
