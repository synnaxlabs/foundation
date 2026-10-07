//! The two halves of a stream: an ordered, reliable sequence of whole messages.

use std::future::poll_fn;
use std::ops::Range;
use std::rc::Rc;

use block::Block;

use crate::class::Class;
use crate::code::Code;
use crate::error::Error;
use crate::quic;

/// Bytes of a block to send, then zeros.
///
/// ```
/// use transport::stream::Part;
///
/// // A series of 5 bytes at offset 64, padded to 8.
/// let series = Part { range: 64..69, zeros: 3 };
/// assert_eq!(series.range.len() + usize::from(series.zeros), 8);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// The bytes of the block to send.
    pub range: Range<usize>,
    /// The zero bytes to send after them.
    pub zeros: u8,
}

/// The sending half of a stream. Dropping it without [`finish`](Self::finish) resets
/// the stream with `Code(0)`, so the peer never reads a cut-off stream as complete.
/// Dropping it after `finish` lets delivery go on.
///
/// ```
/// use block::Block;
/// use transport::{Error, stream::Sender};
///
/// async fn send_all(mut sender: Sender, frames: Vec<Block>) -> Result<(), Error> {
///     for frame in frames {
///         sender.send(frame).await?;
///     }
///     sender.finish()
/// }
/// ```
#[derive(Debug)]
pub struct Sender {
    session: Rc<quic::Session>,
    class: Class,
    stream: quic::stream::Sender,
}

impl Sender {
    pub(crate) fn new(
        session: Rc<quic::Session>,
        class: Class,
        stream: quic::stream::Sender,
    ) -> Self {
        Self {
            session,
            class,
            stream,
        }
    }

    /// The stream's traffic class.
    ///
    /// ```
    /// fn class(sender: &transport::stream::Sender) -> transport::Class {
    ///     sender.class()
    /// }
    /// ```
    #[must_use]
    pub fn class(&self) -> Class {
        self.class
    }

    /// The largest message the peer takes: its `message_bytes_max`. It does not
    /// change during the session. A message over it gives [`Error::TooLarge`].
    ///
    /// ```
    /// fn fits(sender: &transport::stream::Sender, frame: &block::Block) -> bool {
    ///     frame.len() <= sender.bytes_max()
    /// }
    /// ```
    #[must_use]
    pub fn bytes_max(&self) -> usize {
        self.stream.bytes_max()
    }

    /// Sends `message` whole. It waits while the peer's flow control has no room,
    /// and returns once the stream holds the message, not when the peer has it. If the
    /// future drops after the stream sent a byte of the message and before it
    /// completes, the stream resets with `Code(0)`, because the peer would get part of
    /// the message. If it drops before that, nothing is sent and the stream stays
    /// open.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over the peer's
    /// [`Config::message_bytes_max`](crate::Config::message_bytes_max),
    /// [`Error::Stopped`] when the peer stopped reading, [`Error::Reset`] with
    /// `Code(0)` after a dropped send future reset the stream, or the error that
    /// ended the session.
    ///
    /// # Panics
    ///
    /// When called after [`finish`](Self::finish).
    ///
    /// ```
    /// use transport::{Error, stream::Sender};
    ///
    /// async fn send(sender: &mut Sender, frame: block::Block) -> Result<(), Error> {
    ///     sender.send(frame).await
    /// }
    /// ```
    pub async fn send(&mut self, message: Block) -> Result<(), Error> {
        let mut sending = Sending {
            session: &self.session,
            stream: &self.stream,
            message: Some(message),
            done: false,
        };
        let sent = poll_fn(|cx| {
            let Sending {
                session,
                stream,
                message,
                ..
            } = &mut sending;
            session.poll_write(cx, stream, message)
        })
        .await;
        sending.done = true;
        sent
    }

    /// Sends `message` whole when the stream can take it now, and never waits. Gives
    /// `message` back, with nothing of it sent, when the stream cannot take it now:
    /// the messages that the session's streams hold and have not passed to the
    /// carrier leave no room for it within the peer's
    /// [`Config::window_bytes`](crate::Config::window_bytes), a stream that goes
    /// ahead of it waits for that room or for the carrier to take more, or the
    /// stream still holds part of an earlier message. Like [`send`](Self::send), it
    /// returns once the stream holds the message, not when the peer has it. It never
    /// resets the stream.
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send): [`Error::TooLarge`], [`Error::Stopped`],
    /// [`Error::Reset`], or the error that ended the session.
    ///
    /// # Panics
    ///
    /// When called after [`finish`](Self::finish).
    ///
    /// ```
    /// use transport::{Error, stream::Sender};
    ///
    /// fn live(sender: &mut Sender, frame: block::Block) -> Result<(), Error> {
    ///     if let Some(frame) = sender.try_send(frame)? {
    ///         drop(frame); // `hub` adds it to the pending gap.
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub fn try_send(&mut self, message: Block) -> Result<Option<Block>, Error> {
        self.session.try_write(&self.stream, message)
    }

    /// Sends one message: for each of `parts`, in order, the bytes of its range of
    /// `block`, then its zeros. The stream holds `block` until the carrier takes the
    /// message, and copies no byte of it before then. It never sends a byte of
    /// `block` outside the ranges. Waits, returns, and resets on drop as
    /// [`send`](Self::send).
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send). [`Error::TooLarge`] when the sum of the range lengths
    /// and the zeros is over the peer's message limit.
    ///
    /// # Panics
    ///
    /// When called after [`finish`](Self::finish), or when a range starts after its
    /// end or ends past the block.
    ///
    /// ```
    /// use transport::Error;
    /// use transport::stream::{Part, Sender};
    ///
    /// async fn series(sender: &mut Sender, frame: block::Block) -> Result<(), Error> {
    ///     let parts = [
    ///         Part { range: 0..8, zeros: 0 },
    ///         Part { range: 64..69, zeros: 3 },
    ///     ];
    ///     sender.send_parts(frame, &parts).await
    /// }
    /// ```
    pub async fn send_parts(
        &mut self,
        block: Block,
        parts: &[Part],
    ) -> Result<(), Error> {
        drop((block, parts));
        todo!("#68")
    }

    /// [`send_parts`](Self::send_parts) when the stream can take the message now, as
    /// [`try_send`](Self::try_send): gives `block` back, with nothing sent, when it
    /// cannot.
    ///
    /// # Errors
    ///
    /// As [`send_parts`](Self::send_parts).
    ///
    /// # Panics
    ///
    /// As [`send_parts`](Self::send_parts).
    ///
    /// ```
    /// use block::Block;
    /// use transport::Error;
    /// use transport::stream::{Part, Sender};
    ///
    /// fn live(sender: &mut Sender, frame: Block) -> Result<Option<Block>, Error> {
    ///     sender.try_send_parts(frame, &[Part { range: 0..8, zeros: 0 }])
    /// }
    /// ```
    pub fn try_send_parts(
        &mut self,
        block: Block,
        parts: &[Part],
    ) -> Result<Option<Block>, Error> {
        drop((block, parts));
        todo!("#68")
    }

    /// Ends the stream after the messages already sent. The peer's
    /// [`Receiver::recv`] returns `None` after the last one. The sender stays, so
    /// [`reset`](Self::reset) can still cancel what the peer does not have yet.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped reading, [`Error::Reset`] with
    /// `Code(0)` after a dropped send future reset the stream, or the error that
    /// ended the session.
    ///
    /// ```
    /// use transport::{Error, stream::Sender};
    ///
    /// fn done(sender: &mut Sender) -> Result<(), Error> {
    ///     sender.finish()
    /// }
    /// ```
    pub fn finish(&mut self) -> Result<(), Error> {
        self.session.finish(&mut self.stream)
    }

    /// Cancels the stream: messages the peer does not have yet drop, and the peer
    /// sees [`Error::Reset`] with `code`. After [`finish`](Self::finish), it does
    /// nothing once the peer has every message.
    ///
    /// ```
    /// fn cancel(sender: transport::stream::Sender) {
    ///     sender.reset(transport::Code(16));
    /// }
    /// ```
    pub fn reset(mut self, code: Code) {
        self.session.reset(&mut self.stream, code);
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        if !self.stream.ended() {
            self.session.reset(&mut self.stream, Code(0));
        }
    }
}

