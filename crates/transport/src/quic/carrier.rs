//! One shard's QUIC endpoint on a UDP socket: a task that moves its datagrams and
//! timers, and the sessions it carries.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};

use env::clock::{Clock, Sleep};
use env::net::Ecn;
use env::net::udp::{self, Meta, Transmit};
use types::node::PublicKey;
use types::time::Monotonic;

use super::{Endpoint, Event, connection};
use crate::{Code, Config, Error, PAYLOAD_IPV4, Peer};

/// The most batches one receive takes.
const BATCHES: usize = 8;

/// One shard's QUIC endpoint on a UDP socket. A task on the shard moves its
/// datagrams and runs its timers until the socket breaks, or until every clone and
/// every [`Session`] dropped. It stays on the thread that made it.
#[derive(Clone)]
pub(crate) struct Carrier(Rc<RefCell<State>>);

impl Carrier {
    /// Starts an endpoint for `config` whose connection IDs start with `shard`, on
    /// the socket of `sender` and `receiver`, and spawns its task on `config.tasks`.
    ///
    /// # Panics
    ///
    /// When [`Transport::new`](crate::Transport::new) refuses `config`, with its error.
    pub(crate) fn new(
        config: &Config,
        shard: u8,
        sender: udp::Sender,
        receiver: udp::Receiver,
    ) -> Self {
        let state = Rc::new(RefCell::new(State {
            endpoint: Endpoint::new(config, shard, sender.batch_max()),
            clock: config.clock.clone(),
            task: None,
            sessions: BTreeMap::new(),
            accepted: VecDeque::new(),
            accepting: Vec::new(),
            failed: None,
        }));
        let task = Task::new(Rc::downgrade(&state), &config.clock, sender, receiver);
        config.tasks.spawn(task.run());
        Self(state)
    }

    /// Dials `remote` and waits until it proves `peer`. The session may have ended
    /// since. Dropping the future closes the dial.
    ///
    /// # Errors
    ///
    /// Why the dial ended before the handshake finished, as [`Session::closed`]
    /// gives it.
    ///
    /// # Panics
    ///
    /// When no datagram can go to `remote`: its port is 0 or its IP is unspecified.
    pub(crate) async fn connect(
        &self,
        peer: PublicKey,
        remote: SocketAddr,
    ) -> Result<Session, Error> {
        let session = self.dial(peer, remote)?;
        poll_fn(|cx| session.poll_connected(cx)).await?;
        Ok(session)
    }

    /// Waits for the next session that a peer dialed. Each that connects comes once,
    /// and it may have ended since.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] when the socket broke.
    pub(crate) async fn accept(&self) -> Result<Session, Error> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    fn dial(&self, peer: PublicKey, remote: SocketAddr) -> Result<Session, Error> {
        let mut state = self.0.borrow_mut();
        if let Some(error) = &state.failed {
            return Err(Error::Network {
                error: error.clone(),
            });
        }
        let now = state.clock.now();
        let key = state.endpoint.connect(now, peer, remote);
        state.sessions.insert(key, Slot::default());
        state.wake();
        Ok(self.session(key))
    }

    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<Result<Session, Error>> {
        let mut state = self.0.borrow_mut();
        if let Some(key) = state.accepted.pop_front() {
            return Poll::Ready(Ok(self.session(key)));
        }
        if let Some(error) = &state.failed {
            return Poll::Ready(Err(Error::Network {
                error: error.clone(),
            }));
        }
        register(&mut state.accepting, cx.waker());
        Poll::Pending
    }

    fn session(&self, key: connection::Key) -> Session {
        Session {
            carrier: self.clone(),
            key,
        }
    }
}

