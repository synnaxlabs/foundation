//! The QUIC carrier: noq-proto with time, datagrams, and randomness as inputs.

mod cid;
pub(crate) mod connection;
mod settings;
#[cfg(test)]
mod testing;

use std::collections::VecDeque;
use std::mem;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use env::net::Ecn;
use env::net::udp::{Meta, Transmit};
use noq_proto::{
    ConnectError, ConnectionHandle, DatagramEvent, EcnCodepoint, FourTuple,
};
use types::node::PublicKey;
use types::time::Monotonic;

use self::connection::Connection;
use self::settings::Settings;
use crate::{Code, Config, Error, Peer};

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
    /// Indexed by noq-proto's handle.
    connections: Vec<Option<Connection>>,
    /// The connections made so far.
    serial: u64,
    /// The connections that may have a datagram to send, each once, in the order
    /// [`Endpoint::transmit`] polls them.
    ready: VecDeque<connection::Key>,
    events: VecDeque<Event>,
    /// Datagrams that no connection sends, such as a version negotiation or a
    /// stateless reset.
    responses: VecDeque<(noq_proto::Transmit, Vec<u8>)>,
    /// The buffer that each received batch is copied into and split from. noq-proto
    /// decrypts in place and keeps parts of it.
    received: BytesMut,
    /// Where noq-proto writes a response.
    reply: Vec<u8>,
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
}

impl Endpoint {
    /// An endpoint for this node's key whose connection IDs all start with
    /// `shard`.
    ///
    /// # Panics
    ///
    /// When `config.idle` is not positive. `Transport::new` refuses it first.
    pub(crate) fn new(config: &Config, shard: u8) -> Self {
        let (settings, endpoint) = Settings::new(config, shard);
        Self {
            epoch: config.clock.epoch(),
            settings,
            inner: endpoint,
            connections: Vec::new(),
            serial: 0,
            ready: VecDeque::new(),
            events: VecDeque::new(),
            responses: VecDeque::new(),
            received: BytesMut::new(),
            reply: Vec::new(),
        }
    }

    /// Dials `remote` and expects it to prove `peer`. The dial ends in
    /// [`Event::Connected`] or [`Event::Closed`] for the key.
    ///
    /// # Errors
    ///
    /// [`Error::Broken`] when no datagram can go to `remote` (port 0, or an
    /// unspecified IP).
    pub(crate) fn connect(
        &mut self,
        now: Monotonic,
        peer: PublicKey,
        remote: SocketAddr,
    ) -> Result<connection::Key, Error> {
        let now = self.instant(now);
        let dial = self.settings.client(peer);
        let (handle, inner) = match self.inner.connect(now, dial, remote, SERVER_NAME) {
            Ok(connection) => connection,
            Err(error @ ConnectError::InvalidRemoteAddress(_)) => {
                return Err(Error::Broken {
                    reason: error.to_string(),
                });
            }
            Err(error) => {
                panic!("invariant: a dial fails only on its address: {error}")
            }
        };
        let key = self.insert(handle, |key| Connection::dialed(key, inner, peer));
        self.drive(handle);
        Ok(key)
    }

    /// Takes one received batch: `meta.len` bytes of `batch`, in datagrams of
    /// `meta.stride` bytes.
    pub(crate) fn receive(&mut self, now: Monotonic, meta: &Meta, batch: &[u8]) {
        let now = self.instant(now);
        let path = FourTuple::new(meta.source, meta.destination);
        let ecn = meta.ecn.map(codepoint);
        // The stride is 0 only for an empty batch.
        let stride = meta.stride.max(1);
        self.received.extend_from_slice(&batch[..meta.len]);
        let mut datagrams = self.received.split();
        while !datagrams.is_empty() {
            let datagram = datagrams.split_to(stride.min(datagrams.len()));
            self.handle(now, path, ecn, datagram);
        }
    }

