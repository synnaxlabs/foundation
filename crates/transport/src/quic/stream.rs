//! The streams of a connection: whole messages in order over noq-proto's streams. The
//! side that opens a stream starts it with its class byte.

use std::collections::VecDeque;
use std::ops::Range;
use std::slice;
use std::task::Poll;

use block::{Block, Pool};
use bytes::Bytes;
use noq_proto::{
    Dir, FinishError, ReadError, SendStream, StreamEvent, StreamId, VarInt, WriteError,
};

use super::{Event, connection};
use crate::message::{Prefix, Reader};
use crate::{Class, Code, Error};

/// Names one stream of a connection of an [`Endpoint`](super::Endpoint).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub(super) connection: connection::Key,
    pub(super) quic: StreamId,
}

/// The sending half of a stream. The caller gives it to each write call. Dropping it
/// does nothing to the stream.
#[derive(Debug)]
pub(crate) struct Sender {
    key: Key,
    /// The class, until a message or the finish carries its byte.
    class: Option<Class>,
    /// The class byte and length prefix of the message in hand.
    header: [u8; 9],
    /// The bytes of `header` that the stream has not taken.
    unsent: Range<usize>,
    /// The bytes of the message in hand that the stream has not taken.
    body: Bytes,
    finished: bool,
    /// The code that the peer stopped the stream with.
    stopped: Option<Code>,
}

/// The receiving half of a stream. The caller gives it to each read. Dropping it does
/// nothing to the stream.
#[derive(Debug)]
pub(crate) struct Receiver {
    key: Key,
    reader: Reader,
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
#[derive(Debug, Default)]
pub(super) struct Streams {
    /// Streams the peer opened whose class byte has not arrived.
    unclassified: Vec<StreamId>,
    /// Streams the peer opened that the caller has not accepted, by class byte,
    /// oldest first.
    incoming: [VecDeque<StreamId>; 4],
    /// Empty streams that this side finished before the stream took the class byte.
    finishing: Vec<(StreamId, Class)>,
}

/// A message body that the stream holds until the peer acknowledges it.
struct Body(Block);

impl AsRef<[u8]> for Body {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Sender {
    /// A sender for `key` that starts the stream with `class`'s byte, or with no class
    /// byte when `class` is `None`.
    pub(super) fn new(key: Key, class: Option<Class>) -> Self {
        Self {
            key,
            class,
            header: [0; 9],
            unsent: 0..0,
            body: Bytes::new(),
            finished: false,
            stopped: None,
        }
    }

    /// The stream this sender writes.
    pub(crate) fn key(&self) -> Key {
        self.key
    }

    /// Takes `message` as the message in hand, after its header.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream.
    ///
    /// # Panics
    ///
    /// When the sender holds part of a message, or after `finish`.
    pub(super) fn load(&mut self, message: Block) -> Result<(), Error> {
        self.check();
        if let Some(code) = self.stopped {
            return Err(Error::Stopped { code });
        }
        let class = self.class.take().map(Class::byte);
        let prefix = Prefix::new(message.len());
        let start = usize::from(class.is_none());
        self.header[0] = class.unwrap_or(0);
        self.header[1..=prefix.len()].copy_from_slice(&prefix);
        self.unsent = start..prefix.len() + 1;
        self.body = Bytes::from_owner(Body(message));
        Ok(())
    }

