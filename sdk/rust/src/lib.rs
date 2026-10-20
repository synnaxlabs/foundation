//! The Rust SDK: a program's session with one Foundation node.

use std::fmt;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::Poll;

use env::files::Files;
use env::tasks::{Group, Tasks};
use env::thread::Handle;
use tokio::sync::{oneshot, watch};
use types::ed25519::{PrivateKey, PublicKey};
use types::name::Name;
use types::sample::{Scalar, Type};

pub mod read;
pub mod write;

pub use program::Place;

/// The budget of the pool of the transport and the session. Its largest block holds a
/// datagram.
const POOL: block::Config = match block::Config::new(1 << 21) {
    Ok(config) => config,
    Err(_) => panic!("invariant: the budget fits"),
};

/// The budget of the pool of the drafts of the session's writers. Its largest block,
/// 57,344 bytes, bounds the bytes of one frame of [`write::Writer::write_series`].
const FRAMES: block::Config = match block::Config::new(64 << 10) {
    Ok(config) => config,
    Err(_) => panic!("invariant: the budget fits"),
};

/// The budget of the pool of the frames of the session's readers. Its largest block,
/// 7,340,032 bytes, bounds the encoded bytes of one frame of [`read::Reader::next`].
/// It reserves about 504 MiB of address space, with no memory and no commit charge
/// until a frame comes.
const READS: block::Config = match block::Config::new(8 << 20) {
    Ok(config) => config,
    Err(_) => panic!("invariant: the budget fits"),
};

/// The node that a program reaches: where it listens, and the public key that names
/// it.
#[derive(Clone, Debug)]
pub struct Node {
    /// The UDP addresses where the node listens, dialed in this order with a stagger.
    pub addresses: Vec<SocketAddr>,
    /// The public key that the node proves in the handshake, which each hello names
    /// as the node it goes through.
    pub public_key: PublicKey,
}

impl Node {
    /// The node that runs on the data directory `dir` (`--data` of `foundation
    /// start`), and its first admin as the subject, or `None` when `dir` holds no
    /// `admin.key` that a node wrote. Blocks until the read ends.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with the cause: `NotRunning` also when `dir` or `dir/data` is
    /// not there or is not a directory. Each path of its `Files` cause is relative to
    /// `dir/data`, and `.` is `dir/data` itself. [`Error::Thread`] when a thread that
    /// reads the directory cannot start, and [`Error::Panicked`].
    #[expect(
        clippy::missing_panics_doc,
        clippy::unwrap_in_result,
        reason = "a shard that did not panic sent its read"
    )]
    pub fn local(dir: &Path) -> Result<(Self, Option<Subject>), Error> {
        let threads = os::threads().map_err(|e| thread(&e))?;
        let (disk, files) =
            os::stored(dir, &threads, "foundation-sdk-files").map_err(opened)?;
        let (sender, read) = mpsc::channel();
        let shard = shard(move |_| async move {
            let local = sector::local::read(&Files::new(disk)).await;
            sender
                .send(local)
                .expect("invariant: `local` keeps the receiver");
        })?;
        let joined = shard.join().and(files.join());
        joined.map_err(|_panicked| Error::Panicked)?;
        let local = read
            .try_recv()
            .expect("invariant: a shard that did not panic sent its read")
            .map_err(Error::Local)?;
        let node = Self {
            addresses: vec![local.address],
            public_key: local.public_key,
        };
        let subject = local.admin.map(|private_key| Subject {
            name: spec::founding::admin(),
            private_key,
        });
        Ok((node, subject))
    }

    /// The node that answers first at `addresses`, dialed in this order with a stagger,
    /// with the public key that it proves. Blocks until each address fails, or until a
    /// node answers and the transport drains the close of each session (up to 3 s). A
    /// packet of the node that comes before its drain ends makes the close go out
    /// again. Trust on first use: nothing proves that it is the node the caller means,
    /// so the caller pins the result and gives it to each later [`Client::connect`]. It
    /// sends no hello, so it needs no subject.
    ///
    /// # Errors
    ///
    /// [`Error::Thread`] when the thread that dials cannot start, [`Error::Bind`] when
    /// the local UDP socket cannot bind, [`Error::Session`] with
    /// [`program::Error::Transport`] when the dial or the handshake fails, and
    /// [`Error::Panicked`]. With no address, [`Error::Session`] with an `Unreachable`
    /// of no attempt.
    #[expect(
        clippy::missing_panics_doc,
        clippy::unwrap_in_result,
        reason = "a shard that did not panic sent its key"
    )]
    pub fn answering(addresses: Vec<SocketAddr>) -> Result<Self, Error> {
        let dialed: Vec<_> = addresses
            .iter()
            .copied()
            .map(transport::Address::Udp)
            .collect();
        let (sender, key) = mpsc::channel();
        let shard = shard(move |tasks| async move {
            sender
                .send(answered(&dialed, on_os(tasks)).await)
                .expect("invariant: `answering` keeps the receiver");
        })?;
        shard.join().map_err(|_panicked| Error::Panicked)?;
        let public_key = key
            .try_recv()
            .expect("invariant: a shard that did not panic sent its key")?;
        Ok(Self {
            addresses,
            public_key,
        })
    }
}

