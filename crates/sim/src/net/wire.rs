//! The links between nodes and the packets in flight on them.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::IpAddr;
use std::num::NonZeroU64;

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

/// The way from a node to an address.
pub(super) struct Path {
    /// The node that sends.
    from: usize,
    /// The node that has the address, if one has it.
    to: Option<usize>,
    pub(super) link: link::Config,
}

/// The packets that one direction of a link with a rate sends back to back.
struct Transmitter {
    /// The rate of the link since `start`.
    rate: NonZeroU64,
    /// The true time at which the first packet began to leave.
    start: Monotonic,
    /// The bytes of the packets since `start`.
    bytes: u128,
    /// The true time at which the last packet leaves.
    free: Monotonic,
}

/// A packet in flight, with the node that sent it and the true time at which it
/// leaves its link.
struct Flight {
    from: usize,
    departure: Monotonic,
    packet: Packet,
}

/// The links and the packets in flight. Each fate comes from the network's own stream
/// of the seed.
pub(super) struct Wire {
    /// The link of each ordered pair of nodes that has no link of its own.
    default: link::Config,
    links: BTreeMap<(usize, usize), link::Config>,
    /// The transmitter of each direction of a link, by the node that sends and the
    /// node it goes to.
    transmitters: BTreeMap<(usize, Option<usize>), Transmitter>,
    /// Packets by true arrival time, then by a key in the order they were sent.
    flights: BTreeMap<(Monotonic, u64), Flight>,
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

    /// The way from node `from` to `ip`.
    pub(super) fn path(&self, from: usize, ip: IpAddr) -> Path {
        let to = node(ip);
        let link = to.and_then(|to| self.links.get(&(from, to)));
        Path {
            from,
            to,
            link: *link.unwrap_or(&self.default),
        }
    }

    /// Sends a packet of `bytes` on `path` at true time `now`, and gives the true
    /// time at which it has left the link. It starts when the packets sent on the
    /// link before it have left, and takes its bytes at the link's rate, or no time
    /// with no rate. Packets sent back to back at one rate leave at the rate of their
    /// total bytes. `None` past `u64` nanoseconds, where it never leaves and takes no
    /// time of the link.
    pub(super) fn depart(
        &mut self,
        now: Monotonic,
        path: &Path,
        bytes: usize,
    ) -> Option<Monotonic> {
        let key = (path.from, path.to);
        let busy = self.transmitters.get(&key).filter(|sent| sent.free > now);
        let Some(rate) = path.link.rate else {
            return Some(busy.map_or(now, |sent| sent.free));
        };
        let (start, sent) = match busy {
            Some(sent) if sent.rate == rate => (sent.start, sent.bytes),
            Some(sent) => (sent.free, 0),
            None => (now, 0),
        };
        let bytes = sent + u128::try_from(bytes).expect("invariant: a usize fits u128");
        let nanos = (bytes * 1_000_000_000).div_ceil(u128::from(rate.get()));
        let free = start.checked_add(Span::from_nanos(i64::try_from(nanos).ok()?))?;
        let transmitter = Transmitter {
            rate,
            start,
            bytes,
            free,
        };
        self.transmitters.insert(key, transmitter);
        Some(free)
    }

    /// Drops the packets of `node` that have not left their links by true time
    /// `now`, and its transmitters, when its power is cut.
    pub(super) fn cut(&mut self, now: Monotonic, node: usize) {
        self.transmitters.retain(|&(from, _), _| from != node);
        (self.flights)
            .retain(|_, flight| flight.from != node || flight.departure <= now);
    }

    /// The network's stream of the seed, for the draws of each protocol.
    pub(super) fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    /// Puts `datagram` in flight on `path` from true time `departure`, at which it
    /// has left the link, as the link's loss and duplication decide.
    pub(super) fn fly(
        &mut self,
        path: &Path,
        departure: Monotonic,
        datagram: Datagram,
    ) -> Fate {
        if roll(&mut self.rng, path.link.loss) {
            return Fate::Lost;
        }
        let duplicated = roll(&mut self.rng, path.link.duplication);
        if duplicated && let Some(at) = self.draw(path, departure) {
            self.put(path, departure, at, Packet::Datagram(datagram.clone()));
        }
        if let Some(at) = self.draw(path, departure) {
            self.put(path, departure, at, Packet::Datagram(datagram));
        }
        if duplicated {
            Fate::Duplicated
        } else {
            Fate::Sent
        }
    }

    /// The true arrival time of a packet that has left the link of `path` at true
    /// time `departure`: after the delay of the link and a draw of its jitter. `None`
    /// past `u64` nanoseconds, where a packet never arrives.
    pub(super) fn draw(
        &mut self,
        path: &Path,
        departure: Monotonic,
    ) -> Option<Monotonic> {
        let jitter = u64::try_from(path.link.jitter.nanos())
            .expect("invariant: a checked link has no negative jitter");
        let extra = i64::try_from(self.rng.below(jitter + 1))
            .expect("invariant: a draw up to a jitter fits i64");
        (departure.checked_add(path.link.delay))
            .and_then(|at| at.checked_add(Span::from_nanos(extra)))
    }

    /// Puts `packet` in flight on `path`, from true time `departure` to true time `at`.
    pub(super) fn put(
        &mut self,
        path: &Path,
        departure: Monotonic,
        at: Monotonic,
        packet: Packet,
    ) {
        self.next += 1;
        let flight = Flight {
            from: path.from,
            departure,
            packet,
        };
        self.flights.insert((at, self.next), flight);
    }

    /// The packets in flight, with their true arrival times, in the order they
    /// arrive.
    pub(super) fn flights(&self) -> impl Iterator<Item = (Monotonic, &Packet)> {
        (self.flights.iter()).map(|(&(at, _), flight)| (at, &flight.packet))
    }

    /// The true time of the first arrival.
    pub(super) fn first(&self) -> Option<Monotonic> {
        self.flights.first_key_value().map(|(&(at, _), _)| at)
    }

    /// Takes the first packet that arrives by true time `at`.
    pub(super) fn pop(&mut self, at: Monotonic) -> Option<Packet> {
        let flight = self.flights.first_entry()?;
        (flight.key().0 <= at).then(|| flight.remove().packet)
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