/// A [`Sender::send`] in progress. Dropping it before it is done cancels the message
/// once the stream took it.
struct Sending<'a> {
    session: &'a quic::Session,
    stream: &'a quic::stream::Sender,
    /// `None` once the stream took it.
    message: Option<Block>,
    done: bool,
}

impl Drop for Sending<'_> {
    fn drop(&mut self) {
        if !self.done && self.message.is_none() {
            self.session.cancel(self.stream);
        }
    }
}

/// The receiving half of a stream. Dropping it before the end stops the stream with
/// `Code(0)`; dropping it after the end sends nothing. Messages that arrived and are
/// not received hold the session's window, so a receiver that lags holds back every
/// stream of the session from the peer.
///
/// ```
/// use transport::{Error, stream::Receiver};
///
/// async fn drain(mut receiver: Receiver) -> Result<usize, Error> {
///     let mut count = 0;
///     while receiver.recv().await?.is_some() {
///         count += 1;
///     }
///     Ok(count)
/// }
/// ```
#[derive(Debug)]
pub struct Receiver {
    session: Rc<quic::Session>,
    /// `None` only once it stops.
    stream: Option<quic::stream::Receiver>,
}

impl Receiver {
    pub(crate) fn new(
        session: Rc<quic::Session>,
        stream: quic::stream::Receiver,
    ) -> Self {
        Self {
            session,
            stream: Some(stream),
        }
    }

    /// Waits for the next whole message. It lands in one block from the shard's pool.
    /// Returns `None` once the sender finished and every message has arrived.
    ///
    /// It also waits while the pool has no block for the message. The message stays
    /// queued and flow control holds the peer. The reads of one transport that wait
    /// take blocks highest class first, then oldest first. Drop the future to end the
    /// wait.
    ///
    /// # Errors
    ///
    /// [`Error::Reset`] when the sender cancelled the stream, or the error that ended
    /// the session.
    ///
    /// ```
    /// use block::Block;
    /// use transport::{Error, stream::Receiver};
    ///
    /// async fn next(receiver: &mut Receiver) -> Result<Option<Block>, Error> {
    ///     receiver.recv().await
    /// }
    /// ```
    #[expect(
        clippy::missing_panics_doc,
        reason = "a receiver holds its stream until `stop` takes the receiver"
    )]
    pub async fn recv(&mut self) -> Result<Option<Block>, Error> {
        let stream = self.stream.as_mut();
        let mut receiving = Receiving {
            session: &self.session,
            stream: stream
                .expect("invariant: a receiver holds its stream until it stops"),
            done: false,
        };
        let received =
            poll_fn(|cx| receiving.session.poll_read(cx, receiving.stream)).await;
        receiving.done = true;
        received
    }

    /// Waits for the next whole message and writes it to the start of `buffer`.
    /// Gives its length, or `None` once the sender finished and every message has
    /// arrived. It uses the same receive budget as [`recv`](Self::recv). If the
    /// future drops before it gives the length, the message stays queued and `buffer`
    /// may hold part of it.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] with the message length and `buffer.len()` when the
    /// message is longer than `buffer`; the message stays queued. [`Error::Reset`]
    /// when the sender cancelled the stream, or the error that ended the session.
    ///
    /// ```
    /// use transport::{Error, stream::Receiver};
    ///
    /// async fn drain(receiver: &mut Receiver, draft: &mut [u8]) -> Result<(), Error> {
    ///     while let Some(len) = receiver.recv_into(draft).await? {
    ///         let _body = &draft[..len];
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub async fn recv_into(
        &mut self,
        buffer: &mut [u8],
    ) -> Result<Option<usize>, Error> {
        let _ = buffer;
        todo!("#68")
    }

    /// Asks the sender to stop: messages not yet received drop, and the sender sees
    /// [`Error::Stopped`] with `code`.
    ///
    /// ```
    /// fn hang_up(receiver: transport::stream::Receiver) {
    ///     receiver.stop(transport::Code(16));
    /// }
    /// ```
    pub fn stop(mut self, code: Code) {
        if let Some(stream) = self.stream.take() {
            self.session.stop(stream, code);
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            self.session.stop(stream, Code(0));
        }
    }
}

/// A [`Receiver::recv`] in progress. Dropping it before it is done gives up its wait
/// for room in the receive budget, so the room goes to the next read.
struct Receiving<'a> {
    session: &'a quic::Session,
    stream: &'a mut quic::stream::Receiver,
    done: bool,
}

impl Drop for Receiving<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.session.end_wait(self.stream);
        }
    }
}

/// A stream the peer opened. It has a sender when the peer opened it in both
/// directions with [`Session::open`](crate::Session::open), and none when it used
/// [`Session::open_sender`](crate::Session::open_sender). The sender has the same
/// class.
///
/// ```
/// use transport::stream::Incoming;
///
/// fn answerable(incoming: &Incoming) -> bool {
///     incoming.sender.is_some()
/// }
/// ```
#[derive(Debug)]
pub struct Incoming {
    /// The class the peer opened it with.
    pub class: Class,
    /// Messages from the peer.
    pub receiver: Receiver,
    /// Messages to the peer, when the stream goes both ways.
    pub sender: Option<Sender>,
}

