use std::fmt;
use std::future::poll_fn;
use std::rc::{self, Rc};

use types::ed25519::PublicKey;

use crate::class::Class;
use crate::code::Code;
use crate::datagram;
use crate::error::Error;
use crate::quic;
use crate::stream::{Incoming, Receiver, Sender};

/// A connection to one peer. It carries streams of whole messages and datagrams that
/// may drop. It stays on the shard that made it.
///
/// Clones share the session, and so do its streams and datagram halves. When the
/// last of them drops, the session closes with `Code(0)` once the peer has every
/// finished stream, or after [`Config::idle`](crate::Config::idle). The session also
/// ends when either side closes it, or when its carrier fails.
///
/// ```
/// use block::Block;
/// use transport::{Class, Error, Session};
///
/// async fn ask(session: &Session, question: Block) -> Result<Option<Block>, Error> {
///     let (mut sender, mut receiver) = session.open(Class::Command).await?;
///     sender.send(question).await?;
///     sender.finish()?;
///     receiver.recv().await
/// }
/// ```
#[derive(Clone)]
pub struct Session(Rc<quic::Session>);

impl Session {
    pub(crate) fn new(session: quic::Session) -> Self {
        Self(Rc::new(session))
    }

    /// A handle that does not keep the session from closing at its last drop.
    pub(crate) fn downgrade(&self) -> Weak {
        Weak(Rc::downgrade(&self.0))
    }

    /// The carrier's session under this one.
    pub(crate) fn quic(&self) -> &quic::Session {
        &self.0
    }

    /// Who is on the other end.
    ///
    /// ```
    /// use transport::{Peer, Session};
    ///
    /// fn is_node(session: &Session) -> bool {
    ///     matches!(session.peer(), Peer::Node(_))
    /// }
    /// ```
    #[must_use]
    pub fn peer(&self) -> Peer {
        self.0.peer()
    }

    /// Whether the session runs through a relay node. It never changes: a session is
    /// direct or relayed as a whole, and a better path means a new session. A relayed
    /// round trip is not symmetric, so it does not measure clock offset well.
    ///
    /// ```
    /// fn may_sync_clocks(session: &transport::Session) -> bool {
    ///     !session.relayed()
    /// }
    /// ```
    #[must_use]
    pub fn relayed(&self) -> bool {
        false
    }

    /// Opens a stream in both directions. It waits while the peer allows no more
    /// streams; dropping the future before it completes opens nothing. The peer sees
    /// the stream at its first message.
    ///
    /// # Errors
    ///
    /// The error that ended the session.
    ///
    /// ```
    /// use transport::stream::{Receiver, Sender};
    /// use transport::{Class, Error, Session};
    ///
    /// async fn open(session: &Session) -> Result<(Sender, Receiver), Error> {
    ///     session.open(Class::Complete).await
    /// }
    /// ```
    pub async fn open(&self, class: Class) -> Result<(Sender, Receiver), Error> {
        let (sender, receiver) = poll_fn(|cx| self.0.poll_open(cx, class)).await?;
        let sender = Sender::new(Rc::clone(&self.0), class, sender);
        Ok((sender, Receiver::new(Rc::clone(&self.0), receiver)))
    }

    /// Opens a stream that only this node sends on. It waits while the peer allows no
    /// more streams; dropping the future before it completes opens nothing. The peer
    /// sees the stream at its first message.
    ///
    /// # Errors
    ///
    /// The error that ended the session.
    ///
    /// ```
    /// use transport::{Class, Error, Session, stream};
    ///
    /// async fn open(session: &Session) -> Result<stream::Sender, Error> {
    ///     session.open_sender(Class::Latest).await
    /// }
    /// ```
    pub async fn open_sender(&self, class: Class) -> Result<Sender, Error> {
        let sender = poll_fn(|cx| self.0.poll_open_sender(cx, class)).await?;
        Ok(Sender::new(Rc::clone(&self.0), class, sender))
    }

    /// Waits for the next stream the peer opened, highest class first. Every clone
    /// shares one queue, and each stream goes to one caller, so one dispatcher per
    /// session should take them.
    ///
    /// # Errors
    ///
    /// The error that ended the session.
    ///
    /// ```
    /// use transport::{Error, Session};
    ///
    /// async fn serve(session: &Session) -> Result<(), Error> {
    ///     loop {
    ///         let incoming = session.accept().await?;
    ///         let _ = incoming.class;
    ///     }
    /// }
    /// ```
    pub async fn accept(&self) -> Result<Incoming, Error> {
        let incoming = poll_fn(|cx| self.0.poll_accept(cx)).await?;
        Ok(Incoming::new(&self.0, incoming))
    }

