//! One connection of an [`Endpoint`](super::Endpoint), and what its events mean to
//! the caller.

use std::collections::VecDeque;
use std::mem;
use std::time::Instant;

use block::Pool;
use bytes::Bytes;
use noq_proto::crypto::rustls::HandshakeData;
use noq_proto::{ConnectionError, ConnectionHandle, VarInt};
use rustls::pki_types::CertificateDer;
use types::node::PublicKey;

use super::Event;
use super::datagram::Received;
use super::stream::{Fault, Streams};
use crate::{Code, Error, Peer, tls};

/// Names one connection of an [`Endpoint`](super::Endpoint). No other connection of
/// that endpoint gets the same key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    /// noq-proto's handle, which it gives to a new connection after this one drains.
    pub(super) handle: ConnectionHandle,
    /// How many connections the endpoint made before this one.
    pub(super) serial: u64,
}

/// One noq-proto connection, and what the caller knows of it.
pub(super) struct Connection {
    pub(super) key: Key,
    pub(super) inner: noq_proto::Connection,
    /// It is in the endpoint's queue of connections to poll for a datagram.
    pub(super) queued: bool,
    pub(super) streams: Streams,
    /// The datagrams that arrived and wait to be taken.
    pub(super) datagrams: Received,
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
            state,
        }
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
        assert!(
            !drained || matches!(self.state, State::Ended),
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
                self.state = State::Open;
                let peer = self.peer();
                events.push_back(Event::Connected { key, peer });
            }
            noq_proto::Event::ConnectionLost { reason } => {
                let expected = match self.end() {
                    State::Dialing { expected } => Some(expected),
                    State::Open => None,
                    State::Accepting | State::Ended => return,
                };
                let error = error(reason, expected);
                events.push_back(Event::Closed { key, error });
            }
            noq_proto::Event::Stream(event) if self.live() => {
                assert!(
                    self.connected(),
                    "invariant: noq-proto gives stream events only after it connects"
                );
                // A stream event repeats once for each frame, so the same one in a
                // row merges.
                match self.streams.event(&mut self.inner, key, &event) {
                    Ok(Some(event)) if events.back() == Some(&event) => {}
                    Ok(event) => events.extend(event),
                    Err(Fault(reason)) => events.push_back(self.fault(now, reason)),
                }
            }
            noq_proto::Event::DatagramReceived => {
                // noq-proto gives it before stream events, and none after it closes,
                // so it never follows a fault or close of ours.
                assert!(
                    self.connected(),
                    "invariant: noq-proto gives datagrams only on an open connection"
                );
                self.datagrams.pull(&mut self.inner, pool, key, events);
            }
            noq_proto::Event::HandshakeDataReady
            | noq_proto::Event::HandshakeConfirmed
            | noq_proto::Event::Stream(_)
            | noq_proto::Event::DatagramsUnblocked
            | noq_proto::Event::Path(_)
            | noq_proto::Event::NatTraversal(_) => {}
        }
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
    /// and gives its [`Event::Closed`] with [`Error::Broken`].
    ///
    /// # Panics
    ///
    /// When the connection is not open.
    pub(super) fn fault(&mut self, now: Instant, reason: String) -> Event {
        let state = self.end();
        assert!(
            matches!(state, State::Open),
            "invariant: a fault is found on an open connection"
        );
        let code = VarInt::from_u64(1 << 32).expect("invariant: 2^32 is a varint");
        self.inner.close(now, code, Bytes::from(reason.clone()));
        Event::Closed {
            key: self.key,
            error: Error::Broken { reason },
        }
    }

    /// Closes the connection with `code`, and gives its [`Event::Closed`] unless it
    /// already ended.
    ///
    /// # Panics
    ///
    /// When the caller does not have the key yet.
    pub(super) fn close(&mut self, now: Instant, code: Code) -> Option<Event> {
        match self.end() {
            State::Dialing { .. } | State::Open => {
                self.inner
                    .close(now, VarInt::from_u32(code.0), Bytes::new());
                let error = Error::Closed { code };
                Some(Event::Closed {
                    key: self.key,
                    error,
                })
            }
            State::Ended => None,
            State::Accepting => {
                panic!("invariant: the caller has no key for a connection it never got")
            }
        }
    }

    /// Ends the connection, frees the datagrams that no caller can take now, and
    /// gives the state it had.
    fn end(&mut self) -> State {
        self.datagrams = Received::default();
        mem::replace(&mut self.state, State::Ended)
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