/// What a [`Carrier`], its sessions, and its task share.
struct State {
    endpoint: Endpoint,
    clock: Clock,
    /// The waker of the task's last poll.
    task: Option<Waker>,
    /// Each connection a [`Session`] holds, or held before the connection ended.
    sessions: BTreeMap<connection::Key, Slot>,
    /// The connections peers dialed, in the order they connected, for
    /// [`Carrier::accept`].
    accepted: VecDeque<connection::Key>,
    /// The wakers of the [`Carrier::accept`] calls that wait.
    accepting: Vec<Waker>,
    /// What broke the socket.
    failed: Option<env::net::Error>,
}

impl State {
    fn wake(&self) {
        if let Some(task) = &self.task {
            task.wake_by_ref();
        }
    }

    fn slot(&mut self, key: connection::Key) -> &mut Slot {
        self.sessions
            .get_mut(&key)
            .expect("invariant: a connection keeps its slot until it ends")
    }

    fn dispatch(&mut self, event: Event) {
        match event {
            Event::Connected { key, peer } => {
                if let Some(slot) = self.sessions.get_mut(&key) {
                    slot.peer = Some(peer);
                    slot.wake();
                    return;
                }
                let slot = Slot {
                    peer: Some(peer),
                    ..Slot::default()
                };
                self.sessions.insert(key, slot);
                self.accepted.push_back(key);
                self.accepting.drain(..).for_each(Waker::wake);
            }
            Event::Closed { key, error } => self.end(key, error),
            // No session takes streams or datagrams yet.
            Event::Incoming { .. }
            | Event::Available { .. }
            | Event::Readable { .. }
            | Event::Writable { .. }
            | Event::Datagram { .. } => {}
        }
    }

    fn end(&mut self, key: connection::Key, error: Error) {
        let slot = self.slot(key);
        if slot.dropped {
            self.sessions.remove(&key);
            return;
        }
        slot.end = Some(error);
        slot.wake();
    }

    /// Ends each session with [`Error::Network`], and refuses each later dial and
    /// accept.
    fn fail(&mut self, error: env::net::Error) {
        self.sessions.retain(|_, slot| !slot.dropped);
        for slot in self.sessions.values_mut() {
            slot.end.get_or_insert_with(|| Error::Network {
                error: error.clone(),
            });
            slot.wake();
        }
        self.accepting.drain(..).for_each(Waker::wake);
        self.failed = Some(error);
    }
}

impl Drop for State {
    /// Wakes the task, which ends when it finds no state.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.wake();
        }
    }
}

/// The part of [`State`] for one connection.
#[derive(Default)]
struct Slot {
    /// The peer, once the handshake finishes.
    peer: Option<Peer>,
    /// Why the connection ended.
    end: Option<Error>,
    /// The wakers of the calls that wait on the session.
    wakers: Vec<Waker>,
    /// The session dropped before the connection ended.
    dropped: bool,
}

impl Slot {
    fn wake(&mut self) {
        self.wakers.drain(..).for_each(Waker::wake);
    }
}

/// A connection to one peer on a [`Carrier`]. Dropping it closes the connection
/// with code 0.
pub(crate) struct Session {
    carrier: Carrier,
    key: connection::Key,
}

impl Session {
    /// The peer: a node that proved its key, or a client with no key.
    pub(crate) fn peer(&self) -> Peer {
        let mut state = self.carrier.0.borrow_mut();
        state
            .slot(self.key)
            .peer
            .expect("invariant: a session has connected")
    }

    /// Closes the session with `code`, which the peer gets as
    /// [`Error::PeerClosed`]. Does nothing when the session ended.
    pub(crate) fn close(&self, code: Code) {
        let mut state = self.carrier.0.borrow_mut();
        let now = state.clock.now();
        state.endpoint.close(now, self.key, code);
        state.wake();
    }

    /// Waits until the session ends, and gives why.
    pub(crate) async fn closed(&self) -> Error {
        poll_fn(|cx| {
            let mut state = self.carrier.0.borrow_mut();
            let slot = state.slot(self.key);
            if let Some(error) = &slot.end {
                return Poll::Ready(error.clone());
            }
            register(&mut slot.wakers, cx.waker());
            Poll::Pending
        })
        .await
    }

