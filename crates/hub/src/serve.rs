//! The streams that a [`Link`](crate::Link) serves: remote reader sessions, and the
//! hello, request, and read streams of a client, and why one ends.

mod admission;
pub(crate) mod client;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt;
use std::future::poll_fn;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::slice;
use std::task::Poll;

use block::{Block, Unique};
use transport::stream::{Incoming, Part, Receiver, Sender};
use transport::{Class, Code};
use types::channel::{self, Slot};
use types::ed25519::PublicKey;
use types::frame::key_set::KeySet;
use types::frame::{self, Frame, Placed};
use types::name::Name;
use types::node;
use wire::header::MALFORMED;
use wire::hub::client::{BODY_BYTES_MAX, Refusal};
use wire::hub::forward::Forward;
use wire::hub::{
    BUSY, FAILED, FromReader, Head, Home, Mode, NOT_HOME, UNKNOWN, ends, keys,
};

use crate::pending::Pending;
use crate::reader::{self, Channels, Complete, Session, Stop};
use crate::writer;
use crate::{Away, State};

pub use client::{Reply, Request};

/// The most bytes of client bodies that one hub holds at once, over each of its links:
/// the body of a request from its decode until the caller sends or drops its [`Reply`],
/// the open of a read until it decodes after the wait for the home of its index, the
/// open of a write until it decodes, the decoded names of each write stream, 16 bytes
/// and the length of each, and the places of a forward, 4 bytes for each channel of
/// its open, from that decode until it sends its places, and its ends and counts, 20
/// bytes for each place of its writers, from the home of each index until the stream
/// ends. A body that does not fit stops with `BUSY` before the hub reads a byte of it,
/// and ends or names that do not fit stop with `BUSY` before the writer opens. The
/// bodies of one subject hold at most [`BODY_BYTES_MAX`] of it ([`Error::Share`]).
pub const BODIES_BYTES_MAX: u64 = 2 * BODY_BYTES_MAX;

/// Why [`Link::serve`](crate::Link::serve) ended a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A message from a reader's node, or a frame of a write from a program, that
    /// `wire::hub` refuses. Code `MALFORMED`.
    Message(wire::hub::Error),
    /// A message from a program that `wire::hub::client` refuses, or a body that ended
    /// early ([`wire::hub::client::Error::Unfinished`]). Code `MALFORMED`.
    Client(wire::hub::client::Error),
    /// The peer opened the stream one way. Code `MALFORMED`.
    OneWay,
    /// The stream's class is not the class of the open's mode. Code `MALFORMED`.
    Class(Class),
    /// The keys of the open of a read, or the channels of the group of a forward, are
    /// on more than one index. Code `MALFORMED`.
    ManyIndexes,
    /// The selector of a read matches more than [`wire::hub::client::PLACES_MAX`]
    /// places with their index. Code `MALFORMED`.
    Places {
        /// The count of matched places, the index too.
        count: usize,
    },
    /// The keys of the open do not hold their index. Code `MALFORMED`.
    NoIndex,
    /// The open names a channel more than once. Code `MALFORMED`.
    Repeated(channel::Key),
    /// The open names a channel that this node does not know. Code `UNKNOWN`.
    Unknown(channel::Key),
    /// A channel of the open was removed from the definitions. Code `UNKNOWN`.
    Removed(channel::Key),
    /// The mesh names another node as the home of the open's index. Code `NOT_HOME`.
    NotHome,
    /// The mesh stopped, so the home of the open's index is not known. Code `FAILED`.
    Mesh(mesh::Stopped),
    /// The home's buffer failed. Code `FAILED`.
    Buffer(env::files::Error),
    /// The home's pool had no block for a reply (`Exhausted` or `Refused`), or for a
    /// read from disk of a session. Code `BUSY`. A later open can succeed.
    Pool(block::Error),
    /// The stream or its session failed. No code.
    Stream(transport::Error),
    /// `access` refused a hello or a request, or the hub refused the session:
    /// `Unsynced` when it has no mesh time for a challenge, and `Expired` when the
    /// admitted hello ends ([`access::proof::Admitted::ends`]). Code: the one that
    /// CLIENT HELLO gives for the error, `REFUSED` for each that tells about the spec.
    Access(Box<access::proof::Error>),
    /// The hello does not echo the nonce of the last challenge. Code `STALE`.
    Stale,
    /// A later stream before the link admitted a hello. Code `MALFORMED`.
    Unadmitted,
    /// A request stream while another request of the link waits for its reply. Code
    /// `MALFORMED`.
    Pending,
    /// The request bodies, read opens, write opens, and ends and counts of write
    /// streams of the hub hold so many body bytes that a body or ends of `length` more
    /// are over [`BODIES_BYTES_MAX`]. Code `BUSY`. The ends and counts of a write are
    /// released only when its stream ends. A retry can succeed once enough of them
    /// are released.
    Bodies {
        /// The bytes of the refused request, read, or write: a body, or the ends,
        /// counts, or names of a write.
        length: u64,
        /// The body bytes that the open requests, reads, and writes of the hub held.
        held: u64,
    },
    /// The request bodies, read opens, write opens, and ends and counts of write
    /// streams of `subject`, over each link of the hub, hold so many body bytes that a
    /// body or ends of `length` more are over the share of one subject,
    /// [`BODY_BYTES_MAX`]. Code `BUSY`. The ends and counts of a write are released
    /// only when its stream ends. A retry can succeed once enough of them are
    /// released.
    Share {
        /// The subject of the link's admitted hello.
        subject: Name,
        /// The bytes of the refused request, read, or write: a body, or the ends,
        /// counts, or names of a write.
        length: u64,
        /// The body bytes that the open requests, reads, and writes of `subject` held.
        held: u64,
    },
    /// The reader of a read did not open. Code: `UNKNOWN` for [`reader::Error::Empty`],
    /// `MALFORMED` for `ManyIndexes`, `NOT_HOME` for `Remote`, `BUSY` for `Pool`, the
    /// code of the home's refusal but `FAILED` for its `MALFORMED`, and `FAILED` for
    /// `Mesh`, `Transport`, and `Message`. A read gives no `Unsynced`, `Full`, or
    /// `Unrecorded`.
    Reader(Box<reader::Error>),
    /// The reader of a read ended. Code: `UNKNOWN` for [`reader::Ended::Removed`],
    /// `BUSY` for `Pool`, the code of the home's refusal but `FAILED` for its
    /// `MALFORMED`, and `FAILED` for `Buffer`, `Stream`, `Message`, `Frame`, and
    /// `Credit`. A read gives no `Replaced`.
    Ended(Box<reader::Ended>),
    /// The writer of a write did not open. Code: `UNKNOWN` for
    /// [`writer::Error::Unknown`], `NOT_HOME` for `Remote`, `FAILED` for `Mesh`, and
    /// `MALFORMED` for a `Home` error of `Lease`. A write gives no `Empty` and no
    /// `Home` error of `Unsynced`.
    Writer(Box<writer::Error>),
    /// The ends of a frame of a write break a rule of its key set. Code `MALFORMED`.
    Frame(types::frame::Error),
}

