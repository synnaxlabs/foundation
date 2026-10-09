//! One connection of an [`Endpoint`](super::Endpoint), and what its events mean to
//! the caller.

use std::collections::VecDeque;
use std::mem;
use std::time::{Duration, Instant};

use block::Pool;
use bytes::Bytes;
use noq_proto::crypto::rustls::HandshakeData;
use noq_proto::{ConnectionError, ConnectionHandle, VarInt};
use rustls::pki_types::CertificateDer;
use types::ed25519::PublicKey;

use super::Event;
use super::datagram::Received;
use super::stream::Streams;
use crate::{Code, Error, Peer, tls};

/// Names one connection of an [`Endpoint`](super::Endpoint). No other connection of
/// that endpoint gets the same key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Key {
    /// noq-proto's handle, which it gives to a new connection after this one drains.
    pub(super) handle: ConnectionHandle,
    /// How many connections the endpoint made before this one.
    pub(super) serial: u64,
}

/// A fault of the peer's that closes the connection, with the reason.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "fuzzing"),
    expect(unreachable_pub, reason = "only the fuzzing feature exports it")
)]
pub struct Fault(pub(super) String);

/// One noq-proto connection, and what the caller knows of it.
pub(super) struct Connection {
    pub(super) key: Key,
    pub(super) inner: noq_proto::Connection,
    /// It is in the endpoint's queue of connections to poll for a datagram.
    pub(super) queued: bool,
    pub(super) streams: Streams,
    /// The datagrams that arrived and wait to be taken.
    pub(super) datagrams: Received,
    /// The number of the first packet sent after the last ping, until the peer
    /// acknowledges it or a later one.
    pinged: Option<u64>,
    state: State,
}

enum State {
    /// A dial that waits for the peer to prove `expected`.
    Dialing { expected: PublicKey },
    /// An accepted connection that the caller does not know yet.
    Accepting,
    /// The caller has the key.
    Open,
    /// The caller has its [`Event::Closed`], or never had the key.
    Ended,
}

impl Connection {
    /// A dial that expects `expected`.
    pub(super) fn dialed(
        key: Key,
        inner: noq_proto::Connection,
        expected: PublicKey,
        streams: Streams,
    ) -> Self {
        Self::new(key, inner, State::Dialing { expected }, streams)
    }

    /// A connection a peer dialed.
    pub(super) fn accepted(
        key: Key,
        inner: noq_proto::Connection,
        streams: Streams,
    ) -> Self {
        Self::new(key, inner, State::Accepting, streams)
    }

    fn new(
        key: Key,
        inner: noq_proto::Connection,
        state: State,
        streams: Streams,
    ) -> Self {
        Self {
            key,
            inner,
            queued: false,
            streams,
            datagrams: Received::default(),
            pinged: None,
            state,
        }
    }

    /// When [`Connection::timeout`] must next run, if ever.
    pub(super) fn deadline(&self) -> Option<Instant> {
        let streams = self.streams.deadline(|| self.idle());
        self.inner.poll_timeout().into_iter().chain(streams).min()
    }

    /// The idle timeout as noq-proto counts it, at least 3 PTO.
    fn idle(&self) -> Duration {
        let idle = self.inner.idle_timeout();
        idle.expect("invariant: a `Setup` sets an idle timeout")
    }

    /// Runs the timers due at `now`, and queues in `events` the [`Event::Closed`] of a
    /// fault that it finds. Gives whether one ran, so that the caller drives the
    /// connection. The idle timeout goes first: a wake past it and the hello's bound
    /// ends a silent peer with [`Error::TimedOut`].
    pub(super) fn timeout(
        &mut self,
        now: Instant,
        events: &mut VecDeque<Event>,
    ) -> bool {
        let ran = self.inner.poll_timeout().is_some_and(|due| due <= now);
        if ran {
            self.inner.handle_timeout(now);
        }
        if let Err(Fault(reason)) = self.streams.timeout(now, || self.idle())
            && !self.inner.is_closed()
        {
            events.extend(self.fault(now, reason));
            return true;
        }
        ran
    }