    /// Ready when the handshake finished, or with why the dial ended.
    fn poll_connected(&self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let mut state = self.carrier.0.borrow_mut();
        let slot = state.slot(self.key);
        if slot.peer.is_some() {
            return Poll::Ready(Ok(()));
        }
        if let Some(error) = &slot.end {
            return Poll::Ready(Err(error.clone()));
        }
        register(&mut slot.wakers, cx.waker());
        Poll::Pending
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut state = self.carrier.0.borrow_mut();
        let slot = state.slot(self.key);
        if slot.end.is_some() {
            state.sessions.remove(&self.key);
            return;
        }
        slot.dropped = true;
        let now = state.clock.now();
        state.endpoint.close(now, self.key, Code(0));
        state.wake();
    }
}

/// Moves the endpoint's datagrams between it and the socket, and runs its timers.
struct Task {
    state: Weak<RefCell<State>>,
    sender: udp::Sender,
    receiver: udp::Receiver,
    /// Completes at the endpoint's deadline.
    sleep: Sleep,
    /// One buffer for each batch of a receive.
    received: [Vec<u8>; BATCHES],
    metas: [Meta; BATCHES],
    /// The bytes of the last transmit.
    out: Vec<u8>,
    /// The transmit in `out` that the socket had no room for.
    held: Option<Held>,
}

impl Task {
    fn new(
        state: Weak<RefCell<State>>,
        clock: &Clock,
        sender: udp::Sender,
        receiver: udp::Receiver,
    ) -> Self {
        let bytes = usize::from(PAYLOAD_IPV4) * receiver.batch_max().get();
        Self {
            state,
            sender,
            receiver,
            sleep: clock.sleep_until(Monotonic(0)),
            received: std::array::from_fn(|_| vec![0; bytes]),
            metas: std::array::from_fn(|_| Meta::default()),
            out: Vec::new(),
            held: None,
        }
    }

    async fn run(mut self) {
        poll_fn(|cx| self.poll(cx)).await;
    }

    /// Ready when the socket broke, or when the carrier and its sessions dropped.
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let Some(shared) = self.state.upgrade() else {
            return Poll::Ready(());
        };
        let mut state = shared.borrow_mut();
        if !state
            .task
            .as_ref()
            .is_some_and(|task| task.will_wake(cx.waker()))
        {
            state.task = Some(cx.waker().clone());
        }
        let now = state.clock.now();
        // One receive per poll, so a busy socket does not starve the shard.
        let mut more = false;
        match self.receive(cx) {
            Poll::Ready(Ok(batches)) => {
                let received = self.metas.iter().zip(&self.received);
                for (meta, batch) in received.take(batches) {
                    state.endpoint.receive(now, meta, batch);
                }
                more = true;
            }
            Poll::Ready(Err(error)) => {
                state.fail(error);
                return Poll::Ready(());
            }
            Poll::Pending => {}
        }
        state.endpoint.timeout(now);
        self.send(cx, &mut state.endpoint, now);
        while let Some(event) = state.endpoint.poll() {
            state.dispatch(event);
        }
        if let Some(deadline) = state.endpoint.deadline() {
            self.sleep.reset(deadline);
            more |= Pin::new(&mut self.sleep).poll(cx).is_ready();
        }
        if more {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }

