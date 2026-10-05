//! A sim shard for a [`Config`], and a dial between two noq-proto endpoints over a
//! link in virtual time.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Heap, Pool};
use bytes::BytesMut;
use env::clock::Clock;
use env::entropy::Entropy;
use env::tasks::Tasks;
use noq_proto::{
    Connection, ConnectionHandle, DatagramEvent, Endpoint, Event, FourTuple,
};
use types::node::{PrivateKey, PublicKey};
use types::time::Span;

use super::settings::Settings;
use crate::Config;

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
/// The most streams of each kind a peer may open, in [`Shard::config`].
pub(super) const STREAMS_MAX: u32 = 16;

const CLIENT_KEY: PrivateKey = PrivateKey([1; 32]);
const SERVER_KEY: PrivateKey = PrivateKey([2; 32]);

/// What one sim shard gives a [`Config`].
pub(super) struct Shard {
    clock: Clock,
    entropy: Entropy,
    tasks: Tasks,
    pool: Rc<Pool>,
}

impl Shard {
    /// When the run starts.
    pub(super) fn epoch(&self) -> Instant {
        self.clock.epoch()
    }

    /// A config for a node with `private_key` and `idle`, on this shard.
    pub(super) fn config(&self, private_key: PrivateKey, idle: Span) -> Config {
        Config {
            private_key,
            message_bytes_max: NonZeroUsize::new(1 << 16).expect("not zero"),
            window_bytes: 1 << 20,
            streams_max: NonZeroU32::new(STREAMS_MAX).expect("not zero"),
            idle,
            clock: self.clock.clone(),
            entropy: self.entropy.clone(),
            tasks: self.tasks.clone(),
            pool: Rc::clone(&self.pool),
        }
    }
}

/// Runs `test` on one shard of a sim run made from `value`, and gives its result.
pub(super) fn run<T: Send + 'static>(
    value: u64,
    test: impl FnOnce(&Shard) -> T + Send + 'static,
) -> T {
    let mut sim = sim::Sim::new(sim::Config {
        seed: value,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    let (clock, entropy) = (node.clock(), node.entropy());
    let shard = env::shards::Config {
        name: "shard-0".into(),
        core: Some(0),
    };
    let result = Arc::new(Mutex::new(None));
    let out = Arc::clone(&result);
    let handle = node.shards().start(shard, move |tasks| async move {
        let config = block::Config { budget: 1 << 22 };
        let memory = Heap::new(config.reservation());
        let shard = Shard {
            clock,
            entropy,
            tasks,
            pool: Rc::new(Pool::new(config, memory)),
        };
        *out.lock().expect("not poisoned") = Some(test(&shard));
    });
    sim.run().expect("the run ends");
    handle
        .expect("the shard starts")
        .join()
        .expect("the test passes");
    let result = result.lock().expect("not poisoned").take();
    result.expect("the test ran")
}

/// A dial from a node on [`CLIENT_SHARD`] to a node on [`SERVER_SHARD`], over a
/// link that delivers each datagram `delay` after it leaves, unless it drops it.
/// Time moves only in [`Pair::run`].
pub(super) struct Pair {
    start: Instant,
    now: Instant,
    delay: Duration,
    idle: Span,
    /// The side that dials.
    pub(super) client: Side,
    /// The side that accepts.
    pub(super) server: Side,
    /// Arrival, source, destination, and bytes of each datagram on the link, in
    /// the order they arrive.
    link: VecDeque<(Instant, SocketAddr, SocketAddr, Vec<u8>)>,
}

/// One side of a [`Pair`].
pub(super) struct Side {
    endpoint: Endpoint,
    connection: Option<(ConnectionHandle, Connection)>,
    /// Where this side sends from and takes datagrams.
    pub(super) address: SocketAddr,
    /// When (from the start), where to, and the bytes of each datagram this side
    /// sent, the dropped ones too.
    pub(super) sent: Vec<(Duration, SocketAddr, Vec<u8>)>,
    /// When (from the start) and what of each event of this side's connection.
    pub(super) events: Vec<(Duration, Event)>,
    /// The link drops this many of the next datagrams this side sends.
    pub(super) drops: usize,
    /// This side stops: it runs no timer, sends nothing, and gets nothing.
    pub(super) silent: bool,
}

impl Pair {
    /// Starts the dial, with `idle` on both nodes. Nothing is sent yet.
    pub(super) fn new(shard: &Shard, idle: Span, delay: Duration) -> Self {
        let start = shard.epoch();
        let (settings, endpoint) =
            Settings::new(&shard.config(CLIENT_KEY, idle), CLIENT_SHARD);
        let mut client = Side::new(endpoint, CLIENT);
        let pair =
            Ed25519KeyPair::from_seed_unchecked(&SERVER_KEY.0).expect("32 bytes");
        let expected =
            PublicKey(pair.public_key().as_ref().try_into().expect("32 bytes"));
        let dial = settings.client(expected);
        client.connection = Some(
            client
                .endpoint
                .connect(start, dial, SERVER, "foundation")
                .expect("the dial starts"),
        );
        let (_, endpoint) =
            Settings::new(&shard.config(SERVER_KEY, idle), SERVER_SHARD);
        Self {
            start,
            now: start,
            delay,
            idle,
            client,
            server: Side::new(endpoint, SERVER),
            link: VecDeque::new(),
        }
    }

    fn elapsed(&self) -> Duration {
        self.now - self.start
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
                let (_, from, to, bytes) = self.link.pop_front().expect("a datagram");
                self.deliver(from, to, &bytes);
            }
            for side in [&mut self.client, &mut self.server] {
                side.timeout(self.now);
            }
        }
        self.now = end;
    }

    /// Replaces the server's endpoint with a new one for the same node key and
    /// shard, as a restart does. The old connection is gone.
    pub(super) fn restart(&mut self, shard: &Shard) {
        let config = shard.config(SERVER_KEY, self.idle);
        let (_, endpoint) = Settings::new(&config, SERVER_SHARD);
        self.server.endpoint = endpoint;
        self.server.connection = None;
    }

    fn flush(&mut self) {
        let (now, elapsed) = (self.now, self.elapsed());
        for side in [&mut self.client, &mut self.server] {
            for (to, bytes) in side.flush(now, elapsed) {
                self.link
                    .push_back((now + self.delay, side.address, to, bytes));
            }
        }
    }

    fn deliver(&mut self, from: SocketAddr, to: SocketAddr, bytes: &[u8]) {
        let (now, elapsed) = (self.now, self.elapsed());
        let Some(side) = [&mut self.client, &mut self.server]
            .into_iter()
            .find(|side| side.address == to && !side.silent)
        else {
            return;
        };
        if let Some((to, bytes)) = side.take(now, elapsed, from, bytes) {
            self.link
                .push_back((now + self.delay, side.address, to, bytes));
        }
    }
}

