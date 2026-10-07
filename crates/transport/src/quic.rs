//! The QUIC carrier: noq-proto with time, datagrams, and randomness as inputs.

mod carrier;
mod cid;
pub(crate) mod connection;
mod datagram;
#[cfg_attr(
    not(feature = "fuzzing"),
    expect(unreachable_pub, reason = "only the fuzzing feature exports it")
)]
mod hello;
#[cfg(test)]
mod pair;
mod settings;
mod stateless;
pub(crate) mod stream;
mod wait;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::task::Poll;
use std::time::{Duration, Instant};

use block::{Block, Pool, Unique};
use bytes::{Bytes, BytesMut};
use env::net::Ecn;
use env::net::udp::{Meta, Transmit};
use noq_proto::{
    ConnectionHandle, DatagramEvent, Dir, EcnCodepoint, FourTuple, SendDatagramError,
};
use types::node::PublicKey;
use types::time::Monotonic;

use self::connection::Connection;
use self::settings::Settings;
use self::stream::{Incoming, Receiver, Sender, Streams};
use crate::{Class, Code, Config, Error, Peer};

pub(crate) use self::carrier::{Carrier, Session};
#[cfg(feature = "fuzzing")]
pub use self::hello::Hello;

/// The server name a dial sends. The verifiers check the node key, not the name.
const SERVER_NAME: &str = "foundation";

/// The most responses that wait for [`Endpoint::transmit`]. A peer makes one with
/// each datagram it sends, so a later one is dropped, as the network may drop it.
const RESPONSES_MAX: usize = 64;

/// One shard's QUIC endpoint and its connections, with no I/O. The caller gives it
/// the time and the datagrams that arrive, and takes from it the datagrams to send,
/// the next deadline, and connection events. The same entropy and inputs give the
/// same outputs, apart from the bytes that TLS makes.
pub(crate) struct Endpoint {
    /// The instant at `Monotonic(0)`.
    epoch: Instant,
    settings: Settings,
    inner: noq_proto::Endpoint,
    /// The most datagrams in one [`Transmit`].
    datagrams_max: NonZeroUsize,
    /// The pool that each received message goes into.
    pool: Rc<Pool>,
    /// The largest message a receiver takes.
    message_bytes_max: usize,
    /// The most bytes in flight on a connection in each direction.
    window_bytes: usize,
    /// Indexed by noq-proto's handle.
    connections: Vec<Option<Connection>>,
    /// The connections made so far.
    serial: u64,
    /// The connections that may have a datagram to send, each once, in the order
    /// [`Endpoint::transmit`] polls them.
    ready: VecDeque<connection::Key>,
    events: VecDeque<Event>,
    /// Datagrams that no connection sends, such as a version negotiation or a
    /// stateless reset. At most [`RESPONSES_MAX`].
    responses: VecDeque<(noq_proto::Transmit, Vec<u8>)>,
    /// Each connection that a peer dials is refused.
    refusing: bool,
    /// The limit on stateless resets to each address.
    resets: stateless::Limit,
    /// The buffer that each received batch is copied into and split from. noq-proto
    /// decrypts in place and keeps parts of it.
    received: BytesMut,
}

/// A change to a connection that the caller must know about.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The handshake finished and `peer` proved its key, or is a client with no
    /// key. An accepted connection's key is new here.
    Connected { key: connection::Key, peer: Peer },
    /// The connection ended. Each key the caller has, from
    /// [`Endpoint::connect`] or [`Event::Connected`], gets this once.
    Closed { key: connection::Key, error: Error },
    /// [`Endpoint::accept`] has a stream for `key`.
    Incoming { key: connection::Key },
    /// [`Endpoint::open`] and [`Endpoint::open_sender`] may now give a stream. It
    /// comes first when the peer's hello arrives. A peer may never send its hello:
    /// the caller bounds that wait.
    Available { key: connection::Key },
    /// `stream` may have more to read. It can repeat, and it can name a stream the
    /// caller no longer holds or has not accepted yet.
    Readable { stream: stream::Key },
    /// `stream` may take more, or the peer stopped it. It can repeat, and it can
    /// name a stream the caller no longer holds or has not accepted yet.
    Writable { stream: stream::Key },
    /// [`Endpoint::datagrams`] has a datagram of `key` to take. It comes when one
    /// arrives and none waited, so take them all after it.
    Datagram { key: connection::Key },
}

impl Endpoint {
    /// An endpoint for this node's key whose connection IDs all start with
    /// `shard`. Each [`Transmit`] holds at most `datagrams_max` datagrams, the
    /// socket's batch max, and at most
    /// [`TRANSMIT_BYTES_MAX`](env::net::udp::TRANSMIT_BYTES_MAX) bytes.
    ///
    /// # Panics
    ///
    /// When [`Transport::new`](crate::Transport::new) refuses `config`, with its error.
    pub(crate) fn new(config: &Config, shard: u8, datagrams_max: NonZeroUsize) -> Self {
        if let Err(error) = config.check() {
            panic!("{error}");
        }
        let (settings, endpoint) = Settings::new(config, shard);
        Self {
            epoch: config.clock.epoch(),
            settings,
            inner: endpoint,
            datagrams_max: datagrams_max.min(settings::BATCH_MAX),
            pool: Rc::clone(&config.pool),
            message_bytes_max: config.message_bytes_max.get(),
            window_bytes: config.window_bytes,
            connections: Vec::new(),
            serial: 0,
            ready: VecDeque::new(),
            events: VecDeque::new(),
            responses: VecDeque::new(),
            refusing: false,
            resets: stateless::Limit::new(&config.entropy),
            received: BytesMut::new(),
        }
    }

