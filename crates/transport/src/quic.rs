//! The QUIC carrier: noq-proto with time, datagrams, and randomness as inputs.

mod cid;
pub(crate) mod connection;
mod settings;
pub(crate) mod stream;
#[cfg(test)]
mod testing;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::task::Poll;
use std::time::{Duration, Instant};

use block::{Block, Pool};
use bytes::BytesMut;
use env::net::Ecn;
use env::net::udp::{Meta, Transmit};
use noq_proto::{ConnectionHandle, DatagramEvent, Dir, EcnCodepoint, FourTuple};
use types::node::PublicKey;
use types::time::Monotonic;

use self::connection::Connection;
use self::settings::Settings;
use self::stream::{Incoming, Receiver, Sender, Streams};
use crate::{Class, Code, Config, Error, Peer};

/// The server name a dial sends. The verifiers check the node key, not the name.
const SERVER_NAME: &str = "foundation";

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
    /// stateless reset. At most one for each datagram of a batch, because the caller
    /// takes them all with [`Endpoint::transmit`] after each [`Endpoint::receive`].
    responses: VecDeque<(noq_proto::Transmit, Vec<u8>)>,
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
    /// [`Endpoint::open`] and [`Endpoint::open_sender`] may now give a stream.
    Available { key: connection::Key },
    /// `stream` may have more to read. It can repeat, and it can name a stream the
    /// caller no longer holds or has not accepted yet.
    Readable { stream: stream::Key },
    /// `stream` may take more, or the peer stopped it. It can repeat, and it can
    /// name a stream the caller no longer holds or has not accepted yet.
    Writable { stream: stream::Key },
}

