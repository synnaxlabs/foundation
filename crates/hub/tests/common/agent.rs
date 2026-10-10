//! The program end of a hub link: an agent that dials a sim node over a real transport,
//! admits a hello, and sends requests.

use transport::stream::{Receiver, Sender};
use transport::{Address, Class, Code, Session};
use types::connection;
use types::ed25519::{Pair, PrivateKey};
use types::hello::Hello;
use types::name::Name;
use types::time::Span;
use wire::Protocol;
use wire::hub::client::{Challenge, Request, Response, Signed};

use crate::net::{HOME, NODE, own_pool, public_key};

pub(crate) const SUBJECT: &str = "ops.agent";
/// Two more subjects that the spec lists with `AGENT`.
pub(crate) const SECOND: &str = "ops.second";
pub(crate) const THIRD: &str = "ops.third";
/// The key that the spec lists for [`SUBJECT`].
pub(crate) const AGENT: PrivateKey = PrivateKey([3; 32]);
pub(crate) const CONNECTION: connection::Key = connection::Key([7; 16]);
/// How long each hello lives, unless a test says otherwise.
pub(crate) const LIFE: Span = Span::from_nanos(60 * Span::SECOND.nanos());

pub(crate) fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
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

pub(crate) fn reset_with(code: u32) -> Result<Option<Vec<u8>>, transport::Error> {
    Err(transport::Error::Reset { code: Code(code) })
}
