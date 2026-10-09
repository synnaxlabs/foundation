//! The streams of a connection: whole messages in order over noq-proto's streams. The
//! side that opens a stream starts it with its class byte. The other side queues it
//! for accept at the first byte of its first message.

use std::cell::OnceCell;
use std::collections::VecDeque;
use std::mem;
use std::ops::Range;
use std::rc::Rc;
use std::slice;
use std::task::Poll;
use std::time::{Duration, Instant};

use block::Block;
use bytes::Bytes;
use noq_proto::{
    ClosedStream, Dir, FinishError, ReadError, SendStream, StreamEvent, StreamId,
    VarInt, WriteError,
};

use super::connection::{self, Fault};
use super::hello::{self, Hello};
use super::{Body, Event};
use types::hash::Map;

use crate::message::{self, Reader, Step};
use crate::stream::{Part, ZEROS};
use crate::{Class, Code, Error, varint};

/// Names one stream of a connection of an [`Endpoint`](super::Endpoint).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub(super) connection: connection::Key,
    pub(super) id: StreamId,
}

/// The sending half of a stream. The caller gives it to each write call, and to
/// [`Endpoint::reset`](super::Endpoint::reset) to end it. The connection keeps the
/// stream's message in hand. It sends the rest by itself once the stream finished or
/// when the message came from a write that does not wait, and else at the caller's
/// next write. Dropping the sender does nothing to the stream, so finish or reset it
/// first: until then, the message in hand keeps its send budget and its turn.
#[derive(Debug)]
pub(crate) struct Sender {
    key: Key,
    ended: bool,
    closed: Closed,
    /// The peer's largest message.
    bytes_max: usize,
}

/// What a connection keeps of a stream that this side sends on, until the stream
/// finishes or resets.
#[derive(Debug)]
struct Half {
    key: Key,
    /// The stream needs no class byte: it has one, or it is a reply.
    started: bool,
    /// The class byte, then the length prefix of the message in hand.
    header: [u8; 1 + varint::BYTES_MAX],
    /// The bytes of `header` that the stream has not taken.
    unsent: Range<usize>,
    /// The block of the message in hand.
    block: Bytes,
    /// The rest of a chunk of the message in hand that the stream took in part.
    chunk: Bytes,
    /// The parts of the message in hand that the stream has not taken all of. Empty
    /// during the write that gives the message, which reads the caller's parts.
    parts: Kept,
    /// The bytes of the message in hand that the stream has not taken.
    body: usize,
    /// The send budget of the message in hand.
    claim: Claim,
    /// Who writes the rest of the message in hand.
    rest: Rest,
    /// The stream waits its turn in [`Turns`].
    waiting: bool,
    /// The caller has an [`Event::Writable`] for the stream that no write answered.
    notified: bool,
    /// What each later write and finish gives: the peer stopped the stream, or a
    /// cancel reset it.
    ended: Option<Error>,
}

/// Who writes the rest of a stream's message in hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rest {
    /// The caller, at its next write.
    Caller,
    /// The stream, with no call from the caller: the message came from a write that
    /// does not wait.
    Pump,
    /// The stream, which then finishes: the caller finished it.
    Finish,
}

/// The receiving half of a stream. The caller gives it to each read, and to
/// [`Endpoint::stop`](super::Endpoint::stop) to end it. Dropping it does nothing to the
/// stream: the message in the reader keeps its receive budget, or its place among the
/// streams that wait for room, until the connection ends.
#[derive(Debug)]
pub(crate) struct Receiver {
    key: Key,
    reader: Reader,
    /// The receive budget of the message in the reader.
    claim: Claim,
    end: Option<End>,
    closed: Closed,
}

/// The error of a connection's [`Event::Closed`], once it ended. Each handle of the
/// connection keeps it, so it outlives the drain.
type Closed = Rc<OnceCell<Error>>;

/// How a stream that a [`Receiver`] reads ended.
#[derive(Clone, Copy, Debug)]
enum End {
    Finished,
    Reset(Code),
}

/// A stream the peer opened, with a sender when it goes both ways.
#[derive(Debug)]
pub(crate) struct Incoming {
    pub(crate) class: Class,
    pub(crate) receiver: Receiver,
    pub(crate) sender: Option<Sender>,
}

/// What a connection keeps of its streams between calls.
#[derive(Debug)]
pub(super) struct Streams {
    /// This side's limits, which its hello carries.
    own: Hello,
    /// This side sent its hello.
    greeted: bool,
    /// Until the peer's hello arrives, no stream opens or is accepted.
    peer: hello::Peer,
    /// Streams the peer opened whose first message has not started to arrive.
    arriving: Vec<Arriving>,
    /// Streams the peer opened that the caller has not accepted, by class byte,
    /// oldest first, each with the first byte of its first message.
    incoming: [VecDeque<(StreamId, u8)>; 4],
    /// Each stream that this side sends on and has not finished.
    halves: Map<StreamId, Half>,
    sending: Sending,
    /// The messages that hold a block and have not gone to the caller.
    receiving: Budget,
    closed: Closed,
}

/// A stream the peer opened whose first message has not started to arrive.
#[derive(Debug)]
struct Arriving {
    id: StreamId,
    /// The class, once its byte arrived.
    class: Option<Class>,
    /// The code the peer stopped the reply half of a two-way stream with.
    stopped: Option<Code>,
}

/// The send budget and the turn of the streams this side sends on.
#[derive(Debug)]
struct Sending {
    /// The messages this side has started and the streams have not taken in full.
    /// They stay within the peer's window, so the peer's receive budget always has
    /// room for one more message. Empty until the peer's hello.
    budget: Budget,
    /// The streams whose message noq-proto has not taken in full.
    turns: Turns,
    /// The `Complete` share of the turn and of the send budget.
    share: Share,
    /// The streams that got room or the turn, oldest first. [`Streams::pump`] wakes
    /// their callers, or writes what they hold.
    woken: VecDeque<StreamId>,
    /// The stretch that a write copies, kept across writes.
    buffer: Vec<u8>,
}

/// The message bytes that one direction of a connection counts, and the claims that
/// found no room. Room goes to waiting claims in the [`Order`] that each call gives,
/// then oldest first.
#[derive(Debug)]
struct Budget {
    max: usize,
    used: usize,
    /// The claims of each class, by rank, that hold room their stream took.
    held: [usize; 4],
    /// The claims of each class, by rank, that got room their stream has not taken.
    given: [usize; 4],
    /// The claims that wait for room, by class rank, oldest first.
    waiting: [VecDeque<Wait>; 4],
    /// The next ticket of each class.
    tickets: [u64; 4],
    /// For each class, every ticket under this one got room or ended. Room a claim
    /// got counts in `used` until the claim takes it or ends.
    granted: [u64; 4],
}

/// A claim that waits in a [`Budget`].
#[derive(Debug)]
struct Wait {
    stream: Key,
    ticket: u64,
    bytes: usize,
}

/// The part of a [`Budget`] that the message of one stream holds or waits for.
#[derive(Debug)]
struct Claim {
    /// The stream's class, which orders the claim among those that wait.
    class: Class,
    state: State,
}

#[derive(Clone, Copy, Debug)]
enum State {
    Idle,
    /// The claim waits for `bytes`, or got them and has not taken them.
    Queued {
        ticket: u64,
        bytes: usize,
    },
    /// The message counts these bytes.
    Held(usize),
}

/// The order of the classes in turn and for room, first to last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Order([Class; 4]);

/// The senders that wait for noq-proto to take more of their message. Only the
/// first in turn writes.
#[derive(Debug, Default)]
struct Turns {
    /// The senders that wait, by class rank, oldest first.
    queues: [VecDeque<Key>; 4],
}

/// The bytes that `Complete` is owed, which order the turn and the room of the send
/// budget.
#[derive(Debug, Default)]
struct Share {
    /// [`LATEST_COST`] for each byte of `Latest` that noq-proto took, less each byte
    /// of `Complete`. `Complete` goes ahead of `Latest` while it is positive.
    owed: isize,
}

/// The bytes of `Complete` that noq-proto takes for each byte of `Latest`, while both
/// compete for the send budget.
const LATEST_COST: isize = 3;

impl Sender {
    /// A sender for `key`, to a peer whose largest message is `bytes_max`.
    fn new(key: Key, bytes_max: usize, closed: &Closed) -> Self {
        Self {
            key,
            ended: false,
            closed: Rc::clone(closed),
            bytes_max,
        }
    }

    /// The stream this sender writes.
    pub(crate) fn key(&self) -> Key {
        self.key
    }

    /// The peer's largest message.
    pub(crate) fn bytes_max(&self) -> usize {
        self.bytes_max
    }

    /// Whether [`Endpoint::finish`](super::Endpoint::finish) or
    /// [`Endpoint::reset`](super::Endpoint::reset) took it.
    pub(crate) fn ended(&self) -> bool {
        self.ended
    }

    /// The error of the connection's [`Event::Closed`], once it ended.
    pub(super) fn closed(&self) -> Option<&Error> {
        self.closed.get()
    }

    /// Marks the stream finished or reset.
    pub(super) fn end(&mut self) {
        self.ended = true;
    }

    /// # Panics
    ///
    /// After `end`.
    pub(super) fn check_open(&self) {
        assert!(!self.ended, "a sender is used after finish or reset");
    }
}

impl Half {
    /// The half of `key`, a stream this side opened, that starts it with `class`'s
    /// byte.
    fn new(key: Key, class: Class) -> Self {
        let mut header = [0; 1 + varint::BYTES_MAX];
        header[0] = byte(class);
        Self {
            started: false,
            header,
            ..Self::reply(key, class, None)
        }
    }

    /// The reply half of `key`, a stream of `class` that the peer opened and stopped
    /// with `stopped`, if it did.
    fn reply(key: Key, class: Class, stopped: Option<Code>) -> Self {
        Self {
            key,
            started: true,
            header: [0; 1 + varint::BYTES_MAX],
            unsent: 0..0,
            block: Bytes::new(),
            chunk: Bytes::new(),
            parts: Kept::default(),
            body: 0,
            claim: Claim::new(class),
            rest: Rest::Caller,
            waiting: false,
            notified: false,
            ended: stopped.map(|code| Error::Stopped { code }),
        }
    }

    /// Takes `block` as the block of the message in hand, a message of `bytes`, after
    /// its header, with `rest` to write what the stream does not take now. The next
    /// write gives the message's parts. The half holds no part of a message.
    fn load(&mut self, block: Block, bytes: usize, rest: Rest) {
        self.rest = rest;
        self.block = Bytes::from_owner(Body(block));
        self.body = bytes;
        let prefix = message::prefix(bytes);
        let start = usize::from(self.started);
        self.started = true;
        self.header[1..=prefix.len()].copy_from_slice(&prefix);
        self.unsent = start..prefix.len() + 1;
    }

    fn holds(&self) -> bool {
        self.left() > 0
    }

    /// Whether a byte of the message in hand went, its header included.
    fn sent(&self) -> bool {
        // Index 0 of `header` is the class byte, which belongs to no message.
        self.unsent.start > 1
    }

    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream, or [`Error::Reset`] with
    /// `Code(0)` after a cancel reset it.
    fn check(&self) -> Result<(), Error> {
        self.ended.clone().map_or(Ok(()), Err)
    }

    /// The bytes of the message in hand, with its header, that the stream has not
    /// taken.
    fn left(&self) -> usize {
        self.unsent.len() + self.body
    }

    /// Writes what the half holds to `send` until it holds nothing, with `given` the
    /// parts of the message in hand when the caller's write gives them, else `None`.
    /// Then keeps the parts it did not take. It checks neither the send budget nor
    /// the turn.
    ///
    /// # Errors
    ///
    /// The error of the write that took nothing.
    fn write(
        &mut self,
        send: &mut SendStream<'_>,
        buffer: &mut Vec<u8>,
        given: Option<&[Part]>,
    ) -> Result<(), WriteError> {
        let mut parts = mem::take(&mut self.parts);
        let mut left = Left::new(given.unwrap_or(parts.left()));
        let written = self.write_from(send, buffer, &mut left);
        let (head, tail) = (left.head, left.tail.len());
        if self.body == 0 {
            parts.clear();
        } else {
            parts.keep(given, head, tail);
        }
        self.parts = parts;
        written
    }

    /// Keeps `given`, when `Some`, as the parts that the stream did not take.
    fn hold(&mut self, given: Option<&[Part]>) {
        if let Some(given) = given {
            self.parts.hold(given);
        }
    }

    /// Writes the header, the rest of the chunk in hand, then the parts that `left`
    /// holds, until `send` takes no more, and moves `left` past what it took.
    ///
    /// # Errors
    ///
    /// The error of the write that took nothing.
    fn write_from(
        &mut self,
        send: &mut SendStream<'_>,
        buffer: &mut Vec<u8>,
        left: &mut Left<'_>,
    ) -> Result<(), WriteError> {
        while !self.unsent.is_empty() {
            self.unsent.start += send.write(&self.header[self.unsent.clone()])?;
        }
        loop {
            while !self.chunk.is_empty() {
                let chunk = &mut slice::from_mut(&mut self.chunk);
                self.body -= send.write_chunks(chunk)?;
            }
            if !left.skip() {
                return Ok(());
            }
            let (piece, after) = left.piece(&self.block, buffer);
            match piece {
                Piece::Chunk(chunk) => (self.chunk, *left) = (chunk, after),
                Piece::Copied(bytes) => {
                    let written = send.write(bytes)?;
                    self.body -= written;
                    if written == bytes.len() {
                        *left = after;
                    } else {
                        left.advance(written);
                    }
                }
            }
        }
    }
}

/// The longest chunk that noq-proto copies into its own buffer: `MAX_COMBINE` of its
/// `send_buffer` in 1.3.0. It keeps a longer chunk until the ACK.
const COPIED_MAX: usize = 1452;

/// The parts of a message that the stream has not taken all of, the first one cut at
/// the first byte not taken. The list keeps its capacity.
#[derive(Debug, Default)]
struct Kept {
    parts: Vec<Part>,
    /// The index in `parts` of the first part left.
    first: usize,
}

impl Kept {
    /// The parts left.
    fn left(&self) -> &[Part] {
        &self.parts[self.first..]
    }

    /// Keeps the parts from the one that `head` cuts on, with `tail` parts after it:
    /// of `given` when `Some`, which the list must then be empty for, else of
    /// [`Kept::left`].
    fn keep(&mut self, given: Option<&[Part]>, head: Part, tail: usize) {
        if let Some(given) = given {
            self.parts.push(head);
            self.parts.extend_from_slice(&given[given.len() - tail..]);
        } else {
            self.first = self.parts.len() - tail - 1;
            self.parts[self.first] = head;
        }
    }

    /// Keeps all of `given`, which the list must be empty for.
    fn hold(&mut self, given: &[Part]) {
        self.parts.extend_from_slice(given);
    }

    /// Keeps no part.
    fn clear(&mut self) {
        self.parts.clear();
        self.first = 0;
    }
}

/// The parts of a message that the stream has not taken: `head`, cut at the first
/// byte not taken, then `tail`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Left<'a> {
    head: Part,
    tail: &'a [Part],
}

/// One write of a message of parts.
enum Piece<'a> {
    /// Over [`COPIED_MAX`] bytes, which noq-proto keeps: a run, as a slice of the
    /// block, or a stretch of shorter runs and zeros, copied into a new buffer.
    Chunk(Bytes),
    /// A stretch of at most [`COPIED_MAX`] bytes, which noq-proto copies: one range
    /// of the block, or the parts copied into one buffer.
    Copied(&'a [u8]),
}

impl<'a> Left<'a> {
    /// All of `parts`.
    fn new(parts: &'a [Part]) -> Self {
        match parts {
            [head, tail @ ..] => Self {
                head: head.clone(),
                tail,
            },
            [] => Self {
                head: Part {
                    range: 0..0,
                    zeros: 0,
                },
                tail: &[],
            },
        }
    }

    /// Moves to the next part. False at the last one, which it leaves.
    fn pop(&mut self) -> bool {
        let Some((head, tail)) = self.tail.split_first() else {
            return false;
        };
        (self.head, self.tail) = (head.clone(), tail);
        true
    }

    /// Moves past each part at the head with no bytes left. False when no byte is
    /// left.
    fn skip(&mut self) -> bool {
        while self.head.range.is_empty() && self.head.zeros == 0 {
            if !self.pop() {
                return false;
            }
        }
        true
    }

    /// Moves past `bytes` more bytes.
    ///
    /// # Panics
    ///
    /// When fewer bytes are left.
    fn advance(&mut self, mut bytes: usize) {
        loop {
            let range = self.head.range.len().min(bytes);
            self.head.range.start += range;
            let zeros = u8::try_from(bytes - range).unwrap_or(u8::MAX);
            let zeros = zeros.min(self.head.zeros);
            self.head.zeros -= zeros;
            bytes -= range + usize::from(zeros);
            if bytes == 0 {
                return;
            }
            assert!(self.pop(), "a write took more than the message has left");
        }
    }

    /// The run of the block at the head: the head's range and those of the adjacent
    /// parts after it with no zeros between, and what is left after the run.
    fn run(&self) -> (Range<usize>, Self) {
        let mut left = self.clone();
        let (start, mut end) = (left.head.range.start, left.head.range.end);
        while left.head.zeros == 0 {
            match left.tail {
                [next, tail @ ..]
                    if next.range.start == end || next.range.is_empty() =>
                {
                    if !next.range.is_empty() {
                        end = next.range.end;
                    }
                    (left.head, left.tail) = (next.clone(), tail);
                }
                _ => break,
            }
        }
        left.head.range.start = left.head.range.end;
        (start..end, left)
    }

    /// The next write of `block`, and what is left after it: a run over
    /// [`COPIED_MAX`] bytes, else the stretch of shorter runs and zeros up to the
    /// next long run, copied into `buffer`. A stretch of at most [`COPIED_MAX`]
    /// bytes is one range of `block` when it is the last part, with no zeros, else
    /// `buffer`. A longer one is a copy of `buffer` of its length.
    fn piece<'b>(
        &self,
        block: &'b Bytes,
        buffer: &'b mut Vec<u8>,
    ) -> (Piece<'b>, Self) {
        if self.tail.is_empty() && self.head.zeros == 0 {
            let range = self.head.range.clone();
            let mut after = self.clone();
            after.head.range.start = range.end;
            if range.len() <= COPIED_MAX {
                return (Piece::Copied(&block[range]), after);
            }
            return (Piece::Chunk(block.slice(range)), after);
        }
        buffer.clear();
        let after = self.stretch(block, buffer);
        if buffer.is_empty() {
            let (range, after) = self.run();
            return (Piece::Chunk(block.slice(range)), after);
        }
        if buffer.len() > COPIED_MAX {
            // Not `Bytes::copy_from_slice`: with a second caller, it stays out of line
            // in noq-proto's `SendStream::write`, and each long run costs about 12 ns
            // more on the release profile.
            return (Piece::Chunk(Bytes::from(buffer.clone())), after);
        }
        (Piece::Copied(buffer), after)
    }

    /// Appends to `into` the ranges of `block` and the zeros of the stretch at the
    /// head, its shorter runs and zeros up to the next run over [`COPIED_MAX`]
    /// bytes, and gives what is left after it. Appends nothing when the head starts
    /// a long run.
    fn stretch(&self, block: &[u8], into: &mut Vec<u8>) -> Self {
        // Where the run in hand starts in `into`, the index of its first part, and
        // where its last range ends in the block.
        let (mut run, mut end) = ((into.len(), 0_usize), None);
        let (mut part, mut index) = (&self.head, 0);
        loop {
            let range = &part.range;
            if !range.is_empty() {
                if end != Some(range.start) {
                    run = (into.len(), index);
                }
                if into.len() - run.0 + range.len() > COPIED_MAX {
                    into.truncate(run.0);
                    return match run.1.checked_sub(1) {
                        Some(at) => Self::new(&self.tail[at..]),
                        None => self.clone(),
                    };
                }
                into.extend_from_slice(&block[range.clone()]);
                end = Some(range.end);
            }
            if part.zeros > 0 {
                into.extend_from_slice(&ZEROS[..usize::from(part.zeros)]);
                end = None;
            }
            let Some(next) = self.tail.get(index) else {
                return Self::new(&[]);
            };
            (part, index) = (next, index + 1);
        }
    }
}

/// The size rule of every send.
///
/// # Errors
///
/// [`Error::TooLarge`] when a message of `bytes` is over `bytes_max`, the peer's
/// largest message.
pub(super) fn check_size(bytes: usize, bytes_max: usize) -> Result<(), Error> {
    if bytes > bytes_max {
        return Err(Error::TooLarge { bytes, bytes_max });
    }
    Ok(())
}

impl Receiver {
    /// A receiver for `key`, a stream of `class`, that reads with `reader`.
    fn new(key: Key, class: Class, reader: Reader, closed: &Closed) -> Self {
        Self {
            key,
            reader,
            claim: Claim::new(class),
            end: None,
            closed: Rc::clone(closed),
        }
    }

    /// The stream this receiver reads.
    pub(crate) fn key(&self) -> Key {
        self.key
    }

    /// The stream's class.
    pub(crate) fn class(&self) -> Class {
        self.claim.class
    }

    /// Drops the message in hand, so that the receiver holds no bytes of it.
    pub(super) fn clear(&mut self) {
        self.reader.clear();
    }

    /// What each read gives after the stream ended, once it has.
    pub(super) fn ended<T>(&self) -> Option<Result<Poll<Option<T>>, Error>> {
        match self.end? {
            End::Finished => Some(Ok(Poll::Ready(None))),
            End::Reset(code) => Some(Err(Error::Reset { code })),
        }
    }

    /// The error of the connection's [`Event::Closed`], once it ended.
    pub(super) fn closed(&self) -> Option<&Error> {
        self.closed.get()
    }
}

impl Claim {
    fn new(class: Class) -> Self {
        Self {
            class,
            state: State::Idle,
        }
    }
}

impl Budget {
    fn new(max: usize) -> Self {
        Self {
            max,
            used: 0,
            held: [0; 4],
            given: [0; 4],
            waiting: Default::default(),
            tickets: [0; 4],
            granted: [0; 4],
        }
    }

    /// Whether `claim` waits for room.
    fn waits(&self, claim: &Claim) -> bool {
        match claim.state {
            State::Queued { ticket, .. } => ticket >= self.granted[claim.class.rank()],
            State::Idle | State::Held(_) => false,
        }
    }

    /// The claims that queued for room since this budget was made.
    fn queued(&self) -> u64 {
        self.tickets.iter().sum()
    }

    /// Whether a claim of `class` holds room, taken or not.
    fn holds(&self, class: Class) -> bool {
        self.held[class.rank()] + self.given[class.rank()] > 0
    }

    /// Whether a claim of `class` holds room its stream took, or waits for room.
    fn competes(&self, class: Class) -> bool {
        self.held[class.rank()] > 0 || !self.waiting[class.rank()].is_empty()
    }

    /// Charges `bytes` to `claim`, which holds none, when they fit now and no claim
    /// of its class or a class ahead of it in `order` waits.
    fn admit(&mut self, bytes: usize, claim: &mut Claim, order: Order) -> bool {
        let mut ahead = order.through(claim.class);
        let first = ahead.all(|class| self.waiting[class.rank()].is_empty());
        let fits = first && bytes <= self.max - self.used;
        if fits {
            self.used += bytes;
            self.held[claim.class.rank()] += 1;
            claim.state = State::Held(bytes);
        }
        fits
    }

    /// Whether `claim`, the claim of `stream`, holds its bytes. A claim that holds
    /// none charges `bytes` when [`Budget::admit`] does in `order`, and else waits
    /// for room. A claim that waits takes the room it got.
    ///
    /// # Panics
    ///
    /// When a claim that holds none asks for more than the budget, which no claim
    /// could ever fit.
    fn charge(
        &mut self,
        stream: Key,
        bytes: usize,
        claim: &mut Claim,
        order: Order,
    ) -> bool {
        match claim.state {
            State::Held(_) => true,
            State::Queued { bytes, .. } if !self.waits(claim) => {
                self.given[claim.class.rank()] -= 1;
                self.held[claim.class.rank()] += 1;
                claim.state = State::Held(bytes);
                true
            }
            State::Queued { .. } => false,
            State::Idle => {
                assert!(
                    bytes <= self.max,
                    "a claim of {bytes} bytes is over the budget, {} bytes",
                    self.max
                );
                if self.admit(bytes, claim, order) {
                    return true;
                }
                let rank = claim.class.rank();
                let ticket = self.tickets[rank];
                self.tickets[rank] += 1;
                self.waiting[rank].push_back(Wait {
                    stream,
                    ticket,
                    bytes,
                });
                claim.state = State::Queued { ticket, bytes };
                false
            }
        }
    }

    /// Ends `claim`: gives back its bytes, or ends its wait. Then gives room to the
    /// waiting claims of `classes`, in that order, until the next does not fit, and
    /// calls `woken` with the stream of each.
    fn release(
        &mut self,
        claim: &mut Claim,
        classes: impl IntoIterator<Item = Class>,
        mut woken: impl FnMut(Key),
    ) {
        let rank = claim.class.rank();
        match mem::replace(&mut claim.state, State::Idle) {
            State::Idle => {}
            State::Queued { ticket, bytes } if ticket < self.granted[rank] => {
                self.used -= bytes;
                self.given[rank] -= 1;
            }
            State::Queued { ticket, .. } => {
                let waiting = &mut self.waiting[rank];
                let at = waiting.binary_search_by_key(&ticket, |wait| wait.ticket);
                waiting.remove(at.expect("invariant: a waiting claim is queued"));
            }
            State::Held(bytes) => {
                self.used -= bytes;
                self.held[rank] -= 1;
            }
        }
        for class in classes {
            let waiting = &mut self.waiting[class.rank()];
            let granted = &mut self.granted[class.rank()];
            while let Some(next) = waiting.front() {
                if next.bytes > self.max - self.used {
                    return;
                }
                self.used += next.bytes;
                self.given[class.rank()] += 1;
                *granted = next.ticket + 1;
                woken(next.stream);
                waiting.pop_front();
            }
        }
    }
}

impl Order {
    /// By rank.
    const RANK: Self = Self([
        Class::Command,
        Class::Latest,
        Class::Complete,
        Class::CatchUp,
    ]);
    /// `Complete` ahead of `Latest`.
    const COMPLETE_FIRST: Self = Self([
        Class::Command,
        Class::Complete,
        Class::Latest,
        Class::CatchUp,
    ]);

    /// `class` and the classes ahead of it.
    fn through(self, class: Class) -> impl Iterator<Item = Class> {
        let at = self.0.iter().position(|&other| other == class);
        let at = at.expect("invariant: an order holds each class");
        self.into_iter().take(at + 1)
    }
}

impl IntoIterator for Order {
    type Item = Class;
    type IntoIter = std::array::IntoIter<Class, 4>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl Turns {
    /// The first stream in turn in `order`, if any waits.
    fn first(&self, order: Order) -> Option<Key> {
        order
            .into_iter()
            .find_map(|class| self.queues[class.rank()].front().copied())
    }

    /// Whether `half` may write now in `order`: it is first in turn, or it does not
    /// wait and no stream of its class or a class ahead of it waits.
    fn allows(&self, half: &Half, order: Order) -> bool {
        if half.waiting {
            self.first(order) == Some(half.key)
        } else {
            let mut ahead = order.through(half.claim.class);
            ahead.all(|class| self.queues[class.rank()].is_empty())
        }
    }

    /// Queues `half` last in its class, unless it waits.
    fn join(&mut self, half: &mut Half) {
        if !half.waiting {
            self.queues[half.claim.class.rank()].push_back(half.key);
            half.waiting = true;
        }
    }

    /// Takes `half` out of the queue, if it waits.
    fn leave(&mut self, half: &mut Half) {
        if mem::take(&mut half.waiting) {
            let queue = &mut self.queues[half.claim.class.rank()];
            let at = queue.iter().position(|&key| key == half.key);
            queue.remove(at.expect("invariant: a waiting stream is queued"));
        }
    }
}

impl Share {
    /// The order of the turn and of room in the send budget.
    fn order(&self) -> Order {
        if self.owed > 0 {
            Order::COMPLETE_FIRST
        } else {
            Order::RANK
        }
    }

    /// The classes, in order, that get the room a message of `class` frees. Room that
    /// a class owed bytes frees waits for its next message while the other class
    /// holds room in `sending`, which frees room for all when it ends.
    fn room(
        &self,
        class: Class,
        sending: &Budget,
    ) -> impl Iterator<Item = Class> + use<> {
        let kept = match class {
            Class::Latest => self.owed < 0 && sending.holds(Class::Complete),
            Class::Complete => self.owed > 0 && sending.holds(Class::Latest),
            Class::Command | Class::CatchUp => false,
        };
        let last = if kept { class } else { Class::CatchUp };
        self.order().through(last)
    }

    /// Counts `bytes` of a message of `class` that noq-proto took. `paired` when
    /// both `Latest` and `Complete` compete for the send budget. Else the bytes move
    /// `owed` toward 0 and never past it, so a class alone makes no debt or credit.
    fn took(&mut self, class: Class, bytes: usize, paired: bool) {
        let bytes = isize::try_from(bytes).expect("invariant: a write fits in memory");
        let change = match class {
            Class::Latest => LATEST_COST * bytes,
            Class::Complete => -bytes,
            Class::Command | Class::CatchUp => return,
        };

        let owed = self.owed + change;
        self.owed = if paired {
            owed
        } else {
            owed.clamp(self.owed.min(0), self.owed.max(0))
        };
    }
}

impl Sending {
    /// The first stream in turn, if any waits.
    fn first(&self) -> Option<Key> {
        self.turns.first(self.share.order())
    }

    /// Writes the message that `half` holds to `inner`, and ends it once noq-proto
    /// took it all. `Pending` while noq-proto takes no more now, the message waits
    /// for room in the budget, or it waits its turn.
    fn write(
        &mut self,
        inner: &mut noq_proto::Connection,
        half: &mut Half,
        given: Option<&[Part]>,
    ) -> Poll<()> {
        // A write can change the turn order, so the first stream is read before it.
        let first = self.first();
        let pushed = self.push(inner, half, given);
        if pushed.is_ready() {
            self.end_after(half, first);
        } else if self.first() != Some(half.key) {
            // noq-proto wakes `half` when it takes more, but no other stream.
            self.wake(first);
        }
        pushed
    }

    /// Writes the rest of the message that the half of stream `id` holds to `inner`.
    /// Gives the half once it holds no message, and `None` while it holds one.
    ///
    /// # Errors
    ///
    /// As [`Half::check`].
    fn resume<'a>(
        &mut self,
        inner: &mut noq_proto::Connection,
        halves: &'a mut Map<StreamId, Half>,
        id: StreamId,
    ) -> Result<Option<&'a mut Half>, Error> {
        let half = halves.get_mut(&id).expect(HALF);
        half.notified = false;
        half.check()?;
        if half.holds() && self.write(inner, half, None).is_pending() {
            return Ok(None);
        }
        Ok(Some(half))
    }

    fn push(
        &mut self,
        inner: &mut noq_proto::Connection,
        half: &mut Half,
        given: Option<&[Part]>,
    ) -> Poll<()> {
        let order = self.share.order();
        let charged = self
            .budget
            .charge(half.key, half.body, &mut half.claim, order);
        if charged && self.turns.allows(half, order) {
            let left = half.left();
            let send = &mut inner.send_stream(half.key.id);
            let written = half.write(send, &mut self.buffer, given);
            let paired = self.paired();
            self.share
                .took(half.claim.class, left - half.left(), paired);
            match written {
                Ok(()) => return Poll::Ready(()),
                Err(WriteError::Blocked) => {}
                Err(WriteError::Stopped(_)) => panic!("{STOPPED}"),
                Err(WriteError::ClosedStream) => panic!("{OPEN}"),
            }
        } else {
            half.hold(given);
        }
        // A message that waits for room does not hold back a write.
        if charged {
            self.turns.join(half);
        }
        Poll::Pending
    }

    /// Ends the message that `half` holds or waits for: drops what it holds, and
    /// gives back its budget and its turn. The streams that get the room are woken,
    /// and so is the stream that is now first in turn.
    fn end(&mut self, half: &mut Half) {
        let first = self.first();
        self.end_after(half, first);
    }

    /// [`Sending::end`], with `first` the first stream in turn before the caller
    /// wrote `half`.
    fn end_after(&mut self, half: &mut Half, first: Option<Key>) {
        (half.unsent, half.block, half.body) = (0..0, Bytes::new(), 0);
        half.chunk = Bytes::new();
        half.parts.clear();
        let room = self.share.room(half.claim.class, &self.budget);
        let woken = &mut self.woken;
        let wake = |stream: Key| woken.push_back(stream.id);
        self.budget.release(&mut half.claim, room, wake);
        self.turns.leave(half);
        self.wake(first);
    }

    /// Wakes the first stream in turn, unless it is `first`.
    fn wake(&mut self, first: Option<Key>) {
        let next = self.first();
        if next != first
            && let Some(stream) = next
        {
            self.woken.push_back(stream.id);
        }
    }

    /// Whether both `Latest` and `Complete` compete for the send budget.
    fn paired(&self) -> bool {
        self.budget.competes(Class::Latest) && self.budget.competes(Class::Complete)
    }
}

impl Streams {
    /// The messages that waited for room in the send budget.
    pub(super) fn budget_waits(&self) -> u64 {
        self.sending.budget.queued()
    }

    /// The streams of a connection that refuses a message over `bytes_max`, with a
    /// window of `window_bytes`.
    pub(super) fn new(window_bytes: usize, bytes_max: usize) -> Self {
        Self {
            own: Hello {
                window_bytes,
                message_bytes_max: bytes_max,
            },
            greeted: false,
            peer: hello::Peer::new(),
            arriving: Vec::new(),
            incoming: Default::default(),
            halves: Map::default(),
            sending: Sending {
                budget: Budget::new(0),
                turns: Turns::default(),
                share: Share::default(),
                woken: VecDeque::new(),
                buffer: Vec::new(),
            },
            receiving: Budget::new(window_bytes.saturating_add(bytes_max)),
            closed: Closed::default(),
        }
    }

