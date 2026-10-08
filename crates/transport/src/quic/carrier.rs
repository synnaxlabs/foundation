//! One shard's QUIC endpoint on a UDP socket: a task that moves its datagrams and
//! timers, and the sessions it carries.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use block::Block;
use env::clock::{Clock, Sleep};
use env::net::Ecn;
use env::net::udp::{self, Meta, Transmit};
use noq_proto::StreamId;
use types::ed25519::PublicKey;
use types::hash::Map;
use types::time::Monotonic;

use super::stream::{Incoming, Receiver, Sender};
use super::wait::{self, RETRY};
use super::{Endpoint, Event, Setup, connection};
use crate::{Class, Code, Error, PAYLOAD_IPV4, Peer, Status, port};

/// The most batches one poll of the task takes, so a busy socket does not starve the
/// shard's other tasks.
const BATCHES: usize = 8;

/// One shard's QUIC endpoint on a UDP socket. A task on the shard moves its
/// datagrams and runs its timers until the socket breaks, or until the carrier
/// dropped and each connection drained. It stays on the thread that made it.
pub(crate) struct Carrier(Rc<RefCell<State>>);

impl Carrier {
    /// Starts an endpoint for `setup` on `part`, and spawns its task on
    /// `setup.tasks`.
    pub(crate) fn new(setup: Setup, part: port::Part) -> Self {
        let port::Part {
            index,
            sender,
            receiver,
        } = part;
        let endpoint = Endpoint::new(&setup, index, sender.batch_max());
        let Setup { clock, tasks, .. } = setup;
        let state = Rc::new(RefCell::new(State {
            endpoint,
            clock: clock.clone(),
            task: None,
            sessions: BTreeMap::new(),
            accepted: Some(VecDeque::new()),
            accepting: Vec::new(),
            failed: None,
            connects: 0,
            waits: wait::Queue::default(),
        }));
        let task = Task::new(Rc::clone(&state), &clock, sender, receiver);
        tasks.spawn(task.run());
        Self(state)
    }

    /// Dials `remote` and waits until it proves `peer`. The session may have ended
    /// since. Dropping the future closes the dial.
    ///
    /// # Errors
    ///
    /// As [`Carrier::dial`], or why the dial ended before the handshake finished, as
    /// [`Session::closed`] gives it.
    ///
    /// # Panics
    ///
    /// As [`Carrier::dial`].
    #[cfg(test)]
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
    /// [`Error::Network`] when the socket broke and each session that connected
    /// before was given.
    pub(crate) async fn accept(&self) -> Result<Session, Error> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    /// What the carrier counted.
    pub(crate) fn status(&self) -> Status {
        let state = self.0.borrow();
        state.waits.status(state.clock.now())
    }

    /// The clock of the carrier's endpoint.
    pub(crate) fn clock(&self) -> Clock {
        self.0.borrow().clock.clone()
    }