    /// Dials `remote` and expects it to prove `peer`. The dial ends in
    /// [`Event::Connected`] or [`Event::Closed`] for the key.
    ///
    /// # Panics
    ///
    /// When no datagram can go to `remote`: its port is 0 or its IP is unspecified.
    pub(crate) fn connect(
        &mut self,
        now: Monotonic,
        peer: PublicKey,
        remote: SocketAddr,
    ) -> connection::Key {
        let now = self.instant(now);
        let dial = self.settings.client(peer);
        let (handle, inner) = self
            .inner
            .connect(now, dial, remote, SERVER_NAME)
            .unwrap_or_else(|error| {
                panic!("a dial fails only on its address: {error}")
            });
        let key = self.insert(handle, |key, streams| {
            Connection::dialed(key, inner, peer, streams)
        });
        self.drive(handle, now);
        key
    }

    /// Takes one received batch: `meta.len` bytes of `batch`, in datagrams of
    /// `meta.stride` bytes.
    ///
    /// # Panics
    ///
    /// When `meta.len` is more than `batch.len()`, or when `meta.stride` is 0 and
    /// `meta.len` is not.
    pub(crate) fn receive(&mut self, now: Monotonic, meta: &Meta, batch: &[u8]) {
        let now = self.instant(now);
        let path = FourTuple::new(meta.source, meta.destination);
        let ecn = meta.ecn.map(codepoint);
        assert!(
            meta.stride > 0 || meta.len == 0,
            "invariant: a batch of {} bytes has a stride",
            meta.len
        );
        self.received.extend_from_slice(&batch[..meta.len]);
        let mut datagrams = self.received.split();
        while !datagrams.is_empty() {
            let datagram = datagrams.split_to(meta.stride.min(datagrams.len()));
            self.handle(now, path, ecn, datagram);
        }
    }

