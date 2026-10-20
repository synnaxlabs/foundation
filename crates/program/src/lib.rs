//! The session of a program: it dials one node as a program, keeps its hello
//! admitted, and sends signed requests, reads, and writes.

mod read;
mod turn;
pub mod write;

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
use types::frame::Form;
use types::hello::Hello;
use types::name::{Name, Selector};
use types::time::{Monotonic, Span, Stamp};
use wire::Protocol;
use wire::header::MALFORMED;
use wire::hub::client::{
    BODY_BYTES_MAX, Challenge, OPEN_BYTES_MAX, Request, Response, Signed,
};

pub use read::Reader;
use turn::{Taken, Turn};
pub use wire::hub::client::{Mode, Place, Refusal};
pub use write::Writer;

/// How long each hello of a [`Session`] lives. The session renews it at half its life.
pub const LIFE: Span = Span::from_nanos(10 * Span::MINUTE.nanos());

const HALF: Span = Span::from_nanos(LIFE.nanos() / 2);

/// How long a message that finds the pool full waits before it tries again.
const RETRY: Span = Span::from_nanos(10 * Span::MILLISECOND.nanos());

/// What a [`Session`] is given.
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
    /// The monotonic clock on which the session renews the hello.
    pub clock: env::clock::Clock,
    /// The source of the connection key.
    pub entropy: env::entropy::Entropy,
    /// Where the session spawns the task that renews the hello.
    pub tasks: env::tasks::Tasks,
    /// The pool that the session sends from. It can be the pool of the program's
    /// transport. Each message that the session sends takes a block from it, which the
    /// stream holds until the node acknowledges it. A message that finds the pool full
    /// waits for a block, so a small pool makes the session slower, not fail. The wait
    /// ends when the session closes.
    pub pool: Rc<block::Pool>,
    /// The pool that the drafts of each writer take their blocks from. A draft holds
    /// its block until it drops, and a draft that finds the pool full makes
    /// `Writer::draft` wait. Give a pool apart from `pool` and from the transport's:
    /// drafts that the program holds there stop the renewal of the hello.
    pub frames: Rc<block::Pool>,
    /// The pool that the frames of each reader take their blocks from, as `frames` is
    /// for drafts. It can be `frames`. Its largest block bounds the encoded bytes of a
    /// frame that a reader reads.
    pub reads: Rc<block::Pool>,
}

/// A program's session with one node, as one subject. It keeps the subject's hello
/// admitted, sends one request at a time, and opens reads and writes beside them. It
/// stays on the thread that made it. A clone is the same session. When the last clone
/// and the last [`Reader`] or [`Writer`] drop, it closes the session with `Code(0)`.
#[derive(Clone, Debug)]
pub struct Session(Rc<Handle>);

/// Closes the session when the last clone of a [`Session`] and the last [`Reader`] or
/// [`Writer`] drop. The renewal task holds only the [`Shared`] part.
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
    frames: Rc<block::Pool>,
    reads: Rc<block::Pool>,
    turn: Rc<Turn>,
    /// The error that ended the renewal.
    ended: RefCell<Option<Error>>,
}

impl Session {
    /// Dials the node on `transport` as a program, and waits until the node admits
    /// the subject's first hello. A task on `config.tasks` then renews the hello at
    /// half of [`LIFE`] until the session ends.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] for a refused hello, [`Error::Transport`] when the dial or
    /// the session failed, [`Error::Message`] for a challenge that `wire` refuses,
    /// [`Error::Unanswered`] when the node finished the hello stream with no
    /// challenge, and [`Error::Pool`] when no block of the pool can hold the header or
    /// the hello.
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
            frames,
            reads,
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
            frames,
            reads,
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

    /// The error that ended the session, or `None` while it runs. The session ends
    /// when a renewal of the hello fails, also when the node or the transport closes
    /// it. A call after the end gives this error.
    #[must_use]
    pub fn ended(&self) -> Option<Error> {
        let shared = &self.0.0;
        // The renewal can run after a call that saw the end of the transport.
        let ended = shared.ended.borrow().clone();
        ended.or_else(|| shared.session.ended().map(Error::from))
    }