    /// Checks that the socket still works.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] when the socket broke.
    pub(crate) fn check(&self) -> Result<(), Error> {
        match &self.0.borrow().failed {
            Some(error) => Err(Error::Network {
                error: error.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Starts a dial to `remote` that `peer` must answer, and gives its session,
    /// which [`Session::poll_connected`] waits on. Dropping the session closes the
    /// dial.
    ///
    /// # Errors
    ///
    /// As [`Carrier::check`].
    ///
    /// # Panics
    ///
    /// When no datagram can go to `remote`: its port is 0 or its IP is unspecified.
    pub(crate) fn dial(
        &self,
        peer: PublicKey,
        remote: SocketAddr,
    ) -> Result<Session, Error> {
        self.check()?;
        let mut state = self.0.borrow_mut();
        let now = state.clock.now();
        let key = state.endpoint.connect(now, peer, remote);
        state.sessions.insert(key, Slot::default());
        state.wake();
        Ok(self.session(key))
    }

    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<Result<Session, Error>> {
        let mut state = self.0.borrow_mut();
        if let Some(key) = state.accepted.as_mut().and_then(VecDeque::pop_front) {
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
            state: Rc::clone(&self.0),
            key,
        }
    }
}

impl Drop for Carrier {
    /// Refuses each later dial from a peer until each connection drained, and closes
    /// each session that no caller accepted with code 0.
    fn drop(&mut self) {
        let mut state = self.0.borrow_mut();
        state.endpoint.refuse();
        for key in state.accepted.take().into_iter().flatten() {
            state.sessions.remove(&key);
            state.close(key, Code(0));
        }
        state.wake();
    }
}

/// What a [`Carrier`], its sessions, and its task share.
struct State {
    endpoint: Endpoint,
    clock: Clock,
    /// The waker of the task's last poll, or `None` once the task ended.
    task: Option<Waker>,
    /// Each connection that a [`Session`] holds or that no caller accepted yet.
    sessions: BTreeMap<connection::Key, Slot>,
    /// The connections peers dialed, in the order they connected, for
    /// [`Carrier::accept`]. `None` once the carrier dropped.
    accepted: Option<VecDeque<connection::Key>>,
    /// The wakers of the [`Carrier::accept`] calls that wait.
    accepting: Vec<Waker>,
    /// What broke the socket.
    failed: Option<env::net::Error>,
    /// How many dials connected.
    connects: u64,
    /// The reads that wait for a block.
    waits: wait::Queue,
}

impl State {
    fn wake(&self) {
        if let Some(task) = &self.task {
            task.wake_by_ref();
        }
    }

    fn close(&mut self, key: connection::Key, code: Code) {
        let now = self.clock.now();
        self.endpoint.close(now, key, code);
        self.wake();
    }

    fn slot(&mut self, key: connection::Key) -> &mut Slot {
        self.sessions
            .get_mut(&key)
            .expect("invariant: a session keeps its slot until it drops")
    }

    fn dispatch(&mut self, event: Event) {
        match event {
            Event::Connected { key, peer } => {
                // Only an accept has no slot: a dial's drop ends its connection.
                if let Some(slot) = self.sessions.get_mut(&key) {
                    slot.peer = Some(peer);
                    slot.connected = self.connects;
                    self.connects += 1;
                    slot.wake_status();
                    return;
                }
                let Some(accepted) = &mut self.accepted else {
                    self.close(key, Code(0));
                    return;
                };
                accepted.push_back(key);
                let slot = Slot {
                    peer: Some(peer),
                    ..Slot::default()
                };
                self.sessions.insert(key, slot);
                self.accepting.drain(..).for_each(Waker::wake);
            }
            Event::Closed { key, error } => self.end(key, error),
            Event::Incoming { key } => {
                if let Some(slot) = self.sessions.get_mut(&key) {
                    slot.incoming.drain(..).for_each(Waker::wake);
                }
            }
            Event::Available { key } => {
                if let Some(slot) = self.sessions.get_mut(&key) {
                    slot.available.drain(..).for_each(Waker::wake);
                }
            }
            Event::Readable { stream } => {
                let slot = self.sessions.get_mut(&stream.connection);
                if let Some(waker) = slot.and_then(|s| s.reading.remove(&stream.id)) {
                    waker.wake();
                }
            }
            Event::Writable { stream } => {
                let slot = self.sessions.get_mut(&stream.connection);
                if let Some(waker) = slot.and_then(|s| s.writing.remove(&stream.id)) {
                    waker.wake();
                }
            }
            // No session takes datagrams yet.
            Event::Datagram { .. } => {}
        }
    }

    /// Gives `error` to the session of `key`, unless it dropped.
    fn end(&mut self, key: connection::Key, error: Error) {
        self.waits.end(self.clock.now(), key);
        if let Some(slot) = self.sessions.get_mut(&key) {
            slot.end = Some(error);
            slot.wake();
        }
    }

    /// Ends each session with [`Error::Network`], and refuses each later dial, and
    /// each accept once none that connected waits.
    fn fail(&mut self, error: env::net::Error) {
        self.endpoint.fail(&error);
        while let Some(event) = self.endpoint.poll() {
            self.dispatch(event);
        }
        self.accepting.drain(..).for_each(Waker::wake);
        self.failed = Some(error);
    }
}

/// The part of [`State`] for one connection.
#[derive(Default)]
struct Slot {
    /// The peer, once the handshake finishes.
    peer: Option<Peer>,
    /// How many dials connected before this one, once `peer` is set.
    connected: u64,
    /// Why the connection ended.
    end: Option<Error>,
    /// The wakers of the calls that wait for the handshake or the end.
    status: Vec<Waker>,
    /// The wakers of the opens that wait for the peer to allow a stream.
    available: Vec<Waker>,
    /// The wakers of the accepts that wait for a stream.
    incoming: Vec<Waker>,
    /// The waker of the read that waits on each stream.
    reading: Map<StreamId, Waker>,
    /// The waker of the write that waits on each stream.
    writing: Map<StreamId, Waker>,
}

impl Slot {
    fn wake_status(&mut self) {
        self.status.drain(..).for_each(Waker::wake);
    }

    /// Wakes each call that waits on the session, for its end.
    fn wake(&mut self) {
        self.wake_status();
        self.available.drain(..).for_each(Waker::wake);
        self.incoming.drain(..).for_each(Waker::wake);
        self.reading.drain().for_each(|(_, waker)| waker.wake());
        self.writing.drain().for_each(|(_, waker)| waker.wake());
    }
}

/// A connection to one peer on a [`Carrier`]. Dropping it closes the connection
/// with code 0.
pub(crate) struct Session {
    state: Rc<RefCell<State>>,
    key: connection::Key,
}

impl Session {
    /// The peer: a node that proved its key, or a client with no key.
    pub(crate) fn peer(&self) -> Peer {
        let mut state = self.state.borrow_mut();
        state
            .slot(self.key)
            .peer
            .expect("invariant: a session has connected")
    }

    /// Closes the session with `code`, which the peer gets as
    /// [`Error::PeerClosed`]. Does nothing when the session ended.
    pub(crate) fn close(&self, code: Code) {
        self.state.borrow_mut().close(self.key, code);
    }

    /// Waits until the session ends, and gives why.
    pub(crate) async fn closed(&self) -> Error {
        poll_fn(|cx| {
            let mut state = self.state.borrow_mut();
            let slot = state.slot(self.key);
            if let Some(error) = &slot.end {
                return Poll::Ready(error.clone());
            }
            register(&mut slot.status, cx.waker());
            Poll::Pending
        })
        .await
    }

    /// Ready with a stream of `class` that goes both ways, once the peer allows one,
    /// or with why the session ended.
    pub(crate) fn poll_open(
        &self,
        cx: &mut Context<'_>,
        class: Class,
    ) -> Poll<Result<(Sender, Receiver), Error>> {
        self.poll_queue(
            cx,
            |slot| &mut slot.available,
            |endpoint, clock, key| endpoint.open(clock.now(), key, class),
        )
    }

    /// As [`Session::poll_open`], for a stream that only this side sends on.
    pub(crate) fn poll_open_sender(
        &self,
        cx: &mut Context<'_>,
        class: Class,
    ) -> Poll<Result<Sender, Error>> {
        self.poll_queue(
            cx,
            |slot| &mut slot.available,
            |endpoint, clock, key| endpoint.open_sender(clock.now(), key, class),
        )
    }

    /// Ready with the next stream the peer opened, highest class first, or with why
    /// the session ended.
    pub(crate) fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Incoming, Error>> {
        self.poll_queue(
            cx,
            |slot| &mut slot.incoming,
            |endpoint, _, key| endpoint.accept(key),
        )
    }

    /// Writes `message` when it is `Some`, and takes it. Ready once the stream holds
    /// no message: it took all of `message`, or with `None`, all of the one before.
    /// Ready with the error.
    ///
    /// # Errors
    ///
    /// As [`Endpoint::write`].
    ///
    /// # Panics
    ///
    /// After a [`Session::finish`] that gave `Ok`, or a [`Session::reset`].
    pub(crate) fn poll_write(
        &self,
        cx: &mut Context<'_>,
        sender: &Sender,
        message: &mut Option<Block>,
    ) -> Poll<Result<(), Error>> {
        self.with(|endpoint, clock, slot, _| {
            match endpoint.write(clock.now(), sender, message) {
                Ok(Poll::Pending) => {
                    register_one(&mut slot.writing, sender.key().id, cx.waker());
                    Poll::Pending
                }
                Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
                Err(error) => Poll::Ready(Err(error)),
            }
        })
    }

    /// Ends `sender`'s stream after the messages written to it.
    ///
    /// # Errors
    ///
    /// As [`Endpoint::finish`].
    ///
    /// # Panics
    ///
    /// After a [`Session::finish`] that gave `Ok`, or a [`Session::reset`].
    pub(crate) fn finish(&self, sender: &mut Sender) -> Result<(), Error> {
        self.with(|endpoint, clock, _, _| endpoint.finish(clock.now(), sender))
    }

    /// Puts `message` on `sender`'s stream when the stream can take it now, as
    /// [`Endpoint::try_write`] does. Else gives it back.
    ///
    /// # Errors
    ///
    /// As [`Endpoint::try_write`].
    ///
    /// # Panics
    ///
    /// After a [`Session::finish`] that gave `Ok`, or a [`Session::reset`].
    pub(crate) fn try_write(
        &self,
        sender: &Sender,
        message: Block,
    ) -> Result<Option<Block>, Error> {
        self.with(|endpoint, clock, _, _| {
            endpoint.try_write(clock.now(), sender, message)
        })
    }

    /// Resets `sender`'s stream with `code`, as [`Endpoint::reset`] does.
    pub(crate) fn reset(&self, sender: &mut Sender, code: Code) {
        self.with(|endpoint, clock, slot, _| {
            slot.writing.remove(&sender.key().id);
            endpoint.reset(clock.now(), sender, code);
        });
    }

    /// Ends the last [`Session::poll_write`] on `sender`'s stream: drops its waker,
    /// and when `taken`, cancels the message the stream took from it, as
    /// [`Endpoint::cancel`] does.
    pub(crate) fn abandon(&self, sender: &Sender, taken: bool) {
        self.with(|endpoint, clock, slot, _| {
            slot.writing.remove(&sender.key().id);
            if taken {
                endpoint.cancel(clock.now(), sender);
            }
        });
    }

    /// Ready with the next whole message of `receiver`'s stream, `None` after the
    /// last, or with the error. It takes the message's block through the carrier's
    /// [`wait::Queue`], and waits there while it has no block.
    ///
    /// # Errors
    ///
    /// As [`Endpoint::read`].
    pub(crate) fn poll_read(
        &self,
        cx: &mut Context<'_>,
        receiver: &mut Receiver,
    ) -> Poll<Result<Option<Block>, Error>> {
        self.with(|endpoint, clock, slot, waits| {
            let (now, stream, class) = (clock.now(), receiver.key(), receiver.class());
            let take =
                |pool: &_, len| waits.take(now, pool, stream, class, len, cx.waker());
            let read = match endpoint.read(now, receiver, take) {
                Ok(Poll::Pending) => {
                    register_one(&mut slot.reading, stream.id, cx.waker());
                    return Poll::Pending;
                }
                Ok(Poll::Ready(message)) => Ok(message),
                Err(error) => Err(error),
            };
            waits.leave(now, stream);
            Poll::Ready(read)
        })
    }

    /// Ends a read of `receiver` that waits, as [`Endpoint::end_wait`] does, and its
    /// wait for a block.
    pub(crate) fn end_wait(&self, receiver: &mut Receiver) {
        self.with(|endpoint, clock, slot, waits| {
            slot.reading.remove(&receiver.key().id);
            waits.leave(clock.now(), receiver.key());
            endpoint.end_wait(receiver);
        });
    }

    /// Stops `receiver`'s stream with `code`, as [`Endpoint::stop`] does.
    pub(crate) fn stop(&self, receiver: Receiver, code: Code) {
        self.with(|endpoint, clock, slot, waits| {
            slot.reading.remove(&receiver.key().id);
            waits.leave(clock.now(), receiver.key());
            endpoint.stop(clock.now(), receiver, code);
        });
    }

    /// Ready with what `take` gives, or with why the session ended. Else registers
    /// the waker in the list that `wakers` picks.
    fn poll_queue<T>(
        &self,
        cx: &mut Context<'_>,
        wakers: fn(&mut Slot) -> &mut Vec<Waker>,
        take: impl FnOnce(&mut Endpoint, &Clock, connection::Key) -> Option<T>,
    ) -> Poll<Result<T, Error>> {
        self.with(|endpoint, clock, slot, _| {
            if let Some(error) = &slot.end {
                return Poll::Ready(Err(error.clone()));
            }
            if let Some(taken) = take(endpoint, clock, self.key) {
                return Poll::Ready(Ok(taken));
            }
            register(wakers(slot), cx.waker());
            Poll::Pending
        })
    }

    /// Opens a one-way stream that skips the stream layer, and writes `bytes` on it.
    #[cfg(test)]
    pub(crate) fn raw(&self, bytes: &[u8]) {
        let key = self.key;
        self.with(|endpoint, _, _, _| {
            let connection = super::pair::connection(endpoint, key);
            let id = connection.streams().open(noq_proto::Dir::Uni);
            let mut send = connection.send_stream(id.expect("a stream"));
            assert_eq!(send.write(bytes), Ok(bytes.len()));
        });
    }

    /// Runs `call` on the endpoint, the clock, the session's slot, and the reads
    /// that wait for a block, then wakes the task to send what the call queued.
    fn with<T>(
        &self,
        call: impl FnOnce(&mut Endpoint, &Clock, &mut Slot, &mut wait::Queue) -> T,
    ) -> T {
        let mut state = self.state.borrow_mut();
        let State {
            endpoint,
            clock,
            task,
            sessions,
            waits,
            ..
        } = &mut *state;
        let slot = sessions
            .get_mut(&self.key)
            .expect("invariant: a session keeps its slot until it drops");
        let called = call(endpoint, clock, slot, waits);
        if let Some(task) = task {
            task.wake_by_ref();
        }
        called
    }

    /// Ready when the handshake finished, with how many dials on the carrier
    /// connected before this one, or with why the dial ended.
    pub(crate) fn poll_connected(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<u64, Error>> {
        let mut state = self.state.borrow_mut();
        let slot = state.slot(self.key);
        if slot.peer.is_some() {
            return Poll::Ready(Ok(slot.connected));
        }
        if let Some(error) = &slot.end {
            return Poll::Ready(Err(error.clone()));
        }
        register(&mut slot.status, cx.waker());
        Poll::Pending
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        let slot = state.sessions.remove(&self.key);
        let slot = slot.expect("invariant: a session keeps its slot until it drops");
        if slot.end.is_none() {
            state.close(self.key, Code(0));
        }
    }
}

/// Moves the endpoint's datagrams between it and the socket, and runs its timers.
struct Task {
    state: Rc<RefCell<State>>,
    socket: Socket,
    /// Completes at the endpoint's deadline as of the last poll that changed it.
    sleep: Sleep,
    retry: Retry,
}

/// The timer that wakes the first read that waits for a block once each [`RETRY`].
struct Retry {
    sleep: Sleep,
    /// Whether `sleep` is armed for the reads that wait now.
    armed: bool,
}

impl Task {
    fn new(
        state: Rc<RefCell<State>>,
        clock: &Clock,
        sender: udp::Sender,
        receiver: udp::Receiver,
    ) -> Self {
        Self {
            state,
            socket: Socket::new(sender, receiver),
            sleep: clock.sleep_until(Monotonic(0)),
            retry: Retry {
                sleep: clock.sleep_until(Monotonic(0)),
                armed: false,
            },
        }
    }

    async fn run(mut self) {
        poll_fn(|cx| self.poll(cx)).await;
    }

    /// Ready when the socket broke, or when the carrier dropped, each connection
    /// drained, and the socket holds no datagram.
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.state.borrow_mut();
        let fresh =
            !(state.task.as_ref()).is_some_and(|task| task.will_wake(cx.waker()));
        if fresh {
            state.task = Some(cx.waker().clone());
        }
        let now = state.clock.now();
        let mut more = match self.socket.receive(cx, &mut state.endpoint, now) {
            Ok(more) => more,
            Err(error) => {
                state.fail(error);
                state.task = None;
                return Poll::Ready(());
            }
        };
        if self.sleep.deadline() <= now {
            state.endpoint.timeout(now);
        }
        self.socket.send(cx, &mut state.endpoint, now);
        while let Some(event) = state.endpoint.poll() {
            state.dispatch(event);
        }
        // Once the carrier dropped, no connection starts again.
        if state.accepted.is_none()
            && state.endpoint.drained()
            && self.socket.held.is_none()
            && !more
        {
            state.task = None;
            return Poll::Ready(());
        }
        // Each poll of the sleep arms the timer again.
        if let Some(deadline) = state.endpoint.deadline()
            && (fresh || deadline != self.sleep.deadline())
        {
            self.sleep.reset(deadline);
            more |= Pin::new(&mut self.sleep).poll(cx).is_ready();
        }
        more |= self.retry.poll(cx, &state.waits, now, fresh);
        if more {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

impl Retry {
    /// Wakes the first read of `waits` when the timer is due, and arms it while
    /// reads wait. Returns whether it is due again at once.
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        waits: &wait::Queue,
        now: Monotonic,
        fresh: bool,
    ) -> bool {
        if !waits.waiting() {
            self.armed = false;
            return false;
        }
        if self.armed && self.sleep.deadline() <= now {
            waits.wake_first();
            self.armed = false;
        }
        if self.armed && !fresh {
            return false;
        }
        if !self.armed {
            self.sleep.reset(now + RETRY);
            self.armed = true;
        }
        // Each poll of the sleep arms the timer again.
        Pin::new(&mut self.sleep).poll(cx).is_ready()
    }
}

/// The task's UDP socket and its buffers.
struct Socket {
    sender: udp::Sender,
    receiver: udp::Receiver,
    /// One buffer for each batch of a receive.
    received: [Vec<u8>; BATCHES],
    metas: [Meta; BATCHES],
    /// The bytes of the last transmit.
    out: Vec<u8>,
    /// The transmit in `out` that the socket had no room for.
    held: Option<Held>,
}

impl Socket {
    fn new(sender: udp::Sender, receiver: udp::Receiver) -> Self {
        let bytes = usize::from(PAYLOAD_IPV4) * receiver.batch_max().get();
        Self {
            sender,
            receiver,
            received: std::array::from_fn(|_| vec![0; bytes]),
            metas: std::array::from_fn(|_| Meta::default()),
            out: Vec::new(),
            held: None,
        }
    }

    /// Gives `endpoint` what the socket has, up to [`BATCHES`] batches. `Ok(true)`
    /// when it took that many, so the socket may have more.
    fn receive(
        &mut self,
        cx: &mut Context<'_>,
        endpoint: &mut Endpoint,
        now: Monotonic,
    ) -> Result<bool, env::net::Error> {
        let mut taken = 0;
        while taken < BATCHES {
            let room = BATCHES - taken;
            let mut buffers = self.received.each_mut().map(|b| IoSliceMut::new(b));
            let (buffers, metas) = (&mut buffers[..room], &mut self.metas[..room]);
            let batches = match self.receiver.poll_recv(cx, buffers, metas) {
                Poll::Ready(Ok(batches)) => batches,
                Poll::Ready(Err(error)) => return Err(error),
                Poll::Pending => return Ok(false),
            };
            let received = self.metas.iter().zip(&self.received);
            for (meta, batch) in received.take(batches) {
                endpoint.receive(now, meta, batch);
            }
            taken += batches;
        }
        Ok(true)
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

/// A [`Transmit`] without its bytes, which wait in [`Socket::out`].
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

/// Makes `waker` the one waker of `stream` in `wakers`.
fn register_one(wakers: &mut Map<StreamId, Waker>, stream: StreamId, waker: &Waker) {
    let held = wakers.entry(stream).or_insert_with(|| waker.clone());
    if !held.will_wake(waker) {
        held.clone_from(waker);
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::net::SocketAddr;
    use std::pin::pin;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use block::{Config, Heap, Pool};
    use env::net::udp::{self, Transmit};
    use noq_proto::{ConnectionHandle, Dir, Side, StreamId};
    use sim::Sim;
    use sim::node::Node;
    use types::ed25519::PrivateKey;
    use types::time::{Monotonic, Span};

    use super::{BATCHES, Carrier, Retry, Socket, register};
    use crate::quic::{Endpoint, connection, stream, wait};
    use crate::testing::{self, IDLE, PORT, address, nodes, poll_once, shard, spans};
    use crate::{Class, Code, Error, Peer};

    const CLIENT: PrivateKey = PrivateKey([1; 32]);
    const SERVER: PrivateKey = PrivateKey([2; 32]);

    fn socket(node: &Node) -> Result<(udp::Sender, udp::Receiver), env::net::Error> {
        node.net().udp(&udp::Config {
            local: address(node),
            send_buffer_bytes: 1 << 20,
            recv_buffer_bytes: 1 << 20,
        })
    }

    /// Runs a dial from `value` that the client closes with code 5, and gives the
    /// digest of the run.
    fn dial(value: u64) -> u64 {
        let (mut sim, client, server) = nodes(value);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            assert_eq!(session.peer(), Peer::Node(CLIENT.public()));
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            assert_eq!(session.peer(), Peer::Node(SERVER.public()));
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
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
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
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            assert_eq!(session.closed().await, Error::TimedOut);
        });
        testing::carrier(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            assert_eq!(session.closed().await, Error::TimedOut);
        });
        assert_eq!(sim.run_for(spans(Span::MILLISECOND, 500)), Ok(()));
        link(&mut sim, &client, &server, cut());
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_to_no_socket_times_out() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            assert_eq!(dialed.err(), Some(Error::TimedOut));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn accept_gives_a_session_that_ended_before_it() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, node| async move {
            node.clock().sleep(IDLE).await;
            let session = carrier.accept().await.expect("a session");
            assert_eq!(session.peer(), Peer::Node(CLIENT.public()));
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dropped_session_closes_with_code_0() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            drop(dialed.expect("a session"));
            // The shard drops the task, and the close with it, when this ends.
            node.clock().sleep(Span::MILLISECOND).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_dropped_in_its_handshake_never_shows_at_accept() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, node| async move {
            node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
            drop(carrier);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            {
                let mut dial = pin!(carrier.connect(SERVER.public(), at));
                assert!(poll_once(dial.as_mut()).await.is_none());
            }
            node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
            assert!(poll_once(pin!(carrier.accept())).await.is_none());
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_session_dropped_with_the_last_carrier_closes_with_code_0() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            drop(dialed.expect("a session"));
            drop(carrier);
            // A shard that ends drops its tasks.
            node.clock().sleep(Span::MILLISECOND).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    /// Links `a` and `b` both ways with `config`.
    fn link(sim: &mut Sim, a: &Node, b: &Node, config: sim::link::Config) {
        sim.link(a, b, config);
        sim.link(b, a, config);
    }

    /// A link with a one-way delay of 50 ms.
    fn slow() -> sim::link::Config {
        sim::link::Config {
            delay: spans(Span::MILLISECOND, 50),
            ..sim::link::Config::default()
        }
    }

    fn cut() -> sim::link::Config {
        sim::link::Config {
            loss: 1.0,
            ..sim::link::Config::default()
        }
    }

    #[test]
    fn the_last_drop_on_a_slow_link_sends_the_close() {
        let (mut sim, client, server) = nodes(0);
        link(&mut sim, &client, &server, slow());
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            drop(dialed.expect("a session"));
            drop(carrier);
            node.clock().sleep(spans(IDLE, 3)).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_after_the_last_drop_is_refused() {
        let (mut sim, client, server) = nodes(0);
        let late = sim.node(sim::node::Config::default());
        // The server's close to `client` drains for hundreds of milliseconds.
        link(&mut sim, &client, &server, slow());
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, node| async move {
            drop(carrier.accept().await.expect("a session"));
            drop(carrier);
            node.clock().sleep(spans(IDLE, 3)).await;
        });
        testing::carrier(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&late, CLIENT, move |carrier, node| async move {
            node.clock().sleep(spans(Span::MILLISECOND, 300)).await;
            let dialed = carrier.connect(SERVER.public(), at).await;
            let reason =
                "aborted by peer: the server refused to accept a new connection";
            let reason = String::from(reason);
            assert_eq!(dialed.err(), Some(Error::Broken { reason }));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_handshake_that_finishes_after_the_carrier_drops_closes_with_code_0() {
        let (mut sim, client, server) = nodes(0);
        let late = sim.node(sim::node::Config::default());
        link(&mut sim, &late, &server, slow());
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, node| async move {
            let session = carrier.accept().await.expect("a session");
            // The handshake of `late` starts at 150 ms and finishes at 250 ms.
            node.clock().sleep(spans(Span::MILLISECOND, 200)).await;
            drop(carrier);
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            node.clock().sleep(spans(IDLE, 3)).await;
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        testing::carrier(&late, CLIENT, move |carrier, node| async move {
            node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
            let before = node.clock().now();
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(session.closed().await, closed);
            assert!(node.clock().now() - before < IDLE);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_timer_due_in_a_pause_runs_when_it_ends() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            assert_eq!(session.closed().await, Error::TimedOut);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            node.clock().sleep(spans(Span::MILLISECOND, 50)).await;
            drop(session);
            drop(carrier);
            // The task sends the close and arms the drain timer, which falls due in
            // the pause, before the keep-alive.
            node.clock().sleep(Span::MILLISECOND).await;
            let local = address(&node);
            let held = Some(env::net::Error::AddressInUse { local });
            assert_eq!(socket(&node).err(), held);
            node.pause(spans(Span::MILLISECOND, 150));
            node.clock().sleep(spans(Span::MILLISECOND, 151)).await;
            assert_eq!(socket(&node).err(), None);
        });
        assert_eq!(sim.run_for(spans(Span::MILLISECOND, 40)), Ok(()));
        // Only the timer can wake the client's task.
        link(&mut sim, &client, &server, cut());
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn register_keeps_one_waker_for_each_task() {
        struct Count(AtomicUsize);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let one = Arc::new(Count(AtomicUsize::new(0)));
        let other = Arc::new(Count(AtomicUsize::new(0)));
        let mut wakers = Vec::new();
        for count in [&one, &one, &other] {
            register(&mut wakers, &Waker::from(Arc::clone(count)));
        }
        wakers.into_iter().for_each(Waker::wake);
        let counts = [&one, &other].map(|count| count.0.load(Ordering::Relaxed));
        assert_eq!(counts, [1, 1]);
    }

    #[test]
    fn the_retry_wakes_the_first_read_each_10_ms_with_the_last_task_waker() {
        struct Count(AtomicUsize);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let counts: [_; 3] =
            std::array::from_fn(|_| Arc::new(Count(AtomicUsize::new(0))));
        let [read, old, new] = counts
            .each_ref()
            .map(|count| Waker::from(Arc::clone(count)));
        let woken = move || {
            counts
                .each_ref()
                .map(|count| count.0.load(Ordering::Relaxed))
        };
        let (mut sim, client, _) = nodes(0);
        shard(&client, CLIENT, |config, _| async move {
            let clock = config.clock;
            let start = clock.now();
            let mut retry = Retry {
                sleep: clock.sleep_until(Monotonic(0)),
                armed: false,
            };
            let mut waits = wait::Queue::default();
            let key = stream::Key {
                connection: connection::Key {
                    handle: ConnectionHandle(0),
                    serial: 0,
                },
                id: StreamId::new(Side::Client, Dir::Uni, 0),
            };
            let config = Config { budget: 300 };
            let memory = Heap::new(config.reservation());
            let pool = Pool::new(config, memory);
            // A 100-byte block takes 192 bytes of the budget.
            let held = pool.alloc(100).expect("room");
            let taken = waits.take(start, &pool, key, Class::Complete, 100, &read);
            assert!(taken.is_none());
            for task in [&old, &new] {
                let mut cx = Context::from_waker(task);
                assert!(!retry.poll(&mut cx, &waits, start, true));
            }
            clock.sleep(spans(Span::MILLISECOND, 9)).await;
            assert_eq!(woken(), [0, 0, 0]);
            clock.sleep(spans(Span::MILLISECOND, 2)).await;
            assert_eq!(woken(), [0, 0, 1]);
            let mut cx = Context::from_waker(&new);
            assert!(!retry.poll(&mut cx, &waits, clock.now(), false));
            assert_eq!(woken(), [1, 0, 1]);
            clock.sleep(spans(Span::MILLISECOND, 10)).await;
            assert_eq!(woken(), [1, 0, 2]);
            drop(held);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_carrier_drop_closes_each_session_no_caller_accepted() {
        let (mut sim, client, server) = nodes(0);
        let held = sim.node(sim::node::Config::default());
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, node| async move {
            let session = carrier.accept().await.expect("a session");
            node.clock().sleep(spans(Span::MILLISECOND, 5)).await;
            drop(carrier);
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&held, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            node.clock().sleep(spans(Span::MILLISECOND, 10)).await;
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            node.clock().sleep(spans(Span::MILLISECOND, 2)).await;
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(session.closed().await, closed);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    /// Sends `count` datagrams of zeros to `to` from the port after [`PORT`] on
    /// `node`, each longer than the last, so no two join in one batch.
    async fn junk(node: &Node, to: SocketAddr, count: usize) {
        let local = SocketAddr::new(node.addresses()[0], PORT + 1);
        let config = udp::Config {
            local,
            send_buffer_bytes: 1 << 20,
            recv_buffer_bytes: 1 << 20,
        };
        let (mut sender, _receiver) = node.net().udp(&config).expect("a socket");
        for len in 1..=count {
            let transmit = Transmit {
                destination: to,
                source: None,
                ecn: None,
                contents: &vec![0; len],
                segment: None,
            };
            let sent = poll_fn(|cx| sender.poll_send(cx, &transmit)).await;
            assert_eq!(sent, Ok(()));
        }
    }

    #[test]
    fn a_receive_stops_at_the_batch_limit() {
        let (mut sim, client, server) = nodes(0);
        let to = address(&client);
        shard(&server, SERVER, move |_, node| async move {
            junk(&node, to, 2 * BATCHES).await;
        });
        shard(&client, CLIENT, |config, node| async move {
            let (sender, receiver) = socket(&node).expect("a socket");
            let mut endpoint =
                Endpoint::new(&testing::setup(&config), 0, sender.batch_max());
            let mut socket = Socket::new(sender, receiver);
            node.clock().sleep(Span::MILLISECOND).await;
            let now = node.clock().now();
            let mut taken = Vec::new();
            for _ in 0..3 {
                let receive = |cx: &mut Context<'_>| {
                    Poll::Ready(socket.receive(cx, &mut endpoint, now))
                };
                taken.push(poll_fn(receive).await);
            }
            assert_eq!(taken, [Ok(true), Ok(true), Ok(false)]);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_behind_more_batches_than_one_poll_takes_connects_at_once() {
        let (mut sim, client, server) = nodes(0);
        let (at, to) = (address(&server), address(&client));
        testing::carrier(&server, SERVER, move |carrier, node| async move {
            junk(&node, to, BATCHES).await;
            let session = carrier.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        shard(&client, CLIENT, move |config, node| async move {
            let part = testing::part(&node.net(), address(&node));
            node.clock().sleep(Span::MILLISECOND).await;
            let carrier = Carrier::new(testing::setup(&config), part);
            let before = node.clock().now();
            let dialed = carrier.connect(SERVER.public(), at).await;
            // A lost first datagram goes again only after hundreds of milliseconds.
            assert!(node.clock().now() - before < spans(Span::MILLISECOND, 10));
            let session = dialed.expect("a session");
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn the_last_drop_frees_the_socket() {
        let (mut sim, client, _) = nodes(0);
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
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
    fn a_session_held_after_the_drain_does_not_hold_the_socket() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, node| async move {
            let session = carrier.accept().await.expect("a session");
            drop(carrier);
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
            node.clock().sleep(spans(IDLE, 3)).await;
            assert_eq!(socket(&node).err(), None);
            drop(session);
        });
        testing::carrier(&client, CLIENT, move |carrier, _| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn each_stream_call_after_the_drain_gives_the_end() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        shard(&client, CLIENT, move |config, node| async move {
            let pool = Rc::clone(&config.pool);
            let part = testing::part(&node.net(), address(&node));
            let carrier = Carrier::new(testing::setup(&config), part);
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            let open = poll_fn(|cx| session.poll_open(cx, Class::Complete)).await;
            let (sender, mut receiver) = open.expect("a stream");
            let open = poll_fn(|cx| session.poll_open_sender(cx, Class::Latest)).await;
            let mut finishing = open.expect("a stream");
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
            node.clock().sleep(spans(IDLE, 3)).await;
            // A private read: no public call shows the drain.
            assert!(session.state.borrow().endpoint.drained());
            let block = || testing::block(&pool, b"a");
            let mut message = Some(block());
            let written = poll_fn(|cx| session.poll_write(cx, &sender, &mut message));
            assert_eq!(written.await, Err(closed.clone()));
            assert!(message.is_some());
            let flushed = poll_fn(|cx| session.poll_write(cx, &sender, &mut None));
            assert_eq!(flushed.await, Err(closed.clone()));
            let given = session.try_write(&sender, block());
            assert_eq!(given.map(|given| given.is_some()), Err(closed.clone()));
            assert_eq!(session.finish(&mut finishing), Err(closed.clone()));
            let read = poll_fn(|cx| session.poll_read(cx, &mut receiver)).await;
            assert_eq!(read.map(|read| read.is_some()), Err(closed));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_that_arrives_before_the_drain_behind_junk_is_refused() {
        let (mut sim, client, server) = nodes(0);
        let late = sim.node(sim::node::Config::default());
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, node| async move {
            let session = carrier.accept().await.expect("a session");
            drop(carrier);
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
            let now = node.clock().now();
            // A private read: no public call gives the drain deadline.
            let drain = session.state.borrow().endpoint.deadline().expect("a drain");
            // The dial of `late` arrives at about 41 ms, before the drain ends.
            assert!(drain - now > spans(Span::MILLISECOND, 30));
            node.pause(spans(Span::MILLISECOND, 300));
            node.clock().sleep(spans(IDLE, 3)).await;
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            node.clock().sleep(spans(Span::MILLISECOND, 10)).await;
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
        testing::carrier(&late, CLIENT, move |carrier, node| async move {
            node.clock().sleep(spans(Span::MILLISECOND, 40)).await;
            junk(&node, at, BATCHES).await;
            let dialed = carrier.connect(SERVER.public(), at).await;
            let reason =
                "aborted by peer: the server refused to accept a new connection";
            let reason = String::from(reason);
            assert_eq!(dialed.err(), Some(Error::Broken { reason }));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_broken_socket_ends_each_session_and_refuses_new_ones() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            assert_eq!(session.closed().await, Error::TimedOut);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            node.fail_udp(address(&node));
            let network = Error::Network {
                error: env::net::Error::Io { code: 5 },
            };
            assert_eq!(session.closed().await, network);
            let dialed = carrier.connect(SERVER.public(), at).await;
            assert_eq!(dialed.err(), Some(network.clone()));
            assert_eq!(carrier.accept().await.err(), Some(network));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_broken_socket_ends_a_waiting_accept() {
        let (mut sim, client, _) = nodes(0);
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let mut accepting = pin!(carrier.accept());
            assert!(poll_once(accepting.as_mut()).await.is_none());
            node.fail_udp(address(&node));
            let network = Error::Network {
                error: env::net::Error::Io { code: 5 },
            };
            assert_eq!(accepting.await.err(), Some(network));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_broken_socket_keeps_the_end_of_a_session_that_ended_before() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let session = carrier.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        testing::carrier(&client, CLIENT, move |carrier, node| async move {
            let dialed = carrier.connect(SERVER.public(), at).await;
            let session = dialed.expect("a session");
            session.close(Code(5));
            let closed = Error::Closed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
            node.fail_udp(address(&node));
            let network = Error::Network {
                error: env::net::Error::Io { code: 5 },
            };
            assert_eq!(carrier.accept().await.err(), Some(network));
            assert_eq!(session.closed().await, closed);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_close_the_socket_held_goes_after_the_connections_drain() {
        let (mut sim, client, server) = nodes(0);
        let at = address(&server);
        testing::carrier(&server, SERVER, |carrier, _| async move {
            let sessions = [(); 2].map(|()| carrier.accept());
            for accepted in sessions {
                let session = accepted.await.expect("a session");
                let closed = Error::PeerClosed { code: Code(0) };
                assert_eq!(session.closed().await, closed);
            }
        });
        shard(&client, CLIENT, move |config, node| async move {
            // One datagram fills the send buffer.
            let (sender, receiver) = node
                .net()
                .udp(&udp::Config {
                    local: address(&node),
                    send_buffer_bytes: 1,
                    recv_buffer_bytes: 1 << 20,
                })
                .expect("a socket");
            let part = crate::port::Part {
                index: 0,
                sender,
                receiver,
            };
            let carrier = Carrier::new(testing::setup(&config), part);
            let first = carrier.connect(SERVER.public(), at).await;
            let second = carrier.connect(SERVER.public(), at).await;
            node.clock().sleep(spans(Span::MILLISECOND, 100)).await;
            // About 150 ms of the link: longer than the drain, shorter than IDLE.
            junk(&node, SocketAddr::new(at.ip(), PORT + 1), 150).await;
            // The first close waits behind the junk, and the socket holds the second.
            drop(first.expect("a session"));
            drop(second.expect("a session"));
            drop(carrier);
            node.clock().sleep(spans(IDLE, 3)).await;
        });
        assert_eq!(sim.run_for(spans(Span::MILLISECOND, 50)), Ok(()));
        let rate = sim::link::Config {
            rate: std::num::NonZeroU64::new(100_000),
            ..sim::link::Config::default()
        };
        sim.link(&client, &server, rate);
        assert_eq!(sim.run(), Ok(()));
    }
}