    /// Moves the connection's events to `endpoint` and to `events` at `now`, and
    /// each datagram that arrives into a block from `pool`. Returns whether it
    /// drained: `endpoint` forgot it, and nothing more happens to it.
    pub(super) fn drive(
        &mut self,
        now: Instant,
        endpoint: &mut noq_proto::Endpoint,
        pool: &Pool,
        events: &mut VecDeque<Event>,
    ) -> bool {
        let mut drained = false;
        loop {
            if let Some(event) = self.inner.poll_endpoint_events() {
                drained |= event.is_drained();
                if let Some(event) = endpoint.handle_event(self.key.handle, event) {
                    self.inner.handle_event(event);
                }
            } else if let Some(event) = self.inner.poll() {
                self.event(now, event, pool, events);
            } else {
                break;
            }
        }
        // After every event, so that each stop has reset its stream.
        if self.live() {
            self.streams.pump(&mut self.inner, events);
            if self
                .pinged
                .is_some_and(|mark| self.inner.largest_acked() >= Some(mark))
            {
                self.pinged = None;
                events.push_back(Event::Acked { key: self.key });
            }
        }
        assert!(
            !drained || !self.live(),
            "invariant: noq-proto ends a connection before it drains"
        );
        drained
    }

    /// Queues in `events` what `event` means to the caller, if anything.
    fn event(
        &mut self,
        now: Instant,
        event: noq_proto::Event,
        pool: &Pool,
        events: &mut VecDeque<Event>,
    ) {
        let key = self.key;
        match event {
            noq_proto::Event::Connected => {
                assert!(
                    matches!(self.state, State::Dialing { .. } | State::Accepting),
                    "invariant: noq-proto connects a connection once, before it ends"
                );
                if let Err(Fault(reason)) = self.streams.greet(&mut self.inner) {
                    events.extend(self.fault(now, reason));
                    return;
                }
                self.state = State::Open;
                self.streams.start(now);
                let peer = self.peer();
                events.push_back(Event::Connected { key, peer });
            }
            noq_proto::Event::ConnectionLost { reason } => {
                let expected = match self.end() {
                    State::Dialing { expected } => Some(expected),
                    State::Open => None,
                    State::Accepting | State::Ended => return,
                };
                events.push_back(self.closed(error(reason, expected)));
            }
            noq_proto::Event::Stream(event) if self.live() => {
                assert!(
                    self.connected(),
                    "invariant: noq-proto gives stream events only after it connects"
                );
                let streamed = self.streams.event(&mut self.inner, key, &event, events);
                if let Err(Fault(reason)) = streamed {
                    events.extend(self.fault(now, reason));
                }
            }
            // noq-proto gives the datagrams it queued before a fault of ours.
            noq_proto::Event::DatagramReceived if self.live() => {
                assert!(
                    self.connected(),
                    "invariant: noq-proto gives datagrams only on an open connection"
                );
                self.datagrams.pull(&mut self.inner, pool, key, events);
            }
            // An acceptor has the whole ClientHello here, so it knows the peer's
            // transport parameters and can send at 0.5-RTT, unless a HelloRetryRequest
            // holds them back until `Connected`.
            noq_proto::Event::HandshakeDataReady
                if matches!(self.state, State::Accepting) =>
            {
                if let Err(Fault(reason)) = self.streams.greet(&mut self.inner) {
                    events.extend(self.fault(now, reason));
                }
            }
            noq_proto::Event::HandshakeDataReady
            | noq_proto::Event::HandshakeConfirmed
            | noq_proto::Event::Stream(_)
            | noq_proto::Event::DatagramReceived
            | noq_proto::Event::DatagramsUnblocked
            | noq_proto::Event::Path(_)
            | noq_proto::Event::NatTraversal(_) => {}
        }
    }

    /// Sends a packet that the peer must acknowledge. [`Event::Acked`] comes once the
    /// peer acknowledges it or a later packet. A later ping moves that mark.
    pub(super) fn ping(&mut self) {
        self.pinged = Some(self.inner.next_packet_number());
        self.inner.ping();
    }

    /// Whether the connection has not ended.
    pub(super) fn live(&self) -> bool {
        !matches!(self.state, State::Ended)
    }