impl Side {
    fn new(endpoint: Endpoint, address: SocketAddr) -> Self {
        Self {
            endpoint,
            connection: None,
            address,
            sent: Vec::new(),
            events: Vec::new(),
            drops: 0,
            silent: false,
        }
    }

    /// This side's connection.
    ///
    /// # Panics
    ///
    /// When it has none.
    pub(super) fn connection(&mut self) -> &mut Connection {
        let (_, connection) = self.connection.as_mut().expect("a connection");
        connection
    }

    fn deadline(&self) -> Option<Instant> {
        let (_, connection) = self.connection.as_ref().filter(|_| !self.silent)?;
        connection.poll_timeout()
    }

    fn timeout(&mut self, now: Instant) {
        if let Some(deadline) = self.deadline()
            && deadline <= now
        {
            self.connection().handle_timeout(now);
        }
    }

    /// Records a datagram this side sends, and gives it back unless the link drops
    /// it.
    fn send(
        &mut self,
        elapsed: Duration,
        to: SocketAddr,
        bytes: Vec<u8>,
    ) -> Option<(SocketAddr, Vec<u8>)> {
        self.sent.push((elapsed, to, bytes.clone()));
        if self.drops > 0 {
            self.drops -= 1;
            return None;
        }
        Some((to, bytes))
    }

    /// Takes every event and datagram the connection has, and gives the datagrams
    /// the link carries.
    fn flush(&mut self, now: Instant, elapsed: Duration) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut out = Vec::new();
        if self.silent {
            return out;
        }
        while let Some((handle, connection)) = self.connection.as_mut() {
            let mut bytes = Vec::new();
            if let Some(sent) =
                connection.poll_transmit(now, NonZeroUsize::MIN, &mut bytes)
            {
                assert_eq!(sent.size, bytes.len());
                out.extend(self.send(elapsed, sent.destination, bytes));
            } else if let Some(event) = connection.poll_endpoint_events() {
                if let Some(event) = self.endpoint.handle_event(*handle, event) {
                    connection.handle_event(event);
                }
            } else if let Some(event) = connection.poll() {
                self.events.push((elapsed, event));
            } else {
                break;
            }
        }
        out
    }

    /// Takes a datagram from `from`, and gives the endpoint's reply, if any, unless
    /// the link drops it.
    fn take(
        &mut self,
        now: Instant,
        elapsed: Duration,
        from: SocketAddr,
        bytes: &[u8],
    ) -> Option<(SocketAddr, Vec<u8>)> {
        let mut reply = Vec::new();
        let datagram = BytesMut::from(bytes);
        let path = FourTuple::new(from, None);
        match self
            .endpoint
            .handle(now, path, None, datagram, &mut reply)?
        {
            DatagramEvent::ConnectionEvent(handle, event) => {
                let Some((own, connection)) = self.connection.as_mut() else {
                    panic!("a datagram for no connection");
                };
                assert_eq!(handle, *own, "a datagram for another connection");
                connection.handle_event(event);
                None
            }
            DatagramEvent::NewConnection(incoming) => {
                assert!(self.connection.is_none(), "a second connection");
                let accepted = self
                    .endpoint
                    .accept(incoming, now, &mut reply, None)
                    .unwrap_or_else(|error| {
                        panic!("the server refuses: {:?}", error.cause)
                    });
                self.connection = Some(accepted);
                None
            }
            DatagramEvent::Response(sent) => {
                assert_eq!(sent.size, reply.len());
                self.send(elapsed, sent.destination, reply)
            }
        }
    }
}