    /// The session's datagrams. Every call gives halves of the same queues.
    ///
    /// ```
    /// use transport::{Session, datagram};
    ///
    /// fn split(session: &Session) -> (datagram::Sender, datagram::Receiver) {
    ///     session.datagrams()
    /// }
    /// ```
    #[must_use]
    pub fn datagrams(&self) -> (datagram::Sender, datagram::Receiver) {
        todo!("#68")
    }

    /// Closes the session with `code` now. Data not yet delivered drops, streams on
    /// it end, and the peer sees [`Error::PeerClosed`], or [`Error::Broken`] or
    /// [`Error::TimedOut`] when the close is lost. It does not wait. Closing an ended
    /// session does nothing. Every caller that [`Transport::dial`] gave this session
    /// shares it, so closing it ends their streams too.
    ///
    /// [`Transport::dial`]: crate::Transport::dial
    ///
    /// ```
    /// fn leave(session: &transport::Session) {
    ///     session.close(transport::Code(0));
    /// }
    /// ```
    pub fn close(&self, code: Code) {
        self.0.close(code);
    }

    /// Waits until the session ends and returns why.
    ///
    /// ```
    /// async fn watch(session: &transport::Session) -> transport::Error {
    ///     session.closed().await
    /// }
    /// ```
    pub async fn closed(&self) -> Error {
        self.0.closed().await
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("peer", &self.peer())
            .finish_non_exhaustive()
    }
}

/// A handle to a [`Session`] that does not keep it from closing at its last drop.
#[derive(Default)]
pub(crate) struct Weak(rc::Weak<quic::Session>);

impl Weak {
    /// The session while a handle holds it and it is open: no caller closed it, and
    /// it has not ended.
    pub(crate) fn open(&self) -> Option<Session> {
        self.0
            .upgrade()
            .filter(|session| session.live())
            .map(Session)
    }

    /// Whether this is a handle to `session`.
    pub(crate) fn is(&self, session: &Session) -> bool {
        std::ptr::eq(self.0.as_ptr(), Rc::as_ptr(&session.0))
    }
}

