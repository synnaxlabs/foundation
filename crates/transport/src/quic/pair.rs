//! Two endpoints over a link in virtual time.

use std::collections::VecDeque;
use std::mem;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use env::net::udp::Meta;
use noq_proto::{
    ClientConfig, ConnectionHandle, DatagramEvent, FourTuple, SendDatagramError,
    TransportConfig,
};
use types::node::{PrivateKey, PublicKey};
use types::time::{Monotonic, Span};

use super::settings::{MTU_MIN, Settings};
use super::{Endpoint, Event, SERVER_NAME, cid, connection, find, queue};
use crate::Config;
use crate::testing::Shard;

/// The client's address. The server's is [`SERVER`].
pub(super) const CLIENT: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
/// The server's address.
pub(super) const SERVER: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2);
/// The address of a [`Foreign`] peer.
pub(super) const FOREIGN: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4);
/// The client's shard. The server's is [`SERVER_SHARD`].
pub(super) const CLIENT_SHARD: u8 = 3;
/// The server's shard.
pub(super) const SERVER_SHARD: u8 = 5;
/// The shard of a [`Foreign`] peer.
const FOREIGN_SHARD: u8 = 7;
/// The client's node key. The server's is [`SERVER_KEY`].
pub(super) const CLIENT_KEY: PrivateKey = PrivateKey([1; 32]);
/// The server's node key.
pub(super) const SERVER_KEY: PrivateKey = PrivateKey([2; 32]);
/// The node key of a [`Foreign`] peer.
pub(super) const FOREIGN_KEY: PrivateKey = PrivateKey([4; 32]);

/// An Initial datagram in QUIC draft 29, which no endpoint here speaks.
pub(super) fn draft_29() -> Vec<u8> {
    let len = u8::try_from(cid::LEN).expect("fits");
    let id = [[len].as_slice(), &[1; cid::LEN]].concat();
    let mut initial = [[0xc0].as_slice(), &[0xff, 0, 0, 0x1d], &id, &id].concat();
    initial.resize(usize::from(MTU_MIN), 0);
    initial
}

/// The meta of `datagram` alone, from `source`.
pub(super) fn meta(source: SocketAddr, datagram: &[u8]) -> Meta {
    let len = datagram.len();
    Meta {
        source,
        destination: None,
        ecn: None,
        len,
        stride: len,
    }
}

/// The noq-proto connection of `key`, queued for [`Endpoint::transmit`].
///
/// # Panics
///
/// When the connection ended and drained.
pub(super) fn connection(
    endpoint: &mut Endpoint,
    key: connection::Key,
) -> &mut noq_proto::Connection {
    let connection = find(&mut endpoint.connections, key).expect("a connection");
    queue(&mut endpoint.ready, connection);
    &mut connection.inner
}

/// The time `elapsed` after the start of a run.
pub(super) fn at(elapsed: Duration) -> Monotonic {
    Monotonic(u64::try_from(elapsed.as_nanos()).expect("fits"))
}

/// An endpoint on [`CLIENT_SHARD`] and one on [`SERVER_SHARD`], over a link that
/// delivers each batch `delay` after it leaves, unless it drops it. Time starts at
/// `Monotonic(0)` and moves only in [`Pair::run`].
pub(super) struct Pair {
    now: Duration,
    delay: Duration,
    idle: Span,
    /// The side that dials.
    pub(super) client: Side,
    /// The side that accepts.
    pub(super) server: Side,
    /// A peer at [`FOREIGN`], if any.
    pub(super) foreign: Option<Foreign>,
    /// Arrival, destination, and bytes of each batch on the link, in the order
    /// they arrive.
    link: VecDeque<(Duration, SocketAddr, Meta, Vec<u8>)>,
}

