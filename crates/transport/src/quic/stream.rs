//! The streams of a connection: whole messages in order over noq-proto's streams. The
//! side that opens a stream starts it with its class byte.

use std::collections::VecDeque;
use std::ops::Range;
use std::task::Poll;
use std::{mem, slice, vec};

use block::{Block, Pool};
use bytes::Bytes;
use noq_proto::{
    ClosedStream, Dir, FinishError, ReadError, StreamEvent, StreamId, VarInt,
    WriteError,
};

use super::{Event, connection};
use crate::message::{Prefix, Reader};
use crate::{Class, Code, Error};

/// Names one stream of a connection of an [`Endpoint`](super::Endpoint).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub(super) connection: connection::Key,
    pub(super) id: StreamId,
}

/// The sending half of a stream. The caller gives it to each write call, and to
/// [`Endpoint::reset`](super::Endpoint::reset) to end it. Dropping it does nothing to
/// the stream, and the message in hand keeps its send budget until the connection ends.
#[derive(Debug)]
pub(crate) struct Sender {
    key: Key,
    /// The stream needs no class byte: it has one, or it is a reply.
    started: bool,
    /// The class byte, then the length prefix of the message in hand.
    header: [u8; 9],
    /// The bytes of `header` that the stream has not taken.
    unsent: Range<usize>,
    /// The bytes of the message in hand that the stream has not taken.
    body: Bytes,
    /// The send budget of the message in hand.
    claim: Claim,
    finished: bool,
}

/// The receiving half of a stream. The caller gives it to each read, and to
/// [`Endpoint::stop`](super::Endpoint::stop) to end it. Dropping it does nothing to the
/// stream, and the message in the reader keeps its receive budget until the connection
/// ends.
#[derive(Debug)]
pub(crate) struct Receiver {
    key: Key,
    reader: Reader,
    /// The receive budget of the message in the reader.
    claim: Claim,
    end: Option<End>,
}

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
    /// Streams the peer opened whose class byte has not arrived.
    unclassified: Vec<StreamId>,
    /// Streams the peer opened that the caller has not accepted, by class byte,
    /// oldest first.
    incoming: [VecDeque<StreamId>; 4],
    /// Each stream that this side sends on and has not finished, with the code the
    /// peer stopped it with.
    senders: Vec<(StreamId, Option<Code>)>,
    /// The messages this side has started and the streams have not taken in full.
    /// They stay within the peer's window, so the peer's receive budget always has
    /// room for one more message.
    sending: Budget,
    /// The messages that hold a block and have not gone to the caller.
    receiving: Budget,
}

/// The message bytes that one direction of a connection counts, and the streams that
/// found no room.
#[derive(Debug)]
struct Budget {
    max: usize,
    used: usize,
    /// One more than the wakes so far.
    round: u64,
    /// The streams whose claims wait in this `round`.
    waiting: Vec<Key>,
    /// At most the fewest bytes that a waiting claim asks for, or `usize::MAX`. The
    /// room stays below it between wakes.
    smallest: usize,
}

/// The part of a [`Budget`] that the message of one stream holds or waits for.
#[derive(Debug, Default)]
struct Claim {
    /// The bytes the message counts, or 0.
    bytes: usize,
    /// The budget's round when the stream began to wait, or 0.
    round: u64,
}

/// A fault of the peer's that closes the connection, with the reason.
#[derive(Debug)]
pub(super) struct Fault(pub(super) String);

/// A message body that the stream holds until the peer acknowledges it.
struct Body(Block);

impl AsRef<[u8]> for Body {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Sender {
    /// A sender for `key` that starts the stream with `class`'s byte.
    pub(super) fn new(key: Key, class: Class) -> Self {
        let mut header = [0; 9];
        header[0] = byte(class);
        Self {
            started: false,
            header,
            ..Self::reply(key)
        }
    }

    /// A sender for the reply half of `key`, a stream the peer opened.
    fn reply(key: Key) -> Self {
        Self {
            key,
            started: true,
            header: [0; 9],
            unsent: 0..0,
            body: Bytes::new(),
            claim: Claim::default(),
            finished: false,
        }
    }

    /// The stream this sender writes.
    pub(crate) fn key(&self) -> Key {
        self.key
    }

    /// Takes `message` as the message in hand, after its header.
    ///
    /// # Panics
    ///
    /// When the sender holds part of a message, or after `end`.
    pub(super) fn load(&mut self, message: Block) {
        self.check();
        let prefix = Prefix::new(message.len());
        let start = usize::from(self.started);
        self.started = true;
        self.header[1..=prefix.len()].copy_from_slice(&prefix);
        self.unsent = start..prefix.len() + 1;
        self.body = Bytes::from_owner(Body(message));
    }