/// Who is on the other end of a [`Session`].
///
/// ```
/// use transport::Peer;
///
/// fn key(peer: Peer) -> Option<types::ed25519::PublicKey> {
///     match peer {
///         Peer::Node(key) => Some(key),
///         Peer::Client => None,
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peer {
    /// A node that proved it holds this key.
    Node(PublicKey),
    /// A program with no node key, such as an SDK. It proves who it is above the
    /// transport, with a signed hello.
    Client,
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::num::NonZeroUsize;
    use std::pin::{Pin, pin};
    use std::rc::Rc;

    use types::time::Span;

    use crate::testing::{self, CLIENT, IDLE, SERVER, join, poll_once, spans};
    use crate::{Address, Class, Code, Config, Error, Transport, message};

    #[test]
    fn a_session_stays_open_until_its_last_clone_drops() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = testing::address(&server);
        let held = Span::from_nanos(2 * IDLE.nanos());
        testing::transport(&server, SERVER, move |transport, node| async move {
            let session = transport.accept().await.expect("a session");
            drop(session.clone());
            node.clock().sleep(held).await;
            drop(session);
            // A shard that ends drops its tasks.
            node.clock().sleep(Span::MILLISECOND).await;
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            let connected = node.clock().now();
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(session.closed().await, closed);
            assert!(node.clock().now() - connected >= held);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dropped_recv_gives_its_wait_for_room_to_the_next_stream() {
        dropped_wait(Read::Recv);
    }

    #[test]
    fn a_dropped_recv_into_gives_its_wait_for_room_to_the_next_stream() {
        dropped_wait(Read::RecvInto);
    }

    #[test]
    fn a_too_large_message_keeps_its_room_until_a_read_or_a_reset_takes_it() {
        // The receive budget is the window plus the largest message: 2^17 bytes. A
        // message that never arrives holds 65,000 of it, so 50,000 more fit once.
        const LEN: usize = 50_000;
        let narrow = |config| Config {
            window_bytes: 1 << 16,
            ..config
        };
        let (mut sim, ..) = testing::sessions(
            0,
            narrow,
            |side| async move {
                side.node.clock().sleep(spans(Span::MILLISECOND, 10)).await;
                let complete = 2;
                let header = [[complete].as_slice(), &message::prefix(65_000)].concat();
                side.session.0.raw(&header);
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                let mut senders: Vec<crate::stream::Sender> = Vec::new();
                for fill in 1..=4 {
                    if fill > 2 {
                        side.node.clock().sleep(spans(Span::MILLISECOND, 300)).await;
                    }
                    if fill == 4 {
                        senders.remove(2).reset(Code(16));
                    }
                    let opened = side.session.open_sender(Class::Complete).await;
                    let mut sender = opened.expect("a stream");
                    let block = side.block(&vec![fill; LEN]);
                    sender.send(block).await.expect("sent");
                    senders.push(sender);
                }
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let (mut short, mut buffer) = ([0; 100], vec![0; LEN]);
                let over = Error::TooLarge {
                    bytes: LEN,
                    bytes_max: 100,
                };
                side.node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
                let mut held = side.session.accept().await.expect("a stream").receiver;
                assert!(poll_once(pin!(held.recv())).await.is_none());
                let mut first = side.session.accept().await.expect("a stream").receiver;
                let mut second =
                    side.session.accept().await.expect("a stream").receiver;
                assert_eq!(first.recv_into(&mut short).await, Err(over.clone()));
                let mut waiting = Box::pin(second.recv_into(&mut buffer));
                assert!(poll_once(waiting.as_mut()).await.is_none());
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                assert!(poll_once(waiting.as_mut()).await.is_none());
                let read = first.recv_into(&mut vec![0; LEN]).await;
                assert_eq!(read, Ok(Some(LEN)));
                assert_eq!(waiting.await, Ok(Some(LEN)));
                assert_eq!(buffer, vec![2; LEN]);
                let mut third = side.session.accept().await.expect("a stream").receiver;
                assert_eq!(third.recv_into(&mut short).await, Err(over.clone()));
                side.node.clock().sleep(spans(Span::MILLISECOND, 400)).await;
                let read = third.recv_into(&mut short).await;
                assert_eq!(read, Err(Error::Reset { code: Code(16) }));
                let mut fourth =
                    side.session.accept().await.expect("a stream").receiver;
                assert_eq!(fourth.recv_into(&mut buffer).await, Ok(Some(LEN)));
                assert_eq!(buffer, vec![4; LEN]);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    /// The read that a test runs.
    #[derive(Clone, Copy)]
    enum Read {
        Recv,
        RecvInto,
    }

    fn dropped_wait(read: Read) {
        // The receive budget is the window plus the largest message: 2^17 bytes.
        let narrow = |config| Config {
            window_bytes: 1 << 16,
            ..config
        };
        let (mut sim, ..) = testing::sessions(
            0,
            narrow,
            |side| async move {
                side.node.clock().sleep(spans(Span::MILLISECOND, 10)).await;
                let complete = 2;
                for len in [65_000, 65_000, 1 << 16] {
                    let header =
                        [[complete].as_slice(), &message::prefix(len)].concat();
                    side.session.0.raw(&header);
                }
                let small = [&[complete], &*message::prefix(100), &[7; 100]].concat();
                side.session.0.raw(&small);
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            move |side| async move {
                side.node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
                let mut receivers = Vec::new();
                for _ in 0..4 {
                    let incoming = side.session.accept().await.expect("a stream");
                    assert_eq!(incoming.class, Class::Complete);
                    receivers.push(incoming.receiver);
                }
                let [first, second, waits, small] = &mut receivers[..] else {
                    unreachable!("four streams");
                };
                assert!(poll_once(pin!(first.recv())).await.is_none());
                assert!(poll_once(pin!(second.recv())).await.is_none());
                // Boxed, so that the drop below ends the future, not only a borrow.
                let mut buffer = vec![0; 1 << 16];
                let mut waiting: Pin<Box<dyn Future<Output = _>>> = match read {
                    Read::Recv => {
                        Box::pin(async { waits.recv().await.map(|m| m.is_some()) })
                    }
                    Read::RecvInto => Box::pin(async {
                        waits.recv_into(&mut buffer).await.map(|len| len.is_some())
                    }),
                };
                assert!(poll_once(Pin::new(&mut waiting)).await.is_none());
                // The small message fits, but waits behind the one before it.
                assert!(poll_once(pin!(small.recv())).await.is_none());
                drop(waiting);
                let read = poll_once(pin!(small.recv())).await;
                let read = read.map(|read| read.map(|m| m.map(|block| block.to_vec())));
                assert_eq!(read, Some(Ok(Some(vec![7; 100]))));
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn accept_gives_the_waiting_streams_highest_class_first() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| config,
            |side| async move {
                let mut senders = Vec::new();
                for class in [
                    Class::CatchUp,
                    Class::Complete,
                    Class::Latest,
                    Class::Command,
                ] {
                    let opened = side.session.open_sender(class).await;
                    let mut sender = opened.expect("a stream");
                    sender.send(side.block(b"a")).await.expect("sent");
                    senders.push(sender);
                }
                let closed = Error::PeerClosed { code: Code(4) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                side.node.clock().sleep(spans(Span::MILLISECOND, 200)).await;
                let mut classes = Vec::new();
                for _ in 0..4 {
                    let incoming = side.session.accept().await.expect("a stream");
                    classes.push(incoming.class);
                }
                let order = [
                    Class::Command,
                    Class::Latest,
                    Class::Complete,
                    Class::CatchUp,
                ];
                assert_eq!(classes, order);
                side.session.close(Code(4));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_close_by_the_peer_ends_each_call_that_waits_and_each_later_one() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| config,
            |side| async move {
                let opened = side.session.open(Class::Complete).await;
                let (mut sender, _receiver) = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                side.node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
                side.session.close(Code(5));
                let closed = Error::Closed { code: Code(5) };
                assert_eq!(side.session.closed().await, closed);
            },
            |side| async move {
                let incoming = side.session.accept().await.expect("a stream");
                let (mut receiver, mut sender) = (incoming.receiver, incoming.sender);
                let read = receiver.recv().await.map(|m| m.map(|block| block.to_vec()));
                assert_eq!(read, Ok(Some(b"a".to_vec())));
                let closed = Error::PeerClosed { code: Code(5) };
                let (accepted, read) =
                    join(side.session.accept(), receiver.recv()).await;
                assert_eq!(accepted.map(|_| ()), Err(closed.clone()));
                assert_eq!(read.map(|_| ()), Err(closed.clone()));
                let opened = side.session.open(Class::Complete).await;
                assert_eq!(opened.map(|_| ()), Err(closed.clone()));
                let opened = side.session.open_sender(Class::Complete).await;
                assert_eq!(opened.map(|_| ()), Err(closed.clone()));
                let sender = sender.as_mut().expect("a reply half");
                assert_eq!(sender.send(side.block(b"b")).await, Err(closed.clone()));
                assert_eq!(sender.finish(), Err(closed.clone()));
                assert_eq!(receiver.recv().await.map(|_| ()), Err(closed));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_close_ends_a_recv_that_waits_alone() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| config,
            |side| async move {
                let mut incoming = side.session.accept().await.expect("a stream");
                let received = incoming.receiver.recv().await;
                assert_eq!(
                    received.map(|m| m.map(|m| m.to_vec())),
                    Ok(Some(b"a".to_vec()))
                );
                let closed = Error::PeerClosed { code: Code(8) };
                assert_eq!(incoming.receiver.recv().await.map(|_| ()), Err(closed));
            },
            |side| async move {
                let opened = side.session.open_sender(Class::Complete).await;
                let mut sender = opened.expect("a stream");
                sender.send(side.block(b"a")).await.expect("sent");
                side.node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
                side.session.close(Code(8));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_close_ends_an_accept_that_waits_alone() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| config,
            |side| async move {
                let closed = Error::PeerClosed { code: Code(6) };
                let accepted = side.session.accept().await;
                assert_eq!(accepted.map(|_| ()), Err(closed));
            },
            |side| async move {
                side.node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
                side.session.close(Code(6));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_close_ends_an_open_that_waits_for_the_peer_to_allow_a_stream() {
        let (mut sim, ..) = testing::sessions(
            0,
            |config| config,
            |side| async move {
                let mut held = Vec::new();
                for _ in 0..testing::STREAMS_MAX {
                    held.push(
                        side.session.open(Class::Complete).await.expect("a stream"),
                    );
                }
                let mut opened = pin!(side.session.open(Class::Complete));
                assert!(poll_once(opened.as_mut()).await.is_none());
                let closed = Error::PeerClosed { code: Code(7) };
                assert_eq!(opened.await.map(|_| ()), Err(closed));
            },
            |side| async move {
                side.node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
                side.session.close(Code(7));
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_stream_keeps_its_session_open_after_the_session_drops() {
        let held = Span::from_nanos(2 * IDLE.nanos());
        let (mut sim, ..) = testing::sessions(
            0,
            |config| config,
            move |side| async move {
                let opened = side.session.open(Class::Complete).await;
                let (mut sender, mut receiver) = opened.expect("a stream");
                drop(side.session);
                side.node.clock().sleep(held).await;
                sender
                    .send(testing::block(&side.pool, b"a"))
                    .await
                    .expect("sent");
                sender.finish().expect("finished");
                let read = receiver.recv().await.map(|m| m.map(|block| block.to_vec()));
                assert_eq!(read, Ok(Some(b"b".to_vec())));
            },
            |side| async move {
                let incoming = side.session.accept().await.expect("a stream");
                let (mut receiver, mut sender) = (incoming.receiver, incoming.sender);
                let read = receiver.recv().await.map(|m| m.map(|block| block.to_vec()));
                assert_eq!(read, Ok(Some(b"a".to_vec())));
                let sender = sender.as_mut().expect("a reply half");
                sender.send(side.block(b"b")).await.expect("sent");
                sender.finish().expect("finished");
                let closed = Error::PeerClosed { code: Code(0) };
                assert_eq!(side.session.closed().await, closed);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    const LEN: usize = 40_000;

    #[test]
    fn a_read_that_waits_for_budget_room_lets_a_read_of_another_session_take_a_block() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::shard(&server, SERVER, |config, node| async move {
            let memory = block::Config {
                budget: 3 * block::footprint(LEN),
            };
            let heap = block::Heap::new(memory.reservation());
            let pool = Rc::new(block::Pool::new(memory, heap));
            // The receive budget is the window plus the largest message: 100_000.
            let config = Config {
                message_bytes_max: NonZeroUsize::new(50_000).expect("not zero"),
                window_bytes: 50_000,
                pool: Rc::clone(&pool),
                ..config
            };
            let part = testing::part(&node.net(), testing::address(&node));
            let transport = Transport::new(config, part).expect("a transport");
            let first = transport.accept().await.expect("a session");
            let second = transport.accept().await.expect("a session");
            let clock = node.clock();
            let held = [(); 2].map(|()| pool.alloc(LEN).expect("room"));
            let mut a = first.accept().await.expect("a stream").receiver;
            let mut b = second.accept().await.expect("a stream").receiver;
            let mut stalled = Vec::new();
            for _ in 0..2 {
                let incoming = first.accept().await.expect("a stream");
                assert_eq!(incoming.class, Class::Command);
                stalled.push(incoming.receiver);
            }
            // Each `Command` message holds 40_000 bytes of the first session's budget
            // and no block, so `a` has no room while the pool has one block.
            for receiver in &mut stalled {
                assert!(poll_once(pin!(receiver.recv())).await.is_none());
            }
            let mut a_read = Box::pin(a.recv());
            let mut b_read = Box::pin(b.recv());
            let read = poll_fn(|cx| {
                assert!(a_read.as_mut().poll(cx).is_pending());
                b_read.as_mut().poll(cx)
            });
            let message = read.await.expect("a message").expect("a block");
            assert_eq!(message.to_vec(), vec![2; LEN]);
            drop((message, stalled, held));
            let message = a_read.await.expect("a message").expect("a block");
            assert_eq!(message.to_vec(), vec![1; LEN]);
            first.close(Code(4));
            second.close(Code(4));
            clock.sleep(Span::MILLISECOND).await;
        });
        testing::shard(&client, CLIENT, move |config, node| async move {
            send_and_stall(config, node, at).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    /// Dials the server two times. Sends a message of [`LEN`] bytes of 1 on the first
    /// session and one of 2 on the second, then on the first two `Command` streams
    /// that each hold the prefix of a message of [`LEN`] bytes and only part of it.
    /// Waits for the server to close each session with code 4.
    async fn send_and_stall(config: Config, node: sim::node::Node, at: [Address; 1]) {
        let pool = Rc::clone(&config.pool);
        let transports = testing::transports(&config, &node, 2);
        let server = SERVER.public();
        let first = transports[0].dial(server, &at).await.expect("a session");
        let second = transports[1].dial(server, &at).await.expect("a session");
        for (session, byte) in [(&first, 1), (&second, 2)] {
            let opened = session.open_sender(Class::Complete).await;
            let mut sender = opened.expect("a stream");
            sender
                .send(testing::block(&pool, &vec![byte; LEN]))
                .await
                .expect("sent");
            sender.finish().expect("finished");
        }
        node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
        let command = 0;
        let part = [&[command], &*message::prefix(LEN), &[3; 1_000]].concat();
        for _ in 0..2 {
            first.0.raw(&part);
        }
        let closed = Error::PeerClosed { code: Code(4) };
        assert_eq!(first.closed().await, closed);
        assert_eq!(second.closed().await, closed);
    }
}
