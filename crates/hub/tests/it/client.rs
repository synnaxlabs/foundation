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
use transport::stream::{Receiver, Sender};
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
    CAPPED, CHANGED, Challenge, EXPIRED, FromGateway, Program, REFUSED, Request, STALE,
    Signed, UNSYNCED, VIA,
};

use super::serve::{HOME, PORT, own_pool, public_key, transport};
use super::{AREA, BODY_MAX, NODE, Test};

const SUBJECT: &str = "ops.agent";
/// The key that the spec lists for [`SUBJECT`].
const AGENT: PrivateKey = PrivateKey([3; 32]);
/// A key that the spec does not list.
const OTHER: PrivateKey = PrivateKey([4; 32]);
const CONNECTION: connection::Key = connection::Key([7; 16]);
/// How long each hello lives, unless a test says otherwise.
const LIFE: Span = Span::from_nanos(60 * Span::SECOND.nanos());
/// How long the home holds a request before it replies.
const HOLD: Span = Span::from_nanos(50_000_000);
/// How long the program waits for what must not come.
const QUIET: Span = Span::from_nanos(200_000_000);

fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
}

/// The rules of a root tree that lists `AGENT` for [`SUBJECT`].
fn rules() -> access::Rules {
    let key = Kind::Subject.key(SUBJECT).expect("a subject key");
    let subject = Subject::new(vec![public_key(&AGENT)]).expect("a subject");
    let tree: BTreeMap<Name, Definition> = [(key, Definition::Subject(subject))].into();
    access::Rules::new([(Prefix::ROOT, &tree)])
}

/// What the home's link gave for one stream.
#[derive(Debug, PartialEq, Eq)]
enum Got {
    Ended,
    /// A request, with the subject of its hello and its body.
    Request(Name, Vec<u8>),
}

/// What the home saw: what `serve` gave for each stream, in the order they ended, and
/// how the session ended.
#[derive(Debug)]
struct Home {
    served: Vec<Result<Got, serve::Error>>,
    closed: transport::Error,
}

/// Runs one client session: the home's node makes a [`Test`] hub, with mesh time when
/// `synced`, and serves each hub stream of the session on one `hub::Link`. It replies to
/// each request with its body reversed, after [`HOLD`]. The program's node gives its
/// end to `program`.
fn session<P>(
    seed: u64,
    synced: bool,
    program: impl FnOnce(Agent) -> P + Send + 'static,
) -> Home
where
    P: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
    let at = Address::Udp(SocketAddr::new(nodes[0].addresses()[0], PORT));
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let served = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(Mutex::new(None));
    let (kept, ended) = (Arc::clone(&served), Arc::clone(&closed));
    let node = nodes[0].clone();
    let main = move |tasks: env::tasks::Tasks| async move {
        let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
        let mut test = Test::new(node.clone(), tasks.clone(), layout).await;
        if synced {
            test.sync().await;
        }
        test.hub.rules(rules());
        let transport = transport(&node, &tasks, &own_pool(), HOME);
        let session = transport.accept().await.expect("a session");
        let link = test.hub.link(session.clone());
        while let Ok(mut incoming) = session.accept().await {
            let header = incoming.receiver.recv().await.expect("a header");
            let header = header.expect("the header comes before the finish");
            assert_eq!(wire::header::decode(&header), Ok((Protocol::Hub, &[][..])));
            drop(header);
            let serve = link.serve(incoming);
            let (kept, clock) = (Arc::clone(&kept), node.clock());
            tasks.spawn(async move {
                let got = match serve.await {
                    Ok(Served::Ended) => Ok(Got::Ended),
                    Ok(Served::Request(request)) => {
                        let subject = request.admitted.hello.subject.clone();
                        let reply: Vec<u8> =
                            request.body.iter().rev().copied().collect();
                        clock.sleep(HOLD).await;
                        request.reply.send(&reply).await.expect("sends the reply");
                        Ok(Got::Request(subject, request.body))
                    }
                    Err(error) => Err(error),
                };
                kept.lock().expect("not poisoned").push(got);
            });
        }
        *ended.lock().expect("not poisoned") = Some(session.closed().await);
        drop((link, test));
    };
    drop(
        nodes[0]
            .shards()
            .start(shard("home"), main)
            .expect("starts"),
    );
    let node = nodes[1].clone();
    let main = move |tasks: env::tasks::Tasks| async move {
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
            program: Program::default(),
        };
        hello.send(&wire::header::encode(Protocol::Hub)).await;
        let agent = Agent {
            node: node.clone(),
            pool,
            session: session.clone(),
            hello,
        };
        program(agent).await;
        session.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    };
    drop(
        nodes[1]
            .shards()
            .start(shard("program"), main)
            .expect("starts"),
    );
    sim.run().expect("the run ends");
    let served = std::mem::take(&mut *served.lock().expect("not poisoned"));
    let closed = closed.lock().expect("not poisoned").take();
    Home {
        served,
        closed: closed.expect("the session closed"),
    }
}

/// The program's end of one stream.
struct Stream {
    pool: std::rc::Rc<block::Pool>,
    sender: Sender,
    receiver: Receiver,
    program: Program,
}

impl Stream {
    async fn send(&mut self, bytes: &[u8]) {
        let mut block = self.pool.alloc(bytes.len()).expect("the pool has room");
        block.copy_from_slice(bytes);
        self.sender.send(block.freeze()).await.expect("sends");
    }

    /// The next message from the node, `None` once it finished.
    async fn recv(&mut self) -> Result<Option<Vec<u8>>, transport::Error> {
        Ok(self.receiver.recv().await?.map(|block| block.to_vec()))
    }