    /// The next datagrams to send, all to one destination, written into `buffer`.
    /// `None` when nothing is due. Call it until `None` after each other call but
    /// [`Endpoint::deadline`] and [`Endpoint::poll`], and after [`Datagrams::send`].
    pub(crate) fn transmit<'a>(
        &mut self,
        now: Monotonic,
        buffer: &'a mut Vec<u8>,
    ) -> Option<Transmit<'a>> {
        if let Some((transmit, bytes)) = self.responses.pop_front() {
            buffer.clear();
            buffer.extend_from_slice(&bytes);
            return Some(outgoing(&transmit, buffer));
        }
        let (now, datagrams_max) = (self.instant(now), self.datagrams_max);
        while let Some(key) = self.ready.pop_front() {
            let Some(connection) = find(&mut self.connections, key) else {
                continue;
            };
            connection.queued = false;
            buffer.clear();
            if let Some(transmit) =
                connection.inner.poll_transmit(now, datagrams_max, buffer)
            {
                self.drive(key.handle, now);
                return Some(outgoing(&transmit, buffer));
            }
        }
        None
    }

    /// When [`Endpoint::timeout`] must next run, or `None` with no connection.
    pub(crate) fn deadline(&self) -> Option<Monotonic> {
        let connections = self.connections.iter().flatten();
        let deadline = connections
            .filter_map(|connection| connection.inner.poll_timeout())
            .min()?;
        let nanos = deadline.duration_since(self.epoch).as_nanos();
        Some(Monotonic(u64::try_from(nanos).unwrap_or(u64::MAX)))
    }

    /// Runs the timers due at `now`: loss detection, keep-alives, and the idle
    /// timeout.
    pub(crate) fn timeout(&mut self, now: Monotonic) {
        let now = self.instant(now);
        for handle in (0..self.connections.len()).map(ConnectionHandle) {
            let Some(connection) = &mut self.connections[handle.0] else {
                continue;
            };
            if connection
                .inner
                .poll_timeout()
                .is_some_and(|deadline| deadline <= now)
            {
                connection.inner.handle_timeout(now);
                self.drive(handle, now);
            }
        }
    }

    /// Refuses each connection that a peer dials from now on. The peer's dial ends
    /// at once.
    pub(crate) fn refuse(&mut self) {
        self.refusing = true;
    }

    /// `true` when each connection drained, so none sends again.
    pub(crate) fn drained(&self) -> bool {
        self.connections.iter().all(Option::is_none)
    }

    /// Ends each connection after the socket broke, and queues the
    /// [`Event::Closed`] of each one the caller has, with [`Error::Network`].
    pub(crate) fn fail(&mut self, error: &env::net::Error) {
        let connections = self.connections.iter_mut().flatten();
        let closed = connections.filter_map(|connection| connection.fail(error));
        self.events.extend(closed);
    }

    /// Closes the connection of `key` with `code`, and queues its [`Event::Closed`]
    /// with [`Error::Closed`]. Does nothing when the connection already ended: its
    /// [`Event::Closed`] is queued or was given.
    pub(crate) fn close(&mut self, now: Monotonic, key: connection::Key, code: Code) {
        let now = self.instant(now);
        let Some(connection) = find(&mut self.connections, key) else {
            return;
        };
        let closed = connection.close(now, code);
        self.events.extend(closed);
        self.drive(key.handle, now);
    }

    /// Opens a stream of `class` that goes both ways. `None` until the peer's hello
    /// arrives or while the peer allows no more streams ([`Event::Available`]
    /// follows), and when the connection ended. The peer sees the stream at its first
    /// message.
    #[expect(clippy::unwrap_in_result, reason = "a stream both ways has a receiver")]
    pub(crate) fn open(
        &mut self,
        now: Monotonic,
        key: connection::Key,
        class: Class,
    ) -> Option<(Sender, Receiver)> {
        let (sender, receiver) = self.start(now, key, Dir::Bi, class)?;
        let receiver = receiver.expect("invariant: a stream both ways has a receiver");
        Some((sender, receiver))
    }

    /// As [`Endpoint::open`], for a stream that only this side sends on.
    pub(crate) fn open_sender(
        &mut self,
        now: Monotonic,
        key: connection::Key,
        class: Class,
    ) -> Option<Sender> {
        let (sender, _) = self.start(now, key, Dir::Uni, class)?;
        Some(sender)
    }

    /// The next stream the peer opened on `key`'s connection, highest class first.
    /// `None` when there is none now ([`Event::Incoming`] follows), or when the
    /// connection ended.
    pub(crate) fn accept(&mut self, key: connection::Key) -> Option<Incoming> {
        let connection = find(&mut self.connections, key).filter(|c| c.live())?;
        connection.streams.accept(key)
    }

    /// Puts `message`, when `Some`, on the stream after the messages before it, and
    /// takes it. Leaves it while the stream holds part of an earlier message.
    /// `Ready` when the stream holds no message: it took all of `message`, or with
    /// `None`, all of the one before. Else `Pending`: write again after
    /// [`Event::Writable`] to send the rest. The streams that wait for the
    /// connection take turns, by class with `Complete` ahead of `Latest` while it is
    /// owed bytes, then oldest first, so a write behind one waits.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over the peer's largest message.
    /// Nothing of it is sent, and it stays in `message`. Then the error of the
    /// connection's [`Event::Closed`] once it ended. Then [`Error::Reset`] with
    /// `Code(0)` after an [`Endpoint::cancel`] reset the stream, and
    /// [`Error::Stopped`] when the peer stopped it; each later write gives it too.
    ///
    /// # Panics
    ///
    /// After an [`Endpoint::finish`] that gave `Ok`.
    pub(crate) fn write(
        &mut self,
        now: Monotonic,
        sender: &Sender,
        message: &mut Option<Block>,
    ) -> Result<Poll<()>, Error> {
        sender.check_open();
        if let Some(message) = message {
            stream::check_size(message.len(), sender.bytes_max())?;
        }
        let key = sender.key().connection;
        self.streams(
            now,
            key,
            sender.closed().cloned(),
            |streams, inner, _, _| streams.write(inner, sender, message),
        )
    }

    /// Puts `message` on the stream after the messages before it when the stream
    /// can take it now. Else gives it back with nothing of it sent: when the stream
    /// still holds part of an earlier message once it wrote what it could of it,
    /// when the send budget has no room for it or a stream that goes ahead of it
    /// waits for room or its turn. The stream does not wait for room for a message
    /// it gives back. Once taken, the stream sends the rest of it by itself.
    ///
    /// # Errors
    ///
    /// As [`Endpoint::write`]. Nothing of `message` is sent.
    ///
    /// # Panics
    ///
    /// After an [`Endpoint::finish`] that gave `Ok`.
    pub(crate) fn try_write(
        &mut self,
        now: Monotonic,
        sender: &Sender,
        message: Block,
    ) -> Result<Option<Block>, Error> {
        sender.check_open();
        stream::check_size(message.len(), sender.bytes_max())?;
        let key = sender.key().connection;
        let mut message = Some(message);
        self.streams(
            now,
            key,
            sender.closed().cloned(),
            |streams, inner, _, _| streams.try_write(inner, sender, &mut message),
        )?;
        Ok(message)
    }

    /// Ends the stream after the messages written to it, the rest of the one in hand
    /// included. They arrive after the caller drops `sender`. A stream this side
    /// opened that ends before its first message never reaches the peer, and the
    /// [`Receiver`] of a two-way one gets [`Error::Reset`] with code 0.
    ///
    /// # Errors
    ///
    /// The error of the connection's [`Event::Closed`] once it ended. Then
    /// [`Error::Reset`] with `Code(0)` after an [`Endpoint::cancel`] reset the
    /// stream, and [`Error::Stopped`] when the peer stopped it; each later finish
    /// gives it too.
    ///
    /// # Panics
    ///
    /// After an [`Endpoint::finish`] that gave `Ok`.
    pub(crate) fn finish(
        &mut self,
        now: Monotonic,
        sender: &mut Sender,
    ) -> Result<(), Error> {
        sender.check_open();
        let (stream, closed) = (sender.key(), sender.closed().cloned());
        self.streams(now, stream.connection, closed, |streams, inner, _, _| {
            streams.finish(inner, stream.id)?;
            sender.end();
            Ok(())
        })
    }

    /// The next whole message of `receiver`'s stream, in the block that
    /// `take(pool, len)` gives from the endpoint's pool: exactly `len` bytes, or
    /// `None` when the message may not have one now. `Ready(None)` after the last
    /// one, and on each call after that. `Pending` when no whole message is here yet
    /// ([`Event::Readable`] follows), when `take` gives no block (no event follows:
    /// call again once it may give one).
    ///
    /// # Errors
    ///
    /// - [`Error::Reset`] when the peer reset the stream. Each later read gives it
    ///   too, also once the connection ended.
    /// - Else the error of the connection's [`Event::Closed`] once it ended, also
    ///   when the stream has a message or a reset that no read took. And
    ///   [`Error::Broken`] when the read finds a fault of the peer's.
    pub(crate) fn read(
        &mut self,
        now: Monotonic,
        receiver: &mut Receiver,
        mut take: impl FnMut(&Pool, usize) -> Option<Unique>,
    ) -> Result<Poll<Option<Block>>, Error> {
        if let Some(ended) = receiver.ended() {
            return ended;
        }
        let (key, closed) = (receiver.key().connection, receiver.closed().cloned());
        self.streams(now, key, closed, |streams, inner, pool, events| {
            streams.read(inner, receiver, |len| take(pool, len), events)
        })
    }

    /// Resets `sender`'s stream with `code`. The peer's next read gives
    /// [`Error::Reset`], and the messages it has not read drop, unless it acknowledged
    /// all of the stream. The send budget of the message in hand comes back now, and
    /// the stream's blocks go back to the pool at the latest when the peer
    /// acknowledges the reset. A stream this side opened that resets before its first
    /// message never reaches the peer, and the [`Receiver`] of a two-way one gets
    /// [`Error::Reset`] with code 0. Does nothing when the connection ended. Each
    /// later write or finish with `sender` panics.
    pub(crate) fn reset(&mut self, now: Monotonic, sender: &mut Sender, code: Code) {
        sender.end();
        let key = sender.key().connection;
        let Some(connection) = find(&mut self.connections, key).filter(|c| c.live())
        else {
            return;
        };
        let Connection { inner, streams, .. } = connection;
        streams.reset(inner, sender, code);
        self.drive(key.handle, self.instant(now));
    }

    /// Cancels the message that `sender`'s stream took from the last
    /// [`Endpoint::write`] and holds. When no byte of it went, its header included,
    /// the message drops and the stream stays open. Else the stream resets with
    /// `Code(0)`, as [`Endpoint::reset`] does, and each later write and finish gives
    /// [`Error::Reset`] with `Code(0)`. Either way the send budget and the turn of
    /// the message come back now. Does nothing when the connection ended.
    pub(crate) fn cancel(&mut self, now: Monotonic, sender: &Sender) {
        let key = sender.key().connection;
        let Some(connection) = find(&mut self.connections, key).filter(|c| c.live())
        else {
            return;
        };
        let Connection { inner, streams, .. } = connection;
        streams.cancel(inner, sender);
        self.drive(key.handle, self.instant(now));
    }

    /// Stops `receiver`'s stream with `code`. The messages not read yet drop, and the
    /// peer's next write gives [`Error::Stopped`]. The message in the reader drops, and
    /// its receive budget comes back. Sends nothing after a read of the end. Does
    /// nothing when the connection ended.
    pub(crate) fn stop(&mut self, now: Monotonic, receiver: Receiver, code: Code) {
        let key = receiver.key().connection;
        let Some(connection) = find(&mut self.connections, key).filter(|c| c.live())
        else {
            return;
        };
        let Connection { inner, streams, .. } = connection;
        streams.stop(inner, receiver, code, &mut self.events);
        self.drive(key.handle, self.instant(now));
    }

    /// Ends a read of `receiver` that waits for room in the receive budget, and gives
    /// back room that the read got and has not taken, for a caller that gives up the
    /// read and keeps the stream. The next read waits again, behind the reads that
    /// wait then. Does nothing when no read waits for room, or when the connection
    /// ended.
    pub(crate) fn end_wait(&mut self, receiver: &mut Receiver) {
        let key = receiver.key().connection;
        let Some(connection) = find(&mut self.connections, key).filter(|c| c.live())
        else {
            return;
        };
        connection.streams.end_wait(receiver, &mut self.events);
    }

    /// The datagrams of `key`'s connection. `None` until it connects, and after it
    /// ends.
    pub(crate) fn datagrams(&mut self, key: connection::Key) -> Option<Datagrams<'_>> {
        let connection = find(&mut self.connections, key).filter(|c| c.connected())?;
        let ready = &mut self.ready;
        Some(Datagrams { connection, ready })
    }

    /// The next event, in the order they happened.
    pub(crate) fn poll(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    fn instant(&self, now: Monotonic) -> Instant {
        self.epoch + Duration::from_nanos(now.0)
    }

    /// Opens a stream of `class` in `dir` on the connection of `key`, unless it ended,
    /// and gives its halves.
    fn start(
        &mut self,
        now: Monotonic,
        key: connection::Key,
        dir: Dir,
        class: Class,
    ) -> Option<(Sender, Option<Receiver>)> {
        let connection = find(&mut self.connections, key).filter(|c| c.live())?;
        let sender = connection
            .streams
            .open(&mut connection.inner, key, dir, class);
        self.drive(key.handle, self.instant(now));
        sender
    }

    /// Runs `call` on the streams of `key`'s connection with the pool and the event
    /// queue, and drives the connection. Gives `closed`, the error a handle of the
    /// connection keeps of its [`Event::Closed`], when it ended. A fault of the
    /// peer's that `call` finds closes the connection, and this gives it.
    fn streams<T>(
        &mut self,
        now: Monotonic,
        key: connection::Key,
        closed: Option<Error>,
        call: impl FnOnce(
            &mut Streams,
            &mut noq_proto::Connection,
            &Pool,
            &mut VecDeque<Event>,
        ) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if let Some(error) = closed {
            return Err(error);
        }
        let now = self.instant(now);
        let connection = find(&mut self.connections, key);
        let connection =
            connection.expect("invariant: a connection drains only after it closed");
        let Connection { inner, streams, .. } = connection;
        let result = call(streams, inner, &self.pool, &mut self.events);
        if let Err(Error::Broken { reason }) = &result {
            self.events.extend(connection.fault(now, reason.clone()));
        }
        self.drive(key.handle, now);
        result
    }

    fn insert(
        &mut self,
        handle: ConnectionHandle,
        connection: impl FnOnce(connection::Key, Streams) -> Connection,
    ) -> connection::Key {
        let key = connection::Key {
            handle,
            serial: self.serial,
        };
        self.serial += 1;
        if self.connections.len() <= handle.0 {
            self.connections.resize_with(handle.0 + 1, || None);
        }
        let entry = &mut self.connections[handle.0];
        assert!(
            entry.is_none(),
            "invariant: noq-proto reuses a drained handle"
        );
        let streams = Streams::new(self.window_bytes, self.message_bytes_max);
        *entry = Some(connection(key, streams));
        key
    }

    fn handle(
        &mut self,
        now: Instant,
        path: FourTuple,
        ecn: Option<EcnCodepoint>,
        datagram: BytesMut,
    ) {
        if dropped(&datagram) {
            return;
        }
        // noq-proto answers a short header only with a stateless reset.
        let short = datagram.first().is_some_and(|form| form & 0x80 == 0);
        let mut reply = Vec::new();
        let event = self.inner.handle(now, path, ecn, datagram, &mut reply);
        let response = match event {
            None => None,
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                let connection = self.connections[handle.0]
                    .as_mut()
                    .expect("invariant: noq-proto routes only to a live handle");
                connection.inner.handle_event(event);
                self.drive(handle, now);
                None
            }
            Some(DatagramEvent::NewConnection(incoming)) if self.refusing => {
                Some(self.inner.refuse(incoming, &mut reply))
            }
            Some(DatagramEvent::NewConnection(incoming)) => {
                match self.inner.accept(incoming, now, &mut reply, None) {
                    Ok((handle, inner)) => {
                        self.insert(handle, |key, streams| {
                            Connection::accepted(key, inner, streams)
                        });
                        self.drive(handle, now);
                        None
                    }
                    Err(error) => error.response,
                }
            }
            Some(DatagramEvent::Response(response)) => {
                let admitted =
                    !short || self.resets.admit(now, response.destination.ip());
                admitted.then_some(response)
            }
        };
        if let Some(response) = response
            && self.responses.len() < RESPONSES_MAX
        {
            self.responses.push_back((response, reply));
        }
    }

    /// Drives `handle`'s connection at `now`. Frees it once it drained, and else
    /// queues it for [`Endpoint::transmit`]. Each call that can give it a datagram to
    /// send ends here, but [`Datagrams::send`], which queues it itself.
    fn drive(&mut self, handle: ConnectionHandle, now: Instant) {
        let entry = &mut self.connections[handle.0];
        let connection = entry.as_mut().expect("invariant: a live handle");
        if connection.drive(now, &mut self.inner, &self.pool, &mut self.events) {
            *entry = None;
        } else {
            queue(&mut self.ready, connection);
        }
    }
}