    /// Gives `error` to each handle of the connection, for each later stream call.
    ///
    /// # Panics
    ///
    /// When the connection closed before.
    pub(super) fn end(&self, error: Error) {
        let set = self.closed.set(error);
        assert!(set.is_ok(), "invariant: a connection closes once");
    }

    /// Starts the wait for the peer's hello at `now`, when the handshake ends.
    pub(super) fn start(&mut self, now: Instant) {
        self.peer.wait(now);
    }

    /// When the peer's hello is due on a connection whose idle timeout `idle` gives:
    /// twice it after the handshake, while the hello has not arrived and the
    /// connection has not ended. A cut that the connection lives through ends within
    /// the idle timeout. Calls `idle` only while it waits.
    pub(super) fn deadline(&self, idle: impl FnOnce() -> Duration) -> Option<Instant> {
        let since = self.peer.since()?;
        self.closed.get().is_none().then(|| since + 2 * idle())
    }

    /// Checks the wait for the peer's hello at `now`, on a connection whose idle
    /// timeout `idle` gives.
    ///
    /// # Errors
    ///
    /// [`Fault`] when the hello is due by `now`, has not arrived, and the connection
    /// has not ended.
    pub(super) fn timeout(
        &self,
        now: Instant,
        idle: impl FnOnce() -> Duration,
    ) -> Result<(), Fault> {
        match self.deadline(idle) {
            Some(due) if due <= now => Err(Fault("a peer with no hello".to_owned())),
            Some(_) | None => Ok(()),
        }
    }

    /// Sends this side's hello on `inner`, on this side's first one-way stream, ahead
    /// of every other stream, and finishes it. Does nothing after it sent it, or while
    /// the handshake holds back the peer's transport parameters.
    ///
    /// # Errors
    ///
    /// [`Fault`] when the peer has no room for the whole hello now.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a stream this side just opened is open"
    )]
    pub(super) fn greet(
        &mut self,
        inner: &mut noq_proto::Connection,
    ) -> Result<(), Fault> {
        if self.greeted {
            return Ok(());
        }
        let room = || Fault("a peer with no room for the hello".to_owned());
        let Some(id) = inner.streams().open(Dir::Uni) else {
            return if inner.is_handshaking() {
                Ok(())
            } else {
                Err(room())
            };
        };
        self.greeted = true;
        let mut send = inner.send_stream(id);
        send.set_priority(i32::MAX).expect(OPENED);
        let mut hello = [Bytes::from(self.own.encode())];
        let mut unsent = &mut hello[..];
        match send.write_chunks(&mut unsent) {
            Ok(_) if unsent.is_empty() => {}
            Ok(_) | Err(WriteError::Blocked) => return Err(room()),
            Err(error @ (WriteError::Stopped(_) | WriteError::ClosedStream)) => {
                panic!("{OPENED}: {error}")
            }
        }
        send.finish().expect(OPENED);
        Ok(())
    }

    /// Queues in `events` what `event` of `inner`, the connection of `key`, means to
    /// the caller, if anything. The same event in a row merges, as noq-proto repeats
    /// one for each frame. Until the peer's hello arrives, every event but a stop of
    /// this side's stream goes to the hello. A stream that the peer stops resets here
    /// with the stop's code, except a stream it opened before the hello, which resets
    /// at the hello.
    ///
    /// # Errors
    ///
    /// [`Fault`] when the peer broke the hello or the stream protocol.
    pub(super) fn event(
        &mut self,
        inner: &mut noq_proto::Connection,
        key: connection::Key,
        event: &StreamEvent,
        events: &mut VecDeque<Event>,
    ) -> Result<(), Fault> {
        if self.peer.hello().is_none() {
            // Before the hello, this side's only stream is its hello stream.
            // noq-proto drops its stop once the peer has the whole hello.
            if let StreamEvent::Stopped { id, error_code } = *event
                && id.initiator() == inner.side()
            {
                reset_stopped(inner, id, error_code)?;
                return Ok(());
            }
            if let Some(peer) = self.peer.read(inner, event)? {
                self.arrive(inner, key, peer, events)?;
            }
            return Ok(());
        }
        let event = self.translate(inner, key, event)?;
        if let Some(event) = event
            && events.back() != Some(&event)
        {
            events.push_back(event);
        }
        Ok(())
    }

    /// Takes the peer's hello: its window bounds the send budget, and the streams it
    /// opened before the hello get their class.
    fn arrive(
        &mut self,
        inner: &mut noq_proto::Connection,
        key: connection::Key,
        peer: Hello,
        events: &mut VecDeque<Event>,
    ) -> Result<(), Fault> {
        self.sending.budget = Budget::new(peer.window_bytes);
        events.push_back(Event::Available { key });
        let bi = self.take(inner, key, Dir::Bi)?;
        if self.take(inner, key, Dir::Uni)? || bi {
            events.push_back(Event::Incoming { key });
        }
        Ok(())
    }

    /// What `event` of `inner`, the connection of `key`, means to the caller after
    /// the peer's hello.
    fn translate(
        &mut self,
        inner: &mut noq_proto::Connection,
        key: connection::Key,
        event: &StreamEvent,
    ) -> Result<Option<Event>, Fault> {
        let stream = |id| Key {
            connection: key,
            id,
        };
        match *event {
            StreamEvent::Opened { dir } => Ok(self
                .take(inner, key, dir)?
                .then_some(Event::Incoming { key })),
            StreamEvent::Readable { id } => {
                let Some(at) = self.arriving.iter().position(|other| other.id == id)
                else {
                    return Ok(Some(Event::Readable { stream: stream(id) }));
                };
                let arriving = self.arriving.swap_remove(at);
                let queued = self.queue(inner, key, arriving)?;
                Ok(queued.then_some(Event::Incoming { key }))
            }
            StreamEvent::Writable { .. } => {
                if let Some(first) = self.sending.first() {
                    self.sending.woken.push_back(first.id);
                }
                Ok(None)
            }
            StreamEvent::Stopped { id, error_code } => {
                let code = reset_stopped(inner, id, error_code)?;
                if let Some(arriving) = self.arriving.iter_mut().find(|a| a.id == id) {
                    arriving.stopped = Some(code);
                    return Ok(None);
                }
                let Some(half) = self.halves.get_mut(&id) else {
                    return Ok(None);
                };
                // A cancel that reset the stream first keeps its error.
                if half.ended.is_some() {
                    return Ok(None);
                }
                half.ended = Some(Error::Stopped { code });
                self.sending.end(half);
                if half.rest == Rest::Finish {
                    self.halves.remove(&id);
                    return Ok(None);
                }
                if mem::replace(&mut half.notified, true) {
                    return Ok(None);
                }
                Ok(Some(Event::Writable { stream: stream(id) }))
            }
            StreamEvent::Available { .. } => Ok(Some(Event::Available { key })),
            StreamEvent::Finished { .. } => Ok(None),
        }
    }

    /// Opens a stream of `class` of `connection`'s `inner` in `dir`, and gives its
    /// sender, and its receiver when it goes both ways. `None` until the peer's
    /// hello, and when the peer allows no more now.
    pub(super) fn open(
        &mut self,
        inner: &mut noq_proto::Connection,
        connection: connection::Key,
        dir: Dir,
        class: Class,
    ) -> Option<(Sender, Option<Receiver>)> {
        let peer = self.peer.hello()?;
        let id = inner.streams().open(dir)?;
        let prioritized = inner.send_stream(id).set_priority(priority(class));
        prioritized.expect("invariant: a stream that opens has a send half");
        let key = Key { connection, id };
        self.halves.insert(id, Half::new(key, class));
        let sender = Sender::new(key, peer.message_bytes_max, &self.closed);
        let reader = || Reader::new(self.own.message_bytes_max);
        let receiver = || Receiver::new(key, class, reader(), &self.closed);
        Some((sender, (dir == Dir::Bi).then(receiver)))
    }

    /// The next stream the peer opened, highest class first.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a stream is queued only under a class byte"
    )]
    pub(super) fn accept(&mut self, connection: connection::Key) -> Option<Incoming> {
        let (byte, (id, first)) = (0u8..)
            .zip(&mut self.incoming)
            .find_map(|(byte, queue)| Some((byte, queue.pop_front()?)))?;
        let key = Key { connection, id };
        let class = class(byte).expect("invariant: a queued stream has a class byte");
        let peer = self.peer.hello();
        let peer = peer.expect("invariant: streams queue after the peer's hello");
        let reply = || Sender::new(key, peer.message_bytes_max, &self.closed);
        let reader = Reader::started(self.own.message_bytes_max, first);
        Some(Incoming {
            class,
            receiver: Receiver::new(key, class, reader, &self.closed),
            sender: (id.dir() == Dir::Bi).then(reply),
        })
    }

    /// Writes the rest of the message that `sender`'s stream of `inner` holds, then
    /// puts `parts` of `message`, when `Some`, a message of `bytes`, on the stream
    /// after it, and takes the block. Leaves it while the stream holds part of an
    /// earlier message. `Ready` when the stream holds no message: it took all of
    /// `message`, or with `None`, all of the one before. `Pending` while it holds
    /// one, and [`Event::Writable`] follows when a later write can take more.
    ///
    /// # Errors
    ///
    /// As [`Half::check`].
    pub(super) fn write(
        &mut self,
        inner: &mut noq_proto::Connection,
        sender: &Sender,
        message: &mut Option<Block>,
        parts: &[Part],
        bytes: usize,
    ) -> Result<Poll<()>, Error> {
        let resumed = self
            .sending
            .resume(inner, &mut self.halves, sender.key.id)?;
        let Some(half) = resumed else {
            return Ok(Poll::Pending);
        };
        let Some(message) = message.take() else {
            return Ok(Poll::Ready(()));
        };
        half.load(message, bytes, Rest::Caller);
        Ok(self.sending.write(inner, half, Some(parts)))
    }

    /// Writes the rest of the message that `sender`'s stream of `inner` holds, then
    /// takes the block out of `message` and puts its `parts` on the stream, a message
    /// of `bytes`, when the stream can take it now: it holds no part of an earlier
    /// message, the send budget has room, and no stream of its class or a class
    /// ahead of it in the turn waits for room or its turn. Else leaves it, and the
    /// stream does not wait for room for it. [`Streams::pump`] writes what the
    /// stream does not take now.
    ///
    /// # Errors
    ///
    /// As [`Half::check`].
    pub(super) fn try_write(
        &mut self,
        inner: &mut noq_proto::Connection,
        sender: &Sender,
        message: &mut Option<Block>,
        parts: &[Part],
        bytes: usize,
    ) -> Result<(), Error> {
        let resumed = self
            .sending
            .resume(inner, &mut self.halves, sender.key.id)?;
        let Some(half) = resumed else {
            return Ok(());
        };
        let Sending {
            budget,
            turns,
            share,
            ..
        } = &mut self.sending;
        let order = share.order();
        let allowed = turns.allows(half, order);
        let admitted =
            |_: &mut Block| allowed && budget.admit(bytes, &mut half.claim, order);
        if let Some(taken) = message.take_if(admitted) {
            half.load(taken, bytes, Rest::Pump);
            _ = self.sending.write(inner, half, Some(parts));
        }
        Ok(())
    }

    /// Gives each stream that got room or the turn [`Event::Writable`] in `events`,
    /// so that its caller writes the rest. A stream whose message came from a write
    /// that does not wait, or that the caller finished, writes the rest to `inner`
    /// itself, and gets the event once noq-proto took it all. A finished stream then
    /// finishes, with no event.
    pub(super) fn pump(
        &mut self,
        inner: &mut noq_proto::Connection,
        events: &mut VecDeque<Event>,
    ) {
        while let Some(id) = self.sending.woken.pop_front() {
            let Some(half) = self.halves.get_mut(&id) else {
                continue;
            };
            // An earlier wake in this drive may have written all of it.
            if !half.holds() {
                continue;
            }
            let caller = half.rest == Rest::Caller;
            if !caller && self.sending.write(inner, half, None).is_pending() {
                continue;
            }
            if half.rest == Rest::Finish {
                self.halves.remove(&id);
                finish(inner, id);
            } else if !mem::replace(&mut half.notified, true) {
                events.push_back(Event::Writable { stream: half.key });
            }
        }
    }

    /// Ends stream `id` of `inner` after what it holds. A stream that holds part of
    /// a message ends once noq-proto takes the rest.
    ///
    /// # Errors
    ///
    /// As [`Half::check`]. Each later finish gives it too.
    ///
    /// # Panics
    ///
    /// When this side does not send on `id`, or finished it before.
    pub(super) fn finish(
        &mut self,
        inner: &mut noq_proto::Connection,
        id: StreamId,
    ) -> Result<(), Error> {
        let half = self.halves.get_mut(&id).expect(HALF);
        half.check()?;
        if half.holds() {
            half.rest = Rest::Finish;
            if self.sending.write(inner, half, None).is_pending() {
                return Ok(());
            }
        }
        self.halves.remove(&id);
        finish(inner, id);
        Ok(())
    }

    /// Resets `sender`'s stream of `inner` with `code`, and gives back its send budget
    /// and its turn.
    pub(super) fn reset(
        &mut self,
        inner: &mut noq_proto::Connection,
        sender: &Sender,
        code: Code,
    ) {
        let id = sender.key.id;
        reset(inner, id, code);
        if let Some(mut half) = self.halves.remove(&id) {
            self.sending.end(&mut half);
        }
    }

    /// Takes the message in hand out of `sender`'s stream of `inner`, and gives back
    /// its send budget and its turn. When a byte of it went, its header included,
    /// it also resets the stream with `Code(0)`, and each later write and finish
    /// gives [`Error::Reset`].
    pub(super) fn cancel(
        &mut self,
        inner: &mut noq_proto::Connection,
        sender: &Sender,
    ) {
        let half = self.halves.get_mut(&sender.key.id).expect(HALF);
        if half.sent() {
            reset(inner, half.key.id, Code(0));
            half.ended = Some(Error::Reset { code: Code(0) });
        } else if half.unsent.start == 0 {
            // Not even the class byte went, so the next message opens the stream.
            half.started = false;
        }
        self.sending.end(half);
    }

    /// Stops `receiver`'s stream of `inner` with `code`, drops the message in its
    /// reader, and gives back its receive budget. The receivers that get the freed
    /// room get [`Event::Readable`] in `events`.
    pub(super) fn stop(
        &mut self,
        inner: &mut noq_proto::Connection,
        mut receiver: Receiver,
        code: Code,
        events: &mut VecDeque<Event>,
    ) {
        let stopped = inner
            .recv_stream(receiver.key.id)
            .stop(VarInt::from_u32(code.0));
        match stopped {
            Ok(()) | Err(ClosedStream { .. }) => {}
        }
        let woken = |stream| events.push_back(Event::Readable { stream });
        self.receiving
            .release(&mut receiver.claim, Order::RANK, woken);
    }

    /// Ends the wait of `receiver`'s next message for room in the receive budget, and
    /// gives back room that it got and has not taken. The receivers that get the room
    /// get [`Event::Readable`] in `events`.
    pub(super) fn end_wait(
        &mut self,
        receiver: &mut Receiver,
        events: &mut VecDeque<Event>,
    ) {
        if let State::Queued { .. } = receiver.claim.state {
            let woken = |stream| events.push_back(Event::Readable { stream });
            self.receiving
                .release(&mut receiver.claim, Order::RANK, woken);
        }
    }

    /// Reads the next whole message of `receiver`'s stream from `inner`, and gives
    /// what `land(reader, len)` makes of it. `Ready(None)` at the end.
    /// `Pending` when no whole message is here yet, the next has no room in the
    /// receive budget or waits behind a stream of its class or a higher class, or
    /// `land` gives `Pending`. A message that `land` leaves in `reader` keeps its
    /// room for the next read. The receivers that get the freed room get
    /// [`Event::Readable`] in `events`.
    ///
    /// # Errors
    ///
    /// [`Error::Reset`] when the peer reset the stream, and [`Error::Broken`] when the
    /// peer broke the framing or reset with a code over 32 bits.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a receiver never reads its stream after the end"
    )]
    pub(super) fn read<T>(
        &mut self,
        inner: &mut noq_proto::Connection,
        receiver: &mut Receiver,
        mut land: impl FnMut(&mut Reader, usize) -> Poll<T>,
        events: &mut VecDeque<Event>,
    ) -> Result<Poll<Option<T>>, Error> {
        let Receiver {
            key,
            reader,
            claim,
            end,
            ..
        } = receiver;
        let receiving = &mut self.receiving;
        let mut recv = inner.recv_stream(key.id);
        let (mut empty, mut kept) = (false, false);
        let (mut result, waits) = if receiving.waits(claim) {
            (Ok(Poll::Pending), true)
        } else {
            let mut chunks = recv.read(true).expect(RECEIVING);
            let mut source = |max| match chunks.next(max) {
                Ok(chunk) => Ok(Poll::Ready(chunk.map(|chunk| chunk.bytes))),
                Err(ReadError::Blocked) => Ok(Poll::Pending),
                Err(ReadError::Reset(error)) => Err(reset_error(error)),
            };
            loop {
                match reader.read(&mut source) {
                    Ok(Step::Room(len)) => {
                        if !receiving.charge(*key, len, claim, Order::RANK) {
                            break (Ok(Poll::Pending), true);
                        }
                        reader.admit();
                    }
                    Ok(Step::Block(len)) => match land(reader, len) {
                        Poll::Ready(landed) => {
                            (empty, kept) = (len == 0, reader.whole());
                            break (Ok(Poll::Ready(Some(landed))), kept);
                        }
                        Poll::Pending => break (Ok(Poll::Pending), true),
                    },
                    Ok(Step::Pending) => break (Ok(Poll::Pending), false),
                    Ok(Step::Ended) => break (Ok(Poll::Ready(None)), false),
                    Err(error) => break (Err(error), false),
                }
            }
        };
        // The reader takes no bytes while it waits for room or a landing, or for an
        // empty first message, whose one byte accept took, so only this finds a reset.
        if (waits || empty)
            && let Some(error) = recv.received_reset().expect(RECEIVING)
        {
            (result, kept) = (Err(reset_error(error)), false);
        }
        if !kept && !matches!(result, Ok(Poll::Pending)) {
            receiving.release(claim, Order::RANK, |stream| {
                events.push_back(Event::Readable { stream });
            });
        }
        *end = match result {
            Ok(Poll::Ready(None)) => Some(End::Finished),
            Err(Error::Reset { code }) => Some(End::Reset(code)),
            _ => None,
        };
        result
    }

    /// Accepts each new stream of `inner` in `dir` and reads it up to the first byte
    /// of its first message. Returns whether one is now queued for
    /// [`Streams::accept`].
    #[expect(
        clippy::unwrap_in_result,
        reason = "a two-way stream noq-proto just gave has a send half"
    )]
    fn take(
        &mut self,
        inner: &mut noq_proto::Connection,
        connection: connection::Key,
        dir: Dir,
    ) -> Result<bool, Fault> {
        let mut queued = false;
        while let Some(id) = inner.streams().accept(dir) {
            let mut code = None;
            if dir == Dir::Bi {
                // A stop before the peer's hello gave this side no event.
                let stopped = inner.send_stream(id).stopped();
                let stopped = stopped.expect("invariant: a new stream has a send half");
                code = stopped
                    .map(|code| reset_stopped(inner, id, code))
                    .transpose()?;
            }
            let arriving = Arriving {
                id,
                class: None,
                stopped: code,
            };
            queued |= self.queue(inner, connection, arriving)?;
        }
        Ok(queued)
    }

    /// Reads `arriving`, a stream of `connection`'s `inner`, up to the first byte of
    /// its first message. Then it queues the stream for [`Streams::accept`], and keeps
    /// the reply half of a two-way one with the priority of its class. Returns
    /// whether it queued the stream. A stream that ends or resets before that byte
    /// drops, and the reply half of a two-way one resets with code 0.
    fn queue(
        &mut self,
        inner: &mut noq_proto::Connection,
        connection: connection::Key,
        mut arriving: Arriving,
    ) -> Result<bool, Fault> {
        let id = arriving.id;
        loop {
            let Poll::Ready(next) = next_byte(inner, id)? else {
                self.arriving.push(arriving);
                return Ok(false);
            };
            let Some(next) = next else {
                if id.dir() == Dir::Bi {
                    reset(inner, id, Code(0));
                }
                return Ok(false);
            };
            let Some(class) = arriving.class else {
                let class = class(next)
                    .ok_or_else(|| Fault(format!("a stream of class {next}")));
                arriving.class = Some(class?);
                continue;
            };
            if id.dir() == Dir::Bi {
                // A stop resets the reply half at once, and noq-proto frees it once
                // the peer has the reset. Its first write then gives `Stopped`.
                match inner.send_stream(id).set_priority(priority(class)) {
                    Ok(()) | Err(ClosedStream { .. }) => {}
                }
                let key = Key { connection, id };
                self.halves
                    .insert(id, Half::reply(key, class, arriving.stopped));
            }
            self.incoming[usize::from(byte(class))].push_back((id, next));
            return Ok(true);
        }
    }
}

/// The next byte of new stream `id` of `inner`: `Pending` when none is here yet, and
/// `None` when the stream ended or reset.
///
/// # Errors
///
/// A reset with a code over 32 bits.
#[expect(
    clippy::unwrap_in_result,
    reason = "a stream noq-proto just gave is open and gives no empty chunk"
)]
fn next_byte(
    inner: &mut noq_proto::Connection,
    id: StreamId,
) -> Result<Poll<Option<u8>>, Fault> {
    let next = inner
        .recv_stream(id)
        .read(true)
        .expect("invariant: a new stream is open")
        .next(1);
    match next {
        Ok(Some(chunk)) => {
            let byte = chunk.bytes.first();
            Ok(Poll::Ready(Some(
                *byte.expect("invariant: chunks are not empty"),
            )))
        }
        Err(ReadError::Blocked) => Ok(Poll::Pending),
        Err(ReadError::Reset(error)) if code(error).is_none() => {
            Err(Fault(format!("a reset code over 32 bits: {error}")))
        }
        Ok(None) | Err(ReadError::Reset(_)) => Ok(Poll::Ready(None)),
    }
}

/// The endpoint resets each stream at the peer's stop, before the caller's next call.
const STOPPED: &str = "invariant: a stream resets at the peer's stop";
const OPEN: &str = "invariant: only a stop resets a sender's stream";
const RECEIVING: &str = "invariant: a receiver's stream is open until it ends";
const OPENED: &str = "invariant: a stream this side just opened is open";
const HALF: &str = "invariant: a sender that is not finished has its half";

/// The byte that starts a stream of `class` on the wire. The byte order is the order
/// in which [`Streams::accept`] gives streams.
fn byte(class: Class) -> u8 {
    match class {
        Class::Command => 0,
        Class::Latest => 1,
        Class::Complete => 2,
        Class::CatchUp => 3,
    }
}

/// The noq-proto priority of a stream of `class`. noq-proto sends a higher priority
/// first, and 0, its default, is the lowest class.
fn priority(class: Class) -> i32 {
    let priority = Class::CatchUp.rank() - class.rank();
    i32::try_from(priority).expect("invariant: a rank is at most 3")
}

/// The class whose streams start with `byte`, if any.
fn class(byte: u8) -> Option<Class> {
    match byte {
        0 => Some(Class::Command),
        1 => Some(Class::Latest),
        2 => Some(Class::Complete),
        3 => Some(Class::CatchUp),
        _ => None,
    }
}

/// `code` as a [`Code`], if it fits in 32 bits.
fn code(code: VarInt) -> Option<Code> {
    u32::try_from(code.into_inner()).ok().map(Code)
}

/// The error of a read on a stream that the peer reset with `error`.
fn reset_error(error: VarInt) -> Error {
    match code(error) {
        Some(code) => Error::Reset { code },
        None => Error::Broken {
            reason: format!("a reset code over 32 bits: {error}"),
        },
    }
}

/// Resets stream `id` of `inner`, which the peer stopped with `code`, with that code,
/// and gives it.
///
/// # Errors
///
/// [`Fault`] when `code` is over 32 bits.
fn reset_stopped(
    inner: &mut noq_proto::Connection,
    id: StreamId,
    code: VarInt,
) -> Result<Code, Fault> {
    let Some(code) = self::code(code) else {
        return Err(Fault(format!("a stop code over 32 bits: {code}")));
    };
    reset(inner, id, code);
    Ok(code)
}

/// Ends stream `id` of `inner`, which this side sends on, after what it holds.
fn finish(inner: &mut noq_proto::Connection, id: StreamId) {
    match inner.send_stream(id).finish() {
        Ok(()) => {}
        Err(FinishError::Stopped(_)) => panic!("{STOPPED}"),
        Err(FinishError::ClosedStream) => panic!("{OPEN}"),
    }
}