    fn receive(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<usize, env::net::Error>> {
        let mut buffers = self.received.each_mut().map(|b| IoSliceMut::new(b));
        self.receiver.poll_recv(cx, &mut buffers, &mut self.metas)
    }

    /// Sends what the endpoint has until the socket has no room. A failed send is a
    /// loss, which QUIC recovers.
    fn send(&mut self, cx: &mut Context<'_>, endpoint: &mut Endpoint, now: Monotonic) {
        if let Some(held) = &self.held {
            let transmit = held.transmit(&self.out);
            if self.sender.poll_send(cx, &transmit).is_pending() {
                return;
            }
            self.held = None;
        }
        while let Some(transmit) = endpoint.transmit(now, &mut self.out) {
            if self.sender.poll_send(cx, &transmit).is_pending() {
                self.held = Some(Held::new(&transmit));
                return;
            }
        }
    }
}

/// A [`Transmit`] without its bytes, which wait in [`Task::out`].
struct Held {
    destination: SocketAddr,
    source: Option<IpAddr>,
    ecn: Option<Ecn>,
    len: usize,
    segment: Option<NonZeroUsize>,
}

impl Held {
    fn new(transmit: &Transmit<'_>) -> Self {
        Self {
            destination: transmit.destination,
            source: transmit.source,
            ecn: transmit.ecn,
            len: transmit.contents.len(),
            segment: transmit.segment,
        }
    }

    fn transmit<'a>(&self, out: &'a [u8]) -> Transmit<'a> {
        Transmit {
            destination: self.destination,
            source: self.source,
            ecn: self.ecn,
            contents: &out[..self.len],
            segment: self.segment,
        }
    }
}