    /// The next challenge from the node.
    async fn challenge(&mut self) -> Challenge {
        let message = self.receiver.recv().await.expect("a message");
        let message = message.expect("a challenge before the finish");
        match self.program.decode(&message) {
            Ok(FromGateway::Challenge(challenge)) => challenge,
            other => panic!("a challenge, not {other:?}"),
        }
    }

    /// The response and its body, once the node finished the stream.
    async fn response(&mut self) -> Vec<u8> {
        let mut body = Vec::new();
        while let Some(message) = self.receiver.recv().await.expect("a message") {
            match self.program.decode(&message).expect("a valid message") {
                FromGateway::Response(_) => {}
                FromGateway::Body { bytes, .. } => body.extend_from_slice(bytes),
                FromGateway::Challenge(challenge) => panic!("{challenge:?}"),
            }
        }
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
        let signature = Pair::new(key).sign(&access::proof::hello(&hello));
        let signed = Signed { hello, signature };
        let mut out = vec![0; signed.encoded_len()];
        signed.encode(&mut out);
        self.hello.send(&out).await;
    }

    /// Takes the challenge and answers it with a valid hello.
    async fn admit(&mut self) {
        let challenge = self.hello.challenge().await;
        self.send_hello(Self::hello(challenge), &AGENT).await;
    }

    /// Opens a request stream and sends its header.
    async fn open(&self) -> Stream {
        let (sender, receiver) =
            self.session.open(Class::Complete).await.expect("opens");
        let mut stream = Stream {
            pool: std::rc::Rc::clone(&self.pool),
            sender,
            receiver,
            program: Program::default(),
        };
        stream.send(&wire::header::encode(Protocol::Hub)).await;
        stream
    }

    /// Sends a request of `length` bytes, signed over `body`, and `body` in messages
    /// of at most 64 KiB, on a new stream. Finishes the stream.
    async fn request(&self, length: u64, body: &[u8]) -> Stream {
        let mut stream = self.open().await;
        let signed = access::proof::request(CONNECTION, body);
        let request = Request {
            length,
            signature: Pair::new(&AGENT).sign(&signed),
        };
        let mut out = [0; Request::LEN];
        request.encode(&mut out);
        stream.send(&out).await;
        for chunk in body.chunks(1 << 16) {
            stream.send(chunk).await;
        }
        stream.sender.finish().expect("finishes");
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
        agent.hello.challenge().await;
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
        "access refused the program: the signature does not verify"
    );
}

#[test]
fn refuses_a_hello_that_does_not_echo_the_nonce() {
    let home = session(84, true, |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut hello = Agent::hello(challenge);
        hello.nonce[0] ^= 1;
        agent.send_hello(hello, &AGENT).await;
        assert_eq!(agent.closed().await, closed_with(STALE));
    });
    assert_eq!(home.served, [Err(serve::Error::Stale)]);
    assert_eq!(
        serve::Error::Stale.to_string(),
        "the hello does not echo the nonce of the node's last challenge"
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

#[test]
fn refuses_a_renewal_on_another_connection() {
    let home = session(89, true, |mut agent| async move {
        agent.admit().await;
        let challenge = agent.hello.challenge().await;
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
        agent.hello.challenge().await;
        let mut early = agent.request(2, b"ab").await;
        assert_eq!(early.recv().await, reset_with(MALFORMED));
        agent.admit().await;
        let mut stream = agent.request(2, b"cd").await;
        assert_eq!(stream.response().await, b"dc");
    });
    assert_eq!(home.served[0], Err(serve::Error::Order));
    assert_eq!(
        home.served[1],
        Ok(Got::Request(name(SUBJECT), b"cd".to_vec()))
    );
    assert_eq!(
        serve::Error::Order.to_string(),
        "the program opened a stream out of order: a request before its hello was \
         admitted, a second hello stream, or a second open request"
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
            Err(serve::Error::Order),
            Ok(Got::Request(name(SUBJECT), b"ab".to_vec())),
            Ok(Got::Request(name(SUBJECT), b"ef".to_vec())),
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
    assert_eq!(home.served, [Err(serve::Error::Unfinished { remain: 6 })]);
    assert_eq!(
        serve::Error::Unfinished { remain: 6 }.to_string(),
        "the program finished a request with 6 bytes of its body unsent"
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

/// A request on the hello stream breaks the order of the stream, so the session
/// closes.
#[test]
fn closes_the_session_on_a_request_on_the_hello_stream() {
    let home = session(95, true, |mut agent| async move {
        agent.hello.challenge().await;
        let request = Request {
            length: 0,
            signature: [0; 64],
        };
        let mut out = [0; Request::LEN];
        request.encode(&mut out);
        agent.hello.send(&out).await;
        assert_eq!(agent.closed().await, closed_with(MALFORMED));
    });
    assert_eq!(
        home.served,
        [Err(serve::Error::Message(wire::hub::Error::Mixed {
            kind: 5
        }))]
    );
}

/// A hello whose expiry is past the cap is refused, so a renewal cannot hold a
/// session for longer than the cap.
#[test]
fn refuses_a_hello_past_the_cap() {
    let home = session(96, true, |mut agent| async move {
        let challenge = agent.hello.challenge().await;
        let mut hello = Agent::hello(challenge);
        hello.expires = challenge.now.latest + access::proof::CAP + Span::SECOND;
        agent.send_hello(hello, &AGENT).await;
        assert_eq!(agent.closed().await, closed_with(CAPPED));
    });
    assert!(
        matches!(
            home.served.as_slice(),
            [Err(serve::Error::Access(Refusal::Capped { .. }))]
        ),
        "{:?}",
        home.served
    );
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
