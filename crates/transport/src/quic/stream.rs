//! The streams of a connection: whole messages in order over noq-proto's streams. The
//! side that opens a stream starts it with its class byte.

use std::collections::VecDeque;
use std::ops::Range;
use std::task::Poll;
use std::{mem, slice};

use block::{Block, Pool};
use bytes::Bytes;
use noq_proto::{
    ClosedStream, Dir, FinishError, ReadError, StreamEvent, StreamId, VarInt,
    WriteError,
};

use super::connection::{self, Fault};
use super::hello::{self, Hello};
use super::{Body, Event};
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
/// the stream: the message in hand keeps its send budget and its turn, or its place
/// among the streams that wait, until the connection ends.
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
    /// The sender waits its turn in [`Turns`].
    waiting: bool,
    finished: bool,
    /// The peer's largest message.
    bytes_max: usize,
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
    /// This side's limits, which its hello carries.
    own: Hello,
    /// This side sent its hello.
    greeted: bool,
    /// Until the peer's hello arrives, no stream opens or is accepted.
    peer: hello::Peer,
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
    /// room for one more message. Empty until the peer's hello.
    sending: Budget,
    turns: Turns,
    /// The messages that hold a block and have not gone to the caller.
    receiving: Budget,
}

/// The message bytes that one direction of a connection counts, and the claims that
/// found no room. Room goes to waiting claims highest class first, then oldest first.
#[derive(Debug)]
struct Budget {
    max: usize,
    used: usize,
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

/// The senders that wait for noq-proto to take more of their message, by class rank,
/// oldest first. Only the first in turn writes.
#[derive(Debug, Default)]
struct Turns([VecDeque<Key>; 4]);

impl Sender {
    /// A sender for `key` that starts the stream with `class`'s byte, to a peer whose
    /// largest message is `bytes_max`.
    fn new(key: Key, class: Class, bytes_max: usize) -> Self {
        let mut header = [0; 9];
        header[0] = byte(class);
        Self {
            started: false,
            header,
            ..Self::reply(key, class, bytes_max)
        }
    }

    /// A sender for the reply half of `key`, a stream of `class` that the peer opened,
    /// whose largest message is `bytes_max`.
    fn reply(key: Key, class: Class, bytes_max: usize) -> Self {
        Self {
            key,
            started: true,
            header: [0; 9],
            unsent: 0..0,
            body: Bytes::new(),
            claim: Claim::new(class),
            waiting: false,
            finished: false,
            bytes_max,
        }
    }

    /// The stream this sender writes.
    pub(crate) fn key(&self) -> Key {
        self.key
    }