/// Adds `waker` to `wakers` unless one there wakes the same task.
fn register(wakers: &mut Vec<Waker>, waker: &Waker) {
    if !wakers.iter().any(|w| w.will_wake(waker)) {
        wakers.push(waker.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::net::SocketAddr;
    use std::pin::pin;
    use std::task::Poll;

    use env::net::udp;
    use sim::Sim;
    use sim::node::Node;
    use types::node::PrivateKey;
    use types::time::Span;

    use super::Carrier;
    use crate::testing::Shard;
    use crate::tls::public;
    use crate::{Code, Error, Peer};

    const PORT: u16 = 4433;
    const IDLE: Span = Span::SECOND;
    const CLIENT: PrivateKey = PrivateKey([1; 32]);
    const SERVER: PrivateKey = PrivateKey([2; 32]);

    /// A run from `value` with a client node and a server node.
    fn nodes(value: u64) -> (Sim, Node, Node) {
        let mut sim = Sim::new(sim::Config {
            seed: value,
            ..sim::Config::default()
        });
        let client = sim.node(sim::node::Config::default());
        let server = sim.node(sim::node::Config::default());
        (sim, client, server)
    }

    fn address(node: &Node) -> SocketAddr {
        SocketAddr::new(node.addresses()[0], PORT)
    }

    fn socket(node: &Node) -> Result<(udp::Sender, udp::Receiver), env::net::Error> {
        node.net().udp(&udp::Config {
            local: address(node),
            send_buffer_bytes: 1 << 20,
            recv_buffer_bytes: 1 << 20,
        })
    }

    fn spans(span: Span, n: i64) -> Span {
        Span::from_nanos(span.nanos() * n)
    }

    /// Starts a shard on `node` that runs `body` with a carrier for `key` at
    /// [`address`].
    fn start<F: Future<Output = ()> + 'static>(
        node: &Node,
        key: PrivateKey,
        body: impl FnOnce(Carrier, Node) -> F + Send + 'static,
    ) {
        let own = node.clone();
        let config = env::shards::Config {
            name: "carrier".into(),
            core: None,
        };
        let started = node.shards().start(config, move |tasks| async move {
            let config = Shard::new(&own, tasks).config(key, IDLE);
            let (sender, receiver) = socket(&own).expect("a socket");
            body(Carrier::new(&config, 0, sender, receiver), own).await;
        });
        drop(started.expect("a shard"));
    }

    /// Runs a dial from `value` that the client closes with code 5, and gives the
    /// digest of the run.
    fn dial(value: u64) -> u64 {
        let (mut sim, client, server) = nodes(value);
        let at = address(&server);
        start(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            assert_eq!(session.peer(), Peer::Node(public(&CLIENT)));
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        start(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(public(&SERVER), at).await;
            let session = dialed.expect("a session");
            assert_eq!(session.peer(), Peer::Node(public(&SERVER)));
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        assert_eq!(sim.run(), Ok(()));
        sim.digest()
    }

    #[test]
    fn a_dial_proves_each_key_and_carries_the_close_code() {
        dial(0);
    }

    #[test]
    fn the_same_value_gives_the_same_run() {
        assert_eq!(dial(3), dial(3));
    }

    #[test]
    fn a_quiet_session_outlives_the_idle_timeout() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        start(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        start(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(public(&SERVER), at).await;
            let session = dialed.expect("a session");
            node.clock().sleep(spans(IDLE, 3)).await;
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_cut_link_times_out_each_side() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        start(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            assert_eq!(session.closed().await, Error::TimedOut);
        });
        start(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(public(&SERVER), at).await;
            let session = dialed.expect("a session");
            assert_eq!(session.closed().await, Error::TimedOut);
        });
        assert_eq!(sim.run_for(spans(Span::MILLISECOND, 500)), Ok(()));
        let cut = sim::link::Config {
            loss: 1.0,
            ..sim::link::Config::default()
        };
        sim.link(&client, &server, cut);
        sim.link(&server, &client, cut);
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_to_no_socket_times_out() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        start(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(public(&SERVER), at).await;
            assert_eq!(dialed.err(), Some(Error::TimedOut));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn accept_gives_a_session_that_ended_before_it() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        start(&server, SERVER, |carrier, node| async move {
            node.clock().sleep(IDLE).await;
            let session = carrier.accept().await.expect("a session");
            assert_eq!(session.peer(), Peer::Node(public(&CLIENT)));
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        start(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(public(&SERVER), at).await;
            let session = dialed.expect("a session");
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dropped_session_closes_with_code_0_and_frees_its_slot() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        start(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(session.closed().await, closed);
        });
        start(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(public(&SERVER), at).await;
            drop(dialed.expect("a session"));
            node.clock().sleep(Span::MILLISECOND).await;
            assert_eq!(carrier.0.borrow().sessions.len(), 0);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dropped_dial_frees_its_slot() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        start(&client, CLIENT, move |carrier, node| async move {
            {
                let mut dial = pin!(carrier.connect(public(&SERVER), at));
                poll_fn(|cx| Poll::Ready(dial.as_mut().poll(cx).is_pending())).await;
                assert_eq!(carrier.0.borrow().sessions.len(), 1);
            }
            node.clock().sleep(Span::MILLISECOND).await;
            assert_eq!(carrier.0.borrow().sessions.len(), 0);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn the_last_drop_frees_the_socket() {
        let (mut sim, client, _) = nodes(0);
        start(&client, CLIENT, move |carrier, node| async move {
            let local = address(&node);
            assert_eq!(
                socket(&node).err(),
                Some(env::net::Error::AddressInUse { local })
            );
            // The task waits on the socket, so only the drop can wake it.
            node.clock().sleep(Span::MILLISECOND).await;
            drop(carrier);
            node.clock().sleep(Span::MILLISECOND).await;
            assert_eq!(socket(&node).err(), None);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_broken_socket_ends_each_session_and_refuses_new_ones() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        start(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            assert_eq!(session.closed().await, Error::TimedOut);
        });
        start(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(public(&SERVER), at).await;
            let session = dialed.expect("a session");
            let error = env::net::Error::Io { code: 5 };
            carrier.0.borrow_mut().fail(error.clone());
            let network = Error::Network { error };
            assert_eq!(session.closed().await, network);
            let dialed = carrier.connect(public(&SERVER), at).await;
            assert_eq!(dialed.err(), Some(network.clone()));
            assert_eq!(carrier.accept().await.err(), Some(network));
        });
        assert_eq!(sim.run(), Ok(()));
    }
}