/// The config of a transport of the program on the drivers of `os` and `tasks`.
fn on_os(tasks: Tasks) -> transport::client::Config {
    transport::client::Config {
        net: os::net(),
        clock: os::clock(),
        entropy: os::entropy(),
        tasks,
        pool: Rc::new(block::Pool::heap(POOL)),
    }
}

/// The public key of the node that answers first at `addresses`, on a transport of
/// `config`. Closes the session with code 0 and waits until the close drains, so
/// that a packet of the node that comes before its end makes the close go out again.
async fn answered(
    addresses: &[transport::Address],
    config: transport::client::Config,
) -> Result<PublicKey, Error> {
    let group = Group::new(config.tasks);
    let config = transport::client::Config {
        tasks: group.tasks().clone(),
        ..config
    };
    let transport = transport::Client::new(config).map_err(Error::Bind)?;
    let session = transport.dial_any(addresses).await;
    let session = session.map_err(|e| Error::Session(program::Error::Transport(e)))?;
    let transport::Peer::Node(public_key) = session.peer() else {
        unreachable!("invariant: the dialed side always proves a node key");
    };
    session.close(transport::Code(0));
    drop((session, transport));
    group.ended().await;
    Ok(public_key)
}

/// The public key of the node that answers first at `addresses` to a dial of no key
/// on `transport`. Closes the session with code 0 and waits for its end.
async fn proved(
    transport: &transport::Client,
    addresses: &[transport::Address],
) -> Result<PublicKey, transport::Error> {
    let session = transport.dial_any(addresses).await?;
    let transport::Peer::Node(public_key) = session.peer() else {
        unreachable!("invariant: the dialed side always proves a node key");
    };
    session.close(transport::Code(0));
    session.closed().await;
    Ok(public_key)
}

/// The error of [`Node::local`] whose `os::stored` failed.
fn opened(error: os::Error) -> Error {
    match error {
        os::Error::Dir(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Error::Local(sector::local::Error::NotRunning)
        }
        os::Error::Dir(error) => {
            Error::Local(sector::local::Error::Files(env::files::Error::Io {
                path: ".".into(),
                operation: env::files::Operation::Open,
                code: error.raw_os_error().expect(
                    "invariant: each `Dir` of `os::stored` holds the code of the OS",
                ),
            }))
        }
        os::Error::Thread(error) => thread(&error),
        error @ (os::Error::Cores(_) | os::Error::Memory(_) | os::Error::Wall(_)) => {
            unreachable!(
                "invariant: `os::stored` gives only `Dir` and `Thread`, not {error}"
            )
        }
    }
}