impl Error {
    /// The code that the stream stops with, or `None` when it is broken.
    pub(crate) fn code(&self) -> Option<Code> {
        match self {
            Self::Message(_)
            | Self::Client(_)
            | Self::OneWay
            | Self::Class(_)
            | Self::ManyIndexes
            | Self::Places { .. }
            | Self::NoIndex
            | Self::Repeated(_)
            | Self::Unadmitted
            | Self::Pending
            | Self::Frame(_) => Some(Code(MALFORMED)),
            Self::Access(error) => Some(Code(client::refusal(error).code())),
            Self::Stale => Some(Code(Refusal::Stale.code())),
            Self::Unknown(_) | Self::Removed(_) => Some(Code(UNKNOWN)),
            Self::NotHome => Some(Code(NOT_HOME)),
            Self::Buffer(_) | Self::Mesh(_) => Some(Code(FAILED)),
            Self::Pool(_) | Self::Bodies { .. } | Self::Share { .. } => {
                Some(Code(BUSY))
            }
            Self::Stream(_) => None,
            Self::Reader(error) => Some(Code(client::read::refused(error).code())),
            Self::Ended(ended) => Some(Code(client::read::ended(ended).code())),
            Self::Writer(error) => Some(Code(client::write::refused(error).code())),
        }
    }
}