/// The datagrams of one connected connection: whole messages, each in one QUIC
/// DATAGRAM frame, that may be lost.
pub(crate) struct Datagrams<'a> {
    connection: &'a mut Connection,
    /// The endpoint's queue for [`Endpoint::transmit`].
    ready: &'a mut VecDeque<connection::Key>,
}

impl Datagrams<'_> {
    /// Queues `message` as one datagram. It never waits: when the queue is full, the
    /// oldest unsent datagram drops.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over [`Datagrams::bytes_max`], or the
    /// peer takes no datagrams.
    pub(crate) fn send(&mut self, message: Block) -> Result<(), Error> {
        let bytes = message.len();
        let mut datagrams = self.connection.inner.datagrams();
        match datagrams.send(Bytes::from_owner(Body(message)), true) {
            Ok(()) => {}
            Err(SendDatagramError::TooLarge | SendDatagramError::UnsupportedByPeer) => {
                let bytes_max = datagrams.max_size().unwrap_or(0);
                return Err(Error::TooLarge { bytes, bytes_max });
            }
            Err(
                error @ (SendDatagramError::Disabled | SendDatagramError::Blocked(_)),
            ) => {
                panic!(
                    "invariant: datagrams are on, and a send that drops never blocks: {error}"
                )
            }
        }
        queue(self.ready, self.connection);
        Ok(())
    }

    /// The oldest datagram that arrived and was not taken.
    pub(crate) fn receive(&mut self) -> Option<Block> {
        self.connection.datagrams.pop()
    }

    /// The largest datagram [`Datagrams::send`] takes now. It changes with the path,
    /// and is 0 when the peer takes no datagrams.
    pub(crate) fn bytes_max(&mut self) -> usize {
        self.connection.inner.datagrams().max_size().unwrap_or(0)
    }
}