/// Starts a shard of `os` that logs to the caller's dispatch, and runs `main` on it.
fn shard<M, F>(main: M) -> Result<Handle, Error>
where
    M: FnOnce(Tasks) -> F + Send + 'static,
    F: Future<Output = ()> + 'static,
{
    let log = tracing::dispatcher::get_default(Clone::clone);
    let shards = os::shards().map_err(|e| thread(&e))?.logged(log);
    let config = env::shards::Config {
        name: "foundation-sdk".into(),
        core: None,
    };
    shards.start(config, main).map_err(|e| thread(&e))
}

/// The [`Error::Thread`] of `error`.
fn thread(error: &dyn fmt::Display) -> Error {
    Error::Thread {
        reason: error.to_string(),
    }
}

/// What a program acts as on a node.
#[derive(Clone, Debug)]
pub struct Subject {
    /// The name of the subject.
    pub name: Name,
    /// A private key that the spec lists for the subject.
    pub private_key: PrivateKey,
}

/// Gives a new Ed25519 private key, from the randomness of the OS.
///
/// # Panics
///
/// When the OS gives no randomness.
#[must_use]
pub fn private_key() -> PrivateKey {
    drawn(&os::entropy())
}

/// A private key of 32 bytes of `entropy`.
fn drawn(entropy: &env::entropy::Entropy) -> PrivateKey {
    let mut bytes = [0; 32];
    entropy.fill(&mut bytes);
    PrivateKey(bytes)
}

/// A program's session with one node. It can move to and be shared between threads:
/// a thread of its own holds the session. Drop ends the session and does not wait:
/// that thread then sends the close and ends. [`Client::close`] waits for both. A
/// panic of the thread after a drop goes to the panic hook of the process.
#[derive(Debug)]
pub struct Client {
    thread: Mutex<Thread>,
    requests: tokio::sync::mpsc::UnboundedSender<Open>,
    /// Set at the close or drop of the client, before its thread ends.
    closed: Arc<AtomicBool>,
}

/// The thread that holds the session, the sender whose drop ends it, and the outcomes
/// that it sends. A drop detaches the thread.
#[derive(Debug)]
struct Thread {
    end: End,
    outcome: mpsc::Receiver<Result<(), Error>>,
    handle: Handle,
}

/// The sender whose drop ends the client's thread. The drop first sets `closed`, so
/// that each call of a writer then gives [`Error::Closed`], not [`Error::Panicked`].
#[derive(Debug)]
struct End {
    closed: Arc<AtomicBool>,
    _sender: oneshot::Sender<()>,
}

impl Drop for End {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl Client {
    /// Dials `node` over UDP, and blocks until it admits the first hello of
    /// `subject`. The client then renews the hello until it closes or drops. The
    /// client's thread logs to the `tracing` dispatch that is the default of the
    /// caller of `connect`.
    ///
    /// # Errors
    ///
    /// [`Error::Thread`] when the client's thread or its runtime cannot start,
    /// [`Error::Bind`] when the local UDP socket cannot bind, [`Error::OtherKey`]
    /// when the node proves another key than `node.public_key`, [`Error::Session`]
    /// when the dial, the handshake, or the hello fails, [`Error::Memory`] when the OS
    /// refuses the address space or the first page of the pool of the frames of the
    /// readers, and [`Error::Panicked`]. A `node` with no address is unreachable
    /// with no attempt.
    pub fn connect(node: &Node, subject: Subject) -> Result<Self, Error> {
        let node = node.clone();
        Self::start(move |tasks, outcome, ended, requests| {
            run(node, subject, tasks, outcome, ended, requests)
        })
    }