    /// Marks the stream finished.
    ///
    /// # Panics
    ///
    /// When the sender holds part of a message, or after `end`.
    pub(super) fn end(&mut self) {
        self.check();
        self.finished = true;
    }

    fn check(&self) {
        assert!(!self.holds(), "a sender holds part of a message");
        assert!(!self.finished, "a sender is used after finish");
    }

    fn holds(&self) -> bool {
        !self.unsent.is_empty() || !self.body.is_empty()
    }
}

impl Receiver {
    /// A receiver for `key` that refuses a message over `bytes_max`.
    pub(super) fn new(key: Key, bytes_max: usize) -> Self {
        Self {
            key,
            reader: Reader::new(bytes_max),
            claim: Claim::default(),
            end: None,
        }
    }

    /// The stream this receiver reads.
    pub(crate) fn key(&self) -> Key {
        self.key
    }

    /// What each read gives after the stream ended, once it has.
    pub(super) fn ended(&self) -> Option<Result<Poll<Option<Block>>, Error>> {
        match self.end? {
            End::Finished => Some(Ok(Poll::Ready(None))),
            End::Reset(code) => Some(Err(Error::Reset { code })),
        }
    }
}

impl Budget {
    fn new(max: usize) -> Self {
        Self {
            max,
            used: 0,
            round: 1,
            waiting: Vec::new(),
            smallest: usize::MAX,
        }
    }

    /// Whether the stream of `claim` waits for the next wake.
    fn waits(&self, claim: &Claim) -> bool {
        claim.round == self.round
    }

    /// Charges `bytes` to `claim` when they fit. Else `stream` waits for the next
    /// wake.
    fn charge(&mut self, stream: Key, bytes: usize, claim: &mut Claim) -> bool {
        if self.waits(claim) {
            return false;
        }
        if bytes <= self.max - self.used {
            self.used += bytes;
            claim.bytes = bytes;
            return true;
        }
        claim.round = self.round;
        self.smallest = self.smallest.min(bytes);
        self.waiting.push(stream);
        false
    }

    /// Ends `claim`, the claim of `stream`: gives back its bytes, or stops its wait.
    /// Returns the streams to wake: each waiting stream when the room then fits the
    /// smallest waiting claim, else none.
    fn release(&mut self, stream: Key, claim: &mut Claim) -> vec::Drain<'_, Key> {
        if self.waits(claim) {
            let at = self.waiting.iter().position(|&other| other == stream);
            self.waiting
                .swap_remove(at.expect("invariant: a waiting stream is listed"));
        }
        self.used -= mem::take(claim).bytes;
        let woken = if self.max - self.used >= self.smallest {
            self.round += 1;
            self.smallest = usize::MAX;
            self.waiting.len()
        } else {
            0
        };
        self.waiting.drain(..woken)
    }
}

impl Streams {
    /// The streams of a connection that refuses a message over `bytes_max`, with a
    /// window of `window_bytes`. Until the hello, the peer's window is taken to be
    /// the same.
    pub(super) fn new(window_bytes: usize, bytes_max: usize) -> Self {
        Self {
            unclassified: Vec::new(),
            incoming: Default::default(),
            senders: Vec::new(),
            sending: Budget::new(window_bytes),
            receiving: Budget::new(window_bytes.saturating_add(bytes_max)),
        }
    }

