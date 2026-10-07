//! Remote reader sessions that a hub serves, and why one ends.

use std::cell::RefCell;
use std::fmt;
use std::future::poll_fn;
use std::ops::Range;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use block::{Block, Unique};
use transport::stream::{Incoming, Part, Receiver, Sender};
use transport::{Class, Code};
use types::channel::{self, Slot};
use types::frame::Frame;
use types::frame::key_set::{self, KeySet};
use types::hash;
use wire::header::MALFORMED;
use wire::hub::{BUSY, FAILED, FromReader, Head, Home, Mode, Reply, UNKNOWN, ends};

use crate::State;
use crate::reader::{Credit, Ended, Session};

/// Why [`Hub::serve`](crate::Hub::serve) ended a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A message that `wire::hub::Home` refuses. Code `MALFORMED`.
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
}

impl Error {
    /// The code that the stream stops with, or `None` when it is broken.
    fn code(&self) -> Option<Code> {
        match self {
            Self::Message(_)
            | Self::OneWay
            | Self::Class(_)
            | Self::ManyIndexes
            | Self::NoIndex => Some(Code(MALFORMED)),
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
                "the stream has class {class:?}, which is not the class of the open's mode"
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
    let Incoming {
        class,
        mut receiver,
        sender,
    } = incoming;
    let Some(mut sender) = sender else {
        receiver.stop(Code(MALFORMED));
        return Err(Error::OneWay);
    };
    let served = serve(state, class, &mut receiver, &mut sender).await;
    if let Err(error) = &served
        && let Some(code) = error.code()
    {
        receiver.stop(code);
        sender.reset(code);
    }
    served
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
        mut out,
    }) = open(state, class, &mut home, receiver).await?
    else {
        sender.finish()?;
        return Ok(());
    };
    sender.send(reply(state, Reply::Opened)?).await?;
    loop {
        let event = {
            let mut recv = pin!(receiver.recv());
            let mut take = pin!(session.take());
            poll_fn(|cx| match recv.as_mut().poll(cx) {
                Poll::Ready(message) => Poll::Ready(Event::Message(message)),
                Poll::Pending => take.as_mut().poll(cx).map(Event::Frame),
            })
            .await
        };
        match event {
            Event::Message(message) => {
                let Some(message) = message? else {
                    sender.finish()?;
                    return Ok(());
                };
                let FromReader::Credit(grant) = home.decode(&message)? else {
                    unreachable!(
                        "invariant: after the keys run, Home gives only credits"
                    );
                };
                credit
                    .as_ref()
                    .expect("invariant: Home refuses a credit in a latest session")
                    .grant(grant.limit_bytes);
            }
            Event::Frame(Ok((frame, set, _))) => {
                out.send(state, sender, &frame, set).await?;
            }
            Event::Frame(Err(Ended::Behind)) => {
                sender.send(reply(state, Reply::Behind)?).await?;
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
    Message(Result<Option<Block>, transport::Error>),
    Frame(Result<F, Ended>),
}

/// A session open at the home.
struct Opened {
    session: Session,
    /// The credit of a complete session.
    credit: Option<Credit>,
    out: Out,
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
    let (mut slots, mut seen, mut index) = (Vec::new(), hash::Set::default(), None);
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
            if seen.insert(assigned) {
                slots.push(assigned);
            }
        }
        if last {
            break;
        }
    }
    let index = index.and_then(|(_, slot)| slot).ok_or(Error::NoIndex)?;
    let slots: Box<[Slot]> = slots.into();
    let (session, credit) = match open.mode {
        Mode::Complete { limit_bytes } => {
            let (session, credit) =
                Session::complete(state, slots.clone(), index, limit_bytes);
            (session, Some(credit))
        }
        Mode::Latest => (Session::latest(state, slots.clone(), index), None),
    };
    Ok(Some(Opened {
        session,
        credit,
        out: Out::new(slots, index),
    }))
}

/// A block of the home's pool that holds `reply`.
fn reply(state: &RefCell<State>, reply: Reply) -> Result<Block, Error> {
    let mut block = alloc(state, reply.encoded_len())?;
    reply.encode(&mut block);
    Ok(block.freeze())
}

/// A block of `len` bytes, at most the largest, from the home's pool.
fn alloc(state: &RefCell<State>, len: usize) -> Result<Unique, Error> {
    state
        .borrow()
        .home
        .pool()
        .alloc(len)
        .map_err(|error| match error {
            block::Error::TooLarge { .. } => {
                unreachable!("invariant: a reply block is at most the largest")
            }
            block::Error::Exhausted { .. } | block::Error::Refused { .. } => {
                Error::Pool(error)
            }
        })
}

/// How a session sends each frame, through its places. It keeps its buffers across
/// frames, so a frame of a known key set allocates only its blocks.
struct Out {
    /// The slot of each place.
    slots: Box<[Slot]>,
    index: Slot,
    /// The key set of the last frame, the place of each of its entries, and the
    /// group of the session's index.
    set: Option<(key_set::Key, Vec<Option<usize>>, u32)>,
    /// The range of each place's series in the frame's body.
    by_place: Vec<Option<Range<usize>>>,
    series: Vec<Series>,
    parts: Vec<Part>,
}

impl Out {
    fn new(slots: Box<[Slot]>, index: Slot) -> Self {
        Self {
            by_place: vec![None; slots.len()],
            slots,
            index,
            set: None,
            series: Vec::new(),
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
        let group = self.place(set);
        let Some((_, places, _)) = &self.set else {
            unreachable!("invariant: place sets the key set");
        };
        self.by_place.fill(None);
        let mut start = 0;
        for (entry, end) in frame.ends() {
            if let Some(place) = places[entry] {
                self.by_place[place] = Some(start..end);
            }
            start = end.next_multiple_of(8);
        }
        self.series.clear();
        let mut end = 0_usize;
        for (place, range) in self.by_place.iter().enumerate() {
            let Some(range) = range else { continue };
            let start = end.next_multiple_of(8);
            if let Some(last) = self.series.last_mut() {
                last.zeros = u8::try_from(start - end).expect("a pad is under 8");
            }
            end = start + range.len();
            self.series.push(Series {
                place: u32::try_from(place).expect("a place is a u32"),
                range: range.clone(),
                end: u32::try_from(end).expect("a frame's body fits a u32"),
                zeros: 0,
            });
        }
        let head = Head {
            path: frame.path(),
            range: frame
                .range(group)
                .expect("invariant: a session's frame holds its index"),
            series: u32::try_from(self.series.len())
                .expect("a count of places is a u32"),
        };
        sender.send(reply(state, Reply::Head(head))?).await?;
        let largest = state.borrow().home.pool().largest();
        let per = sender.bytes_max().min(largest) / ends::LEN;
        let mut ends = self.series.iter().map(|series| (series.place, series.end));
        let mut remain = self.series.len();
        while remain > 0 {
            let count = remain.min(per);
            let mut block = alloc(state, count * ends::LEN)?;
            ends::encode(ends.by_ref(), &mut block);
            sender.send(block.freeze()).await?;
            remain -= count;
        }
        let body = frame.body();
        let mut cut = Cut::default();
        while cut.next(&self.series, sender.bytes_max(), &mut self.parts) {
            sender.send_parts(body.clone(), &self.parts).await?;
        }
        Ok(())
    }

    /// Makes `set` the key set of the last frame, and gives the group of the
    /// session's index in it.
    fn place(&mut self, set: &KeySet) -> u32 {
        match &self.set {
            Some((key, _, group)) if *key == set.key() => *group,
            _ => {
                let mut places = vec![None; set.entries().len()];
                for (place, &slot) in self.slots.iter().enumerate() {
                    if let Some(entry) = set.find(slot) {
                        places[entry] = Some(place);
                    }
                }
                let index = set
                    .find(self.index)
                    .expect("invariant: a session's frame holds its index");
                let group = set.entries()[index].group;
                self.set = Some((set.key(), places, group));
                group
            }
        }
    }
}

/// One series of a frame that a session sends.
#[derive(Clone, Debug)]
struct Series {
    place: u32,
    /// Its bytes in the frame's body.
    range: Range<usize>,
    /// Its end in the reader's body.
    end: u32,
    /// The zeros after it in the reader's body, to the start of the next series.
    zeros: u8,
}

/// Where the cut of a body into messages stands: the series and how many of its bytes,
/// then zeros, went into earlier messages.
#[derive(Debug, Default)]
struct Cut {
    at: usize,
    done: usize,
}

impl Cut {
    /// Fills `parts` with the next message of the body of `series`, at most `max`
    /// bytes. Gives `false` once the body is sent.
    fn next(&mut self, series: &[Series], max: usize, parts: &mut Vec<Part>) -> bool {
        parts.clear();
        let mut room = max;
        while room > 0
            && let Some(at) = series.get(self.at)
        {
            let len = at.range.len();
            let total = len + usize::from(at.zeros);
            let take = (total - self.done).min(room);
            let (from, to) = (self.done, self.done + take);
            if take > 0 {
                parts.push(Part {
                    range: at.range.start + from.min(len)..at.range.start + to.min(len),
                    zeros: u8::try_from(to.max(len) - from.max(len))
                        .expect("a pad is under 8"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn series(ranges: &[(Range<usize>, u8)]) -> Vec<Series> {
        ranges
            .iter()
            .zip(0..)
            .map(|((range, zeros), place)| Series {
                place,
                range: range.clone(),
                end: 0,
                zeros: *zeros,
            })
            .collect()
    }

    /// Each message of the cut, as `(range, zeros)` of each part.
    fn cut(series: &[Series], max: usize) -> Vec<Vec<(Range<usize>, u8)>> {
        let (mut cut, mut parts, mut messages) =
            (Cut::default(), Vec::new(), Vec::new());
        while cut.next(series, max, &mut parts) {
            messages.push(parts.iter().map(|p| (p.range.clone(), p.zeros)).collect());
        }
        messages
    }

    #[test]
    fn sends_a_body_that_fits_in_one_message() {
        let series = series(&[(0..5, 3), (16..24, 0)]);
        assert_eq!(cut(&series, 64), [vec![(0..5, 3), (16..24, 0)]]);
    }

    #[test]
    fn cuts_a_series_and_its_zeros_at_the_message_limit() {
        let series = series(&[(0..5, 3), (16..24, 0)]);
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
        let series = series(&[(8..8, 0), (8..10, 0), (16..16, 0)]);
        assert_eq!(cut(&series, 64), [vec![(8..10, 0)]]);
        assert!(cut(&series[..1], 64).is_empty());
    }
}
