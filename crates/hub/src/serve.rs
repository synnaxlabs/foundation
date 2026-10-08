//! The streams that a [`Link`](crate::Link) serves: remote reader sessions, and the
//! hello and request streams of a client, and why one ends.

pub(crate) mod client;

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use block::{Block, Unique};
use transport::stream::{Incoming, Part, Receiver, Sender};
use transport::{Class, Code};
use types::channel::{self, Slot};
use types::frame::key_set::KeySet;
use types::frame::{self, Frame, Placed};
use wire::header::MALFORMED;
use wire::hub::client::STALE;
use wire::hub::{BUSY, FAILED, FromReader, Head, Home, Mode, UNKNOWN, ends};

use crate::State;
use crate::reader::{Credit, Ended, Session};

pub use client::{Reply, Request};

/// Why [`Link::serve`](crate::Link::serve) ended a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A message that `wire::hub` refuses, or a client body that ended early
    /// ([`wire::hub::Error::Unfinished`]). Code `MALFORMED`.
    Message(wire::hub::Error),
    /// The peer opened the stream one way. Code `MALFORMED`.
    OneWay,
    /// The stream's class is not the class of the open's mode. Code `MALFORMED`.
    Class(Class),
    /// The keys of the open are on more than one index. Code `MALFORMED`.
    ManyIndexes,
    /// The keys of the open do not hold their index. Code `MALFORMED`.
    NoIndex,
    /// The open names a channel that this node does not know. Code `UNKNOWN`.
    Unknown(channel::Key),
    /// The home's buffer failed. Code `FAILED`.
    Buffer(env::files::Error),
    /// The home's pool had no block for a reply (`Exhausted` or `Refused`). Code
    /// `BUSY`. A later open can succeed.
    Pool(block::Error),
    /// The stream or its session failed. No code.
    Stream(transport::Error),
    /// `access` refused a hello or a request, or the hub refused the session:
    /// `Unsynced` when it has no mesh time for a challenge, and `Expired` when the
    /// admitted hello expires. Code: the one that CLIENT HELLO gives for the error,
    /// `REFUSED` for each that tells about the spec.
    Access(access::proof::Error),
    /// The hello does not echo the nonce of the last challenge. Code `STALE`.
    Stale,
    /// A request stream before the link admitted a hello. Code `MALFORMED`.
    Unadmitted,
    /// A request stream while another request of the link waits for its reply. Code
    /// `MALFORMED`.
    Pending,
}