    /// Takes `message` as the message in hand, after its header. The sender holds no
    /// part of a message and is not finished.
    pub(super) fn load(&mut self, message: Block) {
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

    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over the peer's largest message.
    pub(super) fn check_size(&self, message: &Block) -> Result<(), Error> {
        if message.len() > self.bytes_max {
            return Err(Error::TooLarge {
                bytes: message.len(),
                bytes_max: self.bytes_max,
            });
        }
        Ok(())
    }

    /// # Panics
    ///
    /// When the sender holds part of a message, or after `end`.
    pub(super) fn check(&self) {
        assert!(!self.holds(), "a sender holds part of a message");
        self.check_unfinished();
    }

    /// # Panics
    ///
    /// After `end`.
    pub(super) fn check_unfinished(&self) {
        assert!(!self.finished, "a sender is used after finish");
    }

    fn holds(&self) -> bool {
        !self.unsent.is_empty() || !self.body.is_empty()
    }
}

impl Receiver {
    /// A receiver for `key`, a stream of `class`, that refuses a message over
    /// `bytes_max`.
    pub(super) fn new(key: Key, class: Class, bytes_max: usize) -> Self {
        Self {
            key,
            reader: Reader::new(bytes_max),
            claim: Claim::new(class),
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

    /// Charges `bytes` to `claim`, which holds none, when they fit now and no claim
    /// of its class or a higher class waits.
    fn admit(&mut self, bytes: usize, claim: &mut Claim) -> bool {
        let first = self.waiting[..=claim.class.rank()]
            .iter()
            .all(VecDeque::is_empty);
        let fits = first && bytes <= self.max - self.used;
        if fits {
            self.used += bytes;
            claim.state = State::Held(bytes);
        }
        fits
    }

    /// Whether `claim`, the claim of `stream`, holds its bytes. A claim that holds
    /// none charges `bytes` when [`Budget::admit`] does, and else waits for room. A
    /// claim that waits takes the room it got.
    ///
    /// # Panics
    ///
    /// When a claim that holds none asks for more than the budget, which no claim
    /// could ever fit.
    fn charge(&mut self, stream: Key, bytes: usize, claim: &mut Claim) -> bool {
        match claim.state {
            State::Held(_) => true,
            State::Queued { bytes, .. } if !self.waits(claim) => {
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
                if self.admit(bytes, claim) {
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
    /// waiting claims in order until the next does not fit, and calls `woken` with
    /// the stream of each.
    fn release(&mut self, claim: &mut Claim, mut woken: impl FnMut(Key)) {
        let rank = claim.class.rank();
        match mem::replace(&mut claim.state, State::Idle) {
            State::Idle => {}
            State::Queued { ticket, bytes } if ticket < self.granted[rank] => {
                self.used -= bytes;
            }
            State::Queued { ticket, .. } => {
                let waiting = &mut self.waiting[rank];
                let at = waiting.binary_search_by_key(&ticket, |wait| wait.ticket);
                waiting.remove(at.expect("invariant: a waiting claim is queued"));
            }
            State::Held(bytes) => self.used -= bytes,
        }
        for (waiting, granted) in self.waiting.iter_mut().zip(&mut self.granted) {
            while let Some(next) = waiting.front() {
                if next.bytes > self.max - self.used {
                    return;
                }
                self.used += next.bytes;
                *granted = next.ticket + 1;
                woken(next.stream);
                waiting.pop_front();
            }
        }
    }
}

impl Turns {
    /// The first sender in turn, if any waits.
    fn first(&self) -> Option<Key> {
        self.0.iter().find_map(|queue| queue.front().copied())
    }

    /// Whether `sender` may write now: it is first in turn, or it does not wait and
    /// no sender of its class or a higher class waits.
    fn open(&self, sender: &Sender) -> bool {
        if sender.waiting {
            self.first() == Some(sender.key)
        } else {
            let rank = sender.claim.class.rank();
            self.0[..=rank].iter().all(VecDeque::is_empty)
        }
    }

    /// Queues `sender` last in its class, unless it waits.
    fn join(&mut self, sender: &mut Sender) {
        if !sender.waiting {
            self.0[sender.claim.class.rank()].push_back(sender.key);
            sender.waiting = true;
        }
    }

    /// Takes `sender` out of the queue, if it waits. Gives the new first sender when
    /// `sender` was first.
    #[expect(clippy::unwrap_in_result, reason = "a waiting sender is queued")]
    fn leave(&mut self, sender: &mut Sender) -> Option<Key> {
        if !mem::take(&mut sender.waiting) {
            return None;
        }
        let first = self.first() == Some(sender.key);
        let queue = &mut self.0[sender.claim.class.rank()];
        let at = queue.iter().position(|&key| key == sender.key);
        queue.remove(at.expect("invariant: a waiting sender is queued"));
        first.then(|| self.first()).flatten()
    }
}

impl Streams {
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
            unclassified: Vec::new(),
            incoming: Default::default(),
            senders: Vec::new(),
            sending: Budget::new(0),
            turns: Turns::default(),
            receiving: Budget::new(window_bytes.saturating_add(bytes_max)),
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
    /// one for each frame. Until the peer's hello arrives, every event goes to the
    /// hello. A stream that the peer stops resets here with the stop's code.
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
        self.sending = Budget::new(peer.window_bytes);
        events.push_back(Event::Available { key });
        let bi = self.take(inner, Dir::Bi)?;
        if self.take(inner, Dir::Uni)? || bi {
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
            StreamEvent::Opened { dir } => {
                Ok(self.take(inner, dir)?.then_some(Event::Incoming { key }))
            }
            StreamEvent::Readable { id } => {
                let Some(at) = self.unclassified.iter().position(|&other| other == id)
                else {
                    return Ok(Some(Event::Readable { stream: stream(id) }));
                };
                self.unclassified.swap_remove(at);
                Ok(self.classify(inner, id)?.then_some(Event::Incoming { key }))
            }
            StreamEvent::Writable { .. } => {
                Ok(self.turns.first().map(|stream| Event::Writable { stream }))
            }
            StreamEvent::Stopped { id, error_code } => {
                let code = reset_stopped(inner, id, error_code)?;
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

    /// Opens a stream of `class` of `connection`'s `inner` in `dir`, and gives its
    /// sender. `None` until the peer's hello, and when the peer allows no more now.
    pub(super) fn open(
        &mut self,
        inner: &mut noq_proto::Connection,
        connection: connection::Key,
        dir: Dir,
        class: Class,
    ) -> Option<Sender> {
        let peer = self.peer.hello()?;
        let id = inner.streams().open(dir)?;
        let prioritized = inner.send_stream(id).set_priority(priority(class));
        prioritized.expect("invariant: a stream that opens has a send half");
        self.senders.push((id, None));
        let key = Key { connection, id };
        Some(Sender::new(key, class, peer.message_bytes_max))
    }

    /// The next stream the peer opened, highest class first.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a stream is queued only under a class byte"
    )]
    pub(super) fn accept(&mut self, connection: connection::Key) -> Option<Incoming> {
        let (byte, id) = (0u8..)
            .zip(&mut self.incoming)
            .find_map(|(byte, queue)| Some((byte, queue.pop_front()?)))?;
        let key = Key { connection, id };
        let class = class(byte).expect("invariant: a queued stream has a class byte");
        let peer = self.peer.hello();
        let peer = peer.expect("invariant: streams queue after the peer's hello");
        let reply = || Sender::reply(key, class, peer.message_bytes_max);
        Some(Incoming {
            class,
            receiver: Receiver::new(key, class, self.own.message_bytes_max),
            sender: (id.dir() == Dir::Bi).then(reply),
        })
    }

    /// Writes what `sender` holds to `inner`. `Ready` when the stream took all of it.
    /// `Pending` when the stream takes no more now, the message has no room in the
    /// send budget, or it is not first in turn. The senders that get the freed room or
    /// the turn get [`Event::Writable`] in `events`.
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
            self.release(sender, events);
        }
        flushed
    }

    /// Takes the block out of `message` and puts it on `sender`'s stream of `inner`
    /// when the stream can take it now: after a flush, `sender` holds no part of an
    /// earlier message, the send budget has room, and no stream of its class or a
    /// higher class waits for room or its turn. Else leaves it, and the stream does
    /// not wait for room for it.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream.
    pub(super) fn try_write(
        &mut self,
        inner: &mut noq_proto::Connection,
        sender: &mut Sender,
        message: &mut Option<Block>,
        events: &mut VecDeque<Event>,
    ) -> Result<(), Error> {
        if sender.holds() && self.flush(inner, sender, events)?.is_pending() {
            return Ok(());
        }
        let open = self.turns.open(sender);
        let admitted = |next: &mut Block| {
            open && self.sending.admit(next.len(), &mut sender.claim)
        };
        let Some(taken) = message.take_if(admitted) else {
            return match self.stopped(sender.key.id) {
                Some(code) => Err(Error::Stopped { code }),
                None => Ok(()),
            };
        };
        sender.load(taken);
        self.flush(inner, sender, events).map(drop)
    }

    fn push(
        &mut self,
        inner: &mut noq_proto::Connection,
        sender: &mut Sender,
    ) -> Result<Poll<()>, Error> {
        if !sender.holds() {
            return Ok(Poll::Ready(()));
        }
        let id = sender.key.id;
        let bytes = sender.body.len();
        let charged = self.sending.charge(sender.key, bytes, &mut sender.claim);
        let blocked = if charged && self.turns.open(sender) {
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
        } else {
            true
        };
        // Only a push that cannot go on looks for a stop, as the look scans `senders`.
        match (self.stopped(id), blocked) {
            (Some(code), _) => {
                (sender.unsent, sender.body) = (0..0, Bytes::new());
                Err(Error::Stopped { code })
            }
            (None, true) => {
                // A message that waits for budget does not hold back a write.
                if charged {
                    self.turns.join(sender);
                }
                Ok(Poll::Pending)
            }
            (None, false) => panic!("{OPEN}"),
        }
    }

    /// Ends the message that `sender` holds or waits for: gives back its send budget
    /// and its turn. The senders that get the room or the turn get
    /// [`Event::Writable`] in `events`.
    fn release(&mut self, sender: &mut Sender, events: &mut VecDeque<Event>) {
        let woken = |stream| events.push_back(Event::Writable { stream });
        self.sending.release(&mut sender.claim, woken);
        if let Some(stream) = self.turns.leave(sender) {
            events.push_back(Event::Writable { stream });
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

    /// Resets `sender`'s stream of `inner` with `code`, and gives back its send budget
    /// and its turn. The senders that get the freed room or the turn get
    /// [`Event::Writable`] in `events`.
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
        self.release(&mut sender, events);
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
        self.receiving.release(&mut receiver.claim, woken);
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
            self.receiving.release(&mut receiver.claim, woken);
        }
    }

    /// Reads the next whole message of `receiver`'s stream from `inner` into a block
    /// from `pool`. `Ready(None)` at the end. `Pending` when no whole message is here
    /// yet, or the next has no room in the receive budget or waits behind a stream of
    /// its class or a higher class. The receivers that get the freed room get
    /// [`Event::Readable`] in `events`.
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
            receiving.release(claim, |stream| {
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

    /// Accepts each new stream of `inner` in `dir` and reads its class byte. Returns
    /// whether one is now queued for [`Streams::accept`].
    #[expect(
        clippy::unwrap_in_result,
        reason = "a two-way stream noq-proto just gave has a send half"
    )]
    fn take(
        &mut self,
        inner: &mut noq_proto::Connection,
        dir: Dir,
    ) -> Result<bool, Fault> {
        let mut queued = false;
        while let Some(id) = inner.streams().accept(dir) {
            if dir == Dir::Bi {
                // A stop before the peer's hello gave this side no event.
                let stopped = inner.send_stream(id).stopped();
                let stopped = stopped.expect("invariant: a new stream has a send half");
                let code = stopped.map(|code| reset_stopped(inner, id, code));
                self.senders.push((id, code.transpose()?));
            }
            queued |= self.classify(inner, id)?;
        }
        Ok(queued)
    }

    /// The code the peer stopped stream `id` with, if this side sends on it.
    fn stopped(&self, id: StreamId) -> Option<Code> {
        self.senders.iter().find(|&&(other, _)| other == id)?.1
    }

    /// Reads the class byte of new stream `id`, and gives the reply half of a two-way
    /// one the priority of the class. Returns whether the stream is now queued for
    /// [`Streams::accept`]. A stream that ends or resets before its class byte drops,
    /// and the reply half of a two-way one resets with code 0.
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
                let Some(class) = class(byte) else {
                    return Err(Fault(format!("a stream of class {byte}")));
                };
                if id.dir() == Dir::Bi {
                    // A stop resets the reply half at once, and noq-proto frees it once
                    // the peer has the reset. Its first write then gives `Stopped`.
                    match inner.send_stream(id).set_priority(priority(class)) {
                        Ok(()) | Err(ClosedStream { .. }) => {}
                    }
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
const OPENED: &str = "invariant: a stream this side just opened is open";

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
        pair.dial(tls::public(&pair::SERVER_KEY));
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
            pair.server.endpoint =
                Endpoint::new(&config, pair::SERVER_SHARD, NonZeroUsize::MIN);
            pair.dial(tls::public(&pair::SERVER_KEY));
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
            (&mut pair.client, pair::CLIENT_KEY, pair::CLIENT_SHARD),
            (&mut pair.server, pair::SERVER_KEY, pair::SERVER_SHARD),
        ];
        for (side, private_key, index) in sides {
            let config = Config {
                window_bytes: NARROW,
                ..shard.config(private_key, Span::SECOND)
            };
            side.endpoint = Endpoint::new(&config, index, NonZeroUsize::MIN);
        }
        pair.dial(tls::public(&pair::SERVER_KEY));
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
                ..shard.config(pair::SERVER_KEY, Span::SECOND)
            };
            pair.server.endpoint =
                Endpoint::new(&config, pair::SERVER_SHARD, NonZeroUsize::MIN);
            pair.server.key = None;
            pair.dial(tls::public(&pair::SERVER_KEY));
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
            let flushed = pair.client.endpoint.flush(now, &mut first);
            assert_eq!(flushed, Ok(Poll::Ready(())));
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

    /// `N` claims of `class`.
    fn claims<const N: usize>(class: Class) -> [Claim; N] {
        [(); N].map(|()| Claim::new(class))
    }

    /// Releases `claim`, and returns the streams that got room.
    fn release(budget: &mut Budget, claim: &mut Claim) -> Vec<Key> {
        let mut woken = Vec::new();
        budget.release(claim, |stream| woken.push(stream));
        woken
    }

    #[test]
    fn a_budget_wakes_no_stream_until_the_room_fits_the_first_claim_that_waits() {
        let mut budget = Budget::new(10);
        let [mut a, mut b, mut c, mut d] = claims(Class::Complete);
        assert!(budget.charge(stream(0), 9, &mut a));
        assert!(!budget.charge(stream(1), 2, &mut b));
        let woken = release(&mut budget, &mut a);
        assert_eq!(woken, [stream(1)]);
        assert!(budget.charge(stream(1), 2, &mut b));
        assert!(budget.charge(stream(2), 7, &mut c));
        assert!(!budget.charge(stream(3), 5, &mut d));
        assert_eq!(release(&mut budget, &mut b), []);
        let woken = release(&mut budget, &mut c);
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
        assert!(budget.charge(stream(0), 10, a));
        for (index, claim) in (1..).zip(rest) {
            assert!(!budget.charge(stream(index), 3, claim));
        }
        let woken = release(&mut budget, a);
        assert_eq!(woken, [stream(2), stream(4), stream(3)]);
    }

    #[test]
    fn a_budget_gives_no_room_past_a_waiting_claim_that_does_not_fit() {
        let mut budget = Budget::new(10);
        let [mut first, mut second, mut large, mut small] = claims(Class::Complete);
        let [mut catch_up] = claims(Class::CatchUp);
        assert!(budget.charge(stream(0), 6, &mut first));
        assert!(budget.charge(stream(1), 3, &mut second));
        assert!(!budget.charge(stream(2), 5, &mut large));
        assert!(!budget.charge(stream(3), 1, &mut small));
        assert!(!budget.charge(stream(4), 1, &mut catch_up));
        assert_eq!(release(&mut budget, &mut second), []);
        let woken = release(&mut budget, &mut first);
        assert_eq!(woken, [stream(2), stream(3), stream(4)]);
    }

    #[test]
    fn a_budget_gives_room_past_a_waiting_claim_that_ends() {
        let mut budget = Budget::new(10);
        let [mut a, mut b, mut c] = claims(Class::Complete);
        assert!(budget.charge(stream(0), 8, &mut a));
        assert!(!budget.charge(stream(1), 5, &mut b));
        assert!(!budget.charge(stream(2), 2, &mut c));
        assert_eq!(release(&mut budget, &mut b), [stream(2)]);
    }

    #[test]
    fn a_budget_refuses_a_claim_behind_any_higher_class_that_waits() {
        let mut budget = Budget::new(20);
        let [mut held, mut command] = claims(Class::Command);
        let [mut catch_up] = claims(Class::CatchUp);
        assert!(budget.charge(stream(0), 8, &mut held));
        assert!(!budget.charge(stream(1), 15, &mut command));
        assert!(!budget.charge(stream(2), 15, &mut catch_up));
        assert!(!budget.admit(1, &mut Claim::new(Class::Latest)));
    }

    #[test]
    fn a_budget_starts_a_new_claim_only_ahead_of_lower_classes_that_wait() {
        let mut budget = Budget::new(20);
        let [mut held] = claims(Class::Command);
        let [mut waiting] = claims(Class::Latest);
        assert!(budget.charge(stream(0), 8, &mut held));
        assert!(!budget.charge(stream(1), 15, &mut waiting));
        let classes = [
            Class::Command,
            Class::Latest,
            Class::Complete,
            Class::CatchUp,
        ];
        let admitted = classes.map(|class| budget.admit(1, &mut Claim::new(class)));
        assert_eq!(admitted, [true, false, false, false]);
    }

    #[test]
    fn a_budget_counts_room_it_gives_until_the_claim_takes_it_or_ends() {
        let mut budget = Budget::new(10);
        let [mut a, mut b, mut c] = claims(Class::Complete);
        assert!(budget.charge(stream(0), 10, &mut a));
        assert!(!budget.charge(stream(1), 6, &mut b));
        assert!(!budget.charge(stream(2), 6, &mut c));
        let woken = release(&mut budget, &mut a);
        assert_eq!(woken, [stream(1)]);
        assert!(!budget.admit(5, &mut Claim::new(Class::Command)));
        let woken = release(&mut budget, &mut b);
        assert_eq!(woken, [stream(2)]);
        assert!(budget.charge(stream(2), 6, &mut c));
        assert!(budget.admit(4, &mut Claim::new(Class::Command)));
    }

    #[test]
    #[should_panic(expected = "a claim of 11 bytes is over the budget, 10 bytes")]
    fn a_budget_refuses_a_claim_over_the_budget_before_it_waits() {
        let mut budget = Budget::new(10);
        let [mut a, mut b] = claims(Class::CatchUp);
        assert!(budget.charge(stream(0), 10, &mut a));
        budget.charge(stream(1), 11, &mut b);
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
            let classes = [incoming.receiver.claim.class, reply.claim.class];
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
                [[byte(Class::Complete)].as_slice(), &*Prefix::new(100)].concat();
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
    fn a_message_over_the_peer_largest_fails_the_write() {
        testing::run(1, |shard| {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            let config = Config {
                message_bytes_max: NonZeroUsize::new(1_472).expect("not zero"),
                ..shard.config(pair::SERVER_KEY, Span::SECOND)
            };
            let shard_key = pair::SERVER_SHARD;
            pair.server.endpoint = Endpoint::new(&config, shard_key, NonZeroUsize::MIN);
            pair.dial(tls::public(&pair::SERVER_KEY));
            pair.run(RUN);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let too_large = Err(Error::TooLarge {
                bytes: 1_473,
                bytes_max: 1_472,
            });
            let over = shard.block(&[1; 1_473]);
            let written = pair.client.endpoint.write(now, &mut sender, over.clone());
            assert_eq!(written, too_large.clone().map(|()| Poll::Ready(())));
            let written = pair.client.endpoint.try_write(now, &mut sender, over);
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
            pair.client.endpoint = Endpoint::new(&config, shard_key, NonZeroUsize::MIN);
            pair.dial(tls::public(&pair::SERVER_KEY));
            pair.run(RUN);
            let (now, key) = (pair.now(), key(&pair.client));
            let opened = pair.client.endpoint.open(now, key, Class::Command);
            let (mut sender, _receiver) = opened.expect("a stream");
            write(&mut pair.client, now, &mut sender, &[shard.block(b"a")]);
            pair.run(RUN);
            let incoming = accept(&mut pair.server);
            let mut reply = incoming.sender.expect("a two-way stream");
            let now = pair.now();
            let over = shard.block(&[1; 1_473]);
            let written = pair.server.endpoint.write(now, &mut reply, over);
            let too_large = Error::TooLarge {
                bytes: 1_473,
                bytes_max: 1_472,
            };
            assert_eq!(written, Err(too_large));
        });
    }

    #[test]
    #[should_panic(expected = "config window_bytes must be at least message_bytes_max")]
    fn with_a_window_below_one_message_panics() {
        testing::run(1, |shard| {
            let config = Config {
                window_bytes: MESSAGE_MAX - 1,
                ..shard.config(pair::SERVER_KEY, Span::SECOND)
            };
            drop(Endpoint::new(
                &config,
                pair::SERVER_SHARD,
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
            pair.dial(tls::public(&pair::SERVER_KEY));
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

    /// Opens [`testing::STREAMS_MAX`] raw streams in `dir` on the client, and ends
    /// each before its class byte: with a reset of `code`, or else with a finish.
    fn end_before_the_class_byte(pair: &mut Pair, dir: Dir, code: Option<VarInt>) {
        for _ in 0..testing::STREAMS_MAX {
            let id = raw(pair.client.connection(), dir, &[], code.is_none());
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
            let id = raw(pair.client.connection(), Dir::Uni, &[], false);
            let code = VarInt::from_u64(1 << 32).expect("a varint");
            let reset = pair.client.connection().send_stream(id).reset(code);
            reset.expect("reset");
            assert_broken(&mut pair, true, "a reset code over 32 bits: 4294967296");
        });
    }

    /// Opens a raw `Complete` stream on the client, lets the server see it, and
    /// resets it with `code`. Gives the server's receiver.
    fn reset(pair: &mut Pair, code: VarInt) -> Receiver {
        let id = raw(pair.client.connection(), Dir::Uni, &[2], false);
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
    /// server, which finds a fault.
    fn misframe(pair: &mut Pair, bytes: &[u8]) {
        raw(pair.client.connection(), Dir::Uni, bytes, true);
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
    fn a_write_after_finish_panics_before_it_checks_the_size() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let endpoint = &mut pair.client.endpoint;
            endpoint.finish(now, &mut sender).expect("finished");
            let over = shard.block(&vec![1; MESSAGE_MAX + 1]);
            drop(endpoint.write(now, &mut sender, over));
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

    /// One write of `message` to `sender` on `side` that does not wait, with a
    /// message given back as bytes.
    fn try_write(
        side: &mut Side,
        now: Monotonic,
        sender: &mut Sender,
        message: Block,
    ) -> Result<Option<Vec<u8>>, Error> {
        let given = side.endpoint.try_write(now, sender, message)?;
        Ok(given.map(|message| message.to_vec()))
    }

    /// Fills the send budget of the client of a [`narrow`] pair: the first of two new
    /// streams holds part of a message, and the second all of one. Gives both.
    fn hold(pair: &mut Pair, shard: &Shard) -> [Sender; 2] {
        let mut first = open_sender(pair, Class::Complete);
        fill(pair, shard, &mut first);
        let mut second = open_sender(pair, Class::Complete);
        let (now, message) = (pair.now(), shard.block(&vec![0xb; MESSAGE_MAX]));
        let written = pair.client.endpoint.write(now, &mut second, message);
        assert_eq!(written, Ok(Poll::Pending));
        [first, second]
    }

    #[test]
    fn a_write_that_does_not_wait_gives_back_a_message_with_no_room() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [first, second] = hold(&mut pair, shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let (now, message) = (pair.now(), shard.block(b"c"));
            let address = message.as_ptr();
            let written = pair.client.endpoint.try_write(now, &mut sender, message);
            let given = written.expect("written").expect("given back");
            assert_eq!((given.as_ptr(), &*given), (address, b"c".as_slice()));
            let (seen, key) = (pair.client.events.len(), sender.key());
            let mut senders = [first, second, sender];
            let read = exchange(&mut pair, &mut senders, 10 * RUN);
            assert!(read.iter().all(|&(at, _)| at != key.id));
            assert!(senders.iter().all(|sender| !sender.holds()));
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
                if sender.holds() {
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
    fn a_write_that_does_not_wait_gives_stopped_while_part_of_the_last_waits() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            fill(&mut pair, shard, &mut sender);
            pair.run(RUN);
            let id = accept(&mut pair.server).receiver.key().id;
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            pair.run(RUN);
            assert!(sender.holds());
            let now = pair.now();
            let written =
                try_write(&mut pair.client, now, &mut sender, shard.block(b"c"));
            assert_eq!(written, Err(Error::Stopped { code: Code(7) }));
        });
    }

    #[test]
    fn a_write_that_does_not_wait_gives_back_the_message_when_the_connection_ended() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Complete);
            let (now, client) = (pair.now(), key(&pair.client));
            pair.client.endpoint.close(now, client, Code(0));
            let written =
                try_write(&mut pair.client, now, &mut sender, shard.block(b"a"));
            assert_eq!(written, Ok(Some(b"a".to_vec())));
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
            pair.server.endpoint = Endpoint::new(&config, shard_key, NonZeroUsize::MIN);
            pair.dial(tls::public(&pair::SERVER_KEY));
            pair.run(RUN);
            let mut first = open_sender(&mut pair, Class::Complete);
            let mut second = open_sender(&mut pair, Class::Latest);
            let now = pair.now();
            // The second message waits for the peer's credit, so it holds 1,472
            // bytes of the send budget.
            for byte in [1, 2] {
                let message = shard.block(&[byte; 1_472]);
                let written = pair.client.endpoint.try_write(now, &mut first, message);
                assert!(matches!(written, Ok(None)), "{written:?}");
            }
            let message = shard.block(&[3; 1_472]);
            let written = pair.client.endpoint.try_write(now, &mut second, message);
            assert_eq!(
                written.map(|back| back.map(|back| back.to_vec())),
                Ok(Some(vec![3; 1_472]))
            );
        });
    }

    #[test]
    #[should_panic(expected = "a sender is used after finish")]
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
            drop(
                pair.client
                    .endpoint
                    .try_write(now, &mut sender, shard.block(b"a")),
            );
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
                if latest[0].holds() {
                    break;
                }
            }
            let block = shard.block(&vec![0xb; MESSAGE_MAX]);
            let written = pair.client.endpoint.write(now, &mut latest[1], block);
            assert_eq!(written, Ok(Poll::Pending));
            let mut command = open_sender(&mut pair, Class::Command);
            let message = shard.block(&vec![0xc; MESSAGE_MAX]);
            let written = pair.client.endpoint.write(now, &mut command, message);
            assert_eq!(written, Ok(Poll::Pending));
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            drain(&mut pair.server, now, &mut incoming.receiver);
            pair.run(RUN);
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed = pair.client.endpoint.flush(now, &mut latest[0]);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            let written =
                try_write(&mut pair.client, now, &mut latest[0], shard.block(b"l"));
            assert_eq!(written, Ok(Some(b"l".to_vec())));
            pair.run(Duration::ZERO);
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
            let mut second = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let message = shard.block(&[0xb; 100]);
            let written = pair.client.endpoint.write(now, &mut second, message);
            assert_eq!(written, Ok(Poll::Pending));
            let mut catch_up = open_sender(&mut pair, Class::CatchUp);
            let message = shard.block(&vec![0xc; MESSAGE_MAX]);
            let written = pair.client.endpoint.write(now, &mut catch_up, message);
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
            let mut empty = open_sender(&mut pair, Class::Complete);
            let now = pair.now();
            let message = shard.block(&[]);
            let written = pair.client.endpoint.write(now, &mut empty, message);
            assert_eq!(written, Ok(Poll::Pending));
            let mut second = open_sender(&mut pair, Class::Complete);
            let mut third = open_sender(&mut pair, Class::Complete);
            for sender in [&mut second, &mut third] {
                let message = shard.block(&vec![1; MESSAGE_MAX]);
                let written = pair.client.endpoint.write(now, sender, message);
                assert_eq!(written, Ok(Poll::Pending));
            }
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            let now = pair.now();
            drain(&mut pair.server, now, &mut incoming.receiver);
            pair.run(RUN);
            let now = pair.now();
            let flushed = pair.client.endpoint.flush(now, &mut first);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            let flushed = pair.client.endpoint.flush(now, &mut empty);
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
            let mut lagging = open_sender(&mut pair, Class::Complete);
            let message = shard.block(&[2; NARROW / 32]);
            for round in 0..3 {
                let now = pair.now();
                let mut written = pair.client.endpoint.flush(now, &mut lagging);
                while written == Ok(Poll::Ready(())) {
                    let message = message.clone();
                    written = pair.client.endpoint.write(now, &mut lagging, message);
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
            let [mut first, mut second] = hold(&mut pair, shard);
            free(&mut pair);
            let now = pair.now();
            let flushed = pair.client.endpoint.flush(now, &mut second);
            assert_eq!(flushed, Ok(Poll::Pending));
            let flushed = pair.client.endpoint.flush(now, &mut first);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            let flushed = pair.client.endpoint.flush(now, &mut second);
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
            assert!(latest.holds());
        });
    }

    #[test]
    fn room_wakes_only_the_first_waiting_sender() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [mut first, mut second] = hold(&mut pair, shard);
            let seen = pair.client.events.len();
            free(&mut pair);
            let [first_writable, second_writable] =
                [&first, &second].map(|sender| Event::Writable {
                    stream: sender.key(),
                });
            assert!(got(&pair.client, seen, &first_writable));
            assert!(!got(&pair.client, seen, &second_writable));
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed = pair.client.endpoint.flush(now, &mut first);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            pair.run(Duration::ZERO);
            assert!(got(&pair.client, seen, &second_writable));
            let flushed = pair.client.endpoint.flush(now, &mut second);
            assert_eq!(flushed, Ok(Poll::Ready(())));
        });
    }

