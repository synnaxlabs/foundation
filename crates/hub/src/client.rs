//! The client session of a program: it dials one node as a program, keeps its hello
//! admitted, and sends signed requests.

mod turn;

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use transport::stream::{Receiver, Sender};
use transport::{Address, Class, Code};
use types::connection;
use types::ed25519::{Pair, PrivateKey, PublicKey};
use types::hello::Hello;
use types::name::Name;
use types::time::{Monotonic, Span};
use wire::Protocol;
use wire::header::MALFORMED;
use wire::hub::client::{
    BODY_BYTES_MAX, Challenge, Refusal, Request, Response, Signed,
};

use turn::{Taken, Turn};

/// How long each hello of a [`Client`] lives. The client renews it at half its life.
pub const LIFE: Span = Span::from_nanos(10 * Span::MINUTE.nanos());

const HALF: Span = Span::from_nanos(LIFE.nanos() / 2);

/// How long a renewal waits for a block before it tries again.
const RETRY: Span = Span::SECOND;

/// What a [`Client`] is given.
#[derive(Debug)]
pub struct Config {
    /// The key of the node to connect to, which each hello names as `via`.
    pub via: types::node::Key,
    /// The public key that the node proves in the handshake.
    pub node: PublicKey,
    /// Where the node listens, tried as `transport::Client::dial` tries them.
    pub addresses: Vec<Address>,
    /// The subject that the program acts as.
    pub subject: Name,
    /// A private key that the spec lists for `subject`.
    pub key: PrivateKey,
    /// The monotonic clock on which the client renews the hello.
    pub clock: env::clock::Clock,
    /// The source of the connection key.
    pub entropy: env::entropy::Entropy,
    /// Where the client spawns the task that renews the hello.
    pub tasks: env::tasks::Tasks,
    /// The pool that the client sends from. It can be the pool of the program's
    /// transport. Each message that the client sends takes a block from it, which the
    /// stream holds until the node has it. So a request needs room for the bytes
    /// that the session holds in flight, up to the node's
    /// [`transport::Config::window_bytes`], plus the next chunk of the body, which
    /// takes its block before it waits for the window, and the blocks of the header
    /// and the request. A chunk is at most [`block::Pool::largest`]. A request gives
    /// [`Error::Pool`] when the pool has no room.
    pub pool: Rc<block::Pool>,
}

/// A program's session with one node, as one subject. It keeps the subject's hello
/// admitted, and sends one request at a time. It stays on the thread that made it. A
/// clone is the same session. When the last clone drops, the client closes the
/// session with `Code(0)`.
#[derive(Clone, Debug)]
pub struct Client(Rc<Handle>);

/// Closes the session when the last clone of a [`Client`] drops. The renewal task
/// holds only the [`Shared`] part.
#[derive(Debug)]
struct Handle(Rc<Shared>);

impl Drop for Handle {
    fn drop(&mut self) {
        self.0.session.close(Code(0));
    }
}

/// What the requests and the renewal task share.
#[derive(Debug)]
struct Shared {
    session: transport::Session,
    pair: Pair,
    subject: Name,
    via: types::node::Key,
    connection: connection::Key,
    clock: env::clock::Clock,
    tasks: env::tasks::Tasks,
    pool: Rc<block::Pool>,
    turn: Rc<Turn>,
    /// The error that ended the renewal.
    ended: RefCell<Option<Error>>,
}

impl Client {
    /// Dials the node on `transport` as a program, and waits until the node admits
    /// the subject's first hello. A task on `config.tasks` then renews the hello at
    /// half of [`LIFE`] until the session ends.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] for a refused hello, [`Error::Transport`] when the dial or
    /// the session failed, [`Error::Message`] for a challenge that `wire` refuses,
    /// [`Error::Unanswered`] when the node finished the hello stream with no
    /// challenge, and [`Error::Pool`] when the pool has no block for the hello.
    pub async fn connect(
        transport: &transport::Client,
        config: Config,
    ) -> Result<Self, Error> {
        let Config {
            via,
            node,
            addresses,
            subject,
            key,
            clock,
            entropy,
            tasks,
            pool,
        } = config;
        let session = transport.dial(node, &addresses).await?;
        let (mut sender, mut receiver) = session.open(Class::Complete).await?;
        let mut connection = [0; 16];
        entropy.fill(&mut connection);
        let shared = Rc::new(Shared {
            session,
            pair: Pair::new(&key),
            subject,
            via,
            connection: connection::Key(connection),
            clock,
            tasks: tasks.clone(),
            pool,
            turn: Rc::default(),
            ended: RefCell::new(None),
        });
        shared
            .send(&mut sender, &wire::header::encode(Protocol::Hub))
            .await?;
        let first = shared.challenge(&mut receiver).await?;
        let next = shared.hello(&mut sender, &mut receiver, first).await?;
        tasks.spawn(renew(Rc::clone(&shared), sender, receiver, next));
        Ok(Self(Rc::new(Handle(shared))))
    }