impl fmt::Display for Error {
    #[expect(clippy::too_many_lines, reason = "one arm for each error")]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(error) => write!(f, "a hub message is not valid: {error}"),
            Self::Client(error) => {
                write!(f, "a client message is not valid: {error}")
            }
            Self::OneWay => f.write_str("a hub session needs a two-way stream"),
            Self::Class(class) => write!(
                f,
                "the stream has class {class:?}, which is not the class of the open's \
                 mode"
            ),
            Self::ManyIndexes => {
                f.write_str("the open names channels on more than one index")
            }
            Self::Places { count } => write!(
                f,
                "the open matches {count} places, over the cap of {}",
                wire::hub::client::PLACES_MAX
            ),
            Self::NoIndex => {
                f.write_str("the open does not name the index of its channels")
            }
            Self::Repeated(key) => {
                write!(f, "the open names channel {key} more than once")
            }
            Self::Unknown(key) => {
                write!(
                    f,
                    "the open names channel {key}, which this node does not know"
                )
            }
            Self::Removed(key) => write!(
                f,
                "the open names channel {key}, which was removed from this node"
            ),
            Self::NotHome => f.write_str(
                "the mesh names another node as the home of the open's index",
            ),
            Self::Mesh(stopped) => write!(f, "the mesh stopped: {stopped}"),
            Self::Buffer(error) => write!(f, "the buffer of the shard failed: {error}"),
            Self::Pool(error) => {
                write!(f, "the home's pool had no block for a reply: {error}")
            }
            Self::Stream(error) => {
                write!(f, "the stream of the hub session failed: {error}")
            }
            Self::Access(error) => write!(f, "access refused the program: {error}"),
            Self::Stale => f.write_str(
                "the hello does not echo the nonce of the node's last challenge",
            ),
            Self::Unadmitted => f.write_str(
                "the program opened a stream before the node admitted a hello",
            ),
            Self::Pending => f.write_str(
                "the program sent a request while another request waits for its reply",
            ),
            Self::Bodies { length, held } => write!(
                f,
                "{length} bytes of a body or of the ends, counts, or names of a write \
                 do not fit under the cap of {BODIES_BYTES_MAX} bytes: the open \
                 requests, reads, and writes of the hub hold {held} bytes"
            ),
            Self::Share {
                subject,
                length,
                held,
            } => write!(
                f,
                "{length} bytes of a body or of the ends, counts, or names of a write \
                 do not fit under the share of {BODY_BYTES_MAX} bytes of subject \
                 {subject}: its open requests, reads, and writes hold {held} bytes"
            ),
            Self::Reader(error) => {
                write!(f, "the reader of a read did not open: {error}")
            }
            Self::Ended(ended) => write!(f, "the reader of a read ended: {ended}"),
            Self::Writer(error) => {
                write!(f, "the writer of a write did not open: {error}")
            }
            Self::Frame(error) => {
                write!(f, "a frame of a write is not valid: {error}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<Away> for Error {
    fn from(away: Away) -> Self {
        match away {
            Away::Remote(..) => Self::NotHome,
            Away::Mesh(stopped) => Self::Mesh(stopped),
        }
    }
}

impl From<wire::hub::Error> for Error {
    fn from(error: wire::hub::Error) -> Self {
        Self::Message(error)
    }
}

impl From<wire::hub::client::Error> for Error {
    fn from(error: wire::hub::client::Error) -> Self {
        Self::Client(error)
    }
}

impl From<transport::Error> for Error {
    fn from(error: transport::Error) -> Self {
        Self::Stream(error)
    }
}

/// Gives the serve that `serve` makes in a box, behind a trait object, so that its
/// frame path has a poll of its own. Out of line, so that its future, which is built on
/// the stack before it moves into the box, adds nothing to the stack frame of each poll
/// of the caller.
#[inline(never)]
fn boxed<'a, F>(
    serve: impl FnOnce() -> F,
) -> Pin<Box<dyn Future<Output = Result<(), Error>> + 'a>>
where
    F: Future<Output = Result<(), Error>> + 'a,
{
    Box::pin(serve())
}

/// Serves the stream `incoming` from a node, a forward or a reader session by its
/// first message. `peer` is the public key that the node's transport session proved,
/// and `holder` is the node when the hub's region holds `peer` as a member.
/// Stops the stream with the code of the error that ends it.
///
/// # Panics
///
/// When `holder` is `None` and the first message is a forward.
pub(crate) async fn run(
    state: &Rc<RefCell<State>>,
    peer: PublicKey,
    holder: Option<node::Key>,
    incoming: Incoming,
) -> Result<(), Error> {
    let class = incoming.class;
    let (mut receiver, mut sender) = halves(incoming)?;
    // No async level between this and the reader's serve, which polls once a frame,
    // and a forward in a box of its own, so that the reader's poll holds none of it.
    let served = match receiver.recv().await {
        Ok(Some(message)) => match Forward::decode(&message) {
            Ok(forward) => {
                drop(message);
                let via = holder.expect("invariant: a forward comes from a member");
                boxed(|| {
                    client::forward::serve(
                        state,
                        via,
                        peer,
                        forward,
                        &mut receiver,
                        &mut sender,
                    )
                })
                .await
            }
            Err(wire::hub::Error::Kind { .. }) => {
                serve(state, class, message, &mut receiver, &mut sender).await
            }
            Err(error) => Err(error.into()),
        },
        Ok(None) => sender.finish().map_err(Error::from),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = &served {
        stop(receiver, sender, error);
    }
    served
}

/// The two halves of `incoming`. Stops a one-way stream.
fn halves(incoming: Incoming) -> Result<(Receiver, Sender), Error> {
    let Incoming {
        receiver, sender, ..
    } = incoming;
    let Some(sender) = sender else {
        receiver.stop(Code(MALFORMED));
        return Err(Error::OneWay);
    };
    Ok((receiver, sender))
}

/// Stops both halves of a stream with the code of `error`, if it has one.
fn stop(receiver: Receiver, sender: Sender, error: &Error) {
    if let Some(code) = error.code() {
        receiver.stop(code);
        sender.reset(code);
    }
}

/// Serves the reader session that `message` opens.
async fn serve(
    state: &Rc<RefCell<State>>,
    class: Class,
    message: Block,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<(), Error> {
    let mut home = Home::default();
    let Some(Opened {
        mut session,
        complete,
        mut layout,
        live,
    }) = open(state, class, &mut home, message, receiver).await?
    else {
        sender.finish()?;
        return Ok(());
    };
    sender
        .send(reply(state, wire::hub::Reply::Opened { live })?)
        .await?;
    // `peer` lives across turns, so a frame never drops a read of the peer. `take`
    // gives up a frame only in the poll that returns it.
    let mut peer = pin!(peer(receiver, home, complete.as_ref()));
    loop {
        // `unless` polls `peer` first, and a grant wakes no session, so a frame that
        // waits for credit goes out at the take after its grant (CREDIT RULES).
        let Some(taken) = unless(peer.as_mut(), session.take()).await? else {
            sender.finish()?;
            return Ok(());
        };
        match taken {
            Ok((frame, lens)) => {
                layout.send(state, sender, &frame, &lens.set).await?;
            }
            Err(Stop::Buffer(error)) => return Err(Error::Buffer(error)),
            Err(Stop::Pool(error)) => return Err(Error::Pool(error)),
            Err(Stop::Removed(key)) => return Err(Error::Removed(key)),
            Err(Stop::Replaced) => {
                unreachable!("invariant: a served session has no name to take over")
            }
        }
    }
}

/// Runs `work` until it ends, unless `peer` ends first: `None` when the peer finished,
/// or the error of the peer. `peer` polls first in each poll, so the node does no work
/// in a poll in which the peer ends.
async fn unless<P, W>(
    mut peer: Pin<&mut P>,
    work: W,
) -> Result<Option<W::Output>, Error>
where
    P: Future<Output = Result<(), Error>>,
    W: Future,
{
    let mut work = pin!(work);
    poll_fn(|cx| match peer.as_mut().poll(cx) {
        Poll::Ready(finished) => Poll::Ready(finished.map(|()| None)),
        Poll::Pending => work.as_mut().poll(cx).map(|output| Ok(Some(output))),
    })
    .await
}

/// Reads what the peer sends after the keys run, and grants each credit. Returns when
/// the peer finishes.
async fn peer(
    receiver: &mut Receiver,
    mut home: Home,
    complete: Option<&Complete>,
) -> Result<(), Error> {
    while let Some(message) = receiver.recv().await? {
        let FromReader::Credit(grant) = home.decode(&message)? else {
            unreachable!("invariant: after the keys run, Home gives only credits");
        };
        complete
            .expect("invariant: Home refuses a credit in a latest session")
            .grant(grant.limit_bytes);
    }
    Ok(())
}

/// A session open at the home.
struct Opened {
    session: Session,
    /// The grant and the ack of a complete session.
    complete: Option<Complete>,
    layout: Layout,
    /// The open position of the session.
    live: u64,
}

/// Decodes the open in `message`, reads its keys, checks each key as it arrives, waits
/// until the mesh names this node the home of the index, and carries the index and
/// opens the session in the order of the keys. A removal of a channel that it checked
/// ends it. Gives `None` when the peer finishes first.
async fn open(
    state: &Rc<RefCell<State>>,
    class: Class,
    home: &mut Home,
    message: Block,
    receiver: &mut Receiver,
) -> Result<Option<Opened>, Error> {
    let FromReader::Open(open) = home.decode(&message)? else {
        unreachable!("invariant: the first message that Home gives is an open");
    };
    drop(message);
    let wanted = match open.mode {
        Mode::Latest => Class::Latest,
        Mode::Complete { .. } => Class::Complete,
    };
    if class != wanted {
        return Err(Error::Class(class));
    }
    let mut opening = Opening::new(state);
    loop {
        let Some(message) = opening.recv(receiver).await? else {
            return Ok(None);
        };
        let FromReader::Keys { keys, last } = home.decode(&message)? else {
            unreachable!("invariant: Home gives the keys run after the open");
        };
        opening.check(keys)?;
        if last {
            break;
        }
    }
    let at = opening.at.ok_or(Error::NoIndex)?;
    let Some(granted) = wait_for(&opening, home, receiver).await? else {
        return Ok(None);
    };
    let keys = opening.into_keys();
    let slots = state.borrow_mut().slots(keys[at], &keys);
    let index = slots[at];
    let channels = Channels {
        keys: keys.into(),
        slots: slots.clone(),
        index,
    };
    let (session, complete, live) = match open.mode {
        Mode::Complete { limit_bytes } => {
            let charge = ::home::reader::complete::Charge::Places(slots.clone());
            let (session, complete, live) =
                Session::complete(state, channels, limit_bytes, charge);
            (session, Some(complete), live)
        }
        Mode::Latest => {
            let (session, live) = Session::latest(state, channels);
            (session, None, live)
        }
    };
    if let Some(complete) = &complete {
        complete.grant(granted);
    }
    Ok(Some(Opened {
        session,
        complete,
        layout: Layout::new(slots, index),
        live,
    }))
}

/// A served open from its first key to its session, which a removal of a channel that
/// it checked ends.
struct Opening<'s> {
    state: &'s Rc<RefCell<State>>,
    pending: Pending<'s>,
    /// The index of the keys checked, which the first key sets.
    index: Option<channel::Key>,
    /// The position of the index in the keys checked.
    at: Option<usize>,
    /// The keys checked, in order.
    keys: Vec<channel::Key>,
    /// The keys checked, so a key listed twice is found.
    listed: BTreeSet<channel::Key>,
}

impl<'s> Opening<'s> {
    fn new(state: &'s Rc<RefCell<State>>) -> Self {
        Self {
            state,
            pending: Pending::new(state, Vec::new()),
            index: None,
            at: None,
            keys: Vec::new(),
            listed: BTreeSet::new(),
        }
    }

    /// Reads the next message of the peer. Fails at once when a change of the
    /// definitions removes a channel of the open.
    async fn recv(&self, receiver: &mut Receiver) -> Result<Option<Block>, Error> {
        let next = self.pending.wait(receiver.recv()).await;
        Ok(next.map_err(Error::Removed)??)
    }

    /// Checks that each of `keys` is known, on the index of the open, and not named
    /// before, and adds it to the channels of the open. It reads the definitions that
    /// `recv` applied, and applies no spec itself: an apply could end the wait before
    /// `extend`.
    fn check(&mut self, keys: keys::Iter<'_>) -> Result<(), Error> {
        let (state, from) = (self.state.borrow(), self.keys.len());
        for key in keys {
            let of = state.channels.get(key).ok_or(Error::Unknown(key))?.index();
            if *self.index.get_or_insert(of) != of {
                return Err(Error::ManyIndexes);
            }
            if !self.listed.insert(key) {
                return Err(Error::Repeated(key));
            }
            if key == of {
                self.at = Some(self.keys.len());
            }
            self.keys.push(key);
        }
        drop(state);
        self.pending.extend(&self.keys[from..]);
        Ok(())
    }

    /// Ends the open, and gives the keys it checked, in order.
    fn into_keys(self) -> Vec<channel::Key> {
        self.keys
    }

    /// Waits until the mesh names this node the home of the index. Fails at once when
    /// a change of the definitions removes a channel of the open.
    ///
    /// # Panics
    ///
    /// Panics if the open checked no key.
    async fn home(&self) -> Result<(), Error> {
        let index = self.index.expect("invariant: the open checked a key");
        let homed = self
            .pending
            .wait(crate::homes(self.state, slice::from_ref(&index)));
        Ok(homed.await.map_err(Error::Removed)??)
    }
}

/// Waits until the mesh names this node the home of the index of `opening`, and reads
/// the peer meanwhile. Gives the highest grant that the peer sent, 0 for none, or
/// `None` when the peer finished first. Fails at once when a change of the
/// definitions removes a channel of `opening`.
async fn wait_for(
    opening: &Opening<'_>,
    home: &mut Home,
    receiver: &mut Receiver,
) -> Result<Option<u64>, Error> {
    let mut homed = pin!(opening.home());
    let mut granted = 0;
    loop {
        let mut recv = pin!(receiver.recv());
        let read = poll_fn(|cx| match homed.as_mut().poll(cx) {
            Poll::Ready(found) => Poll::Ready(Err(found)),
            Poll::Pending => recv.as_mut().poll(cx).map(Ok),
        })
        .await;
        let message = match read {
            Err(found) => {
                found?;
                return Ok(Some(granted));
            }
            Ok(read) => match read? {
                Some(message) => message,
                None => return Ok(None),
            },
        };
        let FromReader::Credit(grant) = home.decode(&message)? else {
            unreachable!("invariant: after the keys run, Home gives only credits");
        };
        granted = granted.max(grant.limit_bytes);
    }
}

/// A block of the home's pool that holds `reply`.
fn reply(state: &RefCell<State>, reply: wire::hub::Reply) -> Result<Block, Error> {
    let mut block = alloc(state, reply.encoded_len())?;
    reply.encode(&mut block);
    Ok(block.freeze())
}

/// A block of `len` bytes from the home's pool. Each caller asks for at most its
/// largest block: a reply, an ends run, which is smaller than the frame's
/// descriptors, or a message of a client stream.
fn alloc(state: &RefCell<State>, len: usize) -> Result<Unique, Error> {
    state.borrow().alloc(len).map_err(Error::Pool)
}

/// How a session sends each frame, through its places. It keeps its buffers across
/// frames, so a frame allocates only its blocks.
struct Layout {
    places: frame::Places,
    index: Slot,
    parts: Vec<Part>,
}

impl Layout {
    fn new(slots: Box<[Slot]>, index: Slot) -> Self {
        Self {
            places: frame::Places::new(slots),
            index,
            parts: Vec::new(),
        }
    }

    /// Sends `frame`: its head, the run of its ends, then its body, each series of a
    /// place in place order.
    async fn send(
        &mut self,
        state: &RefCell<State>,
        sender: &mut Sender,
        frame: &Frame,
        set: &KeySet,
    ) -> Result<(), Error> {
        let placed = self.places.lay(frame, set);
        let head = head(frame, set, self.index, placed.len());
        sender
            .send(reply(state, wire::hub::Reply::Head(head))?)
            .await?;
        for run in runs(placed, sender.bytes_max()) {
            let mut block = alloc(state, run.len() * ends::LEN)?;
            ends::encode(
                run.iter().map(|series| {
                    let place = u32::try_from(series.place).expect("a place is a u32");
                    let end =
                        u32::try_from(series.end).expect("a frame's body fits a u32");
                    (place, end)
                }),
                &mut block,
            );
            sender.send(block.freeze()).await?;
        }
        let mut cut = Cut::default();
        while cut.next(placed, sender.bytes_max(), &mut self.parts) {
            sender.send_parts(frame.body(), &self.parts).await?;
        }
        Ok(())
    }
}

/// The head of `frame`, of key set `set`, for a session of index `index` that sends
/// `series` of its series.
fn head(frame: &Frame, set: &KeySet, index: Slot, series: usize) -> Head {
    let index = set
        .find(index)
        .expect("invariant: a session's frame holds its index");
    Head {
        path: frame.path(),
        range: frame
            .range(set.entries()[index].group)
            .expect("invariant: a frame holds the range of each group"),
        series: u32::try_from(series).expect("a count of places is a u32"),
    }
}

/// `placed` in runs whose ends fit `max` bytes.
fn runs(placed: &[Placed], max: usize) -> std::slice::Chunks<'_, Placed> {
    placed.chunks(max / ends::LEN)
}

/// Where the cut of a body into messages stands: the series and how many of its bytes,
/// then zeros, went into earlier messages.
#[derive(Debug, Default)]
struct Cut {
    at: usize,
    done: usize,
}

impl Cut {
    /// Fills `parts` with the next message of the body of `placed`, at most `max`
    /// bytes. Gives `false` once the body is sent.
    fn next(&mut self, placed: &[Placed], max: usize, parts: &mut Vec<Part>) -> bool {
        parts.clear();
        let mut room = max;
        while room > 0
            && let Some(at) = placed.get(self.at)
        {
            let len = at.bounds.len();
            let pad = placed
                .get(self.at + 1)
                .map_or(0, |next| next.end - next.bounds.len() - at.end);
            let total = len + pad;
            let take = (total - self.done).min(room);
            let (from, to) = (self.done, self.done + take);
            if take > 0 {
                parts.push(Part {
                    range: at.bounds.start + from.min(len)
                        ..at.bounds.start + to.min(len),
                    zeros: u8::try_from(to.max(len) - from.max(len))
                        .expect("a pad fits a u8"),
                });
            }
            room -= take;
            self.done = to;
            if self.done == total {
                (self.at, self.done) = (self.at + 1, 0);
            }
        }
        !parts.is_empty()
    }
}

// These tests call the private `head`, `runs`, and `Cut::next`, so each case of the
// pure parts has a test; the tests through `Link::serve` check them on the wire.
#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::sync::Arc;

