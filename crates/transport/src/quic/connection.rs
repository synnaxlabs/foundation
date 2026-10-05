//! One connection of an [`Endpoint`](super::Endpoint), and what its events mean to
//! the caller.

use std::mem;
use std::time::Instant;

use bytes::Bytes;
use noq_proto::crypto::rustls::HandshakeData;
use noq_proto::{ConnectionError, VarInt};
use rustls::pki_types::CertificateDer;
use types::node::PublicKey;

use super::Event;
use crate::{Code, Error, Peer, tls};

/// Names one connection of an [`Endpoint`](super::Endpoint). No other connection of
/// that endpoint gets the same key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    /// The connection's slot, which is noq-proto's handle.
    pub(super) slot: usize,
    /// How many connections the endpoint made before this one.
    pub(super) serial: u64,
}

/// One noq-proto connection, and what the caller knows of it.
pub(super) struct Connection {
    pub(super) key: Key,
    pub(super) inner: noq_proto::Connection,
    /// It is in the endpoint's queue of connections to poll for a datagram.
    pub(super) queued: bool,
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
    ) -> Self {
        Self::new(key, inner, State::Dialing { expected })
    }

    /// A connection a peer dialed.
    pub(super) fn accepted(key: Key, inner: noq_proto::Connection) -> Self {
        Self::new(key, inner, State::Accepting)
    }

    fn new(key: Key, inner: noq_proto::Connection, state: State) -> Self {
        Self {
            key,
            inner,
            queued: false,
            state,
        }
    }

    /// What `event` means to the caller, if anything.
    pub(super) fn event(&mut self, event: noq_proto::Event) -> Option<Event> {
        let key = self.key;
        match event {
            noq_proto::Event::Connected => {
                assert!(
                    matches!(self.state, State::Dialing { .. } | State::Accepting),
                    "invariant: noq-proto connects a connection once, before it ends"
                );
                self.state = State::Open;
                Some(Event::Connected {
                    key,
                    peer: self.peer(),
                })
            }
            noq_proto::Event::ConnectionLost { reason } => {
                let expected = match mem::replace(&mut self.state, State::Ended) {
                    State::Dialing { expected } => Some(expected),
                    State::Open => None,
                    State::Accepting | State::Ended => return None,
                };
                let error = error(reason, expected);
                Some(Event::Closed { key, error })
            }
            noq_proto::Event::HandshakeDataReady
            | noq_proto::Event::HandshakeConfirmed
            | noq_proto::Event::Stream(_)
            | noq_proto::Event::DatagramReceived
            | noq_proto::Event::DatagramsUnblocked
            | noq_proto::Event::Path(_)
            | noq_proto::Event::NatTraversal(_) => None,
        }
    }

    /// Closes the connection with `code`, and gives its [`Event::Closed`] unless it
    /// already ended.
    ///
    /// # Panics
    ///
    /// When the caller does not have the key yet.
    pub(super) fn close(&mut self, now: Instant, code: Code) -> Option<Event> {
        match mem::replace(&mut self.state, State::Ended) {
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