/// Resets stream `id` of `inner` with `code`. Does nothing when this side reset it
/// before, or the peer has all of it.
fn reset(inner: &mut noq_proto::Connection, id: StreamId, code: Code) {
    match inner.send_stream(id).reset(VarInt::from_u32(code.0)) {
        Ok(()) | Err(ClosedStream { .. }) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use block::{Heap, Pool};
    use types::time::{Monotonic, Span};

    use super::*;
    use crate::Config;
    use crate::quic::Endpoint;
    use crate::quic::pair::{self, Pair, Side};
    use crate::testing::{self, Shard};
    use crate::tls;

    /// The link delay each way.
    const DELAY: Duration = Duration::from_millis(10);
    /// Long enough for a datagram to go and its reply to come back.
    const RUN: Duration = Duration::from_millis(100);
    /// A small part of a round trip, so that credit comes back in small steps.
    const STEP: Duration = Duration::from_millis(1);
    /// The largest message in [`Shard::config`].
    const MESSAGE_MAX: usize = 1 << 16;

    /// A pair whose client dialed the server and connected.
    fn connected(shard: &Shard) -> Pair {
        let mut pair = Pair::new(shard, Span::SECOND, DELAY);
        pair.dial(pair::SERVER_KEY.public());
        pair.run(RUN);
        pair
    }

    fn key(side: &Side) -> connection::Key {
        side.key.expect("a connection")
    }

    fn events(side: &Side) -> Vec<&Event> {
        side.events.iter().map(|(_, event)| event).collect()
    }

    /// Opens a one-way stream of `class` on the client.
    fn open_sender(pair: &mut Pair, class: Class) -> Sender {
        let (now, key) = (pair.now(), key(&pair.client));
        let sender = pair.client.endpoint.open_sender(now, key, class);
        sender.expect("a stream")
    }

    /// Writes each of `messages` to `sender` on `side`, which takes each whole now.
    fn write(side: &mut Side, now: Monotonic, sender: &mut Sender, messages: &[Block]) {
        for message in messages {
            let written = pair::write(
                &mut side.endpoint,
                now,
                sender,
                &mut Some(message.clone()),
            );
            assert_eq!(written, Ok(Poll::Ready(())));
        }
    }

    /// The halves that `side`'s connection of `sender` keeps. Tests read them for
    /// state no call shows: a header split across writes, the class a reply counts
    /// in, whether a sender holds part of a message, and whether a half is gone.
    fn halves<'a>(side: &'a mut Side, sender: &Sender) -> &'a Map<StreamId, Half> {
        let key = sender.key().connection;
        let connection = crate::quic::find(&mut side.endpoint.connections, key);
        &connection.expect("a connection").streams.halves
    }

    /// The half that `side`'s connection keeps of `sender`'s stream.
    fn half<'a>(side: &'a mut Side, sender: &Sender) -> &'a Half {
        halves(side, sender).get(&sender.key().id).expect("a half")
    }

    /// The next stream the peer opened on `side`.
    fn accept(side: &mut Side) -> Incoming {
        let key = key(side);
        side.endpoint.accept(key).expect("a stream")
    }

    /// Writes messages of [`MESSAGE_MAX`] bytes to the client's `sender` until one
    /// waits for the window. Message `i` holds `i` in each byte. Gives the count.
    fn fill(pair: &mut Pair, shard: &Shard, sender: &mut Sender) -> u8 {
        let now = pair.now();
        for count in 1.. {
            let message = shard.block(&vec![count - 1; MESSAGE_MAX]);
            let written =
                pair::write(&mut pair.client.endpoint, now, sender, &mut Some(message));
            if written.expect("written").is_pending() {
                return count;
            }
        }
        unreachable!("the window is full before 256 messages")
    }

    /// The messages that `receiver` on `side` gives now, and whether its stream ended.
    fn drain(
        side: &mut Side,
        now: Monotonic,
        receiver: &mut Receiver,
    ) -> (Vec<Vec<u8>>, bool) {
        let mut messages = Vec::new();
        loop {
            match next(side, now, receiver).expect("read") {
                Poll::Ready(Some(message)) => messages.push(message),
                Poll::Ready(None) => return (messages, true),
                Poll::Pending => return (messages, false),
            }
        }
    }

    /// One read of `receiver` on `side`, with the message as bytes.
    fn next(
        side: &mut Side,
        now: Monotonic,
        receiver: &mut Receiver,
    ) -> Result<Poll<Option<Vec<u8>>>, Error> {
        let read = side.endpoint.read(now, receiver, testing::alloc)?;
        Ok(read.map(|message| message.map(|message| message.to_vec())))
    }

    /// Whether a read of `receiver` on `side` asked the pool for a block and got
    /// none.
    ///
    /// # Panics
    ///
    /// When the read is not `Pending`.
    fn missed(side: &mut Side, now: Monotonic, receiver: &mut Receiver) -> bool {
        let mut missed = false;
        let read = side.endpoint.read(now, receiver, |pool, len| {
            let block = testing::alloc(pool, len);
            missed = block.is_none();
            block
        });
        assert!(matches!(read, Ok(Poll::Pending)), "{read:?}");
        missed
    }

    /// Writes `bytes` on a new stream of `connection` in `dir`, and finishes it when
    /// `finished`.
    fn raw(
        connection: &mut noq_proto::Connection,
        dir: Dir,
        bytes: &[u8],
        finished: bool,
    ) -> StreamId {
        let id = connection.streams().open(dir).expect("a stream");
        let mut send = connection.send_stream(id);
        assert_eq!(send.write(bytes), Ok(bytes.len()));
        if finished {
            send.finish().expect("finished");
        }
        id
    }

    /// Asserts that `found` closed the connection for `reason` and that `told` got
    /// the reason, after a run.
    fn assert_broken(pair: &mut Pair, server_found: bool, reason: &str) {
        pair.run(RUN);
        let (found, told) = if server_found {
            (&pair.server, &pair.client)
        } else {
            (&pair.client, &pair.server)
        };
        let error = Error::Broken {
            reason: reason.into(),
        };
        let closed = Event::Closed {
            key: key(found),
            error,
        };
        assert_eq!(events(found).last(), Some(&&closed));
        let reason = format!("closed by peer: {reason} (code 4294967296)");
        let closed = Event::Closed {
            key: key(told),
            error: Error::Broken { reason },
        };
        assert_eq!(events(told).last(), Some(&&closed));
    }

    #[test]
    fn class_bytes_map_back_to_their_class() {
        let classes = [
            Class::Command,
            Class::Latest,
            Class::Complete,
            Class::CatchUp,
        ];
        for class in classes {
            assert_eq!(super::class(byte(class)), Some(class));
        }
        assert_eq!(super::class(4), None);
    }

    #[test]
    fn carry_whole_messages_both_ways_in_order() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Complete);
            let (mut sender, mut receiver) = opened.expect("a stream");
            let messages = [vec![], vec![7], vec![9; MESSAGE_MAX]];
            let blocks = messages.each_ref().map(|message| shard.block(message));
            write(&mut pair.client, now, &mut sender, &blocks);
            pair.client
                .endpoint
                .finish(now, &mut sender)
                .expect("finished");
            pair.run(RUN);
            let incoming = Event::Incoming {
                key: self::key(&pair.server),
            };
            assert!(events(&pair.server).contains(&&incoming));
            let mut incoming = accept(&mut pair.server);
            assert_eq!(incoming.class, Class::Complete);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (messages.to_vec(), true));
            let read = next(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, Ok(Poll::Ready(None)));
            let mut reply = incoming.sender.expect("a two-way stream");
            let replies = [vec![1; 10], vec![2; 20]];
            let blocks = replies.each_ref().map(|reply| shard.block(reply));
            write(&mut pair.server, now, &mut reply, &blocks);
            pair.server
                .endpoint
                .finish(now, &mut reply)
                .expect("finished");
            pair.run(RUN);
            let now = pair.now();
            assert_eq!(
                drain(&mut pair.client, now, &mut receiver),
                (replies.to_vec(), true)
            );
        });
    }

    #[test]
    fn of_one_way_come_with_no_sender() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Latest);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"abc")]);
            pair.client
                .endpoint
                .finish(now, &mut sender)
                .expect("finished");
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            assert_eq!(incoming.class, Class::Latest);
            assert!(incoming.sender.is_none(), "{:?}", incoming.sender);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![b"abc".to_vec()], true));
        });
    }

    #[test]
    fn are_accepted_highest_class_first() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let now = pair.now();
            for class in [
                Class::CatchUp,
                Class::Complete,
                Class::Latest,
                Class::Command,
            ] {
                let mut sender = open_sender(&mut pair, class);
                write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            }
            pair.run(RUN);
            let key = key(&pair.server);
            let accepted = iter::from_fn(|| pair.server.endpoint.accept(key));
            let classes: Vec<Class> = accepted.map(|incoming| incoming.class).collect();
            let expected = [
                Class::Command,
                Class::Latest,
                Class::Complete,
                Class::CatchUp,
            ];
            assert_eq!(classes, expected);
        });
    }

    #[test]
    fn start_with_the_class_byte_then_each_message_after_its_length() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            for byte in 0..4 {
                raw(
                    pair.client.connection(),
                    Dir::Uni,
                    &[byte, 3, b'a', b'b', b'c'],
                    true,
                );
            }
            pair.run(RUN);
            let expected = [
                Class::Command,
                Class::Latest,
                Class::Complete,
                Class::CatchUp,
            ];
            for class in expected {
                let mut incoming = accept(&mut pair.server);
                assert_eq!(incoming.class, class);
                let now = pair.now();
                let read = drain(&mut pair.server, now, &mut incoming.receiver);
                assert_eq!(read, (vec![b"abc".to_vec()], true));
            }
        });
    }

    #[test]
    fn arrive_at_the_first_byte_of_their_first_message() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let class = [byte(Class::Complete)];
            let id = raw(pair.client.connection(), Dir::Uni, &class, false);
            pair.run(RUN);
            let server = key(&pair.server);
            assert!(pair.server.endpoint.accept(server).is_none());
            let mut send = pair.client.connection().send_stream(id);
            assert_eq!(send.write(&[1]), Ok(1));
            pair.run(RUN);
            let incoming_event = Event::Incoming { key: server };
            assert_eq!(events(&pair.server).last(), Some(&&incoming_event));
            let mut incoming = accept(&mut pair.server);
            let mut send = pair.client.connection().send_stream(id);
            assert_eq!(send.write(b"x"), Ok(1));
            send.finish().expect("finished");
            pair.run(RUN);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![b"x".to_vec()], true));
        });
    }

    #[test]
    fn opened_before_their_class_byte_arrive_when_it_does() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let connection = pair.client.connection();
            let early = connection.streams().open(Dir::Uni).expect("a stream");
            raw(pair.client.connection(), Dir::Uni, &[2, 1, b'x'], true);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            assert_eq!(incoming.class, Class::Complete);
            let server = key(&pair.server);
            assert!(pair.server.endpoint.accept(server).is_none());
            let mut send = pair.client.connection().send_stream(early);
            assert_eq!(send.write(&[0, 1, b'y']), Ok(3));
            send.finish().expect("finished");
            pair.run(RUN);
            let incoming_event = Event::Incoming { key: server };
            assert_eq!(events(&pair.server).last(), Some(&&incoming_event));
            let mut early = accept(&mut pair.server);
            assert_eq!(early.class, Class::Command);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut early.receiver);
            assert_eq!(read, (vec![b"y".to_vec()], true));
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![b"x".to_vec()], true));
        });
    }

    #[test]
    fn past_the_window_wait_for_writable_and_then_arrive_whole() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let count = fill(&mut pair, shard, &mut sender);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let (mut messages, ended) =
                drain(&mut pair.server, now, &mut incoming.receiver);
            assert!(!ended);
            pair.run(RUN);
            let writable = Event::Writable {
                stream: sender.key(),
            };
            assert!(events(&pair.client).contains(&&writable));
            let now = pair.now();
            assert_eq!(
                pair::write(&mut pair.client.endpoint, now, &sender, &mut None),
                Ok(Poll::Ready(()))
            );
            pair.client
                .endpoint
                .finish(now, &mut sender)
                .expect("finished");
            pair.run(RUN);
            let now = pair.now();
            let (rest, ended) = drain(&mut pair.server, now, &mut incoming.receiver);
            messages.extend(rest);
            assert!(ended);
            let expected: Vec<Vec<u8>> =
                (0..count).map(|i| vec![i; MESSAGE_MAX]).collect();
            assert_eq!(messages, expected);
        });
    }

    #[test]
    fn that_get_a_message_after_a_pending_read_are_readable() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Command);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let read_a = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read_a, (vec![b"a".to_vec()], false));
            write(&mut pair.client, now, &mut sender, &[shard.block(b"b")]);
            pair.run(RUN);
            let readable = Event::Readable {
                stream: incoming.receiver.key(),
            };
            assert_eq!(events(&pair.server).last(), Some(&&readable));
            let now = pair.now();
            let read_b = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read_b, (vec![b"b".to_vec()], false));
        });
    }

    #[test]
    fn that_get_several_datagrams_at_once_are_readable_once() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Command);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            drain(&mut pair.server, now, &mut incoming.receiver);
            let seen = pair.server.events.len();
            let message = shard.block(&[1; 4_000]);
            write(&mut pair.client, now, &mut sender, &[message]);
            pair.run(RUN);
            let readable = Event::Readable {
                stream: incoming.receiver.key(),
            };
            let events = &pair.server.events[seen..];
            let count = events.iter().filter(|(_, event)| *event == readable);
            assert_eq!(count.count(), 1, "{events:?}");
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![vec![1; 4_000]], false));
        });
    }

    #[test]
    fn with_a_full_pool_a_read_waits_until_a_block_frees() {
        testing::run(1, |shard| {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            // A 100-byte block takes 192 bytes of the budget, and `_filled` leaves 300.
            let config = block::Config {
                budget: block::footprint(1_472) + 300,
            };
            let memory = Heap::new(config.reservation());
            let pool = Rc::new(Pool::new(config, memory));
            let _filled = pool.alloc(1_472).expect("room");
            let config = Config {
                message_bytes_max: NonZeroUsize::new(pool.largest()).expect("not zero"),
                pool: Rc::clone(&pool),
                ..shard.config(pair::SERVER_KEY, Span::SECOND)
            };
            pair.server.endpoint = Endpoint::new(
                &testing::setup(&config),
                pair::SERVER_SHARD,
                NonZeroUsize::MIN,
            );
            pair.dial(pair::SERVER_KEY.public());
            pair.run(RUN);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(
                &mut pair.client,
                now,
                &mut sender,
                &[shard.block(&[9; 100])],
            );
            pair.client
                .endpoint
                .finish(now, &mut sender)
                .expect("finished");
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let held = pool.alloc(100).expect("room");
            let now = pair.now();
            assert!(missed(&mut pair.server, now, &mut incoming.receiver));
            drop(held);
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![vec![9; 100]], true));
        });
    }

    /// A window of two of the largest messages, so the receive budget holds three.
    const NARROW: usize = 2 * MESSAGE_MAX;

    /// A connected pair whose endpoints have a window of [`NARROW`] bytes.
    fn narrow(shard: &Shard) -> Pair {
        let mut pair = Pair::new(shard, Span::SECOND, DELAY);
        let sides = [
            (&mut pair.client, pair::CLIENT_KEY, pair::CLIENT_SHARD),
            (&mut pair.server, pair::SERVER_KEY, pair::SERVER_SHARD),
        ];
        for (side, private_key, index) in sides {
            let config = Config {
                window_bytes: NARROW,
                ..shard.config(private_key, Span::SECOND)
            };
            side.endpoint =
                Endpoint::new(&testing::setup(&config), index, NonZeroUsize::MIN);
        }
        pair.dial(pair::SERVER_KEY.public());
        pair.run(RUN);
        pair
    }

    /// Opens `count` raw `Complete` streams on the client, and sends on each only the
    /// prefix of a message of [`MESSAGE_MAX`] bytes. Gives the streams.
    fn prefixes(pair: &mut Pair, count: u32) -> Vec<StreamId> {
        let prefix = message::prefix(MESSAGE_MAX);
        let header = [[byte(Class::Complete)].as_slice(), &*prefix].concat();
        let mut ids = Vec::new();
        for _ in 0..count {
            ids.push(raw(pair.client.connection(), Dir::Uni, &header, false));
        }
        pair.run(RUN);
        ids
    }

    /// Accepts each stream the peer opened on the server and reads each one time,
    /// which gives `Pending`. Gives the receivers, oldest stream first.
    fn wait(pair: &mut Pair) -> Vec<Receiver> {
        let (now, server) = (pair.now(), key(&pair.server));
        let mut receivers = Vec::new();
        while let Some(mut incoming) = pair.server.endpoint.accept(server) {
            let read = next(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, Ok(Poll::Pending));
            receivers.push(incoming.receiver);
        }
        receivers
    }

    /// Runs the pair for `span` in steps of [`STEP`]. After each, the client flushes
    /// each of `senders`, starting one later than before as many writers do, and the
    /// server accepts each new stream and reads each stream. Gives each message the
    /// server read, with its stream.
    fn exchange(
        pair: &mut Pair,
        senders: &mut [Sender],
        span: Duration,
    ) -> Vec<(StreamId, Vec<u8>)> {
        let server = key(&pair.server);
        let (mut receivers, mut read) = (Vec::new(), Vec::new());
        let end = pair.now().0 + u64::try_from(span.as_nanos()).expect("fits");
        while pair.now().0 < end {
            pair.run(STEP);
            let now = pair.now();
            senders.rotate_left(1);
            for sender in &mut *senders {
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, sender, &mut None);
                assert!(flushed.is_ok(), "{flushed:?}");
            }
            while let Some(incoming) = pair.server.endpoint.accept(server) {
                receivers.push(incoming.receiver);
            }
            for receiver in &mut receivers {
                let (messages, _) = drain(&mut pair.server, now, receiver);
                let id = receiver.key().id;
                read.extend(messages.into_iter().map(|message| (id, message)));
            }
        }
        read
    }

    /// The stream, length, and distinct bytes of each of `messages`, which stay
    /// short in a failed assert.
    fn shapes(messages: &[(StreamId, Vec<u8>)]) -> Vec<(StreamId, usize, Vec<u8>)> {
        let shape = |(id, message): &(StreamId, Vec<u8>)| {
            let mut bytes = message.clone();
            bytes.dedup();
            (*id, message.len(), bytes)
        };
        messages.iter().map(shape).collect()
    }

    /// Whether `side` got `event` after its first `seen` events.
    fn got(side: &Side, seen: usize, event: &Event) -> bool {
        side.events[seen..].iter().any(|(_, other)| other == event)
    }

    #[test]
    fn prefixes_take_no_block() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            prefixes(&mut pair, testing::STREAMS_MAX);
            let before = shard.committed();
            let receivers = wait(&mut pair);
            assert_eq!(receivers.len(), 16);
            assert_eq!(shard.committed(), before);
            // Private: a prefix outside the pool shows in no public count.
            for receiver in &receivers {
                assert_eq!(receiver.reader.held(), (None, 0));
            }
        });
    }

    /// Fifteen streams each send a message of 16 bytes, one byte of each stream in
    /// each datagram. The pool gives no block until the messages are whole.
    #[test]
    fn a_read_leaves_no_view_of_a_chunk_and_one_buffer_of_the_message() {
        const STREAMS: usize = 15;
        const LEN: u8 = 16;
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let start = [byte(Class::Complete), LEN];
            let connection = pair.client.connection();
            let ids: Vec<_> = (0..STREAMS)
                .map(|_| raw(connection, Dir::Uni, &start, false))
                .collect();
            pair.run(RUN);
            let mut receivers: Vec<_> = (0..STREAMS)
                .map(|_| accept(&mut pair.server).receiver)
                .collect();
            pair.server.kept = Some(Vec::new());
            for have in 1..=LEN {
                for &id in &ids {
                    let mut send = pair.client.connection().send_stream(id);
                    assert_eq!(send.write(&[have]), Ok(1));
                }
                pair.run(RUN);
                let kept = pair.server.kept.replace(Vec::new()).expect("kept");
                // Else noq or the endpoint copied the bytes, and the test is vacuous.
                assert!(!kept.iter().all(Bytes::is_unique));
                let now = pair.now();
                for receiver in &mut receivers {
                    let read = pair.server.endpoint.read(now, receiver, |_, _| None);
                    assert!(matches!(read, Ok(Poll::Pending)), "{read:?}");
                    let len = usize::from(LEN);
                    let held = (Some((usize::from(have), len)), 0);
                    // Private: the copy outside the pool shows in no public count.
                    assert_eq!(receiver.reader.held(), held);
                }
                assert!(kept.iter().all(Bytes::is_unique));
            }
            let now = pair.now();
            for receiver in &mut receivers {
                let read = next(&mut pair.server, now, receiver);
                assert_eq!(read, Ok(Poll::Ready(Some((1..=LEN).collect()))));
                // Private: a buffer left after the read shows in no public count.
                assert_eq!(receiver.reader.held(), (None, 0));
            }
        });
    }

    #[test]
    fn a_too_large_read_into_leaves_no_view_of_a_chunk() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let body = [4; 100];
            let prefix = message::prefix(body.len());
            let bytes = [[byte(Class::Complete)].as_slice(), &*prefix, &body].concat();
            pair.server.kept = Some(Vec::new());
            raw(pair.client.connection(), Dir::Uni, &bytes, false);
            pair.run(RUN);
            let mut receiver = accept(&mut pair.server).receiver;
            let kept = pair.server.kept.replace(Vec::new()).expect("kept");
            // Else noq or the endpoint copied the bytes, and the test is vacuous.
            assert!(!kept.iter().all(Bytes::is_unique));
            let now = pair.now();
            let mut short = [0; 99];
            let read = pair
                .server
                .endpoint
                .read_into(now, &mut receiver, &mut short);
            let over = Error::TooLarge {
                bytes: 100,
                bytes_max: 99,
            };
            assert_eq!(read, Err(over));
            assert!(
                kept.iter().all(Bytes::is_unique),
                "a chunk of the message keeps a receive buffer alive past the read"
            );
            // Private: the copy outside the pool shows in no public count.
            assert_eq!(receiver.reader.held(), (Some((100, 100)), 0));
        });
    }

    /// Two streams each hold a message of 1,000 bytes that finds no block, then the
    /// client closes. The read that gives the close drops its message, and so does a
    /// read after the connection is gone.
    #[test]
    fn a_message_that_waits_for_a_block_is_dropped_when_the_connection_ends() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let body = [6; 1_000];
            let prefix = message::prefix(body.len());
            let bytes = [[byte(Class::Complete)].as_slice(), &*prefix, &body].concat();
            for _ in 0..2 {
                raw(pair.client.connection(), Dir::Uni, &bytes, false);
            }
            pair.run(RUN);
            let mut receivers = [(); 2].map(|()| accept(&mut pair.server).receiver);
            let now = pair.now();
            for receiver in &mut receivers {
                let read = pair.server.endpoint.read(now, receiver, |_, _| None);
                assert!(matches!(read, Ok(Poll::Pending)), "{read:?}");
                // Private: the copy outside the pool shows in no public count.
                // tests/memory/held.rs counts the heap that a read frees.
                assert_eq!(receiver.reader.held(), (Some((1_000, 1_000)), 0));
            }
            let (now, client) = (pair.now(), key(&pair.client));
            pair.client.endpoint.close(now, client, Code(7));
            let [mut before, mut after] = receivers;
            pair.run(RUN);
            let now = pair.now();
            let read = pair.server.endpoint.read(now, &mut before, |_, _| None);
            assert_eq!(read.map(|_| ()), Err(Error::PeerClosed { code: Code(7) }));
            // Private: tests/memory/held.rs checks this drop through the heap.
            assert_eq!(before.reader.held(), (None, 0));
            pair.run(Duration::from_secs(3));
            let now = pair.now();
            let read = pair.server.endpoint.read(now, &mut after, |_, _| None);
            assert_eq!(read.map(|_| ()), Err(Error::PeerClosed { code: Code(7) }));
            // Private: tests/memory/held.rs checks this drop through the heap.
            assert_eq!(after.reader.held(), (None, 0));
        });
    }

    /// Four streams in turn each send a whole message of [`MESSAGE_MAX`] bytes that
    /// finds no block, then reset. The receive budget holds three such messages.
    #[test]
    fn reset_messages_that_wait_for_a_block_hold_no_bytes_past_the_budget() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let large = vec![3; MESSAGE_MAX];
            let prefix = message::prefix(MESSAGE_MAX);
            let bytes = [[byte(Class::Complete)].as_slice(), &*prefix, &large].concat();
            let mut receivers = Vec::new();
            for _ in 0..4 {
                let id = raw(pair.client.connection(), Dir::Uni, &bytes, false);
                pair.run(RUN);
                let mut receiver = accept(&mut pair.server).receiver;
                for tries in 0.. {
                    let (now, mut asked) = (pair.now(), false);
                    let read = pair.server.endpoint.read(now, &mut receiver, |_, _| {
                        asked = true;
                        None
                    });
                    assert!(matches!(read, Ok(Poll::Pending)), "{read:?}");
                    if asked {
                        break;
                    }
                    assert!(tries < 100, "the message never became whole");
                    pair.run(STEP);
                }
                let mut send = pair.client.connection().send_stream(id);
                send.reset(VarInt::from_u32(5)).expect("reset");
                pair.run(RUN);
                let now = pair.now();
                let read = pair.server.endpoint.read(now, &mut receiver, |_, _| None);
                assert_eq!(read.map(|_| ()), Err(Error::Reset { code: Code(5) }));
                receivers.push(receiver);
            }
            // Private: the copies outside the pool show in no public count.
            let held: usize = receivers
                .iter()
                .filter_map(|receiver| receiver.reader.held().0)
                .map(|(_, capacity)| capacity)
                .sum();
            assert!(
                held <= NARROW + MESSAGE_MAX,
                "the readers hold {held} bytes, over the budget of {}",
                NARROW + MESSAGE_MAX
            );
        });
    }

    #[test]
    fn a_whole_message_that_finds_the_pool_full_keeps_its_room() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let config = block::Config {
                budget: block::footprint(MESSAGE_MAX),
            };
            let memory = Heap::new(config.reservation());
            let pool = Rc::new(Pool::new(config, memory));
            let held = pool.alloc(MESSAGE_MAX).expect("room");
            let config = Config {
                window_bytes: NARROW,
                pool,
                ..shard.config(pair::SERVER_KEY, Span::SECOND)
            };
            pair.server.endpoint = Endpoint::new(
                &testing::setup(&config),
                pair::SERVER_SHARD,
                NonZeroUsize::MIN,
            );
            pair.server.key = None;
            pair.dial(pair::SERVER_KEY.public());
            pair.run(RUN);
            prefixes(&mut pair, 2);
            let mut receivers = wait(&mut pair);
            let large = vec![3; MESSAGE_MAX];
            let prefix = message::prefix(MESSAGE_MAX);
            let bytes = [[byte(Class::Complete)].as_slice(), &*prefix, &large].concat();
            raw(pair.client.connection(), Dir::Uni, &bytes, true);
            pair.run(RUN);
            let mut whole = accept(&mut pair.server);
            for tries in 0.. {
                let now = pair.now();
                if missed(&mut pair.server, now, &mut whole.receiver) {
                    break;
                }
                assert!(tries < 100, "the message never became whole");
                pair.run(STEP);
            }
            // The budget holds three of the largest messages, so a fourth message
            // reads only when one of the three gives back its room.
            let message = [7; 100];
            let prefix = message::prefix(message.len());
            let bytes =
                [[byte(Class::Complete)].as_slice(), &*prefix, &message].concat();
            raw(pair.client.connection(), Dir::Uni, &bytes, true);
            pair.run(RUN);
            let mut fourth = accept(&mut pair.server);
            let now = pair.now();
            assert!(!missed(&mut pair.server, now, &mut fourth.receiver));
            assert!(missed(&mut pair.server, now, &mut whole.receiver));
            let first = receivers.remove(0);
            pair.server.endpoint.stop(now, first, Code(0));
            drop(held);
            let read = drain(&mut pair.server, now, &mut fourth.receiver);
            assert_eq!(read, (vec![message.to_vec()], true));
        });
    }

    #[test]
    fn streams_that_wait_for_budget_let_the_streams_with_a_block_finish() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut senders: Vec<Sender> = (0..testing::STREAMS_MAX)
                .map(|_| open_sender(&mut pair, Class::Complete))
                .collect();
            let now = pair.now();
            let mut expected = Vec::new();
            for (byte, sender) in iter::zip(0_u8.., &mut senders) {
                let message = vec![byte; MESSAGE_MAX];
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    sender,
                    &mut Some(shard.block(&message)),
                );
                assert!(written.is_ok(), "{written:?}");
                expected.push((sender.key().id, message));
            }
            let mut read = exchange(&mut pair, &mut senders, 50 * RUN);
            read.sort();
            assert_eq!(shapes(&read), shapes(&expected));
        });
    }

    #[test]
    fn a_header_that_the_window_splits_arrives_whole() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            // After the 10-byte hello, these leave 1 byte of the connection window.
            let messages = [vec![1; MESSAGE_MAX], vec![2; 65_516]];
            let blocks = messages.each_ref().map(|message| shard.block(message));
            write(&mut pair.client, now, &mut first, &blocks);
            let second = open_sender(&mut pair, Class::Complete);
            let message = shard.block(&[3; 100]);
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &second,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            assert_eq!(half(&mut pair.client, &second).unsent, 1..3);
            let id = second.key().id;
            let read = exchange(&mut pair, &mut [first, second], 10 * RUN);
            let read: Vec<_> = read.into_iter().filter(|&(at, _)| at == id).collect();
            assert_eq!(read, [(id, vec![3; 100])]);
        });
    }

    #[test]
    fn a_message_past_the_send_budget_waits_until_another_is_written() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            let count = fill(&mut pair, shard, &mut first);
            let mut second = open_sender(&mut pair, Class::Complete);
            let mut third = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            for (sender, byte) in [(&mut second, 0xb), (&mut third, 0xc)] {
                let message = shard.block(&vec![byte; MESSAGE_MAX]);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    sender,
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
            }
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let (read, _) = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read.len(), usize::from(count - 1));
            pair.run(RUN);
            let now = pair.now();
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &third, &mut None);
            assert_eq!(flushed, Ok(Poll::Pending));
            pair.run(RUN);
            assert!(pair.server.endpoint.accept(key(&pair.server)).is_none());
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &first, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &second, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            pair.run(Duration::ZERO);
            let writable = Event::Writable {
                stream: third.key(),
            };
            assert!(got(&pair.client, seen, &writable));
            let id = third.key().id;
            let mut senders = [first, second, third];
            let read = exchange(&mut pair, &mut senders, 10 * RUN);
            let third: Vec<_> = read.into_iter().filter(|&(at, _)| at == id).collect();
            let expected = [(id, vec![0xc; MESSAGE_MAX])];
            assert_eq!(shapes(&third), shapes(&expected));
        });
    }

    #[test]
    fn a_read_that_waits_for_budget_is_woken_when_a_message_is_read() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let ids = prefixes(&mut pair, 4);
            let mut receivers = wait(&mut pair);
            let body = vec![7; MESSAGE_MAX];
            let mut send = pair.client.connection().send_stream(ids[0]);
            assert_eq!(send.write(&body), Ok(MESSAGE_MAX));
            pair.run(RUN);
            let (seen, now) = (pair.server.events.len(), pair.now());
            let again = next(&mut pair.server, now, &mut receivers[3]);
            assert_eq!(again, Ok(Poll::Pending));
            let read = drain(&mut pair.server, now, &mut receivers[0]);
            assert_eq!(read, (vec![body], false));
            pair.run(Duration::ZERO);
            let readable = Event::Readable {
                stream: receivers[3].key(),
            };
            let events = &pair.server.events[seen..];
            let woken = events.iter().filter(|(_, other)| *other == readable);
            assert_eq!(woken.count(), 1);
        });
    }

    /// Stream `index` of a client's connection, for a [`Budget`] alone.
    fn stream(index: u64) -> Key {
        Key {
            connection: connection::Key {
                handle: noq_proto::ConnectionHandle(0),
                serial: 0,
            },
            id: StreamId::new(noq_proto::Side::Client, Dir::Uni, index),
        }
    }

    /// `N` claims of `class`.
    fn claims<const N: usize>(class: Class) -> [Claim; N] {
        [(); N].map(|()| Claim::new(class))
    }

    /// Releases `claim` with room in `order`, and returns the streams that got room.
    fn release(budget: &mut Budget, claim: &mut Claim, order: Order) -> Vec<Key> {
        let mut woken = Vec::new();
        budget.release(claim, order, |stream| woken.push(stream));
        woken
    }

    #[test]
    fn a_budget_wakes_no_stream_until_the_room_fits_the_first_claim_that_waits() {
        let mut budget = Budget::new(10);
        let [mut a, mut b, mut c, mut d] = claims(Class::Complete);
        assert!(budget.charge(stream(0), 9, &mut a, Order::RANK));
        assert!(!budget.charge(stream(1), 2, &mut b, Order::RANK));
        let woken = release(&mut budget, &mut a, Order::RANK);
        assert_eq!(woken, [stream(1)]);
        assert!(budget.charge(stream(1), 2, &mut b, Order::RANK));
        assert!(budget.charge(stream(2), 7, &mut c, Order::RANK));
        assert!(!budget.charge(stream(3), 5, &mut d, Order::RANK));
        assert_eq!(budget.queued(), 2);
        assert_eq!(release(&mut budget, &mut b, Order::RANK), []);
        let woken = release(&mut budget, &mut c, Order::RANK);
        assert_eq!(woken, [stream(3)]);
    }

    #[test]
    fn a_budget_gives_room_highest_class_first_then_oldest_first() {
        let mut budget = Budget::new(10);
        let classes = [
            Class::Latest,
            Class::CatchUp,
            Class::Command,
            Class::Latest,
            Class::Command,
        ];
        let mut claims = classes.map(Claim::new);
        let [a, rest @ ..] = &mut claims;
        assert!(budget.charge(stream(0), 10, a, Order::RANK));
        for (index, claim) in (1..).zip(rest) {
            assert!(!budget.charge(stream(index), 3, claim, Order::RANK));
        }
        assert_eq!(budget.queued(), 4);
        let woken = release(&mut budget, a, Order::RANK);
        assert_eq!(woken, [stream(2), stream(4), stream(3)]);
    }

    #[test]
    fn a_budget_gives_no_room_past_a_waiting_claim_that_does_not_fit() {
        let mut budget = Budget::new(10);
        let [mut first, mut second, mut large, mut small] = claims(Class::Complete);
        let [mut catch_up] = claims(Class::CatchUp);
        assert!(budget.charge(stream(0), 6, &mut first, Order::RANK));
        assert!(budget.charge(stream(1), 3, &mut second, Order::RANK));
        assert!(!budget.charge(stream(2), 5, &mut large, Order::RANK));
        assert!(!budget.charge(stream(3), 1, &mut small, Order::RANK));
        assert!(!budget.charge(stream(4), 1, &mut catch_up, Order::RANK));
        assert_eq!(budget.queued(), 3);
        assert_eq!(release(&mut budget, &mut second, Order::RANK), []);
        let woken = release(&mut budget, &mut first, Order::RANK);
        assert_eq!(woken, [stream(2), stream(3), stream(4)]);
    }

    #[test]
    fn a_budget_gives_room_past_a_waiting_claim_that_ends() {
        let mut budget = Budget::new(10);
        let [mut a, mut b, mut c] = claims(Class::Complete);
        assert!(budget.charge(stream(0), 8, &mut a, Order::RANK));
        assert!(!budget.charge(stream(1), 5, &mut b, Order::RANK));
        assert!(!budget.charge(stream(2), 2, &mut c, Order::RANK));
        assert_eq!(release(&mut budget, &mut b, Order::RANK), [stream(2)]);
    }

    #[test]
    fn a_budget_refuses_a_claim_behind_any_higher_class_that_waits() {
        let mut budget = Budget::new(20);
        let [mut held, mut command] = claims(Class::Command);
        let [mut catch_up] = claims(Class::CatchUp);
        assert!(budget.charge(stream(0), 8, &mut held, Order::RANK));
        assert!(!budget.charge(stream(1), 15, &mut command, Order::RANK));
        assert!(!budget.charge(stream(2), 15, &mut catch_up, Order::RANK));
        assert!(!budget.admit(1, &mut Claim::new(Class::Latest), Order::RANK));
    }

    #[test]
    fn a_budget_starts_a_new_claim_only_ahead_of_lower_classes_that_wait() {
        let mut budget = Budget::new(20);
        let [mut held] = claims(Class::Command);
        let [mut waiting] = claims(Class::Latest);
        assert!(budget.charge(stream(0), 8, &mut held, Order::RANK));
        assert!(!budget.charge(stream(1), 15, &mut waiting, Order::RANK));
        let classes = [
            Class::Command,
            Class::Latest,
            Class::Complete,
            Class::CatchUp,
        ];
        let admitted =
            classes.map(|class| budget.admit(1, &mut Claim::new(class), Order::RANK));
        assert_eq!(admitted, [true, false, false, false]);
    }

    #[test]
    fn a_budget_counts_room_it_gives_until_the_claim_takes_it_or_ends() {
        let mut budget = Budget::new(10);
        let [mut a, mut b, mut c] = claims(Class::Complete);
        assert!(budget.charge(stream(0), 10, &mut a, Order::RANK));
        assert!(!budget.charge(stream(1), 6, &mut b, Order::RANK));
        assert!(!budget.charge(stream(2), 6, &mut c, Order::RANK));
        let woken = release(&mut budget, &mut a, Order::RANK);
        assert_eq!(woken, [stream(1)]);
        assert!(!budget.admit(5, &mut Claim::new(Class::Command), Order::RANK));
        let woken = release(&mut budget, &mut b, Order::RANK);
        assert_eq!(woken, [stream(2)]);
        assert!(budget.charge(stream(2), 6, &mut c, Order::RANK));
        assert!(budget.admit(4, &mut Claim::new(Class::Command), Order::RANK));
    }

    #[test]
    #[should_panic(expected = "a claim of 11 bytes is over the budget, 10 bytes")]
    fn a_budget_refuses_a_claim_over_the_budget_before_it_waits() {
        let mut budget = Budget::new(10);
        let [mut a, mut b] = claims(Class::CatchUp);
        assert!(budget.charge(stream(0), 10, &mut a, Order::RANK));
        budget.charge(stream(1), 11, &mut b, Order::RANK);
    }

    #[test]
    fn a_budget_gives_room_to_complete_before_latest_when_complete_is_first() {
        let mut budget = Budget::new(10);
        let [mut held] = claims(Class::Command);
        let [mut latest] = claims(Class::Latest);
        let [mut complete] = claims(Class::Complete);
        assert!(budget.charge(stream(0), 10, &mut held, Order::RANK));
        assert!(!budget.charge(stream(1), 5, &mut latest, Order::RANK));
        assert!(!budget.charge(stream(2), 5, &mut complete, Order::RANK));
        let woken = release(&mut budget, &mut held, Order::COMPLETE_FIRST);
        assert_eq!(woken, [stream(2), stream(1)]);
    }

    #[test]
    fn a_budget_refuses_latest_behind_a_waiting_complete_only_when_complete_is_first() {
        let mut budget = Budget::new(20);
        let [mut held] = claims(Class::Command);
        let [mut waiting] = claims(Class::Complete);
        assert!(budget.charge(stream(0), 8, &mut held, Order::RANK));
        assert!(!budget.charge(stream(1), 15, &mut waiting, Order::RANK));
        let orders = [Order::RANK, Order::COMPLETE_FIRST];
        let admitted =
            orders.map(|order| budget.admit(1, &mut Claim::new(Class::Latest), order));
        assert_eq!(admitted, [true, false]);
    }

    #[test]
    fn a_budget_tells_whether_a_class_holds_room_and_whether_it_competes() {
        let mut budget = Budget::new(10);
        let [mut latest, mut late] = claims(Class::Latest);
        let [mut complete] = claims(Class::Complete);
        let state = |budget: &Budget| {
            [Class::Latest, Class::Complete]
                .map(|class| (budget.holds(class), budget.competes(class)))
        };
        assert!(budget.charge(stream(0), 10, &mut latest, Order::RANK));
        assert!(!budget.charge(stream(1), 5, &mut complete, Order::RANK));
        assert_eq!(state(&budget), [(true, true), (false, true)]);
        assert_eq!(release(&mut budget, &mut latest, Order::RANK), [stream(1)]);
        // Room that its stream has not taken holds, and does not compete.
        assert_eq!(state(&budget), [(false, false), (true, false)]);
        assert!(!budget.charge(stream(2), 6, &mut late, Order::RANK));
        assert_eq!(state(&budget), [(false, true), (true, false)]);
        assert_eq!(
            release(&mut budget, &mut complete, Order::RANK),
            [stream(2)]
        );
        assert_eq!(state(&budget), [(true, false), (false, false)]);
        assert!(budget.charge(stream(2), 6, &mut late, Order::RANK));
        assert_eq!(state(&budget), [(true, true), (false, false)]);
        assert_eq!(release(&mut budget, &mut late, Order::RANK), []);
        assert_eq!(state(&budget), [(false, false), (false, false)]);
    }

    /// Writes two messages to `sender` on the client of a [`narrow`] pair, which
    /// leave `left` bytes of the connection's credit, with no new credit until the
    /// pair runs. Gives their lengths.
    fn spend(
        pair: &mut Pair,
        shard: &Shard,
        sender: &mut Sender,
        left: usize,
    ) -> [usize; 2] {
        let hello = Hello {
            window_bytes: NARROW,
            message_bytes_max: MESSAGE_MAX,
        };
        let prefix = message::prefix(MESSAGE_MAX).len();
        // The second message is long enough to have a prefix as long as the first.
        let second =
            NARROW - hello.encode().len() - 1 - 2 * prefix - MESSAGE_MAX - left;
        let messages = [
            shard.block(&vec![0xa; MESSAGE_MAX]),
            shard.block(&vec![0xb; second]),
        ];
        let now = pair.now();
        write(&mut pair.client, now, sender, &messages);
        [MESSAGE_MAX, second]
    }

    #[test]
    fn a_cancel_after_the_header_went_resets_the_stream_and_each_later_call_gives_why()
    {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let message = shard.block(&[0xc; 100]);
            let sent = spend(&mut pair, shard, &mut sender, message::prefix(100).len());
            let now = pair.now();
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let (read, _) = drain(&mut pair.server, now, &mut incoming.receiver);
            let lengths: Vec<_> = read.iter().map(Vec::len).collect();
            assert_eq!(lengths, sent);
            // The stream sends the rest only at the caller's next write.
            let held = half(&mut pair.client, &sender);
            assert_eq!((held.unsent.is_empty(), held.body), (true, 100));
            pair.client.endpoint.cancel(now, &sender);
            let reset = Error::Reset { code: Code(0) };
            let message = shard.block(b"d");
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(message),
            );
            assert_eq!(written, Err(reset.clone()));
            let message = shard.block(b"d");
            let written =
                pair::try_write(&mut pair.client.endpoint, now, &sender, message);
            assert_eq!(written.map(|_| ()), Err(reset.clone()));
            let finished = pair.client.endpoint.finish(now, &mut sender);
            assert_eq!(finished, Err(reset.clone()));
            pair.client.endpoint.reset(now, &mut sender, Code(9));
            pair.run(RUN);
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, Err(reset));
        });
    }

    #[test]
    fn a_peer_stop_after_a_cancel_reset_keeps_the_reset() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let message = shard.block(&[0xc; 100]);
            spend(&mut pair, shard, &mut sender, message::prefix(100).len());
            let now = pair.now();
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            pair.run(RUN);
            let incoming = accept(&mut pair.server);
            let now = pair.now();
            pair.server.endpoint.stop(now, incoming.receiver, Code(9));
            pair.client.endpoint.cancel(now, &sender);
            pair.run(RUN);
            let now = pair.now();
            let message = shard.block(b"d");
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(message),
            );
            assert_eq!(written, Err(Error::Reset { code: Code(0) }));
            let finished = pair.client.endpoint.finish(now, &mut sender);
            assert_eq!(finished, Err(Error::Reset { code: Code(0) }));
        });
    }

    #[test]
    fn a_cancel_after_only_the_class_byte_went_keeps_the_stream() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            spend(&mut pair, shard, &mut first, 1);
            let second = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let message = shard.block(&[0xc; 100]);
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &second,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            assert_eq!(half(&mut pair.client, &second).unsent.start, 1);
            pair.client.endpoint.cancel(now, &second);
            let id = second.key().id;
            let message = shard.block(b"d");
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &second,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            let mut senders = [first, second];
            let read = exchange(&mut pair, &mut senders, 10 * RUN);
            let read: Vec<_> = read.into_iter().filter(|&(at, _)| at == id).collect();
            assert_eq!(read, [(id, b"d".to_vec())]);
        });
    }

    #[test]
    fn a_cancel_after_only_the_class_byte_went_then_a_finish_resets_the_reply() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            spend(&mut pair, shard, &mut first, 1);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Complete);
            let (mut sender, mut receiver) = opened.expect("a stream");
            let message = shard.block(&[0xc; 100]);
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            assert_eq!(half(&mut pair.client, &sender).unsent.start, 1);
            pair.client.endpoint.cancel(now, &sender);
            let finished = pair.client.endpoint.finish(now, &mut sender);
            assert_eq!(finished, Ok(()));
            pair.run(RUN);
            let incoming = accept(&mut pair.server);
            assert_eq!(incoming.receiver.key().id, first.key().id);
            assert!(
                pair.server
                    .endpoint
                    .accept(self::key(&pair.server))
                    .is_none()
            );
            pair.run(RUN);
            let now = pair.now();
            let read = next(&mut pair.client, now, &mut receiver);
            assert_eq!(read, Err(Error::Reset { code: Code(0) }));
        });
    }

    #[test]
    fn a_cancel_of_a_message_that_waits_for_room_gives_the_room_to_the_next() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let second = open_sender(&mut pair, Class::Complete);
            let third = open_sender(&mut pair, Class::Complete);
            let fourth = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            // The second takes room, so the third waits for room, and the fourth,
            // which would fit, waits behind it.
            let messages = [
                (&second, vec![0xb; 100]),
                (&third, vec![0xc; MESSAGE_MAX]),
                (&fourth, vec![0xd]),
            ];
            for (sender, message) in messages {
                let message = shard.block(&message);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    sender,
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
            }
            let seen = pair.client.events.len();
            pair.client.endpoint.cancel(now, &third);
            pair.run(Duration::ZERO);
            let writable = Event::Writable {
                stream: fourth.key(),
            };
            assert!(got(&pair.client, seen, &writable));
            let (now, message) = (pair.now(), shard.block(b"e"));
            let written =
                pair::write(&mut pair.client.endpoint, now, &third, &mut Some(message));
            assert_eq!(written, Ok(Poll::Pending));
            let ids = [third.key().id, fourth.key().id];
            let mut senders = [first, second, third, fourth];
            let read = exchange(&mut pair, &mut senders, 10 * RUN);
            for (id, expected) in ids.into_iter().zip([b"e".to_vec(), vec![0xd]]) {
                let at: Vec<_> =
                    read.iter().filter(|&&(at, _)| at == id).cloned().collect();
                assert_eq!(at, [(id, expected)]);
            }
        });
    }

    #[test]
    fn each_message_that_waits_for_send_budget_room_counts_once() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let waited = pair.client.endpoint.budget_waits();
            let second = open_sender(&mut pair, Class::Complete);
            let third = open_sender(&mut pair, Class::Complete);
            let fourth = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            // The second takes room at once, and the third and fourth wait for it.
            let messages = [
                (&second, vec![0xb; 100]),
                (&third, vec![0xc; MESSAGE_MAX]),
                (&fourth, vec![0xd]),
            ];
            for (sender, message) in messages {
                let message = shard.block(&message);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    sender,
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
            }
            assert_eq!(pair.client.endpoint.budget_waits(), waited + 2);
            let mut senders = [first, second, third, fourth];
            exchange(&mut pair, &mut senders, 10 * RUN);
            assert_eq!(pair.client.endpoint.budget_waits(), waited + 2);
            let (now, key) = (pair.now(), key(&pair.client));
            pair.client.endpoint.close(now, key, Code(1));
            pair.run(10 * RUN);
            assert!(pair.client.endpoint.drained());
            assert_eq!(pair.client.endpoint.budget_waits(), waited + 2);
        });
    }

    mod owed {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        /// The most bytes of one write.
        const WRITE_MAX: usize = 1 << 10;

        /// The class that writes when both `Latest` and `Complete` have a message.
        fn first(share: &Share) -> Class {
            if share.order() == Order::COMPLETE_FIRST {
                Class::Complete
            } else {
                Class::Latest
            }
        }

        #[test]
        fn complete_alone_pays_what_it_is_owed_and_gains_no_credit() {
            let mut share = Share::default();
            share.took(Class::Latest, 100, true);
            share.took(Class::Complete, 299, false);
            assert_eq!(share.order(), Order::COMPLETE_FIRST);
            share.took(Class::Complete, 1, false);
            assert_eq!(share.order(), Order::RANK);
            share.took(Class::Complete, 50, false);
            assert_eq!(share.owed, 0);
        }

        #[test]
        fn latest_alone_spends_its_credit_and_makes_no_debt() {
            let mut share = Share::default();
            share.took(Class::Complete, 300, true);
            share.took(Class::Latest, 99, false);
            assert_eq!(share.owed, -3);
            share.took(Class::Latest, 50, false);
            assert_eq!(share.owed, 0);
            assert_eq!(share.order(), Order::RANK);
        }

        #[test]
        fn latest_owes_three_bytes_of_complete_for_each_of_its_own() {
            let mut share = Share::default();
            share.took(Class::Latest, 10, true);
            assert_eq!(share.order(), Order::COMPLETE_FIRST);
            share.took(Class::Complete, 29, true);
            assert_eq!(share.order(), Order::COMPLETE_FIRST);
            share.took(Class::Complete, 1, true);
            assert_eq!(share.order(), Order::RANK);
        }

        #[test]
        fn room_an_owed_class_frees_waits_while_the_other_holds_room() {
            /// The classes that get room a message of `class` frees.
            fn room(owed: isize, class: Class, sending: &Budget) -> Vec<Class> {
                Share { owed }.room(class, sending).collect()
            }
            let mut budget = Budget::new(10);
            let [mut latest] = claims(Class::Latest);
            let [mut complete] = claims(Class::Complete);
            assert!(budget.charge(stream(0), 5, &mut latest, Order::RANK));
            assert!(budget.charge(stream(1), 5, &mut complete, Order::RANK));
            let (rank, first) = (Order::RANK.0, Order::COMPLETE_FIRST.0);
            let kept = |class| [Class::Command, class];
            assert_eq!(room(-1, Class::Latest, &budget), kept(Class::Latest));
            assert_eq!(room(-1, Class::Complete, &budget), rank);
            assert_eq!(room(0, Class::Latest, &budget), rank);
            assert_eq!(room(0, Class::Complete, &budget), rank);
            assert_eq!(room(1, Class::Complete, &budget), kept(Class::Complete));
            assert_eq!(room(1, Class::Latest, &budget), first);
            assert_eq!(room(1, Class::Command, &budget), first);
            assert_eq!(release(&mut budget, &mut complete, Order::RANK), []);
            assert_eq!(room(-1, Class::Latest, &budget), rank);
            assert_eq!(release(&mut budget, &mut latest, Order::RANK), []);
            assert_eq!(room(1, Class::Complete, &budget), first);
        }

        #[test]
        fn other_classes_owe_nothing() {
            let mut share = Share::default();
            share.took(Class::Command, 10, true);
            share.took(Class::CatchUp, 10, true);
            assert_eq!(share.owed, 0);
        }

        proptest! {
            #[test]
            fn backlogged_latest_and_complete_share_one_to_three_after_any_history(
                history in vec((0..3_u8, 1..=WRITE_MAX), 0..64),
                run in vec(1..=WRITE_MAX, 1..256),
            ) {
                let mut share = Share::default();
                let max = isize::try_from(WRITE_MAX).expect("fits");
                for (writer, bytes) in history {
                    match writer {
                        0 => share.took(Class::Latest, bytes, false),
                        1 => share.took(Class::Complete, bytes, false),
                        _ => share.took(first(&share), bytes, true),
                    }
                    prop_assert!(-max < share.owed && share.owed <= 3 * max);
                }
                let mut took = [0; 4];
                for bytes in run {
                    let class = first(&share);
                    share.took(class, bytes, true);
                    took[class.rank()] += bytes;
                }
                let [_, latest, complete, _] = took;
                let total = latest + complete;
                prop_assert!(4 * latest + 4 * WRITE_MAX > total, "{took:?}");
                prop_assert!(4 * complete + 4 * WRITE_MAX > 3 * total, "{took:?}");
            }
        }
    }

    #[test]
    fn a_stream_the_peer_opened_counts_in_its_budgets_by_its_class() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Latest);
            let (mut sender, receiver) = opened.expect("a stream");
            assert_eq!(receiver.claim.class, Class::Latest);
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let incoming = accept(&mut pair.server);
            let reply = incoming.sender.expect("a sender");
            let reply = half(&mut pair.server, &reply).claim.class;
            let classes = [incoming.receiver.claim.class, reply];
            assert_eq!(classes, [Class::Latest; 2]);
        });
    }

    #[test]
    fn a_read_that_ends_its_wait_gives_back_the_room_it_got() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let ids = prefixes(&mut pair, 6);
            let mut receivers = wait(&mut pair);
            let body = vec![7; MESSAGE_MAX];
            for at in 0..3 {
                let (mut written, mut read) = (0, Vec::new());
                while read.is_empty() {
                    let mut send = pair.client.connection().send_stream(ids[at]);
                    written += send.write(&body[written..]).unwrap_or(0);
                    pair.run(RUN);
                    let now = pair.now();
                    read = drain(&mut pair.server, now, &mut receivers[at]).0;
                }
                assert_eq!(read, slice::from_ref(&body));
            }
            for receiver in &mut receivers[3..] {
                pair.server.endpoint.end_wait(receiver);
            }
            let command = [byte(Class::Command), 1, b'c'];
            raw(pair.client.connection(), Dir::Uni, &command, true);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![b"c".to_vec()], true));
        });
    }

    #[test]
    fn a_read_wakes_a_waiting_stream_only_when_the_room_fits_its_message() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let small =
                [[byte(Class::Complete)].as_slice(), &*message::prefix(100)].concat();
            let ids = [
                raw(pair.client.connection(), Dir::Uni, &small, false),
                raw(pair.client.connection(), Dir::Uni, &small, false),
            ];
            prefixes(&mut pair, 3);
            let mut receivers = wait(&mut pair);
            let readable = Event::Readable {
                stream: receivers[4].key(),
            };
            let body = vec![7; 100];
            for (at, woken) in [(0, false), (1, true)] {
                let mut send = pair.client.connection().send_stream(ids[at]);
                assert_eq!(send.write(&body), Ok(100));
                pair.run(RUN);
                let (seen, now) = (pair.server.events.len(), pair.now());
                let read = drain(&mut pair.server, now, &mut receivers[at]);
                assert_eq!(read, (vec![body.clone()], false));
                pair.run(Duration::ZERO);
                assert_eq!(got(&pair.server, seen, &readable), woken);
            }
        });
    }

    #[test]
    fn a_reset_message_gives_back_its_budget() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let ids = prefixes(&mut pair, 4);
            let mut receivers = wait(&mut pair);
            let reset = pair
                .client
                .connection()
                .send_stream(ids[0])
                .reset(7u32.into());
            reset.expect("reset");
            pair.run(RUN);
            let (seen, now) = (pair.server.events.len(), pair.now());
            let read = next(&mut pair.server, now, &mut receivers[0]);
            assert_eq!(read, Err(Error::Reset { code: Code(7) }));
            pair.run(Duration::ZERO);
            let readable = Event::Readable {
                stream: receivers[3].key(),
            };
            assert!(got(&pair.server, seen, &readable));
        });
    }

    #[test]
    fn a_stopped_message_gives_back_its_budget() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let mut second = open_sender(&mut pair, Class::Complete);
            let mut third = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            for sender in [&mut second, &mut third] {
                let message = shard.block(&vec![1; MESSAGE_MAX]);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    sender,
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
            }
            pair.run(RUN);
            let id = accept(&mut pair.server).receiver.key().id;
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            let seen = pair.client.events.len();
            pair.run(RUN);
            let writable = Event::Writable {
                stream: third.key(),
            };
            assert!(got(&pair.client, seen, &writable));
            let now = pair.now();
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &first, &mut None);
            assert_eq!(flushed, Err(Error::Stopped { code: Code(7) }));
        });
    }

    #[test]
    fn a_stopped_message_that_waits_for_budget_fails_the_flush_and_stops_waiting() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut third = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut third, &[shard.block(b"a")]);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let mut second = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            for (sender, byte) in [(&mut second, 0xb), (&mut third, 0xc)] {
                let message = shard.block(&vec![byte; MESSAGE_MAX]);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    sender,
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
            }
            pair.run(RUN);
            let id = accept(&mut pair.server).receiver.key().id;
            assert_eq!(id, third.key().id);
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            let seen = pair.client.events.len();
            pair.run(RUN);
            let writable = Event::Writable {
                stream: third.key(),
            };
            assert!(got(&pair.client, seen, &writable));
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &third, &mut None);
            assert_eq!(flushed, Err(Error::Stopped { code: Code(7) }));
            exchange(&mut pair, &mut [first, second], 10 * RUN);
            assert!(!got(&pair.client, seen, &writable));
        });
    }

    /// The peer resets a stream before accept, after its first byte. Its first read
    /// finds no room, and no later event wakes it, so that read gives the reset.
    #[test]
    fn a_reset_first_message_that_waits_for_room_fails_its_first_read() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            prefixes(&mut pair, 4);
            let _receivers = wait(&mut pair);
            let header = [byte(Class::Complete), 10];
            let id = raw(pair.client.connection(), Dir::Uni, &header, false);
            pair.run(RUN);
            let reset = pair.client.connection().send_stream(id).reset(7u32.into());
            reset.expect("reset");
            pair.run(RUN);
            let (now, server) = (pair.now(), key(&pair.server));
            let mut incoming = pair.server.endpoint.accept(server).expect("a stream");
            let read = next(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, Err(Error::Reset { code: Code(7) }));
        });
    }

    #[test]
    fn a_reset_message_that_waits_for_budget_fails_the_read_and_stops_waiting() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let ids = prefixes(&mut pair, 4);
            let mut receivers = wait(&mut pair);
            let before = shard.committed();
            let reset = pair
                .client
                .connection()
                .send_stream(ids[3])
                .reset(7u32.into());
            reset.expect("reset");
            pair.run(RUN);
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut receivers[3]);
            assert_eq!(read, Err(Error::Reset { code: Code(7) }));
            assert_eq!(shard.committed(), before);
            let body = vec![7; MESSAGE_MAX];
            let mut send = pair.client.connection().send_stream(ids[0]);
            assert_eq!(send.write(&body), Ok(MESSAGE_MAX));
            pair.run(RUN);
            let (seen, now) = (pair.server.events.len(), pair.now());
            let read = drain(&mut pair.server, now, &mut receivers[0]);
            assert_eq!(read, (vec![body], false));
            pair.run(Duration::ZERO);
            let readable = Event::Readable {
                stream: receivers[3].key(),
            };
            assert!(!got(&pair.server, seen, &readable));
        });
    }

    #[test]
    fn a_reset_message_that_gets_room_fails_the_read_and_passes_the_room_on() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let ids = prefixes(&mut pair, 5);
            let mut receivers = wait(&mut pair);
            let reset = pair
                .client
                .connection()
                .send_stream(ids[3])
                .reset(7u32.into());
            reset.expect("reset");
            let body = vec![7; MESSAGE_MAX];
            let mut send = pair.client.connection().send_stream(ids[0]);
            assert_eq!(send.write(&body), Ok(MESSAGE_MAX));
            pair.run(RUN);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut receivers[0]);
            assert_eq!(read, (vec![body], false));
            let read = next(&mut pair.server, now, &mut receivers[4]);
            assert_eq!(read, Ok(Poll::Pending));
            let seen = pair.server.events.len();
            let read = next(&mut pair.server, now, &mut receivers[3]);
            assert_eq!(read, Err(Error::Reset { code: Code(7) }));
            pair.run(Duration::ZERO);
            let readable = Event::Readable {
                stream: receivers[4].key(),
            };
            assert!(got(&pair.server, seen, &readable));
        });
    }

    /// Opens `count` senders on the narrow `pair`'s client, and writes to each a
    /// message of [`MESSAGE_MAX`] bytes that waits.
    fn pending(pair: &mut Pair, shard: &Shard, count: usize) -> Vec<Sender> {
        let mut senders = Vec::new();
        for _ in 0..count {
            let sender = open_sender(pair, Class::Complete);
            let (now, message) = (pair.now(), shard.block(&vec![1; MESSAGE_MAX]));
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            senders.push(sender);
        }
        senders
    }

    #[test]
    fn a_reset_sender_gives_back_its_budget_and_the_peer_gets_its_code() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let senders = pending(&mut pair, shard, 2);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            assert_eq!(incoming.receiver.key().id, first.key().id);
            let (seen, now) = (pair.client.events.len(), pair.now());
            pair.client.endpoint.reset(now, &mut first, Code(9));
            pair.run(Duration::ZERO);
            let writable = Event::Writable {
                stream: senders[1].key(),
            };
            assert!(got(&pair.client, seen, &writable));
            pair.run(RUN);
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, Err(Error::Reset { code: Code(9) }));
        });
    }

    #[test]
    fn a_reset_sender_gives_back_its_blocks_when_the_peer_acknowledges() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            // noq-proto copies a write of 1452 bytes or less, and holds a larger one
            // until the peer acknowledges it. A 2000-byte block takes 2112 bytes of
            // the budget.
            let config = block::Config { budget: 3000 };
            let memory = Heap::new(config.reservation());
            let pool = Pool::new(config, memory);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let (now, message) = (pair.now(), pool.alloc(2000).expect("room").freeze());
            write(&mut pair.client, now, &mut sender, &[message]);
            pair.client.endpoint.reset(now, &mut sender, Code(9));
            let exhausted = block::Error::Exhausted {
                requested: 2000,
                available: 888,
            };
            assert_eq!(pool.alloc(2000).err(), Some(exhausted));
            pair.run(RUN);
            assert_eq!(pool.alloc(2000).err(), None);
        });
    }

    #[test]
    fn a_reset_sender_that_waits_for_budget_stops_waiting() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let mut senders = pending(&mut pair, shard, 3);
            let (seen, now) = (pair.client.events.len(), pair.now());
            let mut waiting = senders.remove(1);
            let writable = Event::Writable {
                stream: waiting.key(),
            };
            pair.client.endpoint.reset(now, &mut waiting, Code(9));
            pair.client.endpoint.reset(now, &mut first, Code(9));
            pair.run(Duration::ZERO);
            assert!(!got(&pair.client, seen, &writable));
            let woken = Event::Writable {
                stream: senders[1].key(),
            };
            assert!(got(&pair.client, seen, &woken));
        });
    }

    #[test]
    fn a_reset_sender_that_the_peer_stops_gives_only_its_freed_stream() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let receiver = accept(&mut pair.server).receiver;
            let (seen, now) = (pair.client.events.len(), pair.now());
            pair.client.endpoint.reset(now, &mut sender, Code(9));
            pair.server.endpoint.stop(now, receiver, Code(9));
            pair.run(RUN);
            let available = Event::Available {
                key: key(&pair.client),
            };
            assert_eq!(events(&pair.client).split_off(seen), [&available]);
        });
    }

    #[test]
    fn a_reset_after_finish_drops_the_messages_the_peer_has_not_read() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![b"a".to_vec()], false));
            write(&mut pair.client, now, &mut sender, &[shard.block(b"b")]);
            let finished = pair.client.endpoint.finish(now, &mut sender);
            assert_eq!(finished, Ok(()));
            pair.client.endpoint.reset(now, &mut sender, Code(9));
            pair.run(RUN);
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, Err(Error::Reset { code: Code(9) }));
        });
    }

    #[test]
    fn a_stopped_receiver_gives_back_its_budget_and_its_block() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            prefixes(&mut pair, 4);
            let mut receivers = wait(&mut pair);
            let (seen, now, before) =
                (pair.server.events.len(), pair.now(), shard.committed());
            pair.server.endpoint.stop(now, receivers.remove(0), Code(9));
            pair.run(Duration::ZERO);
            let readable = Event::Readable {
                stream: receivers[2].key(),
            };
            assert!(got(&pair.server, seen, &readable));
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut receivers[2]);
            assert_eq!(read, Ok(Poll::Pending));
            assert_eq!(shard.committed(), before);
        });
    }

    #[test]
    fn a_stopped_receiver_gives_the_peer_its_code() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let receiver = accept(&mut pair.server).receiver;
            let now = pair.now();
            pair.server.endpoint.stop(now, receiver, Code(9));
            pair.run(RUN);
            let (now, message) = (pair.now(), shard.block(b"b"));
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(message),
            );
            assert_eq!(written, Err(Error::Stopped { code: Code(9) }));
        });
    }

    #[test]
    fn a_stopped_receiver_that_waits_for_budget_stops_waiting() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            prefixes(&mut pair, 5);
            let mut receivers = wait(&mut pair);
            let last = receivers.pop().expect("a receiver");
            let waiting = receivers.pop().expect("a receiver");
            let (seen, now) = (pair.server.events.len(), pair.now());
            let readable = Event::Readable {
                stream: waiting.key(),
            };
            pair.server.endpoint.stop(now, waiting, Code(9));
            pair.server.endpoint.stop(now, receivers.remove(0), Code(9));
            pair.run(Duration::ZERO);
            assert!(!got(&pair.server, seen, &readable));
            let woken = Event::Readable { stream: last.key() };
            assert!(got(&pair.server, seen, &woken));
        });
    }

    #[test]
    fn after_the_connection_ends_a_reset_or_stop_wakes_no_stream() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            pending(&mut pair, shard, 2);
            let (now, connection) = (pair.now(), key(&pair.client));
            pair.client.endpoint.close(now, connection, Code(7));
            pair.run(Duration::ZERO);
            let seen = pair.client.events.len();
            pair.client.endpoint.reset(now, &mut first, Code(9));
            pair.run(Duration::ZERO);
            assert_eq!(pair.client.events.len(), seen, "{:?}", pair.client.events);
            let mut pair = narrow(shard);
            prefixes(&mut pair, 4);
            let mut receivers = wait(&mut pair);
            let (now, connection) = (pair.now(), key(&pair.server));
            pair.server.endpoint.close(now, connection, Code(7));
            pair.run(Duration::ZERO);
            let seen = pair.server.events.len();
            pair.server.endpoint.stop(now, receivers.remove(0), Code(9));
            pair.run(Duration::ZERO);
            assert_eq!(pair.server.events.len(), seen, "{:?}", pair.server.events);
        });
    }

    #[test]
    fn a_reset_or_stop_after_the_end_sends_nothing() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            assert_eq!(pair.client.endpoint.finish(now, &mut sender), Ok(()));
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![b"a".to_vec()], true));
            pair.run(RUN);
            let sent = (pair.client.sent.len(), pair.server.sent.len());
            let now = pair.now();
            pair.client.endpoint.reset(now, &mut sender, Code(9));
            pair.server.endpoint.stop(now, incoming.receiver, Code(9));
            pair.run(Duration::ZERO);
            assert_eq!((pair.client.sent.len(), pair.server.sent.len()), sent);
        });
    }

    #[test]
    fn a_message_over_the_peer_largest_fails_the_write() {
        testing::run(1, |shard| {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            let config = Config {
                message_bytes_max: NonZeroUsize::new(1_472).expect("not zero"),
                ..shard.config(pair::SERVER_KEY, Span::SECOND)
            };
            let shard_key = pair::SERVER_SHARD;
            pair.server.endpoint =
                Endpoint::new(&testing::setup(&config), shard_key, NonZeroUsize::MIN);
            pair.dial(pair::SERVER_KEY.public());
            pair.run(RUN);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let too_large = Err(Error::TooLarge {
                bytes: 1_473,
                bytes_max: 1_472,
            });
            let over = shard.block(&[1; 1_473]);
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(over.clone()),
            );
            assert_eq!(written, too_large.clone().map(|()| Poll::Ready(())));
            let written =
                pair::try_write(&mut pair.client.endpoint, now, &sender, over);
            let written = written.map(|back| back.map(|back| back.to_vec()));
            assert_eq!(written, too_large.map(|()| None));
            write(
                &mut pair.client,
                now,
                &mut sender,
                &[shard.block(&[2; 1_472])],
            );
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![vec![2; 1_472]], false));
        });
    }

    #[test]
    fn a_reply_over_the_peer_largest_fails_the_write() {
        testing::run(1, |shard| {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            let config = Config {
                message_bytes_max: NonZeroUsize::new(1_472).expect("not zero"),
                ..shard.config(pair::CLIENT_KEY, Span::SECOND)
            };
            let shard_key = pair::CLIENT_SHARD;
            pair.client.endpoint =
                Endpoint::new(&testing::setup(&config), shard_key, NonZeroUsize::MIN);
            pair.dial(pair::SERVER_KEY.public());
            pair.run(RUN);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Command);
            let (mut sender, _receiver) = opened.expect("a stream");
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let incoming = accept(&mut pair.server);
            let reply = incoming.sender.expect("a two-way stream");
            let now = pair.now();
            let over = shard.block(&[1; 1_473]);
            let written =
                pair::write(&mut pair.server.endpoint, now, &reply, &mut Some(over));
            let too_large = Error::TooLarge {
                bytes: 1_473,
                bytes_max: 1_472,
            };
            assert_eq!(written, Err(too_large));
        });
    }

    #[test]
    fn past_the_limit_open_none_until_available() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut senders: Vec<Sender> = (0..testing::STREAMS_MAX)
                .map(|_| open_sender(&mut pair, Class::Command))
                .collect();
            let (now, key) = (pair.now(), key(&pair.client));
            let over = pair.client.endpoint.open_sender(now, key, Class::Command);
            assert!(over.is_none(), "{over:?}");
            for sender in &mut senders {
                write(&mut pair.client, now, sender, &[shard.block(b"a")]);
                pair.client.endpoint.finish(now, sender).expect("finished");
            }
            pair.run(RUN);
            let server = self::key(&pair.server);
            while let Some(mut incoming) = pair.server.endpoint.accept(server) {
                let now = pair.now();
                assert_eq!(
                    drain(&mut pair.server, now, &mut incoming.receiver),
                    (vec![b"a".to_vec()], true)
                );
            }
            pair.run(RUN);
            assert!(events(&pair.client).contains(&&Event::Available { key }));
            let now = pair.now();
            let next = pair.client.endpoint.open_sender(now, key, Class::Command);
            assert!(next.is_some());
        });
    }

    #[test]
    fn before_the_connection_connects_open_none() {
        testing::run(1, |shard| {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            pair.dial(pair::SERVER_KEY.public());
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Command);
            assert!(opened.is_none(), "{opened:?}");
        });
    }

    #[test]
    fn that_end_before_their_first_message_never_reach_the_peer() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Command);
            let (mut sender, mut receiver) = opened.expect("a stream");
            pair.client
                .endpoint
                .finish(now, &mut sender)
                .expect("finished");
            pair.run(RUN);
            assert_eq!(pair.server.events.len(), 2, "{:?}", pair.server.events);
            assert!(
                pair.server
                    .endpoint
                    .accept(self::key(&pair.server))
                    .is_none()
            );
            let now = pair.now();
            let read = next(&mut pair.client, now, &mut receiver);
            assert_eq!(read, Err(Error::Reset { code: Code(0) }));
        });
    }

    /// Opens [`testing::STREAMS_MAX`] raw streams in `dir` on the client, writes
    /// `bytes` on each and lets the server read them, then ends each before the first
    /// byte of its first message: with a reset of `code`, or else with a finish.
    /// Gives the ids.
    fn end_before_the_first_message_byte(
        pair: &mut Pair,
        dir: Dir,
        bytes: &[u8],
        code: Option<VarInt>,
    ) -> Vec<StreamId> {
        let ids: Vec<_> = (0..testing::STREAMS_MAX)
            .map(|_| raw(pair.client.connection(), dir, bytes, false))
            .collect();
        pair.run(RUN);
        for &id in &ids {
            let mut send = pair.client.connection().send_stream(id);
            match code {
                Some(code) => send.reset(code).expect("reset"),
                None => send.finish().expect("finished"),
            }
        }
        pair.run(RUN);
        ids
    }

    #[test]
    fn that_end_after_only_their_class_byte_reset_the_reply() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            for code in [None, Some(VarInt::from_u32(7))] {
                let bytes = [byte(Class::Complete)];
                let ids =
                    end_before_the_first_message_byte(&mut pair, Dir::Bi, &bytes, code);
                let open = pair
                    .server
                    .connection()
                    .streams()
                    .remote_open_streams(Dir::Bi);
                assert_eq!(open, 0, "{code:?}");
                assert!(pair.server.endpoint.accept(key(&pair.server)).is_none());
                for id in ids {
                    let reset =
                        pair.client.connection().recv_stream(id).received_reset();
                    assert_eq!(reset, Ok(Some(VarInt::from_u32(0))), "{code:?}");
                }
            }
        });
    }

    #[test]
    fn a_stop_before_the_class_byte_resets_the_reply() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            // A frame of the later stream opens the earlier one with no byte.
            let id = raw(pair.client.connection(), Dir::Bi, &[], false);
            let bytes = [byte(Class::Complete), 1, b'z'];
            let later = raw(pair.client.connection(), Dir::Bi, &bytes, true);
            pair.run(RUN);
            let resets = |pair: &mut Pair| {
                let stats = pair.client.connection().stats();
                stats.frame_rx.reset_stream
            };
            let before = resets(&mut pair);
            let stopped = pair.client.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            pair.run(RUN);
            assert_eq!(resets(&mut pair) - before, 1, "a reset at the stop");
            let bytes = [byte(Class::Complete), 1, b'a'];
            let mut send = pair.client.connection().send_stream(id);
            assert_eq!(send.write(&bytes), Ok(3));
            send.finish().expect("finished");
            pair.run(RUN);
            let now = pair.now();
            for (stream, message) in [(later, b"z"), (id, b"a")] {
                let mut incoming = accept(&mut pair.server);
                assert_eq!(incoming.receiver.key.id, stream);
                let read = drain(&mut pair.server, now, &mut incoming.receiver);
                assert_eq!(read, (vec![message.to_vec()], true));
                let mut reply = incoming.sender.expect("a two-way stream");
                if stream == id {
                    let block = shard.block(b"b");
                    let written = pair::write(
                        &mut pair.server.endpoint,
                        now,
                        &reply,
                        &mut Some(block),
                    );
                    assert_eq!(written, Err(Error::Stopped { code: Code(7) }));
                } else {
                    let finished = pair.server.endpoint.finish(now, &mut reply);
                    finished.expect("finished");
                }
            }
            pair.run(RUN);
            let streams = pair.server.connection().streams();
            assert_eq!(streams.remote_open_streams(Dir::Bi), 0);
        });
    }

    #[test]
    fn a_stop_after_only_the_class_byte_with_a_code_over_32_bits_breaks() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let bytes = [byte(Class::Complete)];
            let id = raw(pair.client.connection(), Dir::Bi, &bytes, false);
            pair.run(RUN);
            let over = VarInt::from_u64(1 << 32).expect("a varint");
            let stopped = pair.client.connection().recv_stream(id).stop(over);
            stopped.expect("stopped");
            assert_broken(&mut pair, true, "a stop code over 32 bits: 4294967296");
        });
    }

    #[test]
    fn a_stop_over_32_bits_after_the_stream_dropped_before_its_message_breaks() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let bytes = [byte(Class::Complete)];
            let id = raw(pair.client.connection(), Dir::Bi, &bytes, false);
            pair.run(RUN);
            let finished = pair.client.connection().send_stream(id).finish();
            finished.expect("finished");
            // The finish goes in its own packet, ahead of the stop.
            pair.run(Duration::ZERO);
            let over = VarInt::from_u64(1 << 32).expect("a varint");
            let stopped = pair.client.connection().recv_stream(id).stop(over);
            stopped.expect("stopped");
            assert_broken(&mut pair, true, "a stop code over 32 bits: 4294967296");
        });
    }

    /// Runs `pair` until the resent stop of stream `id` reaches the side that sends
    /// on it, the server when `at_server`, after the stream is freed there. Asserts
    /// that no side closed and that that side sent no reset after the stream was
    /// freed.
    fn assert_ignored_late_stop(pair: &mut Pair, at_server: bool, id: StreamId) {
        fn side(pair: &mut Pair, server: bool) -> &mut Side {
            if server {
                &mut pair.server
            } else {
                &mut pair.client
            }
        }
        let mut freed = None;
        for _ in 0..2000 {
            pair.run(Duration::from_micros(100));
            let connection = side(pair, at_server).connection();
            let stats = connection.stats();
            if freed.is_none() && connection.send_stream(id).stopped().is_err() {
                freed = Some(stats.frame_tx.reset_stream);
            }
            if stats.frame_rx.stop_sending > 0 {
                break;
            }
        }
        let resets = freed.expect("the stream is freed before the stop arrives");
        pair.run(RUN);
        let stats = side(pair, at_server).connection().stats();
        assert!(stats.frame_rx.stop_sending > 0, "the stop arrived");
        assert_eq!(stats.frame_tx.reset_stream, resets);
        let closed = |event: &&Event| matches!(event, Event::Closed { .. });
        assert!(!events(&pair.server).iter().any(closed));
        assert!(!events(&pair.client).iter().any(closed));
    }

    #[test]
    fn ignore_a_stop_over_32_bits_after_the_reset_of_an_empty_reply_is_acknowledged() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let bytes = [byte(Class::Complete)];
            let id = raw(pair.client.connection(), Dir::Bi, &bytes, false);
            pair.run(RUN);
            let finished = pair.client.connection().send_stream(id).finish();
            finished.expect("finished");
            pair.run(Duration::ZERO);
            // The stop goes before the server's reset arrives, and the link loses it.
            let over = VarInt::from_u64(1 << 32).expect("a varint");
            let stopped = pair.client.connection().recv_stream(id).stop(over);
            stopped.expect("stopped");
            pair.client.drops = 1;
            pair.run(Duration::ZERO);
            assert_ignored_late_stop(&mut pair, true, id);
        });
    }

    #[test]
    fn ignore_a_stop_over_32_bits_after_a_caller_reset_is_acknowledged() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let id = sender.key().id;
            let now = pair.now();
            pair.client.endpoint.reset(now, &mut sender, Code(9));
            // The stop goes before the client's reset arrives. The link loses it and
            // the next datagram, the `MAX_STREAMS` of the stream that the reset frees,
            // so the resent stop comes after the reset's ACK.
            let over = VarInt::from_u64(1 << 32).expect("a varint");
            let stopped = pair.server.connection().recv_stream(id).stop(over);
            stopped.expect("stopped");
            pair.server.drops = 2;
            pair.run(Duration::ZERO);
            assert_ignored_late_stop(&mut pair, false, id);
        });
    }

    #[test]
    fn ignore_a_stop_over_32_bits_after_a_cancel_reset_is_acknowledged() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let message = shard.block(&[0xc; 100]);
            spend(&mut pair, shard, &mut sender, message::prefix(100).len());
            let now = pair.now();
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            pair.run(RUN);
            let id = sender.key().id;
            let now = pair.now();
            pair.client.endpoint.cancel(now, &sender);
            // The stop goes before the client's reset arrives. The link loses it and
            // the next datagram, so the resent stop comes after the reset's ACK.
            let over = VarInt::from_u64(1 << 32).expect("a varint");
            let stopped = pair.server.connection().recv_stream(id).stop(over);
            stopped.expect("stopped");
            pair.server.drops = 2;
            pair.run(Duration::ZERO);
            assert_ignored_late_stop(&mut pair, false, id);
        });
    }

    #[test]
    fn a_stop_after_only_the_class_byte_resets_the_reply() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let bytes = [byte(Class::Complete)];
            let id = raw(pair.client.connection(), Dir::Bi, &bytes, false);
            pair.run(RUN);
            let resets = |pair: &mut Pair| {
                let stats = pair.client.connection().stats();
                stats.frame_rx.reset_stream
            };
            let before = resets(&mut pair);
            let stopped = pair.client.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            pair.run(RUN);
            assert_eq!(resets(&mut pair) - before, 1, "a reset at the stop");
            let mut send = pair.client.connection().send_stream(id);
            assert_eq!(send.write(&[1, b'a']), Ok(2));
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![b"a".to_vec()], false));
            let reply = incoming.sender.expect("a two-way stream");
            let message = shard.block(b"b");
            let written =
                pair::write(&mut pair.server.endpoint, now, &reply, &mut Some(message));
            assert_eq!(written, Err(Error::Stopped { code: Code(7) }));
            // The reset frees the slot once both halves end.
            let finished = pair.client.connection().send_stream(id).finish();
            finished.expect("finished");
            pair.run(RUN);
            let now = pair.now();
            let read = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read, (vec![], true));
            pair.run(RUN);
            let streams = pair.server.connection().streams();
            assert_eq!(streams.remote_open_streams(Dir::Bi), 0);
        });
    }

    #[test]
    fn that_end_before_their_first_message_byte_drop_and_free_their_slot() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            for dir in [Dir::Uni, Dir::Bi] {
                for bytes in [&[][..], &[byte(Class::Complete)]] {
                    for code in [None, Some(VarInt::from_u32(7))] {
                        end_before_the_first_message_byte(&mut pair, dir, bytes, code);
                        let streams = pair.server.connection().streams();
                        let open = streams.remote_open_streams(dir);
                        assert_eq!(open, 0, "{dir:?} {bytes:?} {code:?}");
                    }
                }
            }
            let events = events(&pair.server);
            let quiet = events.iter().all(|event| {
                matches!(
                    event,
                    Event::Connected { .. }
                        | Event::Available { .. }
                        | Event::Readable { .. }
                )
            });
            assert!(quiet, "{events:?}");
            assert!(pair.server.endpoint.accept(key(&pair.server)).is_none());
        });
    }

    #[test]
    fn that_end_before_their_first_message_byte_leave_the_other_streams_alone() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.server));
            let sender = pair.server.endpoint.open_sender(now, key, Class::Complete);
            let mut sender = sender.expect("a stream");
            write(&mut pair.server, now, &mut sender, &[shard.block(b"a")]);
            let bytes = [byte(Class::Complete)];
            end_before_the_first_message_byte(&mut pair, Dir::Bi, &bytes, None);
            let now = pair.now();
            assert_eq!(pair.server.endpoint.finish(now, &mut sender), Ok(()));
            pair.run(RUN);
            let mut incoming = accept(&mut pair.client);
            let now = pair.now();
            let read = drain(&mut pair.client, now, &mut incoming.receiver);
            assert_eq!(read, (vec![b"a".to_vec()], true));
        });
    }

    #[test]
    fn reset_before_the_class_byte_with_a_code_over_32_bits_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let id = raw(pair.client.connection(), Dir::Uni, &[], false);
            let code = VarInt::from_u64(1 << 32).expect("a varint");
            let reset = pair.client.connection().send_stream(id).reset(code);
            reset.expect("reset");
            assert_broken(&mut pair, true, "a reset code over 32 bits: 4294967296");
        });
    }

    /// Opens a raw `Complete` stream on the client with the prefix of a 5 byte
    /// message, lets the server see it, and resets it with `code`. Gives the server's
    /// receiver.
    fn reset(pair: &mut Pair, code: VarInt) -> Receiver {
        let id = raw(pair.client.connection(), Dir::Uni, &[2, 5], false);
        pair.run(RUN);
        let incoming = accept(&mut pair.server);
        let reset = pair.client.connection().send_stream(id).reset(code);
        reset.expect("reset");
        pair.run(RUN);
        incoming.receiver
    }

    #[test]
    fn reset_by_the_peer_fail_each_read() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut receiver = reset(&mut pair, VarInt::from_u32(7));
            let now = pair.now();
            for _ in 0..2 {
                let read = next(&mut pair.server, now, &mut receiver);
                assert_eq!(read, Err(Error::Reset { code: Code(7) }));
            }
        });
    }

    #[test]
    fn ended_streams_give_their_own_end_after_the_connection_drains() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut reset = reset(&mut pair, VarInt::from_u32(7));
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            assert_eq!(pair.client.endpoint.finish(now, &mut sender), Ok(()));
            pair.run(RUN);
            let mut finished = accept(&mut pair.server).receiver;
            let now = pair.now();
            let drained = drain(&mut pair.server, now, &mut finished);
            assert_eq!(drained, (vec![b"a".to_vec()], true));
            let read = next(&mut pair.server, now, &mut reset);
            let stream_reset = Err(Error::Reset { code: Code(7) });
            assert_eq!(read, stream_reset);
            let server = key(&pair.server);
            pair.server.endpoint.close(now, server, Code(9));
            pair.run(Duration::from_secs(3));
            assert!(pair.server.endpoint.drained());
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut finished);
            assert_eq!(read, Ok(Poll::Ready(None)));
            assert_eq!(next(&mut pair.server, now, &mut reset), stream_reset);
        });
    }

    #[test]
    fn give_the_end_of_the_connection_ahead_of_a_reset_no_read_took() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut receiver = reset(&mut pair, VarInt::from_u32(7));
            let (now, server) = (pair.now(), key(&pair.server));
            pair.server.endpoint.close(now, server, Code(9));
            let read = next(&mut pair.server, now, &mut receiver);
            assert_eq!(read, Err(Error::Closed { code: Code(9) }));
        });
    }

    #[test]
    fn give_the_end_of_the_connection_ahead_of_a_message_no_read_took() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            assert_eq!(pair.client.endpoint.finish(now, &mut sender), Ok(()));
            pair.run(RUN);
            let mut receiver = accept(&mut pair.server).receiver;
            let (now, server) = (pair.now(), key(&pair.server));
            pair.server.endpoint.close(now, server, Code(9));
            let read = next(&mut pair.server, now, &mut receiver);
            assert_eq!(read, Err(Error::Closed { code: Code(9) }));
        });
    }

    #[test]
    fn reset_by_the_peer_fail_the_read_of_an_empty_message_no_read_took() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let id = raw(pair.client.connection(), Dir::Uni, &[2, 0], false);
            pair.run(RUN);
            let mut receiver = accept(&mut pair.server).receiver;
            let reset = pair.client.connection().send_stream(id).reset(7u32.into());
            reset.expect("reset");
            pair.run(RUN);
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut receiver);
            assert_eq!(read, Err(Error::Reset { code: Code(7) }));
        });
    }

    #[test]
    fn reset_with_a_code_over_32_bits_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let code = VarInt::from_u64(1 << 32).expect("a varint");
            let mut receiver = reset(&mut pair, code);
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut receiver);
            let reason = "a reset code over 32 bits: 4294967296";
            let broken = Error::Broken {
                reason: reason.into(),
            };
            assert_eq!(read, Err(broken));
            assert_broken(&mut pair, true, reason);
        });
    }

    /// Opens a `Complete` stream on the client with one message, and has the server
    /// stop it with `code`. Gives the client's sender.
    fn stop(pair: &mut Pair, shard: &Shard, code: VarInt) -> Sender {
        let mut sender = open_sender(pair, Class::Complete);
        let now = pair.now();
        write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
        pair.run(RUN);
        let id = accept(&mut pair.server).receiver.key().id;
        let stopped = pair.server.connection().recv_stream(id).stop(code);
        stopped.expect("stopped");
        pair.run(RUN);
        sender
    }

    #[test]
    fn stopped_by_the_peer_give_the_end_of_the_connection_once_it_ended() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = stop(&mut pair, shard, VarInt::from_u32(7));
            let (now, key) = (pair.now(), key(&pair.client));
            pair.client.endpoint.close(now, key, Code(9));
            let mut message = Some(shard.block(b"b"));
            let written =
                pair::write(&mut pair.client.endpoint, now, &sender, &mut message);
            assert_eq!(written, Err(Error::Closed { code: Code(9) }));
            let finished = pair.client.endpoint.finish(now, &mut sender);
            assert_eq!(finished, Err(Error::Closed { code: Code(9) }));
        });
    }

    #[test]
    fn stopped_by_the_peer_are_writable_and_fail_each_write() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = stop(&mut pair, shard, VarInt::from_u32(7));
            let writable = Event::Writable {
                stream: sender.key(),
            };
            let available = Event::Available {
                key: key(&pair.client),
            };
            let given = events(&pair.client);
            assert_eq!(given[given.len() - 2..], [&writable, &available]);
            let now = pair.now();
            for _ in 0..2 {
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &sender,
                    &mut Some(shard.block(b"b")),
                );
                assert_eq!(written, Err(Error::Stopped { code: Code(7) }));
            }
            let finished = pair.client.endpoint.finish(now, &mut sender);
            assert_eq!(finished, Err(Error::Stopped { code: Code(7) }));
        });
    }

    #[test]
    fn stopped_while_the_connection_window_is_shut_fail_the_write() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            let mut bulk = open_sender(&mut pair, Class::CatchUp);
            fill(&mut pair, shard, &mut bulk);
            pair.run(RUN);
            let id = accept(&mut pair.server).receiver.key().id;
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            pair.run(DELAY + DELAY / 2);
            let now = pair.now();
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &sender,
                &mut Some(shard.block(b"b")),
            );
            assert_eq!(written, Err(Error::Stopped { code: Code(7) }));
        });
    }

    #[test]
    fn stopped_by_the_peer_reset_at_once() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let now = pair.now();
            let _senders: Vec<Sender> = (0..testing::STREAMS_MAX)
                .map(|_| {
                    let mut sender = open_sender(&mut pair, Class::Complete);
                    write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
                    sender
                })
                .collect();
            pair.run(RUN);
            let server = key(&pair.server);
            while let Some(incoming) = pair.server.endpoint.accept(server) {
                let id = incoming.receiver.key().id;
                let stopped =
                    pair.server.connection().recv_stream(id).stop(7u32.into());
                stopped.expect("stopped");
            }
            pair.run(RUN);
            let open = pair
                .server
                .connection()
                .streams()
                .remote_open_streams(Dir::Uni);
            assert_eq!(open, 0);
        });
    }

    /// The count of `MAX_STREAMS` frames for one-way streams that the server sent.
    fn announced(pair: &mut Pair) -> u64 {
        pair.server.connection().stats().frame_tx.max_streams_uni
    }

    /// Ends the client's `sender`, whose stream the server holds in `incoming`, and
    /// reads it to its end on the server, which frees the stream.
    fn end(pair: &mut Pair, sender: &mut Sender, incoming: &mut [Incoming]) {
        let finished = pair.client.endpoint.finish(pair.now(), sender);
        assert_eq!(finished, Ok(()));
        pair.run(RUN);
        let id = sender.key().id;
        let at = |incoming: &&mut Incoming| incoming.receiver.key().id == id;
        let incoming = incoming.iter_mut().find(at).expect("an incoming stream");
        let now = pair.now();
        let read = drain(&mut pair.server, now, &mut incoming.receiver);
        assert_eq!(read, (vec![b"a".to_vec()], true));
        pair.run(RUN);
    }

    /// Opens `n` streams from the client, each with "a", and accepts each on the
    /// server.
    fn open(pair: &mut Pair, shard: &Shard, n: u32) -> (Vec<Sender>, Vec<Incoming>) {
        let now = pair.now();
        let senders = (0..n)
            .map(|_| {
                let mut sender = open_sender(pair, Class::Complete);
                write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
                sender
            })
            .collect();
        pair.run(RUN);
        let server = key(&pair.server);
        let incoming = iter::from_fn(|| pair.server.endpoint.accept(server)).collect();
        (senders, incoming)
    }

    /// Opens a stream from the client with no wait.
    fn try_open(pair: &mut Pair) -> Option<Sender> {
        let (now, key) = (pair.now(), key(&pair.client));
        pair.client.endpoint.open_sender(now, key, Class::Complete)
    }

    #[test]
    fn each_freed_stream_at_the_limit_gives_a_max_streams_frame_with_or_without_a_wait()
    {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (mut senders, mut incoming) =
                open(&mut pair, shard, testing::STREAMS_MAX);
            let before = announced(&mut pair);
            end(&mut pair, &mut senders[0], &mut incoming);
            assert_eq!(announced(&mut pair), before + 1, "a free, and no wait");
            assert!(try_open(&mut pair).is_some());
            assert!(try_open(&mut pair).is_none());
            pair.run(RUN);
            assert_eq!(announced(&mut pair), before + 1, "no stream is free");
            end(&mut pair, &mut senders[1], &mut incoming);
            assert_eq!(announced(&mut pair), before + 2, "a wait, then a free");
            assert!(try_open(&mut pair).is_some());
        });
    }

    #[test]
    fn each_freed_stream_below_the_limit_gives_a_max_streams_frame() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (mut senders, mut incoming) = open(&mut pair, shard, 2);
            let before = announced(&mut pair);
            for (ended, sender) in (1..).zip(&mut senders) {
                end(&mut pair, sender, &mut incoming);
                assert_eq!(announced(&mut pair), before + ended);
            }
        });
    }

    #[test]
    fn a_burst_of_freed_streams_gives_one_max_streams_frame() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (mut senders, mut incoming) =
                open(&mut pair, shard, testing::STREAMS_MAX);
            let before = announced(&mut pair);
            for sender in &mut senders[..2] {
                let finished = pair.client.endpoint.finish(pair.now(), sender);
                assert_eq!(finished, Ok(()));
            }
            pair.run(RUN);
            let now = pair.now();
            for incoming in &mut incoming[..2] {
                let read = drain(&mut pair.server, now, &mut incoming.receiver);
                assert_eq!(read, (vec![b"a".to_vec()], true));
            }
            pair.run(RUN);
            assert_eq!(announced(&mut pair), before + 1);
            for _ in 0..2 {
                assert!(try_open(&mut pair).is_some());
            }
            assert!(try_open(&mut pair).is_none());
        });
    }

    #[test]
    fn stopped_with_a_code_over_32_bits_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let code = VarInt::from_u64(1 << 32).expect("a varint");
            stop(&mut pair, shard, code);
            assert_broken(&mut pair, false, "a stop code over 32 bits: 4294967296");
        });
    }

    #[test]
    fn with_a_class_over_3_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            raw(pair.client.connection(), Dir::Uni, &[4], false);
            assert_broken(&mut pair, true, "a stream of class 4");
        });
    }

    #[test]
    fn with_faults_on_two_streams_at_once_close_once() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            raw(pair.client.connection(), Dir::Uni, &[4], false);
            raw(pair.client.connection(), Dir::Bi, &[4], false);
            assert_broken(&mut pair, true, "a stream of class 4");
            let events = events(&pair.server);
            let closed = events
                .iter()
                .filter(|event| matches!(event, Event::Closed { .. }));
            assert_eq!(closed.count(), 1, "{events:?}");
        });
    }

    /// Sends `bytes` and the end on a raw stream from the client, and reads it on the
    /// server, which finds the fault of `reason`.
    fn misframe(pair: &mut Pair, bytes: &[u8], reason: &str) {
        raw(pair.client.connection(), Dir::Uni, bytes, true);
        pair.run(RUN);
        let mut incoming = accept(&mut pair.server);
        assert_each_read_broken(pair, &mut incoming.receiver, reason);
    }

    /// Asserts that each read of `receiver` on the server, before and after the
    /// connection closes, gives the fault of `reason`, and that the reader holds no
    /// bytes after it.
    fn assert_each_read_broken(pair: &mut Pair, receiver: &mut Receiver, reason: &str) {
        let broken = Err(Error::Broken {
            reason: reason.into(),
        });
        for _ in 0..2 {
            let now = pair.now();
            assert_eq!(next(&mut pair.server, now, receiver), broken);
            // Private: only a peer that misframes breaks a stream, and no
            // `Transport` call can misframe.
            assert_eq!(receiver.reader.held(), (None, 0));
        }
        assert_broken(pair, true, reason);
        let now = pair.now();
        assert_eq!(next(&mut pair.server, now, receiver), broken);
    }

    #[test]
    fn with_a_message_over_the_limit_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let reason = "a message of 65537 bytes is over the limit of 65536";
            misframe(&mut pair, &[2, 0x80, 1, 0, 1], reason);
        });
    }

    #[test]
    fn that_end_inside_a_message_break_the_connection() {
        let cuts = message::cut_prefixes();
        for cut in iter::once(vec![3, b'a']).chain(cuts) {
            let bytes = [&[2], cut.as_slice()].concat();
            testing::run(1, move |shard| {
                let mut pair = connected(shard);
                misframe(&mut pair, &bytes, "the stream ended inside a message");
            });
        }
    }

    #[test]
    fn that_end_inside_a_held_message_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let connection = pair.client.connection();
            let id = raw(
                connection,
                Dir::Uni,
                &[byte(Class::Complete), 5, 1, 2],
                false,
            );
            pair.run(RUN);
            let mut receiver = accept(&mut pair.server).receiver;
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut receiver);
            assert_eq!(read, Ok(Poll::Pending));
            // Private: no public call shows a part of a message.
            assert_eq!(receiver.reader.held(), (Some((2, 5)), 0));
            let mut send = pair.client.connection().send_stream(id);
            send.finish().expect("finished");
            pair.run(RUN);
            let reason = "the stream ended inside a message";
            assert_each_read_broken(&mut pair, &mut receiver, reason);
        });
    }

    #[test]
    fn after_the_connection_ends_give_its_error_also_after_it_drains() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Complete);
            let (mut sender, mut receiver) = opened.expect("a stream");
            let mut other = open_sender(&mut pair, Class::Latest);
            let mut finishing = open_sender(&mut pair, Class::Latest);
            pair.client.endpoint.close(now, key, Code(7));
            let closed = Error::Closed { code: Code(7) };
            for drained in [false, true] {
                if drained {
                    pair.run(Duration::from_secs(3));
                }
                assert_eq!(pair.client.endpoint.drained(), drained);
                let now = pair.now();
                let read = next(&mut pair.client, now, &mut receiver);
                assert_eq!(read, Err(closed.clone()));
                let endpoint = &mut pair.client.endpoint;
                assert!(endpoint.open(now, key, Class::Command).is_none());
                assert!(endpoint.accept(key).is_none());
                let flushed = pair::write(endpoint, now, &sender, &mut None);
                assert_eq!(flushed, Err(closed.clone()));
                let large = Error::TooLarge {
                    bytes: MESSAGE_MAX + 1,
                    bytes_max: MESSAGE_MAX,
                };
                let over = shard.block(&vec![1; MESSAGE_MAX + 1]);
                let written =
                    pair::write(endpoint, now, &sender, &mut Some(over.clone()));
                assert_eq!(written, Err(large.clone()));
                let given = try_write(&mut pair.client, now, &mut other, over);
                assert_eq!(given, Err(large));
                let endpoint = &mut pair.client.endpoint;
                let mut message = Some(shard.block(b"a"));
                let written = pair::write(endpoint, now, &sender, &mut message);
                assert_eq!(written, Err(closed.clone()));
                assert!(message.is_some());
                assert_eq!(endpoint.finish(now, &mut finishing), Err(closed.clone()));
                let block = shard.block(b"b");
                let given = try_write(&mut pair.client, now, &mut other, block);
                assert_eq!(given, Err(closed.clone()));
            }
            let now = pair.now();
            let endpoint = &mut pair.client.endpoint;
            endpoint.reset(now, &mut sender, Code(9));
            endpoint.stop(now, receiver, Code(9));
        });
    }

    #[test]
    fn after_a_peer_close_or_a_fault_give_its_error_also_after_it_drains() {
        let broken = Error::Broken {
            reason: "a stream of class 4".into(),
        };
        let ends = [(true, Error::PeerClosed { code: Code(7) }), (false, broken)];
        for (closed, error) in ends {
            testing::run(1, move |shard| {
                let mut pair = connected(shard);
                let server = key(&pair.server);
                let (now, key) = (pair.now(), key(&pair.client));
                let opened = pair.client.endpoint.open(now, key, Class::Complete);
                let (mut sender, mut receiver) = opened.expect("a stream");
                let mut other = open_sender(&mut pair, Class::Latest);
                let mut finishing = open_sender(&mut pair, Class::Latest);
                if closed {
                    pair.server.endpoint.close(now, server, Code(7));
                } else {
                    raw(pair.server.connection(), Dir::Uni, &[4], false);
                }
                let ended = |event: &&Event| matches!(event, Event::Closed { .. });
                while !events(&pair.client).iter().any(ended) {
                    pair.run(STEP);
                }
                for drained in [false, true] {
                    if drained {
                        pair.run(Duration::from_secs(3));
                    }
                    assert_eq!(pair.client.endpoint.drained(), drained);
                    let now = pair.now();
                    let read = next(&mut pair.client, now, &mut receiver);
                    assert_eq!(read, Err(error.clone()));
                    let endpoint = &mut pair.client.endpoint;
                    let flushed = pair::write(endpoint, now, &sender, &mut None);
                    assert_eq!(flushed, Err(error.clone()));
                    let mut message = Some(shard.block(b"a"));
                    let written = pair::write(endpoint, now, &sender, &mut message);
                    assert_eq!(written, Err(error.clone()));
                    let finished = endpoint.finish(now, &mut finishing);
                    assert_eq!(finished, Err(error.clone()));
                    let block = shard.block(b"b");
                    let given = try_write(&mut pair.client, now, &mut other, block);
                    assert_eq!(given, Err(error.clone()));
                }
                let now = pair.now();
                let endpoint = &mut pair.client.endpoint;
                endpoint.reset(now, &mut sender, Code(9));
                endpoint.stop(now, receiver, Code(9));
            });
        }
    }

    /// Each write of all of `parts` of `block`, with its kind: a slice of the block,
    /// a stretch copied into a new buffer, one range of the block, or sources copied
    /// into the given buffer.
    fn pieces(block: &Bytes, parts: &[Part]) -> Vec<(&'static str, Vec<u8>)> {
        let (mut left, mut buffer) = (Left::new(parts), Vec::new());
        let within = block.as_ptr_range();
        let mut pieces = Vec::new();
        while left.skip() {
            let (piece, after) = left.piece(block, &mut buffer);
            let (kind, bytes) = match piece {
                Piece::Chunk(chunk) if within.contains(&chunk.as_ptr()) => {
                    ("slice", chunk.to_vec())
                }
                Piece::Chunk(chunk) => ("stretch", chunk.to_vec()),
                Piece::Copied(bytes) if within.contains(&bytes.as_ptr()) => {
                    ("one", bytes.to_vec())
                }
                Piece::Copied(bytes) => ("copied", bytes.to_vec()),
            };
            pieces.push((kind, bytes));
            left = after;
        }
        pieces
    }

    #[test]
    fn a_message_of_parts_slices_each_long_run_and_copies_the_rest_in_stretches() {
        let body: Vec<u8> = (0..6000u32).map(|at| at.to_le_bytes()[0]).collect();
        let block = Bytes::from(body.clone());
        let part = |range: Range<usize>, zeros| Part { range, zeros };
        let parts = [
            part(0..1000, 0),
            part(9..9, 0),
            part(1000..2000, 0),
            part(2100..2108, 2),
            part(2200..2210, 0),
            part(2300..2300 + COPIED_MAX, 0),
            part(2300 + COPIED_MAX..3753, 0),
            part(3800..3801, 0),
            part(3900..3900 + COPIED_MAX + 1, 0),
            part(4000..4000 + COPIED_MAX, 0),
            part(0..0, 3),
        ];
        let stretch = [&body[2100..2108], &[0; 2], &body[2200..2210]].concat();
        let sent = [
            ("slice", body[..2000].to_vec()),
            ("copied", stretch),
            ("slice", body[2300..3753].to_vec()),
            ("copied", body[3800..3801].to_vec()),
            ("slice", body[3900..=3900 + COPIED_MAX].to_vec()),
            (
                "stretch",
                [&body[4000..4000 + COPIED_MAX], &[0; 3]].concat(),
            ),
        ];
        assert_eq!(pieces(&block, &parts), sent);
        let one = |parts| pieces(&block, parts);
        assert_eq!(one(&parts[7..8]), [("one", body[3800..3801].to_vec())]);
        let long = body[3900..=3900 + COPIED_MAX].to_vec();
        assert_eq!(one(&parts[8..9]), [("slice", long)]);
        let most = [part(0..COPIED_MAX, 0)];
        assert_eq!(one(&most), [("one", body[..COPIED_MAX].to_vec())]);
        assert_eq!(one(&parts[10..]), [("copied", vec![0; 3])]);
        assert_eq!(pieces(&block, &[]), []);
    }

    #[test]
    fn a_stretch_over_the_copied_most_goes_whole_in_a_new_buffer() {
        let body: Vec<u8> = (0..6000u32).map(|at| at.to_le_bytes()[0]).collect();
        let block = Bytes::from(body.clone());
        let part = |range: Range<usize>, zeros| Part { range, zeros };
        let check = |parts: &[Part], sent: &[(&str, Vec<u8>)]| {
            assert_eq!(pieces(&block, parts), sent);
        };
        let short: Vec<_> = (0..200).map(|at| part(at * 10..at * 10 + 8, 0)).collect();
        let bytes: Vec<u8> = short
            .iter()
            .flat_map(|part| body[part.range.clone()].to_vec())
            .collect();
        check(&short, &[("stretch", bytes)]);
        let long = [&body[..1400], &[0; 255]].concat();
        check(&[part(0..1400, u8::MAX)], &[("stretch", long)]);
        let full = [&body[..1450], &[0; 2]].concat();
        check(&[part(0..1450, 2)], &[("copied", full)]);
        let over = [&body[..1450], &[0; 3]].concat();
        check(&[part(0..1450, 3)], &[("stretch", over)]);
        let parts = [part(0..10, 0), part(100..900, 0), part(900..1600, 0)];
        let sent = [
            ("copied", body[..10].to_vec()),
            ("slice", body[100..1600].to_vec()),
        ];
        check(&parts, &sent);
        let parts = [part(0..1000, 2), part(2000..2300, 0), part(2300..2600, 0)];
        let stretch = [&body[..1000], &[0; 2], &body[2000..2600]].concat();
        check(&parts, &[("stretch", stretch)]);
    }

    mod cut {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        proptest! {
            #[test]
            fn the_writes_carry_each_byte_in_order_and_noq_keeps_each_long_one(
                drawn in vec((0..4000_usize, 0..2000_usize, any::<u8>()), 0..40),
            ) {
                let bytes = (0..4000_u32).map(|at| at.to_le_bytes()[0]);
                let block = Bytes::from_iter(bytes);
                let parts: Vec<_> = drawn
                    .into_iter()
                    .map(|(start, len, zeros)| {
                        let range = start..(start + len).min(4000);
                        Part { range, zeros }
                    })
                    .collect();
                let all: Vec<u8> = parts
                    .iter()
                    .flat_map(|part| {
                        let zeros = vec![0; usize::from(part.zeros)];
                        [block[part.range.clone()].to_vec(), zeros].concat()
                    })
                    .collect();
                let pieces = pieces(&block, &parts);
                for (kind, bytes) in &pieces {
                    let long = bytes.len() > COPIED_MAX;
                    let kept = matches!(*kind, "slice" | "stretch");
                    prop_assert_eq!(long, kept, "{} {}", kind, bytes.len());
                }
                let sent = pieces.into_iter().flat_map(|(_, bytes)| bytes);
                prop_assert_eq!(sent.collect::<Vec<u8>>(), all);
            }
        }
    }

    mod resume {
        use proptest::collection::vec;
        use proptest::prelude::*;

        use super::*;

        /// The bytes of a block.
        const BLOCK: usize = 64;

        /// The bytes of `parts` of `block`.
        fn bytes(block: &Bytes, parts: &[Part]) -> Vec<u8> {
            let pieces = pieces(block, parts).into_iter();
            pieces.flat_map(|(_, bytes)| bytes).collect()
        }

        proptest! {
            #[test]
            fn the_kept_parts_hold_each_byte_not_taken_in_order(
                drawn in vec((0..BLOCK, 0..BLOCK, 0..4_u8), 0..12),
                takes in vec(0..40_usize, 1..12),
            ) {
                let block = Bytes::from_iter((1..=u8::MAX).take(BLOCK));
                let parts: Vec<_> = drawn
                    .into_iter()
                    .map(|(a, b, zeros)| Part { range: a.min(b)..a.max(b), zeros })
                    .collect();
                let all = bytes(&block, &parts);
                let mut kept = Kept::default();
                let mut taken = 0;
                for (write, take) in takes.into_iter().enumerate() {
                    if taken == all.len() {
                        break;
                    }
                    let given = (write == 0).then_some(parts.as_slice());
                    let mut left = Left::new(given.unwrap_or(kept.left()));
                    let take = take.min(all.len() - taken);
                    left.advance(take);
                    let (head, tail) = (left.head, left.tail.len());
                    kept.keep(given, head, tail);
                    taken += take;
                    prop_assert_eq!(bytes(&block, kept.left()), &all[taken..]);
                }
            }
        }
    }

    #[test]
    fn a_write_while_the_sender_holds_part_of_a_message_leaves_it() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut sender);
            let (now, mut message) = (pair.now(), Some(shard.block(b"a")));
            let written =
                pair::write(&mut pair.client.endpoint, now, &sender, &mut message);
            assert_eq!(written, Ok(Poll::Pending));
            assert_eq!(message.as_deref(), Some(&b"a"[..]));
        });
    }

    #[test]
    fn a_large_write_while_the_sender_holds_part_of_a_message_gives_too_large() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut sender);
            let now = pair.now();
            let over = shard.block(&vec![1; MESSAGE_MAX + 1]);
            let written =
                pair::write(&mut pair.client.endpoint, now, &sender, &mut Some(over));
            let large = Error::TooLarge {
                bytes: MESSAGE_MAX + 1,
                bytes_max: MESSAGE_MAX,
            };
            assert_eq!(written, Err(large));
        });
    }

    #[test]
    fn a_finish_while_the_sender_holds_part_of_a_message_ends_the_stream_after_it() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let count = fill(&mut pair, shard, &mut sender);
            let (now, seen) = (pair.now(), pair.client.events.len());
            assert_eq!(pair.client.endpoint.finish(now, &mut sender), Ok(()));
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let (mut read, mut ended) = (Vec::new(), false);
            for _ in 0..100 {
                let now = pair.now();
                let messages;
                (messages, ended) =
                    drain(&mut pair.server, now, &mut incoming.receiver);
                read.extend(messages);
                if ended {
                    break;
                }
                pair.run(RUN);
            }
            let expected: Vec<_> = (0..count).map(|i| vec![i; MESSAGE_MAX]).collect();
            assert_eq!((read, ended), (expected, true));
            let writable = Event::Writable {
                stream: sender.key(),
            };
            assert!(!got(&pair.client, seen, &writable));
        });
    }

    #[test]
    fn a_finish_after_writable_ends_the_stream_after_the_held_message() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let count = fill(&mut pair, shard, &mut sender);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let (mut read, mut ended) =
                drain(&mut pair.server, now, &mut incoming.receiver);
            assert!(!ended);
            pair.run(RUN);
            let writable = Event::Writable {
                stream: sender.key(),
            };
            assert!(events(&pair.client).contains(&&writable));
            let now = pair.now();
            assert_eq!(pair.client.endpoint.finish(now, &mut sender), Ok(()));
            for _ in 0..100 {
                pair.run(RUN);
                let now = pair.now();
                let messages;
                (messages, ended) =
                    drain(&mut pair.server, now, &mut incoming.receiver);
                read.extend(messages);
                if ended {
                    break;
                }
            }
            let expected: Vec<_> = (0..count).map(|i| vec![i; MESSAGE_MAX]).collect();
            assert_eq!((read.len(), ended), (expected.len(), true));
            assert_eq!(read, expected);
        });
    }

    #[test]
    fn a_finish_after_the_writable_of_room_ends_the_stream_after_the_message() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [first, mut second] = hold(&mut pair, shard);
            free(&mut pair);
            let now = pair.now();
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &first, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            let seen = pair.client.events.len();
            pair.run(Duration::ZERO);
            let writable = Event::Writable {
                stream: second.key(),
            };
            assert!(got(&pair.client, seen, &writable));
            let now = pair.now();
            assert_eq!(pair.client.endpoint.finish(now, &mut second), Ok(()));
            let server = key(&pair.server);
            let (mut receivers, mut read, mut ended) = (Vec::new(), Vec::new(), false);
            for _ in 0..100 {
                pair.run(RUN);
                receivers.extend(iter::from_fn(|| pair.server.endpoint.accept(server)));
                let now = pair.now();
                for incoming in &mut receivers {
                    let receiver = &mut incoming.receiver;
                    let (messages, end) = drain(&mut pair.server, now, receiver);
                    if receiver.key().id == second.key().id {
                        read.extend(messages);
                        ended |= end;
                    }
                }
                if ended {
                    break;
                }
            }
            assert_eq!((read, ended), (vec![vec![0xb; MESSAGE_MAX]], true));
        });
    }

    #[test]
    #[should_panic(expected = "a sender is used after finish or reset")]
    fn a_write_after_finish_panics_before_it_checks_the_size() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let endpoint = &mut pair.client.endpoint;
            endpoint.finish(now, &mut sender).expect("finished");
            let over = shard.block(&vec![1; MESSAGE_MAX + 1]);
            drop(pair::write(endpoint, now, &sender, &mut Some(over)));
        });
    }

    #[test]
    #[should_panic(expected = "a sender is used after finish or reset")]
    fn a_second_finish_panics() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            pair.client
                .endpoint
                .finish(now, &mut sender)
                .expect("finished");
            drop(pair.client.endpoint.finish(now, &mut sender));
        });
    }

    /// One write of `message` to `sender` on `side` that does not wait, with a
    /// message given back as bytes.
    fn try_write(
        side: &mut Side,
        now: Monotonic,
        sender: &mut Sender,
        message: Block,
    ) -> Result<Option<Vec<u8>>, Error> {
        let given = pair::try_write(&mut side.endpoint, now, sender, message)?;
        Ok(given.map(|message| message.to_vec()))
    }

    /// Fills the send budget of the client of a [`narrow`] pair: the first of two new
    /// streams holds part of a message, and the second all of one. Gives both.
    fn hold(pair: &mut Pair, shard: &Shard) -> [Sender; 2] {
        let mut first = open_sender(pair, Class::Complete);
        fill(pair, shard, &mut first);
        let second = open_sender(pair, Class::Complete);
        let (now, message) = (pair.now(), shard.block(&vec![0xb; MESSAGE_MAX]));
        let written =
            pair::write(&mut pair.client.endpoint, now, &second, &mut Some(message));
        assert_eq!(written, Ok(Poll::Pending));
        [first, second]
    }

    #[test]
    fn a_write_that_does_not_wait_gives_back_a_message_with_no_room() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [first, second] = hold(&mut pair, shard);
            let sender = open_sender(&mut pair, Class::Complete);
            let (now, message) = (pair.now(), shard.block(b"c"));
            let address = message.as_ptr();
            let written =
                pair::try_write(&mut pair.client.endpoint, now, &sender, message);
            let given = written.expect("written").expect("given back");
            assert_eq!((given.as_ptr(), &*given), (address, b"c".as_slice()));
            let (seen, key) = (pair.client.events.len(), sender.key());
            let mut senders = [first, second, sender];
            let read = exchange(&mut pair, &mut senders, 10 * RUN);
            assert!(read.iter().all(|&(at, _)| at != key.id));
            for sender in &senders {
                assert!(!half(&mut pair.client, sender).holds());
            }
            assert!(!got(&pair.client, seen, &Event::Writable { stream: key }));
            let mut senders = senders.into_iter();
            let mut sender = senders.find(|sender| sender.key() == key).expect("there");
            let id = key.id;
            let now = pair.now();
            let written = try_write(&mut pair.client, now, &mut sender, given);
            assert_eq!(written, Ok(None));
            let read = exchange(&mut pair, slice::from_mut(&mut sender), RUN);
            assert_eq!(read, [(id, b"c".to_vec())]);
        });
    }

    #[test]
    fn a_write_that_does_not_wait_takes_a_message_that_the_stream_takes_part_of() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let (now, id) = (pair.now(), sender.key().id);
            let mut expected = Vec::new();
            for byte in 0_u8.. {
                let message = vec![byte; MESSAGE_MAX];
                let block = shard.block(&message);
                let written = try_write(&mut pair.client, now, &mut sender, block);
                assert_eq!(written, Ok(None));
                expected.push((id, message));
                if half(&mut pair.client, &sender).holds() {
                    break;
                }
            }
            let read = exchange(&mut pair, slice::from_mut(&mut sender), 10 * RUN);
            assert_eq!(shapes(&read), shapes(&expected));
        });
    }

    #[test]
    fn a_write_that_does_not_wait_gives_back_a_message_while_part_of_the_last_waits() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let count = fill(&mut pair, shard, &mut sender);
            let now = pair.now();
            let written =
                try_write(&mut pair.client, now, &mut sender, shard.block(b"c"));
            assert_eq!(written, Ok(Some(b"c".to_vec())));
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let (mut read, _) = drain(&mut pair.server, now, &mut incoming.receiver);
            pair.run(RUN);
            let writable = Event::Writable {
                stream: sender.key(),
            };
            assert!(events(&pair.client).contains(&&writable));
            let now = pair.now();
            let written =
                try_write(&mut pair.client, now, &mut sender, shard.block(b"c"));
            assert_eq!(written, Ok(None));
            pair.run(RUN);
            let now = pair.now();
            let (rest, _) = drain(&mut pair.server, now, &mut incoming.receiver);
            read.extend(rest);
            let mut expected: Vec<Vec<u8>> =
                (0..count).map(|i| vec![i; MESSAGE_MAX]).collect();
            expected.push(b"c".to_vec());
            assert_eq!(read, expected);
        });
    }

    #[test]
    fn a_write_that_does_not_wait_gives_stopped_after_the_peer_stops() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut sender = stop(&mut pair, shard, VarInt::from_u32(7));
            let stopped = Err(Error::Stopped { code: Code(7) });
            let now = pair.now();
            let written =
                try_write(&mut pair.client, now, &mut sender, shard.block(b"b"));
            assert_eq!(written, stopped);
            let _senders = hold(&mut pair, shard);
            let written =
                try_write(&mut pair.client, now, &mut sender, shard.block(b"c"));
            assert_eq!(written, stopped);
        });
    }

    #[test]
    fn a_write_that_does_not_wait_gives_stopped_once_the_peer_stopped_a_held_message() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut sender);
            pair.run(RUN);
            let id = accept(&mut pair.server).receiver.key().id;
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            pair.run(RUN);
            assert!(!half(&mut pair.client, &sender).holds());
            let now = pair.now();
            let written =
                try_write(&mut pair.client, now, &mut sender, shard.block(b"c"));
            assert_eq!(written, Err(Error::Stopped { code: Code(7) }));
        });
    }

    #[test]
    fn a_write_that_does_not_wait_gives_the_close_when_the_connection_ended() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let (now, client) = (pair.now(), key(&pair.client));
            pair.client.endpoint.close(now, client, Code(0));
            let written =
                try_write(&mut pair.client, now, &mut sender, shard.block(b"a"));
            assert_eq!(written, Err(Error::Closed { code: Code(0) }));
        });
    }

    #[test]
    fn a_write_that_does_not_wait_past_the_peer_window_gives_the_message_back() {
        testing::run(1, |shard| {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            let config = Config {
                message_bytes_max: NonZeroUsize::new(1_472).expect("not zero"),
                window_bytes: 2_000,
                ..shard.config(pair::SERVER_KEY, Span::SECOND)
            };
            let shard_key = pair::SERVER_SHARD;
            pair.server.endpoint =
                Endpoint::new(&testing::setup(&config), shard_key, NonZeroUsize::MIN);
            pair.dial(pair::SERVER_KEY.public());
            pair.run(RUN);
            let first = open_sender(&mut pair, Class::Complete);
            let second = open_sender(&mut pair, Class::Latest);
            let now = pair.now();
            // The second message waits for the peer's credit, so it holds 1,472
            // bytes of the send budget.
            for byte in [1, 2] {
                let message = shard.block(&[byte; 1_472]);
                let written =
                    pair::try_write(&mut pair.client.endpoint, now, &first, message);
                assert!(matches!(written, Ok(None)), "{written:?}");
            }
            let message = shard.block(&[3; 1_472]);
            let written =
                pair::try_write(&mut pair.client.endpoint, now, &second, message);
            assert_eq!(
                written.map(|back| back.map(|back| back.to_vec())),
                Ok(Some(vec![3; 1_472]))
            );
        });
    }

    #[test]
    #[should_panic(expected = "a sender is used after finish or reset")]
    fn a_write_that_does_not_wait_after_finish_panics() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let _senders = hold(&mut pair, shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            pair.client
                .endpoint
                .finish(now, &mut sender)
                .expect("finished");
            drop(pair::try_write(
                &mut pair.client.endpoint,
                now,
                &sender,
                shard.block(b"a"),
            ));
        });
    }

    #[test]
    fn a_waiting_command_gets_the_next_room_ahead_of_a_later_try_write() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut latest =
                [Class::Latest; 2].map(|class| open_sender(&mut pair, class));
            let now = pair.now();
            for byte in 0_u8.. {
                let block = shard.block(&vec![byte; MESSAGE_MAX]);
                let written = try_write(&mut pair.client, now, &mut latest[0], block);
                assert_eq!(written, Ok(None));
                if half(&mut pair.client, &latest[0]).holds() {
                    break;
                }
            }
            let block = shard.block(&vec![0xb; MESSAGE_MAX]);
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &latest[1],
                &mut Some(block),
            );
            assert_eq!(written, Ok(Poll::Pending));
            let command = open_sender(&mut pair, Class::Command);
            let message = shard.block(&vec![0xc; MESSAGE_MAX]);
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &command,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            drain(&mut pair.server, now, &mut incoming.receiver);
            let seen = pair.client.events.len();
            pair.run(RUN);
            let now = pair.now();
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &latest[0], &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            let written =
                try_write(&mut pair.client, now, &mut latest[0], shard.block(b"l"));
            assert_eq!(written, Ok(Some(b"l".to_vec())));
            let writable = Event::Writable {
                stream: command.key(),
            };
            assert!(got(&pair.client, seen, &writable));
            pair.run(RUN);
            let now = pair.now();
            drain(&mut pair.server, now, &mut incoming.receiver);
            let id = command.key().id;
            let [_, second] = latest;
            let read = exchange(&mut pair, &mut [second, command], 10 * RUN);
            let command: Vec<_> =
                read.into_iter().filter(|&(at, _)| at == id).collect();
            let expected = [(id, vec![0xc; MESSAGE_MAX])];
            assert_eq!(shapes(&command), shapes(&expected));
        });
    }

    #[test]
    fn a_waiting_catch_up_message_lets_a_later_latest_try_write_start() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let second = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let message = shard.block(&[0xb; 100]);
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &second,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            let catch_up = open_sender(&mut pair, Class::CatchUp);
            let message = shard.block(&vec![0xc; MESSAGE_MAX]);
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &catch_up,
                &mut Some(message),
            );
            assert_eq!(written, Ok(Poll::Pending));
            let mut latest = open_sender(&mut pair, Class::Latest);
            let written =
                try_write(&mut pair.client, now, &mut latest, shard.block(b"l"));
            assert_eq!(written, Ok(None));
            let id = latest.key().id;
            let mut senders = [first, second, catch_up, latest];
            let read = exchange(&mut pair, &mut senders, 10 * RUN);
            assert!(read.contains(&(id, b"l".to_vec())));
        });
    }

    #[test]
    fn an_empty_message_that_started_does_not_wait_behind_a_later_message() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let empty = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let message = shard.block(&[]);
            let written =
                pair::write(&mut pair.client.endpoint, now, &empty, &mut Some(message));
            assert_eq!(written, Ok(Poll::Pending));
            let mut second = open_sender(&mut pair, Class::Complete);
            let mut third = open_sender(&mut pair, Class::Complete);
            for sender in [&mut second, &mut third] {
                let message = shard.block(&vec![1; MESSAGE_MAX]);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    sender,
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
            }
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            drain(&mut pair.server, now, &mut incoming.receiver);
            pair.run(RUN);
            let now = pair.now();
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &first, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &empty, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
        });
    }

    #[test]
    fn a_stream_whose_reader_lags_takes_all_the_room_the_peer_gives() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut other = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let block = shard.block(&vec![1; MESSAGE_MAX]);
            write(&mut pair.client, now, &mut other, &[block]);
            let lagging = open_sender(&mut pair, Class::Complete);
            let message = shard.block(&[2; NARROW / 32]);
            for round in 0..3 {
                let now = pair.now();
                let mut written =
                    pair::write(&mut pair.client.endpoint, now, &lagging, &mut None);
                while written == Ok(Poll::Ready(())) {
                    let message = message.clone();
                    written = pair::write(
                        &mut pair.client.endpoint,
                        now,
                        &lagging,
                        &mut Some(message),
                    );
                }
                assert_eq!(written, Ok(Poll::Pending));
                pair.run(RUN);
                if round == 0 {
                    let mut incoming = accept(&mut pair.server);
                    let now = pair.now();
                    drain(&mut pair.server, now, &mut incoming.receiver);
                    // Under 1/8 of the window, so the peer gives no stream credit.
                    let mut incoming = accept(&mut pair.server);
                    for _ in 0..3 {
                        let read = next(&mut pair.server, now, &mut incoming.receiver);
                        assert!(matches!(read, Ok(Poll::Ready(Some(_)))), "{read:?}");
                    }
                    pair.run(RUN);
                }
            }
            let connection = pair.client.connection();
            let id = connection.streams().open(Dir::Uni).expect("a stream");
            let written = connection.send_stream(id).write(&[0]);
            assert_eq!(written, Err(WriteError::Blocked));
        });
    }

    #[test]
    fn a_write_behind_a_waiting_sender_of_its_class_waits_its_turn() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [first, second] = hold(&mut pair, shard);
            free(&mut pair);
            let now = pair.now();
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &second, &mut None);
            assert_eq!(flushed, Ok(Poll::Pending));
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &first, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &second, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
        });
    }

    #[test]
    fn a_write_that_does_not_wait_gives_back_a_message_behind_a_waiting_sender() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let mut complete = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let written =
                try_write(&mut pair.client, now, &mut complete, shard.block(b"c"));
            assert_eq!(written, Ok(Some(b"c".to_vec())));
            let mut latest = open_sender(&mut pair, Class::Latest);
            let written =
                try_write(&mut pair.client, now, &mut latest, shard.block(b"l"));
            assert_eq!(written, Ok(None));
            assert!(half(&mut pair.client, &latest).holds());
        });
    }

    #[test]
    fn room_wakes_only_the_first_waiting_sender() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [first, second] = hold(&mut pair, shard);
            let seen = pair.client.events.len();
            free(&mut pair);
            let [first_writable, second_writable] =
                [&first, &second].map(|sender| Event::Writable {
                    stream: sender.key(),
                });
            assert!(got(&pair.client, seen, &first_writable));
            assert!(!got(&pair.client, seen, &second_writable));
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &first, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            pair.run(Duration::ZERO);
            assert!(got(&pair.client, seen, &second_writable));
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &second, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
        });
    }

    #[test]
    fn a_waiting_command_writes_ahead_of_waiting_catch_up() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut bulk = open_sender(&mut pair, Class::CatchUp);
            fill(&mut pair, shard, &mut bulk);
            let command = open_sender(&mut pair, Class::Command);
            let now = pair.now();
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &command,
                &mut Some(shard.block(b"go")),
            );
            assert_eq!(written, Ok(Poll::Pending));
            let seen = pair.client.events.len();
            free(&mut pair);
            let [bulk_writable, command_writable] =
                [&bulk, &command].map(|sender| Event::Writable {
                    stream: sender.key(),
                });
            assert!(got(&pair.client, seen, &command_writable));
            assert!(!got(&pair.client, seen, &bulk_writable));
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed = pair::write(&mut pair.client.endpoint, now, &bulk, &mut None);
            assert_eq!(flushed, Ok(Poll::Pending));
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &command, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            pair.run(Duration::ZERO);
            assert!(got(&pair.client, seen, &bulk_writable));
            let flushed = pair::write(&mut pair.client.endpoint, now, &bulk, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
        });
    }

    #[test]
    fn a_reset_of_the_first_waiting_sender_wakes_the_next() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [mut first, second] = hold(&mut pair, shard);
            let (seen, now) = (pair.client.events.len(), pair.now());
            pair.client.endpoint.reset(now, &mut first, Code(9));
            pair.run(Duration::ZERO);
            let writable = Event::Writable {
                stream: second.key(),
            };
            assert!(got(&pair.client, seen, &writable));
        });
    }

    #[test]
    fn a_stop_of_the_first_waiting_sender_wakes_the_next() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [first, second] = hold(&mut pair, shard);
            pair.run(RUN);
            let id = accept(&mut pair.server).receiver.key().id;
            assert_eq!(id, first.key().id);
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            let seen = pair.client.events.len();
            pair.run(RUN);
            let writable = Event::Writable {
                stream: second.key(),
            };
            assert!(got(&pair.client, seen, &writable));
            let now = pair.now();
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &first, &mut None);
            assert_eq!(flushed, Err(Error::Stopped { code: Code(7) }));
        });
    }

    #[test]
    fn a_stop_of_a_finished_sender_that_holds_part_of_a_message_drops_it() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [mut first, second] = hold(&mut pair, shard);
            let now = pair.now();
            assert_eq!(pair.client.endpoint.finish(now, &mut first), Ok(()));
            pair.run(RUN);
            let id = accept(&mut pair.server).receiver.key().id;
            assert_eq!(id, first.key().id);
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            let seen = pair.client.events.len();
            pair.run(RUN);
            let [first_writable, second_writable] =
                [&first, &second].map(|sender| Event::Writable {
                    stream: sender.key(),
                });
            assert!(got(&pair.client, seen, &second_writable));
            assert!(!got(&pair.client, seen, &first_writable));
            assert!(!halves(&mut pair.client, &first).contains_key(&id));
        });
    }

    #[test]
    fn a_later_waiting_sender_that_leaves_wakes_no_sender_and_keeps_the_first() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut second = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut second, &[shard.block(b"a")]);
            pair.run(RUN);
            let id = accept(&mut pair.server).receiver.key().id;
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let mut third = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            for (sender, message) in [(&mut second, b"b"), (&mut third, b"c")] {
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    sender,
                    &mut Some(shard.block(message)),
                );
                assert_eq!(written, Ok(Poll::Pending));
            }
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            pair.run(RUN);
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &second, &mut None);
            assert_eq!(flushed, Err(Error::Stopped { code: Code(7) }));
            pair.client.endpoint.reset(now, &mut third, Code(9));
            pair.run(Duration::ZERO);
            assert_eq!(pair.client.events.len(), seen, "{:?}", pair.client.events);
            free(&mut pair);
            let now = pair.now();
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &first, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
        });
    }

    #[test]
    fn a_sender_woken_and_stopped_in_one_drive_gets_one_writable() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut second = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut second, &[shard.block(b"a")]);
            pair.run(RUN);
            let later = accept(&mut pair.server).receiver.key().id;
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let (now, message) = (pair.now(), Some(shard.block(b"b")));
            let written =
                pair::write(&mut pair.client.endpoint, now, &second, &mut { message });
            assert_eq!(written, Ok(Poll::Pending));
            pair.run(RUN);
            let earlier = accept(&mut pair.server).receiver.key().id;
            assert_eq!(earlier, first.key().id);
            // The packet that acks part of `first` wakes it, then stops it.
            for id in [earlier, later] {
                let stopped =
                    pair.server.connection().recv_stream(id).stop(7u32.into());
                stopped.expect("stopped");
            }
            let seen = pair.client.events.len();
            pair.run(RUN);
            let writable = |sender: &Sender| Event::Writable {
                stream: sender.key(),
            };
            let available = Event::Available {
                key: key(&pair.client),
            };
            let given = events(&pair.client).split_off(seen);
            assert_eq!(given, [&writable(&second), &writable(&first), &available]);
        });
    }

    #[test]
    fn a_sender_woken_in_the_drive_that_breaks_the_connection_gets_no_event() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut doomed = open_sender(&mut pair, Class::Complete);
            let mut waiting = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut doomed, &[shard.block(b"a")]);
            write(&mut pair.client, now, &mut waiting, &[shard.block(b"a")]);
            pair.run(RUN);
            let doomed = accept(&mut pair.server).receiver.key().id;
            accept(&mut pair.server);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let (now, message) = (pair.now(), Some(shard.block(b"b")));
            let written =
                pair::write(&mut pair.client.endpoint, now, &waiting, &mut { message });
            assert_eq!(written, Ok(Poll::Pending));
            pair.run(RUN);
            let earlier = accept(&mut pair.server).receiver.key().id;
            // One packet: the first stop wakes `waiting`, the second breaks the
            // connection.
            let over = VarInt::from_u64(1 << 32).expect("a varint");
            for (id, code) in [(earlier, 7u32.into()), (doomed, over)] {
                let stopped = pair.server.connection().recv_stream(id).stop(code);
                stopped.expect("stopped");
            }
            let seen = pair.client.events.len();
            pair.run(RUN);
            let given = events(&pair.client).split_off(seen);
            let broke = Event::Closed {
                key: key(&pair.client),
                error: Error::Broken {
                    reason: "a stop code over 32 bits: 4294967296".to_owned(),
                },
            };
            assert_eq!(given, [&broke]);
        });
    }

    #[test]
    fn writable_from_noq_proto_wakes_only_the_first_waiting_sender() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            let id = first.key().id;
            assert_eq!(writable(&mut pair, &[id]), []);
            fill(&mut pair, shard, &mut first);
            let second = open_sender(&mut pair, Class::Complete);
            let woken = Event::Writable {
                stream: first.key(),
            };
            assert_eq!(writable(&mut pair, &[second.key().id]), [woken]);
        });
    }

    #[test]
    fn writable_from_noq_proto_twice_in_one_drive_wakes_the_sender_once() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let second = open_sender(&mut pair, Class::Complete);
            let woken = Event::Writable {
                stream: first.key(),
            };
            let ids = [first.key().id, second.key().id];
            assert_eq!(writable(&mut pair, &ids), [woken]);
        });
    }

    #[test]
    fn a_sender_that_waits_again_gets_a_writable_again() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut first);
            let woken = || Event::Writable {
                stream: first.key(),
            };
            let id = first.key().id;
            assert_eq!(writable(&mut pair, &[id]), [woken()]);
            let now = pair.now();
            let written =
                pair::write(&mut pair.client.endpoint, now, &first, &mut None);
            assert_eq!(written, Ok(Poll::Pending));
            assert_eq!(writable(&mut pair, &[id]), [woken()]);
        });
    }

    #[test]
    fn a_stop_after_a_writable_that_no_write_answered_gives_no_second_writable() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut sender);
            let seen = pair.client.events.len();
            free(&mut pair);
            let stopped = pair.server.connection();
            let stopped = stopped.recv_stream(sender.key().id).stop(7u32.into());
            stopped.expect("stopped");
            pair.run(RUN);
            let woken = Event::Writable {
                stream: sender.key(),
            };
            let available = Event::Available {
                key: key(&pair.client),
            };
            assert_eq!(events(&pair.client).split_off(seen), [&woken, &available]);
        });
    }

    /// Writes each sender of `senders` that `Event::Writable` names after event
    /// `seen` on the client, with no new message, until no new event names one.
    fn answer(pair: &mut Pair, senders: &[&Sender], mut seen: usize) {
        while seen < pair.client.events.len() {
            let (_, event) = &pair.client.events[seen];
            seen += 1;
            let &Event::Writable { stream } = event else {
                continue;
            };
            let sender = senders.iter().find(|sender| sender.key() == stream);
            let now = pair.now();
            if let Some(sender) = sender {
                let written =
                    pair::write(&mut pair.client.endpoint, now, sender, &mut None);
                assert!(written.is_ok(), "{written:?}");
            }
        }
    }

    #[test]
    fn a_reset_after_a_write_that_turns_the_order_leaves_the_new_first_sender_awake() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut filler = open_sender(&mut pair, Class::Complete);
            let latest = open_sender(&mut pair, Class::Latest);
            let first = open_sender(&mut pair, Class::Complete);
            let mut second = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let small: Vec<_> = (0..120).map(|_| shard.block(&[1; 1024])).collect();
            write(&mut pair.client, now, &mut filler, &small);
            pair.run(RUN);
            let senders = [&latest, &first, &second];
            let messages = [(2, 40_000), (3, 50_000), (4, 40_000)];
            for (sender, (byte, len)) in senders.iter().zip(messages) {
                let message = Some(shard.block(&vec![byte; len]));
                let written =
                    pair::write(&mut pair.client.endpoint, now, sender, &mut {
                        message
                    });
                assert_eq!(written, Ok(Poll::Pending));
            }
            pair.run(RUN);
            let server = key(&pair.server);
            let mut incoming: Vec<_> =
                iter::from_fn(|| pair.server.endpoint.accept(server)).collect();
            let at = |incoming: &[Incoming], id: StreamId| {
                incoming
                    .iter()
                    .position(|i| i.receiver.key().id == id)
                    .expect("there")
            };
            let fill = at(&incoming, filler.key().id);
            // Steps of credit. `latest` takes the first and puts `Complete` first.
            // `first` and `second` take the rest, and the last step turns the order
            // back while `second` still holds part of its message.
            for _ in 0..5 {
                let now = pair.now();
                for _ in 0..20 {
                    let read =
                        next(&mut pair.server, now, &mut incoming[fill].receiver);
                    assert_eq!(read, Ok(Poll::Ready(Some(vec![1; 1024]))));
                }
                let seen = pair.client.events.len();
                pair.run(RUN);
                answer(&mut pair, &senders, seen);
            }
            {
                let key = key(&pair.client);
                let connection =
                    crate::quic::find(&mut pair.client.endpoint.connections, key);
                let sending = &connection.expect("a connection").streams.sending;
                assert_eq!(sending.first(), Some(latest.key()));
            }
            assert!(half(&mut pair.client, &latest).holds());
            assert!(half(&mut pair.client, &second).holds());
            let now = pair.now();
            pair.client.endpoint.reset(now, &mut second, Code(7));
            let mut got = Vec::new();
            for _ in 0..20 {
                let seen = pair.client.events.len();
                pair.run(RUN);
                answer(&mut pair, &[&latest, &first], seen);
                let now = pair.now();
                drain(&mut pair.server, now, &mut incoming[fill].receiver);
                let at = at(&incoming, latest.key().id);
                got.extend(drain(&mut pair.server, now, &mut incoming[at].receiver).0);
            }
            let lens: Vec<_> = got.iter().map(Vec::len).collect();
            assert_eq!(lens, [40_000], "the server got these messages of `latest`");
        });
    }

    #[test]
    fn a_finish_after_a_pumped_message_leaves_no_turn_behind() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut bulk = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut bulk);
            let mut first = open_sender(&mut pair, Class::Command);
            let now = pair.now();
            let written =
                try_write(&mut pair.client, now, &mut first, shard.block(b"a"));
            assert_eq!(written, Ok(None));
            assert!(half(&mut pair.client, &first).holds());
            let second = open_sender(&mut pair, Class::Command);
            let message = Some(shard.block(b"b"));
            let written =
                pair::write(&mut pair.client.endpoint, now, &second, &mut { message });
            assert_eq!(written, Ok(Poll::Pending));
            free(&mut pair);
            assert!(!half(&mut pair.client, &first).holds());
            let now = pair.now();
            assert_eq!(pair.client.endpoint.finish(now, &mut first), Ok(()));
            let flushed =
                pair::write(&mut pair.client.endpoint, now, &second, &mut None);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            let mut later = open_sender(&mut pair, Class::Command);
            pair.run(10 * RUN);
            let now = pair.now();
            let written =
                try_write(&mut pair.client, now, &mut later, shard.block(b"c"));
            assert_eq!(written, Ok(None));
        });
    }

    /// Has the server read the first stream that the client's connection opened,
    /// which gives the client room again.
    fn free(pair: &mut Pair) {
        pair.run(RUN);
        let mut incoming = accept(&mut pair.server);
        let now = pair.now();
        drain(&mut pair.server, now, &mut incoming.receiver);
        pair.run(RUN);
    }

    /// The events that the client's streams give for a noq-proto `Writable` of each
    /// stream in `ids`, all in one drive.
    fn writable(pair: &mut Pair, ids: &[StreamId]) -> VecDeque<Event> {
        let key = key(&pair.client);
        let connection = super::super::find(&mut pair.client.endpoint.connections, key);
        let connection = connection.expect("a connection");
        let mut events = VecDeque::new();
        let inner = &mut connection.inner;
        for &id in ids {
            let event = StreamEvent::Writable { id };
            let translated = connection.streams.event(inner, key, &event, &mut events);
            translated.expect("no fault");
        }
        connection.streams.pump(inner, &mut events);
        events
    }

    /// A message that needs more than one datagram.
    const BULK: usize = 2 << 10;

    /// Gives the next datagram that `from` has at `now` to `to`. `false` when `from`
    /// has none.
    fn step(from: &mut Side, to: &mut Side, now: Monotonic) -> bool {
        let mut buffer = Vec::new();
        let Some(transmit) = from.endpoint.transmit(now, &mut buffer) else {
            return false;
        };
        let meta = pair::meta(from.address, transmit.contents);
        to.endpoint.receive(now, &meta, transmit.contents);
        true
    }

    /// Gives each datagram that `from` has at `now` to `to`, one at a time, and reads
    /// `receivers` and each stream `to` accepts after each. Gives, for each datagram
    /// after which messages are whole at `to`, the class of each of them.
    fn arrivals(
        from: &mut Side,
        to: &mut Side,
        now: Monotonic,
        mut receivers: Vec<(Class, Receiver)>,
    ) -> Vec<Vec<Class>> {
        let key = key(to);
        let mut arrivals = Vec::new();
        while step(from, to, now) {
            let accepted = iter::from_fn(|| to.endpoint.accept(key));
            receivers
                .extend(accepted.map(|incoming| (incoming.class, incoming.receiver)));
            let mut whole = Vec::new();
            for (class, receiver) in &mut receivers {
                let (messages, _) = drain(to, now, receiver);
                whole.extend(messages.iter().map(|_| *class));
            }
            if !whole.is_empty() {
                arrivals.push(whole);
            }
        }
        arrivals
    }

    #[test]
    fn messages_leave_highest_class_first_whatever_order_they_were_written() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let now = pair.now();
            for class in [
                Class::CatchUp,
                Class::Complete,
                Class::Latest,
                Class::Command,
            ] {
                let mut sender = open_sender(&mut pair, class);
                write(
                    &mut pair.client,
                    now,
                    &mut sender,
                    &[shard.block(&[0; BULK])],
                );
            }
            let whole = arrivals(&mut pair.client, &mut pair.server, now, Vec::new());
            let expected = [
                [Class::Command],
                [Class::Latest],
                [Class::Complete],
                [Class::CatchUp],
            ];
            assert_eq!(whole, expected);
        });
    }

    #[test]
    fn a_two_way_stream_sends_at_the_priority_of_its_class() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let mut latest = open_sender(&mut pair, Class::Latest);
            let opened = pair.client.endpoint.open(now, key, Class::Command);
            let (mut command, _receiver) = opened.expect("a stream");
            let bulk = shard.block(&[0; BULK]);
            write(&mut pair.client, now, &mut latest, slice::from_ref(&bulk));
            write(&mut pair.client, now, &mut command, &[bulk]);
            let whole = arrivals(&mut pair.client, &mut pair.server, now, Vec::new());
            assert_eq!(whole, [[Class::Command], [Class::Latest]]);
        });
    }

    #[test]
    fn streams_of_one_class_share_in_turn() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let now = pair.now();
            let mut first = open_sender(&mut pair, Class::CatchUp);
            let mut second = open_sender(&mut pair, Class::CatchUp);
            write(
                &mut pair.client,
                now,
                &mut first,
                &[shard.block(&[1; 4 * BULK])],
            );
            write(&mut pair.client, now, &mut second, &[shard.block(b"b")]);
            assert!(step(&mut pair.client, &mut pair.server, now));
            assert!(step(&mut pair.client, &mut pair.server, now));
            let key = key(&pair.server);
            let accepted = iter::from_fn(|| pair.server.endpoint.accept(key));
            let ids: Vec<_> = accepted
                .map(|incoming| incoming.receiver.key().id)
                .collect();
            assert_eq!(ids, [first.key().id, second.key().id]);
        });
    }

    // The priority orders only the bytes that noq-proto holds. A full send window
    // makes a `Command` wait for `CatchUp` bytes to be acknowledged (#797).
    #[test]
    fn a_command_written_after_bulk_that_fills_the_window_waits() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut bulk = open_sender(&mut pair, Class::CatchUp);
            fill(&mut pair, shard, &mut bulk);
            let command = open_sender(&mut pair, Class::Command);
            let now = pair.now();
            let written = pair::write(
                &mut pair.client.endpoint,
                now,
                &command,
                &mut Some(shard.block(b"go")),
            );
            assert_eq!(written, Ok(Poll::Pending));
        });
    }

    #[test]
    fn a_reply_sends_at_the_priority_of_its_class() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Command);
            let (mut sender, receiver) = opened.expect("a stream");
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let incoming = accept(&mut pair.server);
            let mut reply = incoming.sender.expect("a two-way stream");
            let (now, key) = (pair.now(), self::key(&pair.server));
            let latest = pair.server.endpoint.open_sender(now, key, Class::Latest);
            let mut latest = latest.expect("a stream");
            let bulk = shard.block(&[0; BULK]);
            write(&mut pair.server, now, &mut latest, slice::from_ref(&bulk));
            write(&mut pair.server, now, &mut reply, &[bulk]);
            let receivers = vec![(Class::Command, receiver)];
            let whole = arrivals(&mut pair.server, &mut pair.client, now, receivers);
            assert_eq!(whole, [[Class::Command], [Class::Latest]]);
        });
    }

    #[test]
    fn a_catch_up_reply_waits_for_a_complete_stream() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::CatchUp);
            let (mut sender, receiver) = opened.expect("a stream");
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let incoming = accept(&mut pair.server);
            let mut reply = incoming.sender.expect("a two-way stream");
            let (now, key) = (pair.now(), self::key(&pair.server));
            let complete = pair.server.endpoint.open_sender(now, key, Class::Complete);
            let mut complete = complete.expect("a stream");
            let bulk = shard.block(&[0; BULK]);
            write(&mut pair.server, now, &mut reply, slice::from_ref(&bulk));
            write(&mut pair.server, now, &mut complete, &[bulk]);
            let receivers = vec![(Class::CatchUp, receiver)];
            let whole = arrivals(&mut pair.server, &mut pair.client, now, receivers);
            assert_eq!(whole, [[Class::Complete], [Class::CatchUp]]);
        });
    }

    #[test]
    fn a_reply_that_the_peer_stopped_before_its_class_fails_the_first_write() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Command);
            let (mut sender, receiver) = opened.expect("a stream");
            pair.client.endpoint.stop(now, receiver, Code(9));
            pair.run(RUN);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let incoming = accept(&mut pair.server);
            let reply = incoming.sender.expect("a two-way stream");
            let (now, message) = (pair.now(), shard.block(b"b"));
            let written =
                pair::write(&mut pair.server.endpoint, now, &reply, &mut Some(message));
            assert_eq!(written, Err(Error::Stopped { code: Code(9) }));
        });
    }

    #[test]
    fn a_reply_that_the_peer_stopped_after_it_opened_and_before_its_class_fails() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let first = pair.client.endpoint.open(now, key, Class::Command);
            let (mut sender, receiver) = first.expect("a stream");
            let second = pair.client.endpoint.open(now, key, Class::Command);
            let (_, later) = second.expect("a stream");
            // The stop of the later stream opens both on the server.
            pair.client.endpoint.stop(now, later, Code(9));
            pair.run(RUN);
            let now = pair.now();
            pair.client.endpoint.stop(now, receiver, Code(7));
            pair.run(RUN);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let incoming = accept(&mut pair.server);
            let reply = incoming.sender.expect("a two-way stream");
            let (now, message) = (pair.now(), shard.block(b"b"));
            let written =
                pair::write(&mut pair.server.endpoint, now, &reply, &mut Some(message));
            assert_eq!(written, Err(Error::Stopped { code: Code(7) }));
        });
    }

    mod share {
        use super::*;

        /// Accepts each new stream on the server and reads each of `receivers` and
        /// each new stream. Adds the message bytes read to `read`, by class rank.
        fn take(
            pair: &mut Pair,
            receivers: &mut Vec<(Class, Receiver)>,
            read: &mut [usize; 4],
        ) {
            let (server, now) = (key(&pair.server), pair.now());
            let accepted = iter::from_fn(|| pair.server.endpoint.accept(server));
            receivers
                .extend(accepted.map(|incoming| (incoming.class, incoming.receiver)));
            for (class, receiver) in receivers {
                let (messages, _) = drain(&mut pair.server, now, receiver);
                read[class.rank()] += messages.iter().map(Vec::len).sum::<usize>();
            }
        }

        /// Flushes `sender` on the client, then writes `message` to it until one
        /// waits.
        fn refill(pair: &mut Pair, sender: &mut Sender, message: &Block) {
            let now = pair.now();
            let mut flushed =
                pair::write(&mut pair.client.endpoint, now, sender, &mut None);
            while flushed == Ok(Poll::Ready(())) {
                flushed = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    sender,
                    &mut Some(message.clone()),
                );
            }
            assert_eq!(flushed, Ok(Poll::Pending));
        }

        /// Runs the pair for two [`RUN`]s and then `span`, in steps of [`STEP`]. After
        /// each, each of `senders` gets messages of `bytes` bytes until one waits,
        /// starting one later than before, and the server reads. Gives the message
        /// bytes the server read in `span`, by class rank.
        fn backlog(
            pair: &mut Pair,
            shard: &Shard,
            senders: &mut [Sender],
            bytes: usize,
            span: Duration,
        ) -> [usize; 4] {
            let message = shard.block(&vec![0; bytes]);
            let (mut receivers, mut read) = (Vec::new(), [0; 4]);
            let nanos = |span: Duration| u64::try_from(span.as_nanos()).expect("fits");
            let warm = pair.now().0 + nanos(2 * RUN);
            let end = warm + nanos(span);
            while pair.now().0 < end {
                pair.run(STEP);
                senders.rotate_left(1);
                for sender in &mut *senders {
                    refill(pair, sender, &message);
                }
                take(pair, &mut receivers, &mut read);
                // A sender that starts alone takes the window, which slow start
                // sends over about a `RUN`.
                if pair.now().0 <= warm {
                    read = [0; 4];
                }
            }
            read
        }

        /// Asserts that `Complete` got 3 bytes of `read` for each byte of `Latest`,
        /// within 5 %, over at least `messages` messages of `Latest`.
        fn assert_share(read: [usize; 4], bytes: usize, messages: usize) {
            let [_, latest, complete, _] = read;
            assert!(latest >= messages * bytes, "{read:?}");
            assert!(20 * complete.abs_diff(3 * latest) <= 3 * latest, "{read:?}");
        }

        #[test]
        fn backlogged_latest_and_complete_share_the_link_one_to_three() {
            testing::run(1, |shard| {
                let mut pair = connected(shard);
                let classes = [Class::Latest, Class::Complete];
                let mut senders = classes.map(|class| open_sender(&mut pair, class));
                let bytes = MESSAGE_MAX / 4;
                let read = backlog(&mut pair, shard, &mut senders, bytes, 5 * RUN);
                assert_share(read, bytes, 50);
            });
        }

        #[test]
        fn backlogged_latest_and_complete_share_the_send_budget_one_to_three() {
            testing::run(1, |shard| {
                let mut pair = narrow(shard);
                let classes = [
                    Class::Latest,
                    Class::Latest,
                    Class::Latest,
                    Class::Latest,
                    Class::Complete,
                ];
                let mut senders = classes.map(|class| open_sender(&mut pair, class));
                let bytes = MESSAGE_MAX;
                let read = backlog(&mut pair, shard, &mut senders, bytes, 20 * RUN);
                assert_share(read, bytes, 40);
            });
        }

        #[test]
        fn latest_waits_while_complete_is_owed_bytes_and_complete_gets_the_turn() {
            testing::run(1, |shard| {
                let mut pair = connected(shard);
                let mut complete = open_sender(&mut pair, Class::Complete);
                fill(&mut pair, shard, &mut complete);
                let mut latest =
                    [Class::Latest; 2].map(|class| open_sender(&mut pair, class));
                let now = pair.now();
                for sender in &mut latest {
                    let message = shard.block(&[1; BULK]);
                    let written = pair::write(
                        &mut pair.client.endpoint,
                        now,
                        sender,
                        &mut Some(message),
                    );
                    assert_eq!(written, Ok(Poll::Pending));
                }
                free(&mut pair);
                let [first, second] = &mut latest;
                let [complete_writable, second_writable] =
                    [&complete, &*second].map(|sender| Event::Writable {
                        stream: sender.key(),
                    });
                let (seen, now) = (pair.client.events.len(), pair.now());
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, first, &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                pair.run(Duration::ZERO);
                assert!(got(&pair.client, seen, &complete_writable));
                assert!(!got(&pair.client, seen, &second_writable));
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, second, &mut None);
                assert_eq!(flushed, Ok(Poll::Pending));
                let seen = pair.client.events.len();
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &complete, &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                pair.run(Duration::ZERO);
                assert!(got(&pair.client, seen, &second_writable));
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, second, &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
            });
        }

        #[test]
        fn a_latest_try_write_gives_back_while_complete_is_owed_and_waits() {
            testing::run(1, |shard| {
                let mut pair = connected(shard);
                let mut complete = open_sender(&mut pair, Class::Complete);
                fill(&mut pair, shard, &mut complete);
                let owing = open_sender(&mut pair, Class::Latest);
                let now = pair.now();
                let message = shard.block(&[1; BULK]);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &owing,
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
                free(&mut pair);
                let now = pair.now();
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &owing, &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                let mut late = open_sender(&mut pair, Class::Latest);
                let now = pair.now();
                let given =
                    try_write(&mut pair.client, now, &mut late, shard.block(b"l"));
                assert_eq!(given, Ok(Some(b"l".to_vec())));
            });
        }

        #[test]
        fn a_latest_try_write_gives_back_while_an_owed_complete_waits_for_room() {
            testing::run(1, |shard| {
                let mut pair = narrow(shard);
                let mut first = open_sender(&mut pair, Class::Complete);
                fill(&mut pair, shard, &mut first);
                let owing = open_sender(&mut pair, Class::Latest);
                let now = pair.now();
                let message = shard.block(&vec![1; MESSAGE_MAX - 1]);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &owing,
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
                let mut waiting =
                    [Class::Complete; 3].map(|class| open_sender(&mut pair, class));
                for (at, sender) in waiting.iter_mut().enumerate() {
                    let len = if at == 0 {
                        MESSAGE_MAX - 10
                    } else {
                        MESSAGE_MAX
                    };
                    let message = shard.block(&vec![2; len]);
                    let written = pair::write(
                        &mut pair.client.endpoint,
                        now,
                        sender,
                        &mut Some(message),
                    );
                    assert_eq!(written, Ok(Poll::Pending));
                }
                free(&mut pair);
                let now = pair.now();
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &owing, &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &first, &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                let mut late = open_sender(&mut pair, Class::Latest);
                let now = pair.now();
                let given =
                    try_write(&mut pair.client, now, &mut late, shard.block(b"l"));
                assert_eq!(given, Ok(Some(b"l".to_vec())));
            });
        }

        #[test]
        fn a_complete_try_write_gives_back_while_a_latest_waits_and_none_is_owed() {
            testing::run(1, |shard| {
                let mut pair = connected(shard);
                let mut latest = open_sender(&mut pair, Class::Latest);
                fill(&mut pair, shard, &mut latest);
                let mut late = open_sender(&mut pair, Class::Complete);
                let now = pair.now();
                let given =
                    try_write(&mut pair.client, now, &mut late, shard.block(b"c"));
                assert_eq!(given, Ok(Some(b"c".to_vec())));
            });
        }

        #[test]
        fn a_complete_try_write_gives_back_while_a_latest_waits_for_room() {
            testing::run(1, |shard| {
                let mut pair = narrow(shard);
                let mut first = open_sender(&mut pair, Class::CatchUp);
                fill(&mut pair, shard, &mut first);
                let [catch_up, latest] = [Class::CatchUp, Class::Latest]
                    .map(|class| open_sender(&mut pair, class));
                let now = pair.now();
                let short = shard.block(&[1; 10]);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &catch_up,
                    &mut Some(short),
                );
                assert_eq!(written, Ok(Poll::Pending));
                let long = shard.block(&vec![2; MESSAGE_MAX]);
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &latest,
                    &mut Some(long),
                );
                assert_eq!(written, Ok(Poll::Pending));
                let mut late = open_sender(&mut pair, Class::Complete);
                let given =
                    try_write(&mut pair.client, now, &mut late, shard.block(b"c"));
                assert_eq!(given, Ok(Some(b"c".to_vec())));
            });
        }

        #[test]
        fn room_goes_to_complete_while_it_is_owed_bytes() {
            testing::run(1, |shard| {
                let mut pair = narrow(shard);
                let mut complete = open_sender(&mut pair, Class::Complete);
                fill(&mut pair, shard, &mut complete);
                let latest =
                    [Class::Latest; 2].map(|class| open_sender(&mut pair, class));
                let waiting = open_sender(&mut pair, Class::Complete);
                let (now, message) = (pair.now(), shard.block(&vec![1; MESSAGE_MAX]));
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &latest[0],
                    &mut Some(message.clone()),
                );
                assert_eq!(written, Ok(Poll::Pending));
                // The send budget is full, so both wait for room.
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &waiting,
                    &mut Some(shard.block(b"c")),
                );
                assert_eq!(written, Ok(Poll::Pending));
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &latest[1],
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
                free(&mut pair);
                let [complete_writable, latest_writable] =
                    [&waiting, &latest[1]].map(|sender| Event::Writable {
                        stream: sender.key(),
                    });
                let (seen, now) = (pair.client.events.len(), pair.now());
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &latest[0], &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                pair.run(Duration::ZERO);
                assert!(got(&pair.client, seen, &complete_writable));
                assert!(!got(&pair.client, seen, &latest_writable));
            });
        }

        #[test]
        fn room_that_an_owed_complete_frees_waits_for_its_next_message() {
            testing::run(1, |shard| {
                let mut pair = narrow(shard);
                let (mut receivers, mut read) = (Vec::new(), [0; 4]);
                let mut complete = open_sender(&mut pair, Class::Complete);
                fill(&mut pair, shard, &mut complete);
                let mut latest =
                    [Class::Latest; 2].map(|class| open_sender(&mut pair, class));
                let (now, message) = (pair.now(), shard.block(&vec![1; MESSAGE_MAX]));
                for sender in &mut latest {
                    let written = pair::write(
                        &mut pair.client.endpoint,
                        now,
                        sender,
                        &mut Some(message.clone()),
                    );
                    assert_eq!(written, Ok(Poll::Pending));
                }
                pair.run(RUN);
                take(&mut pair, &mut receivers, &mut read);
                pair.run(RUN);
                let now = pair.now();
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &latest[0], &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                // The send budget holds `Complete` and `latest[1]`.
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &latest[0],
                    &mut Some(message),
                );
                assert_eq!(written, Ok(Poll::Pending));
                pair.run(RUN);
                take(&mut pair, &mut receivers, &mut read);
                pair.run(RUN);
                let writable = Event::Writable {
                    stream: latest[0].key(),
                };
                let (seen, now) = (pair.client.events.len(), pair.now());
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &complete, &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                pair.run(Duration::ZERO);
                assert!(!got(&pair.client, seen, &writable));
                let next = shard.block(b"c");
                let written = pair::write(
                    &mut pair.client.endpoint,
                    now,
                    &complete,
                    &mut Some(next),
                );
                assert_eq!(written, Ok(Poll::Ready(())));
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &latest[0], &mut None);
                assert_eq!(flushed, Ok(Poll::Pending));
            });
        }

        #[test]
        fn a_light_latest_load_waits_at_most_a_round_trip_behind_a_complete_backlog() {
            testing::run(1, |shard| {
                let mut pair = connected(shard);
                let mut complete = open_sender(&mut pair, Class::Complete);
                let latest = open_sender(&mut pair, Class::Latest);
                let bulk = shard.block(&vec![0; MESSAGE_MAX]);
                let (mut receivers, mut read) = (Vec::new(), [0; 4]);
                let (mut written, mut waits) = (None, Vec::new());
                let round_trip = 2 * DELAY.as_millis() / STEP.as_millis();
                for step in 0..1_000 {
                    pair.run(STEP);
                    refill(&mut pair, &mut complete, &bulk);
                    let now = pair.now();
                    if pair::write(&mut pair.client.endpoint, now, &latest, &mut None)
                        == Ok(Poll::Ready(()))
                    {
                        if let Some(at) = written.take() {
                            waits.push(step - at);
                        }
                        // Slow start sends the window that `Complete` takes alone
                        // over the first 200 steps.
                        if step >= 200 && step % 10 == 0 {
                            written = Some(step);
                            let message = shard.block(&[1; 100]);
                            let sent = pair::write(
                                &mut pair.client.endpoint,
                                now,
                                &latest,
                                &mut Some(message),
                            );
                            if sent == Ok(Poll::Ready(())) {
                                waits.push(written.take().map_or(0, |at| step - at));
                            }
                        }
                    }
                    take(&mut pair, &mut receivers, &mut read);
                }
                assert!(waits.len() >= 50, "{waits:?}");
                assert!(waits.iter().all(|&wait| wait <= round_trip), "{waits:?}");
                assert!(
                    read[Class::Complete.rank()] >= 400 * MESSAGE_MAX,
                    "{read:?}"
                );
            });
        }

        /// The bytes that `Complete` is owed on the client's connection.
        fn owed(pair: &mut Pair) -> isize {
            let key = key(&pair.client);
            let connection =
                crate::quic::find(&mut pair.client.endpoint.connections, key);
            connection.expect("a connection").streams.sending.share.owed
        }

        #[test]
        fn small_latest_and_bulk_complete_share_the_send_budget_one_to_three() {
            testing::run(1, |shard| {
                let mut pair = narrow(shard);
                let mut completes =
                    [Class::Complete; 4].map(|class| open_sender(&mut pair, class));
                let mut latest = open_sender(&mut pair, Class::Latest);
                let bulk = shard.block(&vec![0; MESSAGE_MAX]);
                let small = shard.block(&[1; 100]);
                let (mut receivers, mut read) = (Vec::new(), [0; 4]);
                let nanos =
                    |span: Duration| u64::try_from(span.as_nanos()).expect("fits");
                let warm = pair.now().0 + nanos(2 * RUN);
                let end = warm + nanos(10 * RUN);
                while pair.now().0 < end {
                    pair.run(STEP);
                    refill(&mut pair, &mut latest, &small);
                    completes.rotate_left(1);
                    for complete in &mut completes {
                        refill(&mut pair, complete, &bulk);
                        // Room that its caller has not taken counts for neither
                        // class, so the caller of `Latest` takes it at once.
                        refill(&mut pair, &mut latest, &small);
                    }
                    take(&mut pair, &mut receivers, &mut read);
                    if pair.now().0 <= warm {
                        read = [0; 4];
                    }
                }
                assert_share(read, small.len(), 10_000);
            });
        }

        #[test]
        fn room_that_waits_for_its_caller_makes_no_debt_or_credit() {
            let pairs = [
                (Class::Complete, Class::Latest),
                (Class::Latest, Class::Complete),
            ];
            for (writer, idle) in pairs {
                testing::run(1, move |shard| {
                    let mut pair = narrow(shard);
                    let mut first = open_sender(&mut pair, writer);
                    fill(&mut pair, shard, &mut first);
                    let mut second = open_sender(&mut pair, writer);
                    let mut idle = open_sender(&mut pair, idle);
                    let (now, message) =
                        (pair.now(), shard.block(&vec![1; MESSAGE_MAX]));
                    for sender in [&mut second, &mut idle] {
                        let written = pair::write(
                            &mut pair.client.endpoint,
                            now,
                            sender,
                            &mut Some(message.clone()),
                        );
                        assert_eq!(written, Ok(Poll::Pending), "{writer:?}");
                    }
                    free(&mut pair);
                    let writable = Event::Writable { stream: idle.key() };
                    let (seen, now) = (pair.client.events.len(), pair.now());
                    let flushed =
                        pair::write(&mut pair.client.endpoint, now, &first, &mut None);
                    assert_eq!(flushed, Ok(Poll::Ready(())), "{writer:?}");
                    pair.run(Duration::ZERO);
                    assert!(got(&pair.client, seen, &writable), "{writer:?}");
                    // `idle` got room and has not taken it.
                    let owed_before = owed(&mut pair);
                    let flushed =
                        pair::write(&mut pair.client.endpoint, now, &second, &mut None);
                    assert_eq!(flushed, Ok(Poll::Ready(())), "{writer:?}");
                    assert_eq!(owed(&mut pair), owed_before, "{writer:?}");
                });
            }
        }

        #[test]
        fn room_that_an_owed_complete_frees_goes_to_latest_while_no_latest_holds_room()
        {
            testing::run(1, |shard| {
                let mut pair = narrow(shard);
                let [mut first, mut second] =
                    [Class::Complete; 2].map(|class| open_sender(&mut pair, class));
                fill(&mut pair, shard, &mut first);
                let [mut owing, mut waiting] =
                    [Class::Latest; 2].map(|class| open_sender(&mut pair, class));
                let (now, message) = (pair.now(), shard.block(&vec![1; MESSAGE_MAX]));
                // `owing` takes the rest of the send budget; the others wait for room.
                for sender in [&mut owing, &mut second, &mut waiting] {
                    let written = pair::write(
                        &mut pair.client.endpoint,
                        now,
                        sender,
                        &mut Some(message.clone()),
                    );
                    assert_eq!(written, Ok(Poll::Pending));
                }
                free(&mut pair);
                let now = pair.now();
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &owing, &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                // `second` got the room that `owing` freed, and waits for its turn.
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &second, &mut None);
                assert_eq!(flushed, Ok(Poll::Pending));
                let writable = Event::Writable {
                    stream: waiting.key(),
                };
                let seen = pair.client.events.len();
                let flushed =
                    pair::write(&mut pair.client.endpoint, now, &first, &mut None);
                assert_eq!(flushed, Ok(Poll::Ready(())));
                pair.run(Duration::ZERO);
                assert!(owed(&mut pair) > 0);
                assert!(got(&pair.client, seen, &writable));
            });
        }
    }

    mod carry {
        use proptest::prelude::*;

        use super::*;

        /// The bytes of message `index` of the stream of `class`, `len` long.
        fn message(class: Class, index: usize, len: usize) -> Vec<u8> {
            let start = usize::from(byte(class)) * 8 + index;
            let bytes = (start..).map(|at| u8::try_from(at % 251).expect("fits"));
            bytes.take(len).collect()
        }

        /// Sends each of `streams` (its class and the length of each message) from the
        /// client, then finishes it. Before each round of [`DELAY`], the link drops
        /// as many of the client's next batches as the next count in `drops`. Gives
        /// the class, the messages, and whether it ended of each stream the server
        /// accepted.
        fn carry(
            value: u64,
            streams: Vec<(Class, Vec<usize>)>,
            drops: Vec<usize>,
        ) -> Vec<(Class, Vec<Vec<u8>>, bool)> {
            testing::run(value, move |shard| {
                let mut pair = connected(shard);
                let mut senders: Vec<_> = streams
                    .iter()
                    .map(|&(class, ref lengths)| {
                        let sender = open_sender(&mut pair, class);
                        let messages = lengths.iter().enumerate();
                        let messages =
                            messages.map(|(index, &len)| (class, index, len));
                        (sender, messages.collect::<VecDeque<_>>(), false, false)
                    })
                    .collect();
                let mut read = Vec::new();
                let mut drops = drops.into_iter();
                for _ in 0..ROUNDS {
                    let now = pair.now();
                    for (sender, messages, held, finished) in &mut senders {
                        let client = &mut pair.client.endpoint;
                        if *held {
                            let flushed = pair::write(client, now, sender, &mut None)
                                .expect("flushed");
                            *held = flushed.is_pending();
                        }
                        while !*held
                            && let Some((class, index, len)) = messages.pop_front()
                        {
                            let message = shard.block(&message(class, index, len));
                            let written =
                                pair::write(client, now, sender, &mut Some(message));
                            *held = written.expect("written").is_pending();
                        }
                        if !*held && !*finished {
                            client.finish(now, sender).expect("finished");
                            *finished = true;
                        }
                    }
                    pair.client.drops = drops.next().unwrap_or(0);
                    pair.run(DELAY);
                    let (now, key) = (pair.now(), key(&pair.server));
                    let accepted = iter::from_fn(|| pair.server.endpoint.accept(key));
                    read.extend(accepted.map(|incoming| {
                        (incoming.class, incoming.receiver, Vec::new(), false)
                    }));
                    for (_, receiver, messages, ended) in &mut read {
                        let (more, end) = drain(&mut pair.server, now, receiver);
                        messages.extend(more);
                        *ended |= end;
                    }
                    let done = |(.., ended): &(_, _, _, bool)| *ended;
                    if read.len() == streams.len() && read.iter().all(done) {
                        break;
                    }
                }
                let read = read.into_iter();
                read.map(|(class, _, messages, ended)| (class, messages, ended))
                    .collect()
            })
        }

        /// The most rounds of [`DELAY`] that [`carry`] runs.
        const ROUNDS: usize = 2000;

        /// The length of a message: often short, sometimes the largest.
        fn length() -> impl Strategy<Value = usize> {
            prop_oneof![
                2 => 0..=64_usize,
                1 => 0..=MESSAGE_MAX,
                1 => Just(MESSAGE_MAX),
            ]
        }

        /// One to three streams, each of a different class, with one to eight
        /// messages each.
        fn streams() -> impl Strategy<Value = Vec<(Class, Vec<usize>)>> {
            let classes = [
                Class::Command,
                Class::Latest,
                Class::Complete,
                Class::CatchUp,
            ];
            let classes = prop::sample::subsequence(classes.to_vec(), 1..=3);
            classes.prop_flat_map(|classes| {
                let lengths = prop::collection::vec(length(), 1..=8);
                let lengths = prop::collection::vec(lengths, classes.len());
                lengths.prop_map(move |lengths| {
                    classes.iter().copied().zip(lengths).collect()
                })
            })
        }

        /// The lengths of each stream's messages and whether it ended, which stay
        /// short in a failed assert.
        fn lengths(
            streams: &[(Class, Vec<Vec<u8>>, bool)],
        ) -> Vec<(Class, Vec<usize>, bool)> {
            let lengths = |(class, messages, ended): &(Class, Vec<Vec<u8>>, bool)| {
                (*class, messages.iter().map(Vec::len).collect(), *ended)
            };
            streams.iter().map(lengths).collect()
        }

        /// How many of the client's next batches the link drops in each round:
        /// often none.
        fn drops() -> impl Strategy<Value = Vec<usize>> {
            let count = prop_oneof![3 => Just(0_usize), 1 => 1..=3_usize];
            prop::collection::vec(count, 0..=32)
        }

        /// What the client sends on `stream`: its class, its messages, and the end.
        fn sent((class, lengths): &(Class, Vec<usize>)) -> (Class, Vec<Vec<u8>>, bool) {
            let messages = lengths.iter().enumerate();
            let messages = messages.map(|(index, &len)| message(*class, index, len));
            (*class, messages.collect(), true)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(32))]

            #[test]
            fn every_message_arrives_whole_and_in_order_then_the_end(
                value: u64,
                streams in streams(),
                drops in drops(),
            ) {
                let mut read = carry(value, streams.clone(), drops);
                read.sort_by_key(|(class, ..)| byte(*class));
                let sent: Vec<_> = streams.iter().map(sent).collect();
                prop_assert!(
                    read == sent,
                    "read {:?}, sent {:?}",
                    lengths(&read),
                    lengths(&sent),
                );
            }
        }
    }

    mod hello {
        use std::num::NonZeroU32;
        use std::sync::Arc;

        use noq_proto::TransportConfig;
        use rustls::crypto::CryptoProvider;
        use rustls::crypto::aws_lc_rs::{default_provider, kx_group};

        use super::*;
        use crate::quic::packet::Log;
        use crate::quic::pair::Foreign;

        /// The limits of a node with the [`Shard::config`] limits.
        const OWN: Hello = Hello {
            window_bytes: 1 << 20,
            message_bytes_max: MESSAGE_MAX,
        };

        /// A pair whose server a foreign peer with `change` made dialed, after a run.
        fn foreign_dial(
            shard: &Shard,
            change: impl FnOnce(&mut TransportConfig),
        ) -> Pair {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            let mut foreign = Foreign::new(shard, change);
            let peer = pair::SERVER_KEY.public();
            foreign.dial(pair.now(), peer, pair::SERVER);
            pair.foreign = Some(foreign);
            pair.run(RUN);
            pair
        }

        /// A pair whose client dialed `foreign`, after a run.
        fn dial_foreign(shard: &Shard, foreign: Foreign) -> Pair {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            pair.foreign = Some(foreign);
            let (now, peer) = (pair.now(), pair::FOREIGN_KEY.public());
            let key = pair.client.endpoint.connect(now, peer, pair::FOREIGN);
            pair.client.key = Some(key);
            pair.run(RUN);
            pair
        }

        fn foreign(pair: &mut Pair) -> &mut noq_proto::Connection {
            pair.foreign.as_mut().expect("a foreign peer").connection()
        }

        /// A pair whose server a foreign peer dialed with a [`Log::client`], after a
        /// run, and its log.
        fn logged_dial(shard: &Shard) -> (Pair, Arc<Log>) {
            let log = Arc::new(Log::default());
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            let mut foreign = Foreign::new(shard, |_| {});
            let tls = log.client(pair::SERVER_KEY.public());
            foreign.dial_with(pair.now(), tls, pair::SERVER);
            pair.foreign = Some(foreign);
            pair.run(RUN);
            (pair, log)
        }

        /// The code of each reset of stream `id` that the server sent to the
        /// foreign peer of [`logged_dial`], read from the wire.
        fn reset_codes(pair: &Pair, log: &Log, id: StreamId) -> Vec<u64> {
            let sent = pair.server.sent.iter();
            let datagrams = sent
                .filter(|(_, to, _)| *to == pair::FOREIGN)
                .map(|(_, _, datagram)| &datagram[..]);
            let stream = VarInt::from(id).into_inner();
            let resets = log.resets(datagrams).into_iter();
            resets
                .filter(|reset| reset.stream == stream)
                .map(|reset| reset.code)
                .collect()
        }

        /// The bytes of the next one-way stream the node opened on the foreign peer,
        /// to its end.
        fn read(pair: &mut Pair) -> Vec<u8> {
            let connection = foreign(pair);
            let id = connection.streams().accept(Dir::Uni).expect("a stream");
            let mut recv = connection.recv_stream(id);
            let mut chunks = recv.read(true).expect("open");
            let mut bytes = Vec::new();
            while let Some(chunk) = chunks.next(usize::MAX).expect("read") {
                bytes.extend(chunk.bytes);
            }
            bytes
        }

        /// Whether `side` got [`Event::Available`].
        fn available(side: &Side) -> bool {
            events(side).contains(&&Event::Available { key: key(side) })
        }

        /// A hello of [`BYTES_MAX`](crate::quic::hello::BYTES_MAX) bytes: both limits
        /// in 8-byte varints, then unknown pairs in 4-byte varints.
        fn full() -> Vec<u8> {
            let long = |value: usize| {
                (u64::try_from(value).expect("fits") | 0xc0 << 56).to_be_bytes()
            };
            let (window, message) = (OWN.window_bytes, OWN.message_bytes_max);
            let mut bytes = [long(0), long(window), long(1), long(message)].concat();
            for id in 2..=29_u32 {
                bytes.extend((id | 0x8000_0000).to_be_bytes());
                bytes.extend(0x8000_0000_u32.to_be_bytes());
            }
            assert_eq!(bytes.len(), crate::quic::hello::BYTES_MAX);
            bytes
        }

        /// Asserts that the server closed the connection for `reason` and that the
        /// foreign peer got the reason, after a run.
        fn assert_refused(pair: &mut Pair, reason: &str) {
            pair.run(RUN);
            let closed = Event::Closed {
                key: key(&pair.server),
                error: Error::Broken {
                    reason: reason.into(),
                },
            };
            assert_eq!(events(&pair.server).last(), Some(&&closed));
            assert_eq!(
                lost(pair),
                format!("closed by peer: {reason} (code 4294967296)")
            );
        }

        /// Why the foreign peer's connection ended.
        fn lost(pair: &Pair) -> String {
            let foreign = pair.foreign.as_ref().expect("a foreign peer");
            let reason = foreign.events.iter().find_map(|event| match event {
                noq_proto::Event::ConnectionLost { reason } => Some(reason.to_string()),
                _ => None,
            });
            reason.expect("the foreign peer's connection ended")
        }

        #[test]
        fn come_with_the_connection() {
            testing::run(1, |shard| {
                let pair = connected(shard);
                for side in [&pair.client, &pair.server] {
                    assert!(
                        matches!(
                            side.events.as_slice(),
                            [
                                (at, Event::Connected { key, .. }),
                                (then, Event::Available { key: available }),
                            ] if at == then && key == available
                        ),
                        "{:?}",
                        side.events
                    );
                }
            });
        }

        #[test]
        fn carry_the_limits_on_the_first_one_way_stream() {
            testing::run(1, |shard| {
                let own = [0x00, 0x80, 0x10, 0x00, 0x00, 0x01, 0x80, 0x01, 0x00, 0x00];
                assert_eq!(OWN.encode(), own);
                let mut pair = foreign_dial(shard, |_| {});
                assert_eq!(read(&mut pair), own);
                let mut pair = dial_foreign(shard, Foreign::new(shard, |_| {}));
                assert_eq!(read(&mut pair), own);
            });
        }

        #[test]
        fn hold_back_each_stream_until_the_peer_hello() {
            testing::run(1, |shard| {
                let mut pair = dial_foreign(shard, Foreign::new(shard, |_| {}));
                let (now, key) = (pair.now(), key(&pair.client));
                assert!(matches!(
                    events(&pair.client)[..],
                    [Event::Connected { .. }]
                ));
                let opened = pair.client.endpoint.open(now, key, Class::Command);
                assert!(opened.is_none(), "{opened:?}");
                let opened = pair.client.endpoint.open_sender(now, key, Class::Command);
                assert!(opened.is_none(), "{opened:?}");
                raw(foreign(&mut pair), Dir::Uni, &OWN.encode(), true);
                pair.run(RUN);
                assert_eq!(
                    events(&pair.client).last(),
                    Some(&&Event::Available { key })
                );
                let now = pair.now();
                assert!(
                    pair.client
                        .endpoint
                        .open(now, key, Class::Command)
                        .is_some()
                );
                let opened = pair.client.endpoint.open_sender(now, key, Class::Command);
                assert!(opened.is_some());
            });
        }

        /// On `Endpoint` events, not through `Session`: no public peer can skip its
        /// hello, and a `sim` link that loses a hello loses other datagrams too, so
        /// only a foreign peer pins the time and the close of the bound.
        #[test]
        fn end_a_session_whose_peer_sends_no_hello_for_twice_idle() {
            testing::run(1, |shard| {
                let mut accepted = foreign_dial(shard, |_| {});
                accepted.run(Duration::from_secs(3));
                let mut dialed = dial_foreign(shard, Foreign::new(shard, |_| {}));
                dialed.run(Duration::from_secs(3));
                for side in [&accepted.server, &dialed.client] {
                    let [
                        (connected, Event::Connected { key, .. }),
                        (closed, Event::Closed { key: ended, error }),
                    ] = &side.events[..]
                    else {
                        panic!("{:?}", side.events);
                    };
                    assert_eq!(*closed, *connected + Duration::from_secs(2));
                    assert_eq!(ended, key);
                    let reason = "a peer with no hello".to_owned();
                    assert_eq!(error, &Error::Broken { reason });
                }
                for pair in [&accepted, &dialed] {
                    let reason =
                        "closed by peer: a peer with no hello (code 4294967296)";
                    assert_eq!(lost(pair), reason);
                }
            });
        }

        /// On `Endpoint` events, not through `Session`: no public peer can skip its
        /// hello.
        #[test]
        fn end_a_silent_session_with_no_hello_at_idle_as_timed_out() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                let mut foreign = Foreign::new(shard, |_| {});
                foreign.dial(pair.now(), pair::SERVER_KEY.public(), pair::SERVER);
                pair.foreign = Some(foreign);
                while pair.server.events.is_empty() {
                    pair.run(Duration::from_millis(1));
                }
                pair.foreign = None;
                pair.run(Duration::from_secs(3));
                let [
                    (connected, Event::Connected { .. }),
                    (closed, Event::Closed { error, .. }),
                ] = &pair.server.events[..]
                else {
                    panic!("{:?}", pair.server.events);
                };
                assert_eq!(*closed, *connected + Duration::from_secs(1));
                assert_eq!(error, &Error::TimedOut);
            });
        }

        /// A peer that sends no hello goes silent, and the one wake after it comes past
        /// both the idle timeout and the hello's bound: at the handshake, when the idle
        /// timeout is due first, and 1.5 s after it, when the bound is. On `Endpoint`,
        /// not through `Session`: no public peer can skip its hello, and the test picks
        /// the instant of the wake.
        #[test]
        fn end_a_silent_session_at_a_late_wake_as_timed_out() {
            for (quiet, wake) in [(0, 2_100), (1_500, 2_600)] {
                testing::run(1, move |shard| {
                    let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                    let mut foreign = Foreign::new(shard, |_| {});
                    foreign.dial(pair.now(), pair::SERVER_KEY.public(), pair::SERVER);
                    pair.foreign = Some(foreign);
                    while pair.server.events.is_empty() {
                        pair.run(Duration::from_millis(1));
                    }
                    pair.run(Duration::from_millis(quiet));
                    pair.foreign = None;
                    let (connected, _) = pair.server.events[0];
                    let wake = pair::at(connected + Duration::from_millis(wake));
                    pair.server.endpoint.timeout(wake);
                    let mut events = Vec::new();
                    while let Some(event) = pair.server.endpoint.poll() {
                        events.push(event);
                    }
                    let [Event::Closed { error, .. }] = &events[..] else {
                        panic!("{events:?}");
                    };
                    assert_eq!(error, &Error::TimedOut);
                    let deadline = pair.server.endpoint.deadline();
                    assert!(deadline.is_none_or(|due| due > wake), "{deadline:?}");
                });
            }
        }

        /// A peer that sends no hello goes silent 810 ms after the handshake. The
        /// server's next keep-alive starts the idle timeout again, so at the hello's
        /// bound the peer was silent more than `idle`, and the session still ends with
        /// [`Error::Broken`]. On `Endpoint`, not through `Session`: no public peer can
        /// skip its hello.
        #[test]
        fn end_a_session_silent_for_idle_at_the_bound_as_broken() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                let mut foreign = Foreign::new(shard, |_| {});
                foreign.dial(pair.now(), pair::SERVER_KEY.public(), pair::SERVER);
                pair.foreign = Some(foreign);
                while pair.server.events.is_empty() {
                    pair.run(Duration::from_millis(1));
                }
                pair.run(Duration::from_millis(810));
                let idle = pair.server.connection().idle_timeout().expect("an idle");
                let cut = Duration::from_nanos(pair.now().0);
                pair.foreign = None;
                pair.run(Duration::from_secs(3));
                let [
                    (connected, Event::Connected { .. }),
                    (closed, Event::Closed { error, .. }),
                ] = &pair.server.events[..]
                else {
                    panic!("{:?}", pair.server.events);
                };
                assert_eq!(*closed, *connected + 2 * idle);
                // The peer's last datagram arrives by `cut + DELAY`.
                let silent = closed.checked_sub(cut + DELAY).expect("a close after");
                assert!(silent > idle, "{silent:?}");
                let reason = "a peer with no hello".to_owned();
                assert_eq!(error, &Error::Broken { reason });
            });
        }

        /// A connection whose socket broke, with no hello from a live peer, gives no
        /// second [`Event::Closed`] and no fault when the hello's bound passes. On
        /// `Endpoint` events, not through `Session`: the carrier stops at a broken
        /// socket, so only a direct caller of `Endpoint` runs a timer after `fail`.
        #[test]
        fn end_a_failed_session_once_when_the_bound_passes() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let error = env::net::Error::Io { code: 5 };
                pair.server.endpoint.fail(&error);
                pair.run(Duration::from_secs(3));
                let [
                    (_, Event::Connected { key, .. }),
                    (
                        _,
                        Event::Closed {
                            key: ended,
                            error: closed,
                        },
                    ),
                ] = &pair.server.events[..]
                else {
                    panic!("{:?}", pair.server.events);
                };
                assert_eq!(ended, key);
                assert_eq!(closed, &Error::Network { error });
            });
        }

        /// On `Endpoint` events, not through `Session`: no public peer can send its
        /// hello at a chosen instant.
        #[test]
        fn keep_a_session_whose_peer_hello_arrives_before_twice_idle() {
            testing::run(1, |shard| {
                let mut pair = dial_foreign(shard, Foreign::new(shard, |_| {}));
                let (connected, _) = pair.client.events[0];
                let now = Duration::from_nanos(pair.now().0);
                // One step before the bound.
                let arrival = connected + Duration::from_nanos(1_999_999_999);
                pair.run(arrival.checked_sub(now + DELAY).expect("a send after now"));
                raw(foreign(&mut pair), Dir::Uni, &OWN.encode(), true);
                pair.run(Duration::from_secs(3));
                let key = key(&pair.client);
                assert!(
                    matches!(
                        events(&pair.client)[..],
                        [Event::Connected { .. }, Event::Available { key: available }]
                            if *available == key
                    ),
                    "{:?}",
                    pair.client.events
                );
                assert_eq!(pair.client.events[1].0, arrival);
            });
        }

        /// The carrier sets its sleep only when the deadline changes, so one timeout
        /// at a late wake must leave no deadline at or before it. On `Endpoint`: the
        /// deadline is the carrier's input, and no `Session` call shows it.
        #[test]
        fn leave_no_past_deadline_after_a_late_wake() {
            for late in [100, 300, 600, 900].map(Duration::from_millis) {
                testing::run(1, move |shard| {
                    let mut pair = dial_foreign(shard, Foreign::new(shard, |_| {}));
                    let (connected, _) = pair.client.events[0];
                    let now = Duration::from_nanos(pair.now().0);
                    let before = connected + Duration::from_millis(1_950);
                    pair.run(before.checked_sub(now).expect("a run after now"));
                    let wake = pair::at(connected + Duration::from_secs(2) + late);
                    pair.client.endpoint.timeout(wake);
                    let deadline = pair.client.endpoint.deadline();
                    assert!(
                        deadline.is_none_or(|due| due > wake),
                        "{late:?}: {deadline:?}"
                    );
                });
            }
        }

        #[test]
        fn leave_ahead_of_every_stream() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                pair.dial(pair::SERVER_KEY.public());
                let now = pair.now();
                // The handshake by hand, so the client writes before it sends its
                // hello.
                let mut command = loop {
                    while step(&mut pair.client, &mut pair.server, now) {}
                    while step(&mut pair.server, &mut pair.client, now) {}
                    let events: Vec<_> =
                        iter::from_fn(|| pair.client.endpoint.poll()).collect();
                    if !events.is_empty() {
                        assert!(
                            matches!(
                                events[..],
                                [Event::Connected { .. }, Event::Available { .. }]
                            ),
                            "{events:?}"
                        );
                        break open_sender(&mut pair, Class::Command);
                    }
                };
                let bulk = shard.block(&[0; BULK]);
                write(&mut pair.client, now, &mut command, &[bulk]);
                assert!(step(&mut pair.client, &mut pair.server, now));
                let events: Vec<_> =
                    iter::from_fn(|| pair.server.endpoint.poll()).collect();
                assert!(
                    matches!(
                        events[..],
                        [
                            Event::Connected { .. },
                            Event::Available { .. },
                            Event::Incoming { .. }
                        ]
                    ),
                    "{events:?}"
                );
            });
        }

        #[test]
        fn hold_back_the_peer_streams_until_the_peer_hello() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let connection = foreign(&mut pair);
                let hello = connection.streams().open(Dir::Uni).expect("a stream");
                raw(connection, Dir::Uni, &[0, 1, b'u'], true);
                raw(connection, Dir::Bi, &[1, 1, b'b'], true);
                pair.run(RUN);
                assert!(matches!(
                    events(&pair.server)[..],
                    [Event::Connected { .. }]
                ));
                let server = key(&pair.server);
                assert!(pair.server.endpoint.accept(server).is_none());
                let mut send = foreign(&mut pair).send_stream(hello);
                let own = OWN.encode();
                assert_eq!(send.write(&own), Ok(own.len()));
                send.finish().expect("finished");
                pair.run(RUN);
                let expected = [
                    &Event::Available { key: server },
                    &Event::Incoming { key: server },
                ];
                assert_eq!(events(&pair.server)[1..], expected);
                let now = pair.now();
                for (class, message) in [(Class::Command, b"u"), (Class::Latest, b"b")]
                {
                    let mut incoming = accept(&mut pair.server);
                    assert_eq!(incoming.class, class);
                    let read = drain(&mut pair.server, now, &mut incoming.receiver);
                    assert_eq!(read, (vec![message.to_vec()], true));
                }
            });
        }

        #[test]
        fn keep_a_stop_that_comes_before_them() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let connection = foreign(&mut pair);
                let hello = connection.streams().open(Dir::Uni).expect("a stream");
                let id = raw(connection, Dir::Bi, &[1, 1, b'b'], true);
                let stopped = connection.recv_stream(id).stop(VarInt::from_u32(9));
                stopped.expect("stopped");
                pair.run(RUN);
                assert!(matches!(
                    events(&pair.server)[..],
                    [Event::Connected { .. }]
                ));
                let mut send = foreign(&mut pair).send_stream(hello);
                let own = OWN.encode();
                assert_eq!(send.write(&own), Ok(own.len()));
                send.finish().expect("finished");
                pair.run(RUN);
                let incoming = accept(&mut pair.server);
                let reply = incoming.sender.expect("a two-way stream");
                let (now, message) = (pair.now(), shard.block(b"b"));
                let written = pair::write(
                    &mut pair.server.endpoint,
                    now,
                    &reply,
                    &mut Some(message),
                );
                assert_eq!(written, Err(Error::Stopped { code: Code(9) }));
            });
        }

        #[test]
        fn reset_at_the_hello_with_the_code_of_a_stop_before_it() {
            // The stream has no byte, waits for its first message byte, or queues.
            let streams: [(&[u8], bool); 3] =
                [(&[], false), (&[1], false), (&[1, 1, b'b'], true)];
            for (bytes, end) in streams {
                testing::run(1, move |shard| {
                    let (mut pair, log) = logged_dial(shard);
                    let connection = foreign(&mut pair);
                    let hello = connection.streams().open(Dir::Uni).expect("a stream");
                    let id = raw(connection, Dir::Bi, bytes, end);
                    let stopped = connection.recv_stream(id).stop(VarInt::from_u32(9));
                    stopped.expect("stopped");
                    pair.run(RUN);
                    assert!(reset_codes(&pair, &log, id).is_empty());
                    let mut send = foreign(&mut pair).send_stream(hello);
                    let own = OWN.encode();
                    assert_eq!(send.write(&own), Ok(own.len()));
                    send.finish().expect("finished");
                    pair.run(RUN);
                    assert_eq!(reset_codes(&pair, &log, id), [9]);
                });
            }
        }

        #[test]
        fn reset_an_accepted_stream_with_the_code_of_a_stop() {
            testing::run(1, |shard| {
                let (mut pair, log) = logged_dial(shard);
                let connection = foreign(&mut pair);
                raw(connection, Dir::Uni, &OWN.encode(), true);
                let id = raw(connection, Dir::Bi, &[1, 1, b'b'], true);
                pair.run(RUN);
                let incoming = accept(&mut pair.server);
                let stopped =
                    foreign(&mut pair).recv_stream(id).stop(VarInt::from_u32(9));
                stopped.expect("stopped");
                pair.run(RUN);
                assert_eq!(reset_codes(&pair, &log, id), [9]);
                let reply = incoming.sender.expect("a two-way stream");
                let (now, message) = (pair.now(), shard.block(b"b"));
                let written = pair::write(
                    &mut pair.server.endpoint,
                    now,
                    &reply,
                    &mut Some(message),
                );
                assert_eq!(written, Err(Error::Stopped { code: Code(9) }));
            });
        }

        #[test]
        fn reset_an_arriving_stream_with_the_code_of_a_stop() {
            // The stream has no byte, or waits for its first message byte.
            let streams: [&[u8]; 2] = [&[], &[1]];
            for bytes in streams {
                testing::run(1, move |shard| {
                    let (mut pair, log) = logged_dial(shard);
                    let connection = foreign(&mut pair);
                    raw(connection, Dir::Uni, &OWN.encode(), true);
                    // A frame of the later stream opens the earlier one with no byte.
                    let id = raw(connection, Dir::Bi, bytes, false);
                    raw(connection, Dir::Bi, &[1, 1, b'z'], true);
                    pair.run(RUN);
                    let stopped =
                        foreign(&mut pair).recv_stream(id).stop(VarInt::from_u32(9));
                    stopped.expect("stopped");
                    pair.run(RUN);
                    assert_eq!(reset_codes(&pair, &log, id), [9], "{bytes:?}");
                });
            }
        }

        /// A pair whose server a foreign peer dialed with a [`Log::client`], its log,
        /// and the server's hello stream, which the peer stopped with `code`. The
        /// stop arrived before the ACK of the hello, and the peer sent no hello.
        fn stopped_hello(shard: &Shard, code: VarInt) -> (Pair, Arc<Log>, StreamId) {
            let log = Arc::new(Log::default());
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            let mut peer = Foreign::new(shard, |_| {});
            let tls = log.client(pair::SERVER_KEY.public());
            peer.dial_with(pair.now(), tls, pair::SERVER);
            pair.foreign = Some(peer);
            let mut steps = 0;
            let id = loop {
                pair.run(Duration::from_micros(100));
                if let Some(id) = foreign(&mut pair).streams().accept(Dir::Uni) {
                    break id;
                }
                steps += 1;
                assert!(steps < 10_000, "no hello stream");
            };
            let stopped = foreign(&mut pair).recv_stream(id).stop(code);
            stopped.expect("stopped");
            // noq-proto frees the stream, and drops the stop, at the ACK.
            let mut state = Ok(None);
            for _ in 0..200 {
                pair.run(Duration::from_micros(100));
                if pair.server.key.is_some() {
                    state = pair.server.connection().send_stream(id).stopped();
                    if state != Ok(None) {
                        break;
                    }
                }
            }
            assert_eq!(state, Ok(Some(code)));
            (pair, log, id)
        }

        #[test]
        fn reset_its_own_hello_stream_at_a_stop_before_the_peer_hello() {
            testing::run(1, |shard| {
                let (mut pair, log, id) = stopped_hello(shard, VarInt::from_u32(9));
                pair.run(RUN);
                let freed = pair.server.connection().send_stream(id).stopped();
                assert!(matches!(freed, Err(ClosedStream { .. })), "{freed:?}");
                assert_eq!(reset_codes(&pair, &log, id), [9]);
                raw(foreign(&mut pair), Dir::Uni, &OWN.encode(), true);
                pair.run(RUN);
                assert!(available(&pair.server));
                assert_eq!(reset_codes(&pair, &log, id), [9]);
            });
        }

        #[test]
        fn break_at_a_stop_of_its_own_hello_stream_with_a_code_over_32_bits() {
            testing::run(1, |shard| {
                let over = VarInt::from_u64(1 << 32).expect("a varint");
                let (mut pair, _, _) = stopped_hello(shard, over);
                assert_refused(&mut pair, "a stop code over 32 bits: 4294967296");
            });
        }

        #[test]
        fn ignore_a_stop_with_a_code_over_32_bits_after_all_is_acknowledged() {
            testing::run(1, |shard| {
                let (mut pair, log) = logged_dial(shard);
                raw(foreign(&mut pair), Dir::Uni, &OWN.encode(), true);
                pair.run(RUN);
                let (now, key) = (pair.now(), key(&pair.server));
                let sender =
                    pair.server.endpoint.open_sender(now, key, Class::Complete);
                let mut sender = sender.expect("a stream");
                write(&mut pair.server, now, &mut sender, &[shard.block(b"a")]);
                assert_eq!(pair.server.endpoint.finish(now, &mut sender), Ok(()));
                let id = sender.key().id;
                pair.run(RUN);
                let freed = pair.server.connection().send_stream(id).stopped();
                assert!(matches!(freed, Err(ClosedStream { .. })), "{freed:?}");
                let over = VarInt::from_u64(1 << 32).expect("a varint");
                let stopped = foreign(&mut pair).recv_stream(id).stop(over);
                stopped.expect("stopped");
                pair.run(RUN);
                let stats = pair.server.connection().stats();
                assert_eq!(stats.frame_rx.stop_sending, 1);
                let now = pair.now();
                let next = pair.server.endpoint.open_sender(now, key, Class::Complete);
                assert!(next.is_some(), "the connection stays up");
                let closed = |event: &&Event| matches!(event, Event::Closed { .. });
                assert!(!events(&pair.server).iter().any(closed));
                assert!(reset_codes(&pair, &log, id).is_empty());
            });
        }

        #[test]
        fn reset_at_the_hello_a_reply_stopped_before_it_that_queues() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let connection = foreign(&mut pair);
                let hello = connection.streams().open(Dir::Uni).expect("a stream");
                let id = raw(connection, Dir::Bi, &[1, 1, b'b'], true);
                let stopped = connection.recv_stream(id).stop(VarInt::from_u32(9));
                stopped.expect("stopped");
                pair.run(RUN);
                let resets = |pair: &mut Pair| {
                    let stats = foreign(pair).stats();
                    stats.frame_rx.reset_stream
                };
                let before = resets(&mut pair);
                let mut send = foreign(&mut pair).send_stream(hello);
                let own = OWN.encode();
                assert_eq!(send.write(&own), Ok(own.len()));
                send.finish().expect("finished");
                pair.run(RUN);
                assert_eq!(resets(&mut pair) - before, 1, "a reset at the hello");
                let mut incoming = accept(&mut pair.server);
                let now = pair.now();
                let read = drain(&mut pair.server, now, &mut incoming.receiver);
                assert_eq!(read, (vec![b"b".to_vec()], true));
                pair.run(RUN);
                let streams = pair.server.connection().streams();
                assert_eq!(streams.remote_open_streams(Dir::Bi), 0);
            });
        }

        #[test]
        fn reset_at_the_hello_each_reply_stopped_before_it() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let connection = foreign(&mut pair);
                let hello = connection.streams().open(Dir::Uni).expect("a stream");
                for _ in 0..2 {
                    let id = raw(connection, Dir::Bi, &[1, 1, b'b'], true);
                    let stopped = connection.recv_stream(id).stop(VarInt::from_u32(9));
                    stopped.expect("stopped");
                }
                pair.run(RUN);
                let resets = |pair: &mut Pair| {
                    let stats = foreign(pair).stats();
                    stats.frame_rx.reset_stream
                };
                let before = resets(&mut pair);
                let mut send = foreign(&mut pair).send_stream(hello);
                let own = OWN.encode();
                assert_eq!(send.write(&own), Ok(own.len()));
                send.finish().expect("finished");
                pair.run(RUN);
                assert_eq!(
                    resets(&mut pair) - before,
                    2,
                    "a reset of each at the hello"
                );
            });
        }

        #[test]
        fn break_at_the_hello_on_a_stop_before_it_with_a_code_over_32_bits() {
            // The stream has no byte, waits for its first message byte, queues, or
            // drops at the hello.
            let streams: [(&[u8], bool); 5] = [
                (&[], false),
                (&[1], false),
                (&[1, 1, b'b'], true),
                (&[1], true),
                (&[], true),
            ];
            for (bytes, end) in streams {
                testing::run(1, move |shard| {
                    let mut pair = foreign_dial(shard, |_| {});
                    let connection = foreign(&mut pair);
                    let hello = connection.streams().open(Dir::Uni).expect("a stream");
                    let id = raw(connection, Dir::Bi, bytes, end);
                    let over = VarInt::from_u64(1 << 32).expect("a varint");
                    let stopped = connection.recv_stream(id).stop(over);
                    stopped.expect("stopped");
                    pair.run(RUN);
                    let mut send = foreign(&mut pair).send_stream(hello);
                    let own = OWN.encode();
                    assert_eq!(send.write(&own), Ok(own.len()));
                    send.finish().expect("finished");
                    assert_refused(&mut pair, "a stop code over 32 bits: 4294967296");
                });
            }
        }

        #[test]
        fn reset_at_the_hello_a_reply_stopped_before_it_with_no_byte() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let connection = foreign(&mut pair);
                let hello = connection.streams().open(Dir::Uni).expect("a stream");
                // Only the stop opens the stream on the server.
                let id = raw(connection, Dir::Bi, &[], false);
                let stopped = connection.recv_stream(id).stop(VarInt::from_u32(9));
                stopped.expect("stopped");
                pair.run(RUN);
                let resets = |pair: &mut Pair| {
                    let stats = foreign(pair).stats();
                    stats.frame_rx.reset_stream
                };
                let before = resets(&mut pair);
                let mut send = foreign(&mut pair).send_stream(hello);
                let own = OWN.encode();
                assert_eq!(send.write(&own), Ok(own.len()));
                send.finish().expect("finished");
                pair.run(RUN);
                assert_eq!(resets(&mut pair) - before, 1, "a reset at the hello");
                let mut send = foreign(&mut pair).send_stream(id);
                assert_eq!(send.write(&[1, 1, b'b']), Ok(3));
                send.finish().expect("finished");
                pair.run(RUN);
                let incoming = accept(&mut pair.server);
                let reply = incoming.sender.expect("a two-way stream");
                let (now, message) = (pair.now(), shard.block(b"b"));
                let written = pair::write(
                    &mut pair.server.endpoint,
                    now,
                    &reply,
                    &mut Some(message),
                );
                assert_eq!(written, Err(Error::Stopped { code: Code(9) }));
            });
        }

        #[test]
        fn reset_at_the_hello_a_reply_stopped_before_it() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let connection = foreign(&mut pair);
                let hello = connection.streams().open(Dir::Uni).expect("a stream");
                let id = raw(connection, Dir::Bi, &[1], false);
                let stopped = connection.recv_stream(id).stop(VarInt::from_u32(9));
                stopped.expect("stopped");
                pair.run(RUN);
                let resets = |pair: &mut Pair| {
                    let stats = foreign(pair).stats();
                    stats.frame_rx.reset_stream
                };
                let before = resets(&mut pair);
                let mut send = foreign(&mut pair).send_stream(hello);
                let own = OWN.encode();
                assert_eq!(send.write(&own), Ok(own.len()));
                send.finish().expect("finished");
                pair.run(RUN);
                assert_eq!(resets(&mut pair) - before, 1, "a reset at the hello");
                let mut send = foreign(&mut pair).send_stream(id);
                assert_eq!(send.write(&[1, b'b']), Ok(2));
                send.finish().expect("finished");
                pair.run(RUN);
                let mut incoming = accept(&mut pair.server);
                let now = pair.now();
                let read = drain(&mut pair.server, now, &mut incoming.receiver);
                assert_eq!(read, (vec![b"b".to_vec()], true));
                pair.run(RUN);
                let streams = pair.server.connection().streams();
                assert_eq!(streams.remote_open_streams(Dir::Bi), 0);
            });
        }

        #[test]
        fn reset_at_the_hello_each_waiting_reply_stopped_before_it() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let connection = foreign(&mut pair);
                let hello = connection.streams().open(Dir::Uni).expect("a stream");
                let mut ids = Vec::new();
                for _ in 0..2 {
                    let id = raw(connection, Dir::Bi, &[1], false);
                    let stopped = connection.recv_stream(id).stop(VarInt::from_u32(9));
                    stopped.expect("stopped");
                    ids.push(id);
                }
                pair.run(RUN);
                let resets = |pair: &mut Pair| {
                    let stats = foreign(pair).stats();
                    stats.frame_rx.reset_stream
                };
                let before = resets(&mut pair);
                let mut send = foreign(&mut pair).send_stream(hello);
                let own = OWN.encode();
                assert_eq!(send.write(&own), Ok(own.len()));
                send.finish().expect("finished");
                pair.run(RUN);
                assert_eq!(
                    resets(&mut pair) - before,
                    2,
                    "a reset of each at the hello"
                );
                for &id in &ids {
                    let mut send = foreign(&mut pair).send_stream(id);
                    assert_eq!(send.write(&[1, b'b']), Ok(2));
                    send.finish().expect("finished");
                }
                pair.run(RUN);
                for _ in &ids {
                    let incoming = accept(&mut pair.server);
                    let reply = incoming.sender.expect("a two-way stream");
                    let (now, message) = (pair.now(), shard.block(b"b"));
                    let written = pair::write(
                        &mut pair.server.endpoint,
                        now,
                        &reply,
                        &mut Some(message),
                    );
                    assert_eq!(written, Err(Error::Stopped { code: Code(9) }));
                }
            });
        }

        #[test]
        fn leave_the_peer_streams_max_one_way_streams() {
            testing::run(1, |shard| {
                let one = |config: &mut Config| config.streams_max = NonZeroU32::MIN;
                let mut pair = Pair::with(shard, Span::SECOND, DELAY, one);
                pair.dial(pair::SERVER_KEY.public());
                for _ in 0..100 {
                    if available(&pair.client) {
                        break;
                    }
                    pair.run(STEP);
                }
                let (now, key) = (pair.now(), key(&pair.client));
                let opened = pair.client.endpoint.open_sender(now, key, Class::Command);
                assert!(opened.is_some());
                pair.run(RUN);
                let now = pair.now();
                let opened = pair.client.endpoint.open_sender(now, key, Class::Command);
                assert!(opened.is_none(), "{opened:?}");
            });
        }

        /// A client whose first key share is `MLKEM768` alone. A node lacks that
        /// group, so it asks for `X25519` in a `HelloRetryRequest`.
        fn retrying() -> Arc<rustls::ClientConfig> {
            let provider = CryptoProvider {
                kx_groups: vec![kx_group::MLKEM768, kx_group::X25519],
                ..default_provider()
            };
            tls::anonymous(provider, pair::SERVER_KEY.public())
        }

        #[test]
        fn leave_after_a_hello_retry_request() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                let mut foreign = Foreign::new(shard, |_| {});
                foreign.dial_with(pair.now(), retrying(), pair::SERVER);
                pair.foreign = Some(foreign);
                pair.run(RUN);
                assert!(
                    matches!(events(&pair.server)[..], [Event::Connected { .. }]),
                    "{:?}",
                    pair.server.events
                );
                assert_eq!(read(&mut pair), OWN.encode());
            });
        }

        #[test]
        fn arrive_only_at_the_end_of_their_stream() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let own = OWN.encode();
                let id = raw(foreign(&mut pair), Dir::Uni, &own[..5], false);
                pair.run(RUN);
                assert!(!available(&pair.server));
                let mut send = foreign(&mut pair).send_stream(id);
                assert_eq!(send.write(&own[5..]), Ok(5));
                pair.run(RUN);
                assert!(!available(&pair.server));
                foreign(&mut pair)
                    .send_stream(id)
                    .finish()
                    .expect("finished");
                pair.run(RUN);
                assert!(available(&pair.server));
            });
        }

        #[test]
        fn that_reset_break_the_connection() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let id = raw(foreign(&mut pair), Dir::Uni, &[0], false);
                let reset = foreign(&mut pair)
                    .send_stream(id)
                    .reset(VarInt::from_u32(0));
                reset.expect("reset");
                assert_refused(&mut pair, "a hello that reset");
            });
        }

        #[test]
        fn over_the_limit_break_the_connection_before_their_end() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                raw(foreign(&mut pair), Dir::Uni, &[0; 257], false);
                assert_refused(&mut pair, "a hello over 256 bytes");
            });
        }

        #[test]
        fn of_bytes_max_arrive_at_their_end() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let id = raw(foreign(&mut pair), Dir::Uni, &full(), false);
                pair.run(RUN);
                assert!(!available(&pair.server), "{:?}", pair.server.events);
                let finished = foreign(&mut pair).send_stream(id).finish();
                finished.expect("finished");
                pair.run(RUN);
                assert!(available(&pair.server), "{:?}", pair.server.events);
            });
        }

        #[test]
        fn of_bytes_max_break_the_connection_at_one_more_byte() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                let id = raw(foreign(&mut pair), Dir::Uni, &full(), false);
                pair.run(RUN);
                assert!(!available(&pair.server), "{:?}", pair.server.events);
                let written = foreign(&mut pair).send_stream(id).write(&[0]);
                assert_eq!(written, Ok(1));
                assert_refused(&mut pair, "a hello over 256 bytes");
            });
        }

        #[test]
        fn that_do_not_decode_break_the_connection() {
            testing::run(1, |shard| {
                let mut pair = foreign_dial(shard, |_| {});
                raw(foreign(&mut pair), Dir::Uni, &[0x00, 0x47, 0xd0], true);
                assert_refused(&mut pair, "a hello with no message_bytes_max");
            });
        }

        /// Transport parameters that leave no room for a hello.
        fn hostile() -> [fn(&mut TransportConfig); 2] {
            [
                |config| {
                    config.max_concurrent_uni_streams(VarInt::from_u32(0));
                },
                |config| {
                    config.stream_receive_window(VarInt::from_u32(5));
                },
            ]
        }

        /// What a foreign peer sees of a close before the handshake is confirmed: QUIC
        /// carries no reason then.
        const HANDSHAKE: &str = "aborted by peer: the application or application \
                                 protocol caused the connection to be closed during \
                                 the handshake";

        #[test]
        fn with_no_room_at_the_peer_break_a_dial() {
            testing::run(1, |shard| {
                for change in hostile() {
                    let pair = dial_foreign(shard, Foreign::new(shard, change));
                    let reason = "a peer with no room for the hello";
                    let closed = Event::Closed {
                        key: key(&pair.client),
                        error: Error::Broken {
                            reason: reason.into(),
                        },
                    };
                    assert_eq!(events(&pair.client), [&closed]);
                    assert_eq!(lost(&pair), HANDSHAKE);
                }
            });
        }

        #[test]
        fn with_no_room_at_the_peer_refuse_an_accept() {
            // With no one-way stream, the acceptor waits for `Connected` in case a
            // HelloRetryRequest held back the limit, so its close has a reason.
            let closed = "closed by peer: a peer with no room for the hello (code \
                          4294967296)";
            testing::run(1, move |shard| {
                for (change, seen) in hostile().into_iter().zip([closed, HANDSHAKE]) {
                    let pair = foreign_dial(shard, change);
                    assert!(pair.server.events.is_empty(), "{:?}", pair.server.events);
                    assert_eq!(lost(&pair), seen);
                }
            });
        }

        #[test]
        fn with_no_room_at_the_peer_drop_the_datagrams_of_a_dial() {
            testing::run(1, |shard| {
                let mut foreign = Foreign::new(shard, |config| {
                    config.max_concurrent_uni_streams(VarInt::from_u32(0));
                });
                foreign.datagram = Some(Bytes::from_static(b"x"));
                let pair = dial_foreign(shard, foreign);
                let closed = Event::Closed {
                    key: key(&pair.client),
                    error: Error::Broken {
                        reason: "a peer with no room for the hello".into(),
                    },
                };
                assert_eq!(events(&pair.client), [&closed]);
            });
        }
    }
}