    use types::frame::key_set::{Group, Interner};
    use types::frame::{Draft, Form, Path};
    use types::sample::{Scalar, Type};

    use super::*;

    const I64: Type = Type::Scalar(Scalar::I64);

    fn key(key: u128) -> channel::Key {
        channel::Key::from_u128(key)
    }

    /// Each end of the reader of a read, but `Replaced`, stops it with its code. A
    /// refusal of the home keeps its code. Only a remote reader gives a refusal,
    /// `Error::Pool`, `Transport`, and `Message`, and `Ended::Pool`, `Stream`,
    /// `Message`, `Frame`, and `Credit`. A read opens no remote reader until #2419, so
    /// this test pins those arms.
    #[test]
    #[expect(clippy::too_many_lines, reason = "one case for each error")]
    fn each_error_of_the_reader_of_a_read_has_its_code() {
        let pool = block::Error::Exhausted {
            requested: 1,
            available: 0,
        };
        let refused = |refusal| {
            (
                reader::Error::Refused(refusal),
                reader::Ended::Refused(refusal),
            )
        };
        let mut cases = vec![
            (Error::Reader(Box::new(reader::Error::Empty)), UNKNOWN),
            (
                Error::Reader(Box::new(reader::Error::Remote {
                    home: types::node::Key::from_u128(2),
                })),
                NOT_HOME,
            ),
            (
                Error::Reader(Box::new(reader::Error::ManyIndexes {
                    first: "a".parse().expect("a name"),
                    other: "b".parse().expect("a name"),
                })),
                MALFORMED,
            ),
            (
                Error::Reader(Box::new(reader::Error::Pool(pool.clone()))),
                BUSY,
            ),
            (
                Error::Reader(Box::new(reader::Error::Mesh(mesh::Stopped::Dropped))),
                FAILED,
            ),
            (
                Error::Reader(Box::new(reader::Error::Transport(
                    transport::Error::Unroutable,
                ))),
                FAILED,
            ),
            (
                Error::Reader(Box::new(reader::Error::Message(
                    wire::hub::Error::Empty,
                ))),
                FAILED,
            ),
            (
                Error::Ended(Box::new(reader::Ended::Removed(key(2)))),
                UNKNOWN,
            ),
            (Error::Ended(Box::new(reader::Ended::Pool(pool))), BUSY),
            (
                Error::Ended(Box::new(reader::Ended::Buffer(
                    env::files::Error::Full {
                        path: "ring".into(),
                    },
                ))),
                FAILED,
            ),
            (
                Error::Ended(Box::new(reader::Ended::Stream(
                    transport::Error::Unroutable,
                ))),
                FAILED,
            ),
            (
                Error::Ended(Box::new(reader::Ended::Message(wire::hub::Error::Empty))),
                FAILED,
            ),
            (
                Error::Ended(Box::new(reader::Ended::Frame(
                    frame::Error::OutOfRange {
                        entry: 1,
                        entries: 1,
                    },
                ))),
                FAILED,
            ),
            (
                Error::Ended(Box::new(reader::Ended::Credit { limit_bytes: 1 })),
                FAILED,
            ),
        ];
        for (refusal, code) in [
            (wire::hub::Refusal::Malformed, FAILED),
            (wire::hub::Refusal::Unknown, UNKNOWN),
            (wire::hub::Refusal::NotHome, NOT_HOME),
            (wire::hub::Refusal::Failed, FAILED),
            (wire::hub::Refusal::Busy, BUSY),
        ] {
            let (error, ended) = refused(refusal);
            cases.extend([
                (Error::Reader(Box::new(error)), code),
                (Error::Ended(Box::new(ended)), code),
            ]);
        }
        for (error, code) in cases {
            assert_eq!(error.code(), Some(Code(code)), "{error:?}");
        }
    }