    /// Sends `body` as a request signed with the subject's key, and gives the body of
    /// the response. Requests of one session go one at a time, in the order they
    /// began. A request dropped before its response began keeps the turn until the
    /// response begins or the stream ends, because the node holds it open until then.
    ///
    /// # Errors
    ///
    /// [`Error::Body`] when `body` is over `BODY_BYTES_MAX`, with nothing sent. The
    /// error that ended the renewal, when the renewal ended before or during the
    /// request. Else [`Error::Refused`] when the node stopped the request or closed
    /// the session with a refusal, [`Error::Transport`] when the stream or the session
    /// failed; [`Error::Message`] for a response that `wire` refuses or that ends
    /// early; [`Error::Unanswered`] when the node finished the stream with no response.
    pub async fn request(&self, body: &[u8]) -> Result<Vec<u8>, Error> {
        let shared = &self.0.0;
        let length = u64::try_from(body.len())
            .ok()
            .filter(|&length| length <= BODY_BYTES_MAX)
            .ok_or(Error::Body { length: body.len() })?;
        shared
            .exchange(body, length)
            .await
            .map_err(|error| shared.ended.borrow().clone().unwrap_or(error))
    }

    /// Opens a reader of `mode` on the channels that `select` matches, as the
    /// session's subject, and waits until the node opened it. The node resolves the
    /// names and runs the reader at the home of their index. A read takes no turn: it
    /// runs beside the requests.
    ///
    /// # Errors
    ///
    /// [`Error::Open`] when the open of `select` is over `OPEN_BYTES_MAX`, with
    /// nothing sent. The error that ended the renewal, when the renewal ended before
    /// or during the open. Else [`Error::Refused`] when the node stopped the read
    /// (`UNKNOWN` for a selector that matches no channel, `MALFORMED` for channels of
    /// more than one index), [`Error::Transport`], [`Error::Message`] for places that
    /// `wire` refuses or a stream that ends before the last place,
    /// [`Error::Reply`] for a reply that `wire` refuses or a
    /// stream that ends before `Opened`, and [`Error::Pool`].
    pub async fn reader(&self, select: Selector, mode: Mode) -> Result<Reader, Error> {
        let shared = &self.0.0;
        let open = wire::hub::client::Open { mode, select };
        let length = open.encoded_len();
        if u64::try_from(length).map_or(true, |bytes| bytes > OPEN_BYTES_MAX) {
            return Err(Error::Open { length });
        }
        Reader::open(Rc::clone(&self.0), &open)
            .await
            .map_err(|error| shared.ended.borrow().clone().unwrap_or(error))
    }

    /// Opens a writer on `config.channels` as the session's subject, and waits until
    /// the node opened it. The node must be the home of their index. A write takes no
    /// turn: it runs beside the requests and the reads.
    ///
    /// # Errors
    ///
    /// [`Error::Unnamed`] when `config.channels` is empty, and [`Error::Open`] when the
    /// open is over `OPEN_BYTES_MAX`, each with nothing sent. The error that ended the
    /// renewal, when it ended before or during the open. Else [`Error::Refused`] when
    /// the node stopped the write (`UNKNOWN` for a name of no channel, `FAILED` when
    /// the home cannot open the write, `MALFORMED` for channels of more than one index
    /// or a lease that the home refuses, `NOT_HOME` when the node is not the home of
    /// the index, `REFUSED` for a signature that the node refuses, `BUSY` when the node
    /// has no room for the open), [`Error::Transport`], [`Error::Message`] for places
    /// that `wire` refuses or a stream that ends before the last place,
    /// [`Error::Unplaced`] for places that hold no place of a channel of the open, and
    /// [`Error::Pool`] when no block of [`Config::pool`] holds the open, or the head
    /// and an end of each place of a frame, which stops the stream.
    pub async fn writer(&self, config: write::Config) -> Result<Writer, Error> {
        let shared = &self.0.0;
        let write::Config {
            authority,
            lease,
            channels,
        } = config;
        if channels.is_empty() {
            return Err(Error::Unnamed);
        }
        let open = wire::hub::client::write::Open {
            authority,
            lease,
            channels,
        };
        let length = open.encoded_len();
        if u64::try_from(length).map_or(true, |bytes| bytes > OPEN_BYTES_MAX) {
            return Err(Error::Open { length });
        }
        Writer::open(Rc::clone(&self.0), &open)
            .await
            .map_err(|error| shared.ended.borrow().clone().unwrap_or(error))
    }
}