/// Whether the endpoint drops `datagram` unread: a long header of a version it does
/// not speak, in fewer than [`MTU_MIN`](settings::MTU_MIN) bytes. noq-proto 1.3.0
/// answers such a header at any size, which QUIC forbids, so a spoofed source would
/// get more bytes than it sent (#534). Version 0 is a version negotiation for a dial,
/// so it passes.
fn dropped(datagram: &[u8]) -> bool {
    match *datagram {
        [form, a, b, c, d, ..]
            if form & 0x80 != 0 && datagram.len() < usize::from(settings::MTU_MIN) =>
        {
            let version = u32::from_be_bytes([a, b, c, d]);
            version != 0 && !settings::VERSIONS.contains(&version)
        }
        _ => false,
    }
}

/// Puts `connection` in `ready`, the queue for [`Endpoint::transmit`], unless it is
/// there.
fn queue(ready: &mut VecDeque<connection::Key>, connection: &mut Connection) {
    if !connection.queued {
        connection.queued = true;
        ready.push_back(connection.key);
    }
}

/// The connection of `key` in `connections`, until it drains.
fn find(
    connections: &mut [Option<Connection>],
    key: connection::Key,
) -> Option<&mut Connection> {
    let connection = connections.get_mut(key.handle.0)?.as_mut()?;
    (connection.key == key).then_some(connection)
}

/// A message that noq-proto holds until it needs the bytes no more.
struct Body(Block);

impl AsRef<[u8]> for Body {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

fn outgoing<'a>(transmit: &noq_proto::Transmit, buffer: &'a [u8]) -> Transmit<'a> {
    Transmit {
        destination: transmit.destination,
        source: transmit.src_ip,
        ecn: transmit.ecn.map(ecn),
        contents: &buffer[..transmit.size],
        segment: transmit.segment_size.map(|size| {
            NonZeroUsize::new(size)
                .expect("invariant: noq-proto segments are not empty")
        }),
    }
}

fn codepoint(ecn: Ecn) -> EcnCodepoint {
    match ecn {
        Ecn::Ect0 => EcnCodepoint::Ect0,
        Ecn::Ect1 => EcnCodepoint::Ect1,
        Ecn::Ce => EcnCodepoint::Ce,
    }
}