    /// Sends `body` as a request signed with the subject's key, and gives the body of
    /// the response. Requests of one client go one at a time, in the order they
    /// began. A request dropped before its response began keeps the turn until the
    /// response begins or the stream ends, because the node holds it open until then.
    ///
    /// # Errors
    ///
    /// [`Error::Body`] when `body` is over `BODY_BYTES_MAX`, with nothing sent. The
    /// error that ended the renewal, once the renewal ended. Else [`Error::Refused`]
    /// when the node stopped the request or closed the session with a refusal,
    /// [`Error::Transport`] when the stream or the session failed;
    /// [`Error::Message`] for a response that `wire` refuses or that ends early;
    /// [`Error::Unanswered`] when the node finished the stream with no response;
    /// [`Error::Pool`] when the pool has no block for a message.
    pub async fn request(&self, body: &[u8]) -> Result<Vec<u8>, Error> {
        let shared = &self.0.0;
        let length = u64::try_from(body.len())
            .ok()
            .filter(|&length| length <= BODY_BYTES_MAX)
            .ok_or(Error::Body { length: body.len() })?;
        let mut open = Open {
            taken: Some(shared.turn.take().await),
            receiver: None,
            begun: false,
            tasks: &shared.tasks,
        };
        if let Some(error) = shared.ended.borrow().clone() {
            return Err(error);
        }
        let (mut sender, receiver) = shared.session.open(Class::Complete).await?;
        let receiver = open.receiver.insert(receiver);
        shared
            .send(&mut sender, &wire::header::encode(Protocol::Hub))
            .await?;
        let signature = shared
            .pair
            .sign(&access::proof::request(shared.connection, body));
        let mut message = shared.pool.alloc(Request::LEN)?;
        Request { length, signature }.encode(&mut message);
        sender.send(message.freeze()).await?;
        let most = sender.bytes_max().min(shared.pool.largest());
        for chunk in body.chunks(most) {
            shared.send(&mut sender, chunk).await?;
        }
        sender.finish()?;
        let first = receiver.recv().await;
        // Only after `recv`: a drop during it waits for the node to free the request.
        open.begun = true;
        let Some(message) = first? else {
            return Err(Error::Unanswered);
        };
        let mut rest = Response::decode(&message)?.body();
        let mut reply = Vec::with_capacity(rest.remain());
        while rest.remain() > 0 {
            let Some(message) = receiver.recv().await? else {
                break;
            };
            reply.extend_from_slice(rest.take(&message)?);
        }
        rest.end()?;
        Ok(reply)
    }
}

impl Shared {
    async fn send(&self, sender: &mut Sender, bytes: &[u8]) -> Result<(), Error> {
        sender.send(self.pool.copy(bytes)?).await?;
        Ok(())
    }

    /// The next challenge, and when it came.
    async fn challenge(
        &self,
        receiver: &mut Receiver,
    ) -> Result<(Challenge, Monotonic), Error> {
        let message = receiver.recv().await?.ok_or(Error::Unanswered)?;
        Ok((Challenge::decode(&message)?, self.clock.now()))
    }

    /// Sends a hello that answers `challenge`, and gives the next challenge, which the
    /// node sends once it admits the hello.
    async fn hello(
        &self,
        sender: &mut Sender,
        receiver: &mut Receiver,
        (challenge, came): (Challenge, Monotonic),
    ) -> Result<(Challenge, Monotonic), Error> {
        // The mesh time of the challenge is as old as the challenge, so the expiry
        // adds the time since it came.
        let hello = Hello {
            subject: self.subject.clone(),
            key: self.pair.public(),
            via: self.via,
            connection: self.connection,
            nonce: challenge.nonce,
            expires: challenge.now.latest + (self.clock.now() - came) + LIFE,
        };
        let signature = self.pair.sign(&access::proof::hello(&hello));
        let signed = Signed { hello, signature };
        let mut message = self.pool.alloc(signed.encoded_len())?;
        signed.encode(&mut message);
        sender.send(message.freeze()).await?;
        self.challenge(receiver).await
    }
}

