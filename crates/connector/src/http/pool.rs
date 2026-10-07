//! The idle connections of a client.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::SocketAddr;

use hyper::client::conn::http1::SendRequest;
use types::time::{Monotonic, Span};

use super::body::Whole;

/// The longest a connection stays idle. The client sends no keep-alive, so a
/// firewall or NAT may have dropped the state of an older stream.
const IDLE_MAX: Span = Span::from_nanos(90 * Span::SECOND.nanos());

/// At most one idle connection for each origin. A `BTreeMap` drops connections in
/// the same order in each run.
#[derive(Debug, Default)]
pub(super) struct Pool(RefCell<BTreeMap<SocketAddr, Idle>>);

#[derive(Debug)]
struct Idle {
    sender: SendRequest<Whole>,
    /// When its last exchange ended.
    since: Monotonic,
}

impl Pool {
    /// Drops each connection idle longer than 90 s at `now`, then takes the one for
    /// `origin` when it is ready for a request.
    pub(super) fn take(
        &self,
        origin: SocketAddr,
        now: Monotonic,
    ) -> Option<SendRequest<Whole>> {
        let mut idle = self.0.borrow_mut();
        idle.retain(|_, idle| {
            idle.since
                .checked_add(IDLE_MAX)
                .is_none_or(|end| now <= end)
        });
        idle.remove(&origin)
            .map(|idle| idle.sender)
            .filter(SendRequest::is_ready)
    }

    /// Keeps `sender` as the idle connection for `origin`, from `now`. It drops the
    /// connection it replaces.
    pub(super) fn put(
        &self,
        origin: SocketAddr,
        sender: SendRequest<Whole>,
        now: Monotonic,
    ) {
        self.0
            .borrow_mut()
            .insert(origin, Idle { sender, since: now });
    }
}
