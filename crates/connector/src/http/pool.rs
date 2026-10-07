//! The idle connections of a client.

use std::cell::RefCell;
use std::collections::BTreeMap;

use types::time::{Monotonic, Span};

use super::{Connection, Origin};

/// The longest a connection stays idle. The client sends no keep-alive, so a
/// firewall or NAT may have dropped the state of an older stream.
const IDLE_MAX: Span = Span::from_nanos(90 * Span::SECOND.nanos());

/// At most one idle connection for each origin. A `BTreeMap` drops connections in
/// the same order in each run.
#[derive(Debug, Default)]
pub(super) struct Pool(RefCell<BTreeMap<Origin, Idle>>);

#[derive(Debug)]
struct Idle {
    connection: Connection,
    /// When its last exchange ended.
    since: Monotonic,
}

impl Pool {
    /// Drops each connection idle longer than 90 s at `now`, then takes the one for
    /// `origin`.
    pub(super) fn take(&self, origin: &Origin, now: Monotonic) -> Option<Connection> {
        let mut idle = self.0.borrow_mut();
        idle.retain(|_, idle| {
            idle.since
                .checked_add(IDLE_MAX)
                .is_none_or(|end| now <= end)
        });
        idle.remove(origin).map(|idle| idle.connection)
    }

    /// Keeps `connection` as the idle one for `origin`, from `now`. It drops the
    /// connection it replaces.
    pub(super) fn put(&self, origin: Origin, connection: Connection, now: Monotonic) {
        self.0.borrow_mut().insert(
            origin,
            Idle {
                connection,
                since: now,
            },
        );
    }
}