/// Renews the hello at half of [`LIFE`] after each admission, until the session
/// closes or a renewal fails. A renewal with no block tries again after [`RETRY`],
/// and the node closes the session if the hello expires first. Keeps the error that
/// ended it, and closes the session.
async fn renew(
    shared: Rc<Shared>,
    mut sender: Sender,
    mut receiver: Receiver,
    mut last: (Challenge, Monotonic),
) {
    let mut at = last.1 + HALF;
    let error = loop {
        let closed = {
            let mut closed = pin!(shared.session.closed());
            let mut half = pin!(shared.clock.sleep_until(at));
            poll_fn(|cx| match closed.as_mut().poll(cx) {
                Poll::Ready(error) => Poll::Ready(Some(error)),
                Poll::Pending => half.as_mut().poll(cx).map(|()| None),
            })
            .await
        };
        if let Some(error) = closed {
            break Error::from(error);
        }
        match shared.hello(&mut sender, &mut receiver, last).await {
            Ok(next) => {
                last = next;
                at = last.1 + HALF;
            }
            Err(Error::Pool(_)) => at = shared.clock.now() + RETRY,
            Err(error) => break error,
        }
    };
    let code = match error {
        Error::Message(_) => MALFORMED,
        _ => 0,
    };
    shared.ended.replace(Some(error));
    shared.session.close(Code(code));
}

/// A request, which keeps the turn until the node frees it.
struct Open<'a> {
    /// `Some` until the drop.
    taken: Option<Taken>,
    /// The receiver of the request once it opens. Until its response begins, the
    /// node holds the request open, so a drop gives the turn only once the first
    /// message or the end of the stream comes.
    receiver: Option<Receiver>,
    /// Whether the response began, so the node freed the request.
    begun: bool,
    tasks: &'a env::tasks::Tasks,
}

impl Drop for Open<'_> {
    fn drop(&mut self) {
        let taken = self.taken.take();
        match self.receiver.take() {
            Some(mut receiver) if !self.begun => self.tasks.spawn(async move {
                drop(receiver.recv().await);
                drop(taken);
            }),
            _ => drop(taken),
        }
    }
}

/// Why a [`Client`] call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The body is over `BODY_BYTES_MAX`. Nothing was sent.
    Body {
        /// The bytes of the body.
        length: usize,
    },
    /// The node stopped a stream or closed the session with a code of `Refusal`.
    Refused(Refusal),
    /// The dial, a stream, or the session failed with no code of `Refusal`: also a
    /// close with 0 or with a code outside the set.
    Transport(transport::Error),
    /// The node sent a message that `wire` refuses, or a body that ended early.
    Message(wire::hub::Error),
    /// The node finished a stream with no answer: a request with no response, or the
    /// hello stream with no challenge.
    Unanswered,
    /// The pool had no block for a message to send.
    Pool(block::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Body { length } => write!(
                f,
                "the body has {length} bytes, over the {BODY_BYTES_MAX} that a request \
                 holds"
            ),
            Self::Refused(refusal) => write!(f, "{refusal}"),
            Self::Transport(error) => write!(f, "the session failed: {error}"),
            Self::Message(error) => {
                write!(f, "the node sent a message that is not valid: {error}")
            }
            Self::Unanswered => {
                f.write_str("the node finished a stream with no answer")
            }
            Self::Pool(error) => {
                write!(f, "the pool had no block for a message: {error}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<transport::Error> for Error {
    fn from(error: transport::Error) -> Self {
        match error {
            transport::Error::Reset { code }
            | transport::Error::Stopped { code }
            | transport::Error::PeerClosed { code } => {
                Refusal::from_code(code.0).map_or(Self::Transport(error), Self::Refused)
            }
            error => Self::Transport(error),
        }
    }
}

impl From<wire::hub::Error> for Error {
    fn from(error: wire::hub::Error) -> Self {
        Self::Message(error)
    }
}

impl From<block::Error> for Error {
    fn from(error: block::Error) -> Self {
        Self::Pool(error)
    }
}