    /// Marks the stream finished, and gives the class whose byte it still needs.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream.
    ///
    /// # Panics
    ///
    /// When the sender holds part of a message, or after `finish`.
    pub(super) fn end(&mut self) -> Result<Option<Class>, Error> {
        self.check();
        self.finished = true;
        match self.stopped {
            Some(code) => Err(Error::Stopped { code }),
            None => Ok(self.class.take()),
        }
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

impl Streams {
    /// What `event` of `inner`, the connection of `key`, means to the caller, if
    /// anything.
    ///
    /// # Errors
    ///
    /// [`Error::Broken`] when the peer broke the stream protocol.
    pub(super) fn event(
        &mut self,
        inner: &mut noq_proto::Connection,
        key: connection::Key,
        event: &StreamEvent,
    ) -> Result<Option<Event>, Error> {
        let stream = |quic| Key {
            connection: key,
            quic,
        };
        match *event {
            StreamEvent::Opened { dir } => {
                let mut queued = false;
                while let Some(id) = inner.streams().accept(dir) {
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
            StreamEvent::Writable { id } | StreamEvent::Stopped { id, .. } => {
                let Some(at) =
                    self.finishing.iter().position(|&(other, _)| other == id)
                else {
                    return Ok(Some(Event::Writable { stream: stream(id) }));
                };
                let (_, class) = self.finishing.swap_remove(at);
                match self.finish(inner, id, Some(class)) {
                    Err(error @ Error::Broken { .. }) => Err(error),
                    _ => Ok(None),
                }
            }
            StreamEvent::Available { .. } => Ok(Some(Event::Available { key })),
            StreamEvent::Finished { .. } => Ok(None),
        }
    }

    /// The next stream the peer opened, highest class first.
    pub(super) fn accept(
        &mut self,
        connection: connection::Key,
        bytes_max: usize,
    ) -> Option<Incoming> {
        let (class, quic) = Class::ALL
            .into_iter()
            .zip(&mut self.incoming)
            .find_map(|(class, queue)| Some((class, queue.pop_front()?)))?;
        let key = Key { connection, quic };
        Some(Incoming {
            class,
            receiver: Receiver::new(key, bytes_max),
            sender: (quic.dir() == Dir::Bi).then(|| Sender::new(key, None)),
        })
    }

    /// Whether a stream waits for [`Streams::accept`].
    pub(super) fn queued(&self) -> bool {
        self.incoming.iter().any(|queue| !queue.is_empty())
    }

    /// Ends stream `id` of `inner` after what it holds. With `class`, the stream is
    /// empty and its class byte goes first, now or at the stream's next event.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream, and [`Error::Broken`] when
    /// it did so with a code over 32 bits.
    pub(super) fn finish(
        &mut self,
        inner: &mut noq_proto::Connection,
        id: StreamId,
        class: Option<Class>,
    ) -> Result<(), Error> {
        let mut send = inner.send_stream(id);
        if let Some(class) = class {
            match send.write(&[class.byte()]) {
                Ok(_) => {}
                Err(WriteError::Blocked) => {
                    self.finishing.push((id, class));
                    return Ok(());
                }
                Err(WriteError::Stopped(code)) => return Err(stopped(&mut send, code)),
                Err(WriteError::ClosedStream) => panic!("{OPEN}"),
            }
        }
        match send.finish() {
            Ok(()) => Ok(()),
            Err(FinishError::Stopped(code)) => Err(stopped(&mut send, code)),
            Err(FinishError::ClosedStream) => panic!("{OPEN}"),
        }
    }

    /// Reads the class byte of new stream `id`. Returns whether the stream is now
    /// queued for [`Streams::accept`]. A stream reset before its class byte drops.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a stream noq-proto just gave is open and gives no empty chunk"
    )]
    fn classify(
        &mut self,
        inner: &mut noq_proto::Connection,
        id: StreamId,
    ) -> Result<bool, Error> {
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
                if Class::from_byte(byte).is_none() {
                    return Err(broken(format!("a stream of class {byte}")));
                }
                self.incoming[usize::from(byte)].push_back(id);
                Ok(true)
            }
            Ok(None) => Err(broken("a stream ended before its class".to_owned())),
            Err(ReadError::Blocked) => {
                self.unclassified.push(id);
                Ok(false)
            }
            Err(ReadError::Reset(_)) => Ok(false),
        }
    }
}

const OPEN: &str = "invariant: a sender's stream is open until it finishes";

/// Writes what `sender` holds to `inner`. `Ready` when the stream took all of it.
///
/// # Errors
///
/// [`Error::Stopped`] when the peer stopped the stream, and [`Error::Broken`] when it
/// did so with a code over 32 bits.
pub(super) fn flush(
    inner: &mut noq_proto::Connection,
    sender: &mut Sender,
) -> Result<Poll<()>, Error> {
    if let Some(code) = sender.stopped {
        return Err(Error::Stopped { code });
    }
    let mut send = inner.send_stream(sender.key.quic);
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
        match written {
            Ok(()) => {}
            Err(WriteError::Blocked) => return Ok(Poll::Pending),
            Err(WriteError::Stopped(code)) => {
                (sender.unsent, sender.body) = (0..0, Bytes::new());
                let error = stopped(&mut send, code);
                if let Error::Stopped { code } = error {
                    sender.stopped = Some(code);
                }
                return Err(error);
            }
            Err(WriteError::ClosedStream) => panic!("{OPEN}"),
        }
    }
}

/// Reads the next whole message of `receiver`'s stream from `inner` into a block from
/// `pool`. `Ready(None)` at the end.
///
/// # Errors
///
/// [`Error::Reset`] when the peer reset the stream, [`Error::Pool`] when `pool` has no
/// room now, and [`Error::Broken`] when the peer broke the framing or reset with a
/// code over 32 bits.
#[expect(
    clippy::unwrap_in_result,
    reason = "a receiver never reads its stream after the end"
)]
pub(super) fn read(
    inner: &mut noq_proto::Connection,
    receiver: &mut Receiver,
    pool: &Pool,
) -> Result<Poll<Option<Block>>, Error> {
    let mut recv = inner.recv_stream(receiver.key.quic);
    let mut chunks = recv
        .read(true)
        .expect("invariant: a receiver's stream is open until it ends");
    let result = receiver.reader.read(pool, |max| match chunks.next(max) {
        Ok(chunk) => Ok(Poll::Ready(chunk.map(|chunk| chunk.bytes))),
        Err(ReadError::Blocked) => Ok(Poll::Pending),
        Err(ReadError::Reset(code)) => Err(match u32::try_from(code.into_inner()) {
            Ok(code) => Error::Reset { code: Code(code) },
            Err(_) => broken(format!("a reset code over 32 bits: {code}")),
        }),
    });
    receiver.end = match result {
        Ok(Poll::Ready(None)) => Some(End::Finished),
        Err(Error::Reset { code }) => Some(End::Reset(code)),
        _ => None,
    };
    result
}