    /// Whether the handshake finished and the connection has not ended.
    pub(super) fn connected(&self) -> bool {
        matches!(self.state, State::Open)
    }

    /// Closes the connection on a fault of the peer's, with code 2^32 and `reason`,
    /// and gives its [`Event::Closed`] with [`Error::Broken`] when the caller has the
    /// key.
    ///
    /// # Panics
    ///
    /// When the connection ended.
    pub(super) fn fault(&mut self, now: Instant, reason: String) -> Option<Event> {
        let known = match self.end() {
            State::Dialing { .. } | State::Open => true,
            State::Accepting => false,
            State::Ended => {
                panic!("invariant: a fault is found on a live connection")
            }
        };
        let code = VarInt::from_u64(1 << 32).expect("invariant: 2^32 is a varint");
        self.inner.close(now, code, Bytes::from(reason.clone()));
        known.then(|| self.closed(Error::Broken { reason }))
    }

    /// Closes the connection with `code`, and gives its [`Event::Closed`] unless it
    /// already ended.
    ///
    /// # Panics
    ///
    /// When the caller does not have the key yet.
    pub(super) fn close(&mut self, now: Instant, code: Code) -> Option<Event> {
        match self.state {
            State::Dialing { .. } | State::Open => {
                self.end();
                self.inner
                    .close(now, VarInt::from_u32(code.0), Bytes::new());
                Some(self.closed(Error::Closed { code }))
            }
            State::Ended => None,
            State::Accepting => {
                panic!("invariant: the caller has no key for a connection it never got")
            }
        }
    }

    /// Ends a live connection after the socket broke, and gives its
    /// [`Event::Closed`] with [`Error::Network`] when the caller has the key.
    pub(super) fn fail(&mut self, error: &env::net::Error) -> Option<Event> {
        if !self.live() {
            return None;
        }
        let known = !matches!(self.end(), State::Accepting);
        known.then(|| {
            self.closed(Error::Network {
                error: error.clone(),
            })
        })
    }

    /// Ends the connection, frees the datagrams that no caller can take now, and
    /// gives the state it had.
    fn end(&mut self) -> State {
        self.datagrams = Received::default();
        mem::replace(&mut self.state, State::Ended)
    }

    /// Gives `error` to the stream calls, and gives the [`Event::Closed`] of the
    /// connection, which [`Connection::end`] ended.
    fn closed(&self, error: Error) -> Event {
        self.streams.end(error.clone());
        Event::Closed {
            key: self.key,
            error,
        }
    }

    /// The peer of a connected connection.
    fn peer(&self) -> Peer {
        let session = self.inner.crypto_session();
        let data = session
            .handshake_data()
            .expect("invariant: a connected session has handshake data");
        let data = data
            .downcast::<HandshakeData>()
            .expect("invariant: the session is rustls");
        let certificates = session.peer_identity().map(|identity| {
            *identity
                .downcast::<Vec<CertificateDer<'static>>>()
                .expect("invariant: the session is rustls")
        });
        tls::peer(data.protocol.as_deref(), certificates.as_deref())
            .expect("invariant: rustls refuses a QUIC handshake with no protocol")
    }
}

/// What `reason` means to the caller. `expected` is the key a dial still waits for.
fn error(reason: ConnectionError, expected: Option<PublicKey>) -> Error {
    if let (Some(expected), ConnectionError::TransportError(error)) =
        (expected, &reason)
        && let Some(rustls::Error::InvalidCertificate(_)) = error
            .crypto
            .as_deref()
            .and_then(|crypto| crypto.downcast_ref::<rustls::Error>())
    {
        return Error::Authentication { expected };
    }
    match reason {
        ConnectionError::TimedOut => Error::TimedOut,
        ConnectionError::ApplicationClosed(ref close) => {
            match u32::try_from(close.error_code.into_inner()) {
                Ok(code) => Error::PeerClosed { code: Code(code) },
                Err(_) => Error::Broken {
                    reason: reason.to_string(),
                },
            }
        }
        reason => Error::Broken {
            reason: reason.to_string(),
        },
    }
}