    /// What `event` of `inner`, the connection of `key`, means to the caller, if
    /// anything. A stream that the peer stops resets here with the stop's code.
    ///
    /// # Errors
    ///
    /// [`Fault`] when the peer broke the stream protocol.
    pub(super) fn event(
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
            StreamEvent::Opened { dir } => {
                let mut queued = false;
                while let Some(id) = inner.streams().accept(dir) {
                    if dir == Dir::Bi {
                        self.senders.push((id, None));
                    }
                    queued |= self.classify(inner, id)?;
                }
                Ok(queued.then_some(Event::Incoming { key }))
            }
            StreamEvent::Readable { id } => {
                let Some(at) = self.unclassified.iter().position(|&other| other == id)
                else {
                    return Ok(Some(Event::Readable { stream: stream(id) }));
                };
                self.unclassified.swap_remove(at);
                Ok(self.classify(inner, id)?.then_some(Event::Incoming { key }))
            }
            StreamEvent::Writable { id } => {
                Ok(Some(Event::Writable { stream: stream(id) }))
            }
            StreamEvent::Stopped { id, error_code } => {
                let Some(code) = code(error_code) else {
                    return Err(Fault(format!(
                        "a stop code over 32 bits: {error_code}"
                    )));
                };
                reset(inner, id, code);
                let sender = self.senders.iter_mut().find(|(other, _)| *other == id);
                Ok(sender.map(|(_, stopped)| {
                    *stopped = Some(code);
                    Event::Writable { stream: stream(id) }
                }))
            }
            StreamEvent::Available { .. } => Ok(Some(Event::Available { key })),
            StreamEvent::Finished { .. } => Ok(None),
        }
    }

    /// Opens a stream of `inner` in `dir`. `None` when the peer allows no more now.
    pub(super) fn open(
        &mut self,
        inner: &mut noq_proto::Connection,
        dir: Dir,
    ) -> Option<StreamId> {
        let id = inner.streams().open(dir)?;
        self.senders.push((id, None));
        Some(id)
    }

    /// The next stream the peer opened, highest class first.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a stream is queued only under a class byte"
    )]
    pub(super) fn accept(
        &mut self,
        connection: connection::Key,
        bytes_max: usize,
    ) -> Option<Incoming> {
        let (byte, id) = (0u8..)
            .zip(&mut self.incoming)
            .find_map(|(byte, queue)| Some((byte, queue.pop_front()?)))?;
        let key = Key { connection, id };
        Some(Incoming {
            class: class(byte).expect("invariant: a queued stream has a class byte"),
            receiver: Receiver::new(key, bytes_max),
            sender: (id.dir() == Dir::Bi).then(|| Sender::reply(key)),
        })
    }

    /// Writes what `sender` holds to `inner`. `Ready` when the stream took all of it.
    /// `Pending` when the stream takes no more now, or the message has no room in the
    /// send budget. The senders that room returns to get [`Event::Writable`] in
    /// `events`.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream. The sender drops what it
    /// holds.
    pub(super) fn flush(
        &mut self,
        inner: &mut noq_proto::Connection,
        sender: &mut Sender,
        events: &mut VecDeque<Event>,
    ) -> Result<Poll<()>, Error> {
        let flushed = self.push(inner, sender);
        if !matches!(flushed, Ok(Poll::Pending)) {
            let woken = self.sending.release(sender.key, &mut sender.claim);
            events.extend(woken.map(|stream| Event::Writable { stream }));
        }
        flushed
    }

    fn push(
        &mut self,
        inner: &mut noq_proto::Connection,
        sender: &mut Sender,
    ) -> Result<Poll<()>, Error> {
        let id = sender.key.id;
        let bytes = sender.body.len();
        let blocked = if sender.holds()
            && sender.claim.bytes == 0
            && !self.sending.charge(sender.key, bytes, &mut sender.claim)
        {
            true
        } else {
            let mut send = inner.send_stream(id);
            loop {
                let written = if !sender.unsent.is_empty() {
                    let unsent = &sender.header[sender.unsent.clone()];
                    send.write(unsent).map(|bytes| sender.unsent.start += bytes)
                } else if !sender.body.is_empty() {
                    let mut chunks = slice::from_mut(&mut sender.body);
                    send.write_chunks(&mut chunks).map(drop)
                } else {
                    return Ok(Poll::Ready(()));
                };
                // A reset stream gives `Blocked` while the connection's window is
                // shut.
                match written {
                    Ok(()) => {}
                    Err(WriteError::Blocked) => break true,
                    Err(WriteError::ClosedStream) => break false,
                    Err(WriteError::Stopped(_)) => panic!("{STOPPED}"),
                }
            }
        };
        // Only a push that cannot go on looks for a stop, as the look scans `senders`.
        match (self.stopped(id), blocked) {
            (Some(code), _) => {
                (sender.unsent, sender.body) = (0..0, Bytes::new());
                Err(Error::Stopped { code })
            }
            (None, true) => Ok(Poll::Pending),
            (None, false) => panic!("{OPEN}"),
        }
    }

    /// Ends stream `id` of `inner` after what it holds.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream.
    ///
    /// # Panics
    ///
    /// When this side does not send on `id`, or finished it before.
    pub(super) fn finish(
        &mut self,
        inner: &mut noq_proto::Connection,
        id: StreamId,
    ) -> Result<(), Error> {
        let Some(at) = self.senders.iter().position(|&(other, _)| other == id) else {
            panic!("invariant: a sender finishes once");
        };
        if let (_, Some(code)) = self.senders.swap_remove(at) {
            return Err(Error::Stopped { code });
        }
        match inner.send_stream(id).finish() {
            Ok(()) => Ok(()),
            Err(FinishError::Stopped(_)) => panic!("{STOPPED}"),
            Err(FinishError::ClosedStream) => panic!("{OPEN}"),
        }
    }

    /// Resets `sender`'s stream of `inner` with `code`, and gives back its send budget.
    /// The senders that room returns to get [`Event::Writable`] in `events`.
    pub(super) fn reset(
        &mut self,
        inner: &mut noq_proto::Connection,
        mut sender: Sender,
        code: Code,
        events: &mut VecDeque<Event>,
    ) {
        let id = sender.key.id;
        if let Some(at) = self.senders.iter().position(|&(other, _)| other == id) {
            self.senders.swap_remove(at);
        }
        reset(inner, id, code);
        let woken = self.sending.release(sender.key, &mut sender.claim);
        events.extend(woken.map(|stream| Event::Writable { stream }));
    }

    /// Stops `receiver`'s stream of `inner` with `code`, drops the message in its
    /// reader, and gives back its receive budget. The receivers that room returns to
    /// get [`Event::Readable`] in `events`.
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
        let woken = self.receiving.release(receiver.key, &mut receiver.claim);
        events.extend(woken.map(|stream| Event::Readable { stream }));
    }

    /// Reads the next whole message of `receiver`'s stream from `inner` into a block
    /// from `pool`. `Ready(None)` at the end. `Pending` when no whole message is here
    /// yet, or the next has no room in the receive budget. The receivers that room
    /// returns to get [`Event::Readable`] in `events`.
    ///
    /// # Errors
    ///
    /// [`Error::Reset`] when the peer reset the stream, [`Error::Pool`] when `pool`
    /// has no room now, and [`Error::Broken`] when the peer broke the framing or
    /// reset with a code over 32 bits.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a receiver never reads its stream after the end"
    )]
    pub(super) fn read(
        &mut self,
        inner: &mut noq_proto::Connection,
        receiver: &mut Receiver,
        pool: &Pool,
        events: &mut VecDeque<Event>,
    ) -> Result<Poll<Option<Block>>, Error> {
        let Receiver {
            key,
            reader,
            claim,
            end,
        } = receiver;
        let receiving = &mut self.receiving;
        let mut recv = inner.recv_stream(key.id);
        let mut result = Ok(Poll::Pending);
        if !receiving.waits(claim) {
            let mut chunks = recv.read(true).expect(RECEIVING);
            let admit = |len| receiving.charge(*key, len, claim);
            result = reader.read(pool, admit, |max| match chunks.next(max) {
                Ok(chunk) => Ok(Poll::Ready(chunk.map(|chunk| chunk.bytes))),
                Err(ReadError::Blocked) => Ok(Poll::Pending),
                Err(ReadError::Reset(error)) => Err(reset_error(error)),
            });
        }
        // The reader takes no bytes while it waits, so only this finds a reset.
        if receiving.waits(claim)
            && let Some(error) = recv.received_reset().expect(RECEIVING)
        {
            result = Err(reset_error(error));
        }
        if !matches!(result, Ok(Poll::Pending)) {
            let woken = receiving.release(*key, claim);
            events.extend(woken.map(|stream| Event::Readable { stream }));
        }
        *end = match result {
            Ok(Poll::Ready(None)) => Some(End::Finished),
            Err(Error::Reset { code }) => Some(End::Reset(code)),
            _ => None,
        };
        result
    }

    /// The code the peer stopped stream `id` with, if this side sends on it.
    fn stopped(&self, id: StreamId) -> Option<Code> {
        self.senders.iter().find(|&&(other, _)| other == id)?.1
    }

    /// Reads the class byte of new stream `id`. Returns whether the stream is now
    /// queued for [`Streams::accept`]. A stream that ends or resets before its class
    /// byte drops, and the reply half of a two-way one resets with code 0.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a stream noq-proto just gave is open and gives no empty chunk"
    )]
    fn classify(
        &mut self,
        inner: &mut noq_proto::Connection,
        id: StreamId,
    ) -> Result<bool, Fault> {
        let next = inner
            .recv_stream(id)
            .read(true)
            .expect("invariant: a new stream is open")
            .next(1);
        match next {
            Ok(Some(chunk)) => {
                let byte = *chunk
                    .bytes
                    .first()
                    .expect("invariant: chunks are not empty");
                if class(byte).is_none() {
                    return Err(Fault(format!("a stream of class {byte}")));
                }
                self.incoming[usize::from(byte)].push_back(id);
                Ok(true)
            }
            Err(ReadError::Blocked) => {
                self.unclassified.push(id);
                Ok(false)
            }
            Err(ReadError::Reset(error)) if code(error).is_none() => {
                Err(Fault(format!("a reset code over 32 bits: {error}")))
            }
            Ok(None) | Err(ReadError::Reset(_)) => {
                if id.dir() == Dir::Bi {
                    reset(inner, id, Code(0));
                    self.senders.retain(|&(other, _)| other != id);
                }
                Ok(false)
            }
        }
    }
}

