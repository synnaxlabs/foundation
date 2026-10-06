//! Two endpoints over a link in virtual time.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::time::Duration;

use env::net::udp::Meta;
use types::node::{PrivateKey, PublicKey};
use types::time::{Monotonic, Span};

use super::settings::MTU_MIN;
use super::{Endpoint, Event, cid, connection, find, queue};
use crate::testing::Shard;

/// The client's address. The server's is [`SERVER`].
pub(super) const CLIENT: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
/// The server's address.
pub(super) const SERVER: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2);
/// The client's shard. The server's is [`SERVER_SHARD`].
pub(super) const CLIENT_SHARD: u8 = 3;
/// The server's shard.
pub(super) const SERVER_SHARD: u8 = 5;
/// The client's node key. The server's is [`SERVER_KEY`].
pub(super) const CLIENT_KEY: PrivateKey = PrivateKey([1; 32]);
/// The server's node key.
pub(super) const SERVER_KEY: PrivateKey = PrivateKey([2; 32]);

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
        let config = shard.config(CLIENT_KEY, idle);
        let client = Endpoint::new(&config, CLIENT_SHARD, NonZeroUsize::MIN);
        let config = shard.config(SERVER_KEY, idle);
        let server = Endpoint::new(&config, SERVER_SHARD, NonZeroUsize::MIN);
        Self {
            now: Duration::ZERO,
            delay,
            idle,
            client: Side::new(client, CLIENT),
            server: Side::new(server, SERVER),
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
            let next = [self.client.deadline(), self.server.deadline()]
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
    }

    fn deliver(&mut self, to: SocketAddr, meta: &Meta, bytes: &[u8]) {
        let now = at(self.now);
        let side = [&mut self.client, &mut self.server]
            .into_iter()
            .find(|side| side.address == to && !side.silent);
        if let Some(side) = side {
            side.endpoint.receive(now, meta, bytes);
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
