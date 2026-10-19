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
use wire::hub::FromHome;
use wire::hub::client::{
    self, Challenge, Mode, Open, Place, Places, Read, Request, Response, Signed, write,
};
use wire::hub::write::{Head, Reply};

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
    /// The mode of the read that the stream sent.
    mode: Option<Mode>,
    /// The open position that `Opened` gave, once [`Stream::opened`] read it.
    pub(crate) live: Option<u64>,
}

impl Stream {
    pub(crate) async fn send(&mut self, bytes: &[u8]) {
        let block = self.pool.copy(bytes).expect("the pool has room");
        self.sender.send(block).await.expect("sends");
    }

    /// Sends a request of `length` bytes, signed over `body`, and `body` in messages
    /// of at most 64 KiB.
    pub(crate) async fn request(&mut self, length: u64, body: &[u8]) {
        let signed =
            access::proof::signed(access::proof::Kind::Request, CONNECTION, body);
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

    /// Sends a read of `open`, signed with `key` as a `kind`, and the open in one
    /// message.
    pub(crate) async fn read(
        &mut self,
        open: &Open,
        kind: access::proof::Kind,
        key: &PrivateKey,
    ) {
        let mut body = vec![0; open.encoded_len()];
        open.encode(&mut body);
        let signed = access::proof::signed(kind, CONNECTION, &body);
        let read = Read {
            length: u64::try_from(body.len()).expect("an open fits a u64"),
            signature: Pair::new(key).sign(&signed),
        };
        let mut out = [0; Read::LEN];
        read.encode(&mut out);
        self.send(&out).await;
        self.send(&body).await;
        self.mode = Some(open.mode);
    }

    /// Sends a write of `open`, signed with `key` as a `kind`, and the open in one
    /// message.
    pub(crate) async fn write(
        &mut self,
        open: &write::Open,
        kind: access::proof::Kind,
        key: &PrivateKey,
    ) {
        let mut body = vec![0; open.encoded_len()];
        open.encode(&mut body);
        let signed = access::proof::signed(kind, CONNECTION, &body);
        let write = write::Write {
            length: u64::try_from(body.len()).expect("an open fits a u64"),
            signature: Pair::new(key).sign(&signed),
        };
        let mut out = [0; write::Write::LEN];
        write.encode(&mut out);
        self.send(&out).await;
        self.send(&body).await;
    }

    /// Reads the start of the write that the stream sent: the places and their run.
    pub(crate) async fn started(&mut self) -> write::Start {
        let mut opening = write::Opening::default();
        loop {
            let message = self.recv().await.expect("receives");
            let message = message.expect("a message before the finish");
            let decoded = opening.decode(Some(&message));
            if let Some(start) = decoded.expect("a message of the run") {
                return start;
            }
        }
    }

    /// Sends one frame of a write: `head`, the run of `ends` in one message, and
    /// `body` in messages of 4 KiB.
    pub(crate) async fn frame(&mut self, head: Head, ends: &[(u32, u32)], body: &[u8]) {
        let mut out = [0; Head::LEN];
        head.encode(&mut out);
        self.send(&out).await;
        let mut out = vec![0; ends.len() * wire::hub::ends::LEN];
        wire::hub::ends::encode(ends.iter().copied(), &mut out);
        self.send(&out).await;
        for part in body.chunks(1 << 12) {
            self.send(part).await;
        }
    }

    /// The next reply of a write.
    pub(crate) async fn reply(&mut self) -> Reply {
        let message = self.recv().await.expect("receives");
        Reply::decode(&message.expect("a reply before the finish")).expect("a reply")
    }

    /// The next message from the node, `None` once it finished.
    pub(crate) async fn recv(&mut self) -> Result<Option<Vec<u8>>, transport::Error> {
        Ok(self.receiver.recv().await?.map(|block| block.to_vec()))
    }

    /// Reads the start of the read that the stream sent: the places, their run, and
    /// `Opened`, and keeps its open position in `live`. Gives the places, the run, and
    /// the decoder of the frames.
    ///
    /// # Panics
    ///
    /// When the stream sent no read.
    pub(crate) async fn opened(&mut self) -> (Places, Vec<Place>, wire::hub::Reader) {
        let mode = self.mode.expect("the stream sent a read");
        let mut start = client::Reader::new(mode);
        let mut next = async || {
            let message = self.recv().await.expect("receives");
            message.expect("a message before the finish")
        };
        let mut decoded = None;
        let client::Start {
            places: run,
            home: mut reader,
            ..
        } = loop {
            if let Some(decoded) = decoded {
                break decoded;
            }
            decoded = start
                .decode(Some(&next().await))
                .expect("a message of the run");
        };
        let places = Places {
            count: u32::try_from(run.len()).expect("the count fits a u32"),
        };
        let Ok(FromHome::Opened { live }) = reader.decode(Some(&next().await)) else {
            panic!("then `Opened`");
        };
        self.live = Some(live);
        (places, run, reader)
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
            mode: None,
            live: None,
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
        self.open_as(Class::Complete).await
    }

    /// Opens a stream of `class` and sends its header.
    pub(crate) async fn open_as(&self, class: Class) -> Stream {
        let mut stream = self.silent_as(class).await;
        stream.send(&wire::header::encode(Protocol::Hub)).await;
        stream
    }

    /// Opens a stream and sends nothing, so the node does not see it yet.
    pub(crate) async fn silent(&self) -> Stream {
        self.silent_as(Class::Complete).await
    }

    async fn silent_as(&self, class: Class) -> Stream {
        let (sender, receiver) = self.session.open(class).await.expect("opens");
        Stream {
            pool: std::rc::Rc::clone(&self.pool),
            sender,
            receiver,
            mode: None,
            live: None,
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

    /// Sends a read of `select` in `mode`, signed by the agent, on a new stream of the
    /// class of `mode`. Leaves the stream open.
    pub(crate) async fn read(&self, mode: Mode, select: &[&str]) -> Stream {
        let class = match mode {
            Mode::Latest => Class::Latest,
            Mode::Complete => Class::Complete,
        };
        let mut stream = self.open_as(class).await;
        let open = Open {
            mode,
            select: types::name::Selector::new(select.iter().copied())
                .expect("a selector"),
        };
        stream.read(&open, access::proof::Kind::Read, &AGENT).await;
        stream
    }

    /// Sends a write of `channels` at authority 1 with no lease, signed by the agent,
    /// on a new `Complete` stream. Leaves the stream open.
    pub(crate) async fn write(&self, channels: &[&str]) -> Stream {
        self.write_as(Class::Complete, channels).await
    }

    /// As [`Agent::write`], on a stream of `class`.
    pub(crate) async fn write_as(&self, class: Class, channels: &[&str]) -> Stream {
        let mut stream = self.open_as(class).await;
        let open = write::Open {
            authority: types::authority::Authority(1),
            lease: None,
            channels: channels.iter().map(|channel| name(channel)).collect(),
        };
        stream
            .write(&open, access::proof::Kind::Write, &AGENT)
            .await;
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