    #[test]
    fn a_waiting_command_writes_ahead_of_waiting_catch_up() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut bulk = open_sender(&mut pair, Class::CatchUp);
            fill(&mut pair, shard, &mut bulk);
            let mut command = open_sender(&mut pair, Class::Command);
            let now = pair.now();
            let written =
                pair.client
                    .endpoint
                    .write(now, &mut command, shard.block(b"go"));
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
            let flushed = pair.client.endpoint.flush(now, &mut bulk);
            assert_eq!(flushed, Ok(Poll::Pending));
            let flushed = pair.client.endpoint.flush(now, &mut command);
            assert_eq!(flushed, Ok(Poll::Ready(())));
            pair.run(Duration::ZERO);
            assert!(got(&pair.client, seen, &bulk_writable));
            let flushed = pair.client.endpoint.flush(now, &mut bulk);
            assert_eq!(flushed, Ok(Poll::Ready(())));
        });
    }

    #[test]
    fn a_reset_of_the_first_waiting_sender_wakes_the_next() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [first, second] = hold(&mut pair, shard);
            let (seen, now) = (pair.client.events.len(), pair.now());
            pair.client.endpoint.reset(now, first, Code(9));
            pair.run(Duration::ZERO);
            let writable = Event::Writable {
                stream: second.key(),
            };
            assert!(got(&pair.client, seen, &writable));
        });
    }

    #[test]
    fn a_stop_of_the_first_waiting_sender_wakes_the_next_once_the_write_fails() {
        testing::run(1, |shard| {
            let mut pair = narrow(shard);
            let [mut first, second] = hold(&mut pair, shard);
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
            assert!(!got(&pair.client, seen, &writable));
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed = pair.client.endpoint.flush(now, &mut first);
            assert_eq!(flushed, Err(Error::Stopped { code: Code(7) }));
            pair.run(Duration::ZERO);
            assert!(got(&pair.client, seen, &writable));
        });
    }

    #[test]
    fn a_stop_or_reset_of_a_later_waiting_sender_wakes_no_sender() {
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
                let written =
                    pair.client
                        .endpoint
                        .write(now, sender, shard.block(message));
                assert_eq!(written, Ok(Poll::Pending));
            }
            let stopped = pair.server.connection().recv_stream(id).stop(7u32.into());
            stopped.expect("stopped");
            pair.run(RUN);
            let (seen, now) = (pair.client.events.len(), pair.now());
            let flushed = pair.client.endpoint.flush(now, &mut second);
            assert_eq!(flushed, Err(Error::Stopped { code: Code(7) }));
            pair.client.endpoint.reset(now, third, Code(9));
            pair.run(Duration::ZERO);
            assert_eq!(pair.client.events.len(), seen, "{:?}", pair.client.events);
        });
    }

    #[test]
    fn writable_from_noq_proto_wakes_only_the_first_waiting_sender() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut first = open_sender(&mut pair, Class::Complete);
            let id = first.key().id;
            assert_eq!(writable(&mut pair, id), []);
            fill(&mut pair, shard, &mut first);
            let second = open_sender(&mut pair, Class::Complete);
            let woken = Event::Writable {
                stream: first.key(),
            };
            assert_eq!(writable(&mut pair, second.key().id), [woken]);
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

    /// The events that the client's streams give for a noq-proto `Writable` of
    /// stream `id`.
    fn writable(pair: &mut Pair, id: StreamId) -> VecDeque<Event> {
        let key = key(&pair.client);
        let connection = super::super::find(&mut pair.client.endpoint.connections, key);
        let connection = connection.expect("a connection");
        let mut events = VecDeque::new();
        let event = StreamEvent::Writable { id };
        let inner = &mut connection.inner;
        let translated = connection.streams.event(inner, key, &event, &mut events);
        translated.expect("no fault");
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
            let mut command = open_sender(&mut pair, Class::Command);
            let now = pair.now();
            let written =
                pair.client
                    .endpoint
                    .write(now, &mut command, shard.block(b"go"));
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
            let mut reply = incoming.sender.expect("a two-way stream");
            let (now, message) = (pair.now(), shard.block(b"b"));
            let written = pair.server.endpoint.write(now, &mut reply, message);
            assert_eq!(written, Err(Error::Stopped { code: Code(9) }));
        });
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
                            let flushed = client.flush(now, sender).expect("flushed");
                            *held = flushed.is_pending();
                        }
                        while !*held
                            && let Some((class, index, len)) = messages.pop_front()
                        {
                            let message = shard.block(&message(class, index, len));
                            let written = client.write(now, sender, message);
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
            let peer = tls::public(&pair::SERVER_KEY);
            foreign.dial(pair.now(), peer, pair::SERVER);
            pair.foreign = Some(foreign);
            pair.run(RUN);
            pair
        }

        /// A pair whose client dialed `foreign`, after a run.
        fn dial_foreign(shard: &Shard, foreign: Foreign) -> Pair {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            pair.foreign = Some(foreign);
            let (now, peer) = (pair.now(), tls::public(&pair::FOREIGN_KEY));
            let key = pair.client.endpoint.connect(now, peer, pair::FOREIGN);
            pair.client.key = Some(key);
            pair.run(RUN);
            pair
        }

        fn foreign(pair: &mut Pair) -> &mut noq_proto::Connection {
            pair.foreign.as_mut().expect("a foreign peer").connection()
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

        #[test]
        fn leave_ahead_of_every_stream() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                pair.dial(tls::public(&pair::SERVER_KEY));
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
                let mut reply = incoming.sender.expect("a two-way stream");
                let (now, message) = (pair.now(), shard.block(b"b"));
                let written = pair.server.endpoint.write(now, &mut reply, message);
                assert_eq!(written, Err(Error::Stopped { code: Code(9) }));
            });
        }

        #[test]
        fn leave_the_peer_streams_max_one_way_streams() {
            testing::run(1, |shard| {
                let one = |config: &mut Config| config.streams_max = NonZeroU32::MIN;
                let mut pair = Pair::with(shard, Span::SECOND, DELAY, one);
                pair.dial(tls::public(&pair::SERVER_KEY));
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
            tls::anonymous(provider, tls::public(&pair::SERVER_KEY))
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