/// One side of a [`Pair`].
pub(super) struct Side {
    pub(super) endpoint: Endpoint,
    /// The connection this side dialed, or the first it accepted.
    pub(super) key: Option<connection::Key>,
    /// Where this side sends from and takes datagrams.
    pub(super) address: SocketAddr,
    /// When (from the start), where to, and the bytes of each datagram this side
    /// sent, the dropped ones too.
    pub(super) sent: Vec<(Duration, SocketAddr, Vec<u8>)>,
    /// The most datagrams in one batch this side sent.
    pub(super) batch_max: usize,
    /// When (from the start) and what of each event of this side's endpoint.
    pub(super) events: Vec<(Duration, Event)>,
    /// The link drops this many of the next batches this side sends.
    pub(super) drops: usize,
    /// This side stops: it runs no timer, sends nothing, and gets nothing.
    pub(super) silent: bool,
}

impl Pair {
    /// Two endpoints with `idle`, and no connection.
    pub(super) fn new(shard: &Shard, idle: Span, delay: Duration) -> Self {
        Self::with(shard, idle, delay, |_| {})
    }

    /// As [`Pair::new`], with `change` made to the config of each endpoint.
    pub(super) fn with(
        shard: &Shard,
        idle: Span,
        delay: Duration,
        change: impl Fn(&mut Config),
    ) -> Self {
        let mut config = shard.config(CLIENT_KEY, idle);
        change(&mut config);
        let client = Endpoint::new(&config, CLIENT_SHARD, NonZeroUsize::MIN);
        let mut config = shard.config(SERVER_KEY, idle);
        change(&mut config);
        let server = Endpoint::new(&config, SERVER_SHARD, NonZeroUsize::MIN);
        Self {
            now: Duration::ZERO,
            delay,
            idle,
            client: Side::new(client, CLIENT),
            server: Side::new(server, SERVER),
            foreign: None,
            link: VecDeque::new(),
        }
    }

    /// Now.
    pub(super) fn now(&self) -> Monotonic {
        at(self.now)
    }

    /// Starts a dial from the client to the server, which must prove `peer`.
    /// Nothing is sent yet.
    pub(super) fn dial(&mut self, peer: PublicKey) {
        let key = self.client.endpoint.connect(self.now(), peer, SERVER);
        self.client.key = Some(key);
    }

    /// Moves datagrams and runs timers for `span`.
    pub(super) fn run(&mut self, span: Duration) {
        let end = self.now + span;
        loop {
            self.flush();
            let foreign = self.foreign.as_ref().and_then(Foreign::deadline);
            let next = [self.client.deadline(), self.server.deadline(), foreign]
                .into_iter()
                .chain([self.link.front().map(|&(arrival, ..)| arrival)])
                .flatten()
                .min();
            match next {
                Some(next) if next <= end => self.now = self.now.max(next),
                _ => break,
            }
            while let Some((arrival, ..)) = self.link.front()
                && *arrival <= self.now
            {
                let (_, to, meta, bytes) = self.link.pop_front().expect("a batch");
                self.deliver(to, &meta, &bytes);
            }
            for side in [&mut self.client, &mut self.server] {
                if !side.silent {
                    side.endpoint.timeout(at(self.now));
                }
            }
            if let Some(foreign) = &mut self.foreign {
                foreign.timeout(self.now);
            }
        }
        self.now = end;
    }

    /// Replaces the server's endpoint with a new one for the same node key and
    /// shard, as a restart does. The old connection is gone.
    pub(super) fn restart(&mut self, shard: &Shard) {
        let config = shard.config(SERVER_KEY, self.idle);
        self.server.endpoint = Endpoint::new(&config, SERVER_SHARD, NonZeroUsize::MIN);
        self.server.key = None;
    }

    fn flush(&mut self) {
        let (now, arrival) = (self.now, self.now + self.delay);
        for side in [&mut self.client, &mut self.server] {
            for (to, meta, bytes) in side.flush(now) {
                self.link.push_back((arrival, to, meta, bytes));
            }
        }
        if let Some(foreign) = &mut self.foreign {
            for (to, meta, bytes) in foreign.flush(now) {
                self.link.push_back((arrival, to, meta, bytes));
            }
        }
    }

