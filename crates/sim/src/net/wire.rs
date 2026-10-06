//! The links between nodes and the packets in flight on them.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::IpAddr;

use env::rng::Rng;
use types::time::{Monotonic, Span};

use super::tcp::Segment;
use super::udp::Datagram;
use super::{Fate, node};
use crate::chance::roll;
use crate::link;

/// A packet in flight.
pub(super) enum Packet {
    Datagram(Datagram),
    Segment(Segment),
}

/// The links and the packets in flight. Each fate comes from the network's own stream
/// of the seed.
pub(super) struct Wire {
    /// The link of each ordered pair of nodes that has no link of its own.
    default: link::Config,
    links: BTreeMap<(usize, usize), link::Config>,
    /// The true time at which the last packet sent on each link with a rate leaves
    /// it, by the node that sends and the node it goes to.
    transmitters: BTreeMap<(usize, Option<usize>), Monotonic>,
    /// Packets by true arrival time, then by a key in the order they were sent.
    flights: BTreeMap<(Monotonic, u64), Packet>,
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
            transmitters: BTreeMap::new(),
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

    /// The true time at which a packet of `bytes` that node `from` sends to `ip` at
    /// true time `now` leaves the link: at the link's rate, after the packets sent on
    /// it before. `None` past `u64` nanoseconds, where it never leaves.
    pub(super) fn depart(
        &mut self,
        now: Monotonic,
        from: usize,
        ip: IpAddr,
        bytes: usize,
    ) -> Option<Monotonic> {
        let Some(rate) = self.path(from, ip).rate else {
            return Some(now);
        };
        let bytes = u128::try_from(bytes).expect("invariant: a usize fits u128");
        let nanos = (bytes * 1_000_000_000).div_ceil(u128::from(rate.get()));
        let transmit = Span::from_nanos(i64::try_from(nanos).ok()?);
        let free = self.transmitters.entry((from, node(ip))).or_insert(now);
        *free = (*free).max(now).checked_add(transmit)?;
        Some(*free)
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
        if duplicated && let Some(at) = self.draw(now, link) {
            self.put(at, Packet::Datagram(datagram.clone()));
        }
        if let Some(at) = self.draw(now, link) {
            self.put(at, Packet::Datagram(datagram));
        }
        if duplicated {
            Fate::Duplicated
        } else {
            Fate::Sent
        }
    }

    /// The true arrival time of a packet sent on `link` at true time `now`: after the
    /// delay of the link and a draw of its jitter. `None` past `u64` nanoseconds,
    /// where a packet never arrives.
    pub(super) fn draw(
        &mut self,
        now: Monotonic,
        link: &link::Config,
    ) -> Option<Monotonic> {
        let jitter = u64::try_from(link.jitter.nanos())
            .expect("invariant: a checked link has no negative jitter");
        let extra = i64::try_from(self.rng.below(jitter + 1))
            .expect("invariant: a draw up to a jitter fits i64");
        (now.checked_add(link.delay))
            .and_then(|at| at.checked_add(Span::from_nanos(extra)))
    }

    /// Puts `packet` in flight to arrive at true time `at`.
    pub(super) fn put(&mut self, at: Monotonic, packet: Packet) {
        self.next += 1;
        self.flights.insert((at, self.next), packet);
    }

    /// The true time of the first arrival.
    pub(super) fn first(&self) -> Option<Monotonic> {
        self.flights.first_key_value().map(|(&(at, _), _)| at)
    }

    /// Takes the first packet that arrives by true time `at`.
    pub(super) fn pop(&mut self, at: Monotonic) -> Option<Packet> {
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