    /// The next datagrams to send, at most `datagrams_max` of them to one
    /// destination, written into `buffer`. `None` when nothing is due. Call it until
    /// `None` after each other call but [`Endpoint::deadline`] and
    /// [`Endpoint::poll`].
    pub(crate) fn transmit<'a>(
        &mut self,
        now: Monotonic,
        datagrams_max: NonZeroUsize,
        buffer: &'a mut Vec<u8>,
    ) -> Option<Transmit<'a>> {
        if let Some((transmit, bytes)) = self.responses.pop_front() {
            *buffer = bytes;
            return Some(outgoing(&transmit, buffer));
        }
        let now = self.instant(now);
        while let Some(key) = self.ready.pop_front() {
            let Some(connection) = self.get(key) else {
                continue;
            };
            connection.queued = false;
            buffer.clear();
            if let Some(transmit) =
                connection.inner.poll_transmit(now, datagrams_max, buffer)
            {
                self.drive(key.handle);
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
                self.drive(handle);
            }
        }
    }

    /// Closes the connection of `key` with `code`, and queues its [`Event::Closed`]
    /// with [`Error::Closed`]. Does nothing when the connection already ended: its
    /// [`Event::Closed`] is queued or was given.
    pub(crate) fn close(&mut self, now: Monotonic, key: connection::Key, code: Code) {
        let now = self.instant(now);
        let Some(connection) = self.get(key) else {
            return;
        };
        let closed = connection.close(now, code);
        self.events.extend(closed);
        self.drive(key.handle);
    }

    /// The next event, in the order they happened.
    pub(crate) fn poll(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// The noq-proto connection of `key`, queued for [`Endpoint::transmit`].
    ///
    /// # Panics
    ///
    /// When the connection ended and drained.
    #[cfg(test)]
    fn connection(&mut self, key: connection::Key) -> &mut noq_proto::Connection {
        self.drive(key.handle);
        &mut self.get(key).expect("a connection").inner
    }

    fn instant(&self, now: Monotonic) -> Instant {
        self.epoch + Duration::from_nanos(now.0)
    }

    fn get(&mut self, key: connection::Key) -> Option<&mut Connection> {
        let connection = self.connections.get_mut(key.handle.0)?.as_mut()?;
        (connection.key == key).then_some(connection)
    }

    fn insert(
        &mut self,
        handle: ConnectionHandle,
        connection: impl FnOnce(connection::Key) -> Connection,
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
        *entry = Some(connection(key));
        key
    }

    fn handle(
        &mut self,
        now: Instant,
        path: FourTuple,
        ecn: Option<EcnCodepoint>,
        datagram: BytesMut,
    ) {
        self.reply.clear();
        let event = self.inner.handle(now, path, ecn, datagram, &mut self.reply);
        let response = match event {
            None => None,
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                let connection = self.connections[handle.0]
                    .as_mut()
                    .expect("invariant: noq-proto routes only to a live handle");
                connection.inner.handle_event(event);
                self.drive(handle);
                None
            }
            Some(DatagramEvent::NewConnection(incoming)) => {
                match self.inner.accept(incoming, now, &mut self.reply, None) {
                    Ok((handle, inner)) => {
                        self.insert(handle, |key| Connection::accepted(key, inner));
                        self.drive(handle);
                        None
                    }
                    Err(error) => error.response,
                }
            }
            Some(DatagramEvent::Response(response)) => Some(response),
        };
        if let Some(response) = response {
            self.responses
                .push_back((response, mem::take(&mut self.reply)));
        }
    }

    /// Drives `handle`'s connection. Frees it once it drained, and else queues it
    /// for [`Endpoint::transmit`]: each call that can give it a datagram to send ends
    /// here.
    fn drive(&mut self, handle: ConnectionHandle) {
        let entry = &mut self.connections[handle.0];
        let connection = entry.as_mut().expect("invariant: a live handle");
        if connection.drive(&mut self.inner, &mut self.events) {
            *entry = None;
        } else if !connection.queued {
            connection.queued = true;
            self.ready.push_back(connection.key);
        }
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
        fn to_port_zero_is_broken() {
            let dialed = testing::run(1, |shard| {
                let config = shard.config(testing::CLIENT_KEY, Span::SECOND);
                let mut endpoint = Endpoint::new(&config, testing::CLIENT_SHARD);
                let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
                endpoint.connect(Monotonic(0), server(), remote)
            });
            let reason = "invalid remote address: 127.0.0.1:0".into();
            assert_eq!(dialed, Err(Error::Broken { reason }));
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

    mod transmit {
        use super::*;

        /// The destination ID of a datagram with a short header.
        fn destination(datagram: &[u8]) -> &[u8] {
            &datagram[1..=cid::LEN]
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
                    let connection = pair.client.endpoint.connection(key);
                    let stream = connection.streams().open(Dir::Uni).expect("a stream");
                    let mut send = connection.send_stream(stream);
                    send.write(&[0; 10_000]).expect("written");
                }
                let (now, mut buffer) = (pair.now(), Vec::new());
                let ids: Vec<Vec<u8>> = (0..4)
                    .map(|_| {
                        let transmit = pair.client.endpoint.transmit(
                            now,
                            NonZeroUsize::MIN,
                            &mut buffer,
                        );
                        destination(transmit.expect("a datagram").contents).to_vec()
                    })
                    .collect();
                assert_ne!(ids[0], ids[1]);
                assert_eq!([&ids[0], &ids[1]], [&ids[2], &ids[3]]);
            });
        }
    }

    mod receive {
        use super::*;

        #[test]
        fn splits_a_batch_into_its_datagrams() {
            testing::run(1, |shard| {
                let mut pair = Pair::new(shard, Span::SECOND, DELAY);
                pair.client.datagrams_max = NonZeroUsize::new(10).expect("not zero");
                pair.dial(server());
                pair.run(Duration::from_millis(100));
                let sent: Vec<u8> = (0..=u8::MAX).cycle().take(20_000).collect();
                let client = pair.client.connection();
                let stream = client.streams().open(Dir::Uni).expect("a stream");
                let written = client.send_stream(stream).write(&sent).expect("written");
                assert_eq!(written, sent.len());
                client.send_stream(stream).finish().expect("finished");
                pair.run(Duration::from_millis(100));
                assert!(pair.client.batch_max > 1, "{}", pair.client.batch_max);
                let server = pair.server.connection();
                let stream = server.streams().accept(Dir::Uni).expect("a stream");
                let mut receive = server.recv_stream(stream);
                let mut chunks = receive.read(true).expect("readable");
                let mut read = Vec::new();
                while let Some(chunk) = chunks.next(usize::MAX).expect("read") {
                    read.extend_from_slice(&chunk.bytes);
                }
                assert_eq!(read, sent);
            });
        }
    }
}