    /// Starts the client's thread on a shard of `os` that logs to the caller's
    /// dispatch, runs `main` on it, and blocks until `main` sends its outcome. `main`
    /// takes the requests of each call such as [`Client::open_writer`].
    fn start<M, F>(main: M) -> Result<Self, Error>
    where
        M: FnOnce(
                Tasks,
                mpsc::Sender<Result<(), Error>>,
                oneshot::Receiver<()>,
                Requests,
            ) -> F
            + Send
            + 'static,
        F: Future<Output = ()> + 'static,
    {
        let (outcomes, outcome) = mpsc::channel();
        let (sender, ended) = oneshot::channel();
        let closed = Arc::new(AtomicBool::new(false));
        let end = End {
            closed: Arc::clone(&closed),
            _sender: sender,
        };
        let (requests, opened) = tokio::sync::mpsc::unbounded_channel();
        let handle = shard(move |tasks| main(tasks, outcomes, ended, opened))?;
        #[expect(
            clippy::disallowed_methods,
            reason = "connect blocks its caller, which may run a Tokio runtime"
        )]
        let connected = outcome.recv();
        let thread = Thread {
            end,
            outcome,
            handle,
        };
        match connected {
            Ok(Ok(())) => Ok(Self {
                thread: Mutex::new(thread),
                requests,
                closed,
            }),
            Ok(Err(error)) => thread.end().and(Err(error)),
            Err(_) => {
                thread.end()?;
                unreachable!("a thread that sent no outcome panicked")
            }
        }
    }
}

impl Client {
    /// Opens a writer on `config.channels` at the client's node, and blocks until the
    /// node opened it. The node must be the home of their index. Each writer holds a
    /// stream of the session, and the node bounds the streams of a session (64 by
    /// default, one of them the hello): past that bound, the open waits until another
    /// stream of the session ends, such as at the close or drop of a writer.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] when the client's session ended at a close or a drop, and
    /// [`Error::Panicked`] when the client's thread panicked. [`Error::Session`] with
    /// the error that ended the session when it failed. Else [`Error::Stream`] with
    /// each error of `program::Session::writer`, such as `Refused(Refusal::Unknown)`
    /// for a name of no channel, `Refused(Refusal::NotHome)` when the node is not the
    /// home of their index, and `Refused(Refusal::Malformed)` for channels of more
    /// than one index, each a `program::Refusal`.
    pub fn open_writer(&self, config: write::Config) -> Result<write::Writer, Error> {
        write::Writer::open(&self.requests, &self.closed, config)
    }

    /// Opens a reader on `config.channels` at the client's node, and blocks until the
    /// node opened it. The node must be the home of their index. Each reader holds a
    /// stream of the session, and the node bounds the streams of a session (64 by
    /// default, one of them the hello): past that bound, the open waits until another
    /// stream of the session ends, such as at the drop of a reader. A frame of more
    /// than 7,340,032 bytes, encoded, ends the reader with
    /// `Stream(Pool(TooLarge { .. }))`.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] when the client's session ended at a close or a drop, and
    /// [`Error::Panicked`] when the client's thread panicked. [`Error::Session`] with
    /// the error that ended the session when it failed. Else [`Error::Stream`] with
    /// each error of `program::Session::reader`, such as `Refused(Refusal::NotHome)`
    /// when the node is not the home of their index, and `Refused(Refusal::Malformed)`
    /// for channels of more than one index, each a `program::Refusal`. Also
    /// `Refused(Refusal::Unknown)` for a name of no channel: the node gives it when no
    /// name is of a channel, and the client when only some are. `Unnamed` when
    /// `config.channels` is empty, with nothing sent.
    pub fn open_reader(&self, config: read::Config) -> Result<read::Reader, Error> {
        read::Reader::open(&self.requests, &self.closed, config)
    }

    /// Closes the session, and blocks until the close goes out, the transport drains
    /// each close (such as that of a dial that lost the stagger to another address,
    /// up to 3 s), and the client's thread ends.
    ///
    /// # Errors
    ///
    /// [`Error::Session`] with the cause when the session ended before the close, and
    /// [`Error::Panicked`].
    #[expect(
        clippy::missing_panics_doc,
        clippy::unwrap_in_result,
        reason = "no call locks the thread, so it is never poisoned"
    )]
    pub fn close(self) -> Result<(), Error> {
        self.thread
            .into_inner()
            .expect("invariant: no call locks the thread of a client")
            .end()
    }
}