impl Error {
    /// The code that the stream stops with, or `None` when it is broken.
    pub(crate) fn code(&self) -> Option<Code> {
        match self {
            Self::Message(_)
            | Self::OneWay
            | Self::Class(_)
            | Self::ManyIndexes
            | Self::NoIndex
            | Self::Unadmitted
            | Self::Pending => Some(Code(MALFORMED)),
            Self::Access(error) => Some(Code(client::code(error))),
            Self::Stale => Some(Code(STALE)),
            Self::Unknown(_) => Some(Code(UNKNOWN)),
            Self::Buffer(_) => Some(Code(FAILED)),
            Self::Pool(_) => Some(Code(BUSY)),
            Self::Stream(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(error) => write!(f, "a hub message is not valid: {error}"),
            Self::OneWay => f.write_str("a hub session needs a two-way stream"),
            Self::Class(class) => write!(
                f,
                "the stream has class {class:?}, which is not the class of the open's \
                 mode"
            ),
            Self::ManyIndexes => {
                f.write_str("the open names channels on more than one index")
            }
            Self::NoIndex => {
                f.write_str("the open does not name the index of its channels")
            }
            Self::Unknown(key) => {
                write!(
                    f,
                    "the open names channel {key}, which this node does not know"
                )
            }
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
                "the program sent a request before the node admitted a hello",
            ),
            Self::Pending => f.write_str(
                "the program sent a request while another request waits for its reply",
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<wire::hub::Error> for Error {
    fn from(error: wire::hub::Error) -> Self {
        Self::Message(error)
    }
}

impl From<transport::Error> for Error {
    fn from(error: transport::Error) -> Self {
        Self::Stream(error)
    }
}

/// Serves the session on `incoming`, and stops the stream with the code of the error
/// that ends it.
pub(crate) async fn run(
    state: &Rc<RefCell<State>>,
    incoming: Incoming,
) -> Result<(), Error> {
    let class = incoming.class;
    let (mut receiver, mut sender) = halves(incoming)?;
    let served = serve(state, class, &mut receiver, &mut sender).await;
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

async fn serve(
    state: &Rc<RefCell<State>>,
    class: Class,
    receiver: &mut Receiver,
    sender: &mut Sender,
) -> Result<(), Error> {
    let mut home = Home::default();
    let Some(Opened {
        mut session,
        credit,
        mut layout,
    }) = open(state, class, &mut home, receiver).await?
    else {
        sender.finish()?;
        return Ok(());
    };
    sender.send(reply(state, wire::hub::Reply::Opened)?).await?;
    // `peer` lives across turns, so a frame never drops a read of the peer. `take`
    // gives up a frame only in the poll that returns it.
    let mut peer = pin!(peer(receiver, home, credit.as_ref()));
    loop {
        let event = {
            let mut take = pin!(session.take());
            // `take` polls after `peer` in each poll: a grant wakes no session, so a
            // frame that waits for credit goes out only at the take after its grant
            // (CREDIT RULES).
            poll_fn(|cx| match peer.as_mut().poll(cx) {
                Poll::Ready(finished) => Poll::Ready(Event::Finished(finished)),
                Poll::Pending => take.as_mut().poll(cx).map(Event::Frame),
            })
            .await
        };
        match event {
            Event::Finished(finished) => {
                finished?;
                sender.finish()?;
                return Ok(());
            }
            Event::Frame(Ok((frame, set, _))) => {
                layout.send(state, sender, &frame, set).await?;
            }
            Event::Frame(Err(Ended::Behind)) => {
                sender.send(reply(state, wire::hub::Reply::Behind)?).await?;
                sender.finish()?;
                return Ok(());
            }
            Event::Frame(Err(Ended::Buffer(error))) => {
                return Err(Error::Buffer(error));
            }
        }
    }
}

enum Event<F> {
    /// The peer finished, or broke the stream or HUB WIRE.
    Finished(Result<(), Error>),
    Frame(Result<F, Ended>),
}

/// Reads what the peer sends after the keys run, and grants each credit. Returns when
/// the peer finishes.
async fn peer(
    receiver: &mut Receiver,
    mut home: Home,
    credit: Option<&Credit>,
) -> Result<(), Error> {
    while let Some(message) = receiver.recv().await? {
        let FromReader::Credit(grant) = home.decode(&message)? else {
            unreachable!("invariant: after the keys run, Home gives only credits");
        };
        credit
            .expect("invariant: Home refuses a credit in a latest session")
            .grant(grant.limit_bytes);
    }
    Ok(())
}

/// A session open at the home.
struct Opened {
    session: Session,
    /// The credit of a complete session.
    credit: Option<Credit>,
    layout: Layout,
}

/// Reads the open and its keys, checks each key as it arrives, and opens the session
/// in the order of the keys. Gives `None` when the peer finishes first.
async fn open(
    state: &Rc<RefCell<State>>,
    class: Class,
    home: &mut Home,
    receiver: &mut Receiver,
) -> Result<Option<Opened>, Error> {
    let Some(message) = receiver.recv().await? else {
        return Ok(None);
    };
    let FromReader::Open(open) = home.decode(&message)? else {
        unreachable!("invariant: the first message that Home gives is an open");
    };
    let wanted = match open.mode {
        Mode::Latest => Class::Latest,
        Mode::Complete { .. } => Class::Complete,
    };
    if class != wanted {
        return Err(Error::Class(class));
    }
    let (mut slots, mut index) = (Vec::new(), None);
    loop {
        let Some(message) = receiver.recv().await? else {
            return Ok(None);
        };
        let FromReader::Keys { keys, last } = home.decode(&message)? else {
            unreachable!("invariant: Home gives the keys run after the open");
        };
        let mut state = state.borrow_mut();
        let state = &mut *state;
        for key in keys {
            let of = *state.indexes.get(&key).ok_or(Error::Unknown(key))?;
            let (first, slot) = index.get_or_insert((of, None));
            if *first != of {
                return Err(Error::ManyIndexes);
            }
            let assigned = state.interner.slots().assign(key);
            if key == of {
                *slot = Some(assigned);
            }
            slots.push(assigned);
        }
        if last {
            break;
        }
    }
    let index = index.and_then(|(_, slot)| slot).ok_or(Error::NoIndex)?;
    let slots: Box<[Slot]> = slots.into();
    let (session, credit) = match open.mode {
        Mode::Complete { limit_bytes } => {
            let charge = ::home::reader::complete::Charge::Places(slots.clone());
            let (session, credit) =
                Session::complete(state, slots.clone(), index, limit_bytes, charge);
            (session, Some(credit))
        }
        Mode::Latest => (Session::latest(state, slots.clone(), index), None),
    };
    Ok(Some(Opened {
        session,
        credit,
        layout: Layout::new(slots, index),
    }))
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
    state
        .borrow()
        .home
        .pool()
        .alloc(len)
        .map_err(|error| match error {
            block::Error::TooLarge { .. } => {
                unreachable!(
                    "invariant: no caller asks for more than the largest block"
                )
            }
            block::Error::Exhausted { .. } | block::Error::Refused { .. } => {
                Error::Pool(error)
            }
        })
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
        let pool = block::Pool::heap(block::Config { budget: 1 << 20 });
        let mut interner = Interner::new();
        let index = interner.slots().assign(key(1));
        let (frame, set) = frame(&mut interner, &pool, &[2, 3], &[8, 8, 8]);
        let head = head(&frame, &set, index, 2);
        assert_eq!(head.path, Path::Live);
        assert_eq!(Some(head.range), frame.range(0));
        assert_eq!(head.series, 2);
    }

    /// The head carries the range of the session's index group, not the first group.
    #[test]
    fn gives_the_range_of_the_index_group_of_the_session() {
        let pool = block::Pool::heap(block::Config { budget: 1 << 20 });
        let mut interner = Interner::new();
        let [_, _, index, _] = [1, 2, 4, 5].map(|k| interner.slots().assign(key(k)));
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
