//! The Rust SDK: a program's session with one Foundation node.

use std::fmt;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Mutex, mpsc};

use env::tasks::{Group, Tasks};
use env::thread::Handle;
use tokio::sync::oneshot;
use types::ed25519::{PrivateKey, PublicKey};
use types::name::Name;

/// The budget of the pool of the transport and the session. Its largest block holds a
/// datagram.
const POOL: block::Config = match block::Config::new(1 << 21) {
    Ok(config) => config,
    Err(_) => panic!("invariant: the budget fits"),
};

/// The budget of the pool of the frames of the session's reads.
const FRAMES: block::Config = match block::Config::new(64 << 10) {
    Ok(config) => config,
    Err(_) => panic!("invariant: the budget fits"),
};

/// The node that a program reaches: where it listens, and the keys that name it.
#[derive(Clone, Debug)]
pub struct Node {
    /// The UDP addresses where the node listens, dialed in this order with a stagger.
    pub addresses: Vec<SocketAddr>,
    /// The key of the node, which each hello names as the node it goes through.
    pub key: types::node::Key,
    /// The public key that the node proves in the handshake.
    pub public_key: PublicKey,
}

/// What a program acts as on a node.
#[derive(Clone, Debug)]
pub struct Subject {
    /// The name of the subject.
    pub name: Name,
    /// A private key that the spec lists for the subject.
    pub private_key: PrivateKey,
}

/// A program's session with one node. It can move to and be shared between threads:
/// a thread of its own holds the session. Drop ends the session and does not wait:
/// that thread then sends the close and ends. [`Client::close`] waits for both. A
/// panic of the thread after a drop goes to the panic hook of the process.
#[derive(Debug)]
pub struct Client(Mutex<Thread>);

/// The thread that holds the session, the sender whose drop ends it, and the outcomes
/// that it sends. A drop detaches the thread.
#[derive(Debug)]
struct Thread {
    end: oneshot::Sender<()>,
    outcome: mpsc::Receiver<Result<(), Error>>,
    handle: Handle,
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
    /// [`Error::Bind`] when the local UDP socket cannot bind, [`Error::Session`]
    /// when the dial, the handshake, or the hello fails, and [`Error::Panicked`]. A
    /// `node` with no address is unreachable with no attempt.
    pub fn connect(node: &Node, subject: Subject) -> Result<Self, Error> {
        let node = node.clone();
        Self::start(move |tasks, outcome, ended| {
            run(node, subject, tasks, outcome, ended)
        })
    }

    /// Starts the client's thread on a shard of `os` that logs to the caller's
    /// dispatch, runs `main` on it, and blocks until `main` sends its outcome.
    fn start<M, F>(main: M) -> Result<Self, Error>
    where
        M: FnOnce(Tasks, mpsc::Sender<Result<(), Error>>, oneshot::Receiver<()>) -> F
            + Send
            + 'static,
        F: Future<Output = ()> + 'static,
    {
        let thread = |e: &dyn fmt::Display| Error::Thread {
            reason: e.to_string(),
        };
        let log = tracing::dispatcher::get_default(Clone::clone);
        let shards = os::shards().map_err(|e| thread(&e))?.logged(log);
        let (sender, outcome) = mpsc::channel();
        let (end, ended) = oneshot::channel();
        let config = env::shards::Config {
            name: "foundation-sdk".into(),
            core: None,
        };
        let handle = shards
            .start(config, move |tasks| main(tasks, sender, ended))
            .map_err(|e| thread(&e))?;
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
            Ok(Ok(())) => Ok(Self(Mutex::new(thread))),
            Ok(Err(error)) => thread.end().and(Err(error)),
            Err(_) => {
                thread.end()?;
                unreachable!("a thread that sent no outcome panicked")
            }
        }
    }
}

impl Client {
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
        self.0
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

/// Why [`Client::connect`] gave no client, or why the session ended before
/// [`Client::close`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The client's thread or its runtime cannot start.
    Thread {
        /// The text of the error of `env` or `os`.
        reason: String,
    },
    /// The local UDP socket cannot bind.
    Bind(transport::Error),
    /// The session failed before the node admitted the hello, or ended before the
    /// close.
    Session(program::Error),
    /// The client's thread panicked, and the panic hook of the process got its
    /// message. Only a build where panics unwind gives it: with `panic = "abort"`, the
    /// panic ends the process.
    Panicked,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Thread { reason } => {
                write!(f, "cannot start the client thread: {reason}")
            }
            Self::Bind(error) => write!(f, "cannot bind the local UDP socket: {error}"),
            Self::Session(error) => write!(f, "{error}"),
            Self::Panicked => write!(f, "the client thread panicked"),
        }
    }
}

impl std::error::Error for Error {}

/// The main function of the client's thread: connects, sends the outcome, and holds
/// the session until `ended` completes. It then sends the error that ended the
/// session before `ended`, if any, and waits for each task, so that the close of the
/// session goes out.
async fn run(
    node: Node,
    subject: Subject,
    tasks: Tasks,
    outcome: mpsc::Sender<Result<(), Error>>,
    ended: oneshot::Receiver<()>,
) {
    let group = Group::new(tasks);
    match connect(node, subject, group.tasks().clone()).await {
        Ok(session) => {
            drop(outcome.send(Ok(())));
            drop(ended.await);
            if let Some(error) = session.ended() {
                drop(outcome.send(Err(Error::Session(error))));
            }
        }
        Err(error) => drop(outcome.send(Err(error))),
    }
    group.ended().await;
}

/// A session with `node` as `subject`, on the drivers of `os` and `tasks`.
async fn connect(
    node: Node,
    subject: Subject,
    tasks: Tasks,
) -> Result<program::Session, Error> {
    let clock = os::clock();
    let entropy = os::entropy();
    let pool = Rc::new(block::Pool::heap(POOL));
    let transport = transport::Client::new(transport::client::Config {
        net: os::net(),
        clock: clock.clone(),
        entropy: entropy.clone(),
        tasks: tasks.clone(),
        pool: Rc::clone(&pool),
    })
    .map_err(Error::Bind)?;
    let config = program::Config {
        via: node.key,
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
        frames: Rc::new(block::Pool::heap(FRAMES)),
    };
    program::Session::connect(&transport, config)
        .await
        .map_err(Error::Session)
}

#[cfg(test)]
mod tests;