impl Thread {
    /// Ends the thread and waits for it. Gives the error that it sent after the
    /// outcome that `connect` read, if any.
    fn end(self) -> Result<(), Error> {
        drop(self.end);
        self.handle.join().map_err(|_panicked| Error::Panicked)?;
        self.outcome.try_recv().unwrap_or(Ok(()))
    }
}

/// Why [`Node::local`] gave no node, [`Client::connect`] gave no client, a call of a
/// client, of a [`write::Writer`], or of a [`read::Reader`] failed, or the session
/// ended before [`Client::close`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A thread of the client or of [`Node::local`], or its runtime, cannot start.
    Thread {
        /// The text of the error of `env` or `os`.
        reason: String,
    },
    /// The local UDP socket cannot bind.
    Bind(transport::Error),
    /// The OS gave no address space for the pool of the frames of the client's
    /// readers, or no memory for its first page.
    Memory(os::memory::Error),
    /// The session failed before the node admitted the hello, or ended before the
    /// close.
    Session(program::Error),
    /// The node proves `proved`, not the pinned `pinned`: an address refused the
    /// pinned key, and a dial of no key there proves `proved`.
    OtherKey {
        /// The public key of the [`Node`].
        pinned: PublicKey,
        /// The public key that the node proves.
        proved: PublicKey,
    },
    /// The node refused the open of a stream, or a call of a writer or a reader
    /// failed. The session goes on. After `Layout`, `Unsent`, and `Pool` of a writer,
    /// nothing was sent and the writer goes on. Each other error of a writer, and each
    /// error of a reader, ended it, and each later call of it gives it again.
    Stream(program::Error),
    /// The data directory of [`Node::local`] names no node to reach, as the cause says.
    Local(sector::local::Error),
    /// The client's thread panicked, and the panic hook of the process got its
    /// message. Only a build where panics unwind gives it: with `panic = "abort"`, the
    /// panic ends the process. Each call of a writer or a reader of the client gives it
    /// after the panic, until the client closes or drops.
    Panicked,
    /// A value of a write is not of the type of its place. The write sent nothing.
    Value {
        /// The place that the value names.
        place: usize,
        /// The scalar of the value.
        scalar: Scalar,
        /// The type of the place.
        data_type: Type,
    },
    /// The client's session ended at [`Client::close`] or a drop of the client.
    Closed,
    /// A series of a read does not decode. The reader ended.
    Decode {
        /// The place of the series.
        place: usize,
        /// Why it does not decode.
        cause: codec::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Thread { reason } => {
                write!(f, "cannot start the client thread: {reason}")
            }
            Self::Bind(error) => write!(f, "cannot bind the local UDP socket: {error}"),
            Self::Memory(os::memory::Error::Reserve { len, code }) => write!(
                f,
                "the client cannot reserve {len} bytes of address space for the frames \
                 of its readers: {}; raise the address-space limit of the process",
                std::io::Error::from_raw_os_error(*code)
            ),
            Self::Memory(os::memory::Error::Refused) => write!(
                f,
                "the OS refused memory for the frames of the client's readers; free \
                 memory or raise the commit limit of the system"
            ),
            Self::Session(error) | Self::Stream(error) => write!(f, "{error}"),
            Self::OtherKey { pinned, proved } => write!(
                f,
                "the node proves public key {proved}, not the pinned {pinned}"
            ),
            Self::Local(error) => write!(f, "{error}"),
            Self::Panicked => write!(f, "the client thread panicked"),
            Self::Value {
                place,
                scalar,
                data_type,
            } => {
                let scalar = Type::Scalar(*scalar);
                write!(f, "place {place} holds {data_type}, not {scalar}")
            }
            Self::Closed => write!(f, "the client is closed"),
            Self::Decode { place, cause } => {
                write!(f, "the series of place {place} does not decode: {cause}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// The requests of the calls of a client, which its thread serves.
type Requests = tokio::sync::mpsc::UnboundedReceiver<Open>;

/// The open of a stream, which the client's thread serves in a task of its own.
#[derive(Debug)]
enum Open {
    Write(write::Open),
    Read(read::Open),
}

/// Blocks for the answer of the client's thread, or the error of [`lost`] when the
/// thread dropped its sender.
fn wait<T>(answers: &mpsc::Receiver<T>, closed: &AtomicBool) -> Result<T, Error> {
    #[expect(
        clippy::disallowed_methods,
        reason = "a call of a stream blocks its caller, which may run a Tokio runtime"
    )]
    let answer = answers.recv();
    answer.map_err(|_ended| lost(closed))
}

/// The error of a call whose task ended: [`Error::Closed`] when the client closed or
/// dropped (`closed`), else [`Error::Panicked`], as only a panic ends the task first.
fn lost(closed: &AtomicBool) -> Error {
    if closed.load(Ordering::Acquire) {
        Error::Closed
    } else {
        Error::Panicked
    }
}

/// Sends `call` to the task of a stream.
///
/// # Errors
///
/// The error of [`lost`] when the task ended.
///
/// # Panics
///
/// When `calls` is full: each call takes its answer before the next, and no call
/// follows a lost answer.
fn send<T>(
    calls: &tokio::sync::mpsc::Sender<T>,
    call: T,
    closed: &AtomicBool,
) -> Result<(), Error> {
    use tokio::sync::mpsc::error::TrySendError;
    calls.try_send(call).map_err(|error| match error {
        TrySendError::Closed(_) => lost(closed),
        TrySendError::Full(_) => {
            unreachable!("invariant: each call takes its answer before the next")
        }
    })
}

/// Runs `served`, the task of a stream, until it ends or `closed` changes. The sender
/// of `closed` drops at the close of the client, so each stream drops before the
/// session closes.
async fn serve(served: impl Future<Output = ()>, mut closed: watch::Receiver<()>) {
    let mut served = pin!(served);
    let mut changed = pin!(closed.changed());
    poll_fn(|cx| {
        if changed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        served.as_mut().poll(cx)
    })
    .await;
}

/// [`Error::Session`] when `session` ended, else [`Error::Stream`] with `error`.
fn failed(session: &program::Session, error: program::Error) -> Error {
    match session.ended() {
        Some(cause) => Error::Session(cause),
        None => Error::Stream(error),
    }
}

/// The places of a stream of `names` in `entries`, the places of the node, whose index
/// is entry `index`: place `n` is `names[n]`, then the index when `names` does not name
/// it. Each place names the place of the index as its index. Gives the entry of each
/// place, the places, and the place of the index, or `None` when a name is no entry.
fn placed(
    names: &[Name],
    entries: &[Place],
    index: usize,
) -> Option<(Vec<usize>, Vec<Place>, usize)> {
    let mut each = names
        .iter()
        .map(|name| entries.iter().position(|entry| entry.name == *name))
        .collect::<Option<Vec<_>>>()?;
    let at = each
        .iter()
        .position(|&entry| entry == index)
        .unwrap_or_else(|| {
            each.push(index);
            each.len() - 1
        });
    let place = |&entry: &usize| Place {
        index: at,
        ..entries[entry].clone()
    };
    let places = each.iter().map(place).collect();
    Some((each, places, at))
}

/// The main function of the client's thread: connects, sends the outcome, and holds
/// the session and serves each of `requests` until `ended` completes. It then ends
/// each writer, sends the error that ended the session before `ended`, if any, and
/// waits for each task, so that the close of the session goes out.
async fn run(
    node: Node,
    subject: Subject,
    tasks: Tasks,
    outcome: mpsc::Sender<Result<(), Error>>,
    ended: oneshot::Receiver<()>,
    requests: Requests,
) {
    let group = Group::new(tasks);
    match connect(node, subject, group.tasks().clone()).await {
        Ok((session, bytes_max)) => {
            drop(outcome.send(Ok(())));
            // `_close` drops before the wait for the tasks, which ends each writer.
            let (_close, closed) = watch::channel(());
            let tasks = group.tasks();
            served(&session, bytes_max, tasks, ended, requests, &closed).await;
            if let Some(error) = session.ended() {
                drop(outcome.send(Err(Error::Session(error))));
            }
        }
        Err(error) => drop(outcome.send(Err(error))),
    }
    group.ended().await;
}

/// Serves each of `requests` in a task on `tasks` until `ended` completes or the
/// client dropped. Each task ends when `closed` changes. `bytes_max` is the largest
/// block of the session's pool of frames.
async fn served(
    session: &program::Session,
    bytes_max: usize,
    tasks: &Tasks,
    mut ended: oneshot::Receiver<()>,
    mut requests: Requests,
    closed: &watch::Receiver<()>,
) {
    while let Some(open) = poll_fn(|cx| {
        if Pin::new(&mut ended).poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        requests.poll_recv(cx)
    })
    .await
    {
        let (session, closed) = (session.clone(), closed.clone());
        match open {
            Open::Write(open) => {
                let served = write::served(session, bytes_max, open);
                tasks.spawn(serve(served, closed));
            }
            Open::Read(open) => {
                tasks.spawn(serve(read::served(session, open), closed));
            }
        }
    }
}

/// A session with `node` as `subject`, on the drivers of `os` and `tasks`, and the
/// largest block of its pool of frames.
async fn connect(
    node: Node,
    subject: Subject,
    tasks: Tasks,
) -> Result<(program::Session, usize), Error> {
    let frames = Rc::new(block::Pool::heap(FRAMES));
    let reads = reads().map_err(Error::Memory)?;
    let pinned = node.public_key;
    let on = on_os(tasks.clone());
    let (clock, entropy, pool) =
        (on.clock.clone(), on.entropy.clone(), Rc::clone(&on.pool));
    let transport = transport::Client::new(on).map_err(Error::Bind)?;
    let config = program::Config {
        node: node.public_key,
        addresses: node
            .addresses
            .into_iter()
            .map(transport::Address::Udp)
            .collect(),
        subject: subject.name,
        key: subject.private_key,
        clock,
        entropy,
        tasks,
        pool,
        frames: Rc::clone(&frames),
        reads: Rc::new(reads),
    };
    match program::Session::connect(&transport, config).await {
        Ok(session) => Ok((session, frames.largest())),
        Err(error) => Err(other_key(&transport, pinned, error).await),
    }
}

/// The error of a connect to the node of `pinned` that failed with `error`:
/// [`Error::OtherKey`] when a dial of no key on `transport`, to each address that
/// refused `pinned`, proves another key. Else `error`. A node that is down refuses no
/// key, so no address of it gets a second dial.
async fn other_key(
    transport: &transport::Client,
    pinned: PublicKey,
    error: program::Error,
) -> Error {
    let program::Error::Transport(transport::Error::Unreachable { attempts, .. }) =
        &error
    else {
        return Error::Session(error);
    };
    let refused: Vec<_> = attempts
        .iter()
        .filter(|(_, cause)| matches!(cause, transport::Error::Authentication { .. }))
        .map(|&(address, _)| address)
        .collect();
    match proved(transport, &refused).await {
        Ok(proved) if proved != pinned => Error::OtherKey { pinned, proved },
        _ => Error::Session(error),
    }
}

/// The pool of the frames of the readers of a session. It gives back the pages of the
/// sizes that it no longer uses when another size needs them, so on Linux it keeps at
/// most [`READS`] in memory, and up to two partial pages more of each size. On macOS,
/// the pages of the free blocks of a size in use stay in memory until the OS needs
/// them.
fn reads() -> Result<block::Pool, os::memory::Error> {
    let memory = os::memory::Memory::new(READS.reservation())?;
    Ok(block::Pool::new(READS, memory))
}

#[cfg(test)]
mod tests;