    fn deliver(&mut self, to: SocketAddr, meta: &Meta, bytes: &[u8]) {
        let now = at(self.now);
        let side = [&mut self.client, &mut self.server]
            .into_iter()
            .find(|side| side.address == to && !side.silent);
        if let Some(side) = side {
            side.endpoint.receive(now, meta, bytes);
        } else if let Some(foreign) = self.foreign.as_mut().filter(|_| to == FOREIGN) {
            foreign.receive(self.now, meta, bytes);
        }
    }
}

impl Side {
    fn new(endpoint: Endpoint, address: SocketAddr) -> Self {
        Self {
            endpoint,
            key: None,
            address,
            sent: Vec::new(),
            batch_max: 0,
            events: Vec::new(),
            drops: 0,
            silent: false,
        }
    }

    /// The noq-proto connection of [`Side::key`].
    ///
    /// # Panics
    ///
    /// When this side has none.
    pub(super) fn connection(&mut self) -> &mut noq_proto::Connection {
        let key = self.key.expect("a connection");
        connection(&mut self.endpoint, key)
    }

    fn deadline(&self) -> Option<Duration> {
        let deadline = self.endpoint.deadline().filter(|_| !self.silent)?;
        Some(Duration::from_nanos(deadline.0))
    }

    /// Takes every event and batch the endpoint has at `now`, and gives the
    /// batches the link carries: destination, meta, and bytes.
    fn flush(&mut self, now: Duration) -> Vec<(SocketAddr, Meta, Vec<u8>)> {
        let mut out = Vec::new();
        if self.silent {
            return out;
        }
        let mut buffer = Vec::new();
        while let Some(transmit) = self.endpoint.transmit(at(now), &mut buffer) {
            let len = transmit.contents.len();
            let stride = transmit.segment.map_or(len, NonZeroUsize::get);
            for datagram in transmit.contents.chunks(stride) {
                self.sent
                    .push((now, transmit.destination, datagram.to_vec()));
            }
            self.batch_max = self.batch_max.max(len.div_ceil(stride));
            if self.drops > 0 {
                self.drops -= 1;
                continue;
            }
            let meta = Meta {
                source: self.address,
                destination: None,
                ecn: transmit.ecn,
                len,
                stride,
            };
            out.push((transmit.destination, meta, transmit.contents.to_vec()));
        }
        while let Some(event) = self.endpoint.poll() {
            if let Event::Connected { key, .. } = event {
                self.key.get_or_insert(key);
            }
            self.events.push((now, event));
        }
        out
    }
}

/// A peer that runs noq-proto with no [`Endpoint`] above it, so a test writes its
/// streams, its hello too. It speaks the TLS of a node with [`FOREIGN_KEY`].
pub(super) struct Foreign {
    /// The instant at the start of a run.
    epoch: Instant,
    settings: Settings,
    endpoint: noq_proto::Endpoint,
    /// The one connection it dialed or accepted.
    connection: Option<(ConnectionHandle, noq_proto::Connection)>,
    /// Datagrams that no connection sends: destination and bytes.
    responses: Vec<(SocketAddr, Vec<u8>)>,
    /// Each event of its connection, in order.
    pub(super) events: Vec<noq_proto::Event>,
    /// A datagram it sends as soon as its connection can.
    pub(super) datagram: Option<Bytes>,
}

impl Foreign {
    /// A peer with the transport parameters of a node on `shard`, with `change` made.
    pub(super) fn new(
        shard: &Shard,
        change: impl FnOnce(&mut TransportConfig),
    ) -> Self {
        let config = shard.config(FOREIGN_KEY, Span::SECOND);
        let (settings, endpoint) = Settings::foreign(&config, FOREIGN_SHARD, change);
        Self {
            epoch: config.clock.epoch(),
            settings,
            endpoint,
            connection: None,
            responses: Vec::new(),
            events: Vec::new(),
            datagram: None,
        }
    }

    /// Dials `remote` at `now`, which must prove `peer`.
    pub(super) fn dial(&mut self, now: Monotonic, peer: PublicKey, remote: SocketAddr) {
        let dial = self.settings.client(peer);
        self.connect(now, dial, remote);
    }

