//! The links between nodes and the packets in flight on them.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::IpAddr;

use env::rng::Rng;
use types::time::{Monotonic, Span};

use super::udp::Datagram;
use super::{Fate, node};
use crate::chance::roll;
use crate::link;

/// The links and the packets in flight. Each fate comes from the network's own stream
/// of the seed.
pub(super) struct Wire {
    /// The link of each ordered pair of nodes that has no link of its own.
    default: link::Config,
    links: BTreeMap<(usize, usize), link::Config>,
    /// Datagrams by true arrival time, then by a key in the order they were sent.
    flights: BTreeMap<(Monotonic, u64), Datagram>,
    rng: Rng,
    next: u64,
    /// A hash of every send and arrival, in order.
    digest: DefaultHasher,
}

impl Wire {
    pub(super) fn new(default: link::Config, rng: Rng) -> Self {
        Self {
            default,
            links: BTreeMap::new(),
            flights: BTreeMap::new(),
            rng,
            next: 0,
            digest: DefaultHasher::new(),
        }
    }

    /// Sets the link from node `from` to node `to`.
    pub(super) fn link(&mut self, from: usize, to: usize, config: link::Config) {
        self.links.insert((from, to), config);
    }

    /// The link from node `from` to `ip`.
    pub(super) fn path(&self, from: usize, ip: IpAddr) -> link::Config {
        let to = node(ip);
        *(to.and_then(|to| self.links.get(&(from, to)))).unwrap_or(&self.default)
    }

    /// The network's stream of the seed, for the draws of each protocol.
    pub(super) fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    /// Puts `datagram` in flight on `link` at true time `now`, as the link's loss and
    /// duplication decide.
    pub(super) fn fly(
        &mut self,
        now: Monotonic,
        link: &link::Config,
        datagram: Datagram,
    ) -> Fate {
        if roll(&mut self.rng, link.loss) {
            return Fate::Lost;
        }
        let duplicated = roll(&mut self.rng, link.duplication);
        if duplicated {
            self.arrive(now, link, datagram.clone());
        }
        self.arrive(now, link, datagram);
        if duplicated {
            Fate::Duplicated
        } else {
            Fate::Sent
        }
    }

    /// Schedules the arrival of `datagram` after the delay of `link` and a draw of its
    /// jitter. One that would arrive past `u64` nanoseconds never arrives.
    fn arrive(&mut self, now: Monotonic, link: &link::Config, datagram: Datagram) {
        let jitter = u64::try_from(link.jitter.nanos())
            .expect("invariant: a checked link has no negative jitter");
        let extra = i64::try_from(self.rng.below(jitter + 1))
            .expect("invariant: a draw up to a jitter fits i64");
        let at = (now.checked_add(link.delay))
            .and_then(|at| at.checked_add(Span::from_nanos(extra)));
        if let Some(at) = at {
            self.next += 1;
            self.flights.insert((at, self.next), datagram);
        }
    }

    /// The true time of the first arrival.
    pub(super) fn first(&self) -> Option<Monotonic> {
        self.flights.first_key_value().map(|(&(at, _), _)| at)
    }

    /// Takes the first datagram that arrives by true time `at`.
    pub(super) fn pop(&mut self, at: Monotonic) -> Option<Datagram> {
        let flight = self.flights.first_entry()?;
        (flight.key().0 <= at).then(|| flight.remove())
    }

    /// Adds `event` to the digest.
    pub(super) fn record(&mut self, event: impl Hash) {
        event.hash(&mut self.digest);
    }

    /// A hash of every event recorded so far.
    pub(super) fn digest(&self) -> u64 {
        self.digest.finish()
    }
}