    /// The per-frame results of the serve loops hold an `Error`, so a large rare cause
    /// is boxed.
    #[test]
    fn keeps_an_error_at_64_bytes() {
        assert_eq!(size_of::<Error>(), 64);
    }

    /// Each error of the writer of a write that a write can give stops it with its
    /// code, and so does a frame that breaks a rule of its key set.
    #[test]
    fn each_error_of_the_writer_of_a_write_has_its_code() {
        let cases = [
            (
                Error::Writer(Box::new(writer::Error::Unknown(
                    "a".parse().expect("a name"),
                ))),
                UNKNOWN,
            ),
            (
                Error::Writer(Box::new(writer::Error::Remote {
                    home: types::node::Key::from_u128(2),
                })),
                NOT_HOME,
            ),
            (
                Error::Writer(Box::new(writer::Error::Mesh(mesh::Stopped::Dropped))),
                FAILED,
            ),
            (
                Error::Writer(Box::new(writer::Error::Home(
                    ::home::writer::Error::Lease {
                        span: types::time::Span::ZERO,
                    },
                ))),
                MALFORMED,
            ),
            (
                Error::Frame(frame::Error::OutOfRange {
                    entry: 1,
                    entries: 1,
                }),
                MALFORMED,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), Some(Code(code)), "{error:?}");
        }
    }

    #[test]
    #[should_panic(expected = "invariant: a write does not give Empty")]
    fn panics_for_the_code_of_a_writer_that_names_no_channel() {
        Error::Writer(Box::new(writer::Error::Empty)).code();
    }

    #[test]
    #[should_panic(expected = "invariant: a write does not give Home(Unsynced)")]
    fn panics_for_the_code_of_a_writer_that_the_home_refuses_as_unsynced() {
        Error::Writer(Box::new(writer::Error::Home(
            ::home::writer::Error::Unsynced,
        )))
        .code();
    }

    #[test]
    #[should_panic(expected = "invariant: an unnamed reader does not give Unsynced")]
    fn panics_for_the_code_of_an_error_that_only_a_named_reader_gives() {
        Error::Reader(Box::new(reader::Error::Unsynced)).code();
    }

    #[test]
    #[should_panic(expected = "invariant: a read does not end as Replaced")]
    fn panics_for_the_code_of_an_end_that_a_read_sends_or_never_has() {
        Error::Ended(Box::new(reader::Ended::Replaced)).code();
    }

    /// A frame of `data` on index 1, with `lens` bytes in each series by entry.
    fn frame(
        interner: &mut Interner,
        pool: &block::Pool,
        data: &[u128],
        lens: &[usize],
    ) -> (Frame, Arc<KeySet>) {
        let data: Vec<_> = data.iter().map(|&k| (key(k), I64)).collect();
        let set = interner.intern(&[Group {
            index: key(1),
            data: &data,
        }]);
        let lens: Vec<_> = lens.iter().copied().enumerate().collect();
        let mut draft = Draft::new(pool, &set, Form::Encoded, &lens).expect("room");
        draft.set_count(0, 1);
        draft.set_seq(0, 7);
        (draft.freeze(Path::Live), set)
    }

    #[test]
    fn gives_the_path_range_and_series_count_of_the_frame_in_the_head() {
        let pool =
            block::Pool::heap(block::Config::new(1 << 20).expect("the budget fits"));
        let mut interner = Interner::new();
        let index = interner.slots().index(key(1));
        let (frame, set) = frame(&mut interner, &pool, &[2, 3], &[8, 8, 8]);
        let head = head(&frame, &set, index, 2);
        assert_eq!(head.path, Path::Live);
        assert_eq!(Some(head.range), frame.range(0));
        assert_eq!(head.series, 2);
    }

    /// The head carries the range of the session's index group, not the first group.
    #[test]
    fn gives_the_range_of_the_index_group_of_the_session() {
        let pool =
            block::Pool::heap(block::Config::new(1 << 20).expect("the budget fits"));
        let mut interner = Interner::new();
        let slots = interner.slots();
        slots.index(key(1));
        slots.data(key(2), I64);
        let index = slots.index(key(4));
        slots.data(key(5), I64);
        let set = interner.intern(&[
            Group {
                index: key(1),
                data: &[(key(2), I64)],
            },
            Group {
                index: key(4),
                data: &[(key(5), I64)],
            },
        ]);
        let lens: Vec<_> = (0..4).map(|entry| (entry, 8)).collect();
        let mut draft = Draft::new(&pool, &set, Form::Encoded, &lens).expect("room");
        draft.set_count(0, 1);
        draft.set_count(1, 3);
        let frame = draft.freeze(Path::Live);
        let head = head(&frame, &set, index, 2);
        assert_eq!(Some(head.range), frame.range(1));
        assert_ne!(frame.range(0), frame.range(1));
    }

    #[test]
    fn splits_the_ends_into_runs_that_fit_the_message_limit() {
        let placed = placed(&[(0..8, 0), (8..16, 0), (16..24, 0)]);
        let runs = |max| runs(&placed, max).map(<[Placed]>::len).collect::<Vec<_>>();
        assert_eq!(runs(16), [2, 1]);
        assert_eq!(runs(23), [2, 1]);
        assert_eq!(runs(24), [3]);
    }

    /// Series with `bounds` in the home's body, each followed by `zeros` of padding in
    /// the reader's body.
    fn placed(bounds: &[(Range<usize>, usize)]) -> Vec<Placed> {
        let mut start = 0;
        bounds
            .iter()
            .enumerate()
            .map(|(place, (bounds, zeros))| {
                let end = start + bounds.len();
                start = end + zeros;
                Placed {
                    place,
                    bounds: bounds.clone(),
                    end,
                }
            })
            .collect()
    }

    /// Each message of the cut, as `(range, zeros)` of each part.
    fn cut(placed: &[Placed], max: usize) -> Vec<Vec<(Range<usize>, u8)>> {
        let (mut cut, mut parts, mut messages) =
            (Cut::default(), Vec::new(), Vec::new());
        while cut.next(placed, max, &mut parts) {
            messages.push(parts.iter().map(|p| (p.range.clone(), p.zeros)).collect());
        }
        messages
    }

    #[test]
    fn sends_a_body_that_fits_in_one_message() {
        let series = placed(&[(0..5, 3), (16..24, 0)]);
        assert_eq!(cut(&series, 64), [vec![(0..5, 3), (16..24, 0)]]);
    }

    #[test]
    fn cuts_a_series_and_its_zeros_at_the_message_limit() {
        let series = placed(&[(0..5, 3), (16..24, 0)]);
        assert_eq!(
            cut(&series, 4),
            [
                vec![(0..4, 0)],
                vec![(4..5, 3)],
                vec![(16..20, 0)],
                vec![(20..24, 0)],
            ]
        );
        assert_eq!(
            cut(&series, 6),
            [
                vec![(0..5, 1)],
                vec![(5..5, 2), (16..20, 0)],
                vec![(20..24, 0)],
            ]
        );
    }

    #[test]
    fn sends_no_message_for_empty_series() {
        let series = placed(&[(8..8, 0), (8..10, 0), (16..16, 0)]);
        assert_eq!(cut(&series, 64), [vec![(8..10, 0)]]);
        assert!(cut(&series[..1], 64).is_empty());
    }
}