/// Resets a stream that the peer stopped with `code`, with the same code, and gives
/// the error for it.
fn stopped(send: &mut SendStream<'_>, code: VarInt) -> Error {
    let Ok(code) = u32::try_from(code.into_inner()) else {
        return broken(format!("a stop code over 32 bits: {code}"));
    };
    send.reset(VarInt::from_u32(code))
        .expect("invariant: a stopped stream is open");
    Error::Stopped { code: Code(code) }
}

fn broken(reason: String) -> Error {
    Error::Broken { reason }
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
                pair.client.endpoint.finish(now, sender).expect("finished");
            }
            pair.run(RUN);
            let server = self::key(&pair.server);
            while let Some(mut incoming) = pair.server.endpoint.accept(server) {
                let now = pair.now();
                assert_eq!(
                    drain(&mut pair.server, now, &mut incoming.receiver),
                    (vec![], true)
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
    fn that_are_empty_arrive_with_their_class_and_end() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut sender = open_sender(&mut pair, Class::Latest);
            let now = pair.now();
            pair.client
                .endpoint
                .finish(now, &mut sender)
                .expect("finished");
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            assert_eq!(incoming.class, Class::Latest);
            let now = pair.now();
            assert_eq!(
                drain(&mut pair.server, now, &mut incoming.receiver),
                (vec![], true)
            );
            let again = next(&mut pair.server, now, &mut incoming.receiver);
            assert_eq!(again, Ok(Poll::Ready(None)));
        });
    }

    #[test]
    fn that_are_empty_go_out_when_the_window_opens_after_the_sender_drops() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let mut bulk = open_sender(&mut pair, Class::CatchUp);
            fill(&mut pair, shard, &mut bulk);
            let now = pair.now();
            let mut empty = open_sender(&mut pair, Class::Command);
            pair.client
                .endpoint
                .finish(now, &mut empty)
                .expect("finished");
            drop(empty);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            assert_eq!(incoming.class, Class::CatchUp);
            let server = key(&pair.server);
            assert!(pair.server.endpoint.accept(server).is_none());
            let now = pair.now();
            drain(&mut pair.server, now, &mut incoming.receiver);
            pair.run(RUN);
            let mut incoming = accept(&mut pair.server);
            assert_eq!(incoming.class, Class::Command);
            let now = pair.now();
            assert_eq!(
                drain(&mut pair.server, now, &mut incoming.receiver),
                (vec![], true)
            );
        });
    }

    #[test]
    fn reset_before_the_class_byte_drop_with_no_event() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let id = raw(&mut pair.client, Dir::Uni, &[], false);
            let reset = pair
                .client
                .connection()
                .send_stream(id)
                .reset(VarInt::from_u32(7));
            reset.expect("reset");
            pair.run(RUN);
            assert_eq!(pair.server.events.len(), 1, "{:?}", pair.server.events);
            assert!(pair.server.endpoint.accept(key(&pair.server)).is_none());
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
        let quic = accept(&mut pair.server).receiver.key().quic;
        let stopped = pair.server.connection().recv_stream(quic).stop(code);
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
    fn stopped_with_a_code_over_32_bits_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            let code = VarInt::from_u64(1 << 32).expect("a varint");
            let mut sender = stop(&mut pair, shard, code);
            let now = pair.now();
            let written =
                pair.client
                    .endpoint
                    .write(now, &mut sender, shard.block(b"b"));
            assert_eq!(written, Ok(Poll::Pending));
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
    fn that_end_before_the_class_byte_break_the_connection() {
        testing::run(1, |shard| {
            let mut pair = connected(shard);
            raw(&mut pair.client, Dir::Uni, &[], true);
            assert_broken(&mut pair, true, "a stream ended before its class");
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
