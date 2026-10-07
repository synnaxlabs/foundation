//! The two halves of a stream: an ordered, reliable sequence of whole messages.

use std::future::poll_fn;
use std::rc::Rc;
use std::task::Poll;

use block::Block;

use crate::class::Class;
use crate::code::Code;
use crate::error::Error;
use crate::quic;

/// What a [`Sender`] gives after a [`Sender::send`] future dropped and reset its
/// stream.
const CANCELLED: Error = Error::Reset { code: Code(0) };

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
    /// `None` once a `send` future dropped and reset the stream.
    stream: Option<quic::stream::Sender>,
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
            stream: Some(stream),
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

    /// Sends `message` whole. It waits while the peer's flow control has no room,
    /// and returns once the stream holds the message, not when the peer has it. If the
    /// future drops before it completes, the stream resets with `Code(0)`, because
    /// part of the message may be sent.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over the peer's
    /// [`Config::message_bytes_max`](crate::Config::message_bytes_max),
    /// [`Error::Stopped`] when the peer stopped reading, [`Error::Reset`] with
    /// `Code(0)` after a `send` future dropped, or the error that ended the session.
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
            stream: &mut self.stream,
            done: false,
        };
        let mut message = Some(message);
        let sent = poll_fn(|cx| {
            let Some(stream) = sending.stream else {
                return Poll::Ready(Err(CANCELLED));
            };
            sending.session.poll_write(cx, stream, &mut message)
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
        self.stream.as_mut().ok_or(CANCELLED)?;
        drop(message);
        todo!("#68")
    }

    /// Ends the stream after the messages already sent. The peer's
    /// [`Receiver::recv`] returns `None` after the last one. The sender stays, so
    /// [`reset`](Self::reset) can still cancel what the peer does not have yet.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped reading, [`Error::Reset`] with
    /// `Code(0)` after a `send` future dropped, or the error that ended the session.
    ///
    /// ```
    /// use transport::{Error, stream::Sender};
    ///
    /// fn done(sender: &mut Sender) -> Result<(), Error> {
    ///     sender.finish()
    /// }
    /// ```
    pub fn finish(&mut self) -> Result<(), Error> {
        let stream = self.stream.as_mut().ok_or(CANCELLED)?;
        self.session.finish(stream)
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
        if let Some(stream) = self.stream.take() {
            self.session.reset(stream, code);
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take()
            && !stream.finished()
        {
            self.session.reset(stream, Code(0));
        }
    }
}

/// A [`Sender::send`] in progress. Dropping it before it is done resets the stream,
/// because the stream may hold part of the message.
struct Sending<'a> {
    session: &'a quic::Session,
    stream: &'a mut Option<quic::stream::Sender>,
    done: bool,
}

impl Drop for Sending<'_> {
    fn drop(&mut self) {
        if !self.done
            && let Some(stream) = self.stream.take()
        {
            self.session.reset(stream, Code(0));
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
    /// # Errors
    ///
    /// [`Error::Reset`] when the sender cancelled the stream, [`Error::Pool`] when the
    /// pool has no room for the next message (it stays queued), or the error that
    /// ended the session.
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
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::task::{Context, Waker};

    use block::Block;
    use sim::Sim;
    use sim::node::Node;
    use types::time::Span;

    use crate::testing::{self, IDLE, poll_once, spans};
    use crate::{Class, Code, Config, Error};

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
                let large = Error::TooLarge {
                    bytes: 1473,
                    bytes_max: 1472,
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
            message: "a sender is used after finish".into(),
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
    fn a_close_after_the_session_ended_keeps_its_error() {
        for broken in [true, false] {
            let (mut sim, ..) = testing::sessions(
                0,
                same,
                move |side| async move {
                    let opened = side.session.open_sender(Class::Complete).await;
                    let mut sender = opened.expect("a stream");
                    sender.send(side.block(b"a")).await.expect("sent");
                    let end = if broken {
                        side.node.fail_udp(testing::address(&side.node));
                        Error::Network {
                            error: env::net::Error::Io { code: 5 },
                        }
                    } else {
                        Error::PeerClosed { code: Code(7) }
                    };
                    assert_eq!(side.session.closed().await, end);
                    side.session.close(Code(5));
                    assert_eq!(side.session.closed().await, end);
                    let sent = sender.send(side.block(b"b")).await;
                    assert_eq!(sent, Err(end.clone()));
                    assert_eq!(sender.finish(), Err(end));
                },
                move |side| async move {
                    if broken {
                        assert_eq!(side.session.closed().await, Error::TimedOut);
                    } else {
                        side.session.accept().await.expect("a stream");
                        side.session.close(Code(7));
                    }
                },
            );
            assert_eq!(sim.run(), Ok(()), "broken: {broken}");
        }
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
}