fn ecn(codepoint: EcnCodepoint) -> Ecn {
    match codepoint {
        EcnCodepoint::Ect0 => Ecn::Ect0,
        EcnCodepoint::Ect1 => Ecn::Ect1,
        EcnCodepoint::Ce => Ecn::Ce,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use bytes::Bytes;
    use noq_proto::{Dir, VarInt};
    use types::node::PrivateKey;
    use types::time::Span;

    use super::*;
    use crate::quic::pair::{self, Pair, Side};
    use crate::testing;
    use crate::tls;

    /// The link delay each way in [`dial`].
    const DELAY: Duration = Duration::from_millis(10);

    /// A dial to a server that must prove `peer`, after 100 ms.
    fn dial(shard: &testing::Shard, peer: PublicKey) -> Pair {
        let mut pair = Pair::new(shard, Span::SECOND, DELAY);
        pair.dial(peer);
        pair.run(Duration::from_millis(100));
        pair
    }

    fn server() -> PublicKey {
        tls::public(&pair::SERVER_KEY)
    }

    fn events(side: &Side) -> Vec<&Event> {
        side.events.iter().map(|(_, event)| event).collect()
    }

    fn available(side: &Side) -> Event {
        let key = side.key.expect("a connection");
        Event::Available { key }
    }

    fn closed(side: &Side, error: Error) -> Event {
        let key = side.key.expect("a connection");
        Event::Closed { key, error }
    }

    /// Gives each datagram the client has now to the server at once, as if the OS
    /// gave `destination` and `ecn`.
    fn deliver(pair: &mut Pair, destination: Option<IpAddr>, ecn: Option<Ecn>) {
        let (now, mut buffer) = (pair.now(), Vec::new());
        while let Some(transmit) = pair.client.endpoint.transmit(now, &mut buffer) {
            let meta = Meta {
                destination,
                ecn,
                ..pair::meta(pair::CLIENT, transmit.contents)
            };
            pair.server.endpoint.receive(now, &meta, transmit.contents);
        }
    }

    /// Writes 100 bytes on a new stream of `side`.
    fn write(side: &mut Side) {
        let connection = side.connection();
        let stream = connection.streams().open(Dir::Uni).expect("a stream");
        let mut send = connection.send_stream(stream);
        assert_eq!(send.write(&[0; 100]).expect("written"), 100);
    }

    mod connect {
        use super::*;

        #[test]
        fn proves_each_node_key_to_the_other() {
            testing::run(1, |shard| {
                let pair = dial(shard, server());
                let connected = |side: &Side, key| Event::Connected {
                    key: side.key.expect("a connection"),
                    peer: Peer::Node(tls::public(key)),
                };
                let client = connected(&pair.client, &pair::SERVER_KEY);
                let available = available(&pair.client);
                assert_eq!(events(&pair.client), [&client, &available]);
                let server = connected(&pair.server, &pair::CLIENT_KEY);
                let available = super::available(&pair.server);
                assert_eq!(events(&pair.server), [&server, &available]);
            });
        }

        #[test]
        fn to_another_key_fails_authentication_and_tells_the_server_nothing() {
            testing::run(1, |shard| {
                let expected = tls::public(&PrivateKey([9; 32]));
                let mut pair = dial(shard, expected);
                pair.run(Duration::from_secs(1));
                let error = Error::Authentication { expected };
                assert_eq!(events(&pair.client), [&closed(&pair.client, error)]);
                assert!(pair.server.events.is_empty(), "{:?}", pair.server.events);
            });
        }

        #[test]
        #[should_panic(
            expected = "a dial fails only on its address: invalid remote address: \
                        127.0.0.1:0"
        )]
        fn to_port_zero_panics() {
            testing::run(1, |shard| {
                let config = shard.config(pair::CLIENT_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, pair::CLIENT_SHARD, NonZeroUsize::MIN);
                let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
                endpoint.connect(Monotonic(0), server(), remote);
            });
        }
    }

    mod close {
        use super::*;

        #[test]
        fn gives_the_code_to_both_sides() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, server());
                let (now, key, code) = (pair.now(), pair.client.key, Code(7));
                pair.client.endpoint.close(now, key.expect("a key"), code);
                pair.run(Duration::from_millis(100));
                let client = closed(&pair.client, Error::Closed { code });
                assert_eq!(events(&pair.client)[2..], [&client]);
                let server = closed(&pair.server, Error::PeerClosed { code });
                assert_eq!(events(&pair.server)[2..], [&server]);
            });
        }

        #[test]
        fn a_dial_before_it_connects_gives_closed_and_the_server_nothing() {
            testing::run(1, |shard| {
                // At 15 ms the server has the dial and the client has no reply.
                for elapsed in [Duration::ZERO, Duration::from_millis(15)] {
                    let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                    pair.dial(server());
                    pair.run(elapsed);
                    let key = pair.client.key.expect("a key");
                    pair.client.endpoint.close(pair.now(), key, Code(7));
                    pair.run(Duration::from_secs(2));
                    let error = Error::Closed { code: Code(7) };
                    assert_eq!(events(&pair.client), [&closed(&pair.client, error)]);
                    assert!(pair.server.events.is_empty(), "{:?}", pair.server.events);
                }
            });
        }

        #[test]
        fn twice_gives_one_event() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, server());
                let (now, key) = (pair.now(), pair.client.key.expect("a key"));
                pair.client.endpoint.close(now, key, Code(7));
                pair.client.endpoint.close(now, key, Code(8));
                pair.run(Duration::from_secs(1));
                let error = Error::Closed { code: Code(7) };
                assert_eq!(events(&pair.client)[2..], [&closed(&pair.client, error)]);
            });
        }

        #[test]
        fn after_a_timeout_gives_no_event() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, server());
                pair.server.silent = true;
                pair.run(Duration::from_secs(3));
                let (now, key) = (pair.now(), pair.client.key.expect("a key"));
                pair.client.endpoint.close(now, key, Code(7));
                pair.run(Duration::from_secs(1));
                let error = Error::TimedOut;
                assert_eq!(events(&pair.client)[2..], [&closed(&pair.client, error)]);
            });
        }

        #[test]
        fn then_a_reset_gives_no_second_event() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, server());
                pair.restart(shard);
                let (now, key) = (pair.now(), pair.client.key.expect("a key"));
                pair.client.endpoint.close(now, key, Code(7));
                pair.run(Duration::from_secs(1));
                let error = Error::Closed { code: Code(7) };
                assert_eq!(events(&pair.client)[2..], [&closed(&pair.client, error)]);
            });
        }

        #[test]
        fn frees_the_handle_for_a_new_key_that_the_old_key_cannot_close() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, server());
                let old = pair.client.key.expect("a key");
                pair.client.endpoint.close(pair.now(), old, Code(7));
                pair.run(Duration::from_secs(3));
                pair.dial(server());
                let new = pair.client.key.expect("a key");
                assert_eq!((new.handle, new == old), (old.handle, false));
                pair.client.endpoint.close(pair.now(), old, Code(8));
                pair.run(Duration::from_millis(100));
                let peer = Peer::Node(server());
                let connected = Event::Connected { key: new, peer };
                let available = available(&pair.client);
                assert_eq!(events(&pair.client)[3..], [&connected, &available]);
            });
        }

        #[test]
        fn with_a_code_over_u32_breaks_the_peer() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, server());
                let now = pair.client.endpoint.instant(pair.now());
                let code = VarInt::from_u64(1 << 32).expect("fits");
                pair.client.connection().close(now, code, Bytes::new());
                pair.run(Duration::from_millis(100));
                let reason = "closed by peer: 4294967296".into();
                let server = closed(&pair.server, Error::Broken { reason });
                assert_eq!(events(&pair.server)[2..], [&server]);
            });
        }
    }

    mod deadline {
        use super::*;

        #[test]
        fn stays_at_the_earliest_timer_when_a_later_dial_starts() {
            testing::run(1, |shard| {
                let config = shard.config(pair::CLIENT_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, pair::CLIENT_SHARD, NonZeroUsize::MIN);
                let mut buffer = Vec::new();
                endpoint.connect(Monotonic(0), server(), pair::SERVER);
                while endpoint.transmit(Monotonic(0), &mut buffer).is_some() {}
                let earliest = endpoint.deadline().expect("a deadline");
                let later = pair::at(Duration::from_millis(500));
                endpoint.connect(later, server(), pair::SERVER);
                while endpoint.transmit(later, &mut buffer).is_some() {}
                assert_eq!(endpoint.deadline(), Some(earliest));
            });
        }
    }

    mod transmit {
        use super::*;

        /// The destination ID of a datagram with a short header.
        fn destination(datagram: &[u8]) -> &[u8] {
            &datagram[1..=cid::LEN]
        }

        #[test]
        fn writes_a_response_into_the_callers_buffer() {
            testing::run(1, |shard| {
                let config = shard.config(pair::SERVER_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, pair::SERVER_SHARD, NonZeroUsize::MIN);
                let initial = pair::draft_29();
                let meta = pair::meta(pair::CLIENT, &initial);
                endpoint.receive(Monotonic(0), &meta, &initial);
                let mut buffer = Vec::with_capacity(1 << 16);
                let start = buffer.as_ptr();
                let transmit = endpoint.transmit(Monotonic(0), &mut buffer);
                let contents = transmit.expect("a version negotiation").contents;
                assert_eq!(contents.as_ptr(), start);
            });
        }

        #[test]
        fn keeps_at_most_responses_max_responses() {
            testing::run(1, |shard| {
                let config = shard.config(pair::SERVER_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, pair::SERVER_SHARD, NonZeroUsize::MIN);
                let initial = pair::draft_29();
                let meta = pair::meta(pair::CLIENT, &initial);
                for _ in 0..=RESPONSES_MAX {
                    endpoint.receive(Monotonic(0), &meta, &initial);
                }
                let mut buffer = Vec::new();
                let mut responses = 0;
                while endpoint.transmit(Monotonic(0), &mut buffer).is_some() {
                    responses += 1;
                }
                assert_eq!(responses, RESPONSES_MAX);
            });
        }

        #[test]
        fn sends_from_the_address_the_peer_sent_to() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                pair.dial(server());
                let to = Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 9)));
                deliver(&mut pair, to, None);
                let (now, mut buffer) = (pair.now(), Vec::new());
                let transmit = pair.server.endpoint.transmit(now, &mut buffer);
                assert_eq!(transmit.expect("a reply").source, to);
            });
        }

        #[test]
        fn marks_datagrams_ect0_once_the_peer_reports_ecn() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, server());
                write(&mut pair.client);
                let (now, mut buffer) = (pair.now(), Vec::new());
                let transmit = pair.client.endpoint.transmit(now, &mut buffer);
                assert_eq!(transmit.expect("a datagram").ecn, Some(Ecn::Ect0));
            });
        }

        #[test]
        fn gives_each_connection_a_turn() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                pair.dial(server());
                let first = pair.client.key.expect("a key");
                pair.dial(server());
                let second = pair.client.key.expect("a key");
                pair.run(Duration::from_millis(100));
                for key in [first, second] {
                    let connection = pair::connection(&mut pair.client.endpoint, key);
                    let stream = connection.streams().open(Dir::Uni).expect("a stream");
                    let mut send = connection.send_stream(stream);
                    send.write(&[0; 10_000]).expect("written");
                }
                let (now, mut buffer) = (pair.now(), Vec::new());
                let ids: Vec<Vec<u8>> = (0..4)
                    .map(|_| {
                        let transmit = pair.client.endpoint.transmit(now, &mut buffer);
                        destination(transmit.expect("a datagram").contents).to_vec()
                    })
                    .collect();
                assert_ne!(ids[0], ids[1]);
                assert_eq!([&ids[0], &ids[1]], [&ids[2], &ids[3]]);
            });
        }

        #[test]
        fn keeps_a_transmit_within_one_send_at_the_largest_batch() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, Duration::from_millis(1));
                let config = shard.config(pair::CLIENT_KEY, Span::SECOND);
                let batch = NonZeroUsize::new(64).expect("not zero");
                pair.client.endpoint =
                    Endpoint::new(&config, pair::CLIENT_SHARD, batch);
                pair.dial(server());
                pair.run(Duration::from_millis(100));
                let sent: Vec<u8> = (0..=u8::MAX).cycle().take(1 << 16).collect();
                let (now, key) = (pair.now(), pair.client.key.expect("a key"));
                let client = &mut pair.client.endpoint;
                for _ in 0..testing::STREAMS_MAX - 1 {
                    let opened = client.open_sender(now, key, Class::Command);
                    let sender = opened.expect("a stream");
                    let written =
                        client.write(now, &sender, &mut Some(shard.block(&sent)));
                    assert_eq!(written, Ok(Poll::Ready(())));
                }
                pair.run(Duration::from_secs(1));
                assert_eq!(pair.client.batch_max, settings::BATCH_MAX.get());
            });
        }
    }

    mod receive {
        use std::iter;
        use std::sync::Arc;

        use noq_proto::crypto::rustls::QuicClientConfig;
        use noq_proto::{ConnectionId, PathId};
        use rustls::crypto::CryptoProvider;
        use rustls::crypto::aws_lc_rs::{default_provider, kx_group};

        use super::*;

        /// A dial whose whole TLS client hello fits in its first Initial, and which
        /// offers only a protocol that no node speaks.
        fn other_protocol() -> noq_proto::ClientConfig {
            let kx_groups = vec![kx_group::X25519];
            let provider = CryptoProvider {
                kx_groups,
                ..default_provider()
            };
            #[expect(
                clippy::disallowed_methods,
                reason = "the server refuses the dial before the client checks a time"
            )]
            let mut tls =
                rustls::ClientConfig::builder_with_provider(Arc::new(provider))
                    .with_protocol_versions(&[&rustls::version::TLS13])
                    .expect("TLS 1.3")
                    .with_root_certificates(rustls::RootCertStore::empty())
                    .with_no_client_auth();
            tls.alpn_protocols = vec![b"foundation/2".to_vec()];
            let crypto = QuicClientConfig::try_from(tls).expect("AES-128-GCM");
            #[expect(clippy::disallowed_methods, reason = "it sets the destination ID")]
            let mut config = noq_proto::ClientConfig::new(Arc::new(crypto));
            config.initial_dst_cid_provider(Arc::new(|| {
                ConnectionId::new(&[1; cid::LEN])
            }));
            config
        }

        #[test]
        fn answers_a_refused_first_initial_with_a_close() {
            testing::run(1, |shard| {
                let config = shard.config(pair::SERVER_KEY, Span::SECOND);
                let mut server =
                    Endpoint::new(&config, pair::SERVER_SHARD, NonZeroUsize::MIN);
                let config = shard.config(pair::CLIENT_KEY, Span::SECOND);
                let (_, mut client) = Settings::new(&config, pair::CLIENT_SHARD);
                let now = server.instant(Monotonic(0));
                let dial =
                    client.connect(now, other_protocol(), pair::SERVER, SERVER_NAME);
                let (_, mut connection) = dial.expect("a dial");
                let mut buffer = Vec::new();
                let initial =
                    connection.poll_transmit(now, NonZeroUsize::MIN, &mut buffer);
                let len = initial.expect("an Initial").size;
                let meta = Meta {
                    source: pair::CLIENT,
                    destination: None,
                    ecn: None,
                    len,
                    stride: len,
                };
                server.receive(Monotonic(0), &meta, &buffer);
                let close =
                    server.transmit(Monotonic(0), &mut buffer).expect("a close");
                assert_eq!(close.destination, pair::CLIENT);
                let datagram = BytesMut::from(close.contents);
                let path = FourTuple::new(pair::SERVER, None);
                let event = client.handle(now, path, None, datagram, &mut Vec::new());
                let Some(DatagramEvent::ConnectionEvent(_, event)) = event else {
                    panic!("no event for the dial");
                };
                connection.handle_event(event);
                let reason =
                    iter::from_fn(|| connection.poll()).find_map(|event| match event {
                        noq_proto::Event::ConnectionLost { reason } => Some(reason),
                        _ => None,
                    });
                let reason = reason.expect("lost").to_string();
                let refusal = "aborted by peer: the cryptographic handshake failed: \
                               error 120: peer doesn't support any known protocol";
                assert_eq!(reason, refusal);
                assert_eq!(server.poll(), None);
            });
        }

        #[test]
        fn reports_a_ce_mark_to_the_sender() {
            testing::run(1, |shard| {
                let mut pair = dial(shard, server());
                write(&mut pair.client);
                deliver(&mut pair, None, Some(Ecn::Ce));
                pair.run(Duration::from_millis(100));
                let client = pair.client.connection();
                let stats = client.path_stats(PathId::ZERO).expect("a path");
                assert_eq!(stats.congestion_events, 1);
            });
        }

        #[test]
        #[should_panic(expected = "invariant: a batch of 10 bytes has a stride")]
        fn with_no_stride_panics() {
            testing::run(1, |shard| {
                let config = shard.config(pair::SERVER_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, pair::SERVER_SHARD, NonZeroUsize::MIN);
                let initial = pair::draft_29();
                let meta = Meta {
                    len: 10,
                    stride: 0,
                    ..pair::meta(pair::CLIENT, &initial)
                };
                endpoint.receive(Monotonic(0), &meta, &initial);
            });
        }

        #[test]
        fn splits_a_batch_into_its_datagrams() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                let config = shard.config(pair::CLIENT_KEY, Span::SECOND);
                let batch = NonZeroUsize::new(10).expect("not zero");
                pair.client.endpoint =
                    Endpoint::new(&config, pair::CLIENT_SHARD, batch);
                pair.dial(server());
                pair.run(Duration::from_millis(100));
                let sent: Vec<u8> = (0..=u8::MAX).cycle().take(20_000).collect();
                let (now, key) = (pair.now(), pair.client.key.expect("a key"));
                let client = &mut pair.client.endpoint;
                let opened = client.open_sender(now, key, Class::Command);
                let sender = opened.expect("a stream");
                let written = client.write(now, &sender, &mut Some(shard.block(&sent)));
                assert_eq!(written, Ok(Poll::Ready(())));
                pair.run(Duration::from_millis(100));
                assert!(pair.client.batch_max > 1, "{}", pair.client.batch_max);
                let (now, key) = (pair.now(), pair.server.key.expect("a key"));
                let server = &mut pair.server.endpoint;
                let mut incoming = server.accept(key).expect("a stream");
                let read = server.read(now, &mut incoming.receiver, testing::alloc);
                let read = read.expect("read");
                let Poll::Ready(Some(message)) = read else {
                    panic!("no message");
                };
                assert_eq!(*message, *sent);
            });
        }
    }
}
