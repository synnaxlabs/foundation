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
use types::frame::key_set::KeySet;
use types::frame::{self, Frame};
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
        mut places,
    }) = open(state, class, &mut home, receiver).await?
    else {
        sender.finish()?;
        return Ok(());
    };
    sender.send(reply(state, Reply::Opened)?).await?;
    loop {
        // Both futures are cancel-safe: a dropped `recv` keeps its message queued, and
        // `take` gives up a frame only in the poll that returns it.
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
                places.send(state, sender, &frame, set).await?;
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
    places: Places,
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
            let (session, credit) =
                Session::complete(state, slots.clone(), index, limit_bytes);
            (session, Some(credit))
        }
        Mode::Latest => (Session::latest(state, slots.clone(), index), None),
    };
    Ok(Some(Opened {
        session,
        credit,
        places: Places::new(slots, index),
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
/// frames, so a frame allocates only its blocks.
struct Places {
    /// The slot of each place: of each listing in the open, repeats too.
    slots: Box<[Slot]>,
    index: Slot,
    /// The place of each entry of the frame's key set.
    by_entry: Vec<Option<usize>>,
    /// The range of each place's series in the frame's body.
    by_place: Vec<Option<Range<usize>>>,
    series: Vec<Series>,
    parts: Vec<Part>,
}

impl Places {
    fn new(slots: Box<[Slot]>, index: Slot) -> Self {
        Self {
            by_place: vec![None; slots.len()],
            slots,
            index,
            by_entry: Vec::new(),
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
        let head = self.lay(frame, set);
        sender.send(reply(state, Reply::Head(head))?).await?;
        let largest = state.borrow().home.pool().largest();
        for run in self.runs(sender.bytes_max().min(largest)) {
            let mut block = alloc(state, run.len() * ends::LEN)?;
            ends::encode(
                run.iter().map(|series| (series.place, series.end)),
                &mut block,
            );
            sender.send(block.freeze()).await?;
        }
        let body = frame.body();
        let mut cut = Cut::default();
        while cut.next(&self.series, sender.bytes_max(), &mut self.parts) {
            sender.send_parts(body.clone(), &self.parts).await?;
        }
        Ok(())
    }

    /// Lays out the series of `frame` that the session has a place for, in place
    /// order, and gives the frame's head.
    fn lay(&mut self, frame: &Frame, set: &KeySet) -> Head {
        self.by_entry.clear();
        self.by_entry.resize(set.entries().len(), None);
        for (place, &slot) in self.slots.iter().enumerate() {
            if let Some(entry) = set.find(slot) {
                self.by_entry[entry].get_or_insert(place);
            }
        }
        self.by_place.fill(None);
        for ((entry, series), (_, end)) in frame.iter().zip(frame.ends()) {
            if let Some(place) = self.by_entry[entry] {
                self.by_place[place] = Some(end - series.len()..end);
            }
        }
        self.series.clear();
        let present = self
            .by_place
            .iter()
            .enumerate()
            .filter_map(|(place, range)| {
                let range = range.clone()?;
                let len = range.len();
                Some(((place, range), len))
            });
        let mut last_end = 0;
        for ((place, range), end) in frame::ends(present) {
            if let Some(last) = self.series.last_mut() {
                let start = end - range.len();
                last.zeros = u8::try_from(start - last_end).expect("a pad fits a u8");
            }
            last_end = end;
            self.series.push(Series {
                place: u32::try_from(place).expect("a place is a u32"),
                range,
                end: u32::try_from(end).expect("a frame's body fits a u32"),
                zeros: 0,
            });
        }
        let index = set
            .find(self.index)
            .expect("invariant: a session's frame holds its index");
        Head {
            path: frame.path(),
            range: frame
                .range(set.entries()[index].group)
                .expect("invariant: a frame holds the range of each group"),
            series: u32::try_from(self.series.len())
                .expect("a count of places is a u32"),
        }
    }

    /// The series that [`Self::lay`] gave, in runs whose ends fit `max` bytes.
    fn runs(&self, max: usize) -> std::slice::Chunks<'_, Series> {
        self.series.chunks(max / ends::LEN)
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

#[cfg(test)]
mod tests {
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

    /// Each series that `places` laid places, as `(place, range, end, zeros)`.
    fn laid(places: &Places) -> Vec<(u32, Range<usize>, u32, u8)> {
        places
            .series
            .iter()
            .map(|s| (s.place, s.range.clone(), s.end, s.zeros))
            .collect()
    }

    /// The frame's entries are in slot order (3, 1, 2, 5), and the places are 1, 2, 3,
    /// and 4, which the frame does not hold.
    #[test]
    fn lays_out_the_series_of_each_place_in_place_order() {
        let pool = block::Pool::heap(block::Config { budget: 1 << 20 });
        let mut interner = Interner::new();
        let [b, index, a, _, absent] =
            [3, 1, 2, 5, 4].map(|k| interner.slots().assign(key(k)));
        let mut places = Places::new([index, a, b, absent].into(), index);
        let (wide, set) = frame(&mut interner, &pool, &[2, 3, 5], &[3, 8, 5, 8]);
        let head = places.lay(&wide, &set);
        assert_eq!(
            laid(&places),
            [(0, 8..16, 8, 0), (1, 16..21, 13, 3), (2, 0..3, 19, 0)]
        );
        assert_eq!(head.path, Path::Live);
        assert_eq!(Some(head.range), wide.range(0));
        assert_eq!(head.series, 3);
        let (narrow, set) = frame(&mut interner, &pool, &[2], &[8, 5]);
        let head = places.lay(&narrow, &set);
        assert_eq!(laid(&places), [(0, 0..8, 8, 0), (1, 8..13, 13, 0)]);
        assert_eq!(head.series, 2);
    }

    /// The head carries the range of the session's index group, not the first group.
    #[test]
    fn gives_the_range_of_the_index_group_of_the_session() {
        let pool = block::Pool::heap(block::Config { budget: 1 << 20 });
        let mut interner = Interner::new();
        let [_, _, index, value] =
            [1, 2, 4, 5].map(|k| interner.slots().assign(key(k)));
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
        let mut places = Places::new([index, value].into(), index);
        let head = places.lay(&frame, &set);
        assert_eq!(Some(head.range), frame.range(1));
        assert_ne!(frame.range(0), frame.range(1));
    }

    /// A key listed twice has the place of its first listing, and the second listing
    /// holds a place with no series.
    #[test]
    fn gives_a_series_the_place_of_its_first_listing() {
        let pool = block::Pool::heap(block::Config { budget: 1 << 20 });
        let mut interner = Interner::new();
        let [index, a, b] = [1, 2, 3].map(|k| interner.slots().assign(key(k)));
        let mut places = Places::new([index, a, index, b].into(), index);
        let (frame, set) = frame(&mut interner, &pool, &[2, 3], &[8, 8, 8]);
        let head = places.lay(&frame, &set);
        assert_eq!(
            laid(&places),
            [(0, 0..8, 8, 0), (1, 8..16, 16, 0), (3, 16..24, 24, 0)]
        );
        assert_eq!(head.series, 3);
    }

    #[test]
    fn splits_the_ends_into_runs_that_fit_the_message_limit() {
        let pool = block::Pool::heap(block::Config { budget: 1 << 20 });
        let mut interner = Interner::new();
        let [index, a, b] = [1, 2, 3].map(|k| interner.slots().assign(key(k)));
        let mut places = Places::new([index, a, b].into(), index);
        let (wide, set) = frame(&mut interner, &pool, &[2, 3], &[8, 8, 8]);
        places.lay(&wide, &set);
        let runs = |max| places.runs(max).map(<[Series]>::len).collect::<Vec<_>>();
        assert_eq!(runs(16), [2, 1]);
        assert_eq!(runs(23), [2, 1]);
        assert_eq!(runs(24), [3]);
    }

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