impl Shared {
    /// Takes the turn, sends the request of `body`, whose length is `length`, and
    /// gives the body of the response. Gives each error of [`Session::request`] but
    /// [`Error::Body`], with the error of the close in place of the error that ended
    /// the renewal.
    async fn exchange(&self, body: &[u8], length: u64) -> Result<Vec<u8>, Error> {
        let mut open = Open {
            taken: Some(self.turn.take().await),
            receiver: None,
            begun: false,
            tasks: &self.tasks,
        };
        let (mut sender, receiver) = self.session.open(Class::Complete).await?;
        let receiver = open.receiver.insert(receiver);
        self.send(&mut sender, &wire::header::encode(Protocol::Hub))
            .await?;
        let signature = self.pair.sign(&access::proof::signed(
            access::proof::Kind::Request,
            self.connection,
            body,
        ));
        let mut request = [0; Request::LEN];
        Request { length, signature }.encode(&mut request);
        // Never `Error::Pool`: `connect` took a larger block for its hello.
        self.send(&mut sender, &request).await?;
        self.send_chunks(&mut sender, body).await?;
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

    /// What `take` takes from the pool. While the pool is full, tries again after
    /// [`RETRY`]: the streams give their blocks back as the node acknowledges them,
    /// and frames as the program drops them. Gives [`Error::Pool`] when no block can
    /// hold what `take` asks for, and the error of the close when the session closes
    /// first.
    async fn retry<T>(
        &self,
        mut take: impl FnMut() -> Result<T, block::Error>,
    ) -> Result<T, Error> {
        loop {
            match take() {
                Ok(taken) => return Ok(taken),
                Err(block::Error::Exhausted { .. } | block::Error::Refused { .. }) => {
                    self.sleep_until(self.clock.now() + RETRY).await?;
                }
                Err(error @ block::Error::TooLarge { .. }) => return Err(error.into()),
            }
        }
    }

    /// Sends the header, then the first message of a signed stream, which `first`
    /// writes from the length of `body` and the signature of `kind` over it, then
    /// `body`, whose length the caller capped at `OPEN_BYTES_MAX`.
    async fn open<const LEN: usize>(
        &self,
        sender: &mut Sender,
        kind: access::proof::Kind,
        body: &[u8],
        first: impl FnOnce(u64, [u8; 64], &mut [u8; LEN]),
    ) -> Result<(), Error> {
        let signature =
            self.pair
                .sign(&access::proof::signed(kind, self.connection, body));
        let length =
            u64::try_from(body.len()).expect("invariant: the caller capped it");
        let mut message = [0; LEN];
        first(length, signature, &mut message);
        self.send(sender, &wire::header::encode(Protocol::Hub))
            .await?;
        self.send(sender, &message).await?;
        self.send_chunks(sender, body).await
    }

    /// A block of `len` bytes, as [`Shared::retry`] takes it.
    async fn alloc(&self, len: usize) -> Result<block::Unique, Error> {
        self.retry(|| self.pool.alloc(len)).await
    }

    /// Sends `bytes` on `sender` in a block of [`Shared::alloc`].
    async fn send(&self, sender: &mut Sender, bytes: &[u8]) -> Result<(), Error> {
        let mut block = self.alloc(bytes.len()).await?;
        block.copy_from_slice(bytes);
        sender.send(block.freeze()).await?;
        Ok(())
    }

    /// Sends `body` on `sender` in chunks that fit a message and a block of the pool.
    async fn send_chunks(&self, sender: &mut Sender, body: &[u8]) -> Result<(), Error> {
        let most = sender.bytes_max().min(self.pool.largest());
        for chunk in body.chunks(most) {
            self.send(sender, chunk).await?;
        }
        Ok(())
    }

    /// Sleeps until `at` while the session is open, and gives the error of the close
    /// when the session closes first.
    async fn sleep_until(&self, at: Monotonic) -> Result<(), Error> {
        let mut closed = pin!(self.session.closed());
        let mut sleep = pin!(self.clock.sleep_until(at));
        poll_fn(|cx| match closed.as_mut().poll(cx) {
            Poll::Ready(error) => Poll::Ready(Err(Error::from(error))),
            Poll::Pending => sleep.as_mut().poll(cx).map(Ok),
        })
        .await
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
        let hello = Hello {
            subject: self.subject.clone(),
            key: self.pair.public(),
            via: self.via,
            connection: self.connection,
            nonce: challenge.nonce,
            // Set after the wait for a block, so the wait takes none of the life.
            expires: challenge.now.latest,
        };
        let mut signed = Signed {
            hello,
            signature: [0; 64],
        };
        let mut message = self.alloc(signed.encoded_len()).await?;
        // The mesh time of the challenge is as old as the challenge. It is the node's,
        // so the expiry stops at the end of mesh time.
        signed.hello.expires = challenge
            .now
            .latest
            .checked_add(self.clock.now() - came)
            .and_then(|stamp| stamp.checked_add(LIFE))
            .unwrap_or(Stamp::from_nanos(i64::MAX));
        signed.signature = self.pair.sign(&access::proof::hello(&signed.hello));
        signed.encode(&mut message);
        sender.send(message.freeze()).await?;
        self.challenge(receiver).await
    }
}

/// Renews the hello at half of [`LIFE`] after each admission, until the session
/// closes or a renewal fails. A renewal waits while the pool is full, and the node
/// closes the session if the hello ends first. Keeps the error that ended it, and
/// closes the session.
async fn renew(
    shared: Rc<Shared>,
    mut sender: Sender,
    mut receiver: Receiver,
    mut last: (Challenge, Monotonic),
) {
    let mut at = last.1 + HALF;
    let error = loop {
        if let Err(error) = shared.sleep_until(at).await {
            break error;
        }
        match shared.hello(&mut sender, &mut receiver, last).await {
            Ok(next) => {
                last = next;
                at = last.1 + HALF;
            }
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

/// Ends `stream` after `error`: stops it as `MALFORMED` when the node broke a rule of
/// the wire, else drops it.
fn end((sender, receiver): (Sender, Receiver), error: &Error) {
    if matches!(
        error,
        Error::Message(_) | Error::Reply(_) | Error::Frame(_) | Error::Unplaced(_)
    ) {
        sender.reset(Code(MALFORMED));
        receiver.stop(Code(MALFORMED));
    }
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

/// Why a [`Session`] call failed.
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
    Message(wire::hub::client::Error),
    /// The node finished a stream with no answer: a request with no response, or the
    /// hello stream with no challenge.
    Unanswered,
    /// No block of [`Config::pool`] can hold a message that the session sends, or
    /// the head and an end of each place of a writer, or no block of
    /// [`Config::reads`] can hold a frame of a read, or of [`Config::frames`] a draft
    /// of a writer.
    Pool(block::Error),
    /// The open of a read or a write is over `OPEN_BYTES_MAX`. Nothing was sent.
    Open {
        /// The bytes of the open.
        length: usize,
    },
    /// The node sent a reply of a read or a write that `wire` refuses, or finished
    /// the stream where a reply must come.
    Reply(wire::hub::Error),
    /// The ends of a frame of a read break a rule of the frame layout.
    Frame(types::frame::Error),
    /// The series of [`Writer::draft`] break a rule of the frame layout. Nothing was
    /// taken, and the writer goes on.
    Layout(types::frame::Error),
    /// The config of a stream names no channel. Nothing was sent.
    Unnamed,
    /// The node gave no place for `0`, a channel of the open. The stream stops with
    /// code `MALFORMED`.
    Unplaced(Name),
    /// [`Writer::write`] cannot send a frame: it holds no sample, or it is of
    /// [`write::Time::Node`] and is encoded or holds more than one sample. Nothing was
    /// sent, and the writer goes on.
    Unsent {
        /// The samples of each series of the frame.
        count: u32,
        /// The form of the frame.
        form: Form,
    },
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
                write!(
                    f,
                    "no block of the pool holds a message or a frame: {error}"
                )
            }
            Self::Open { length } => write!(
                f,
                "the open has {length} bytes, over the {OPEN_BYTES_MAX} that an open \
                 holds"
            ),
            Self::Reply(error) => {
                write!(f, "the node sent a reply that is not valid: {error}")
            }
            Self::Frame(error) => {
                write!(f, "the node sent a frame that is not valid: {error}")
            }
            Self::Layout(error) => {
                write!(f, "the series of the draft are not valid: {error}")
            }
            Self::Unnamed => f.write_str("the stream names no channel"),
            Self::Unplaced(name) => write!(
                f,
                "the node gave no place for the channel {name} of the open"
            ),
            Self::Unsent { count: 0, .. } => {
                f.write_str("the frame holds no sample, so it was not sent")
            }
            Self::Unsent { count, form } => write!(
                f,
                "a frame that the node stamps holds 1 sample in the raw form, not \
                 {count} in the {form:?} form, so it was not sent"
            ),
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

impl From<wire::hub::client::Error> for Error {
    fn from(error: wire::hub::client::Error) -> Self {
        Self::Message(error)
    }
}

impl From<block::Error> for Error {
    fn from(error: block::Error) -> Self {
        Self::Pool(error)
    }
}