impl Endpoint {
    /// An endpoint for this node's key whose connection IDs all start with
    /// `shard`. Each [`Transmit`] holds at most `datagrams_max` datagrams: the
    /// socket's batch max.
    ///
    /// # Panics
    ///
    /// When `config.idle` is not positive, or `config.window_bytes` is below
    /// `config.message_bytes_max`. `Transport::new` refuses both first.
    pub(crate) fn new(config: &Config, shard: u8, datagrams_max: NonZeroUsize) -> Self {
        assert!(
            config.window_bytes >= config.message_bytes_max.get(),
            "a window of {} bytes is below the largest message, {} bytes",
            config.window_bytes,
            config.message_bytes_max
        );
        let (settings, endpoint) = Settings::new(config, shard);
        Self {
            epoch: config.clock.epoch(),
            settings,
            inner: endpoint,
            datagrams_max,
            pool: Rc::clone(&config.pool),
            message_bytes_max: config.message_bytes_max.get(),
            window_bytes: config.window_bytes,
            connections: Vec::new(),
            serial: 0,
            ready: VecDeque::new(),
            events: VecDeque::new(),
            responses: VecDeque::new(),
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
    /// [`Endpoint::deadline`] and [`Endpoint::poll`].
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

    /// Opens a stream of `class` that goes both ways. `None` before
    /// [`Event::Connected`], when the peer allows no more streams now
    /// ([`Event::Available`] follows), or when the connection ended. The peer sees
    /// the stream at its first message.
    pub(crate) fn open(
        &mut self,
        now: Monotonic,
        key: connection::Key,
        class: Class,
    ) -> Option<(Sender, Receiver)> {
        let stream = self.start(now, key, Dir::Bi)?;
        let receiver = Receiver::new(stream, self.message_bytes_max);
        Some((Sender::new(stream, class), receiver))
    }

    /// As [`Endpoint::open`], for a stream that only this side sends on.
    pub(crate) fn open_sender(
        &mut self,
        now: Monotonic,
        key: connection::Key,
        class: Class,
    ) -> Option<Sender> {
        let stream = self.start(now, key, Dir::Uni)?;
        Some(Sender::new(stream, class))
    }

    /// The next stream the peer opened on `key`'s connection, highest class first.
    /// `None` when there is none now ([`Event::Incoming`] follows), or when the
    /// connection ended.
    pub(crate) fn accept(&mut self, key: connection::Key) -> Option<Incoming> {
        let connection = find(&mut self.connections, key).filter(|c| c.live())?;
        connection.streams.accept(key, self.message_bytes_max)
    }

    /// Puts `message` on the stream after the messages before it. `Ready` when the
    /// stream took all of it. Else `sender` holds the rest: call
    /// [`Endpoint::flush`] after [`Event::Writable`]. `Pending` also when the
    /// connection ended.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream. Each later write gives it
    /// too.
    ///
    /// # Panics
    ///
    /// When `sender` holds part of a message, after [`Endpoint::finish`], or when
    /// `message` is over the largest message (this side's own until the hello).
    pub(crate) fn write(
        &mut self,
        now: Monotonic,
        sender: &mut Sender,
        message: Block,
    ) -> Result<Poll<()>, Error> {
        assert!(
            message.len() <= self.message_bytes_max,
            "a message of {} bytes is over the largest message, {} bytes",
            message.len(),
            self.message_bytes_max
        );
        sender.load(message);
        self.flush(now, sender)
    }

    /// Writes the rest of the message that `sender` holds. `Ready` when it holds
    /// none. `Pending` when the stream takes no more now ([`Event::Writable`]
    /// follows), or when the connection ended.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream.
    pub(crate) fn flush(
        &mut self,
        now: Monotonic,
        sender: &mut Sender,
    ) -> Result<Poll<()>, Error> {
        let key = sender.key().connection;
        self.streams(now, key, Poll::Pending, |streams, inner, _, events| {
            streams.flush(inner, sender, events)
        })
    }

    /// Ends the stream after the messages written to it. They arrive after the
    /// caller drops `sender`. A stream this side opened that ends before its first
    /// message never reaches the peer, and the [`Receiver`] of a two-way one gets
    /// [`Error::Reset`] with code 0. Does nothing when the connection ended.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the peer stopped the stream.
    ///
    /// # Panics
    ///
    /// When `sender` holds part of a message, or after [`Endpoint::finish`].
    pub(crate) fn finish(
        &mut self,
        now: Monotonic,
        sender: &mut Sender,
    ) -> Result<(), Error> {
        sender.end();
        let stream = sender.key();
        self.streams(now, stream.connection, (), |streams, inner, _, _| {
            streams.finish(inner, stream.id)
        })
    }

    /// The next whole message of `receiver`'s stream, in one block from the pool.
    /// `Ready(None)` after the last one, and on each call after that. `Pending` when
    /// no whole message is here yet ([`Event::Readable`] follows), or when the
    /// connection ended.
    ///
    /// # Errors
    ///
    /// - [`Error::Reset`] when the peer reset the stream. Each later read gives it
    ///   too.
    /// - [`Error::Pool`] when the pool has no room for the message now. Call again
    ///   when it has.
    pub(crate) fn read(
        &mut self,
        now: Monotonic,
        receiver: &mut Receiver,
    ) -> Result<Poll<Option<Block>>, Error> {
        if let Some(ended) = receiver.ended() {
            return ended;
        }
        let key = receiver.key().connection;
        self.streams(now, key, Poll::Pending, |streams, inner, pool, events| {
            streams.read(inner, receiver, pool, events)
        })
    }

    /// Resets `sender`'s stream with `code`. The peer's next read gives
    /// [`Error::Reset`], and the messages it has not read drop, unless it acknowledged
    /// all of the stream. The message in hand drops, and its send budget comes back.
    /// Does nothing when the connection ended.
    pub(crate) fn reset(&mut self, now: Monotonic, sender: Sender, code: Code) {
        let key = sender.key().connection;
        let Some(connection) = find(&mut self.connections, key).filter(|c| c.live())
        else {
            return;
        };
        let Connection { inner, streams, .. } = connection;
        streams.reset(inner, sender, code, &mut self.events);
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

    /// The next event, in the order they happened.
    pub(crate) fn poll(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    fn instant(&self, now: Monotonic) -> Instant {
        self.epoch + Duration::from_nanos(now.0)
    }

    /// Opens a stream in `dir` on the connection of `key`, unless it ended.
    fn start(
        &mut self,
        now: Monotonic,
        key: connection::Key,
        dir: Dir,
    ) -> Option<stream::Key> {
        let connection = find(&mut self.connections, key).filter(|c| c.live())?;
        let id = connection.streams.open(&mut connection.inner, dir);
        self.drive(key.handle, self.instant(now));
        Some(stream::Key {
            connection: key,
            id: id?,
        })
    }

    /// Runs `call` on the streams of `key`'s connection with the pool and the event
    /// queue, and drives the connection. A fault of the peer's that `call` finds
    /// closes the connection: the caller gets it from [`Event::Closed`], and this
    /// gives `ended`, as it does when the connection ended before.
    fn streams<T>(
        &mut self,
        now: Monotonic,
        key: connection::Key,
        ended: T,
        call: impl FnOnce(
            &mut Streams,
            &mut noq_proto::Connection,
            &Pool,
            &mut VecDeque<Event>,
        ) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let now = self.instant(now);
        let Some(connection) = find(&mut self.connections, key).filter(|c| c.live())
        else {
            return Ok(ended);
        };
        let Connection { inner, streams, .. } = connection;
        let result = match call(streams, inner, &self.pool, &mut self.events) {
            Err(Error::Broken { reason }) => {
                self.events.push_back(connection.fault(now, reason));
                Ok(ended)
            }
            result => result,
        };
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
            Some(DatagramEvent::Response(response)) => Some(response),
        };
        if let Some(response) = response {
            self.responses.push_back((response, reply));
        }
    }

    /// Drives `handle`'s connection at `now`. Frees it once it drained, and else
    /// queues it for [`Endpoint::transmit`]: each call that can give it a datagram to
    /// send ends here.
    fn drive(&mut self, handle: ConnectionHandle, now: Instant) {
        let entry = &mut self.connections[handle.0];
        let connection = entry.as_mut().expect("invariant: a live handle");
        if connection.drive(now, &mut self.inner, &mut self.events) {
            *entry = None;
        } else {
            queue(&mut self.ready, connection);
        }
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
    use crate::quic::testing::{self, Pair, Side};
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
        tls::public(&testing::SERVER_KEY)
    }

    fn events(side: &Side) -> Vec<&Event> {
        side.events.iter().map(|(_, event)| event).collect()
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
            let len = transmit.contents.len();
            let meta = Meta {
                source: testing::CLIENT,
                destination,
                ecn,
                len,
                stride: len,
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
                let client = connected(&pair.client, &testing::SERVER_KEY);
                assert_eq!(events(&pair.client), [&client]);
                let server = connected(&pair.server, &testing::CLIENT_KEY);
                assert_eq!(events(&pair.server), [&server]);
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
                let config = shard.config(testing::CLIENT_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, testing::CLIENT_SHARD, NonZeroUsize::MIN);
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
                assert_eq!(events(&pair.client)[1..], [&client]);
                let server = closed(&pair.server, Error::PeerClosed { code });
                assert_eq!(events(&pair.server)[1..], [&server]);
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
                assert_eq!(events(&pair.client)[1..], [&closed(&pair.client, error)]);
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
                assert_eq!(events(&pair.client)[1..], [&closed(&pair.client, error)]);
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
                assert_eq!(events(&pair.client)[1..], [&closed(&pair.client, error)]);
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
                assert_eq!(events(&pair.client)[2..], [&connected]);
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
                assert_eq!(events(&pair.server)[1..], [&server]);
            });
        }
    }

    mod deadline {
        use super::*;

        #[test]
        fn stays_at_the_earliest_timer_when_a_later_dial_starts() {
            testing::run(1, |shard| {
                let config = shard.config(testing::CLIENT_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, testing::CLIENT_SHARD, NonZeroUsize::MIN);
                let mut buffer = Vec::new();
                endpoint.connect(Monotonic(0), server(), testing::SERVER);
                while endpoint.transmit(Monotonic(0), &mut buffer).is_some() {}
                let earliest = endpoint.deadline().expect("a deadline");
                let later = testing::at(Duration::from_millis(500));
                endpoint.connect(later, server(), testing::SERVER);
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
                let config = shard.config(testing::SERVER_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, testing::SERVER_SHARD, NonZeroUsize::MIN);
                let (meta, initial) = testing::draft_29();
                endpoint.receive(Monotonic(0), &meta, &initial);
                let mut buffer = Vec::with_capacity(1 << 16);
                let start = buffer.as_ptr();
                let transmit = endpoint.transmit(Monotonic(0), &mut buffer);
                let contents = transmit.expect("a version negotiation").contents;
                assert_eq!(contents.as_ptr(), start);
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
                    let connection =
                        testing::connection(&mut pair.client.endpoint, key);
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
                let config = shard.config(testing::SERVER_KEY, Span::SECOND);
                let mut server =
                    Endpoint::new(&config, testing::SERVER_SHARD, NonZeroUsize::MIN);
                let config = shard.config(testing::CLIENT_KEY, Span::SECOND);
                let (_, mut client) = Settings::new(&config, testing::CLIENT_SHARD);
                let now = server.instant(Monotonic(0));
                let dial =
                    client.connect(now, other_protocol(), testing::SERVER, SERVER_NAME);
                let (_, mut connection) = dial.expect("a dial");
                let mut buffer = Vec::new();
                let initial =
                    connection.poll_transmit(now, NonZeroUsize::MIN, &mut buffer);
                let len = initial.expect("an Initial").size;
                let meta = Meta {
                    source: testing::CLIENT,
                    destination: None,
                    ecn: None,
                    len,
                    stride: len,
                };
                server.receive(Monotonic(0), &meta, &buffer);
                let close =
                    server.transmit(Monotonic(0), &mut buffer).expect("a close");
                assert_eq!(close.destination, testing::CLIENT);
                let datagram = BytesMut::from(close.contents);
                let path = FourTuple::new(testing::SERVER, None);
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
                let config = shard.config(testing::SERVER_KEY, Span::SECOND);
                let mut endpoint =
                    Endpoint::new(&config, testing::SERVER_SHARD, NonZeroUsize::MIN);
                let (mut meta, initial) = testing::draft_29();
                (meta.len, meta.stride) = (10, 0);
                endpoint.receive(Monotonic(0), &meta, &initial);
            });
        }

        #[test]
        fn splits_a_batch_into_its_datagrams() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                let config = shard.config(testing::CLIENT_KEY, Span::SECOND);
                let batch = NonZeroUsize::new(10).expect("not zero");
                pair.client.endpoint =
                    Endpoint::new(&config, testing::CLIENT_SHARD, batch);
                pair.dial(server());
                pair.run(Duration::from_millis(100));
                let sent: Vec<u8> = (0..=u8::MAX).cycle().take(20_000).collect();
                let (now, key) = (pair.now(), pair.client.key.expect("a key"));
                let client = &mut pair.client.endpoint;
                let opened = client.open_sender(now, key, Class::Command);
                let mut sender = opened.expect("a stream");
                let written = client.write(now, &mut sender, shard.block(&sent));
                assert_eq!(written, Ok(Poll::Ready(())));
                pair.run(Duration::from_millis(100));
                assert!(pair.client.batch_max > 1, "{}", pair.client.batch_max);
                let (now, key) = (pair.now(), pair.server.key.expect("a key"));
                let server = &mut pair.server.endpoint;
                let mut incoming = server.accept(key).expect("a stream");
                let read = server.read(now, &mut incoming.receiver).expect("read");
                let Poll::Ready(Some(message)) = read else {
                    panic!("no message");
                };
                assert_eq!(*message, *sent);
            });
        }
    }
}