/// The endpoint resets each stream at the peer's stop, before the caller's next call.
const STOPPED: &str = "invariant: a stream resets at the peer's stop";
const OPEN: &str = "invariant: only a stop resets a sender's stream";
const RECEIVING: &str = "invariant: a receiver's stream is open until it ends";

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
    use std::rc::Rc;
    use std::time::Duration;

    use block::Heap;
    use types::time::{Monotonic, Span};

    use super::*;
    use crate::Config;
    use crate::quic::Endpoint;
    use crate::quic::testing::{self, Pair, Shard, Side};
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
        pair.dial(tls::public(&testing::SERVER_KEY));
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
            let written = side.endpoint.write(now, sender, message.clone());
            assert_eq!(written, Ok(Poll::Ready(())));
        }
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
            let written = pair.client.endpoint.write(now, sender, message);
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
        let read = side.endpoint.read(now, receiver)?;
        Ok(read.map(|message| message.map(|message| message.to_vec())))
    }

    /// Writes `bytes` on a new noq-proto stream of `side` in `dir`, and finishes it
    /// when `finished`.
    fn raw(side: &mut Side, dir: Dir, bytes: &[u8], finished: bool) -> StreamId {
        let connection = side.connection();
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
                    &mut pair.client,
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
    fn opened_before_their_class_byte_arrive_when_it_does() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let connection = pair.client.connection();
            let early = connection.streams().open(Dir::Uni).expect("a stream");
            raw(&mut pair.client, Dir::Uni, &[2, 1, b'x'], true);
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
                pair.client.endpoint.flush(now, &mut sender),
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
    fn with_a_full_pool_fail_the_read_until_a_block_frees() {
        testing::run(1, |shard| {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            // A 100-byte block takes 192 bytes of the budget.
            let config = block::Config { budget: 300 };
            let memory = Heap::new(config.reservation());
            let pool = Rc::new(Pool::new(config, memory));
            let config = Config {
                pool: Rc::clone(&pool),
                ..shard.config(testing::SERVER_KEY, Span::SECOND)
            };
            pair.server.endpoint =
                Endpoint::new(&config, testing::SERVER_SHARD, NonZeroUsize::MIN);
            pair.dial(tls::public(&testing::SERVER_KEY));
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
            let full = Error::Pool {
                bytes: 100,
                available: 108,
            };
            let read_full = next(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read_full, Err(full));
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
            (&mut pair.client, testing::CLIENT_KEY, testing::CLIENT_SHARD),
            (&mut pair.server, testing::SERVER_KEY, testing::SERVER_SHARD),
        ];
        for (side, private_key, index) in sides {
            let config = Config {
                window_bytes: NARROW,
                ..shard.config(private_key, Span::SECOND)
            };
            side.endpoint = Endpoint::new(&config, index, NonZeroUsize::MIN);
        }
        pair.dial(tls::public(&testing::SERVER_KEY));
        pair.run(RUN);
        pair
    }

    /// Opens `count` raw `Complete` streams on the client, and sends on each only the
    /// prefix of a message of [`MESSAGE_MAX`] bytes. Gives the streams.
    fn prefixes(pair: &mut Pair, count: u32) -> Vec<StreamId> {
        let prefix = Prefix::new(MESSAGE_MAX);
        let header = [[byte(Class::Complete)].as_slice(), &*prefix].concat();
        let mut ids = Vec::new();
        for _ in 0..count {
            ids.push(raw(&mut pair.client, Dir::Uni, &header, false));
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
                let flushed = pair.client.endpoint.flush(now, sender);
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
    fn prefixes_past_the_budget_wait_and_take_no_more_of_the_pool() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            prefixes(&mut pair, testing::STREAMS_MAX);
            let before = shard.committed();
            let receivers = wait(&mut pair);
            assert_eq!(receivers.len(), 16);
            let taken = shard.committed() - before;
            assert_eq!(taken, 3 * block::footprint(MESSAGE_MAX));
        });
    }

    #[test]
    fn a_read_that_finds_the_pool_full_gives_back_its_budget() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let config = block::Config {
                budget: 3 * block::footprint(MESSAGE_MAX) - 1,
            };
            let memory = Heap::new(config.reservation());
            let config = Config {
                window_bytes: NARROW,
                pool: Rc::new(Pool::new(config, memory)),
                ..shard.config(testing::SERVER_KEY, Span::SECOND)
            };
            pair.server.endpoint =
                Endpoint::new(&config, testing::SERVER_SHARD, NonZeroUsize::MIN);
            pair.server.key = None;
            pair.dial(tls::public(&testing::SERVER_KEY));
            pair.run(RUN);
            prefixes(&mut pair, 3);
            let (now, server) = (pair.now(), key(&pair.server));
            let mut receivers = Vec::new();
            while let Some(incoming) = pair.server.endpoint.accept(server) {
                receivers.push(incoming.receiver);
            }
            assert_eq!(receivers.len(), 3);
            for receiver in &mut receivers[..2] {
                assert_eq!(next(&mut pair.server, now, receiver), Ok(Poll::Pending));
            }
            let full = Err(Error::Pool {
                bytes: MESSAGE_MAX,
                available: block::footprint(MESSAGE_MAX) - 1,
            });
            assert_eq!(next(&mut pair.server, now, &mut receivers[2]), full);
            assert_eq!(next(&mut pair.server, now, &mut receivers[2]), full);
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
                let written =
                    pair.client
                        .endpoint
                        .write(now, sender, shard.block(&message));
                assert!(written.is_ok(), "{written:?}");
                expected.push((sender.key().id, message));
            }
            let mut read = exchange(&mut pair, &mut senders, 50 * RUN);
            read.sort();
            assert_eq!(shapes(&read), shapes(&expected));
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
                let written = pair.client.endpoint.write(now, sender, message);
                assert_eq!(written, Ok(Poll::Pending));
            }
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            let (read, _) = drain(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(read.len(), usize::from(count - 1));
            pair.run(RUN);
            let now = pair.now();
            let flushed = pair.client.endpoint.flush(now, &mut third);
            assert_eq!(flushed, Ok(Poll::Pending));
            pair.run(RUN);
            assert!(pair.server.endpoint.accept(key(&pair.server)).is_none());
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed = pair.client.endpoint.flush(now, &mut second);
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

    #[test]
    fn a_budget_wakes_no_stream_until_the_room_fits_the_smallest_claim_that_waits() {
        let mut budget = Budget::new(10);
        let [mut a, mut b, mut c, mut d] = <[Claim; 4]>::default();
        assert!(budget.charge(stream(0), 9, &mut a));
        assert!(!budget.charge(stream(1), 2, &mut b));
        let woken: Vec<_> = budget.release(stream(0), &mut a).collect();
        assert_eq!(woken, [stream(1)]);
        assert!(budget.charge(stream(1), 2, &mut b));
        assert!(budget.charge(stream(2), 7, &mut c));
        assert!(!budget.charge(stream(3), 5, &mut d));
        assert_eq!(budget.release(stream(1), &mut b).count(), 0);
        let woken: Vec<_> = budget.release(stream(2), &mut c).collect();
        assert_eq!(woken, [stream(3)]);
    }

    #[test]
    fn a_budget_wakes_every_waiting_stream_when_the_room_fits_the_smallest_claim() {
        let mut budget = Budget::new(10);
        let [mut a, mut b, mut c, mut d] = <[Claim; 4]>::default();
        assert!(budget.charge(stream(0), 6, &mut a));
        assert!(budget.charge(stream(3), 3, &mut d));
        assert!(!budget.charge(stream(1), 2, &mut b));
        assert!(!budget.charge(stream(2), 5, &mut c));
        let woken: Vec<_> = budget.release(stream(3), &mut d).collect();
        assert_eq!(woken, [stream(1), stream(2)]);
    }

    #[test]
    fn a_read_wakes_no_stream_until_the_room_fits_the_smallest_waiting_message() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let small =
                [[byte(Class::Complete)].as_slice(), &*Prefix::new(100)].concat();
            let ids = [
                raw(&mut pair.client, Dir::Uni, &small, false),
                raw(&mut pair.client, Dir::Uni, &small, false),
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
                let written = pair.client.endpoint.write(now, sender, message);
                assert_eq!(written, Ok(Poll::Pending));
            }
            pair.run(RUN);
            let id = accept(&mut pair.server).receiver.key().id;
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            pair.run(RUN);
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed = pair.client.endpoint.flush(now, &mut first);
            assert_eq!(flushed, Err(Error::Stopped { code: Code(7) }));
            pair.run(Duration::ZERO);
            let writable = Event::Writable {
                stream: third.key(),
            };
            assert!(got(&pair.client, seen, &writable));
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
                let written = pair.client.endpoint.write(now, sender, message);
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
            let flushed = pair.client.endpoint.flush(now, &mut third);
            assert_eq!(flushed, Err(Error::Stopped { code: Code(7) }));
            exchange(&mut pair, &mut [first, second], 10 * RUN);
            assert!(!got(&pair.client, seen, &writable));
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
    fn a_reset_message_that_finds_no_room_after_a_wake_fails_the_read() {
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
            let read = next(&mut pair.server, now, &mut receivers[3]);
            assert_eq!(read, Err(Error::Reset { code: Code(7) }));
        });
    }

    /// Opens `count` senders on the narrow `pair`'s client, and writes to each a
    /// message of [`MESSAGE_MAX`] bytes that waits.
    fn pending(pair: &mut Pair, shard: &Shard, count: usize) -> Vec<Sender> {
        let mut senders = Vec::new();
        for _ in 0..count {
            let mut sender = open_sender(pair, Class::Complete);
            let (now, message) = (pair.now(), shard.block(&vec![1; MESSAGE_MAX]));
            let written = pair.client.endpoint.write(now, &mut sender, message);
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
            pair.client.endpoint.reset(now, first, Code(9));
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
            pair.client.endpoint.reset(now, sender, Code(9));
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
            let waiting = senders.remove(1);
            let writable = Event::Writable {
                stream: waiting.key(),
            };
            pair.client.endpoint.reset(now, waiting, Code(9));
            pair.client.endpoint.reset(now, first, Code(9));
            pair.run(Duration::ZERO);
            assert!(!got(&pair.client, seen, &writable));
            let woken = Event::Writable {
                stream: senders[1].key(),
            };
            assert!(got(&pair.client, seen, &woken));
        });
    }

    #[test]
    fn a_reset_sender_that_the_peer_stops_gives_no_event() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let receiver = accept(&mut pair.server).receiver;
            let (seen, now) = (pair.client.events.len(), pair.now());
            pair.client.endpoint.reset(now, sender, Code(9));
            pair.server.endpoint.stop(now, receiver, Code(9));
            pair.run(RUN);
            assert_eq!(pair.client.events.len(), seen, "{:?}", pair.client.events);
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
            pair.client.endpoint.reset(now, sender, Code(9));
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
            let written = pair.client.endpoint.write(now, &mut sender, message);
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
            pair.client.endpoint.reset(now, first, Code(9));
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
            pair.client.endpoint.reset(now, sender, Code(9));
            pair.server.endpoint.stop(now, incoming.receiver, Code(9));
            pair.run(Duration::ZERO);
            assert_eq!((pair.client.sent.len(), pair.server.sent.len()), sent);
        });
    }

    #[test]
    #[should_panic(expected = "a message of 65537 bytes is over the largest message, \
        65536 bytes")]
    fn a_message_over_the_largest_panics() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let (now, message) = (pair.now(), shard.block(&vec![1; MESSAGE_MAX + 1]));
            drop(pair.client.endpoint.write(now, &mut sender, message));
        });
    }

    #[test]
    #[should_panic(
        expected = "a window of 65535 bytes is below the largest message, 65536 bytes"
    )]
    fn with_a_window_below_one_message_panics() {
        testing::run(1, |shard| {
            let config = Config {
                window_bytes: MESSAGE_MAX - 1,
                ..shard.config(testing::SERVER_KEY, Span::SECOND)
            };
            drop(Endpoint::new(
                &config,
                testing::SERVER_SHARD,
                NonZeroUsize::MIN,
            ));
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
            pair.dial(tls::public(&testing::SERVER_KEY));
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
            assert_eq!(pair.server.events.len(), 1, "{:?}", pair.server.events);
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

    /// Opens [`testing::STREAMS_MAX`] raw streams in `dir` on the client, and ends
    /// each before its class byte: with a reset of `code`, or else with a finish.
    fn end_before_the_class_byte(pair: &mut Pair, dir: Dir, code: Option<VarInt>) {
        for _ in 0..testing::STREAMS_MAX {
            let id = raw(&mut pair.client, dir, &[], code.is_none());
            if let Some(code) = code {
                let reset = pair.client.connection().send_stream(id).reset(code);
                reset.expect("reset");
            }
        }
        pair.run(RUN);
    }

    #[test]
    fn that_end_before_the_class_byte_drop_and_free_their_slot() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            for dir in [Dir::Uni, Dir::Bi] {
                for code in [None, Some(VarInt::from_u32(7))] {
                    end_before_the_class_byte(&mut pair, dir, code);
                    let open =
                        pair.server.connection().streams().remote_open_streams(dir);
                    assert_eq!(open, 0, "{dir:?} {code:?}");
                }
            }
            let events = events(&pair.server);
            let quiet = events.iter().all(|event| {
                matches!(event, Event::Connected { .. } | Event::Readable { .. })
            });
            assert!(quiet, "{events:?}");
            assert!(pair.server.endpoint.accept(key(&pair.server)).is_none());
        });
    }

    #[test]
    fn that_end_before_the_class_byte_leave_the_other_streams_alone() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.server));
            let sender = pair.server.endpoint.open_sender(now, key, Class::Complete);
            let mut sender = sender.expect("a stream");
            write(&mut pair.server, now, &mut sender, &[shard.block(b"a")]);
            end_before_the_class_byte(&mut pair, Dir::Bi, None);
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
            let id = raw(&mut pair.client, Dir::Uni, &[], false);
            let code = VarInt::from_u64(1 << 32).expect("a varint");
            let reset = pair.client.connection().send_stream(id).reset(code);
            reset.expect("reset");
            assert_broken(&mut pair, true, "a reset code over 32 bits: 4294967296");
        });
    }

    /// Opens a raw `Complete` stream on the client, lets the server see it, and
    /// resets it with `code`. Gives the server's receiver.
    fn reset(pair: &mut Pair, code: VarInt) -> Receiver {
        let id = raw(&mut pair.client, Dir::Uni, &[2], false);
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
    fn reset_with_a_code_over_32_bits_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let code = VarInt::from_u64(1 << 32).expect("a varint");
            let mut receiver = reset(&mut pair, code);
            let now = pair.now();
            let read = next(&mut pair.server, now, &mut receiver);
            assert_eq!(read, Ok(Poll::Pending));
            assert_broken(&mut pair, true, "a reset code over 32 bits: 4294967296");
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
    fn stopped_by_the_peer_are_writable_and_fail_each_write() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = stop(&mut pair, shard, VarInt::from_u32(7));
            let writable = Event::Writable {
                stream: sender.key(),
            };
            assert_eq!(events(&pair.client).last(), Some(&&writable));
            let now = pair.now();
            for _ in 0..2 {
                let written =
                    pair.client
                        .endpoint
                        .write(now, &mut sender, shard.block(b"b"));
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
            let written =
                pair.client
                    .endpoint
                    .write(now, &mut sender, shard.block(b"b"));
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
            raw(&mut pair.client, Dir::Uni, &[4], false);
            assert_broken(&mut pair, true, "a stream of class 4");
        });
    }

    #[test]
    fn with_faults_on_two_streams_at_once_close_once() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            raw(&mut pair.client, Dir::Uni, &[4], false);
            raw(&mut pair.client, Dir::Bi, &[4], false);
            assert_broken(&mut pair, true, "a stream of class 4");
            let events = events(&pair.server);
            let closed = events
                .iter()
                .filter(|event| matches!(event, Event::Closed { .. }));
            assert_eq!(closed.count(), 1, "{events:?}");
        });
    }

    /// Sends `bytes` and the end on a raw stream from the client, and reads it on the
    /// server, which finds a fault.
    fn misframe(pair: &mut Pair, bytes: &[u8]) {
        raw(&mut pair.client, Dir::Uni, bytes, true);
        pair.run(RUN);
        let mut incoming = accept(&mut pair.server);
        let now = pair.now();
        let read = next(&mut pair.server, now, &mut incoming.receiver);
        assert_eq!(read, Ok(Poll::Pending));
    }

    #[test]
    fn with_a_message_over_the_limit_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            misframe(&mut pair, &[2, 0x80, 1, 0, 1]);
            let reason = "a message of 65537 bytes is over the limit of 65536";
            assert_broken(&mut pair, true, reason);
        });
    }

    #[test]
    fn that_end_inside_a_message_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            misframe(&mut pair, &[2, 3, b'a']);
            assert_broken(&mut pair, true, "the stream ended inside a message");
        });
    }

    #[test]
    fn after_the_connection_ends_give_nothing_and_take_nothing() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Complete);
            let (mut sender, mut receiver) = opened.expect("a stream");
            let mut other = open_sender(&mut pair, Class::Latest);
            pair.client.endpoint.close(now, key, Code(7));
            for run in [Duration::ZERO, Duration::from_secs(3)] {
                pair.run(run);
                let now = pair.now();
                assert_eq!(
                    next(&mut pair.client, now, &mut receiver),
                    Ok(Poll::Pending)
                );
                let endpoint = &mut pair.client.endpoint;
                assert!(endpoint.open(now, key, Class::Command).is_none());
                assert!(endpoint.accept(key).is_none());
                assert_eq!(endpoint.flush(now, &mut sender), Ok(Poll::Pending));
            }
            let now = pair.now();
            let endpoint = &mut pair.client.endpoint;
            let written = endpoint.write(now, &mut sender, shard.block(b"a"));
            assert_eq!(written, Ok(Poll::Pending));
            assert_eq!(endpoint.finish(now, &mut other), Ok(()));
            endpoint.reset(now, sender, Code(9));
            endpoint.stop(now, receiver, Code(9));
        });
    }

    #[test]
    #[should_panic(expected = "a sender holds part of a message")]
    fn a_write_while_the_sender_holds_part_of_a_message_panics() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut sender);
            let now = pair.now();
            drop(
                pair.client
                    .endpoint
                    .write(now, &mut sender, shard.block(b"a")),
            );
        });
    }

    #[test]
    #[should_panic(expected = "a sender is used after finish")]
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
}