impl Incoming {
    pub(crate) fn new(
        session: &Rc<quic::Session>,
        incoming: quic::stream::Incoming,
    ) -> Self {
        let quic::stream::Incoming {
            class,
            receiver,
            sender,
        } = incoming;
        Self {
            class,
            receiver: Receiver::new(Rc::clone(session), receiver),
            sender: sender.map(|sender| Sender::new(Rc::clone(session), class, sender)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::task::{Context, Waker};

    use std::future::poll_fn;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::Poll;

    use block::Block;
    use block::testing::Scarce;
    use sim::Sim;
    use sim::node::Node;
    use types::time::Span;

    use crate::testing::{self, IDLE, MESSAGE_BYTES_MAX, poll_once, spans};
    use crate::{Address, Class, Code, Config, Error, Transport};

    /// The messages of [`lossy`].
    const COUNT: u32 = 1000;

    const CANCELLED: Error = Error::Reset { code: Code(0) };

    fn same(config: Config) -> Config {
        config
    }

    fn bytes(read: Result<Option<Block>, Error>) -> Result<Option<Vec<u8>>, Error> {
        read.map(|message| message.map(|block| block.to_vec()))
    }

    /// Reads `receiver` until it errs, and gives the error.
    async fn until_error(receiver: &mut super::Receiver) -> Error {
        loop {
            match receiver.recv().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the stream finished"),
                Err(error) => return error,
            }
        }
    }

    #[test]
    fn a_stream_carries_whole_messages_both_ways_in_order() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open(Class::Command).await;
                let (mut sender, mut receiver) = opened.expect("a stream");
                assert_eq!(sender.class(), Class::Command);
                for message in [b"a".as_slice(), b"bc", b"def"] {
                    sender.send(side.block(message)).await.expect("sent");
                }
                sender.finish().expect("finished");
                assert_eq!(bytes(receiver.recv().await), Ok(Some(b"3".to_vec())));
                assert_eq!(bytes(receiver.recv().await), Ok(None));
            },
            |side| async move {
                let incoming = side.session.accept().await.expect("a stream");
                assert_eq!(incoming.class, Class::Command);
                let mut receiver = incoming.receiver;
                let mut sender = incoming.sender.expect("a reply half");
                assert_eq!(sender.class(), Class::Command);
                let mut read = Vec::new();
                while let Some(message) = receiver.recv().await.expect("a message") {
                    read.push(message.to_vec());
                }
                assert_eq!(read, [b"a".to_vec(), b"bc".to_vec(), b"def".to_vec()]);
                sender.send(side.block(b"3")).await.expect("sent");
                sender.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(0) };
                assert_eq!(side.session.closed().await, closed);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_one_way_stream_has_no_reply_half() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Latest).await;
                let mut sender = opened.expect("a stream");
                assert_eq!(sender.class(), Class::Latest);
                sender.send(side.block(b"a")).await.expect("sent");
                sender.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(3) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let mut incoming = side.session.accept().await.expect("a stream");
                assert_eq!(incoming.class, Class::Latest);
                assert!(incoming.sender.is_none());
                let read = incoming.receiver.recv().await;
                assert_eq!(bytes(read), Ok(Some(b"a".to_vec())));
                assert_eq!(bytes(incoming.receiver.recv().await), Ok(None));
                side.session.close(Code(3));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    /// A run from `value` in which the client sends [`COUNT`] numbered messages of
    /// 1,000 bytes on one stream over links that lose, duplicate, and reorder
    /// datagrams. The server checks that each arrives once and in order, and counts
    /// them in the counter it gives.
    fn lossy(value: u64) -> (Sim, Node, Node, Arc<AtomicU32>) {
        let read = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&read);
        let (mut sim, client, server) = testing::sessions(
            value,
            same,
            |side| async move {
                let opened = side.session.open(Class::Complete).await;
                let (mut sender, mut receiver) = opened.expect("a stream");
                for n in 0..COUNT {
                    let mut message = vec![0; 1000];
                    message[..4].copy_from_slice(&n.to_le_bytes());
                    sender.send(side.block(&message)).await.expect("sent");
                }
                sender.finish().expect("finished");
                let count = COUNT.to_le_bytes().to_vec();
                assert_eq!(bytes(receiver.recv().await), Ok(Some(count)));
            },
            |side| async move {
                let incoming = side.session.accept().await.expect("a stream");
                let (mut receiver, mut n) = (incoming.receiver, 0_u32);
                while let Some(message) = receiver.recv().await.expect("a message") {
                    assert_eq!(message[..4], n.to_le_bytes());
                    n += 1;
                    counter.store(n, Ordering::Relaxed);
                }
                let mut sender = incoming.sender.expect("a reply half");
                sender
                    .send(side.block(&n.to_le_bytes()))
                    .await
                    .expect("sent");
                sender.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(0) };
                assert_eq!(side.session.closed().await, closed);
            },
        );
        let lossy = sim::link::Config {
            delay: spans(Span::MILLISECOND, 10),
            jitter: spans(Span::MILLISECOND, 10),
            loss: 0.1,
            duplication: 0.05,
            ..sim::link::Config::default()
        };
        sim.link(&client, &server, lossy);
        sim.link(&server, &client, lossy);
        (sim, client, server, read)
    }

    #[test]
    fn messages_stay_whole_and_in_order_through_loss_reordering_and_a_partition() {
        let (mut sim, client, server, read) = lossy(1);
        while read.load(Ordering::Relaxed) < COUNT / 3 {
            assert_eq!(sim.run_for(Span::MILLISECOND), Ok(()));
        }
        let cut = sim::link::Config {
            loss: 1.0,
            ..sim::link::Config::default()
        };
        sim.link(&client, &server, cut);
        sim.link(&server, &client, cut);
        // Past the delay and jitter of the datagrams in flight.
        assert_eq!(sim.run_for(spans(Span::MILLISECOND, 50)), Ok(()));
        let before = read.load(Ordering::Relaxed);
        assert_eq!(sim.run_for(spans(Span::MILLISECOND, 450)), Ok(()));
        assert_eq!(read.load(Ordering::Relaxed), before);
        sim.link(&client, &server, sim::link::Config::default());
        sim.link(&server, &client, sim::link::Config::default());
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn the_same_value_gives_the_same_trace() {
        // The last close can be lost, so the run may fail, the same way each time.
        let trace = |value| {
            let (mut sim, ..) = lossy(value);
            (sim.run(), sim.digest())
        };
        assert_eq!(trace(2), trace(2));
    }

    #[test]
    fn a_sender_dropped_before_finish_resets_and_one_dropped_after_delivers() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open(Class::Complete).await;
                let (mut sender, mut receiver) = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                assert_eq!(bytes(receiver.recv().await), Ok(Some(b"b".to_vec())));
                drop(sender);
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.send(side.block(b"c")).await.expect("sent");
                sender.finish().expect("finished");
                drop(sender);
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let incoming = side.session.accept().await.expect("a stream");
                let (mut receiver, mut reply) = (incoming.receiver, incoming.sender);
                let read = receiver.recv().await;
                assert_eq!(bytes(read), Ok(Some(b"a".to_vec())));
                let reply = reply.as_mut().expect("a reply half");
                reply.send(side.block(b"b")).await.expect("sent");
                assert_eq!(until_error(&mut receiver).await, CANCELLED);
                let mut incoming = side.session.accept().await.expect("a stream");
                let read = incoming.receiver.recv().await;
                assert_eq!(bytes(read), Ok(Some(b"c".to_vec())));
                assert_eq!(bytes(incoming.receiver.recv().await), Ok(None));
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_receiver_dropped_or_stopped_before_the_end_stops_the_sender() {
        for code in [None, Some(Code(16))] {
            let (mut sim, ..) = testing::sessions(
                0,
                same,
                move |side| async move {
                    let opened = side.session.open_sender(Class::Complete).await;
                    let mut sender = opened.expect("a stream");
                    sender.send(side.block(b"a")).await.expect("sent");
                    let mut sent = Ok(());
                    for _ in 0..100 {
                        side.node.clock().sleep(Span::MILLISECOND).await;
                        sent = sender.send(side.block(b"b")).await;
                        if sent.is_err() {
                            break;
                        }
                    }
                    let code = code.unwrap_or(Code(0));
                    assert_eq!(sent, Err(Error::Stopped { code }), "{code:?}");
                },
                move |side| async move {
                    let mut incoming = side.session.accept().await.expect("a stream");
                    let read = incoming.receiver.recv().await;
                    assert_eq!(bytes(read), Ok(Some(b"a".to_vec())));
                    match code {
                        Some(code) => incoming.receiver.stop(code),
                        None => drop(incoming.receiver),
                    }
                    let closed = Error::PeerClosed { code: Code(0) };
                    assert_eq!(side.session.closed().await, closed);
                },
            );
            assert_eq!(sim.run(), Ok(()));
        }
    }

    #[test]
    fn a_reset_before_or_after_finish_gives_the_peer_its_code() {
        for finished in [false, true] {
            let (mut sim, ..) = testing::sessions(
                0,
                same,
                move |side| async move {
                    let opened = side.session.open(Class::Complete).await;
                    let (mut sender, mut receiver) = opened.expect("a stream");
                    sender.send(side.block(b"a")).await.expect("sent");
                    assert_eq!(bytes(receiver.recv().await), Ok(Some(b"b".to_vec())));
                    // Too large for one flight, so the peer cannot have it all yet.
                    let message = vec![7; 32 << 10];
                    sender.send(side.block(&message)).await.expect("sent");
                    if finished {
                        sender.finish().expect("finished");
                    }
                    sender.reset(Code(16));
                    let closed = Error::PeerClosed { code: Code(4) };
                    assert_eq!(side.session.closed().await, closed);
                },
                move |side| async move {
                    let mut incoming = side.session.accept().await.expect("a stream");
                    let read = incoming.receiver.recv().await;
                    assert_eq!(bytes(read), Ok(Some(b"a".to_vec())));
                    let reply = incoming.sender.as_mut().expect("a reply half");
                    reply.send(side.block(b"b")).await.expect("sent");
                    let error = until_error(&mut incoming.receiver).await;
                    let reset = Error::Reset { code: Code(16) };
                    assert_eq!(error, reset, "finished: {finished}");
                    side.session.close(Code(4));
                },
            );
            assert_eq!(sim.run(), Ok(()));
        }
    }

    #[test]
    fn a_dropped_send_resets_the_stream_and_each_later_call_gives_why() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open(Class::Complete).await;
                let (mut sender, mut receiver) = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                assert_eq!(bytes(receiver.recv().await), Ok(Some(b"b".to_vec())));
                let body = vec![7; 60_000];
                let mut sent = 0;
                loop {
                    match poll_once(pin!(sender.send(side.block(&body)))).await {
                        Some(done) => done.expect("sent"),
                        None => break,
                    }
                    sent += 1;
                }
                // The peer's window holds at most 17 such messages.
                assert!((1..=17).contains(&sent), "{sent}");
                assert_eq!(sender.send(side.block(b"a")).await, Err(CANCELLED));
                let tried = sender.try_send(side.block(b"a")).map(|_| ());
                assert_eq!(tried, Err(CANCELLED));
                assert_eq!(sender.finish(), Err(CANCELLED));
                assert_eq!(sender.bytes_max(), MESSAGE_BYTES_MAX);
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let mut incoming = side.session.accept().await.expect("a stream");
                let reply = incoming.sender.as_mut().expect("a reply half");
                reply.send(side.block(b"b")).await.expect("sent");
                let reply = incoming.sender.as_mut().expect("a reply half");
                reply.send(side.block(b"b")).await.expect("sent");
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                assert_eq!(until_error(&mut incoming.receiver).await, CANCELLED);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_send_future_dropped_before_its_first_poll_sends_nothing() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                drop(sender.send(side.block(b"b")));
                sender.send(side.block(b"c")).await.expect("sent");
                sender.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let mut incoming = side.session.accept().await.expect("a stream");
                for message in [b"a", b"c"] {
                    let read = incoming.receiver.recv().await;
                    assert_eq!(bytes(read), Ok(Some(message.to_vec())));
                }
                assert_eq!(bytes(incoming.receiver.recv().await), Ok(None));
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    /// A window of two of the largest messages, of 1472 bytes.
    fn small(config: Config) -> Config {
        Config {
            message_bytes_max: NonZeroUsize::new(1472).expect("not zero"),
            window_bytes: 2 * 1472,
            ..config
        }
    }

    #[test]
    fn a_send_future_dropped_while_it_waits_behind_another_stream_sends_nothing() {
        let (mut sim, ..) = testing::sessions(
            0,
            small,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut first = opened.expect("a stream");
                let opened = side.session.open_sender(Class::Complete).await;
                let mut second = opened.expect("a stream");
                let body = vec![7; 1472];
                let mut count = 0;
                let held = loop {
                    let mut sending = Box::pin(first.send(side.block(&body)));
                    count += 1;
                    match poll_once(sending.as_mut()).await {
                        Some(done) => done.expect("sent"),
                        None => break sending,
                    }
                };
                let waiting = poll_once(pin!(second.send(side.block(b"c")))).await;
                assert_eq!(waiting, None, "the second stream waits");
                held.await.expect("sent");
                assert!(count > 1, "{count}");
                assert_eq!(second.send(side.block(b"c")).await, Ok(()));
                first.finish().expect("finished");
                second.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                let mut incoming = side.session.accept().await.expect("a stream");
                loop {
                    let read = bytes(incoming.receiver.recv().await).expect("read");
                    match read {
                        Some(message) => assert_eq!(message, vec![7; 1472]),
                        None => break,
                    }
                }
                let mut incoming = side.session.accept().await.expect("a stream");
                let read = incoming.receiver.recv().await;
                assert_eq!(bytes(read), Ok(Some(b"c".to_vec())));
                assert_eq!(bytes(incoming.receiver.recv().await), Ok(None));
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    /// Sends messages of 1472 bytes on `sender` with `try_send` until one comes back,
    /// which the stream gives back while it holds part of the one before. Gives the
    /// count it took.
    fn try_fill(side: &testing::Side, sender: &mut super::Sender) -> usize {
        let body = vec![7; 1472];
        for count in 0.. {
            let message = side.block(&body);
            let address = message.as_ptr();
            if let Some(given) = sender.try_send(message).expect("sent") {
                assert_eq!((given.as_ptr(), &*given), (address, body.as_slice()));
                assert!(count > 0, "the first message is not given back");
                return count;
            }
        }
        unreachable!("a window holds no more than 3 messages")
    }

    /// Reads `count` messages of 1472 bytes from `receiver`.
    async fn read_filled(receiver: &mut super::Receiver, count: usize) {
        for _ in 0..count {
            let read = bytes(receiver.recv().await);
            assert_eq!(read, Ok(Some(vec![7; 1472])));
        }
    }

    /// What a sender does after [`try_fill`].
    #[derive(Clone, Copy, Debug)]
    enum Then {
        Send,
        Finish,
        /// Drops a send that waits behind the rest, then finishes.
        DropSend,
        Drop,
    }

    #[test]
    fn a_try_send_taken_in_part_goes_whole_unless_the_sender_drops() {
        for then in [Then::Send, Then::Finish, Then::DropSend, Then::Drop] {
            let counted = Arc::new(AtomicUsize::new(0));
            let read = Arc::clone(&counted);
            let (mut sim, ..) = testing::sessions(
                0,
                small,
                move |side| async move {
                    let opened = side.session.open_sender(Class::Complete).await;
                    let mut sender = opened.expect("a stream");
                    counted.store(try_fill(&side, &mut sender), Ordering::Relaxed);
                    match then {
                        Then::Send => {
                            assert_eq!(sender.send(side.block(b"c")).await, Ok(()));
                            sender.finish().expect("finished");
                        }
                        Then::Finish => sender.finish().expect("finished"),
                        Then::DropSend => {
                            let send = sender.send(side.block(b"c"));
                            let waiting = poll_once(pin!(send)).await;
                            assert_eq!(waiting, None, "the send waits");
                            sender.finish().expect("finished");
                        }
                        Then::Drop => {
                            // The first message reaches the peer first, as the peer
                            // drops a stream reset before it.
                            side.node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
                            drop(sender);
                        }
                    }
                    let closed = Error::PeerClosed { code: Code(4) };
                    assert_eq!(side.session.closed().await, closed);
                },
                move |side| async move {
                    let mut incoming = side.session.accept().await.expect("a stream");
                    side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                    let receiver = &mut incoming.receiver;
                    if let Then::Drop = then {
                        assert_eq!(until_error(receiver).await, CANCELLED);
                    } else {
                        read_filled(receiver, read.load(Ordering::Relaxed)).await;
                        if let Then::Send = then {
                            let message = bytes(receiver.recv().await);
                            assert_eq!(message, Ok(Some(b"c".to_vec())));
                        }
                        assert_eq!(bytes(receiver.recv().await), Ok(None));
                    }
                    side.session.close(Code(4));
                },
            );
            assert_eq!(sim.run(), Ok(()), "then {then:?}");
        }
    }

    #[test]
    fn a_try_send_on_an_ended_session_gives_why_it_ended() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                let closed = Error::PeerClosed { code: Code(5) };
                assert_eq!(side.session.closed().await, closed);
                let tried = sender.try_send(side.block(b"b")).map(|_| ());
                assert_eq!(tried, Err(closed.clone()));
                side.node.clock().sleep(spans(Span::SECOND, 10)).await;
                let tried = sender.try_send(side.block(b"b")).map(|_| ());
                assert_eq!(tried, Err(closed));
            },
            |side| async move {
                let _incoming = side.session.accept().await.expect("a stream");
                side.session.close(Code(5));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_message_over_the_peers_largest_is_too_large_after_a_dropped_send() {
        let (mut sim, ..) = testing::sessions(
            0,
            small,
            |side| async move {
                let opened = side.session.open(Class::Complete).await;
                let (mut sender, mut receiver) = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                assert_eq!(bytes(receiver.recv().await), Ok(Some(b"b".to_vec())));
                let body = vec![7; 1473];
                while let Some(done) =
                    poll_once(pin!(sender.send(side.block(&body[1..])))).await
                {
                    done.expect("sent");
                }
                let large = Err(Error::TooLarge {
                    bytes: 1473,
                    bytes_max: 1472,
                });
                assert_eq!(sender.send(side.block(&body)).await, large);
                let tried = sender.try_send(side.block(&body)).map(|_| ());
                assert_eq!(tried, large);
                let sent = sender.send(side.block(&body[1..])).await;
                assert_eq!(sent, Err(CANCELLED));
                let tried = sender.try_send(side.block(&body[1..])).map(|_| ());
                assert_eq!(tried, Err(CANCELLED));
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let mut incoming = side.session.accept().await.expect("a stream");
                let reply = incoming.sender.as_mut().expect("a reply half");
                reply.send(side.block(b"b")).await.expect("sent");
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                assert_eq!(until_error(&mut incoming.receiver).await, CANCELLED);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_message_over_the_peers_largest_is_too_large_after_this_side_closes() {
        let small = |config| Config {
            message_bytes_max: NonZeroUsize::new(1472).expect("not zero"),
            ..config
        };
        let (mut sim, ..) = testing::sessions(
            0,
            small,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                let large = Err(Error::TooLarge {
                    bytes: 1473,
                    bytes_max: 1472,
                });
                let body = vec![7; 1473];
                side.session.close(Code(5));
                assert_eq!(sender.send(side.block(&body)).await, large);
                let closed = Error::Closed { code: Code(5) };
                assert_eq!(side.session.closed().await, closed);
                assert_eq!(sender.send(side.block(&body)).await, large);
                side.node.clock().sleep(spans(IDLE, 3)).await;
                assert_eq!(sender.send(side.block(&body)).await, large);
                assert_eq!(sender.send(side.block(b"a")).await, Err(closed));
            },
            |side| async move {
                let closed = Error::PeerClosed { code: Code(5) };
                assert_eq!(side.session.closed().await, closed);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn each_half_gives_the_peers_largest_message() {
        let small = |config| Config {
            message_bytes_max: NonZeroUsize::new(1472).expect("not zero"),
            ..config
        };
        let (mut sim, ..) = testing::sessions(
            0,
            small,
            |side| async move {
                let opened = side.session.open(Class::Complete).await;
                let (mut sender, _receiver) = opened.expect("a stream");
                assert_eq!(sender.bytes_max(), 1472);
                sender.send(side.block(b"a")).await.expect("sent");
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let incoming = side.session.accept().await.expect("a stream");
                let reply = incoming.sender.as_ref().expect("a reply half");
                assert_eq!(reply.bytes_max(), MESSAGE_BYTES_MAX);
                let opened = side.session.open_sender(Class::Complete).await;
                assert_eq!(opened.expect("a stream").bytes_max(), MESSAGE_BYTES_MAX);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_message_over_the_peers_largest_is_too_large() {
        let small = |config| Config {
            message_bytes_max: NonZeroUsize::new(1472).expect("not zero"),
            ..config
        };
        let (mut sim, ..) = testing::sessions(
            0,
            small,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                assert_eq!(sender.bytes_max(), 1472);
                let large = Error::TooLarge {
                    bytes: 1473,
                    bytes_max: sender.bytes_max(),
                };
                let body = vec![7; 1473];
                assert_eq!(sender.send(side.block(&body)).await, Err(large));
                sender.send(side.block(&body[1..])).await.expect("sent");
                sender.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let mut incoming = side.session.accept().await.expect("a stream");
                let read = incoming.receiver.recv().await;
                assert_eq!(read.map(|m| m.map(|b| b.len())), Ok(Some(1472)));
                assert_eq!(bytes(incoming.receiver.recv().await), Ok(None));
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_send_after_finish_panics() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.finish().expect("finished");
                drop(sender.send(side.block(b"a")).await);
            },
            |side| async move {
                drop(side.session.closed().await);
            },
        );
        let panicked = sim::Error::Panicked {
            thread: "transport".into(),
            message: "a sender is used after finish or reset".into(),
            seed: 0,
        };
        assert_eq!(sim.run(), Err(panicked));
    }

    #[test]
    fn a_recv_polled_by_a_new_waker_wakes_that_one() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                side.node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
                sender.send(side.block(b"b")).await.expect("sent");
                sender.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let mut incoming = side.session.accept().await.expect("a stream");
                let read = incoming.receiver.recv().await;
                assert_eq!(bytes(read), Ok(Some(b"a".to_vec())));
                let mut read = pin!(incoming.receiver.recv());
                let mut elsewhere = Context::from_waker(Waker::noop());
                assert!(read.as_mut().poll(&mut elsewhere).is_pending());
                assert_eq!(bytes(read.await), Ok(Some(b"b".to_vec())));
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    // Compares `Debug` output: a log tells the halves of two sessions apart only by
    // the session key in it.
    #[test]
    fn a_half_shows_its_session_key() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open(Class::Complete).await;
                let (sender, receiver) = opened.expect("a stream");
                for shown in [format!("{sender:?}"), format!("{receiver:?}")] {
                    assert!(shown.contains("Session { key: "), "{shown}");
                }
            },
            |side| async move {
                drop(side.session.closed().await);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_close_that_cuts_a_message_ends_each_later_send_and_finish() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                let body = vec![7; 60_000];
                let closed = Error::PeerClosed { code: Code(5) };
                let ended = loop {
                    if let Err(error) = sender.send(side.block(&body)).await {
                        break error;
                    }
                };
                assert_eq!(ended, closed);
                let sent = sender.send(side.block(b"a")).await;
                assert_eq!(sent, Err(closed.clone()));
                assert_eq!(sender.finish(), Err(closed));
            },
            |side| async move {
                let _incoming = side.session.accept().await.expect("a stream");
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                side.session.close(Code(5));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_send_cut_by_this_sides_close_then_finish_gives_the_close() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                let body = vec![7; 60_000];
                let closed = Error::Closed { code: Code(5) };
                loop {
                    let mut sending = pin!(sender.send(side.block(&body)));
                    if let Some(done) = poll_once(sending.as_mut()).await {
                        done.expect("sent");
                        continue;
                    }
                    side.session.close(Code(5));
                    let cut = poll_once(sending.as_mut()).await;
                    assert_eq!(cut, Some(Err(closed.clone())));
                    break;
                }
                assert_eq!(sender.finish(), Err(closed.clone()));
                let sent = sender.send(side.block(b"a")).await;
                assert_eq!(sent, Err(closed.clone()));
                side.session.close(Code(6));
                let sent = sender.send(side.block(b"a")).await;
                assert_eq!(sent, Err(closed));
            },
            |side| async move {
                let closed = Error::PeerClosed { code: Code(5) };
                assert_eq!(side.session.closed().await, closed);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_send_after_the_socket_breaks_gives_the_break() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                side.node.fail_udp(testing::address(&side.node));
                let broken = Error::Network {
                    error: env::net::Error::Io { code: 5 },
                };
                assert_eq!(side.session.closed().await, broken);
                let sent = sender.send(side.block(b"b")).await;
                assert_eq!(sent, Err(broken.clone()));
                assert_eq!(sender.finish(), Err(broken));
            },
            |side| async move {
                assert_eq!(side.session.closed().await, Error::TimedOut);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_close_after_the_socket_breaks_keeps_the_break() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                side.node.fail_udp(testing::address(&side.node));
                let broken = Error::Network {
                    error: env::net::Error::Io { code: 5 },
                };
                assert_eq!(side.session.closed().await, broken);
                side.session.close(Code(5));
                assert_eq!(side.session.closed().await, broken);
                let sent = sender.send(side.block(b"b")).await;
                assert_eq!(sent, Err(broken.clone()));
                assert_eq!(sender.finish(), Err(broken));
            },
            |side| async move {
                assert_eq!(side.session.closed().await, Error::TimedOut);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_send_cut_by_a_peer_close_then_a_close_then_send_gives_the_peer_close() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                let body = vec![7; 60_000];
                let ended = loop {
                    if let Err(error) = sender.send(side.block(&body)).await {
                        break error;
                    }
                };
                let closed = Error::PeerClosed { code: Code(5) };
                assert_eq!(ended, closed);
                side.session.close(Code(6));
                let sent = sender.send(side.block(b"a")).await;
                assert_eq!(sent, Err(closed));
            },
            |side| async move {
                let _incoming = side.session.accept().await.expect("a stream");
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                side.session.close(Code(5));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_send_cut_by_a_socket_break_then_a_close_then_send_gives_the_break() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                let body = vec![7; 60_000];
                let broken = Error::Network {
                    error: env::net::Error::Io { code: 5 },
                };
                loop {
                    let mut sending = pin!(sender.send(side.block(&body)));
                    if let Some(done) = poll_once(sending.as_mut()).await {
                        done.expect("sent");
                        continue;
                    }
                    side.node.fail_udp(testing::address(&side.node));
                    assert_eq!(side.session.closed().await, broken);
                    let cut = poll_once(sending.as_mut()).await;
                    assert_eq!(cut, Some(Err(broken.clone())));
                    break;
                }
                side.session.close(Code(5));
                let sent = sender.send(side.block(b"a")).await;
                assert_eq!(sent, Err(broken));
            },
            |side| async move {
                assert_eq!(side.session.closed().await, Error::TimedOut);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_send_cut_by_a_socket_break_then_send_gives_the_break() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                let body = vec![7; 60_000];
                let broken = Error::Network {
                    error: env::net::Error::Io { code: 5 },
                };
                loop {
                    let mut sending = pin!(sender.send(side.block(&body)));
                    if let Some(done) = poll_once(sending.as_mut()).await {
                        done.expect("sent");
                        continue;
                    }
                    side.node.fail_udp(testing::address(&side.node));
                    assert_eq!(side.session.closed().await, broken);
                    let cut = poll_once(sending.as_mut()).await;
                    assert_eq!(cut, Some(Err(broken.clone())));
                    break;
                }
                let sent = sender.send(side.block(b"a")).await;
                assert_eq!(sent, Err(broken.clone()));
                assert_eq!(sender.finish(), Err(broken));
            },
            |side| async move {
                assert_eq!(side.session.closed().await, Error::TimedOut);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_finish_right_after_this_side_closes_gives_the_close() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                side.session.close(Code(5));
                let closed = Err(Error::Closed { code: Code(5) });
                assert_eq!(sender.finish(), closed);
                assert_eq!(sender.finish(), closed);
            },
            |side| async move {
                let closed = Error::PeerClosed { code: Code(5) };
                assert_eq!(side.session.closed().await, closed);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn each_finish_after_the_peer_stopped_the_stream_gives_the_stop() {
        let (mut sim, ..) = testing::sessions(
            0,
            same,
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                let stopped = Err(Error::Stopped { code: Code(3) });
                assert_eq!(sender.finish(), stopped);
                assert_eq!(sender.finish(), stopped);
                side.session.close(Code(5));
            },
            |side| async move {
                let incoming = side.session.accept().await.expect("a stream");
                incoming.receiver.stop(Code(3));
                let closed = Error::PeerClosed { code: Code(5) };
                assert_eq!(side.session.closed().await, closed);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    /// The bytes of each message in the tests of reads that wait for a block. The
    /// pool of [`scarce`] holds one.
    const LARGE: usize = 40_000;

    /// `config` with a pool of 64 KiB on `memory`, and messages up to its largest.
    fn scarce(config: Config, memory: impl block::Memory + 'static) -> Config {
        let pool = block::Pool::new(block::Config { budget: 1 << 16 }, memory);
        Config {
            message_bytes_max: NonZeroUsize::new(pool.largest()).expect("not zero"),
            pool: Rc::new(pool),
            ..config
        }
    }

    fn heap() -> block::Heap {
        block::Heap::new(block::Config { budget: 1 << 16 }.reservation())
    }

    /// Opens a one-way stream of each of `classes` in order, sends a [`LARGE`]
    /// message that holds its index in each byte, and finishes it. Waits `pause`
    /// after each stream.
    async fn send_large(side: &testing::Side, classes: &[Class], pause: Span) {
        for (index, &class) in (0..).zip(classes) {
            let opened = side.session.open_sender(class).await;
            let mut sender = opened.expect("a stream");
            sender
                .send(side.block(&vec![index; LARGE]))
                .await
                .expect("sent");
            sender.finish().expect("finished");
            side.node.clock().sleep(pause).await;
        }
    }

    /// Polls each of `reads` until `span` passes, and checks that none completes.
    async fn pending_for<F: Future + Unpin>(
        clock: &env::clock::Clock,
        span: Span,
        reads: &mut [F],
    ) {
        let mut sleep = pin!(clock.sleep(span));
        poll_fn(|cx| {
            for read in &mut *reads {
                assert!(Pin::new(read).poll(cx).is_pending());
            }
            sleep.as_mut().poll(cx)
        })
        .await;
    }

    #[test]
    fn a_recv_with_a_full_pool_waits_until_a_block_frees() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| scarce(config, heap()),
            |side| async move {
                send_large(&side, &[Class::Complete], Span::ZERO).await;
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let clock = side.node.clock();
                let none = crate::Status {
                    waited: Span::ZERO,
                    refusals: 0,
                };
                assert_eq!(side.transport.status(), none);
                let held = side.pool.alloc(LARGE).expect("room");
                let start = clock.now();
                let mut incoming = side.session.accept().await.expect("a stream");
                let mut read = pin!(incoming.receiver.recv());
                pending_for(
                    &clock,
                    spans(Span::MILLISECOND, 100),
                    &mut [read.as_mut()],
                )
                .await;
                drop(held);
                let message = read.await.expect("a message").expect("not finished");
                assert_eq!(message.to_vec(), vec![0; LARGE]);
                let waited = side.transport.status().waited;
                assert!(waited > spans(Span::MILLISECOND, 50), "{waited:?}");
                assert!(waited <= clock.now() - start, "{waited:?}");
                clock.sleep(spans(Span::MILLISECOND, 50)).await;
                assert_eq!(
                    side.transport.status(),
                    crate::Status {
                        waited,
                        refusals: 0
                    }
                );
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_recv_whose_commit_the_system_refuses_counts_it_and_waits() {
        let (memory, switch) =
            Scarce::new(block::Config { budget: 1 << 16 }.reservation());
        let (mut sim, ..) = testing::sessions(
            0,
            move |config| scarce(config, memory),
            |side| async move {
                send_large(&side, &[Class::Complete], Span::ZERO).await;
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            move |side| async move {
                let clock = side.node.clock();
                switch.refuse();
                let mut incoming = side.session.accept().await.expect("a stream");
                let mut read = pin!(incoming.receiver.recv());
                pending_for(
                    &clock,
                    spans(Span::MILLISECOND, 100),
                    &mut [read.as_mut()],
                )
                .await;
                let refused = side.transport.status().refusals;
                // The first try and at least one retry.
                assert!(refused > 1, "{refused}");
                switch.allow();
                let message = read.await.expect("a message").expect("not finished");
                assert_eq!(message.to_vec(), vec![0; LARGE]);
                assert_eq!(side.transport.status().refusals, refused);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn reads_that_wait_take_blocks_highest_class_first_then_oldest() {
        let classes = [
            Class::CatchUp,
            Class::Complete,
            Class::Complete,
            Class::Command,
        ];
        let (mut sim, ..) = testing::sessions(
            0,
            |config| scarce(config, heap()),
            move |side| async move {
                let pause = spans(Span::MILLISECOND, 20);
                send_large(&side, &classes, pause).await;
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let clock = side.node.clock();
                let held = side.pool.alloc(LARGE).expect("room");
                let mut receivers = Vec::new();
                for _ in 0..4 {
                    let incoming = side.session.accept().await.expect("a stream");
                    receivers.push(incoming.receiver);
                }
                let mut reads: Vec<_> = receivers
                    .iter_mut()
                    .map(|r| Some(Box::pin(r.recv())))
                    .collect();
                let freed = async {
                    clock.sleep(spans(Span::MILLISECOND, 100)).await;
                    drop(held);
                };
                let mut order = Vec::new();
                let taken = poll_fn(|cx| {
                    for slot in &mut reads {
                        if let Some(read) = slot
                            && let Poll::Ready(message) = read.as_mut().poll(cx)
                        {
                            let block = message.expect("a message").expect("a block");
                            order.push(block[0]);
                            *slot = None;
                        }
                    }
                    if order.len() == 4 {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                });
                testing::join(taken, freed).await;
                assert_eq!(order, [3, 1, 2, 0]);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dropped_recv_that_waited_first_gives_the_block_to_the_next() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| scarce(config, heap()),
            |side| async move {
                let pause = spans(Span::MILLISECOND, 50);
                send_large(&side, &[Class::Complete, Class::Complete], pause).await;
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let clock = side.node.clock();
                let held = side.pool.alloc(LARGE).expect("room");
                let wait = spans(Span::MILLISECOND, 30);
                let mut second;
                let mut first = side.session.accept().await.expect("a stream").receiver;
                let mut first_read = Box::pin(first.recv());
                pending_for(&clock, wait, &mut [first_read.as_mut()]).await;
                second = side.session.accept().await.expect("a stream").receiver;
                let mut second_read = Box::pin(second.recv());
                pending_for(
                    &clock,
                    wait,
                    &mut [first_read.as_mut(), second_read.as_mut()],
                )
                .await;
                drop(first_read);
                drop(held);
                let message = second_read.await.expect("a message").expect("a block");
                assert_eq!(message.to_vec(), vec![1; LARGE]);
                drop(message);
                let message = first.recv().await.expect("a message").expect("a block");
                assert_eq!(message.to_vec(), vec![0; LARGE]);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_message_that_waits_for_a_block_holds_the_peers_send() {
        let narrow = |config| {
            let config = scarce(config, heap());
            Config {
                window_bytes: config.message_bytes_max.get(),
                ..config
            }
        };
        let (mut sim, ..) = testing::sessions(
            0,
            narrow,
            |side| async move {
                let clock = side.node.clock();
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender
                    .send(side.block(&vec![0; LARGE]))
                    .await
                    .expect("sent");
                let mut send = Box::pin(sender.send(side.block(&vec![1; LARGE])));
                pending_for(
                    &clock,
                    spans(Span::MILLISECOND, 100),
                    &mut [send.as_mut()],
                )
                .await;
                send.await.expect("sent");
                sender.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let clock = side.node.clock();
                let held = side.pool.alloc(LARGE).expect("room");
                let mut incoming = side.session.accept().await.expect("a stream");
                let mut read = Box::pin(incoming.receiver.recv());
                pending_for(
                    &clock,
                    spans(Span::MILLISECOND, 150),
                    &mut [read.as_mut()],
                )
                .await;
                drop(held);
                let message = read.await.expect("a message").expect("a block");
                assert_eq!(message.to_vec(), vec![0; LARGE]);
                drop(message);
                let read = incoming.receiver.recv().await;
                let message = read.expect("a message").expect("a block");
                assert_eq!(message.to_vec(), vec![1; LARGE]);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_session_that_ends_while_a_read_waits_ends_the_read_and_its_wait() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| scarce(config, heap()),
            |side| async move {
                send_large(&side, &[Class::Complete], Span::ZERO).await;
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                side.session.close(Code(6));
            },
            |side| async move {
                let clock = side.node.clock();
                let _held = side.pool.alloc(LARGE).expect("room");
                let mut incoming = side.session.accept().await.expect("a stream");
                let read = incoming.receiver.recv().await;
                assert_eq!(bytes(read), Err(Error::PeerClosed { code: Code(6) }));
                let status = side.transport.status();
                assert!(status.waited > Span::ZERO, "{status:?}");
                clock.sleep(spans(Span::MILLISECOND, 50)).await;
                assert_eq!(side.transport.status(), status);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_read_that_needs_no_block_ends_while_another_read_waits_for_one() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| scarce(config, heap()),
            |side| async move {
                let pause = spans(Span::MILLISECOND, 50);
                send_large(&side, &[Class::Complete, Class::Complete], pause).await;
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let clock = side.node.clock();
                let mut first = side.session.accept().await.expect("a stream").receiver;
                // The first message fills the pool.
                let held = first.recv().await.expect("a message").expect("a block");
                let mut second =
                    side.session.accept().await.expect("a stream").receiver;
                let mut second_read = Box::pin(second.recv());
                pending_for(
                    &clock,
                    spans(Span::MILLISECOND, 30),
                    &mut [second_read.as_mut()],
                )
                .await;
                // The first stream finished: its read needs no block.
                let mut end = Box::pin(first.recv());
                let mut deadline = pin!(clock.sleep(spans(Span::SECOND, 2)));
                let ended = poll_fn(|cx| {
                    assert!(second_read.as_mut().poll(cx).is_pending());
                    if let Poll::Ready(read) = end.as_mut().poll(cx) {
                        return Poll::Ready(Some(bytes(read)));
                    }
                    deadline.as_mut().poll(cx).map(|()| None)
                })
                .await;
                assert_eq!(ended, Some(Ok(None)), "the end waits behind the block");
                drop(end);
                drop(held);
                let message = second_read.await.expect("a message").expect("a block");
                assert_eq!(message.to_vec(), vec![1; LARGE]);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn two_reads_of_messages_that_each_fill_the_pool_each_get_theirs() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| scarce(config, heap()),
            |side| async move {
                send_large(&side, &[Class::Complete, Class::Complete], Span::ZERO)
                    .await;
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let clock = side.node.clock();
                let mut a = side.session.accept().await.expect("a stream").receiver;
                let mut b = side.session.accept().await.expect("a stream").receiver;
                let mut reads = [Some(Box::pin(a.recv())), Some(Box::pin(b.recv()))];
                let mut got = Vec::new();
                let mut deadline = pin!(clock.sleep(spans(Span::SECOND, 5)));
                let done = poll_fn(|cx| {
                    for slot in &mut reads {
                        if let Some(read) = slot
                            && let Poll::Ready(message) = read.as_mut().poll(cx)
                        {
                            // Each message drops at once.
                            let block = message.expect("a message").expect("a block");
                            got.push(block[0]);
                            *slot = None;
                        }
                    }
                    if got.len() == 2 {
                        return Poll::Ready(true);
                    }
                    deadline.as_mut().poll(cx).map(|()| false)
                })
                .await;
                assert!(done, "only {got:?} arrived");
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_recv_on_an_ended_session_errs_while_another_session_waits_for_a_block() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::shard(&server, testing::SERVER, |config, node| async move {
            let config = scarce(config, heap());
            let part = testing::part(&node.net(), testing::address(&node));
            let transport = Transport::new(config, part).expect("a transport");
            let ended = transport.accept().await.expect("a session");
            let waiting = transport.accept().await.expect("a session");
            let clock = node.clock();
            let mut first = waiting.accept().await.expect("a stream").receiver;
            // The first message fills the pool.
            let held = first.recv().await.expect("a message").expect("a block");
            let mut second = waiting.accept().await.expect("a stream").receiver;
            let mut second_read = Box::pin(second.recv());
            let mut receiver = ended.accept().await.expect("a stream").receiver;
            let mut closed = pin!(ended.closed());
            let closed = poll_fn(|cx| {
                assert!(second_read.as_mut().poll(cx).is_pending());
                closed.as_mut().poll(cx)
            })
            .await;
            assert_eq!(closed, Error::PeerClosed { code: Code(7) });
            let read = poll_once(pin!(receiver.recv())).await.map(bytes);
            assert_eq!(read, Some(Err(Error::PeerClosed { code: Code(7) })));
            drop(held);
            let message = second_read.await.expect("a message").expect("a block");
            assert_eq!(message.to_vec(), vec![1; LARGE]);
            waiting.close(Code(4));
            clock.sleep(Span::MILLISECOND).await;
        });
        testing::shard(&client, testing::CLIENT, move |config, node| async move {
            let pool = Rc::clone(&config.pool);
            let part = testing::part(&node.net(), testing::address(&node));
            let transport = Transport::new(config, part).expect("a transport");
            let server = crate::tls::public(&testing::SERVER);
            let ended = transport.dial(server, &at).await.expect("a session");
            let waiting = transport.dial(server, &at).await.expect("a session");
            let messages = [(&waiting, 0), (&waiting, 1), (&ended, 2)];
            for (session, byte) in messages {
                let opened = session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                let block = testing::block(&pool, &vec![byte; LARGE]);
                sender.send(block).await.expect("sent");
                sender.finish().expect("finished");
            }
            node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
            ended.close(Code(7));
            let closed = Error::PeerClosed { code: Code(4) };
            assert_eq!(waiting.closed().await, closed);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_recv_after_a_dropped_wait_and_a_drained_session_gives_the_end() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| scarce(config, heap()),
            |side| async move {
                send_large(&side, &[Class::Complete, Class::Complete], Span::ZERO)
                    .await;
                side.node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
                side.session.close(Code(4));
                let closed = Error::Closed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let clock = side.node.clock();
                let mut first = side.session.accept().await.expect("a stream").receiver;
                let held = first.recv().await.expect("a message").expect("a block");
                let mut second =
                    side.session.accept().await.expect("a stream").receiver;
                assert!(poll_once(pin!(second.recv())).await.is_none());
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
                clock.sleep(spans(IDLE, 3)).await;
                let mut read = pin!(second.recv());
                let mut deadline = pin!(clock.sleep(Span::SECOND));
                let read = poll_fn(|cx| {
                    if let Poll::Ready(read) = read.as_mut().poll(cx) {
                        return Poll::Ready(Some(bytes(read)));
                    }
                    deadline.as_mut().poll(cx).map(|()| None)
                })
                .await;
                assert_eq!(read, Some(Err(closed)));
                drop(held);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_recv_that_waits_for_a_block_gives_a_reset_at_once() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| scarce(config, heap()),
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                let sent = sender.send(side.block(&vec![0; LARGE])).await;
                sent.expect("sent");
                side.node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
                sender.reset(Code(9));
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let held = side.pool.alloc(LARGE).expect("room");
                let mut incoming = side.session.accept().await.expect("a stream");
                let mut read = pin!(incoming.receiver.recv());
                let mut deadline = pin!(side.node.clock().sleep(spans(IDLE, 1)));
                let read = poll_fn(|cx| {
                    if let Poll::Ready(read) = read.as_mut().poll(cx) {
                        return Poll::Ready(Some(bytes(read)));
                    }
                    deadline.as_mut().poll(cx).map(|()| None)
                })
                .await;
                assert_eq!(read, Some(Err(Error::Reset { code: Code(9) })));
                drop(held);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }
}

#[cfg(test)]
mod stress;