    /// Dials `remote` at `now` with `tls`.
    pub(super) fn dial_with(
        &mut self,
        now: Monotonic,
        tls: Arc<rustls::ClientConfig>,
        remote: SocketAddr,
    ) {
        let dial = self.settings.dial(tls);
        self.connect(now, dial, remote);
    }

    fn connect(&mut self, now: Monotonic, dial: ClientConfig, remote: SocketAddr) {
        let now = self.epoch + Duration::from_nanos(now.0);
        let connection = self.endpoint.connect(now, dial, remote, SERVER_NAME);
        self.connection = Some(connection.expect("a dial"));
    }

    /// Its connection.
    ///
    /// # Panics
    ///
    /// When it has none.
    pub(super) fn connection(&mut self) -> &mut noq_proto::Connection {
        &mut self.connection.as_mut().expect("a connection").1
    }

    fn deadline(&self) -> Option<Duration> {
        let (_, connection) = self.connection.as_ref()?;
        let deadline = connection.poll_timeout()?;
        Some(deadline.saturating_duration_since(self.epoch))
    }

    fn timeout(&mut self, now: Duration) {
        let now = self.epoch + now;
        if let Some((_, connection)) = &mut self.connection
            && connection
                .poll_timeout()
                .is_some_and(|deadline| deadline <= now)
        {
            connection.handle_timeout(now);
        }
    }

    fn receive(&mut self, now: Duration, meta: &Meta, bytes: &[u8]) {
        let now = self.epoch + now;
        let path = FourTuple::new(meta.source, None);
        for datagram in bytes[..meta.len].chunks(meta.stride) {
            let datagram = BytesMut::from(datagram);
            let mut reply = Vec::new();
            match self.endpoint.handle(now, path, None, datagram, &mut reply) {
                Some(DatagramEvent::NewConnection(incoming)) => {
                    let accepted =
                        self.endpoint.accept(incoming, now, &mut reply, None);
                    self.connection = Some(accepted.expect("an accept"));
                }
                Some(DatagramEvent::ConnectionEvent(_, event)) => {
                    self.connection().handle_event(event);
                }
                Some(DatagramEvent::Response(transmit)) => {
                    let reply = reply[..transmit.size].to_vec();
                    self.responses.push((transmit.destination, reply));
                }
                None => {}
            }
            self.drive();
        }
    }

    /// Takes every event and datagram the connection has at `now`, and gives the
    /// batches the link carries, one datagram each.
    fn flush(&mut self, now: Duration) -> Vec<(SocketAddr, Meta, Vec<u8>)> {
        let now = self.epoch + now;
        let mut out = mem::take(&mut self.responses);
        if let Some((_, connection)) = &mut self.connection {
            let mut buffer = Vec::new();
            while let Some(transmit) =
                connection.poll_transmit(now, NonZeroUsize::MIN, &mut buffer)
            {
                out.push((transmit.destination, buffer[..transmit.size].to_vec()));
                buffer.clear();
            }
        }
        self.drive();
        let out = out
            .into_iter()
            .map(|(to, bytes)| (to, meta(FOREIGN, &bytes), bytes));
        out.collect()
    }

    /// Moves the connection's events to the endpoint and to [`Foreign::events`].
    fn drive(&mut self) {
        let Some((handle, connection)) = &mut self.connection else {
            return;
        };
        while let Some(event) = connection.poll_endpoint_events() {
            if let Some(event) = self.endpoint.handle_event(*handle, event) {
                connection.handle_event(event);
            }
        }
        while let Some(event) = connection.poll() {
            self.events.push(event);
        }
        if let Some(datagram) = self.datagram.take() {
            match connection.datagrams().send(datagram.clone(), false) {
                Ok(()) => {}
                // Until it has the peer's transport parameters.
                Err(SendDatagramError::UnsupportedByPeer) => {
                    self.datagram = Some(datagram);
                }
                Err(error) => panic!("{error}"),
            }
        }
    }
}
